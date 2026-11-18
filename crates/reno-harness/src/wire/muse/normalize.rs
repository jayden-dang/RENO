//! Muse session items to [`ExecEvent`]s.
//!
//! Sans-IO (ARCH-6): frames in, events out. The driver owns the process and the life of the run
//! (which turn is ours, when it is over); this module owns what an item, a delta or a terminal
//! means.
//!
//! A turn's work arrives as items, each re-sent whole on `item/started`, `item/updated` and
//! `item/completed` with a rising `revision`, plus `item/delta` appends to one field (`text`,
//! `summary.<n>`, `output`). Text is emitted as the suffix past what was already sent, so a full
//! re-send after deltas never duplicates. A delta belongs to an item the run has seen start: a
//! subagent's own messages carry the child's turn id and their deltas name only the item, so a
//! delta for an item nobody started here is not ours and is dropped.
//!
//! # Item to event
//!
//! | Muse item | `ExecEvent` |
//! |---|---|
//! | `agentMessage` | `Text` (suffixes; a later message of the turn starts a new paragraph) |
//! | `reasoning` | `Reasoning` |
//! | `toolCall`, `userShell` | `Tool`, then `ToolResult` when it ends |
//! | `subagent`, `workflow` | `Tool` named `subagent` / `workflow`, then `ToolResult`; one in progress is background work |
//! | `session/tokenUsage` | `Usage` (disjoint counts: the prompt tokens counted once, less the cache's) |
//! | `session/contextUsage` | `ContextUsage` |
//!
//! `userMessage` (the prompt itself), `reminderChild` (Muse's per-step housekeeping agents),
//! `compaction` and the rest of the transcript's furniture are known and not shown.
//!
//! # Tool names
//!
//! | Muse tool | [`ActionClass`] |
//! |---|---|
//! | `bash`, `shell`, `exec_command`, a `userShell` item | `Command` |
//! | `read`, `read_file`, `view`, `grep`, `search`, `rg`, `glob`, `find`, `list_files`, `ls` | `Read` |
//! | `write`, `write_file`, `create_file`, `edit`, `edit_file`, `str_replace`, `replace`, `apply_patch`, `patch` | `Edit` |
//! | `web_fetch`, `fetch`, `browse`, `web_search` | `Network` |
//! | `<server>__<tool>` (an MCP server's) | `McpTool` |
//! | anything else | `Other` |

use std::collections::{HashMap, HashSet};

use reno_proto::{ActionClass, ExecEvent, StopReason, ToolCall, ToolOutput};
use serde_json::{Value, json};

use super::frames::string;
use super::known;

/// Tools whose own item carries the work (a subagent, a question): their tool row would only
/// duplicate it.
const SHADOWED_TOOLS: &[&str] = &["subagent_spawn", "request_user_input"];

/// The class of a Muse tool call (see the module docs).
pub(crate) fn action_class(tool: &str) -> ActionClass {
    match tool {
        "bash" | "shell" | "exec_command" => ActionClass::Command,
        "read" | "read_file" | "view" | "grep" | "search" | "rg" | "glob" | "find"
        | "list_files" | "ls" => ActionClass::Read,
        "write" | "write_file" | "create_file" | "edit" | "edit_file" | "str_replace"
        | "replace" | "apply_patch" | "patch" => ActionClass::Edit,
        "web_fetch" | "fetch" | "browse" | "web_search" => ActionClass::Network,
        name => match name.split_once("__") {
            Some((server, tool)) if !server.is_empty() && !tool.is_empty() => ActionClass::McpTool,
            _ => ActionClass::Other,
        },
    }
}

/// How a turn ended: the stop reason, and what went wrong when it did not complete.
pub(crate) fn terminal(params: &Value, interrupted: bool) -> (StopReason, Option<String>) {
    let reason = params["error"]["message"]
        .as_str()
        .or_else(|| params["reason"].as_str())
        .filter(|r| !r.is_empty())
        .map(str::to_owned);
    match string(params, "terminal") {
        "completed" => (StopReason::Completed, None),
        "cancelled" if interrupted => (StopReason::Interrupted, None),
        "cancelled" => (
            StopReason::Interrupted,
            reason.filter(|r| !r.contains("cancelled")),
        ),
        _ if params["error"]["requestDenied"] == true => (
            StopReason::Refusal,
            Some(reason.unwrap_or_else(|| "Muse's provider refused the request".into())),
        ),
        other => (
            StopReason::Error,
            Some(reason.unwrap_or_else(|| format!("Muse turn {other}"))),
        ),
    }
}

