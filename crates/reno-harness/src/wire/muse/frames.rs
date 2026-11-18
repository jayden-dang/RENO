//! MSP frames: what comes in, what may go out, and the requests Reno builds.
//!
//! Requests carry integer ids the client mints. Every state-changing method is a COMMAND whose
//! params also carry a client-minted UUIDv7 `commandId` (the server's idempotency handle: it
//! never mints one itself, and a fresh turn's id IS its `turn/start` command id). Queries carry
//! none. The builders here take the command id as an argument, because minting one reads the
//! clock.
//!
//! # What may be sent
//!
//! 1.4.23 serves 133 methods. Reno sends [`SENT`] and nothing else: the one writer in the driver
//! refuses any other method. [`WRITES_CONFIG`] names the served methods that write Muse's own
//! configuration, credentials, trust, plugins or settings; none is in [`SENT`], and a test proves
//! it. Reno never writes a harness's configuration (the owner's rule), so a run that wanted one
//! of those would be a bug, and the refusal makes it a failed run instead of a changed machine.

use serde_json::{Value, json};

/// The protocol envelope version this wire speaks.
pub(crate) const SCHEMA_VERSION: u64 = 1;

/// The methods Reno sends. Everything else is refused at the writer.
pub(crate) const SENT: &[&str] = &[
    "initialize",
    "account/read",
    "model/list",
    "session/start",
    "session/resume",
    "session/setApprovalMode",
    "turn/start",
    "turn/interrupt",
    "approval/decide",
    "userInput/answer",
    "userInput/cancel",
];

/// The notifications Reno sends.
pub(crate) const SENT_NOTIFICATIONS: &[&str] = &["initialized"];

/// Served methods that write what the owner configured in Muse, or sign them in or out, or reach
/// out as them. Never sent. The list is read off the 1.4.23 schema (`muse schema
/// generate-json-schema --experimental`); `session/setModel` is here because the schema calls its
/// selection "durable" and whether that reaches Muse's settings was not established.
pub(crate) const WRITES_CONFIG: &[&str] = &[
    "account/loginStart",
    "account/loginCancel",
    "account/logout",
    "settings/writeMany",
    "settings/writeValue",
    "mcpServer/oauthLogin",
    "mcpServer/oauthLoginCancel",
    "mcpServer/oauthLogout",
    "skill/setActivation",
    "skill/import",
    "plugin/install",
    "plugin/marketplaceAdd",
    "plugin/marketplaceRemove",
    "plugin/marketplaceUpdate",
    "computerUseSettings/update",
    "computerUseSetup/start",
    "feedback/submit",
    "usage/refresh",
    "session/setModel",
    "session/setPermissionProfile",
    "session/setReasoningEffort",
    "session/rename",
    "host/shutdown",
];

/// Methods that are queries: no `commandId`.
pub(crate) const QUERIES: &[&str] = &["initialize", "account/read", "model/list"];

/// Whether Reno may send `method` as a request.
pub(crate) fn may_send(method: &str) -> bool {
    SENT.contains(&method)
}

/// Whether Reno may send `method` as a notification.
pub(crate) fn may_notify(method: &str) -> bool {
    SENT_NOTIFICATIONS.contains(&method)
}

/// Whether `method` is a query (carries no `commandId`).
pub(crate) fn is_query(method: &str) -> bool {
    QUERIES.contains(&method)
}

/// A string member, or `""`.
pub(crate) fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value[key].as_str().unwrap_or("")
}

/// A server-authored MSP error: its `data.kind` is the machine-readable cause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    /// `data.kind`.
    pub(crate) kind: String,
    /// The error's own words.
    pub(crate) message: String,
}

