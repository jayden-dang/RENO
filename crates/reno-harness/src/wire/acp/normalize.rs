//! ACP `session/update` to [`ExecEvent`] (protocol v1).
//!
//! Sans-IO (ARCH-6): updates in, events out. Tolerant by construction: decoded from raw
//! [`Value`]s, unknown `sessionUpdate` kinds and content types map to nothing, and missing fields
//! degrade to empty strings rather than errors. Wire shapes follow `agent-client-protocol-schema`
//! 1.3.0 (`SessionUpdate` is tagged `sessionUpdate`/snake_case, structs are camelCase, tool kinds
//! and statuses are snake_case).
//!
//! # What becomes an event
//!
//! | `sessionUpdate` | Event |
//! |---|---|
//! | `agent_message_chunk` (text) | `Text` |
//! | `agent_thought_chunk` (text) | `Reasoning` |
//! | `tool_call`, `tool_call_update` | `Tool` once per call id, classified by the call's `kind` (see `permission`); `ToolResult` when `status` is `completed` or `failed` |
//! | the prompt response's `_meta` or `usage` token counts | `Usage` |
//!
//! Everything else (plans, context gauges, command lists, mode changes, a replayed user message,
//! images) is dropped: the contract has no place for it. A call is reported by its first update
//! only: a later update that carries a kind never reclassifies it, and a kindless one never turns
//! a classified call into `Other`.

use std::collections::HashSet;

use reno_proto::{ExecEvent, StopReason, ToolCall};
use serde_json::Value;

use super::permission;

/// The longest tool-result summary, in characters.
const SUMMARY_CHARS: usize = 200;

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

/// The text of a `ContentBlock` (`{type: "text", text}`); other blocks (image, audio, resource)
/// have none.
fn block_text(block: &Value) -> Option<&str> {
    (block.get("type").and_then(Value::as_str) == Some("text"))
        .then(|| block.get("text").and_then(Value::as_str))
        .flatten()
}

/// A `ContentChunk`'s streamed text (`{content: {type: "text", ...}}`).
fn chunk_text(update: &Value) -> Option<String> {
    let text = block_text(update.get("content")?)?;
    (!text.is_empty()).then(|| text.to_owned())
}

/// The last non-empty line of a tool call's text `content`, cut to [`SUMMARY_CHARS`].
fn output_summary(update: &Value) -> Option<String> {
    let line = update
        .get("content")?
        .as_array()?
        .iter()
        .filter(|entry| entry.get("type").and_then(Value::as_str) == Some("content"))
        .filter_map(|entry| block_text(entry.get("content")?))
        .flat_map(str::lines)
        .map(str::trim)
        .rfind(|line| !line.is_empty())?;
    Some(line.chars().take(SUMMARY_CHARS).collect())
}

/// Turns one session's `session/update` payloads into events.
#[derive(Debug, Default)]
pub(crate) struct Normalizer {
    /// Ids of the calls whose `Tool` event went out.
    announced: HashSet<String>,
}

impl Normalizer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The events one `update` object (`params.update`) stands for.
    pub(crate) fn update(&mut self, update: &Value) -> Vec<ExecEvent> {
        match str_field(update, "sessionUpdate") {
            "agent_message_chunk" => chunk_text(update)
                .map(|text| ExecEvent::Text { text })
                .into_iter()
                .collect(),
            "agent_thought_chunk" => chunk_text(update)
                .map(|text| ExecEvent::Reasoning { text })
                .into_iter()
                .collect(),
            "tool_call" | "tool_call_update" => self.tool(update),
            _ => Vec::new(),
        }
    }

    fn tool(&mut self, update: &Value) -> Vec<ExecEvent> {
        let id = str_field(update, "toolCallId").to_owned();
        let mut events = Vec::new();
        if self.announced.insert(id.clone()) {
            events.push(ExecEvent::Tool {
                id: id.clone(),
                call: ToolCall::new(
                    permission::tool_name(update),
                    update.get("rawInput").cloned().unwrap_or(Value::Null),
                    permission::action_class(permission::kind_of(update)),
                ),
            });
        }
        // A terminal status resolves the call, in the same update that opens it (some agents send
        // one terminal `tool_call`) or in a later one.
        let ok = match str_field(update, "status") {
            "completed" => Some(true),
            "failed" => Some(false),
            _ => None,
        };
        if let Some(ok) = ok {
            events.push(ExecEvent::ToolResult {
                id,
                ok,
                summary: output_summary(update),
            });
        }
        events
    }
}

/// How a `session/prompt` ended: the stop reason and, for a failure, why. `response` is the
/// prompt's result, or `Err` with the transport's message when it failed. An `interrupted` run
/// ends `Interrupted` whatever the agent said.
pub(crate) fn stop(
    response: Result<&Value, &str>,
    interrupted: bool,
) -> (StopReason, Option<String>) {
    if interrupted {
        return (StopReason::Interrupted, None);
    }
    match response {
        Ok(response) => match str_field(response, "stopReason") {
            "cancelled" => (StopReason::Interrupted, None),
            "error" => (
                StopReason::Error,
                Some("The agent failed to complete the turn.".to_owned()),
            ),
            "refusal" => (
                StopReason::Refusal,
                Some("The agent refused to continue.".to_owned()),
            ),
            "max_tokens" => (StopReason::MaxTokens, None),
            // `end_turn`, `max_turn_requests` and anything newer: the turn ended and its output
            // is in the stream.
            _ => (StopReason::Completed, None),
        },
        Err(message) => (StopReason::Error, Some(message.to_owned())),
    }
}

