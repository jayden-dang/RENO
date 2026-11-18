//! Pi session events to [`ExecEvent`]s.
//!
//! Sans-IO (ARCH-6): frames in, events out. The driver owns the process, the run's lifecycle
//! (when it is over, see `drivers::pi`) and the dialogs; this module owns what a message, a delta
//! or a tool event means.
//!
//! # Event to `ExecEvent`
//!
//! | Pi | `ExecEvent` |
//! |---|---|
//! | `message_update` / `text_delta` | `Text` |
//! | `message_update` / `thinking_delta` | `Reasoning` |
//! | `message_end` (assistant): the part of the final text no delta carried | `Text` / `Reasoning` |
//! | `message_end` (assistant), `usage` | `Usage` (Pi's four counts are already disjoint), `ContextUsage` |
//! | `tool_execution_start` | `Tool` |
//! | `tool_execution_end` | `ToolResult` |
//! | `compaction_end`, `estimatedTokensAfter` | `ContextUsage` |
//!
//! # Tool names
//!
//! | Pi tool | [`ActionClass`] |
//! |---|---|
//! | `bash`, `powershell` | `Command` |
//! | `read`, `grep`, `find`, `ls` | `Read` |
//! | `edit`, `write` | `Edit` |
//! | `mcp__<server>__<tool>` (Pi's own MCP, and Reno's bridge, which names its tools the same way) | `McpTool` |
//! | anything else | `Other` |
//!
//! # How a run ends
//!
//! The terminal status is the last assistant message's: `stopReason: "error"` is an error (a
//! retry that succeeds clears it; a final `auto_retry_end` failure sets it), `"aborted"` and an
//! `agent_settled` with `aborted: true` are an interruption, `"length"` is `MaxTokens`. A throwing
//! extension handler (`extension_error`) is the extension's failure, not the run's.
//!
//! Frames the table in [`super::known`] does not list are counted by kind in
//! [`Normalizer::take_unknown`].

use std::collections::HashMap;

use reno_proto::{ActionClass, ExecEvent, StopReason, ToolCall, ToolOutput};
use serde_json::Value;

use super::frames::string;
use super::known;

/// The class of a Pi tool call (see the module docs).
pub(crate) fn action_class(tool: &str) -> ActionClass {
    match tool {
        "bash" | "powershell" => ActionClass::Command,
        "read" | "grep" | "find" | "ls" => ActionClass::Read,
        "edit" | "write" => ActionClass::Edit,
        t if t.starts_with("mcp__") => ActionClass::McpTool,
        _ => ActionClass::Other,
    }
}

/// How a run ended, from what its events said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ending {
    /// The stop reason.
    pub(crate) stop: StopReason,
    /// The text of the last assistant message, for a run that completed.
    pub(crate) result: Option<String>,
    /// What went wrong, for a run that did not complete.
    pub(crate) diagnostic: Option<String>,
}

/// The normalizer for one run.
#[derive(Debug, Default)]
pub(crate) struct Normalizer {
    text: HashMap<usize, String>,
    thinking: HashMap<usize, String>,
    /// Open tool calls: id to tool name.
    tools: HashMap<String, String>,
    error: Option<String>,
    aborted: bool,
    length: bool,
    final_text: String,
    window: Option<u64>,
    unknown: Vec<String>,
}

impl Normalizer {
    /// A normalizer with no history.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The model's context window, once known (`get_state`).
    pub(crate) fn set_window(&mut self, window: Option<u64>) {
        self.window = window.or(self.window);
    }

    /// The frame kinds met that the driver does not know, since the last call.
    pub(crate) fn take_unknown(&mut self) -> Vec<String> {
        std::mem::take(&mut self.unknown)
    }

    /// Forget what a previous prompt left (a new run on the same process).
    pub(crate) fn reset(&mut self) {
        self.text.clear();
        self.thinking.clear();
        self.error = None;
        self.aborted = false;
        self.length = false;
        self.final_text.clear();
    }

    /// How the run ended so far.
    pub(crate) fn ending(&self, interrupted: bool) -> Ending {
        if interrupted || self.aborted {
            return Ending {
                stop: StopReason::Interrupted,
                result: None,
                diagnostic: None,
            };
        }
        if let Some(error) = &self.error {
            return Ending {
                stop: StopReason::Error,
                result: None,
                diagnostic: Some(error.clone()),
            };
        }
        Ending {
            stop: if self.length {
                StopReason::MaxTokens
            } else {
                StopReason::Completed
            },
            result: (!self.final_text.is_empty()).then(|| self.final_text.clone()),
            diagnostic: None,
        }
    }