/// One decoded inbound frame.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Inbound {
    /// The answer to one of Reno's requests.
    Response {
        /// The id Reno sent.
        id: u64,
        /// The result, or the host's refusal.
        result: Result<Value, Failure>,
    },
    /// The host asking something (`approval/request`, `userInput/request`). What it asks also
    /// arrives as a notification, which is where the run acts on it; the request itself is
    /// answered with an empty result.
    Request {
        /// The host's id, echoed in the answer.
        id: Value,
        /// The method.
        method: String,
    },
    /// A notification.
    Notification {
        /// The method.
        method: String,
        /// Its params (null when it has none).
        params: Value,
    },
}

/// `frame` as an [`Inbound`], or why it is not one.
pub(crate) fn decode(frame: &Value) -> Result<Inbound, String> {
    if frame["jsonrpc"] != "2.0" {
        return Err("Muse sent a frame that is not JSON-RPC 2.0".into());
    }
    if let Some(method) = frame["method"].as_str() {
        return Ok(match frame.get("id") {
            Some(id) if !id.is_null() => Inbound::Request {
                id: id.clone(),
                method: method.into(),
            },
            _ => Inbound::Notification {
                method: method.into(),
                params: frame.get("params").cloned().unwrap_or(Value::Null),
            },
        });
    }
    let id = frame["id"]
        .as_u64()
        .ok_or("Muse response has no usable id")?;
    let result = match (frame.get("result"), frame.get("error")) {
        (Some(result), None) => Ok(result.clone()),
        (None, Some(error)) => Err(Failure {
            kind: error["data"]["kind"].as_str().unwrap_or("unknown").into(),
            message: error["message"].as_str().unwrap_or("request failed").into(),
        }),
        _ => return Err("Muse response must carry exactly one of result or error".into()),
    };
    Ok(Inbound::Response { id, result })
}

/// A request frame.
pub(crate) fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// A notification frame.
pub(crate) fn notification(method: &str) -> Value {
    json!({"jsonrpc": "2.0", "method": method})
}

/// The answer to a host request.
pub(crate) fn reply(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

/// The refusal of a host request Reno does not serve.
pub(crate) fn reply_not_found(id: &Value, method: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {
        "code": -32601,
        "message": format!("method not found: {method}"),
        "data": {"kind": "methodNotFound", "retryable": false}
    }})
}

/// `initialize` params. `experimental` opts into the experimental surface (the account methods
/// live there in 1.4); `session_mcp` asks for session-scoped MCP servers.
pub(crate) fn initialize(version: &str, experimental: bool, session_mcp: bool) -> Value {
    json!({
        "clientInfo": {"name": "reno", "title": "Reno", "version": version},
        "capabilities": {
            "requestedCapabilities": if session_mcp { json!(["sessionMcp"]) } else { json!([]) },
            "experimentalApi": experimental,
        },
    })
}

/// What `initialize` answered that the driver uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hello {
    /// `serverInfo.version`: the CLI's own version, with no launcher or build suffix.
    pub(crate) version: Option<String>,
    /// Whether the host takes session-scoped MCP servers.
    pub(crate) session_mcp: bool,
}

/// Read the `initialize` result, refusing an envelope this wire does not speak.
pub(crate) fn hello(result: &Value) -> Result<Hello, String> {
    if result["schema"]["version"].as_u64() != Some(SCHEMA_VERSION) {
        return Err(
            "Muse speaks an unsupported protocol version; update Reno or Muse Code".to_owned(),
        );
    }
    Ok(Hello {
        version: result["serverInfo"]["version"].as_str().map(str::to_owned),
        session_mcp: result["grantedCapabilities"]
            .as_array()
            .is_some_and(|granted| granted.iter().any(|c| c == "sessionMcp")),
    })
}

/// What a `session/start` asks.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Start<'a> {
    /// The new session's id (a UUIDv7).
    pub(crate) session: &'a str,
    /// The command id.
    pub(crate) command: &'a str,
    /// The canonical workspace root.
    pub(crate) root: &'a str,
    /// The provider (`meta`).
    pub(crate) provider: &'a str,
    /// The model, when the run names one.
    pub(crate) model: Option<&'a str>,
    /// The posture.
    pub(crate) posture: super::launch::Posture,
    /// `{"mcpServers": ...}`, when there are servers and the host takes them.
    pub(crate) config: Option<&'a Value>,
}

