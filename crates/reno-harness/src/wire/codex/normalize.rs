//! Codex app-server notification and item to [`ExecEvent`] normalization.
//!
//! Sans-IO (ARCH-6): notifications in, events out. The driver owns the clock (it passes `now`
//! for a rate limit's reset text) and the turn lifecycle (which turn is in flight, what settles
//! it); this module owns what an item or a delta means.
//!
//! Tolerant by construction: both field spellings the app server has shipped
//! (`delta`/`textDelta`, `exitCode`/`exit_code`, camelCase and snake_case item types) are
//! accepted, and an unknown item type or notification maps to nothing.
//!
//! # Item to tool call
//!
//! | Item | Tool name | [`ActionClass`] | Input |
//! |---|---|---|---|
//! | `commandExecution` | `exec` | `Command` | `{ command, cwd? }` |
//! | `fileChange` | `apply_patch` | `Edit` | `{ changes: [{ path, kind }] }` |
//! | `mcpToolCall` | `mcp__<server>__<tool>` | `McpTool` | the call's `arguments` |
//! | `webSearch` | `web_search` | `Network` | `{ query }` |
//! | `imageView` | `view_image` | `Read` | `{ path }` |
//! | `imageGeneration` | `image_generation` | `Other` | none |
//! | `todoList` | `todo` | `Other` | `{ items }` |
//! | `dynamicToolCall` | the tool's name | `Other` | its `arguments` |
//! | `collabAgentToolCall` | the collab tool, snake-cased (`spawn_agent`, `wait`, ...) | `Other` | the item |
//! | `subAgentActivity` (started or spawned) | `sub_agent` | `Other` | the item |
//!
//! A call opens when its item starts (or, for an item that only ever completes, when it
//! completes) and closes once with a `ToolResult`. A child thread's traffic is never mapped:
//! see [`route_child_notification`].

use std::collections::{HashMap, HashSet, VecDeque};

use reno_proto::{ActionClass, ExecEvent, StopReason, ToolCall};
use serde_json::{Value, json};

use super::permission::{COMMAND_TOOL, FILE_CHANGE_TOOL};

/// The longest tool-result summary, in characters.
const SUMMARY_CHARS: usize = 200;
/// Closed call ids and open file changes remembered at once.
const REMEMBERED: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Started,
    Completed,
}

fn field<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|k| v.get(*k))
}

fn str_field(v: &Value, keys: &[&str]) -> String {
    field(v, keys)
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned()
}

/// Delta text under either spelling the app server has used (`delta` on agentMessage,
/// `textDelta` on some reasoning builds).
pub(crate) fn delta_text(params: &Value) -> Option<String> {
    field(params, &["delta", "textDelta"])
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Codex streams independent reasoning items and indexed parts without text separators.
/// Preserve those boundaries; otherwise adjacent Markdown headings become `**one****two**`. One
/// instance belongs to one thread, so child activity cannot split a parent's in-flight paragraph.
#[derive(Default)]
pub(crate) struct ReasoningStream {
    last_part: Option<(String, bool, u64)>,
    announced_summary: Option<(String, u64)>,
    trailing_newlines: usize,
}

impl ReasoningStream {
    pub(crate) fn map(&mut self, method: &str, params: &Value) -> Vec<ExecEvent> {
        let id = item_id(params);
        let summary = method != "item/reasoning/textDelta";
        let index = field(
            params,
            if summary {
                &["summaryIndex", "summary_index"]
            } else {
                &["contentIndex", "content_index"]
            },
        )
        .and_then(Value::as_u64)
        .or_else(|| {
            self.announced_summary
                .as_ref()
                .filter(|(item, _)| summary && item == &id)
                .map(|(_, index)| *index)
        })
        .unwrap_or(0);
        if method == "item/reasoning/summaryPartAdded" {
            self.announced_summary = Some((id, index));
            return Vec::new();
        }
        let Some(text) = delta_text(params) else {
            return Vec::new();
        };
        let part = (id, summary, index);
        let mut events = Vec::new();
        if self.last_part.as_ref().is_some_and(|last| last != &part) {
            let leading_newlines = text.chars().take_while(|&c| c == '\n').count();
            let missing = 2usize.saturating_sub(self.trailing_newlines + leading_newlines);
            if missing > 0 {
                events.push(ExecEvent::Reasoning {
                    text: "\n".repeat(missing),
                });
                self.trailing_newlines += missing;
            }
        }
        let trailing = text.chars().rev().take_while(|&c| c == '\n').count();
        self.trailing_newlines = if trailing == text.len() {
            self.trailing_newlines + trailing
        } else {
            trailing
        };
        self.last_part = Some(part);
        events.push(ExecEvent::Reasoning { text });
        events
    }
}

pub(crate) fn item_id(params: &Value) -> String {
    str_field(params, &["itemId", "item_id"])
}

/// `params.turn.id` on the turn/* lifecycle notifications.
pub(crate) fn turn_id(params: &Value) -> String {
    params
        .get("turn")
        .map(|t| str_field(t, &["id"]))
        .unwrap_or_default()
}

/// `params.turn.error` as display text (turn/completed carries an optional error; turn/failed
/// always should). `limits` is the account's latest `account/rateLimits/updated` snapshot, for
/// a limit error's reset time.
pub(crate) fn turn_error_message(
    params: &Value,
    limits: Option<&Value>,
    now_unix: i64,
) -> Option<String> {
    params
        .get("turn")
        .and_then(|t| t.get("error"))
        .filter(|e| !e.is_null())
        .map(|e| codex_error_text(e, limits, now_unix))
}

/// What the person reads when the provider refuses Codex's credentials (a 401): a missing or
/// expired ChatGPT login, or an API key (OpenAI's or a custom provider's) that is wrong or
/// revoked.
pub(crate) const SIGNED_OUT_MESSAGE: &str = "Codex's credentials were rejected. Run codex login \
     (or check the API key for your model provider), then send again.";

/// A Codex `TurnError` (`turn.error`, or an `error` notification's `error`) as display text. The
/// structured `codexErrorInfo` decides the sign-in and limit cases instead of the prose (a retry
/// note mentioning "rate limit" is not a limit); `additionalDetails`, when present, says more
/// than the generic message.
pub(crate) fn codex_error_text(error: &Value, limits: Option<&Value>, now_unix: i64) -> String {
    let message = str_field(error, &["message"]);
    let details = error
        .get("additionalDetails")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|d| !d.is_empty());
    let info = error.get("codexErrorInfo").unwrap_or(&Value::Null);
    let code = match info {
        Value::String(code) => Some(code.as_str()),
        Value::Object(variant) => variant.keys().next().map(String::as_str),
        _ => None,
    };
    // The transport variants carry the upstream status: a missing or dead login fails as
    // `httpConnectionFailed { httpStatusCode: 401 }` (live, 0.164.0), and retries exhausted on
    // a 429 are a rate limit.
    let http_status = info
        .as_object()
        .and_then(|variant| variant.values().next())
        .and_then(|detail| detail.get("httpStatusCode"))
        .and_then(Value::as_u64);
    let limit = match (code, http_status) {
        (Some("unauthorized"), _) | (_, Some(401)) => return SIGNED_OUT_MESSAGE.to_owned(),
        (Some("usageLimitExceeded"), _) => Some("Codex usage limit reached"),
        (Some("rateLimitExceeded"), _) | (_, Some(429)) => Some("Codex rate limit reached"),
        _ => None,
    };
    let text = details.map(str::to_owned).unwrap_or_else(|| {
        if message.is_empty() {
            error.to_string()
        } else {
            message
        }
    });
    let Some(limit) = limit else {
        return text;
    };
    match limits.and_then(|limits| limit_reset_in(limits, now_unix)) {
        Some(wait) => format!("{limit} — it resets in {}.", describe_wait(wait)),
        None if text.is_empty() => format!("{limit}."),
        None => format!("{limit}. {text}"),
    }
}