/// `session/tokenUsage` as a `Usage` event. The four counts are disjoint: the prompt tokens
/// counted once, less what the cache read or wrote. `None` when nothing was used.
pub(crate) fn usage(params: &Value) -> Option<ExecEvent> {
    let counters = &params["usage"];
    let n = |value: &Value, key: &str| value[key].as_u64();
    let output = n(counters, "outputTokens").unwrap_or(0);
    let prompt = n(params, "promptTokens")
        .or_else(|| n(counters, "inputTokens"))
        .unwrap_or(0);
    // A provider that does not tell reads from writes reports one `cachedTokens`: all reads.
    let (read, write) = match (
        n(counters, "cacheReadTokens"),
        n(counters, "cacheWriteTokens"),
    ) {
        (None, None) => (n(counters, "cachedTokens").unwrap_or(0), 0),
        (read, write) => (read.unwrap_or(0), write.unwrap_or(0)),
    };
    let input = prompt.saturating_sub(read).saturating_sub(write);
    (input + output + read + write > 0).then_some(ExecEvent::Usage {
        input_tokens: input,
        output_tokens: output,
        cache_read_tokens: read,
        cache_write_tokens: write,
    })
}

/// `session/contextUsage` as a `ContextUsage` event.
pub(crate) fn context_usage(params: &Value) -> ExecEvent {
    ExecEvent::ContextUsage {
        tokens: params["usedTokens"].as_u64(),
        window: params["windowTokens"].as_u64(),
    }
}

/// The normalizer for one run.
#[derive(Debug, Default)]
pub(crate) struct Normalizer {
    revisions: HashMap<String, i64>,
    kinds: HashMap<String, String>,
    /// What was emitted of each text-bearing item (message text or reasoning summary), to emit
    /// only new suffixes.
    emitted: HashMap<String, String>,
    /// Whether any assistant text went out this turn (later messages are separated from it by a
    /// blank line).
    wrote_text: bool,
    last_message: Option<String>,
    announced: HashSet<String>,
    finished: HashSet<String>,
    /// Subagents and workflows still running: background work the turn's end does not end.
    running: HashSet<String>,
    /// The text of the last assistant message of the run, for the result.
    final_text: String,
    unknown: Vec<String>,
}

impl Normalizer {
    /// A normalizer with no history.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// A new turn: item bookkeeping restarts, running background work carries over.
    pub(crate) fn next_turn(&mut self) {
        let running = std::mem::take(&mut self.running);
        let unknown = std::mem::take(&mut self.unknown);
        let final_text = std::mem::take(&mut self.final_text);
        // A subagent shown in an earlier turn is not shown again when it ends in a later one.
        let announced: HashSet<String> = self
            .announced
            .iter()
            .filter(|id| running.contains(*id))
            .cloned()
            .collect();
        *self = Self::default();
        self.running = running;
        self.announced = announced;
        self.unknown = unknown;
        // The run's result is the last message of the last turn, whichever turn that was.
        self.final_text = final_text;
    }

    /// How many subagents and workflows are running.
    pub(crate) fn background(&self) -> usize {
        self.running.len()
    }

    /// The last assistant message of the run.
    pub(crate) fn final_text(&self) -> Option<String> {
        (!self.final_text.is_empty()).then(|| self.final_text.clone())
    }

    /// The frame kinds met that the driver does not know, since the last call.
    pub(crate) fn take_unknown(&mut self) -> Vec<String> {
        std::mem::take(&mut self.unknown)
    }

