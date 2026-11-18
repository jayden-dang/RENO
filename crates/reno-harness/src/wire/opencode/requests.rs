//! The requests Reno makes to an opencode server, and the guard that refuses every other.
//!
//! Reno never writes opencode's configuration or credentials (the harness is the owner's, see
//! `drivers::opencode`). The server has routes that do: `PATCH /config` and `/global/config`,
//! `PUT /auth/{provider}`, `/provider/{id}/oauth/*`, `/api/credential/*`, `/api/integration/*/connect/*`,
//! `POST /mcp`, `/global/upgrade`, `PATCH /session/{id}`, `/api/permission/saved` and more. The
//! driver sends every request through [`permitted`], an allowlist of (method, route shape): a route
//! not listed is refused before it is sent, so a new writer in a later opencode is refused by
//! default and a builder added by mistake cannot reach one. [`Method`] has no `PATCH`, `PUT` or
//! `DELETE` at all.
//!
//! A permission reply is [`Reply::Once`] or [`Reply::Reject`]. There is no way to say `always`:
//! that saves a durable rule in the owner's permission store.

use serde_json::{Value, json};

use super::version::{Protocol, ServerVersion};

/// The HTTP methods Reno uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Method {
    /// A read.
    Get,
    /// A command.
    Post,
}

/// One request: a route, without the directory scope the driver adds.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Request {
    pub(crate) method: Method,
    pub(crate) path: String,
    pub(crate) body: Option<Value>,
}

impl Request {
    /// A read of a fixed route (a health probe, the bus).
    pub(crate) fn get_path(path: &str) -> Self {
        Self::get(path.to_owned())
    }

    fn get(path: String) -> Self {
        Self {
            method: Method::Get,
            path,
            body: None,
        }
    }

    fn post(path: String, body: Value) -> Self {
        Self {
            method: Method::Post,
            path,
            body: Some(body),
        }
    }
}

/// The routes Reno may call. `{}` matches exactly one non-empty path segment.
const ALLOWED: &[(Method, &str)] = &[
    // Health, readiness, discovery.
    (Method::Get, "/global/health"),
    (Method::Get, "/api/info"),
    (Method::Get, "/api/status"),
    (Method::Get, "/api/health"),
    (Method::Get, "/provider"),
    (Method::Get, "/agent"),
    (Method::Get, "/api/model"),
    (Method::Get, "/api/agent"),
    (Method::Get, "/api/agent/{}"),
    (Method::Get, "/api/provider"),
    // The MCP servers the server registered (a read: `POST /mcp` adds one and is refused).
    (Method::Get, "/mcp"),
    (Method::Get, "/api/mcp"),
    // The bus.
    (Method::Get, "/global/event"),
    (Method::Get, "/api/event"),
    // Sessions: create, look up, status, history of one message, children.
    (Method::Post, "/session"),
    (Method::Get, "/session/{}"),
    (Method::Get, "/session/status"),
    (Method::Get, "/session/{}/message/{}"),
    (Method::Get, "/session/{}/children"),
    (Method::Post, "/api/session"),
    (Method::Get, "/api/session/{}"),
    (Method::Get, "/api/session/active"),
    (Method::Get, "/api/session/{}/message/{}"),
    // Turns.
    (Method::Post, "/session/{}/prompt_async"),
    (Method::Post, "/session/{}/abort"),
    (Method::Post, "/api/session/{}/prompt"),
    (Method::Post, "/api/session/{}/interrupt"),
    (Method::Post, "/api/session/{}/model"),
    (Method::Post, "/api/session/{}/agent"),
    // Requests the agent is blocked on: list them, answer them.
    (Method::Get, "/permission"),
    (Method::Get, "/question"),
    (Method::Get, "/api/session/{}/permission"),
    (Method::Get, "/api/session/{}/form"),
    (Method::Post, "/permission/{}/reply"),
    (Method::Post, "/session/{}/permissions/{}"),
    (Method::Post, "/question/{}/reply"),
    (Method::Post, "/question/{}/reject"),
    (Method::Post, "/api/session/{}/permission/{}/reply"),
    (Method::Post, "/api/session/{}/form/{}/reply"),
];

