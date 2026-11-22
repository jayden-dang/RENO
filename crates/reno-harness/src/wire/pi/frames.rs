//! Commands Reno sends to Pi, and the shapes of the replies it reads.
//!
//! # Commands that are never sent
//!
//! In Pi 1.5.0 these RPC commands write Pi's own settings file (`~/.pi/agent/settings.json`)
//! through `settingsManager.set*` (checked in `dist/core/agent-session.js` and
//! `dist/modes/rpc/rpc-mode.js`): `set_steering_mode`, `set_follow_up_mode`,
//! `set_auto_compaction`, `set_auto_retry`, `cycle_model` and `cycle_thinking_level`. Reno never
//! writes a harness's own configuration, so [`writes_pi_settings`] names them and the driver's
//! writer refuses them (a test drives a run and fails on any such frame). `set_model` and
//! `set_thinking_level` are fine: RPC mode calls them without `persist`, so they land in the
//! session transcript only. Steering uses `prompt` with `streamingBehavior: "steer"`.

use serde_json::{Value, json};

/// Whether sending command `kind` would make Pi write its own settings (see the module docs).
pub(crate) fn writes_pi_settings(kind: &str) -> bool {
    matches!(
        kind,
        "set_steering_mode"
            | "set_follow_up_mode"
            | "set_auto_compaction"
            | "set_auto_retry"
            | "cycle_model"
            | "cycle_thinking_level"
    )
}

/// `{"type":"get_state"}`.
pub(crate) fn get_state() -> Value {
    json!({ "type": "get_state" })
}

/// `{"type":"get_commands"}`: the extension commands, prompt templates and skills Pi has loaded.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the driver arms the gate with it from Task 6 on")
)]
pub(crate) fn get_commands() -> Value {
    json!({ "type": "get_commands" })
}

/// Whether the `data` of a `get_commands` reply lists the extension command `name`: an entry
/// named `name` whose `source` is `extension`, so a skill or prompt template of that name does
/// not count.
#[cfg_attr(
    not(test),
    expect(dead_code, reason = "the driver arms the gate with it from Task 6 on")
)]
pub(crate) fn lists_extension_command(data: &Value, name: &str) -> bool {
    data.get("commands")
        .and_then(Value::as_array)
        .is_some_and(|commands| {
            commands.iter().any(|command| {
                string(command, "name") == name && string(command, "source") == "extension"
            })
        })
}

/// `{"type":"get_available_models"}`: the models of every provider Pi holds a credential for.
pub(crate) fn get_available_models() -> Value {
    json!({ "type": "get_available_models" })
}

/// Select `provider`/`model_id` for this session only (no `persist`: the transcript, not the
/// settings file).
pub(crate) fn set_model(provider: &str, model_id: &str) -> Value {
    json!({ "type": "set_model", "provider": provider, "modelId": model_id })
}

/// Set the thinking level for this session. Pi clamps a level the model lacks to the nearest it
/// has, so the caller need not ask which levels exist.
pub(crate) fn set_thinking_level(level: &str) -> Value {
    json!({ "type": "set_thinking_level", "level": level })
}

/// The task prompt, which starts a run.
pub(crate) fn prompt(message: &str) -> Value {
    json!({ "type": "prompt", "message": message })
}

/// A steering message: queued behind the current assistant turn's tool calls, before the next
/// model call. Sent as a `prompt` with `streamingBehavior`, which also starts a run if Pi has
/// just gone idle: one atomic operation, so no idle race can strand it (a separate `steer`
/// command would queue it with no run to take it).
pub(crate) fn steer(message: &str) -> Value {
    json!({ "type": "prompt", "message": message, "streamingBehavior": "steer" })
}

/// Drop the steering and follow-up queues (before an abort, so queued input does not restart the
/// run).
pub(crate) fn clear_queue() -> Value {
    json!({ "type": "clear_queue" })
}

/// Abort the current operation and reply once Pi is idle.
pub(crate) fn abort() -> Value {
    json!({ "type": "abort" })
}

/// The answer to a dialog: a chosen or typed value.
pub(crate) fn ui_value(id: &Value, value: &str) -> Value {
    json!({ "type": "extension_ui_response", "id": id, "value": value })
}

/// The answer to a `confirm` dialog.
pub(crate) fn ui_confirmed(id: &Value, confirmed: bool) -> Value {
    json!({ "type": "extension_ui_response", "id": id, "confirmed": confirmed })
}

/// Dismiss a dialog: the extension sees `undefined` (or `false` for a confirmation).
pub(crate) fn ui_cancelled(id: &Value) -> Value {
    json!({ "type": "extension_ui_response", "id": id, "cancelled": true })
}