    /// Whether `item` belongs to the run's own turns is the driver's call; this is told only
    /// about the items that do. Background work finishing outside a turn goes through
    /// [`Self::background_item`] instead.
    pub(crate) fn item(&mut self, item: &Value) -> Vec<ExecEvent> {
        let id = string(item, "itemId");
        if id.is_empty() {
            return Vec::new();
        }
        let kind = string(item, "kind");
        if !known::is_known_item(kind) {
            self.unknown.push(format!("item:{kind}"));
            return Vec::new();
        }
        let revision = item["revision"].as_i64().unwrap_or(0);
        if self.revisions.get(id).is_some_and(|seen| *seen >= revision) {
            return Vec::new();
        }
        self.revisions.insert(id.into(), revision);
        self.kinds.insert(id.into(), kind.into());
        match kind {
            "agentMessage" => self.text(id, string(item, "text"), false),
            "reasoning" => self.text(id, &reasoning_text(item), true),
            "toolCall" | "userShell" => self.tool(id, item),
            "subagent" => self.background_work(id, item, "subagent"),
            "workflow" => self.background_work(id, item, "workflow"),
            _ => Vec::new(),
        }
    }

    /// A subagent or workflow item seen outside a turn of the run (background work finishing
    /// while the host is idle): it only counts the work down.
    pub(crate) fn background_item(&mut self, item: &Value) -> Vec<ExecEvent> {
        match string(item, "kind") {
            "subagent" | "workflow" => self.item(item),
            _ => Vec::new(),
        }
    }

    /// One `item/delta`: `field` names what grows.
    pub(crate) fn delta(&mut self, params: &Value) -> Vec<ExecEvent> {
        let id = string(params, "itemId");
        let delta = string(params, "delta");
        // Only an item that started here: a child's messages are not the run's.
        let Some(kind) = self.kinds.get(id).map(String::as_str) else {
            return Vec::new();
        };
        if delta.is_empty() || self.finished.contains(id) {
            return Vec::new();
        }
        let field = params["field"].as_str().unwrap_or("text");
        let reasoning = match (field, kind) {
            ("text", "reasoning") => true,
            ("text", "agentMessage") => false,
            (field, _) if field.starts_with("summary.") => true,
            _ => return Vec::new(),
        };
        let mut so_far = self.emitted.get(id).cloned().unwrap_or_default();
        if reasoning && field.starts_with("summary.") && !so_far.is_empty() {
            // Summary parts are separate paragraphs; a new index starts one.
            let index: usize = field[8..].parse().unwrap_or(0);
            if index >= so_far.split("\n\n").count() {
                so_far.push_str("\n\n");
            }
        }
        so_far.push_str(delta);
        self.text(id, &so_far, reasoning)
    }

    fn text(&mut self, id: &str, text: &str, reasoning: bool) -> Vec<ExecEvent> {
        if !reasoning {
            self.final_text = text.to_owned();
        }
        let previous = self.emitted.entry(id.into()).or_default();
        // A rewrite, not a growth: what went out stands.
        let Some(suffix) = text.strip_prefix(previous.as_str()) else {
            return Vec::new();
        };
        if suffix.is_empty() {
            return Vec::new();
        }
        let mut suffix = suffix.to_owned();
        let first = previous.is_empty();
        *previous = text.into();
        if reasoning {
            return vec![ExecEvent::Reasoning { text: suffix }];
        }
        if first && self.wrote_text && self.last_message.as_deref() != Some(id) {
            suffix.insert_str(0, "\n\n");
        }
        self.wrote_text = true;
        self.last_message = Some(id.into());
        vec![ExecEvent::Text { text: suffix }]
    }

    fn tool(&mut self, id: &str, item: &Value) -> Vec<ExecEvent> {
        let user_shell = string(item, "kind") == "userShell";
        let tool = if user_shell {
            "bash"
        } else {
            string(item, "tool")
        };
        let status = string(item, "status");
        let failed = status == "failed";
        if SHADOWED_TOOLS.contains(&tool) && !failed {
            return Vec::new();
        }
        let args = arguments(item);
        let mut events = Vec::new();
        // Arguments stream as model-authored JSON: announce once they parse, or at the latest
        // when the call ends.
        if !self.announced.contains(id) && (args.is_some() || user_shell || status != "inProgress")
        {
            self.announced.insert(id.into());
            let input = if user_shell {
                json!({"command": string(item, "commandText")})
            } else {
                args.clone().unwrap_or(Value::Null)
            };
            events.push(ExecEvent::Tool {
                id: id.into(),
                call: ToolCall::new(tool, input, action_class(tool)),
            });
        }
        if status != "inProgress" && self.announced.contains(id) && self.finished.insert(id.into())
        {
            let ok = status == "completed" && item["exitCode"].as_i64().is_none_or(|c| c == 0);
            let text = item["visibleOutput"]
                .as_str()
                .or_else(|| item["failureReason"].as_str())
                .unwrap_or("");
            events.push(ExecEvent::ToolResult {
                id: id.into(),
                ok,
                summary: (!ok).then(|| first_line(text)).flatten(),
                output: ToolOutput::from_raw_opt(text),
            });
        }
        events
    }