/// Seconds until an exhausted rate-limit window of `limits` (a `RateLimitSnapshot`) resets: the
/// latest one, since usage resumes only when every exhausted window has. `None` when no window
/// is exhausted: the limit hit is one the snapshot doesn't describe.
fn limit_reset_in(limits: &Value, now: i64) -> Option<i64> {
    ["primary", "secondary"]
        .iter()
        .filter_map(|key| limits.get(*key))
        .filter(|window| {
            window
                .get("usedPercent")
                .and_then(Value::as_f64)
                .is_some_and(|used| used >= 100.0)
        })
        .filter_map(|window| window.get("resetsAt").and_then(Value::as_i64))
        .max()
        .map(|at| (at - now).max(0))
}

fn describe_wait(secs: i64) -> String {
    let minutes = (secs + 59) / 60;
    let (days, hours, minutes) = (minutes / 1440, minutes / 60 % 24, minutes % 60);
    match (days, hours, minutes) {
        (0, 0, 0) => "under a minute".into(),
        (0, 0, m) => format!("{m}m"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

/// Fold a sparse `account/rateLimits/updated` snapshot into the last one: a window or field the
/// update leaves out (or null) keeps its value.
fn merge_rate_limits(held: &mut Option<Value>, params: &Value) {
    let Some(update) = params.get("rateLimits").and_then(Value::as_object) else {
        return;
    };
    let held = held.get_or_insert_with(|| Value::Object(Default::default()));
    if let Some(held) = held.as_object_mut() {
        for (key, value) in update {
            if !value.is_null() {
                held.insert(key.clone(), value.clone());
            }
        }
    }
}

/// An `error` notification. 0.146.x nests it (`params.error`); older builds were flat
/// (`params.message`).
pub(crate) struct ErrorNotice {
    pub(crate) message: String,
    /// Codex is retrying the request itself ("Reconnecting... 2/5"): progress, not an outcome.
    /// Missing on old builds, which only sent final errors.
    pub(crate) will_retry: bool,
}

pub(crate) fn error_notice(params: &Value, limits: Option<&Value>, now_unix: i64) -> ErrorNotice {
    let message = match params.get("error").filter(|e| e.is_object()) {
        Some(error) => codex_error_text(error, limits, now_unix),
        None => str_field(params, &["message"]),
    };
    ErrorNotice {
        message: if message.is_empty() {
            "Codex error".into()
        } else {
            message
        },
        will_retry: params
            .get("willRetry")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }
}

/// How a settled turn reports its error, given the last final `error` notification it raised
/// (`held`). `turn.error` wins; a failed turn without one takes `held`; a turn that did not fail
/// keeps `held` as a standalone notice. Returns the turn's error and that notice.
pub(crate) fn settle_turn_error(
    params: &Value,
    held: Option<String>,
    limits: Option<&Value>,
    now_unix: i64,
) -> (Option<String>, Option<String>) {
    let failed = params.pointer("/turn/status").and_then(Value::as_str) == Some("failed");
    match turn_error_message(params, limits, now_unix) {
        Some(error) => (Some(error), None),
        None if failed => (
            Some(held.unwrap_or_else(|| "Codex turn failed".to_owned())),
            None,
        ),
        None => (None, held),
    }
}

/// `thread/tokenUsage/updated` as a [`ExecEvent::Usage`] snapshot of the LAST turn's tokens
/// (held until the turn ends, emitted just before its `Done`).
pub(crate) fn usage_event(params: &Value) -> Option<ExecEvent> {
    let last = field(params, &["tokenUsage", "token_usage"])?.get("last")?;
    let count = |keys: &[&str]| {
        field(last, keys)
            .and_then(Value::as_u64)
            .unwrap_or_default()
    };
    // Codex's `inputTokens` is the whole input: `cachedInputTokens` and `cacheWriteInputTokens`
    // are subsets of it (the Responses API's `input_tokens_details`). `Usage.input_tokens` is
    // the uncached remainder, so the cached parts come out of it and nothing is counted twice.
    let cache_read = count(&["cachedInputTokens", "cached_input_tokens"]);
    let cache_write = count(&["cacheWriteInputTokens", "cache_write_input_tokens"]);
    let input = count(&["inputTokens", "input_tokens"]);
    Some(ExecEvent::Usage {
        input_tokens: input.saturating_sub(cache_read.saturating_add(cache_write)),
        output_tokens: count(&["outputTokens", "output_tokens"]),
        cache_read_tokens: cache_read,
        cache_write_tokens: cache_write,
    })
}

pub(crate) fn item_type(item: &Value) -> &str {
    item.get("type").and_then(Value::as_str).unwrap_or("")
}

/// The thread a notification is addressed to: `thread/started` carries it at
/// `params.thread.id`, everything else at `params.threadId`. `None` for thread-less methods (old
/// builds, account noise).
pub(crate) fn notification_thread_id(method: &str, params: &Value) -> Option<String> {
    if method == "thread/started" {
        return params
            .get("thread")
            .and_then(|t| t.get("id"))
            .and_then(Value::as_str)
            .map(str::to_owned);
    }
    field(params, &["threadId", "thread_id"])
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// What becomes of a notification addressed to a thread that is not the run's own (a child
/// agent's).
///
/// Exported and pure so the table can be asserted directly. The shape (and the fail-open
/// default) is load-bearing: two shipped bugs in an earlier Codex client came from a catch-all
/// that swallowed everything a child emitted, and a swallowed `serverRequest/resolved` left the
/// parent's approvals stuck.
///
/// - `Dropped`: a child's content, bookkeeping and lifecycle. A child's transcript is not the
///   run's, and none of it may settle the run's turn (a child's `turn/completed` is the exact
///   bug class this table exists for), or rewrite its state (`thread/started` repeats,
///   status, name and usage updates).
/// - `Parent`: pass through to the run's own path. Unknown methods land here BY DESIGN, so a
///   codex update that adds a notification degrades to "the run sees it", never to silent loss.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChildRoute {
    Dropped,
    Parent,
}

pub(crate) fn route_child_notification(method: &str) -> ChildRoute {
    match method {
        // Child message/reasoning deltas DO stream on this wire tagged with the child's
        // threadId (live-verified against codex-cli 0.146.1), and so do its turn ends.
        "item/started"
        | "item/completed"
        | "item/agentMessage/delta"
        | "item/reasoning/textDelta"
        | "item/reasoning/summaryTextDelta"
        | "item/reasoning/summaryPartAdded"
        | "turn/started"
        | "turn/completed"
        | "turn/failed"
        | "turn/aborted"
        | "error"
        | "thread/closed"
        // Child status bookkeeping and chatter with no consumer on this wire.
        | "thread/status/changed"
        | "thread/tokenUsage/updated"
        | "item/commandExecution/outputDelta"
        | "item/fileChange/outputDelta"
        | "item/fileChange/patchUpdated"
        | "item/plan/delta"
        | "turn/plan/updated"
        | "turn/diff/updated"
        | "thread/name/updated"
        | "thread/settings/updated"
        | "rawResponseItem/completed"
        // Child-owned thread lifecycle that maps onto the run's state in a naive passthrough.
        | "thread/archived"
        | "thread/unarchived"
        | "thread/compacted"
        | "thread/started" => ChildRoute::Dropped,
        // Unknown or run-owned (approvals bookkeeping, account noise).
        _ => ChildRoute::Parent,
    }
}

/// A collab tool's name (`spawnAgent`) as the snake-case tool name (`spawn_agent`).
fn snake_case(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 2);
    for c in name.chars() {
        if c.is_ascii_uppercase() {
            if !out.is_empty() {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// A one-line outcome: the exit code, then the last non-empty line of output, redacted.
fn summarize(output: Option<&str>, exit_code: Option<i32>) -> Option<String> {
    let last = output
        .and_then(|output| {
            output
                .lines()
                .rev()
                .map(str::trim)
                .find(|line| !line.is_empty())
        })
        .map(|line| {
            let line = crate::redact_secrets(line);
            if line.chars().count() > SUMMARY_CHARS {
                let cut: String = line.chars().take(SUMMARY_CHARS).collect();
                format!("{cut}…")
            } else {
                line
            }
        });
    match (exit_code, last) {
        (Some(code), Some(line)) => Some(format!("exit code {code}: {line}")),
        (Some(code), None) => Some(format!("exit code {code}")),
        (None, line) => line,
    }
}

fn failed(status: &str) -> bool {
    matches!(
        status,
        "failed" | "errored" | "declined" | "cancelled" | "canceled"
    )
}

/// The `{ path, kind }` list of a `fileChange` item. Unknown kinds degrade to `update`.
fn changes(item: &Value) -> Vec<(String, &'static str)> {
    item.get("changes")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|change| {
            let path = str_field(change, &["path"]);
            // A string on older app-servers, `{ type, .. }` on 0.169.
            let kind = change.get("kind").and_then(|kind| {
                kind.as_str()
                    .or_else(|| kind.get("type").and_then(Value::as_str))
            });
            let kind = match kind {
                Some("add") => "add",
                Some("delete") => "delete",
                _ => "update",
            };
            (!path.is_empty()).then_some((path, kind))
        })
        .collect()
}

/// The tool call an item stands for, or `None` for an item that is no tool call.
fn call(item: &Value) -> Option<ToolCall> {
    Some(match item_type(item) {
        "commandExecution" | "command_execution" => {
            let mut input = json!({ "command": str_field(item, &["command"]) });
            let cwd = str_field(item, &["cwd"]);
            if !cwd.is_empty() {
                input["cwd"] = cwd.into();
            }
            ToolCall::new(COMMAND_TOOL, input, ActionClass::Command)
        }
        "fileChange" | "file_change" => {
            let changes: Vec<Value> = changes(item)
                .into_iter()
                .map(|(path, kind)| json!({ "path": path, "kind": kind }))
                .collect();
            ToolCall::new(
                FILE_CHANGE_TOOL,
                json!({ "changes": changes }),
                ActionClass::Edit,
            )
        }
        "mcpToolCall" | "mcp_tool_call" => ToolCall::new(
            format!(
                "mcp__{}__{}",
                str_field(item, &["server"]),
                str_field(item, &["tool"])
            ),
            item.get("arguments").cloned().unwrap_or(Value::Null),
            ActionClass::McpTool,
        ),
        "dynamicToolCall" | "dynamic_tool_call" => {
            let tool = str_field(item, &["tool"]);
            let name = match str_field(item, &["namespace"]).as_str() {
                "" => tool,
                namespace => format!("{namespace}__{tool}"),
            };
            ToolCall::new(
                name,
                item.get("arguments").cloned().unwrap_or(Value::Null),
                ActionClass::Other,
            )
        }
        "webSearch" | "web_search" => ToolCall::new(
            "web_search",
            json!({ "query": str_field(item, &["query"]) }),
            ActionClass::Network,
        ),
        // `view_image`: the model looked at an image on disk. It reads like any file read.
        "imageView" | "image_view" => ToolCall::new(
            "view_image",
            json!({ "path": str_field(item, &["path"]) }),
            ActionClass::Read,
        ),
        "imageGeneration" | "image_generation" => {
            ToolCall::new("image_generation", Value::Null, ActionClass::Other)
        }
        "todoList" | "todo_list" => ToolCall::new(
            "todo",
            json!({ "items": item.get("items").cloned().unwrap_or_else(|| json!([])) }),
            ActionClass::Other,
        ),
        "collabAgentToolCall" | "collab_agent_tool_call" => ToolCall::new(
            snake_case(&str_field(item, &["tool"])),
            item.clone(),
            ActionClass::Other,
        ),
        // A subagent spawn/lifecycle marker on the PARENT thread (multi-agent v2): only its
        // start is a call. A marker for anything else has its own item id and is not another
        // spawn; the child's own turn notifications carry its terminal state.
        "subAgentActivity" | "sub_agent_activity" => {
            if !matches!(
                item.get("kind").and_then(Value::as_str),
                Some("started" | "spawned")
            ) {
                return None;
            }
            ToolCall::new("sub_agent", item.clone(), ActionClass::Other)
        }
        _ => return None,
    })
}

/// How a completed item went: whether it succeeded, and a one-line outcome.
fn outcome(item: &Value) -> (bool, Option<String>) {
    let status = str_field(item, &["status"]);
    match item_type(item) {
        "commandExecution" | "command_execution" => {
            // Absent when the command never ran (declined) or Codex lost it.
            let exit_code = field(item, &["exitCode", "exit_code"])
                .and_then(Value::as_i64)
                .and_then(|code| i32::try_from(code).ok());
            let output =
                field(item, &["aggregatedOutput", "aggregated_output"]).and_then(Value::as_str);
            (
                !failed(&status) && exit_code.is_none_or(|code| code == 0),
                summarize(output, exit_code),
            )
        }
        "mcpToolCall" | "mcp_tool_call" => {
            let error = item
                .pointer("/error/message")
                .and_then(Value::as_str)
                .map(|m| summarize(Some(m), None))
                .unwrap_or_default();
            (!failed(&status) && error.is_none(), error)
        }
        "dynamicToolCall" | "dynamic_tool_call" => (
            item.get("success")
                .and_then(Value::as_bool)
                .unwrap_or(!failed(&status)),
            None,
        ),
        "imageGeneration" | "image_generation" => {
            // `result` can be megabytes of inline media. Never inspect, clone, log, or forward
            // it: savedPath is the sole supported source.
            let path = str_field(item, &["savedPath", "saved_path"]);
            let failure = item.get("failure").filter(|v| !v.is_null());
            let error = if let Some(failure) = failure {
                Some(
                    if matches!(
                        failure.get("type").and_then(Value::as_str),
                        Some("usageLimitExceeded" | "usage_limit_exceeded")
                    ) {
                        "Image generation usage limit exceeded"
                    } else {
                        "Image generation failed"
                    },
                )
            } else if failed(&status) {
                Some("Image generation failed")
            } else if path.trim().is_empty() {
                Some("Image generation completed without a saved file")
            } else {
                None
            };
            match error {
                Some(message) => (false, Some(message.to_owned())),
                None => (true, Some(path)),
            }
        }
        "collabAgentToolCall" | "collab_agent_tool_call" => (!failed(&status), None),
        _ => (!failed(&status), None),
    }
}

/// How a settled turn ended, for the driver: usage to emit first, then the `Done`'s parts.
#[derive(Debug, PartialEq)]
pub(crate) struct TurnEnd {
    /// The last turn's token usage, emitted just before `Done`.
    pub usage: Option<ExecEvent>,
    pub stop: StopReason,
    /// The turn's final assistant message, for a completed turn.
    pub result: Option<String>,
    pub diagnostic: Option<String>,
}

impl TurnEnd {
    /// The `Done` this end stands for.
    pub(crate) fn done(self) -> ExecEvent {
        ExecEvent::Done {
            stop: self.stop,
            result: self.result,
            diagnostic: self.diagnostic,
            reason: None,
        }
    }
}

/// The per-run state of the mapping: what streamed, which calls are open, the paths of the file
/// changes in flight, the held error and usage of the turn.
#[derive(Default)]
pub(crate) struct Normalizer {
    /// Agent-message item ids whose text streamed as deltas, so a model that never streams
    /// (`item/completed` only) still emits its text exactly once.
    streamed: HashSet<String>,
    reasoning: ReasoningStream,
    /// Call ids with a `Tool` out and no `ToolResult` yet.
    open: HashSet<String>,
    /// Call ids already closed (an item can complete more than once on this wire).
    closed: VecDeque<String>,
    /// The paths of each file change in flight, by item id: `item/fileChange/requestApproval`
    /// names only its item.
    file_changes: HashMap<String, Vec<String>>,
    file_change_order: VecDeque<String>,
    /// The turn's last completed assistant message.
    last_message: Option<String>,
    usage: Option<ExecEvent>,
    /// The turn's last final `error` notification, reported by its end (see
    /// [`settle_turn_error`]): never as a notice AND the Done's diagnostic.
    held_error: Option<String>,
    /// The account's rate-limit windows, for a limit error's reset time.
    rate_limits: Option<Value>,
}

impl Normalizer {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// An `item/agentMessage/delta`.
    pub(crate) fn agent_delta(&mut self, params: &Value) -> Vec<ExecEvent> {
        self.streamed.insert(item_id(params));
        delta_text(params)
            .map(|text| ExecEvent::Text { text })
            .into_iter()
            .collect()
    }

    /// An `item/reasoning/*` notification of the run's own thread.
    pub(crate) fn reasoning(&mut self, method: &str, params: &Value) -> Vec<ExecEvent> {
        self.reasoning.map(method, params)
    }

    /// `thread/tokenUsage/updated`: held until the turn ends.
    pub(crate) fn token_usage(&mut self, params: &Value) {
        if let Some(usage) = usage_event(params) {
            self.usage = Some(usage);
        }
    }

    /// `account/rateLimits/updated`.
    pub(crate) fn rate_limits(&mut self, params: &Value) {
        merge_rate_limits(&mut self.rate_limits, params);
    }

    /// An `error` notification: a retry is progress, anything else is held for the turn's end.
    pub(crate) fn error(&mut self, params: &Value, now_unix: i64) {
        let notice = error_notice(params, self.rate_limits.as_ref(), now_unix);
        if notice.will_retry {
            tracing::info!(target: "reno_harness::codex", "codex retrying: {}", notice.message);
        } else {
            self.held_error = Some(notice.message);
        }
    }

    /// The final error the turn reported and never got to close, for a crash message to lead
    /// with: it is usually why the turn went nowhere.
    pub(crate) fn take_held_error(&mut self) -> Option<String> {
        self.held_error.take()
    }

    /// The paths a file change in flight carried, by its item id.
    pub(crate) fn file_paths(&self, item_id: &str) -> Option<&[String]> {
        self.file_changes.get(item_id).map(Vec::as_slice)
    }

    fn remember_change(&mut self, id: &str, item: &Value) {
        let paths: Vec<String> = changes(item).into_iter().map(|(path, _)| path).collect();
        if paths.is_empty() || id.is_empty() {
            return;
        }
        if self.file_changes.insert(id.to_owned(), paths).is_none() {
            self.file_change_order.push_back(id.to_owned());
            while self.file_change_order.len() > REMEMBERED {
                if let Some(oldest) = self.file_change_order.pop_front() {
                    self.file_changes.remove(&oldest);
                }
            }
        }
    }

    fn forget_change(&mut self, id: &str) {
        if self.file_changes.remove(id).is_some() {
            self.file_change_order.retain(|queued| queued != id);
        }
    }

    /// Map one `item/started` or `item/completed` payload's item to events.
    pub(crate) fn item(&mut self, phase: Phase, item: &Value) -> Vec<ExecEvent> {
        let id = str_field(item, &["id"]);
        if matches!(item_type(item), "agentMessage" | "agent_message") {
            return if phase == Phase::Completed {
                self.agent_message(&id, item)
            } else {
                Vec::new()
            };
        }
        if matches!(item_type(item), "fileChange" | "file_change") {
            match phase {
                Phase::Started => self.remember_change(&id, item),
                Phase::Completed => self.forget_change(&id),
            }
        }
        if id.is_empty() {
            return Vec::new();
        }
        let Some(tool) = call(item) else {
            // Not a tool: a reasoning or user-message item (their text flows through deltas),
            // or an item type this version does not know.
            if item_type(item) == "error" {
                let message = str_field(item, &["message"]);
                if !message.is_empty() {
                    self.held_error = Some(message);
                }
            }
            return Vec::new();
        };
        let mut events = Vec::new();
        match phase {
            Phase::Started => {
                if self.closed.contains(&id) || !self.open.insert(id.clone()) {
                    return Vec::new();
                }
                events.push(ExecEvent::Tool { id, call: tool });
            }
            Phase::Completed => {
                if self.closed.contains(&id) {
                    return Vec::new();
                }
                // A call that only ever completes opens and closes together.
                if !self.open.remove(&id) {
                    events.push(ExecEvent::Tool {
                        id: id.clone(),
                        call: tool,
                    });
                }
                let (ok, summary) = outcome(item);
                self.closed.push_back(id.clone());
                while self.closed.len() > REMEMBERED {
                    self.closed.pop_front();
                }
                events.push(ExecEvent::ToolResult { id, ok, summary });
            }
        }
        events
    }

    /// A completed assistant message: its text if it never streamed, then a paragraph break.
    /// Codex emits several assistant messages per turn (commentary, final answer); their deltas
    /// carry no separator, so consecutive messages would render concatenated.
    fn agent_message(&mut self, id: &str, item: &Value) -> Vec<ExecEvent> {
        let text = str_field(item, &["text"]);
        let mut events = Vec::new();
        if !self.streamed.contains(id) && !text.is_empty() {
            events.push(ExecEvent::Text { text: text.clone() });
        }
        events.push(ExecEvent::Text {
            text: "\n\n".into(),
        });
        if !text.trim().is_empty() {
            self.last_message = Some(text);
        }
        events
    }

    /// A turn settled: `turn/completed`, `turn/failed` or `turn/aborted`. `interrupted` is
    /// whether this run asked for the stop. Resets what belongs to the turn.
    pub(crate) fn finish_turn(
        &mut self,
        method: &str,
        params: &Value,
        interrupted: bool,
        now_unix: i64,
    ) -> TurnEnd {
        self.streamed.clear();
        self.open.clear();
        self.file_changes.clear();
        self.file_change_order.clear();
        let usage = self.usage.take();
        let last = self.last_message.take();
        let held = self.held_error.take();
        let limits = self.rate_limits.as_ref();
        let end = |stop, result, diagnostic| TurnEnd {
            usage: usage.clone(),
            stop,
            result,
            diagnostic,
        };
        let by_someone_else =
            params.pointer("/turn/status").and_then(Value::as_str) == Some("interrupted");
        match method {
            "turn/failed" if interrupted => end(StopReason::Interrupted, None, None),
            "turn/failed" => end(
                StopReason::Error,
                None,
                Some(
                    turn_error_message(params, limits, now_unix)
                        .or(held)
                        .unwrap_or_else(|| "Codex turn failed".into()),
                ),
            ),
            "turn/aborted" => end(StopReason::Interrupted, None, None),
            _ => {
                let (error, notice) = settle_turn_error(params, held, limits, now_unix);
                if interrupted || (error.is_none() && by_someone_else) {
                    // Stopped by us, or by someone else (another client of the thread, Codex
                    // itself): still not a finished answer.
                    end(StopReason::Interrupted, None, None)
                } else if error.is_some() {
                    end(StopReason::Error, None, error)
                } else {
                    // A turn that did not fail but reported a final error: worth a line, not a
                    // diagnostic (which is the detail of a failure).
                    if let Some(notice) = notice {
                        tracing::warn!(
                            target: "reno_harness::codex",
                            "turn completed after an error notice: {notice}"
                        );
                    }
                    end(
                        StopReason::Completed,
                        last.map(|m| m.trim().to_owned()),
                        None,
                    )
                }
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn reasoning_text(events: Vec<ExecEvent>) -> String {
        events
            .into_iter()
            .filter_map(|e| match e {
                ExecEvent::Reasoning { text } => Some(text),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn reasoning_parts_preserve_chunking_and_existing_paragraph_breaks() {
        let mut stream = ReasoningStream::default();
        let mut text = String::new();
        for params in [
            json!({"itemId":"r1", "contentIndex":0, "textDelta":"**First"}),
            json!({"itemId":"r1", "contentIndex":0, "textDelta":" heading**\n"}),
            json!({"itemId":"r1", "contentIndex":1, "delta":"\nSecond paragraph.\n\n"}),
            // An empty delta must not consume the next item's boundary.
            json!({"itemId":"r2", "contentIndex":0, "delta":""}),
            json!({"item_id":"r2", "content_index":0, "delta":"Third paragraph."}),
        ] {
            text.push_str(&reasoning_text(
                stream.map("item/reasoning/textDelta", &params),
            ));
        }
        assert_eq!(
            text,
            "**First heading**\n\nSecond paragraph.\n\nThird paragraph."
        );
    }

    #[test]
    fn delta_accepts_both_spellings() {
        assert_eq!(delta_text(&json!({"delta": "a"})), Some("a".into()));
        assert_eq!(delta_text(&json!({"textDelta": "b"})), Some("b".into()));
        assert_eq!(delta_text(&json!({"delta": ""})), None);
        assert_eq!(delta_text(&json!({})), None);
    }

    fn started(item: Value) -> Vec<ExecEvent> {
        Normalizer::new().item(Phase::Started, &item)
    }

    #[test]
    fn every_item_kind_has_its_tool_name_and_class() {
        let cases = [
            (
                json!({"id": "1", "type": "commandExecution", "command": "ls", "cwd": "/w"}),
                "exec",
                ActionClass::Command,
                json!({"command": "ls", "cwd": "/w"}),
            ),
            (
                json!({"id": "1", "type": "command_execution", "command": "ls"}),
                "exec",
                ActionClass::Command,
                json!({"command": "ls"}),
            ),
            (
                json!({"id": "1", "type": "fileChange", "changes": [
                    {"path": "/a", "kind": "add"}, {"path": "/b", "kind": "strange"}, {"kind": "add"}]}),
                "apply_patch",
                ActionClass::Edit,
                json!({"changes": [{"path": "/a", "kind": "add"}, {"path": "/b", "kind": "update"}]}),
            ),
            // The 0.169 wire: a change's `kind` is an object tagged by `type`.
            (
                json!({"id": "1", "type": "fileChange", "changes": [
                    {"path": "/a", "diff": "", "kind": {"type": "add"}},
                    {"path": "/b", "diff": "", "kind": {"type": "delete"}},
                    {"path": "/c", "diff": "", "kind": {"type": "update", "move_path": null}},
                    {"path": "/d", "diff": "", "kind": {"type": "strange"}}]}),
                "apply_patch",
                ActionClass::Edit,
                json!({"changes": [
                    {"path": "/a", "kind": "add"}, {"path": "/b", "kind": "delete"},
                    {"path": "/c", "kind": "update"}, {"path": "/d", "kind": "update"}]}),
            ),
            (
                json!({"id": "1", "type": "mcpToolCall", "server": "linear", "tool": "search", "arguments": {"q": 1}}),
                "mcp__linear__search",
                ActionClass::McpTool,
                json!({"q": 1}),
            ),
            (
                json!({"id": "1", "type": "webSearch", "query": "rust"}),
                "web_search",
                ActionClass::Network,
                json!({"query": "rust"}),
            ),
            (
                json!({"id": "1", "type": "imageView", "path": "/i.png"}),
                "view_image",
                ActionClass::Read,
                json!({"path": "/i.png"}),
            ),
            (
                json!({"id": "1", "type": "dynamicToolCall", "namespace": "app", "tool": "go", "arguments": {"x": 1}}),
                "app__go",
                ActionClass::Other,
                json!({"x": 1}),
            ),
            (
                json!({"id": "1", "type": "imageGeneration"}),
                "image_generation",
                ActionClass::Other,
                Value::Null,
            ),
            (
                json!({"id": "1", "type": "todoList", "items": [{"text": "a", "completed": true}]}),
                "todo",
                ActionClass::Other,
                json!({"items": [{"text": "a", "completed": true}]}),
            ),
        ];
        for (item, name, class, input) in cases {
            assert_eq!(
                started(item.clone()),
                vec![ExecEvent::Tool {
                    id: "1".into(),
                    call: ToolCall::new(name, input, class)
                }],
                "{item}"
            );
        }
    }

    #[test]
    fn collab_tools_and_subagent_markers_are_calls_named_for_their_role() {
        let spawn = json!({"id": "s", "type": "collabAgentToolCall", "tool": "spawnAgent", "status": "inProgress", "receiverThreadIds": []});
        let events = started(spawn.clone());
        let [ExecEvent::Tool { call, .. }] = events.as_slice() else {
            panic!("one call");
        };
        assert_eq!(call.name, "spawn_agent");
        assert_eq!(call.class, ActionClass::Other);
        assert_eq!(call.input, spawn);
        for (tool, name) in [
            ("sendInput", "send_input"),
            ("wait", "wait"),
            ("closeAgent", "close_agent"),
            ("resumeAgent", "resume_agent"),
        ] {
            let events = started(json!({"id": "c", "type": "collabAgentToolCall", "tool": tool}));
            let [ExecEvent::Tool { call, .. }] = events.as_slice() else {
                panic!("one call");
            };
            assert_eq!(call.name, name);
        }
        // Only a start is a spawn: a lifecycle marker with its own id is no call at all.
        for kind in ["started", "spawned"] {
            assert_eq!(
                started(json!({"id": "a", "type": "subAgentActivity", "kind": kind, "agentPath": "/root/alpha"})).len(),
                1
            );
        }
        for kind in ["interacted", "completed", "failed"] {
            assert!(
                started(json!({"id": "a", "type": "subAgentActivity", "kind": kind})).is_empty(),
                "{kind}"
            );
        }
    }

    #[test]
    fn a_call_opens_once_and_closes_once() {
        let mut n = Normalizer::new();
        let command = |status: &str, exit: Option<i64>| {
            json!({"id": "c1", "type": "commandExecution", "command": "ls", "status": status,
                   "exitCode": exit, "aggregatedOutput": "one\ntwo\n"})
        };
        assert_eq!(
            n.item(Phase::Started, &command("inProgress", None)).len(),
            1
        );
        // A repeated start opens nothing.
        assert!(
            n.item(Phase::Started, &command("inProgress", None))
                .is_empty()
        );
        assert_eq!(
            n.item(Phase::Completed, &command("completed", Some(1))),
            vec![ExecEvent::ToolResult {
                id: "c1".into(),
                ok: false,
                summary: Some("exit code 1: two".into()),
            }]
        );
        // A repeated completion closes nothing, and neither reopens it.
        assert!(
            n.item(Phase::Completed, &command("completed", Some(1)))
                .is_empty()
        );
        assert!(
            n.item(Phase::Started, &command("inProgress", None))
                .is_empty()
        );
        // A call that only ever completes opens and closes together.
        let events = n.item(
            Phase::Completed,
            &json!({"id": "w", "type": "webSearch", "query": "q"}),
        );
        assert!(matches!(
            events.as_slice(),
            [
                ExecEvent::Tool { .. },
                ExecEvent::ToolResult { ok: true, .. }
            ]
        ));
    }

    #[test]
    fn outcomes_follow_the_items_own_status() {
        let ok = |item: Value| {
            let mut n = Normalizer::new();
            match n.item(Phase::Completed, &item).last() {
                Some(ExecEvent::ToolResult { ok, .. }) => *ok,
                other => panic!("{other:?}"),
            }
        };
        let command = |status, exit: Option<i64>| json!({"id": "c", "type": "commandExecution", "command": "x", "status": status, "exitCode": exit});
        assert!(ok(command("completed", Some(0))));
        assert!(!ok(command("completed", Some(2))));
        assert!(!ok(command("failed", None)));
        // Declined: it never ran.
        assert!(!ok(command("declined", None)));
        assert!(ok(command("completed", None)));
        assert!(!ok(
            json!({"id": "f", "type": "fileChange", "status": "failed", "changes": []})
        ));
        assert!(!ok(
            json!({"id": "f", "type": "fileChange", "status": "declined", "changes": []})
        ));
        assert!(ok(
            json!({"id": "f", "type": "fileChange", "status": "completed", "changes": []})
        ));
        assert!(!ok(
            json!({"id": "m", "type": "mcpToolCall", "server": "s", "tool": "t", "status": "failed"})
        ));
        assert!(!ok(
            json!({"id": "m", "type": "mcpToolCall", "server": "s", "tool": "t", "status": "completed", "error": {"message": "boom"}})
        ));
        assert!(ok(
            json!({"id": "d", "type": "dynamicToolCall", "tool": "t", "status": "completed", "success": true})
        ));
        assert!(!ok(
            json!({"id": "d", "type": "dynamicToolCall", "tool": "t", "status": "completed", "success": false})
        ));
        assert!(!ok(
            json!({"id": "a", "type": "collabAgentToolCall", "tool": "wait", "status": "failed"})
        ));
    }

    #[test]
    fn image_generation_never_touches_the_inline_result() {
        let done = |item: Value| {
            let mut n = Normalizer::new();
            n.item(Phase::Completed, &item)
        };
        let saved = done(
            json!({"id": "i", "type": "imageGeneration", "status": "completed",
            "result": "INLINE_IMAGE_SENTINEL", "savedPath": "/codex/goblin.png"}),
        );
        assert!(!format!("{saved:?}").contains("INLINE_IMAGE_SENTINEL"));
        assert!(matches!(
            saved.last(),
            Some(ExecEvent::ToolResult { ok: true, summary: Some(s), .. }) if s == "/codex/goblin.png"
        ));
        for (item, message) in [
            (
                json!({"id": "i", "type": "imageGeneration", "status": "failed", "failure": {"type": "usageLimitExceeded"}, "result": "X"}),
                "Image generation usage limit exceeded",
            ),
            (
                json!({"id": "i", "type": "imageGeneration", "status": "cancelled"}),
                "Image generation failed",
            ),
            (
                json!({"id": "i", "type": "imageGeneration", "status": "completed", "result": "X"}),
                "Image generation completed without a saved file",
            ),
        ] {
            assert!(matches!(
                done(item).last(),
                Some(ExecEvent::ToolResult { ok: false, summary: Some(s), .. }) if s == message
            ));
        }
    }

    #[test]
    fn a_summary_is_one_short_redacted_line() {
        assert_eq!(
            summarize(Some("a\n\nlast line\n  \n"), Some(0)),
            Some("exit code 0: last line".into())
        );
        assert_eq!(summarize(None, Some(3)), Some("exit code 3".into()));
        assert_eq!(summarize(Some("only"), None), Some("only".into()));
        assert_eq!(summarize(Some("  \n"), None), None);
        let long = "x".repeat(SUMMARY_CHARS + 50);
        assert_eq!(
            summarize(Some(&long), None).unwrap().chars().count(),
            SUMMARY_CHARS + 1
        );
        assert_eq!(
            summarize(Some("Authorization: Bearer abc123"), None),
            Some("Authorization: Bearer [REDACTED]".into())
        );
    }

    #[test]
    fn file_change_paths_are_kept_while_the_item_is_in_flight() {
        let mut n = Normalizer::new();
        let item = json!({"id": "f1", "type": "fileChange", "changes": [{"path": "/a", "kind": "add"}, {"path": "b", "kind": "update"}]});
        assert_eq!(n.file_paths("f1"), None);
        n.item(Phase::Started, &item);
        assert_eq!(
            n.file_paths("f1"),
            Some(&["/a".to_owned(), "b".to_owned()][..])
        );
        assert_eq!(n.file_paths("other"), None);
        n.item(Phase::Completed, &item);
        assert_eq!(n.file_paths("f1"), None);
        // Bounded: the oldest are forgotten first.
        for at in 0..REMEMBERED + 5 {
            n.item(
                Phase::Started,
                &json!({"id": format!("f{at}"), "type": "fileChange", "changes": [{"path": "/x", "kind": "add"}]}),
            );
        }
        assert_eq!(n.file_paths("f0"), None);
        assert!(n.file_paths(&format!("f{}", REMEMBERED + 4)).is_some());
    }

    #[test]
    fn agent_messages_close_as_paragraphs_and_never_repeat_streamed_text() {
        let mut n = Normalizer::new();
        let text = |t: &str| ExecEvent::Text { text: t.into() };
        assert_eq!(
            n.agent_delta(&json!({"itemId": "m1", "delta": "Hel"})),
            vec![text("Hel")]
        );
        assert_eq!(
            n.item(
                Phase::Completed,
                &json!({"id": "m1", "type": "agentMessage", "text": "Hello"})
            ),
            vec![text("\n\n")]
        );
        assert_eq!(
            n.item(
                Phase::Completed,
                &json!({"id": "m2", "type": "agentMessage", "text": "Whole"})
            ),
            vec![text("Whole"), text("\n\n")]
        );
        assert!(
            n.item(
                Phase::Started,
                &json!({"id": "m3", "type": "agentMessage", "text": ""})
            )
            .is_empty()
        );
        let end = n.finish_turn("turn/completed", &json!({"turn": {"id": "t"}}), false, 0);
        assert_eq!(end.result.as_deref(), Some("Whole"));
    }

    #[test]
    fn cached_input_is_a_subset_of_codex_input_and_is_taken_out_of_it() {
        // The 0.169 `TokenUsageBreakdown`: `inputTokens` already holds the cached and the
        // cache-written parts, and `totalTokens` is input plus output.
        let params = json!({"threadId": "th", "turnId": "t", "tokenUsage": {
            "total": {"totalTokens": 99999, "inputTokens": 90000, "cachedInputTokens": 80000, "cacheWriteInputTokens": 0, "outputTokens": 9999, "reasoningOutputTokens": 100},
            "last": {"totalTokens": 11000, "inputTokens": 10000, "cachedInputTokens": 7000, "cacheWriteInputTokens": 500, "outputTokens": 1000, "reasoningOutputTokens": 50},
            "modelContextWindow": 272000}});
        assert_eq!(
            usage_event(&params),
            Some(ExecEvent::Usage {
                input_tokens: 2500,
                output_tokens: 1000,
                cache_read_tokens: 7000,
                cache_write_tokens: 500,
            })
        );
        // A report whose cached count exceeds its input (never expected) cannot go negative.
        let odd = json!({"tokenUsage": {"last": {"inputTokens": 5, "cachedInputTokens": 9, "outputTokens": 1}}});
        assert!(matches!(
            usage_event(&odd),
            Some(ExecEvent::Usage {
                input_tokens: 0,
                cache_read_tokens: 9,
                ..
            })
        ));
    }

    #[test]
    fn usage_reads_last_snapshot_under_both_spellings() {
        assert_eq!(
            usage_event(&json!({"tokenUsage": {"last": {"inputTokens": 42, "outputTokens": 7}}})),
            Some(ExecEvent::Usage {
                input_tokens: 42,
                output_tokens: 7,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            })
        );
        assert_eq!(
            usage_event(&json!({"token_usage": {"last": {"input_tokens": 1, "output_tokens": 2}}})),
            Some(ExecEvent::Usage {
                input_tokens: 1,
                output_tokens: 2,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            })
        );
        assert_eq!(usage_event(&json!({})), None);
    }

    #[test]
    fn child_routing_drops_a_childs_traffic_and_fails_open_to_the_run() {
        for m in [
            "item/started",
            "item/completed",
            "item/agentMessage/delta",
            "item/reasoning/textDelta",
            "error",
            "thread/closed",
            // A child's turn ends must never settle the run's turn.
            "turn/completed",
            "turn/aborted",
            "turn/failed",
            "turn/started",
            // Child-owned lifecycle would rewrite the run's state.
            "thread/archived",
            "thread/compacted",
            "thread/started",
            "thread/status/changed",
            "thread/tokenUsage/updated",
        ] {
            assert_eq!(route_child_notification(m), ChildRoute::Dropped, "{m}");
        }
        // Unknown methods degrade to "the run sees it", never silent loss.
        for m in [
            "serverRequest/resolved",
            "thread/somethingBrandNew",
            "account/rateLimits/updated",
        ] {
            assert_eq!(route_child_notification(m), ChildRoute::Parent, "{m}");
        }
    }

    #[test]
    fn notification_thread_ids_read_both_shapes() {
        assert_eq!(
            notification_thread_id("thread/started", &json!({"thread": {"id": "th-c"}})),
            Some("th-c".into())
        );
        assert_eq!(
            notification_thread_id(
                "turn/completed",
                &json!({"threadId": "th-1", "turn": {"id": "t"}})
            ),
            Some("th-1".into())
        );
        assert_eq!(
            notification_thread_id("error", &json!({"message": "x"})),
            None
        );
    }

    #[test]
    fn turn_error_extraction() {
        assert_eq!(
            turn_error_message(
                &json!({"turn": {"id": "t", "error": {"message": "boom"}}}),
                None,
                0
            ),
            Some("boom".into())
        );
        assert_eq!(
            turn_error_message(&json!({"turn": {"id": "t"}}), None, 0),
            None
        );
        assert_eq!(
            turn_error_message(&json!({"turn": {"error": null}}), None, 0),
            None
        );
        // The longer explanation wins over the generic message.
        assert_eq!(
            turn_error_message(
                &json!({"turn": {"error": {"message": "stream error", "additionalDetails": "  upstream 502 from the model  "}}}),
                None,
                0
            ),
            Some("upstream 502 from the model".into())
        );
    }

    #[test]
    fn structured_error_info_decides_sign_in_and_limit_text() {
        let signed_out = codex_error_text(
            &json!({"message": "401 Unauthorized: token_expired", "codexErrorInfo": "unauthorized"}),
            None,
            0,
        );
        assert_eq!(signed_out, SIGNED_OUT_MESSAGE);
        // Live 0.164.0 with no login at all: the 401 rides a transport variant.
        assert_eq!(
            codex_error_text(
                &json!({"message": "unexpected status 401 Unauthorized: Missing bearer or basic authentication in header", "codexErrorInfo": {"httpConnectionFailed": {"httpStatusCode": 401}}, "additionalDetails": null}),
                None,
                0
            ),
            SIGNED_OUT_MESSAGE
        );
        // A 401 is not always a ChatGPT login: API-key users are told too.
        assert!(signed_out.contains("API key"), "{signed_out}");

        let now: i64 = 1_800_000_000;
        let limits = json!({
            "primary": {"usedPercent": 100, "resetsAt": now + 2 * 3600 + 14 * 60 - 5},
            "secondary": {"usedPercent": 40, "resetsAt": now + 6 * 86400},
        });
        assert_eq!(
            codex_error_text(
                &json!({"message": "You've hit your usage limit.", "codexErrorInfo": "usageLimitExceeded"}),
                Some(&limits),
                now,
            ),
            "Codex usage limit reached — it resets in 2h 14m."
        );
        // No exhausted window known: keep Codex's own explanation.
        assert_eq!(
            codex_error_text(
                &json!({"message": "Slow down", "codexErrorInfo": "rateLimitExceeded"}),
                None,
                0
            ),
            "Codex rate limit reached. Slow down"
        );
        // Retries exhausted on a 429 are a rate limit too.
        assert!(
            codex_error_text(
                &json!({"message": "exceeded retry limit", "codexErrorInfo": {"responseTooManyFailedAttempts": {"httpStatusCode": 429}}}),
                None, 0
            )
            .starts_with("Codex rate limit reached")
        );
        // Prose alone never classifies: a 500 that mentions limits stays as is.
        assert_eq!(
            codex_error_text(
                &json!({"message": "internal error near rate limit middleware", "codexErrorInfo": {"responseTooManyFailedAttempts": {"httpStatusCode": 500}}}),
                None,
                0
            ),
            "internal error near rate limit middleware"
        );
    }

    #[test]
    fn rate_limit_updates_merge_sparsely() {
        let mut held = None;
        merge_rate_limits(
            &mut held,
            &json!({"rateLimits": {"primary": {"usedPercent": 10, "resetsAt": 5}, "secondary": {"usedPercent": 1}}}),
        );
        merge_rate_limits(
            &mut held,
            &json!({"rateLimits": {"primary": {"usedPercent": 100, "resetsAt": 7}, "secondary": null}}),
        );
        let held = held.unwrap();
        assert_eq!(held["primary"]["usedPercent"], 100);
        assert_eq!(held["secondary"]["usedPercent"], 1);
        assert_eq!(limit_reset_in(&held, 0), Some(7));
        assert_eq!(describe_wait(0), "under a minute");
        assert_eq!(describe_wait(3600), "1h");
        assert_eq!(describe_wait(3 * 86400 + 5 * 3600), "3d 5h");
    }

    #[test]
    fn error_notices_separate_retries_from_final_errors() {
        let retry = error_notice(
            &json!({"error": {"message": "Reconnecting... 2/5 (rate limit)"}, "willRetry": true, "threadId": "t", "turnId": "u"}),
            None,
            0,
        );
        assert!(retry.will_retry);
        let last = error_notice(&json!({"message": "flat legacy error"}), None, 0);
        assert!(!last.will_retry);
        assert_eq!(last.message, "flat legacy error");

        // turn.error wins and the held notice is dropped (never both).
        let failed = json!({"turn": {"id": "t", "status": "failed", "error": {"message": "boom"}}});
        assert_eq!(
            settle_turn_error(&failed, Some("held".into()), None, 0),
            (Some("boom".into()), None)
        );
        // A failed turn without its own error takes the held one.
        let bare = json!({"turn": {"id": "t", "status": "failed"}});
        assert_eq!(
            settle_turn_error(&bare, Some("held".into()), None, 0),
            (Some("held".into()), None)
        );
        assert_eq!(
            settle_turn_error(&bare, None, None, 0),
            (Some("Codex turn failed".into()), None)
        );
        // A turn that did not fail keeps it as a notice.
        let done = json!({"turn": {"id": "t", "status": "completed"}});
        assert_eq!(
            settle_turn_error(&done, Some("held".into()), None, 0),
            (None, Some("held".into()))
        );
    }

    #[test]
    fn a_turns_end_becomes_one_done_with_the_usage_before_it() {
        let usage = ExecEvent::Usage {
            input_tokens: 3,
            output_tokens: 4,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
        };
        let usage_params = json!({"tokenUsage": {"last": {"inputTokens": 3, "outputTokens": 4}}});
        let turn = |status: &str| json!({"turn": {"id": "t", "status": status}});

        // Completed: the last message is the result, and retries never mattered.
        let mut n = Normalizer::new();
        n.token_usage(&usage_params);
        n.item(
            Phase::Completed,
            &json!({"id": "a", "type": "agentMessage", "text": "  answer \n"}),
        );
        n.error(
            &json!({"error": {"message": "Reconnecting"}, "willRetry": true}),
            0,
        );
        assert_eq!(
            n.finish_turn("turn/completed", &turn("completed"), false, 0),
            TurnEnd {
                usage: Some(usage.clone()),
                stop: StopReason::Completed,
                result: Some("answer".into()),
                diagnostic: None,
            }
        );
        // State resets: the next turn starts clean.
        let next = n.finish_turn("turn/completed", &turn("completed"), false, 0);
        assert_eq!((next.usage, next.result), (None, None));

        // A final error with a turn that did not fail is logged: a Completed Done carries no
        // failure detail.
        let mut n = Normalizer::new();
        n.error(
            &json!({"error": {"message": "late warning"}, "willRetry": false}),
            0,
        );
        let end = n.finish_turn("turn/completed", &turn("completed"), false, 0);
        assert_eq!(
            (end.stop, end.diagnostic.as_deref()),
            (StopReason::Completed, None)
        );

        // A failed turn is an error with the turn's own text, else the held one.
        let mut n = Normalizer::new();
        n.error(
            &json!({"error": {"message": "held"}, "willRetry": false}),
            0,
        );
        let end = n.finish_turn("turn/failed", &json!({"turn": {"id": "t"}}), false, 0);
        assert_eq!(
            (end.stop, end.diagnostic.as_deref()),
            (StopReason::Error, Some("held"))
        );
        let end = Normalizer::new().finish_turn(
            "turn/failed",
            &json!({"turn": {"error": {"message": "boom"}}}),
            false,
            0,
        );
        assert_eq!(end.diagnostic.as_deref(), Some("boom"));
        let end = Normalizer::new().finish_turn(
            "turn/completed",
            &json!({"turn": {"status": "failed", "error": {"message": "x"}}}),
            false,
            0,
        );
        assert_eq!(
            (end.stop, end.diagnostic.as_deref()),
            (StopReason::Error, Some("x"))
        );

        // Interrupted by us, aborted, or interrupted by someone else: never a finished answer.
        for (method, params, ours) in [
            ("turn/completed", turn("completed"), true),
            (
                "turn/failed",
                json!({"turn": {"error": {"message": "x"}}}),
                true,
            ),
            ("turn/aborted", json!({"turn": {}}), false),
            ("turn/completed", turn("interrupted"), false),
        ] {
            let end = Normalizer::new().finish_turn(method, &params, ours, 0);
            assert_eq!(
                (end.stop, end.result, end.diagnostic),
                (StopReason::Interrupted, None, None),
                "{method}"
            );
        }
        assert_eq!(
            ExecEvent::Done {
                stop: StopReason::Completed,
                result: None,
                diagnostic: None,
                reason: None,
            },
            TurnEnd {
                usage: None,
                stop: StopReason::Completed,
                result: None,
                diagnostic: None
            }
            .done()
        );
    }

    #[test]
    fn a_crash_leads_with_the_final_error_the_turn_never_closed() {
        let mut n = Normalizer::new();
        n.error(&json!({"error": {"message": "unexpected status 401 Unauthorized", "codexErrorInfo": {"httpConnectionFailed": {"httpStatusCode": 401}}}, "willRetry": false}), 0);
        assert_eq!(n.take_held_error().as_deref(), Some(SIGNED_OUT_MESSAGE));
        assert_eq!(n.take_held_error(), None);
        // An `error` item is held the same way.
        n.item(
            Phase::Completed,
            &json!({"id": "e", "type": "error", "message": "item error"}),
        );
        assert_eq!(n.take_held_error().as_deref(), Some("item error"));
    }
}
