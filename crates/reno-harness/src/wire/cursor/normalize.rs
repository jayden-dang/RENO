//! The shim's stdout frames to [`ExecEvent`]s.
//!
//! Sans-IO (ARCH-6): frames in, events and one signal out. The driver owns the process and when
//! the run is over; this module owns what a frame means.
//!
//! ```text
//! {"ev":"ready","agentId","model"?}                 -> Started
//! {"ev":"steered"}                                  -> Signal::Steered (one steering input consumed)
//! {"ev":"text","text","parent"?}                    -> Text
//! {"ev":"thinking","text","parent"?}                -> Reasoning
//! {"ev":"tool","phase":"start","id","name","args"?} -> Tool
//! {"ev":"tool","phase":"end","id","name","error"?}  -> ToolResult
//! {"ev":"usage","input","output","cacheRead"?,"cacheWrite"?} -> Usage
//! {"ev":"turn","status":"finished"|"error"|"cancelled","error"?} -> Signal::Turn
//! {"ev":"fatal","message"}                          -> Signal::Fatal
//! ```
//!
//! # Subagents
//!
//! The SDK streams a subagent's whole transcript as `tool-call-delta` updates, which the shim
//! tags with the spawning task's call id (`parent`). [`ExecEvent`] has no subagent variant, and
//! the transcript of a child must not become the run's reply, so tagged frames are known and
//! dropped. The `task` tool itself is shown as a call, and its end as a result.
//!
//! # The result
//!
//! `Done.result` is the last assistant message: the text after the last tool, reasoning or
//! steering boundary, as in Pi's wire.
//!
//! # Tools
//!
//! | SDK tool | [`ActionClass`] |
//! |---|---|
//! | `shell` | `Command` |
//! | `read`, `grep`, `glob`, `ls`, `readLints`, `semSearch` | `Read` |
//! | `write`, `edit`, `delete`, `applyAgentDiff` | `Edit` |
//! | `webSearch`, `webFetch` | `Network` |
//! | `mcp` (named `mcp__<server>__<tool>` when the SDK says both) | `McpTool` |
//! | anything else (`task`, `updateTodos`, ...) | `Other` |

use std::collections::HashSet;

use reno_proto::{ActionClass, ExecEvent, SessionRef, ToolCall};
use serde_json::Value;

use super::known;
use super::state::valid_agent_id;

/// The most of the last message kept for `Done.result`. The whole text has gone out as `Text`
/// events; this only bounds what the normalizer holds (ARCH-11).
const MAX_RESULT: usize = 1024 * 1024;

/// How a turn of the SDK ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Turn {
    /// The turn finished.
    Finished,
    /// It was cancelled (`interrupt`).
    Cancelled,
    /// It failed, with the SDK's own message.
    Error(String),
}

/// What a frame means for the run beyond its events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Signal {
    /// Nothing the run's control flow cares about.
    None,
    /// One steering input was consumed (by the live turn, or as the next turn).
    Steered,
    /// A turn ended. The run is over only when no steering is outstanding.
    Turn(Turn),
    /// The shim cannot go on (authentication, SDK start, an unusable reply), or the conversation
    /// is not the one asked for.
    Fatal(String),
}

/// What one frame gave.
#[derive(Debug, PartialEq)]
pub(crate) struct Mapped {
    /// The events, in order.
    pub(crate) events: Vec<ExecEvent>,
    /// The signal.
    pub(crate) signal: Signal,
}

impl Mapped {
    fn events(events: Vec<ExecEvent>) -> Self {
        Self {
            events,
            signal: Signal::None,
        }
    }

    fn signal(signal: Signal) -> Self {
        Self {
            events: Vec::new(),
            signal,
        }
    }
}