/// Whether `request` is one Reno may send. Everything outside [`ALLOWED`] is refused: config and
/// auth writers, MCP and plugin management, upgrades, session mutation and deletion.
pub(crate) fn permitted(request: &Request) -> bool {
    let path: Vec<&str> = request.path.split('/').collect();
    ALLOWED.iter().any(|(method, shape)| {
        let shape: Vec<&str> = shape.split('/').collect();
        *method == request.method
            && shape.len() == path.len()
            && shape.iter().zip(&path).all(|(want, got)| {
                if *want == "{}" {
                    !got.is_empty()
                } else {
                    want == got
                }
            })
    })
}

/// The one error a refused request becomes.
pub(crate) fn refusal(request: &Request) -> String {
    format!(
        "refused: Reno does not send {:?} {} to opencode (it is not on the list of routes Reno \
         uses, and opencode's configuration and credentials are never written by Reno)",
        request.method, request.path
    )
}

/// One path segment for an id the server minted: everything outside `[A-Za-z0-9_-]` is
/// percent-encoded, so an id can never add a segment, a query or a `..`.
fn seg(id: &str) -> String {
    let mut out = String::with_capacity(id.len());
    for b in id.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Whether `id` looks like an opencode session id (`ses_` and letters and digits): a session to
/// resume that does not is refused before a server is started.
pub(crate) fn valid_session_id(id: &str) -> bool {
    id.strip_prefix("ses_").is_some_and(|rest| {
        !rest.is_empty() && rest.len() <= 64 && rest.bytes().all(|b| b.is_ascii_alphanumeric())
    })
}

/// How a permission request is answered. There is no `always`: it would save a rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reply {
    /// This one call.
    Once,
    /// Refuse it.
    Reject,
}

impl Reply {
    fn word(self) -> &'static str {
        match self {
            Self::Once => "once",
            Self::Reject => "reject",
        }
    }
}

/// `POST` a session.
pub(crate) fn create_session(
    protocol: Protocol,
    dir: Option<&str>,
    agent: Option<&str>,
) -> Request {
    match protocol {
        // 1.x scopes by directory (header and query), the body is empty.
        Protocol::V1 => Request::post("/session".into(), json!({})),
        // 2.x takes the directory in the BODY: the header is ignored on this route.
        Protocol::V2 => {
            let mut body = match dir {
                Some(dir) => json!({"location": {"directory": dir}}),
                None => json!({}),
            };
            if let Some(agent) = agent {
                body["agent"] = json!(agent);
            }
            Request::post("/api/session".into(), body)
        }
    }
}

/// `GET` one session (resume).
pub(crate) fn get_session(protocol: Protocol, id: &str) -> Request {
    Request::get(match protocol {
        Protocol::V1 => format!("/session/{}", seg(id)),
        Protocol::V2 => format!("/api/session/{}", seg(id)),
    })
}

/// The map of running sessions.
pub(crate) fn session_activity(protocol: Protocol) -> Request {
    Request::get(
        match protocol {
            Protocol::V1 => "/session/status",
            Protocol::V2 => "/api/session/active",
        }
        .into(),
    )
}

/// Whether `activity` (the body of [`session_activity`], unwrapped) lists `session` as running.
pub(crate) fn is_running(activity: &Value, session: &str) -> bool {
    unwrap_data(activity)
        .get(session)
        .is_some_and(|state| state.get("type").and_then(Value::as_str) != Some("idle"))
}

/// The prompt: 1.x names model, variant and agent on each prompt (`prompt_async`, a 204); 2.x set
/// the model and agent on the session and takes text only. `message_id` makes a prompt whose
/// answer was lost checkable and a retry safe.
pub(crate) fn prompt(
    protocol: Protocol,
    session: &str,
    text: &str,
    model: Option<(&str, &str)>,
    variant: Option<&str>,
    agent: Option<&str>,
    message_id: &str,
) -> Request {
    match protocol {
        Protocol::V1 => {
            let mut body = json!({
                "messageID": message_id,
                "parts": [{"type": "text", "text": text}],
            });
            if let Some((provider, model)) = model {
                body["model"] = json!({"providerID": provider, "modelID": model});
            }
            if let Some(variant) = variant {
                body["variant"] = json!(variant);
            }
            // A prompt naming no agent runs the default one.
            if let Some(agent) = agent {
                body["agent"] = json!(agent);
            }
            Request::post(format!("/session/{}/prompt_async", seg(session)), body)
        }
        Protocol::V2 => Request::post(
            format!("/api/session/{}/prompt", seg(session)),
            json!({"id": message_id, "text": text, "files": []}),
        ),
    }
}