/// A string field, empty when absent or not a string.
pub(crate) fn string<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// The reply to one command: the outcome Pi gave and the `data` it carried.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Reply {
    /// The `id` the command carried.
    pub(crate) id: String,
    /// The command the reply is for (`prompt`, `get_state`, ...).
    pub(crate) command: String,
    /// `data` on success, Pi's error text on failure.
    pub(crate) outcome: Result<Value, String>,
}

impl Reply {
    /// The reply in `frame`, when it is one.
    pub(crate) fn parse(frame: &Value) -> Option<Self> {
        if string(frame, "type") != "response" {
            return None;
        }
        let command = string(frame, "command").to_owned();
        let outcome = if frame.get("success").and_then(Value::as_bool) == Some(true) {
            Ok(frame.get("data").cloned().unwrap_or(Value::Null))
        } else {
            let error = string(frame, "error");
            Err(if error.is_empty() {
                format!("Pi {command}: command failed")
            } else {
                format!("Pi {command}: {error}")
            })
        };
        Some(Self {
            id: frame.get("id").map_or_else(String::new, |id| match id {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            }),
            command,
            outcome,
        })
    }
}

/// What Pi did with a submitted prompt (`data.disposition` of a successful `prompt` reply).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Disposition {
    /// A run started for it.
    Started,
    /// It joined the run in progress (a steering message, or a follow-up).
    Queued,
    /// An extension command or input handler consumed it: no run started for it, though one
    /// the handler began may be going.
    Handled,
    /// The reply carried no disposition (an older Pi).
    Unknown,
}

impl Disposition {
    /// Read `data` of a `prompt` reply.
    pub(crate) fn of(data: &Value) -> Self {
        match string(data, "disposition") {
            "started" => Self::Started,
            "queued" => Self::Queued,
            "handled" => Self::Handled,
            _ => Self::Unknown,
        }
    }
}

/// The parts of `get_state` the driver reads.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct State {
    /// The session id, which `--session-id` takes back.
    pub(crate) session_id: String,
    /// Where Pi keeps (or will keep: it writes the file with the first assistant message) the
    /// transcript.
    pub(crate) session_file: Option<String>,
    /// The model in use as `provider/id`, when one is selected.
    pub(crate) model: Option<String>,
    /// The model's context window in tokens.
    pub(crate) context_window: Option<u64>,
    /// A run is in progress (retries and continuations included).
    pub(crate) streaming: bool,
    /// A compaction is in progress.
    pub(crate) compacting: bool,
    /// How many messages the session holds.
    pub(crate) message_count: u64,
}

impl State {
    /// Read `data` of a `get_state` reply. `None` when it names no session.
    pub(crate) fn parse(data: &Value) -> Option<Self> {
        let session_id = string(data, "sessionId");
        if session_id.is_empty() {
            return None;
        }
        let model = data.get("model").filter(|m| m.is_object()).map(|m| {
            let id = string(m, "id");
            match string(m, "provider") {
                "" => id.to_owned(),
                provider => format!("{provider}/{id}"),
            }
        });
        Some(Self {
            session_id: session_id.to_owned(),
            session_file: data
                .get("sessionFile")
                .and_then(Value::as_str)
                .filter(|f| !f.is_empty())
                .map(str::to_owned),
            model,
            context_window: data
                .get("model")
                .and_then(|m| m.get("contextWindow"))
                .and_then(Value::as_u64)
                .filter(|w| *w > 0),
            // A missing flag is treated as busy: the barrier must not close a run on a guess.
            streaming: data.get("isStreaming").and_then(Value::as_bool) != Some(false),
            compacting: data.get("isCompacting").and_then(Value::as_bool) != Some(false),
            message_count: data
                .get("messageCount")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        })
    }