/// The class of a tool of the SDK's vocabulary.
pub(crate) fn action_class(tool: &str) -> ActionClass {
    match tool {
        "shell" => ActionClass::Command,
        "read" | "grep" | "glob" | "ls" | "readLints" | "semSearch" => ActionClass::Read,
        "write" | "edit" | "delete" | "applyAgentDiff" => ActionClass::Edit,
        "webSearch" | "webFetch" => ActionClass::Network,
        "mcp" => ActionClass::McpTool,
        _ => ActionClass::Other,
    }
}

fn string<'a>(frame: &'a Value, key: &str) -> &'a str {
    frame.get(key).and_then(Value::as_str).unwrap_or_default()
}

fn first_string<'a>(args: &'a Value, keys: &[&str]) -> &'a str {
    keys.iter()
        .find_map(|key| args.get(*key).and_then(Value::as_str))
        .filter(|text| !text.is_empty())
        .unwrap_or_default()
}

/// The call a tool frame announces.
pub(crate) fn tool_call(name: &str, args: &Value) -> ToolCall {
    if name == "mcp" {
        let server = first_string(args, &["server", "serverName"]);
        let tool = first_string(args, &["tool", "toolName", "name"]);
        let named = if server.is_empty() || tool.is_empty() {
            "mcp".to_owned()
        } else {
            format!("mcp__{server}__{tool}")
        };
        let input = args
            .get("args")
            .or_else(|| args.get("input"))
            .unwrap_or(args)
            .clone();
        return ToolCall::new(named, input, ActionClass::McpTool);
    }
    ToolCall::new(name, args.clone(), action_class(name))
}

/// The normalizer of one run.
#[derive(Debug, Default)]
pub(crate) struct Normalizer {
    /// The conversation the run was asked to resume.
    resume: Option<String>,
    /// The model the run asked for, reported when the SDK does not name the one it uses.
    model: Option<String>,
    /// Tools that announced themselves and have not ended.
    open: HashSet<String>,
    reply: String,
    reply_open: bool,
    unknown: Vec<String>,
}

impl Normalizer {
    /// A normalizer for a run that resumes `resume` (if any) and asked for `model` (if any).
    pub(crate) fn new(resume: Option<&str>, model: Option<&str>) -> Self {
        Self {
            resume: resume.map(str::to_owned),
            model: model.map(str::to_owned),
            ..Self::default()
        }
    }

    /// The frame kinds met that the driver does not know, since the last call.
    pub(crate) fn take_unknown(&mut self) -> Vec<String> {
        std::mem::take(&mut self.unknown)
    }

    /// The last assistant message, for `Done.result`.
    pub(crate) fn result(&self) -> Option<String> {
        (!self.reply.is_empty()).then(|| self.reply.clone())
    }

    /// What `frame` means.
    pub(crate) fn map(&mut self, frame: &Value) -> Mapped {
        let kind = string(frame, "ev");
        if !known::is_known_frame(kind) {
            self.unknown.push(format!("ev:{kind}"));
            return Mapped::events(Vec::new());
        }
        let nested = frame.get("parent").is_some_and(|p| !p.is_null());
        match kind {
            "ready" => self.ready(frame),
            "steered" => {
                self.reply_open = false;
                Mapped::signal(Signal::Steered)
            }
            "text" if nested => Mapped::events(Vec::new()),
            "text" => {
                let text = string(frame, "text");
                if text.is_empty() {
                    return Mapped::events(Vec::new());
                }
                if !self.reply_open {
                    self.reply.clear();
                    self.reply_open = true;
                }
                if self.reply.len() < MAX_RESULT {
                    self.reply.push_str(text);
                }
                Mapped::events(vec![ExecEvent::Text { text: text.into() }])
            }
            "thinking" if nested => Mapped::events(Vec::new()),
            "thinking" => {
                self.reply_open = false;
                let text = string(frame, "text");
                Mapped::events(if text.is_empty() {
                    Vec::new()
                } else {
                    vec![ExecEvent::Reasoning { text: text.into() }]
                })
            }
            "tool" if nested => Mapped::events(Vec::new()),
            "tool" => self.tool(frame),
            "usage" => {
                let count = |key: &str| frame.get(key).and_then(Value::as_u64).unwrap_or(0);
                let (cache_read, cache_write) = (count("cacheRead"), count("cacheWrite"));
                // The SDK's input count is the whole prompt, cache reads and writes included
                // (documented for 1.4.1's `model-call-completed`, "counted like `turn-ended`
                // usage"; not checked against 1.0.32's backend). `ExecEvent::Usage` is disjoint.
                Mapped::events(vec![ExecEvent::Usage {
                    input_tokens: count("input")
                        .saturating_sub(cache_read.saturating_add(cache_write)),
                    output_tokens: count("output"),
                    cache_read_tokens: cache_read,
                    cache_write_tokens: cache_write,
                }])
            }
            "turn" => {
                self.reply_open = false;
                let turn = match string(frame, "status") {
                    "cancelled" => Turn::Cancelled,
                    "error" => Turn::Error(
                        Some(string(frame, "error"))
                            .filter(|e| !e.is_empty())
                            .unwrap_or("the Cursor turn failed")
                            .to_owned(),
                    ),
                    _ => Turn::Finished,
                };
                Mapped::signal(Signal::Turn(turn))
            }
            "fatal" => Mapped::signal(Signal::Fatal(
                Some(string(frame, "message"))
                    .filter(|m| !m.is_empty())
                    .unwrap_or("the Cursor shim failed")
                    .to_owned(),
            )),
            _ => Mapped::events(Vec::new()),
        }
    }