/// 2.x: the model (and reasoning variant) of the session.
pub(crate) fn set_session_model(
    session: &str,
    provider: &str,
    model: &str,
    variant: Option<&str>,
) -> Request {
    let mut model_ref = json!({"providerID": provider, "id": model});
    if let Some(variant) = variant {
        model_ref["variant"] = json!(variant);
    }
    Request::post(
        format!("/api/session/{}/model", seg(session)),
        json!({"model": model_ref}),
    )
}

/// 2.x: the agent of the session.
pub(crate) fn set_session_agent(session: &str, agent: &str) -> Request {
    Request::post(
        format!("/api/session/{}/agent", seg(session)),
        json!({"agent": agent}),
    )
}

/// End the session's turn.
pub(crate) fn abort(protocol: Protocol, session: &str) -> Request {
    match protocol {
        Protocol::V1 => Request::post(format!("/session/{}/abort", seg(session)), json!({})),
        Protocol::V2 => Request::post(
            format!("/api/session/{}/interrupt", seg(session)),
            json!({}),
        ),
    }
}

/// The routes that answer a permission request, in the order to try them. 1.x has the global
/// route and a session-scoped fallback (`response`), 2.x the session's route; its key is `reply`
/// before 2.0.4 and `decision` from it (2.3.2 answers 400 `Missing key ["decision"]` to
/// `{"reply": ..}`).
pub(crate) fn permission_reply(
    protocol: Protocol,
    version: Option<&ServerVersion>,
    session: &str,
    request_id: &str,
    reply: Reply,
) -> Vec<Request> {
    match protocol {
        Protocol::V1 => vec![
            Request::post(
                format!("/permission/{}/reply", seg(request_id)),
                json!({"reply": reply.word()}),
            ),
            Request::post(
                format!("/session/{}/permissions/{}", seg(session), seg(request_id)),
                json!({"response": reply.word()}),
            ),
        ],
        Protocol::V2 => {
            let key = if version.is_some_and(|v| v.at_least((2, 0, 4))) {
                "decision"
            } else {
                "reply"
            };
            vec![Request::post(
                format!(
                    "/api/session/{}/permission/{}/reply",
                    seg(session),
                    seg(request_id)
                ),
                json!({ key: reply.word() }),
            )]
        }
    }
}

/// 1.x: the answers to a `question.asked`, one list of labels per question, in order.
pub(crate) fn question_reply(request_id: &str, answers: &[Vec<String>]) -> Request {
    Request::post(
        format!("/question/{}/reply", seg(request_id)),
        json!({"answers": answers}),
    )
}

/// 1.x: decline a `question.asked`.
pub(crate) fn question_reject(request_id: &str) -> Request {
    Request::post(format!("/question/{}/reject", seg(request_id)), Value::Null)
}

/// 2.x: the answers to a question form, keyed by field (`{"answer": {"q0": ..}}`).
pub(crate) fn form_reply(session: &str, form_id: &str, body: Value) -> Request {
    Request::post(
        format!("/api/session/{}/form/{}/reply", seg(session), seg(form_id)),
        body,
    )
}

/// The requests still waiting for an answer, as the routes that list them: the bus never replays,
/// so a request asked while it was reconnecting exists only here.
pub(crate) fn pending(protocol: Protocol, sessions: &[String]) -> Vec<Request> {
    match protocol {
        Protocol::V1 => vec![
            Request::get("/permission".into()),
            Request::get("/question".into()),
        ],
        Protocol::V2 => sessions
            .iter()
            .flat_map(|s| {
                [
                    Request::get(format!("/api/session/{}/permission", seg(s))),
                    Request::get(format!("/api/session/{}/form", seg(s))),
                ]
            })
            .collect(),
    }
}

/// One stored message, to see whether a prompt whose answer was lost landed.
pub(crate) fn get_message(protocol: Protocol, session: &str, message: &str) -> Request {
    Request::get(match protocol {
        Protocol::V1 => format!("/session/{}/message/{}", seg(session), seg(message)),
        Protocol::V2 => format!("/api/session/{}/message/{}", seg(session), seg(message)),
    })
}

/// 1.x: a session's child (subagent) sessions.
pub(crate) fn children(session: &str) -> Request {
    Request::get(format!("/session/{}/children", seg(session)))
}

/// The provider catalog: `/provider` (1.x) or the flat enabled-model list `/api/model` (2.x).
pub(crate) fn models(protocol: Protocol) -> Request {
    Request::get(
        match protocol {
            Protocol::V1 => "/provider",
            Protocol::V2 => "/api/model",
        }
        .into(),
    )
}