/// `session/start` params.
pub(crate) fn session_start(start: &Start<'_>) -> Value {
    let mut params = json!({
        "commandId": start.command,
        "sessionId": start.session,
        "workspaceRoot": start.root,
        "providerId": start.provider,
        "approvalMode": start.posture.approval_mode,
        "permissionProfile": start.posture.profile,
    });
    if let Some(model) = start.model {
        params["modelId"] = json!(model);
    }
    if let Some(config) = start.config {
        params["config"] = config.clone();
    }
    params
}

/// `session/resume` params. The history stays on the host (`excludeItems`); a resumed session
/// keeps the approval mode it last had, which is why the run sets its own right after.
pub(crate) fn session_resume(
    session: &str,
    command: &str,
    profile: &str,
    model: Option<&str>,
    config: Option<&Value>,
) -> Value {
    let mut params = json!({
        "commandId": command,
        "sessionId": session,
        "excludeItems": true,
        "permissionProfile": profile,
    });
    if let Some(model) = model {
        params["modelId"] = json!(model);
    }
    if let Some(config) = config {
        params["config"] = config.clone();
    }
    params
}

/// `session/setApprovalMode` params.
pub(crate) fn set_approval_mode(session: &str, command: &str, mode: &str) -> Value {
    json!({"commandId": command, "sessionId": session, "mode": mode})
}

/// How `turn/start` meets a turn that is already running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IfBusy {
    /// The opening prompt: start a turn.
    Queue,
    /// A steering prompt: steer the running turn, or start one when idle, in one atomic command.
    Steer,
}

/// `turn/start` params. `turn` is the command id, which is also the id the new turn gets. A
/// steer never carries `workspaceRoots`: it is a sticky replacement, bound at a launch, and Muse
/// refuses it on a steer that may launch no turn.
pub(crate) fn turn_start(
    session: &str,
    turn: &str,
    prompt: &str,
    effort: Option<&str>,
    if_busy: IfBusy,
    root: &str,
) -> Value {
    let text = if prompt.is_empty() {
        "(no message)"
    } else {
        prompt
    };
    let mut params = json!({
        "commandId": turn,
        "sessionId": session,
        "input": [{"type": "text", "text": text}],
        "displayText": prompt,
        "ifBusy": match if_busy { IfBusy::Queue => "queue", IfBusy::Steer => "steer" },
    });
    if if_busy == IfBusy::Queue {
        params["workspaceRoots"] = json!([root]);
    }
    if let Some(effort) = effort {
        params["reasoningEffort"] = json!(effort);
    }
    params
}

