//! Opening an ACP session: the handshake requests, restoring a session, signing in when the agent
//! asks, and the request helpers that keep the transport drained while a setup call waits.

use std::collections::VecDeque;
use std::time::Duration;

use futures::future::BoxFuture;
use reno_proto::{AccessMode, ExecSpec, McpServerSpec};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::HarnessError;
use crate::agent_id::HarnessId;
use crate::jsonrpc::{Incoming, RpcClient};
use crate::wire::acp::permission;

/// Bound on one session control call (model or config option switch).
pub(super) const CONTROL_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) fn initialize_params() -> Value {
    // Grok gets no `_meta.clientType`, deliberately: naming a client type that can show a prompt
    // turns the actions its Auto-mode classifier denies into permission requests. Devin gets no
    // `cognition.ai/subagentSupport`: with it, every nested subagent message would arrive as the
    // parent's own, and the contract has no place for a subagent's transcript.
    json!({
        "protocolVersion": 1,
        "clientInfo": {
            "name": "reno",
            "title": "Reno",
            "version": env!("CARGO_PKG_VERSION"),
        },
        // Declined: agents fall back to their own fs/terminal access, which is what Reno wants:
        // the working tree is the source of truth, and commands belong to the agent's own
        // sandbox.
        "clientCapabilities": {
            "fs": { "readTextFile": false, "writeTextFile": false },
            "terminal": false,
        },
    })
}

/// `session/cancel` params. Grok treats a cancel carrying a user gesture as a barrier against its
/// stale background-task wake prompts until the next real prompt; a bare cancel lets them
/// restart the stopped work.
pub(super) fn cancel_params(harness: HarnessId, session_id: &str) -> Value {
    let mut params = json!({ "sessionId": session_id });
    if harness == HarnessId::Grok {
        params["_meta"] = json!({ "cancelTrigger": "ctrl_c" });
    }
    params
}