/// The agents.
pub(crate) fn agents(protocol: Protocol) -> Request {
    Request::get(
        match protocol {
            Protocol::V1 => "/agent",
            Protocol::V2 => "/api/agent",
        }
        .into(),
    )
}

/// 2.x: one agent with its merged permission rules (`/api/agent` lists them too, but a list
/// fetched at startup raced the config load on 2.3.2 and lacked the inline rules).
pub(crate) fn agent(id: &str) -> Request {
    Request::get(format!("/api/agent/{}", seg(id)))
}

/// The MCP servers the server registered.
pub(crate) fn mcp_listing(protocol: Protocol) -> Request {
    Request::get(
        match protocol {
            Protocol::V1 => "/mcp",
            Protocol::V2 => "/api/mcp",
        }
        .into(),
    )
}

/// 2.x wraps answers in `{data}`; unwrap when present.
pub(crate) fn unwrap_data(value: &Value) -> &Value {
    value
        .get("data")
        .filter(|d| d.is_object() || d.is_array())
        .unwrap_or(value)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn post(path: &str) -> Request {
        Request::post(path.into(), Value::Null)
    }

    #[test]
    fn every_builder_makes_a_request_the_guard_permits() {
        let v2_new = ServerVersion::parse("2.3.2");
        let mut all = vec![
            create_session(Protocol::V1, Some("/w"), None),
            create_session(Protocol::V2, Some("/w"), Some("plan")),
            get_session(Protocol::V1, "ses_a"),
            get_session(Protocol::V2, "ses_a"),
            session_activity(Protocol::V1),
            session_activity(Protocol::V2),
            prompt(
                Protocol::V1,
                "ses_a",
                "hi",
                Some(("p", "m")),
                Some("high"),
                None,
                "msg_1",
            ),
            prompt(Protocol::V2, "ses_a", "hi", None, None, None, "msg_1"),
            set_session_model("ses_a", "p", "m", None),
            set_session_agent("ses_a", "plan"),
            abort(Protocol::V1, "ses_a"),
            abort(Protocol::V2, "ses_a"),
            question_reply("que_1", &[vec!["a".into()]]),
            question_reject("que_1"),
            form_reply("ses_a", "frm_1", json!({"answer": {}})),
            get_message(Protocol::V1, "ses_a", "msg_1"),
            get_message(Protocol::V2, "ses_a", "msg_1"),
            children("ses_a"),
            models(Protocol::V1),
            models(Protocol::V2),
            agents(Protocol::V1),
            agents(Protocol::V2),
            agent("build"),
            mcp_listing(Protocol::V1),
            mcp_listing(Protocol::V2),
        ];
        all.extend(permission_reply(
            Protocol::V1,
            None,
            "ses_a",
            "per_1",
            Reply::Once,
        ));
        all.extend(permission_reply(
            Protocol::V2,
            Some(&v2_new),
            "ses_a",
            "per_1",
            Reply::Reject,
        ));
        all.extend(pending(Protocol::V1, &[]));
        all.extend(pending(Protocol::V2, &["ses_a".into()]));
        for request in all {
            assert!(permitted(&request), "{request:?}");
        }
    }

    /// Every route of opencode 1.19.5's `/doc` and 2.3.2's route table that writes the owner's
    /// configuration, credentials or sessions, or manages MCP servers, plugins or the install.
    #[test]
    fn the_routes_that_write_opencodes_config_or_auth_are_refused() {
        for path in [
            "/config",
            "/global/config",
            "/auth/anthropic",
            "/provider/anthropic/oauth/authorize",
            "/provider/anthropic/oauth/callback",
            "/api/credential/cred_1",
            "/api/credential/cred_1/activate",
            "/api/integration/anthropic/connect/key",
            "/api/integration/anthropic/connect/oauth",
            "/api/integration/anthropic/connect/command",
            "/api/integration/attempt/att_1/complete",
            "/api/permission/saved",
            "/api/permission/saved/per_1",
            "/api/config",
            "/api/config/shell",
            "/api/experimental/config",
            "/api/location/reload",
            "/api/plugin/update",
            "/mcp/reno/connect",
            "/mcp/reno/auth",
            "/api/mcp/resource",
            "/global/upgrade",
            "/global/dispose",
            "/instance/dispose",
            "/session/ses_a/share",
            "/session/ses_a/revert",
            "/session/ses_a/init",
            "/session/ses_a/shell",
            "/session/ses_a/summarize",
            "/session/ses_a/command",
            "/session/ses_a/fork",
            "/api/session/ses_a/compact",
            "/api/session/ses_a/revert/commit",
            "/api/session/ses_a/shell",
            "/api/session/ses_a/command",
            "/api/experimental/fs/write",
            "/api/pty",
            "/pty",
            "/tui/execute-command",
            "/log",
            "/sync/replay",
        ] {
            for method in [Method::Get, Method::Post] {
                let request = Request {
                    method,
                    path: path.into(),
                    body: None,
                };
                // A few of these are readable on a server, but Reno has no use for them.
                assert!(!permitted(&request), "{method:?} {path}");
            }
        }
        // Listing the registered MCP servers is a read; registering one is not.
        assert!(!permitted(&post("/mcp")));
        assert!(!permitted(&post("/api/mcp")));
        // A permitted shape with an extra or missing segment is another route.
        assert!(!permitted(&post("/session/ses_a/prompt_async/extra")));
        assert!(!permitted(&post("/session//abort")));
        assert!(!permitted(&Request::get("/session/ses_a/abort".into())));
    }

    #[test]
    fn an_id_cannot_add_a_segment_a_query_or_a_dot_dot() {
        let request = abort(Protocol::V1, "ses_a/../../config?x=1");
        assert_eq!(
            request.path,
            "/session/ses_a%2F%2E%2E%2F%2E%2E%2Fconfig%3Fx%3D1/abort"
        );
        assert!(permitted(&request));
        assert_eq!(request.path.matches('/').count(), 3);
        let request = get_session(Protocol::V2, "..");
        assert_eq!(request.path, "/api/session/%2E%2E");
    }

    #[test]
    fn a_permission_reply_is_once_or_reject_and_the_key_follows_the_version() {
        let new = ServerVersion::parse("2.3.2");
        let old = ServerVersion::parse("2.0.3");
        let body = |version: &ServerVersion| {
            permission_reply(Protocol::V2, Some(version), "ses_a", "per_1", Reply::Once)[0]
                .body
                .clone()
                .unwrap()
        };
        assert_eq!(body(&new), json!({"decision": "once"}));
        assert_eq!(body(&old), json!({"reply": "once"}));
        let routes = permission_reply(Protocol::V1, None, "ses_a", "per_1", Reply::Reject);
        assert_eq!(routes[0].body, Some(json!({"reply": "reject"})));
        assert_eq!(routes[1].body, Some(json!({"response": "reject"})));
        // `always` saves a durable rule: the wire word does not exist in this layer.
        assert!(
            !format!("{:?}", Reply::Once)
                .to_lowercase()
                .contains("always")
        );
    }

    #[test]
    fn a_session_to_resume_must_look_like_one() {
        assert!(valid_session_id("ses_e11857550ffe2haMqKIdHvcaY0"));
        for bad in [
            "", "ses_", "ses_a/b", "msg_abc", "ses_ a", "../ses_a", "ses_é",
        ] {
            assert!(!valid_session_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_v1_prompt_names_model_variant_and_agent_and_the_v2_one_only_text() {
        let v1 = prompt(
            Protocol::V1,
            "ses_a",
            "hi",
            Some(("anthropic", "claude")),
            Some("high"),
            Some("plan"),
            "msg_1",
        );
        assert_eq!(v1.path, "/session/ses_a/prompt_async");
        assert_eq!(
            v1.body.unwrap(),
            json!({
                "messageID": "msg_1",
                "parts": [{"type": "text", "text": "hi"}],
                "model": {"providerID": "anthropic", "modelID": "claude"},
                "variant": "high",
                "agent": "plan",
            })
        );
        let v2 = prompt(
            Protocol::V2,
            "ses_a",
            "hi",
            Some(("anthropic", "claude")),
            None,
            Some("plan"),
            "msg_1",
        );
        assert_eq!(v2.path, "/api/session/ses_a/prompt");
        assert_eq!(
            v2.body.unwrap(),
            json!({"id": "msg_1", "text": "hi", "files": []})
        );
    }

    #[test]
    fn running_is_anything_but_idle_and_two_x_wraps_the_map() {
        assert!(is_running(&json!({"ses_a": {"type": "busy"}}), "ses_a"));
        assert!(is_running(
            &json!({"data": {"ses_a": {"type": "retry"}}}),
            "ses_a"
        ));
        assert!(!is_running(&json!({"ses_a": {"type": "idle"}}), "ses_a"));
        assert!(!is_running(&json!({}), "ses_a"));
    }
}