    /// A subagent or a workflow: shown as one tool row that ends with its result. Muse runs a
    /// subagent as a child session the host cannot address, so its own transcript is not
    /// streamed.
    fn background_work(&mut self, id: &str, item: &Value, name: &str) -> Vec<ExecEvent> {
        let status = string(item, "status");
        let mut events = Vec::new();
        if self.announced.insert(id.into()) {
            let input = if name == "subagent" {
                json!({"objective": item["objective"], "role": item["role"]})
            } else {
                json!({"entry": item["entryId"]})
            };
            events.push(ExecEvent::Tool {
                id: id.into(),
                call: ToolCall::new(name, input, ActionClass::Other),
            });
        }
        if status == "inProgress" {
            self.running.insert(id.into());
        } else {
            self.running.remove(id);
            if self.finished.insert(id.into()) {
                let ok = status == "completed";
                let text = item["result"]["summary"]
                    .as_str()
                    .or_else(|| item["finalSummary"].as_str())
                    .or_else(|| item["failureReason"].as_str())
                    .unwrap_or("");
                events.push(ExecEvent::ToolResult {
                    id: id.into(),
                    ok,
                    summary: (!ok).then(|| first_line(text)).flatten(),
                    output: ToolOutput::from_raw_opt(text),
                });
            }
        }
        events
    }
}

/// The first non-empty line, cut to 200 characters.
fn first_line(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let mut short: String = line.chars().take(200).collect();
    if short.len() < line.len() {
        short.push('\u{2026}');
    }
    Some(short)
}

fn reasoning_text(item: &Value) -> String {
    match item["summary"].as_array() {
        Some(parts) if !parts.is_empty() => parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => string(item, "text").into(),
    }
}