    /// Whether Pi is doing anything: a run, or a compaction.
    pub(crate) fn busy(&self) -> bool {
        self.streaming || self.compacting
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn get_commands_is_the_bare_command() {
        assert_eq!(get_commands(), json!({ "type": "get_commands" }));
    }

    #[test]
    fn only_an_extension_command_of_that_name_is_listed() {
        let data = json!({ "commands": [
            { "name": "skill:reno-gate", "source": "skill" },
            { "name": "reno-gate", "source": "prompt" },
            { "name": "reno-gate-2", "source": "extension" },
            { "name": "reno-gate", "description": "Reno's gate on tool calls", "source": "extension", "sourceInfo": {} },
        ] });
        assert!(lists_extension_command(&data, "reno-gate"));
        assert!(!lists_extension_command(&data, "other"));
        for lookalike in [
            json!({ "commands": [{ "name": "skill:reno-gate", "source": "skill" }] }),
            json!({ "commands": [{ "name": "reno-gate", "source": "prompt" }] }),
            json!({ "commands": [{ "name": "reno-gate" }] }),
            json!({ "commands": [{ "source": "extension" }] }),
            json!({ "commands": [] }),
            json!({ "commands": "reno-gate" }),
            json!({}),
            Value::Null,
        ] {
            assert!(
                !lists_extension_command(&lookalike, "reno-gate"),
                "{lookalike}"
            );
        }
    }

    #[test]
    fn the_settings_writing_commands_are_named_and_the_session_scoped_ones_are_not() {
        for kind in [
            "set_steering_mode",
            "set_follow_up_mode",
            "set_auto_compaction",
            "set_auto_retry",
            "cycle_model",
            "cycle_thinking_level",
        ] {
            assert!(writes_pi_settings(kind), "{kind}");
        }
        for kind in [
            "prompt",
            "get_state",
            "get_available_models",
            "set_model",
            "set_thinking_level",
            "abort",
            "clear_queue",
            "extension_ui_response",
        ] {
            assert!(!writes_pi_settings(kind), "{kind}");
        }
    }

    #[test]
    fn none_of_the_builders_make_a_settings_write() {
        for frame in [
            get_state(),
            get_available_models(),
            set_model("anthropic", "claude-opus-4-8"),
            set_thinking_level("high"),
            prompt("hi"),
            steer("hi"),
            clear_queue(),
            abort(),
        ] {
            assert!(!writes_pi_settings(string(&frame, "type")), "{frame}");
        }
    }

    #[test]
    fn a_steer_is_a_prompt_that_says_how_to_queue() {
        let frame = steer("go left");
        assert_eq!(frame["type"], "prompt");
        assert_eq!(frame["streamingBehavior"], "steer");
        assert!(prompt("go left").get("streamingBehavior").is_none());
    }

    #[test]
    fn replies_carry_their_data_or_pis_words() {
        let ok = Reply::parse(&json!({
            "type": "response", "id": "r-1", "command": "prompt", "success": true,
            "data": { "disposition": "queued" }
        }))
        .unwrap();
        assert_eq!(ok.id, "r-1");
        assert_eq!(
            Disposition::of(ok.outcome.as_ref().unwrap()),
            Disposition::Queued
        );
        let failed = Reply::parse(&json!({
            "type": "response", "id": "r-2", "command": "set_model", "success": false,
            "error": "Model not found: a/b"
        }))
        .unwrap();
        assert_eq!(
            failed.outcome,
            Err("Pi set_model: Model not found: a/b".into())
        );
        let parse = Reply::parse(&json!({
            "type": "response", "command": "parse", "success": false, "error": "bad"
        }))
        .unwrap();
        assert_eq!(parse.id, "");
        assert!(Reply::parse(&json!({ "type": "agent_start" })).is_none());
        assert_eq!(Disposition::of(&json!({})), Disposition::Unknown);
        assert_eq!(
            Disposition::of(&json!({ "disposition": "handled" })),
            Disposition::Handled
        );
        assert_eq!(
            Disposition::of(&json!({ "disposition": "started" })),
            Disposition::Started
        );
    }

    /// The `get_state` reply Pi 1.5.0 gave in the live handshake (model trimmed).
    #[test]
    fn get_state_of_pi_1_5_0_reads() {
        let data = json!({
            "model": { "id": "claude-opus-4-8", "provider": "anthropic", "contextWindow": 1_000_000 },
            "thinkingLevel": "medium", "isStreaming": false, "isCompacting": false,
            "steeringMode": "one-at-a-time", "followUpMode": "one-at-a-time",
            "sessionId": "01a1ee3c-f4ce-72b1-b5fe-d4ec98e14122", "autoCompactionEnabled": true,
            "messageCount": 0, "pendingMessageCount": 0
        });
        let state = State::parse(&data).unwrap();
        assert_eq!(state.session_id, "01a1ee3c-f4ce-72b1-b5fe-d4ec98e14122");
        assert_eq!(state.model.as_deref(), Some("anthropic/claude-opus-4-8"));
        assert_eq!(state.context_window, Some(1_000_000));
        assert_eq!(state.session_file, None);
        assert!(!state.busy());
    }

    #[test]
    fn a_state_without_its_flags_is_busy_and_one_without_a_session_is_nothing() {
        let state = State::parse(&json!({ "sessionId": "s" })).unwrap();
        assert!(state.busy());
        assert!(State::parse(&json!({ "isStreaming": false })).is_none());
        assert!(State::parse(&json!({ "sessionId": "" })).is_none());
    }
}