/// Per-turn token usage from a settled `session/prompt` response, when the agent attaches it:
/// a first-class `usage` object, or the `_meta` Grok settles it on (`inputTokens`/`outputTokens`).
pub(crate) fn usage(response: &Value) -> Option<ExecEvent> {
    let usage = response.get("usage").or_else(|| response.get("_meta"))?;
    let count = |keys: &[&str]| {
        keys.iter()
            .find_map(|key| usage.get(*key))
            .and_then(Value::as_u64)
    };
    let input = count(&["inputTokens", "input_tokens"]);
    let output = count(&["outputTokens", "output_tokens"]);
    (input.is_some() || output.is_some()).then(|| ExecEvent::Usage {
        input_tokens: input.unwrap_or(0),
        output_tokens: output.unwrap_or(0),
    })
}

/// The `update` object of a `session/update` notification for `session_id`; `None` for any other
/// method or another session's traffic.
pub(crate) fn session_update<'a>(
    method: &str,
    params: &'a Value,
    session_id: &str,
) -> Option<&'a Value> {
    if method != "session/update" {
        return None;
    }
    if params
        .get("sessionId")
        .and_then(Value::as_str)
        .is_some_and(|id| id != session_id)
    {
        return None;
    }
    params.get("update")
}

/// The `sessionUpdate` kind of an update.
pub(crate) fn update_kind(update: &Value) -> &str {
    str_field(update, "sessionUpdate")
}

/// Whether an update is turn progress: text, a thought or a tool call. Session boilerplate (the
/// command list, config and mode changes) is not.
pub(crate) fn is_progress(update: &Value) -> bool {
    matches!(
        update_kind(update),
        "agent_message_chunk" | "agent_thought_chunk" | "tool_call" | "tool_call_update" | "plan"
    )
}

/// Whether an update is session boilerplate agents send on every session, healthy or not.
pub(crate) fn is_boilerplate(update: &Value) -> bool {
    matches!(
        update_kind(update),
        "available_commands_update" | "config_option_update" | "current_mode_update"
    )
}

#[cfg(test)]
mod tests {
    use reno_proto::ActionClass;
    use serde_json::json;

    use super::*;

    fn events(updates: &[Value]) -> Vec<ExecEvent> {
        let mut normalizer = Normalizer::new();
        updates
            .iter()
            .flat_map(|update| normalizer.update(update))
            .collect()
    }