/// The arguments of a `toolCall`: the model-authored JSON, parsed. `None` while it does not
/// parse as an object yet.
fn arguments(item: &Value) -> Option<Value> {
    match &item["args"] {
        Value::String(raw) => serde_json::from_str::<Value>(raw)
            .ok()
            .filter(Value::is_object),
        Value::Object(_) => Some(item["args"].clone()),
        _ => None,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn item(id: &str, kind: &str, revision: i64, status: &str, extra: Value) -> Value {
        let mut item = json!({"itemId": id, "kind": kind, "revision": revision, "status": status});
        item.as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        item
    }

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
    fn message_text_streams_once_across_deltas_and_full_resends() {
        let mut n = Normalizer::new();
        assert!(
            n.item(&item(
                "m",
                "agentMessage",
                1,
                "inProgress",
                json!({"text": ""})
            ))
            .is_empty()
        );
        let events = n.delta(&json!({"itemId": "m", "field": "text", "delta": "Hel"}));
        assert_eq!(texts(&events), ["Hel"]);
        n.delta(&json!({"itemId": "m", "field": "text", "delta": "lo"}));
        // The completed re-send adds only what the deltas did not carry.
        let events = n.item(&item(
            "m",
            "agentMessage",
            2,
            "completed",
            json!({"text": "Hello!"}),
        ));
        assert_eq!(texts(&events), ["!"]);
        // A stale revision is ignored.
        assert!(
            n.item(&item(
                "m",
                "agentMessage",
                2,
                "completed",
                json!({"text": "Hello!!"})
            ))
            .is_empty()
        );
        assert_eq!(n.final_text().as_deref(), Some("Hello!"));
    }

    #[test]
    fn a_later_message_of_the_turn_is_a_new_paragraph() {
        let mut n = Normalizer::new();
        let first = n.item(&item(
            "a",
            "agentMessage",
            1,
            "completed",
            json!({"text": "one"}),
        ));
        let second = n.item(&item(
            "b",
            "agentMessage",
            1,
            "completed",
            json!({"text": "two"}),
        ));
        assert_eq!(texts(&first), ["one"]);
        assert_eq!(texts(&second), ["\n\ntwo"]);
        assert_eq!(n.final_text().as_deref(), Some("two"));
    }

    #[test]
    fn a_delta_for_an_item_that_never_started_here_is_not_the_runs() {
        // A subagent's own message: its item names the child's turn and the driver never hands it
        // over, but its deltas name only the item.
        let mut n = Normalizer::new();
        assert!(
            n.delta(&json!({"itemId": "child-msg", "field": "text", "delta": "Hello"}))
                .is_empty()
        );
    }

    #[test]
    fn reasoning_streams_by_summary_part_and_is_never_text() {
        let mut n = Normalizer::new();
        n.item(&item(
            "r",
            "reasoning",
            1,
            "inProgress",
            json!({"summary": []}),
        ));
        let one = n.delta(&json!({"itemId": "r", "field": "summary.0", "delta": "think"}));
        let two = n.delta(&json!({"itemId": "r", "field": "summary.1", "delta": "more"}));
        assert_eq!(
            one,
            [ExecEvent::Reasoning {
                text: "think".into()
            }]
        );
        assert_eq!(
            two,
            [ExecEvent::Reasoning {
                text: "\n\nmore".into()
            }]
        );
        assert!(texts(&one).is_empty() && n.final_text().is_none());
    }

    #[test]
    fn a_tool_call_is_announced_once_its_arguments_parse_and_ends_with_its_output() {
        let mut n = Normalizer::new();
        let started = n.item(&item(
            "t",
            "toolCall",
            1,
            "inProgress",
            json!({"tool": "bash", "args": "{\"command\":\"ec"}),
        ));
        assert!(started.is_empty(), "arguments still streaming");
        let announced = n.item(&item(
            "t",
            "toolCall",
            2,
            "inProgress",
            json!({"tool": "bash", "args": "{\"command\":\"echo hi\"}"}),
        ));
        assert!(matches!(
            &announced[..],
            [ExecEvent::Tool { id, call }] if id == "t"
                && call.name == "bash"
                && call.class == ActionClass::Command
                && call.input["command"] == "echo hi"
        ));
        let done = n.item(&item(
            "t",
            "toolCall",
            3,
            "completed",
            json!({"tool": "bash", "args": "{\"command\":\"echo hi\"}", "visibleOutput": "hi\n", "exitCode": 0}),
        ));
        assert!(matches!(
            &done[..],
            [ExecEvent::ToolResult { id, ok: true, summary: None, output: Some(o) }]
                if id == "t" && o.text == "hi\n"
        ));
    }

    #[test]
    fn a_failed_call_or_a_non_zero_exit_is_not_ok_and_says_why() {
        let mut n = Normalizer::new();
        let failed = n.item(&item(
            "f",
            "toolCall",
            1,
            "failed",
            json!({"tool": "read", "args": "{\"path\":\"/x\"}", "failureReason": "no such file\nmore"}),
        ));
        assert!(matches!(
            &failed[..],
            [_, ExecEvent::ToolResult { ok: false, summary: Some(s), .. }] if s == "no such file"
        ));
        let exit = n.item(&item(
            "e",
            "toolCall",
            1,
            "completed",
            json!({"tool": "bash", "args": "{\"command\":\"false\"}", "exitCode": 1, "visibleOutput": "boom"}),
        ));
        assert!(matches!(
            &exit[..],
            [_, ExecEvent::ToolResult { ok: false, .. }]
        ));
    }

    #[test]
    fn a_subagents_spawn_tool_is_shadowed_by_its_own_item_unless_it_failed() {
        let mut n = Normalizer::new();
        assert!(
            n.item(&item(
                "s",
                "toolCall",
                1,
                "completed",
                json!({"tool": "subagent_spawn", "args": "{}"})
            ))
            .is_empty()
        );
        let failed = n.item(&item(
            "s2",
            "toolCall",
            1,
            "failed",
            json!({"tool": "subagent_spawn", "args": "{}", "failureReason": "no"}),
        ));
        assert_eq!(failed.len(), 2);
    }

    #[test]
    fn a_running_subagent_is_background_work_until_its_item_ends() {
        let mut n = Normalizer::new();
        let started = n.item(&item(
            "sub",
            "subagent",
            1,
            "inProgress",
            json!({"objective": "reply Hello", "role": "hello agent"}),
        ));
        assert!(matches!(
            &started[..],
            [ExecEvent::Tool { call, .. }] if call.name == "subagent"
                && call.input["objective"] == "reply Hello"
        ));
        assert_eq!(n.background(), 1);
        // The next turn keeps counting it.
        n.next_turn();
        assert_eq!(n.background(), 1);
        let ended = n.background_item(&item(
            "sub",
            "subagent",
            2,
            "completed",
            json!({"result": {"summary": "Hello"}}),
        ));
        assert!(matches!(
            &ended[..],
            [ExecEvent::ToolResult { ok: true, output: Some(o), .. }] if o.text == "Hello"
        ));
        assert_eq!(n.background(), 0);
    }

    #[test]
    fn housekeeping_items_are_known_and_a_new_kind_is_counted() {
        let mut n = Normalizer::new();
        assert!(
            n.item(&item(
                "u",
                "userMessage",
                1,
                "completed",
                json!({"text": "hi"})
            ))
            .is_empty()
        );
        assert!(
            n.item(&item("r", "reminderChild", 1, "inProgress", json!({})))
                .is_empty()
        );
        assert!(n.take_unknown().is_empty());
        assert!(
            n.item(&item("h", "hologram", 1, "completed", json!({})))
                .is_empty()
        );
        assert_eq!(n.take_unknown(), ["item:hologram"]);
    }

    #[test]
    fn turns_end_as_their_terminal_says() {
        let done = json!({"terminal": "completed"});
        assert_eq!(terminal(&done, false), (StopReason::Completed, None));
        let cancelled = json!({"terminal": "cancelled", "reason": "turn cancelled by the user"});
        assert_eq!(terminal(&cancelled, true), (StopReason::Interrupted, None));
        assert_eq!(terminal(&cancelled, false), (StopReason::Interrupted, None));
        let failed = json!({"terminal": "failed", "error": {"message": "quota", "kind": "x"}});
        assert_eq!(
            terminal(&failed, false),
            (StopReason::Error, Some("quota".into()))
        );
        let denied =
            json!({"terminal": "failed", "error": {"message": "no", "requestDenied": true}});
        assert_eq!(terminal(&denied, false).0, StopReason::Refusal);
        assert_eq!(
            terminal(&json!({"terminal": "exploded"}), false),
            (StopReason::Error, Some("Muse turn exploded".into()))
        );
    }

    #[test]
    fn usage_counts_the_prompt_once_and_keeps_the_cache_apart() {
        let both = usage(&json!({"promptTokens": 1000, "usage": {
            "inputTokens": 1000, "outputTokens": 40, "cachedTokens": 900,
            "cacheReadTokens": 800, "cacheWriteTokens": 100}}));
        assert_eq!(
            both,
            Some(ExecEvent::Usage {
                input_tokens: 100,
                output_tokens: 40,
                cache_read_tokens: 800,
                cache_write_tokens: 100
            })
        );
        // A provider that does not tell reads from writes: cachedTokens are reads.
        let cached = usage(&json!({"promptTokens": 33144, "usage": {
            "inputTokens": 33144, "outputTokens": 123, "cachedTokens": 32625}}));
        assert_eq!(
            cached,
            Some(ExecEvent::Usage {
                input_tokens: 519,
                output_tokens: 123,
                cache_read_tokens: 32625,
                cache_write_tokens: 0
            })
        );
        assert_eq!(
            usage(&json!({"promptTokens": 0, "usage": {"outputTokens": 0}})),
            None
        );
        assert_eq!(
            context_usage(&json!({"usedTokens": 33029, "windowTokens": 1007997})),
            ExecEvent::ContextUsage {
                tokens: Some(33029),
                window: Some(1_007_997)
            }
        );
    }

    #[test]
    fn tools_are_classed_by_what_they_do() {
        assert_eq!(action_class("bash"), ActionClass::Command);
        assert_eq!(action_class("grep"), ActionClass::Read);
        assert_eq!(action_class("str_replace"), ActionClass::Edit);
        assert_eq!(action_class("web_fetch"), ActionClass::Network);
        assert_eq!(action_class("linear__search"), ActionClass::McpTool);
        assert_eq!(action_class("__x"), ActionClass::Other);
        assert_eq!(action_class("todo_write"), ActionClass::Other);
    }
}