/// `session/new` `mcpServers` for the injected servers: ACP spells a stdio server as
/// name/command/args plus `[{name, value}]` env pairs. A name that repeats keeps its first entry:
/// Reno's own server is first in [`ExecSpec::mcp`]. The user's own servers come from the agent's
/// config, never from here.
pub(super) fn mcp_servers(servers: &[McpServerSpec]) -> Vec<Value> {
    let mut seen = std::collections::HashSet::new();
    servers
        .iter()
        .filter(|server| seen.insert(server.name.as_str()))
        .map(|server| {
            json!({
                "name": server.name,
                "command": server.command,
                "args": server.args,
                "env": server
                    .env
                    .iter()
                    .map(|(name, value)| json!({ "name": name, "value": value }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect()
}

/// `initialize._meta.steering.supported`: the `_session/steering` extension some agents
/// advertise (not part of the v1 spec).
pub(super) fn steering_supported(init: &Value) -> bool {
    init.get("_meta")
        .and_then(|m| m.get("steering"))
        .and_then(|s| s.get("supported"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// How a resumed run restores its session: `session/load` replays the whole transcript,
/// `session/resume` only reattaches it. Each is used only when advertised; Antigravity prefers
/// resume because its load replays every tool call, which on a long session outlasts the
/// handshake. An agent declaring neither still gets a load attempt: a method-not-found refusal
/// falls back to a fresh session (see [`session_unrestorable`]).
fn restore_method(init: &Value, harness: HarnessId) -> &'static str {
    let capabilities = &init["agentCapabilities"];
    let load = capabilities["loadSession"].as_bool() == Some(true);
    let resume = !capabilities["sessionCapabilities"]["resume"].is_null();
    match (load, resume) {
        (true, true) if harness == HarnessId::Antigravity => "session/resume",
        (false, true) => "session/resume",
        _ => "session/load",
    }
}

/// Whether a failed restore means the agent no longer has the session, so starting fresh loses
/// nothing a retry would recover: ACP's resource-not-found code, no restore method at all, or a
/// message that names the session as missing ("Session not found" from Devin). A bare "not
/// found" is not enough: a missing MCP server command or model fails a load in the same words
/// while the session is fine. Grok is the one exception: for an unknown session its only answer
/// is a generic "Path not found." (FS_NOT_FOUND, verified on 1.0.46), and a missing working
/// directory cannot be the cause there, because the agent was started in it. Anything else (the
/// load timing out, the agent exiting or its pipe closing before an answer, any other refusal)
/// keeps the session for the next run.
pub(super) fn session_unrestorable(harness: HarnessId, error: &HarnessError) -> bool {
    let HarnessError::Protocol(message) = error else {
        return false;
    };
    let message = message.to_ascii_lowercase();
    let says = |needles: &[&str]| needles.iter().any(|needle| message.contains(needle));
    says(&["(code -32002)", "(code -32601)"])
        || says(&["session not found", "unknown session", "no such session"])
        || (harness == HarnessId::Grok
            && message.contains("path not found")
            && message.contains("fs_not_found"))
}

/// `session/new`, or for a resumed run the advertised restore with a fallback to a fresh session
/// when the agent no longer has it (see [`session_unrestorable`]); any other restore failure
/// errors the run and keeps the session. auth_required propagates so setup can sign in and
/// retry.
#[allow(clippy::too_many_arguments)]
pub(super) async fn open_session(
    client: &RpcClient,
    incoming: &mut mpsc::Receiver<Incoming>,
    init: &Value,
    harness: HarnessId,
    spec: &ExecSpec,
    access: AccessMode,
    agent_name: &str,
    load_timeout: Duration,
) -> Result<(String, Value), HarnessError> {
    let params = json!({
        "cwd": spec.cwd,
        "mcpServers": mcp_servers(&spec.mcp),
    });
    if let Some(resume) = &spec.resume {
        let method = restore_method(init, harness);
        let mut restore = params.clone();
        restore["sessionId"] = Value::String(resume.0.clone());
        let restored = if method == "session/load" {
            tokio::time::timeout(
                load_timeout,
                request_draining(client, incoming, access, method, restore),
            )
            .await
            .unwrap_or_else(|_| {
                Err(HarnessError::Protocol(format!(
                    "session/load: the history replay did not finish within {}s",
                    load_timeout.as_secs()
                )))
            })
        } else {
            request_draining(client, incoming, access, method, restore).await
        };
        match restored {
            Ok(response) => return Ok((resume.0.clone(), response)),
            Err(e) if is_auth_required(&e) => return Err(e),
            // A slow or interrupted restore is retried by the next run; starting fresh would
            // replace the stored session for good.
            Err(e) if !session_unrestorable(harness, &e) => {
                return Err(HarnessError::Protocol(format!(
                    "{agent_name} could not restore session {}; the next run tries again: {e}",
                    resume.0
                )));
            }
            // A missing or foreign session falls back to a fresh one.
            Err(e) => {
                tracing::warn!(
                    target: "reno_harness::acp",
                    "{agent_name} could not restore session {}; starting a new session without \
                     the previous context: {e}",
                    resume.0
                );
            }
        }
    }
    let new = request_draining(client, incoming, access, "session/new", params).await?;
    let session_id = new
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok((session_id, new))
}

/// The sign-in a run may perform on its own after auth_required: the method the agent itself
/// names as its default (`_meta.defaultAuthMethodId`, Grok's `cached_token`, which re-reads its
/// stored login). Not just any advertised method: the others open a browser sign-in nobody is
/// watching. `Err` names the kind of a method that cannot run here at all (terminal, env var).
fn run_auth_method(init: &Value) -> Result<Option<String>, String> {
    let Some(default) = init["_meta"]["defaultAuthMethodId"].as_str() else {
        return Ok(None);
    };
    let Some(method) = init["authMethods"]
        .as_array()
        .and_then(|methods| methods.iter().find(|m| m["id"].as_str() == Some(default)))
    else {
        return Ok(None);
    };
    match method["type"].as_str() {
        None | Some("agent") => Ok(Some(default.to_owned())),
        Some(kind) => Err(kind.replace('_', " ")),
    }
}

/// Answer auth_required once from the run itself (see [`run_auth_method`]), so a login the agent
/// can restore by itself does not fail the run.
pub(super) async fn authenticate_for_run(
    client: &RpcClient,
    incoming: &mut mpsc::Receiver<Incoming>,
    access: AccessMode,
    init: &Value,
    agent_name: &str,
    required: &HarnessError,
) -> Result<(), HarnessError> {
    let method = match run_auth_method(init) {
        Ok(Some(method)) => method,
        Ok(None) => return Err(HarnessError::Protocol(signed_out(agent_name, required))),
        Err(kind) => {
            return Err(HarnessError::Protocol(format!(
                "{agent_name} isn't signed in, and its sign-in ({kind}) can't run inside a \
                 run. Sign in from a terminal, then start the run again."
            )));
        }
    };
    request_draining(
        client,
        incoming,
        access,
        "authenticate",
        json!({ "methodId": method }),
    )
    .await
    .map(|_| ())
    .map_err(|e| HarnessError::Protocol(signed_out(agent_name, &e)))
}

pub(super) fn signed_out(agent_name: &str, error: &HarnessError) -> String {
    format!("{agent_name} isn't signed in: {error}")
}

/// Where an agent that signs in outside a run sends a signed-out one.
pub(super) fn not_signed_in(agent_name: &str) -> String {
    format!("{agent_name} isn't signed in. Sign in with the agent, then start the run again.")
}

/// acp reserves -32000 for auth_required.
pub(super) fn is_auth_required(error: &HarnessError) -> bool {
    matches!(error, HarnessError::Protocol(message) if message.contains("(code -32000)"))
}

/// Answer a server request while nobody can be asked (setup, a control call). What the run's
/// access mode lets through unasked gets `allow_once`; any other permission, and every question,
/// is `cancelled`, never allowed silently. Any other method is unsupported: fs and terminal were
/// declined at `initialize`.
pub(super) fn answer_unattended(
    client: &RpcClient,
    id: &Value,
    method: &str,
    params: &Value,
    access: AccessMode,
) {
    if method != "session/request_permission" {
        tracing::debug!(target: "reno_harness::acp", "unhandled server request: {method}");
        client.respond_error(id, -32601, &format!("unsupported method: {method}"));
        return;
    }
    let options = permission::options(params);
    let kind = params
        .get("toolCall")
        .and_then(permission::kind_of)
        .map(str::to_owned);
    let allow = !permission::is_question(&options)
        && permission::allowed_without_asking(access, kind.as_deref());
    let outcome = if allow {
        permission::outcome(&options, true)
    } else {
        permission::cancelled()
    };
    client.respond(id, outcome);
}

/// Await a setup request while draining incoming messages, so a `session/load` whose replay
/// outruns the incoming channel's capacity can't deadlock the reader. Replayed `session/update`s
/// are dropped (the caller started this run and has none of that history); server requests are
/// answered unattended under `access`.
pub(super) async fn request_draining(
    client: &RpcClient,
    incoming: &mut mpsc::Receiver<Incoming>,
    access: AccessMode,
    method: &'static str,
    params: Value,
) -> Result<Value, HarnessError> {
    request_draining_with(client, incoming, access, method, params, None).await
}

/// A sign-in's page an agent asks for (ACP's `elicitation/create`, its `url` mode): a web page,
/// reported to `on_page` and accepted (the person asked to sign in; the page is the caller's to
/// open, as any sign-in page); anything else declined.
fn answer_elicitation(
    client: &RpcClient,
    id: &Value,
    params: &Value,
    on_page: &(dyn Fn(String) + Send + Sync),
) {
    let url = params
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|u| {
            u.starts_with("https://") && u.len() <= 4096 && !u.chars().any(char::is_whitespace)
        });
    let known = params
        .get("elicitationId")
        .and_then(Value::as_str)
        .is_some_and(|e| !e.trim().is_empty() && e.len() <= 128);
    match url {
        Some(url) if known && params.get("mode").and_then(Value::as_str) == Some("url") => {
            on_page(url.to_owned());
            client.respond(id, json!({ "action": "accept" }));
        }
        _ => client.respond(id, json!({ "action": "decline" })),
    }
}

/// [`request_draining`], answering the agent's page requests (`elicitation/create`) with
/// `on_page` when given.
pub(super) async fn request_draining_with(
    client: &RpcClient,
    incoming: &mut mpsc::Receiver<Incoming>,
    access: AccessMode,
    method: &'static str,
    params: Value,
    on_page: Option<&(dyn Fn(String) + Send + Sync)>,
) -> Result<Value, HarnessError> {
    let loading_session = matches!(method, "session/new" | "session/load" | "session/resume");
    let requested_session = params
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut metadata = VecDeque::new();
    let mut handle_incoming = |inc| match inc {
        Incoming::Request { id, method, params }
            if method == "elicitation/create" && on_page.is_some() =>
        {
            if let Some(on_page) = on_page {
                answer_elicitation(client, &id, &params, on_page);
            }
        }
        Incoming::Request { id, method, params } => {
            if method == "session/request_permission"
                && params.get("sessionId").and_then(Value::as_str) != requested_session.as_deref()
            {
                client.respond(&id, permission::cancelled());
            } else {
                answer_unattended(client, &id, &method, &params, access);
            }
        }
        Incoming::Notification { method, params }
            if loading_session
                && method == "session/update"
                && matches!(
                    params["update"]["sessionUpdate"].as_str(),
                    Some(
                        "config_option_update"
                            | "available_commands_update"
                            | "current_mode_update"
                    )
                ) =>
        {
            if metadata.len() == 32 {
                metadata.pop_front();
            }
            metadata.push_back(params);
        }
        _ => {}
    };
    let mut fut = prompt_like_request(client.clone(), method, params);
    let res = loop {
        tokio::select! {
            res = &mut fut => break res,
            inc = incoming.recv() => match inc {
                Some(inc) => handle_incoming(inc),
                None => {
                    return Err(HarnessError::Protocol(format!(
                        "{method}: agent exited during setup"
                    )));
                }
            },
        }
    };
    // Responses resolve through the pending map, not the incoming queue, so replay updates the
    // reader forwarded BEFORE the response line may still sit in the buffer: flush them now or
    // they'd leak into the live turn.
    while let Ok(inc) = incoming.try_recv() {
        handle_incoming(inc);
    }
    // Keep config refreshes even when they race the session response. They are session state,
    // not replayed transcript, and dropping them can lose the only notification advertising a
    // newly released model.
    res.map(|mut response| {
        let id = response
            .get("sessionId")
            .and_then(Value::as_str)
            .or(requested_session.as_deref());
        let id = id.map(str::to_owned);
        for params in metadata {
            if params["sessionId"].as_str() != id.as_deref() {
                continue;
            }
            let update = &params["update"];
            match update["sessionUpdate"].as_str() {
                Some("config_option_update") if update["configOptions"].is_array() => {
                    response["configOptions"] = update["configOptions"].clone();
                }
                Some("current_mode_update") if update["currentModeId"].is_string() => {
                    if !response["modes"].is_object() {
                        response["modes"] = json!({});
                    }
                    response["modes"]["currentModeId"] = update["currentModeId"].clone();
                }
                _ => {}
            }
        }
        response
    })
}

/// [`request_draining`] for a session control call (`set_model`, `set_config_option`): the
/// session loop is blocked while one waits, so an agent that never answers must not hold off the
/// run's cancel.
pub(super) async fn control_request(
    client: &RpcClient,
    incoming: &mut mpsc::Receiver<Incoming>,
    access: AccessMode,
    method: &'static str,
    params: Value,
) -> Result<Value, HarnessError> {
    tokio::time::timeout(
        CONTROL_REQUEST_TIMEOUT,
        request_draining(client, incoming, access, method, params),
    )
    .await
    .unwrap_or_else(|_| {
        Err(HarnessError::Protocol(format!(
            "{method}: the agent did not answer within {}s",
            CONTROL_REQUEST_TIMEOUT.as_secs()
        )))
    })
}

pub(super) fn prompt_like_request(
    client: RpcClient,
    method: &'static str,
    params: Value,
) -> BoxFuture<'static, Result<Value, HarnessError>> {
    Box::pin(async move { client.request(method, params).await })
}

/// A mid-turn `_session/steering` call. `idleBehavior: promptRequired` covers the turn-ended
/// race: the agent hands the text back instead of firing an untracked turn.
pub(super) fn steering_call_future(
    client: &RpcClient,
    session_id: &str,
    text: &str,
) -> BoxFuture<'static, Result<Value, HarnessError>> {
    let params = json!({
        "sessionId": session_id,
        "prompt": prompt_blocks(text),
        "_meta": { "steering": { "idleBehavior": "promptRequired" } },
    });
    prompt_like_request(client.clone(), "_session/steering", params)
}

/// The content blocks of a prompt: its text.
pub(super) fn prompt_blocks(text: &str) -> Vec<Value> {
    vec![json!({ "type": "text", "text": text })]
}

/// One turn: `session/prompt` whose response (the `stopReason`) ends it. `prompt_id` (agents with
/// the prompt-complete extension) rides `_meta` so the `_x.ai/session/prompt_complete`
/// notification can be matched exactly; Grok echoes it back (verified live, 1.0.4).
pub(super) fn prompt_turn(
    client: &RpcClient,
    session_id: &str,
    text: &str,
    prompt_id: Option<&str>,
) -> BoxFuture<'static, Result<Value, HarnessError>> {
    let mut params = json!({
        "sessionId": session_id,
        "prompt": prompt_blocks(text),
    });
    if let Some(id) = prompt_id {
        params["_meta"] = json!({ "promptId": id, "requestId": id });
    }
    // Written now, not on first poll: a steer's `session/cancel` issued in the same loop
    // iteration must reach the agent after this prompt.
    client.request_now("session/prompt", params)
}

/// Whether `line` is the agent saying it is not signed in (Antigravity prints a link to open).
pub(super) fn is_sign_in_prompt(harness: HarnessId, line: &str) -> bool {
    harness == HarnessId::Antigravity
        && line.contains("Open the following link to authenticate the ACP server")
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use reno_proto::McpServerSpec;

    use super::*;
    use crate::process::{Command, Stdio};

    #[test]
    fn restore_prefers_what_the_agent_advertises() {
        let init = |load: bool, resume: bool| {
            let mut capabilities = json!({ "loadSession": load });
            if resume {
                capabilities["sessionCapabilities"] = json!({ "resume": {} });
            }
            json!({ "agentCapabilities": capabilities })
        };
        assert_eq!(
            restore_method(&init(true, true), HarnessId::Grok),
            "session/load"
        );
        assert_eq!(
            restore_method(&init(true, true), HarnessId::Antigravity),
            "session/resume"
        );
        assert_eq!(
            restore_method(&init(false, true), HarnessId::Grok),
            "session/resume"
        );
        assert_eq!(
            restore_method(&init(true, false), HarnessId::Antigravity),
            "session/load"
        );
        // Undeclared: a load attempt, whose refusal starts fresh.
        assert_eq!(
            restore_method(&json!({}), HarnessId::Hermes),
            "session/load"
        );
    }

    #[test]
    fn only_a_missing_session_is_unrestorable() {
        let error = |message: &str| HarnessError::Protocol(format!("session/load: {message}"));
        // Grok 1.0.46's answer for an unknown session id.
        let grok_missing = r#"Path not found. (code -32603): {"detail":"No such file or directory (os error 2)","code":"FS_NOT_FOUND"}"#;
        assert!(session_unrestorable(HarnessId::Grok, &error(grok_missing)));
        // From another agent the same words say nothing about the session.
        assert!(!session_unrestorable(
            HarnessId::Devin,
            &error(grok_missing)
        ));
        for missing in [
            // Devin 3000.11 for an unknown session id.
            r#"Session not found (code -32016): {"cognition.ai/errorKind":"session_not_found"}"#,
            "Resource not found (code -32002)",
            "Method not found (code -32601)",
            "unknown session (code -32602)",
        ] {
            assert!(
                session_unrestorable(HarnessId::Devin, &error(missing)),
                "{missing}"
            );
        }
        for kept in [
            "the history replay did not finish within 90s",
            "app-server exited before responding",
            "app-server stdin closed: Broken pipe",
            "agent exited during setup",
            "session store is locked (code -32603)",
            "Invalid params (code -32602)",
            // A load fails in these words while the session is fine.
            "MCP server command not found: reno (code -32603)",
            "model not found: grok-9 (code -32603)",
            "Path not found. (code -32603)",
        ] {
            for harness in [HarnessId::Grok, HarnessId::Devin, HarnessId::Hermes] {
                assert!(!session_unrestorable(harness, &error(kept)), "{kept}");
            }
        }
    }

    #[test]
    fn a_run_signs_in_only_with_the_agents_own_default_method() {
        // Grok 1.0.46's initialize.
        let grok = json!({
            "authMethods": [
                { "id": "cached_token", "name": "cached_token" },
                { "id": "grok.com", "name": "Grok" },
            ],
            "_meta": { "defaultAuthMethodId": "cached_token" },
        });
        assert_eq!(run_auth_method(&grok), Ok(Some("cached_token".into())));
        // Devin advertises only a browser login and names no default.
        let devin = json!({
            "authMethods": [{ "id": "devin-browser", "name": "Log in with browser" }],
        });
        assert_eq!(run_auth_method(&devin), Ok(None));
        // A default that is not advertised is not trusted either.
        let stale = json!({ "authMethods": [], "_meta": { "defaultAuthMethodId": "gone" } });
        assert_eq!(run_auth_method(&stale), Ok(None));
        for (kind, named) in [("terminal", "terminal"), ("env_var", "env var")] {
            let init = json!({
                "authMethods": [{ "id": "m", "name": "M", "type": kind }],
                "_meta": { "defaultAuthMethodId": "m" },
            });
            assert_eq!(run_auth_method(&init), Err(named.to_owned()));
        }
    }

    #[test]
    fn grok_cancel_carries_the_gesture_and_others_stay_bare() {
        assert_eq!(
            cancel_params(HarnessId::Grok, "s"),
            json!({ "sessionId": "s", "_meta": { "cancelTrigger": "ctrl_c" } })
        );
        assert_eq!(
            cancel_params(HarnessId::Devin, "s"),
            json!({ "sessionId": "s" })
        );
    }

    /// No client type (it would turn Grok's classifier denials into permission requests) and no
    /// Devin subagent support (every nested message would arrive as the parent's own).
    #[test]
    fn initialize_declines_fs_and_terminal_and_asks_for_nothing_extra() {
        let params = initialize_params();
        assert_eq!(params["protocolVersion"], 1);
        assert_eq!(params["clientInfo"]["name"], "reno");
        assert_eq!(params["clientCapabilities"]["fs"]["readTextFile"], false);
        assert_eq!(params["clientCapabilities"]["fs"]["writeTextFile"], false);
        assert_eq!(params["clientCapabilities"]["terminal"], false);
        assert!(params.get("_meta").is_none());
        assert!(params["clientCapabilities"].get("_meta").is_none());
    }

    #[test]
    fn steering_capability_reads_initialize_meta() {
        assert!(steering_supported(&json!({
            "protocolVersion": 1,
            "_meta": { "steering": { "supported": true } },
        })));
        assert!(!steering_supported(&json!({ "protocolVersion": 1 })));
        assert!(!steering_supported(&json!({
            "_meta": { "steering": { "supported": false } },
        })));
    }

    #[test]
    fn mcp_servers_spell_env_as_name_value_pairs_in_order_and_keep_the_first_of_a_name() {
        assert!(mcp_servers(&[]).is_empty());
        let reno = McpServerSpec {
            name: "reno".into(),
            command: "/opt/reno/reno".into(),
            args: vec!["mcp".into()],
            env: [("RENO_IPC_PORT".to_owned(), "27654".to_owned())]
                .into_iter()
                .collect(),
        };
        let plugin = McpServerSpec {
            name: "plugin-focus".into(),
            command: "/data/plugins/focus/bin/server".into(),
            args: vec![],
            env: [("FOCUS".to_owned(), "1".to_owned())].into_iter().collect(),
        };
        let impostor = McpServerSpec {
            command: "/elsewhere/reno".into(),
            ..reno.clone()
        };
        let servers = mcp_servers(&[reno, plugin, impostor]);
        assert_eq!(
            servers,
            vec![
                json!({
                    "name": "reno",
                    "command": "/opt/reno/reno",
                    "args": ["mcp"],
                    "env": [{ "name": "RENO_IPC_PORT", "value": "27654" }],
                }),
                json!({
                    "name": "plugin-focus",
                    "command": "/data/plugins/focus/bin/server",
                    "args": [],
                    "env": [{ "name": "FOCUS", "value": "1" }],
                }),
            ]
        );
    }

    /// What `answer_unattended` writes back for one server request: a shell that reads one line
    /// and records it stands in for the agent.
    async fn answered(access: AccessMode, method: &str, params: Value) -> Value {
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("reply");
        let mut child = Command::new("/bin/sh")
            .arg("-c")
            .arg(format!(
                "IFS= read -r line; printf '%s' \"$line\" > '{}'.tmp; mv '{0}.tmp' '{0}'",
                out.display()
            ))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let tracker = tokio_util::task::TaskTracker::new();
        let (client, _incoming) = RpcClient::spawn(
            &tracker,
            child.stdin.take().unwrap(),
            child.stdout.take().unwrap(),
        );
        answer_unattended(&client, &json!(7), method, &params, access);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !out.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let reply: Value = serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(reply["id"], 7);
        reply
    }

    #[tokio::test]
    async fn a_setup_request_is_answered_by_the_access_table_and_never_with_the_lasting_option() {
        let permission = |kind: &str| {
            json!({
                "sessionId": "s",
                "toolCall": { "kind": kind },
                "options": [
                    { "optionId": "always", "kind": "allow_always" },
                    { "optionId": "once", "kind": "allow_once" },
                ],
            })
        };
        let selected = |reply: &Value| reply["result"]["outcome"]["optionId"].clone();
        let outcome = |reply: &Value| reply["result"]["outcome"]["outcome"].clone();
        let method = "session/request_permission";
        // Looking around is allowed in every mode, with the once option.
        let reply = answered(AccessMode::Supervised, method, permission("read")).await;
        assert_eq!(selected(&reply), "once");
        // A command is not, unless the run is in full access.
        let reply = answered(AccessMode::Supervised, method, permission("execute")).await;
        assert_eq!(outcome(&reply), "cancelled");
        let reply = answered(AccessMode::FullAccess, method, permission("execute")).await;
        assert_eq!(selected(&reply), "once");
        // A question has nobody to ask during setup.
        let question = json!({
            "sessionId": "s",
            "options": [{ "optionId": "a", "name": "A" }],
        });
        let reply = answered(AccessMode::FullAccess, method, question).await;
        assert_eq!(outcome(&reply), "cancelled");
        // Any other request is unsupported: fs and terminal were declined.
        let reply = answered(AccessMode::FullAccess, "fs/read_text_file", json!({})).await;
        assert_eq!(reply["error"]["code"], -32601);
    }

    /// Updates a `session/new` response races ahead of it (`available_commands_update`,
    /// `config_option_update`, `current_mode_update`) are kept as session state, bounded; the
    /// fixture floods before its answer.
    #[tokio::test]
    async fn setup_retains_bounded_session_metadata_before_response() {
        let mut child = Command::new(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-robust-acp.py"),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
        let tracker = tokio_util::task::TaskTracker::new();
        let (client, mut incoming) = RpcClient::spawn(
            &tracker,
            child.stdin.take().unwrap(),
            child.stdout.take().unwrap(),
        );
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            request_draining(
                &client,
                &mut incoming,
                AccessMode::FullAccess,
                "session/new",
                json!({}),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result["configOptions"], json!([]));
        assert_eq!(result["modes"]["currentModeId"], "plan");
        child.kill().await.unwrap();
    }
}