/// `turn/interrupt` params, for the exact turn.
pub(crate) fn turn_interrupt(session: &str, command: &str, turn: &str) -> Value {
    json!({"commandId": command, "sessionId": session, "turnId": turn})
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn nothing_that_writes_muses_configuration_may_be_sent() {
        for method in WRITES_CONFIG {
            assert!(!may_send(method), "{method} is on the allowlist");
            assert!(!may_notify(method), "{method} is on the notification list");
        }
        for method in SENT {
            assert!(!WRITES_CONFIG.contains(method), "{method}");
        }
        assert!(may_send("turn/start"));
        assert!(!may_send("settings/writeValue"));
        assert!(!may_send("not/a/method"));
    }

    #[test]
    fn the_lists_have_no_duplicates() {
        for table in [SENT, WRITES_CONFIG, QUERIES, SENT_NOTIFICATIONS] {
            let mut sorted = table.to_vec();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), table.len());
        }
        for query in QUERIES {
            assert!(may_send(query), "{query}");
        }
    }

    #[test]
    fn frames_decode_as_what_they_are() {
        let response = decode(&json!({"jsonrpc":"2.0","id":3,"result":{"a":1}})).unwrap();
        assert!(matches!(
            response,
            Inbound::Response {
                id: 3,
                result: Ok(_)
            }
        ));
        let refused = decode(&json!({"jsonrpc":"2.0","id":4,"error":{"code":-32020,
            "message":"gone","data":{"kind":"sessionNotFound"}}}))
        .unwrap();
        assert_eq!(
            refused,
            Inbound::Response {
                id: 4,
                result: Err(Failure {
                    kind: "sessionNotFound".into(),
                    message: "gone".into()
                })
            }
        );
        assert!(matches!(
            decode(&json!({"jsonrpc":"2.0","id":7,"method":"approval/request","params":{}}))
                .unwrap(),
            Inbound::Request { method, .. } if method == "approval/request"
        ));
        assert!(matches!(
            decode(&json!({"jsonrpc":"2.0","method":"item/delta","params":{"x":1}})).unwrap(),
            Inbound::Notification { method, params } if method == "item/delta" && params["x"] == 1
        ));
        assert!(decode(&json!({"id":1,"result":{}})).is_err());
        assert!(decode(&json!({"jsonrpc":"2.0","id":1})).is_err());
        assert!(decode(&json!({"jsonrpc":"2.0","id":"x","result":{}})).is_err());
    }

    #[test]
    fn the_handshake_checks_the_envelope_and_the_granted_capabilities() {
        let ok = json!({"schema":{"version":1},"serverInfo":{"name":"muse","version":"1.4.23"},
            "grantedCapabilities":["sessionMcp"]});
        assert_eq!(
            hello(&ok).unwrap(),
            Hello {
                version: Some("1.4.23".into()),
                session_mcp: true
            }
        );
        let none = json!({"schema":{"version":1},"grantedCapabilities":[]});
        assert_eq!(
            hello(&none).unwrap(),
            Hello {
                version: None,
                session_mcp: false
            }
        );
        assert!(hello(&json!({"schema":{"version":2}})).is_err());
        assert!(hello(&json!({})).is_err());
    }

    #[test]
    fn a_steer_never_rebinds_the_workspace_root_and_an_opening_prompt_pins_it() {
        let opening = turn_start("s", "t", "do it", Some("high"), IfBusy::Queue, "/w");
        assert_eq!(opening["workspaceRoots"], json!(["/w"]));
        assert_eq!(opening["ifBusy"], "queue");
        assert_eq!(opening["reasoningEffort"], "high");
        assert_eq!(opening["input"][0]["text"], "do it");
        let steer = turn_start("s", "t2", "wait", None, IfBusy::Steer, "/w");
        assert!(steer.get("workspaceRoots").is_none());
        assert_eq!(steer["ifBusy"], "steer");
        assert!(steer.get("reasoningEffort").is_none());
        // Muse rejects an empty input: an empty prompt becomes a placeholder part.
        assert_eq!(
            turn_start("s", "t", "", None, IfBusy::Queue, "/w")["input"][0]["text"],
            "(no message)"
        );
    }

    #[test]
    fn a_start_names_its_posture_model_and_servers_only_when_given() {
        let posture = super::super::launch::Posture::ASK;
        let bare = session_start(&Start {
            session: "s",
            command: "c",
            root: "/w",
            provider: "meta",
            model: None,
            posture,
            config: None,
        });
        assert_eq!(bare["approvalMode"], "promptUnmatched");
        assert_eq!(bare["permissionProfile"], ":ask-me");
        assert!(bare.get("modelId").is_none() && bare.get("config").is_none());
        let config = json!({"mcpServers": {}});
        let full = session_start(&Start {
            model: Some("muse-spark-1.3"),
            config: Some(&config),
            ..Start {
                session: "s",
                command: "c",
                root: "/w",
                provider: "meta",
                model: None,
                posture,
                config: None,
            }
        });
        assert_eq!(full["modelId"], "muse-spark-1.3");
        assert_eq!(full["config"], config);
        let resume = session_resume("s", "c", ":ask-me", Some("m"), None);
        assert_eq!(resume["excludeItems"], true);
        assert_eq!(resume["modelId"], "m");
    }
}