    #[test]
    fn message_and_thought_chunks_map_to_text_and_reasoning() {
        assert_eq!(
            events(&[
                json!({ "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": "hello" } }),
                json!({ "sessionUpdate": "agent_thought_chunk",
                        "content": { "type": "text", "text": "hmm" } }),
                // Non-text blocks, empty chunks and a replayed user message are nothing.
                json!({ "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "image", "data": "x", "mimeType": "image/png" } }),
                json!({ "sessionUpdate": "agent_message_chunk",
                        "content": { "type": "text", "text": "" } }),
                json!({ "sessionUpdate": "user_message_chunk",
                        "content": { "type": "text", "text": "old" } }),
            ]),
            [
                ExecEvent::Text {
                    text: "hello".into()
                },
                ExecEvent::Reasoning { text: "hmm".into() },
            ]
        );
    }

    #[test]
    fn a_call_is_announced_once_with_its_kind_input_and_class() {
        let evs = events(&[
            json!({ "sessionUpdate": "tool_call", "toolCallId": "t1", "kind": "execute",
                    "title": "cargo test", "status": "pending",
                    "rawInput": { "command": "cargo test" } }),
            json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1",
                    "kind": "edit", "title": "now something else" }),
            json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t1",
                    "status": "completed",
                    "content": [{ "type": "content", "content": {
                        "type": "text", "text": "compiling\nrunning 3 tests\n\ntest result: ok\n" } }] }),
        ]);
        assert_eq!(
            evs,
            [
                ExecEvent::Tool {
                    id: "t1".into(),
                    call: ToolCall::new(
                        "execute",
                        json!({ "command": "cargo test" }),
                        ActionClass::Command
                    ),
                },
                ExecEvent::ToolResult {
                    id: "t1".into(),
                    ok: true,
                    summary: Some("test result: ok".into()),
                },
            ]
        );
    }

    #[test]
    fn a_kindless_update_never_reclassifies_an_announced_call() {
        let evs = events(&[
            json!({ "sessionUpdate": "tool_call", "toolCallId": "t", "kind": "edit",
                    "locations": [{ "path": "/w/a" }] }),
            json!({ "sessionUpdate": "tool_call_update", "toolCallId": "t", "status": "failed" }),
        ]);
        assert_eq!(evs.len(), 2);
        assert!(matches!(&evs[0], ExecEvent::Tool { call, .. } if call.class == ActionClass::Edit));
        assert_eq!(
            evs[1],
            ExecEvent::ToolResult {
                id: "t".into(),
                ok: false,
                summary: None
            }
        );
    }

    #[test]
    fn a_terminal_call_resolves_in_the_update_that_opens_it() {
        let evs = events(&[json!({
            "sessionUpdate": "tool_call", "toolCallId": "r1", "title": "read file 1",
            "kind": "read", "status": "completed",
            "content": [{ "type": "content", "content": { "type": "text", "text": "contents" } }],
        })]);
        assert_eq!(evs.len(), 2);
        assert!(matches!(&evs[0], ExecEvent::Tool { call, .. }
            if call.name == "read" && call.class == ActionClass::Read));
        assert_eq!(
            evs[1],
            ExecEvent::ToolResult {
                id: "r1".into(),
                ok: true,
                summary: Some("contents".into())
            }
        );
    }

    #[test]
    fn an_update_for_an_unseen_call_announces_it_first() {
        let evs = events(&[json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "late", "kind": "fetch",
            "status": "completed",
        })]);
        assert!(matches!(&evs[0], ExecEvent::Tool { id, call }
            if id == "late" && call.class == ActionClass::Network));
        assert!(matches!(&evs[1], ExecEvent::ToolResult { ok: true, .. }));
    }

    #[test]
    fn a_call_without_a_kind_is_other_and_named_by_its_title() {
        let evs = events(&[json!({
            "sessionUpdate": "tool_call", "toolCallId": "s", "title": "spawn_subagent",
            "rawInput": { "description": "Count files" },
        })]);
        assert_eq!(
            evs,
            [ExecEvent::Tool {
                id: "s".into(),
                call: ToolCall::new(
                    "spawn_subagent",
                    json!({ "description": "Count files" }),
                    ActionClass::Other
                ),
            }]
        );
    }

    #[test]
    fn plans_commands_usage_gauges_and_unknown_kinds_are_dropped() {
        assert!(
            events(&[
                json!({ "sessionUpdate": "plan", "entries": [] }),
                json!({ "sessionUpdate": "available_commands_update", "availableCommands": [] }),
                json!({ "sessionUpdate": "usage_update", "used": 1, "size": 2 }),
                json!({ "sessionUpdate": "current_mode_update", "currentModeId": "x" }),
                json!({ "sessionUpdate": "totally_unknown_kind" }),
                json!({}),
            ])
            .is_empty()
        );
    }

    #[test]
    fn stop_reasons_map_and_an_interrupt_wins() {
        let reason = |reason: &str| stop(Ok(&json!({ "stopReason": reason })), false);
        assert_eq!(reason("end_turn"), (StopReason::Completed, None));
        assert_eq!(reason("max_turn_requests"), (StopReason::Completed, None));
        assert_eq!(reason("max_tokens"), (StopReason::MaxTokens, None));
        assert_eq!(reason("cancelled"), (StopReason::Interrupted, None));
        assert!(matches!(reason("refusal"), (StopReason::Refusal, Some(_))));
        assert!(matches!(reason("error"), (StopReason::Error, Some(_))));
        assert_eq!(
            stop(Err("agent exited"), false),
            (StopReason::Error, Some("agent exited".into()))
        );
        assert_eq!(
            stop(Ok(&json!({ "stopReason": "end_turn" })), true),
            (StopReason::Interrupted, None)
        );
    }

    #[test]
    fn usage_reads_either_spelling_and_nothing_else() {
        assert_eq!(
            usage(&json!({ "_meta": { "inputTokens": 9, "outputTokens": 4 } })),
            Some(ExecEvent::Usage {
                input_tokens: 9,
                output_tokens: 4
            })
        );
        assert_eq!(
            usage(&json!({ "usage": { "input_tokens": 3 } })),
            Some(ExecEvent::Usage {
                input_tokens: 3,
                output_tokens: 0
            })
        );
        assert_eq!(usage(&json!({ "stopReason": "end_turn" })), None);
    }

    #[test]
    fn only_this_sessions_updates_are_taken() {
        let params = json!({ "sessionId": "s1", "update": { "sessionUpdate": "plan" } });
        assert!(session_update("session/update", &params, "s1").is_some());
        assert!(session_update("session/update", &params, "s2").is_none());
        assert!(session_update("other/method", &params, "s1").is_none());
        let bare = json!({ "update": { "sessionUpdate": "plan" } });
        assert!(session_update("session/update", &bare, "s1").is_some());
    }

    #[test]
    fn progress_and_boilerplate_are_told_apart() {
        for kind in [
            "agent_message_chunk",
            "agent_thought_chunk",
            "tool_call",
            "tool_call_update",
            "plan",
        ] {
            assert!(is_progress(&json!({ "sessionUpdate": kind })), "{kind}");
        }
        for kind in [
            "available_commands_update",
            "config_option_update",
            "current_mode_update",
        ] {
            let update = json!({ "sessionUpdate": kind });
            assert!(is_boilerplate(&update) && !is_progress(&update), "{kind}");
        }
    }
}