    fn ready(&mut self, frame: &Value) -> Mapped {
        let agent = string(frame, "agentId");
        if !valid_agent_id(agent) {
            return Mapped::signal(Signal::Fatal(
                "Cursor reported an agent id Reno cannot keep as a session".into(),
            ));
        }
        if let Some(wanted) = &self.resume
            && wanted != agent
        {
            return Mapped::signal(Signal::Fatal(format!(
                "Cursor resumed conversation {agent} instead of {wanted}"
            )));
        }
        let model = Some(string(frame, "model"))
            .filter(|m| !m.is_empty())
            .map(str::to_owned)
            .or_else(|| self.model.clone())
            .unwrap_or_else(|| super::catalog::AUTO.to_owned());
        Mapped::events(vec![ExecEvent::Started {
            session: SessionRef(agent.to_owned()),
            model,
        }])
    }

    fn tool(&mut self, frame: &Value) -> Mapped {
        let id = string(frame, "id").to_owned();
        let name = Some(string(frame, "name"))
            .filter(|n| !n.is_empty())
            .unwrap_or("tool");
        if !known::is_known_tool(name) {
            self.unknown.push(format!("tool:{name}"));
        }
        let args = frame.get("args").cloned().unwrap_or(Value::Null);
        self.reply_open = false;
        match string(frame, "phase") {
            "start" => {
                self.open.insert(id.clone());
                Mapped::events(vec![ExecEvent::Tool {
                    id,
                    call: tool_call(name, &args),
                }])
            }
            "end" => {
                let mut events = Vec::new();
                // A result answers an earlier call: one that never announced itself is
                // announced here.
                if !self.open.remove(&id) {
                    events.push(ExecEvent::Tool {
                        id: id.clone(),
                        call: tool_call(name, &args),
                    });
                }
                events.push(ExecEvent::ToolResult {
                    id,
                    ok: frame.get("error").and_then(Value::as_bool) != Some(true),
                    summary: None,
                    output: None,
                });
                Mapped::events(events)
            }
            _ => Mapped::events(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn map(normalizer: &mut Normalizer, frame: Value) -> Mapped {
        normalizer.map(&frame)
    }

    fn events(normalizer: &mut Normalizer, frame: Value) -> Vec<ExecEvent> {
        normalizer.map(&frame).events
    }

    #[test]
    fn the_sdk_tool_vocabulary_maps_to_classes() {
        for (tool, class) in [
            ("shell", ActionClass::Command),
            ("read", ActionClass::Read),
            ("grep", ActionClass::Read),
            ("glob", ActionClass::Read),
            ("ls", ActionClass::Read),
            ("write", ActionClass::Edit),
            ("edit", ActionClass::Edit),
            ("delete", ActionClass::Edit),
            ("webSearch", ActionClass::Network),
            ("webFetch", ActionClass::Network),
            ("mcp", ActionClass::McpTool),
            ("task", ActionClass::Other),
            ("updateTodos", ActionClass::Other),
            ("somethingNew", ActionClass::Other),
        ] {
            assert_eq!(action_class(tool), class, "{tool}");
        }
    }

    #[test]
    fn an_mcp_call_is_named_like_the_other_drivers_name_theirs() {
        let call = tool_call(
            "mcp",
            &json!({"server":"reno","tool":"report","args":{"status":"done"}}),
        );
        assert_eq!(call.name, "mcp__reno__report");
        assert_eq!(call.class, ActionClass::McpTool);
        assert_eq!(call.input, json!({"status":"done"}));
        // The SDK named no server: the bare tool name stands.
        assert_eq!(tool_call("mcp", &json!({"tool":"x"})).name, "mcp");
    }

    #[test]
    fn a_run_reads_as_started_text_tools_usage_and_a_turn() {
        let mut n = Normalizer::new(None, Some("composer-2.5"));
        let started = events(
            &mut n,
            json!({"ev":"ready","agentId":"agent-1","model":"composer-2.5"}),
        );
        assert_eq!(
            started,
            [ExecEvent::Started {
                session: SessionRef("agent-1".into()),
                model: "composer-2.5".into()
            }]
        );
        assert_eq!(
            events(&mut n, json!({"ev":"thinking","text":"planning"})),
            [ExecEvent::Reasoning {
                text: "planning".into()
            }]
        );
        events(&mut n, json!({"ev":"text","text":"before "}));
        let tool = events(
            &mut n,
            json!({"ev":"tool","phase":"start","id":"c1","name":"shell","args":{"command":"ls"}}),
        );
        assert!(matches!(
            &tool[..],
            [ExecEvent::Tool { id, call }] if id == "c1" && call.class == ActionClass::Command
        ));
        let end = events(
            &mut n,
            json!({"ev":"tool","phase":"end","id":"c1","name":"shell","error":true}),
        );
        assert_eq!(
            end,
            [ExecEvent::ToolResult {
                id: "c1".into(),
                ok: false,
                summary: None,
                output: None
            }]
        );
        // The result is the text after the last tool: the earlier message is gone.
        events(&mut n, json!({"ev":"text","text":"the "}));
        events(&mut n, json!({"ev":"text","text":"answer"}));
        assert_eq!(n.result().as_deref(), Some("the answer"));
        let turn = map(&mut n, json!({"ev":"turn","status":"finished"}));
        assert_eq!(turn.signal, Signal::Turn(Turn::Finished));
        assert!(n.take_unknown().is_empty());
    }

    #[test]
    fn a_result_with_no_announcement_is_announced_first() {
        let mut n = Normalizer::default();
        let end = events(
            &mut n,
            json!({"ev":"tool","phase":"end","id":"x","name":"grep","args":{"pattern":"a"}}),
        );
        assert!(matches!(
            &end[..],
            [ExecEvent::Tool { id, .. }, ExecEvent::ToolResult { ok: true, .. }] if id == "x"
        ));
    }

    #[test]
    fn the_model_falls_back_to_the_one_asked_for_and_then_to_auto() {
        let mut asked = Normalizer::new(None, Some("grok-4.7"));
        assert!(matches!(
            &events(&mut asked, json!({"ev":"ready","agentId":"a"}))[..],
            [ExecEvent::Started { model, .. }] if model == "grok-4.7"
        ));
        let mut none = Normalizer::default();
        assert!(matches!(
            &events(&mut none, json!({"ev":"ready","agentId":"a"}))[..],
            [ExecEvent::Started { model, .. }] if model == "auto"
        ));
    }

    #[test]
    fn a_subagents_frames_are_known_and_never_the_runs_reply() {
        let mut n = Normalizer::default();
        events(&mut n, json!({"ev":"text","text":"main"}));
        for frame in [
            json!({"ev":"text","text":"sub says","parent":"t1"}),
            json!({"ev":"thinking","text":"sub thinks","parent":"t1"}),
            json!({"ev":"tool","phase":"start","id":"s1","name":"grep","parent":"t1"}),
            json!({"ev":"tool","phase":"end","id":"s1","name":"grep","parent":"t1"}),
        ] {
            assert_eq!(map(&mut n, frame), Mapped::events(Vec::new()));
        }
        assert_eq!(n.result().as_deref(), Some("main"));
        assert!(n.take_unknown().is_empty());
    }

    #[test]
    fn usage_is_disjoint_and_never_underflows() {
        let mut n = Normalizer::default();
        let usage = events(
            &mut n,
            json!({"ev":"usage","input":100,"output":7,"cacheRead":60,"cacheWrite":10}),
        );
        assert_eq!(
            usage,
            [ExecEvent::Usage {
                input_tokens: 30,
                output_tokens: 7,
                cache_read_tokens: 60,
                cache_write_tokens: 10
            }]
        );
        // A count the SDK reports inconsistently is clamped, not wrapped.
        let odd = events(
            &mut n,
            json!({"ev":"usage","input":5,"output":1,"cacheRead":60}),
        );
        assert!(matches!(
            &odd[..],
            [ExecEvent::Usage {
                input_tokens: 0,
                ..
            }]
        ));
    }

    #[test]
    fn turns_end_finished_cancelled_or_failed_and_a_fatal_carries_its_message() {
        let mut n = Normalizer::default();
        assert_eq!(
            map(&mut n, json!({"ev":"turn","status":"cancelled"})).signal,
            Signal::Turn(Turn::Cancelled)
        );
        assert_eq!(
            map(&mut n, json!({"ev":"turn","status":"error","error":"boom"})).signal,
            Signal::Turn(Turn::Error("boom".into()))
        );
        assert!(matches!(
            map(&mut n, json!({"ev":"turn","status":"error"})).signal,
            Signal::Turn(Turn::Error(e)) if !e.is_empty()
        ));
        assert_eq!(
            map(&mut n, json!({"ev":"fatal","message":"not signed in"})).signal,
            Signal::Fatal("not signed in".into())
        );
        assert_eq!(
            map(&mut n, json!({"ev":"steered"})).signal,
            Signal::Steered
        );
    }

    #[test]
    fn a_resume_must_continue_the_conversation_it_was_given_and_ids_must_be_keepable() {
        let mut n = Normalizer::new(Some("agent-a"), None);
        assert!(matches!(
            map(&mut n, json!({"ev":"ready","agentId":"agent-b"})).signal,
            Signal::Fatal(why) if why.contains("agent-a") && why.contains("agent-b")
        ));
        assert!(
            !map(&mut n, json!({"ev":"ready","agentId":"agent-a"}))
                .events
                .is_empty()
        );
        let mut any = Normalizer::default();
        assert!(matches!(
            map(&mut any, json!({"ev":"ready","agentId":"../x"})).signal,
            Signal::Fatal(_)
        ));
    }

    #[test]
    fn frames_and_tools_the_driver_never_saw_are_counted_not_fatal() {
        let mut n = Normalizer::default();
        assert!(events(&mut n, json!({"ev":"hologram","x":1})).is_empty());
        let tool = events(
            &mut n,
            json!({"ev":"tool","phase":"start","id":"1","name":"teleport"}),
        );
        assert!(matches!(
            &tool[..],
            [ExecEvent::Tool { call, .. }] if call.class == ActionClass::Other
        ));
        assert_eq!(n.take_unknown(), ["ev:hologram", "tool:teleport"]);
        assert!(n.take_unknown().is_empty());
    }
}