    /// The events `frame` means.
    pub(crate) fn map(&mut self, frame: &Value) -> Vec<ExecEvent> {
        let kind = string(frame, "type");
        if !known::is_known_frame(kind) {
            self.unknown.push(format!("type:{kind}"));
            return Vec::new();
        }
        match kind {
            "message_start" if frame["message"]["role"] == "assistant" => {
                self.text.clear();
                self.thinking.clear();
                Vec::new()
            }
            "message_update" => self.update(&frame["assistantMessageEvent"]),
            "message_end" if frame["message"]["role"] == "assistant" => {
                self.assistant_end(&frame["message"])
            }
            "tool_execution_start" => self.tool_start(frame),
            "tool_execution_end" => self.tool_end(frame),
            "compaction_end" => frame["result"]["estimatedTokensAfter"]
                .as_u64()
                .map(|tokens| {
                    vec![ExecEvent::ContextUsage {
                        tokens: Some(tokens),
                        window: self.window,
                    }]
                })
                .unwrap_or_default(),
            "auto_retry_end" => {
                if frame["success"] == false && self.error.is_none() {
                    self.error = Some(
                        frame["finalError"]
                            .as_str()
                            .unwrap_or("Pi gave up retrying")
                            .to_owned(),
                    );
                }
                Vec::new()
            }
            "agent_settled" => {
                if frame["aborted"] == true {
                    self.aborted = true;
                }
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn update(&mut self, update: &Value) -> Vec<ExecEvent> {
        let kind = string(update, "type");
        if !known::KNOWN_MESSAGE_EVENTS.contains(&kind) {
            self.unknown.push(format!("message_update:{kind}"));
            return Vec::new();
        }
        let index = update["contentIndex"].as_u64().unwrap_or(0) as usize;
        let delta = string(update, "delta");
        match kind {
            "text_delta" => {
                self.text.entry(index).or_default().push_str(delta);
                vec![ExecEvent::Text { text: delta.into() }]
            }
            "thinking_delta" => {
                self.thinking.entry(index).or_default().push_str(delta);
                vec![ExecEvent::Reasoning { text: delta.into() }]
            }
            _ => Vec::new(),
        }
    }

    fn assistant_end(&mut self, message: &Value) -> Vec<ExecEvent> {
        let mut events = Vec::new();
        let stop = string(message, "stopReason");
        self.error = (stop == "error").then(|| {
            message["errorMessage"]
                .as_str()
                .filter(|e| !e.is_empty())
                .unwrap_or("Pi provider failed")
                .to_owned()
        });
        self.aborted = stop == "aborted";
        self.length = stop == "length";
        self.final_text.clear();
        for (index, part) in message["content"]
            .as_array()
            .into_iter()
            .flatten()
            .enumerate()
        {
            let (cache, value, reasoning) = match string(part, "type") {
                "text" => (&mut self.text, string(part, "text"), false),
                "thinking" => (&mut self.thinking, string(part, "thinking"), true),
                _ => continue,
            };
            if !reasoning {
                self.final_text.push_str(value);
            }
            // The final message is authoritative: what the deltas did not carry is the tail.
            let streamed = cache.entry(index).or_default();
            if let Some(rest) = value
                .strip_prefix(streamed.as_str())
                .filter(|rest| !rest.is_empty())
            {
                events.push(if reasoning {
                    ExecEvent::Reasoning { text: rest.into() }
                } else {
                    ExecEvent::Text { text: rest.into() }
                });
            }
            *streamed = value.into();
        }
        let usage = &message["usage"];
        if usage.is_object() {
            let n = |key: &str| usage[key].as_u64().unwrap_or(0);
            let (input, output, read, write) =
                (n("input"), n("output"), n("cacheRead"), n("cacheWrite"));
            if input + output + read + write > 0 {
                events.push(ExecEvent::Usage {
                    input_tokens: input,
                    output_tokens: output,
                    cache_read_tokens: read,
                    cache_write_tokens: write,
                });
                let total = match n("totalTokens") {
                    0 => input + output + read + write,
                    total => total,
                };
                events.push(ExecEvent::ContextUsage {
                    tokens: Some(total),
                    window: self.window,
                });
            }
        }
        events
    }

    fn tool_start(&mut self, frame: &Value) -> Vec<ExecEvent> {
        let id = string(frame, "toolCallId").to_owned();
        let name = string(frame, "toolName");
        if self.tools.contains_key(&id) {
            return Vec::new();
        }
        self.tools.insert(id.clone(), name.to_owned());
        vec![ExecEvent::Tool {
            id,
            call: ToolCall::new(name, frame["args"].clone(), action_class(name)),
        }]
    }

    fn tool_end(&mut self, frame: &Value) -> Vec<ExecEvent> {
        let id = string(frame, "toolCallId").to_owned();
        if self.tools.remove(&id).is_none() {
            self.unknown.push("tool_execution_end:orphan".into());
            return Vec::new();
        }
        let ok = frame["isError"].as_bool() != Some(true);
        let text = frame["result"]["content"]
            .as_array()
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| item["text"].as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default();
        let summary = (!ok)
            .then(|| text.lines().find(|l| !l.trim().is_empty()))
            .flatten()
            .map(|line| {
                let line = line.trim();
                let mut short: String = line.chars().take(200).collect();
                if short.len() < line.len() {
                    short.push('…');
                }
                short
            });
        vec![ExecEvent::ToolResult {
            id,
            ok,
            summary,
            output: ToolOutput::from_raw_opt(&text),
        }]
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use serde_json::json;

    use super::*;

    fn texts(events: &[ExecEvent]) -> Vec<&str> {
        events
            .iter()
            .filter_map(|e| match e {
                ExecEvent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn the_final_message_adds_only_what_the_deltas_missed_and_a_retry_clears_the_error() {
        let mut n = Normalizer::new();
        n.map(&json!({"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"hel"}}));
        let events = n.map(&json!({"type":"message_end","message":{
            "role":"assistant","stopReason":"error","errorMessage":"temporary",
            "content":[{"type":"text","text":"hello"}]}}));
        assert_eq!(texts(&events), ["lo"]);
        assert_eq!(n.ending(false).stop, StopReason::Error);
        assert_eq!(n.ending(false).diagnostic.as_deref(), Some("temporary"));
        n.map(&json!({"type":"message_end","message":{"role":"assistant","stopReason":"stop","content":[]}}));
        assert_eq!(n.ending(false).stop, StopReason::Completed);
    }

    #[test]
    fn a_completed_run_carries_the_last_message_text_as_its_result() {
        let mut n = Normalizer::new();
        n.map(&json!({"type":"message_end","message":{"role":"assistant","stopReason":"toolUse",
            "content":[{"type":"text","text":"checking"},{"type":"toolCall","id":"c"}]}}));
        n.map(&json!({"type":"message_end","message":{"role":"assistant","stopReason":"stop",
            "content":[{"type":"thinking","thinking":"hm"},{"type":"text","text":"all done"}]}}));
        let ending = n.ending(false);
        assert_eq!(ending.stop, StopReason::Completed);
        assert_eq!(ending.result.as_deref(), Some("all done"));
    }

    #[test]
    fn usage_is_pis_four_disjoint_counts_and_context_is_the_total() {
        let mut n = Normalizer::new();
        n.set_window(Some(200_000));
        let events = n.map(&json!({"type":"message_end","message":{"role":"assistant","stopReason":"stop",
            "content":[],"usage":{"input":12,"output":3,"cacheRead":100,"cacheWrite":5,"totalTokens":120}}}));
        assert!(events.contains(&ExecEvent::Usage {
            input_tokens: 12,
            output_tokens: 3,
            cache_read_tokens: 100,
            cache_write_tokens: 5
        }));
        assert!(events.contains(&ExecEvent::ContextUsage {
            tokens: Some(120),
            window: Some(200_000)
        }));
        // A message that reports nothing adds nothing.
        let none = n.map(&json!({"type":"message_end","message":{"role":"assistant","stopReason":"stop",
            "content":[],"usage":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"totalTokens":0}}}));
        assert!(none.is_empty());
    }

    #[test]
    fn reasoning_streams_and_a_user_message_maps_to_nothing() {
        let mut n = Normalizer::new();
        let events = n.map(&json!({"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","contentIndex":0,"delta":"hmm"}}));
        assert_eq!(events, [ExecEvent::Reasoning { text: "hmm".into() }]);
        assert!(n
            .map(&json!({"type":"message_end","message":{"role":"user","content":"hi"}}))
            .is_empty());
    }

    #[test]
    fn a_tool_call_opens_once_and_closes_with_its_output() {
        let mut n = Normalizer::new();
        let start = json!({"type":"tool_execution_start","toolCallId":"t","toolName":"edit",
            "args":{"path":"a","edits":[{"oldText":"old","newText":"new"}]}});
        let opened = n.map(&start);
        assert!(matches!(
            &opened[..],
            [ExecEvent::Tool { id, call }] if id == "t" && call.class == ActionClass::Edit
                && call.input["edits"][0]["newText"] == "new"
        ));
        assert!(n.map(&start).is_empty(), "a repeated start is not a second call");
        let closed = n.map(&json!({"type":"tool_execution_end","toolCallId":"t","isError":false,
            "result":{"content":[{"type":"text","text":"ok"}]}}));
        assert!(matches!(
            &closed[..],
            [ExecEvent::ToolResult { id, ok: true, summary: None, output: Some(o) }]
                if id == "t" && o.text == "ok"
        ));
    }

    #[test]
    fn a_failed_tool_summarises_its_first_line() {
        let mut n = Normalizer::new();
        n.map(&json!({"type":"tool_execution_start","toolCallId":"b","toolName":"bash","args":{"command":"x"}}));
        let closed = n.map(&json!({"type":"tool_execution_end","toolCallId":"b","isError":true,
            "result":{"content":[{"type":"text","text":"\nboom: no such file\nmore"}]}}));
        assert!(matches!(
            &closed[..],
            [ExecEvent::ToolResult { ok: false, summary: Some(s), .. }] if s == "boom: no such file"
        ));
    }

    #[test]
    fn tool_classes() {
        assert_eq!(action_class("bash"), ActionClass::Command);
        assert_eq!(action_class("read"), ActionClass::Read);
        assert_eq!(action_class("grep"), ActionClass::Read);
        assert_eq!(action_class("write"), ActionClass::Edit);
        assert_eq!(action_class("edit"), ActionClass::Edit);
        assert_eq!(action_class("mcp__reno__report"), ActionClass::McpTool);
        assert_eq!(action_class("codemode"), ActionClass::Other);
    }

    #[test]
    fn compaction_reports_the_context_it_left() {
        let mut n = Normalizer::new();
        n.set_window(Some(1000));
        let events = n.map(&json!({"type":"compaction_end","reason":"threshold","aborted":false,
            "result":{"estimatedTokensAfter":300}}));
        assert_eq!(
            events,
            [ExecEvent::ContextUsage { tokens: Some(300), window: Some(1000) }]
        );
        assert!(n
            .map(&json!({"type":"compaction_end","reason":"manual","aborted":true}))
            .is_empty());
    }

    #[test]
    fn aborted_length_and_a_failed_retry_end_as_they_should() {
        let mut n = Normalizer::new();
        n.map(&json!({"type":"message_end","message":{"role":"assistant","stopReason":"length","content":[]}}));
        assert_eq!(n.ending(false).stop, StopReason::MaxTokens);
        n.map(&json!({"type":"agent_settled","aborted":true}));
        assert_eq!(n.ending(false).stop, StopReason::Interrupted);
        n.reset();
        assert_eq!(n.ending(true).stop, StopReason::Interrupted);
        n.map(&json!({"type":"auto_retry_end","success":false,"attempt":3,"finalError":"529 overloaded"}));
        assert_eq!(n.ending(false).diagnostic.as_deref(), Some("529 overloaded"));
    }

    #[test]
    fn a_frame_the_table_lacks_is_counted_and_chatter_is_not() {
        let mut n = Normalizer::new();
        for kind in ["queue_update", "turn_start", "agent_start", "extension_error", "compaction_start"] {
            assert!(n.map(&json!({ "type": kind })).is_empty());
        }
        n.map(&json!({"type":"message_update","assistantMessageEvent":{"type":"toolcall_delta","delta":"{"}}));
        assert!(n.take_unknown().is_empty());
        n.map(&json!({"type":"agent_teleported"}));
        n.map(&json!({"type":"message_update","assistantMessageEvent":{"type":"audio_delta"}}));
        n.map(&json!({"type":"tool_execution_end","toolCallId":"nobody"}));
        assert_eq!(
            n.take_unknown(),
            ["type:agent_teleported", "message_update:audio_delta", "tool_execution_end:orphan"]
        );
        assert!(n.take_unknown().is_empty());
    }
}
