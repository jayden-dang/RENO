//! Frame → [`AgentEvent`] normalization (init dedupe, subagent tagging, tool
//! decoding, error-code mapping).

use crate::upstream_proto::{AgentEvent, DoneStatus, HarnessId, TodoItem, TodoStatus, ToolCall};
use serde_json::Value;

use super::wire::{ContentBlock, Frame, MessageFrame, ResultFrame};

/// Human-readable text for the CLI's assistant-level error codes. These arrive
/// as a terse `error` field on an `assistant` frame — usually with NO text
/// content and NOT as a `result` error — so a usage-limited or otherwise failed
/// turn looks like the agent simply never replied unless we surface it.
fn assistant_error_text(code: &str) -> String {
    match code {
        "authentication_failed" => "Authentication failed — sign in to Claude again.".into(),
        "oauth_org_not_allowed" => "This organization isn't allowed to use Claude here.".into(),
        "billing_error" => "Billing error — check your Claude plan or payment method.".into(),
        "rate_limit" => "Claude usage limit reached — try again after the limit resets.".into(),
        "overloaded" => "Claude is overloaded right now — try again shortly.".into(),
        "invalid_request" => "The request was rejected as invalid.".into(),
        "model_not_found" => "The selected model isn't available.".into(),
        "server_error" => "Claude had a server error — try again.".into(),
        "max_output_tokens" => "The reply hit the maximum output length.".into(),
        "unknown" => "Claude returned an unspecified error.".into(),
        other => format!("Claude error: {other}"),
    }
}

/// Which claude.ai usage window a `rate_limit_event` refers to.
fn rate_window_label(kind: &str) -> &'static str {
    match kind {
        "five_hour" => "5-hour",
        "seven_day" | "seven_day_overage_included" => "weekly",
        "seven_day_opus" => "weekly (Opus)",
        "seven_day_sonnet" => "weekly (Sonnet)",
        "overage" => "overage",
        _ => "usage",
    }
}

/// Fallback wording for a `result` error whose `errors` array is empty, so the
/// turn never ends with a blank (and therefore invisible) error.
fn result_error_text(subtype: &str) -> &'static str {
    match subtype {
        "error_max_turns" => "The run hit the maximum number of turns.",
        "error_max_budget_usd" => "The run hit its cost budget.",
        "error_max_structured_output_retries" => "The run exhausted its structured-output retries.",
        _ => "The run ended with an error.",
    }
}

/// The CLI seeds `result.errors` with internal `[ede_diagnostic]` breadcrumbs
/// for its error_during_execution telemetry ("turn aborted (…) stop_reason=…",
/// "result_type=… last_content_type=… stop_reason=…"). They're diagnostics
/// about the CLI's own turn accounting, not user-relevant errors — surfacing
/// them verbatim put raw `[ede_diagnostic] result_type=user …` boxes in the
/// transcript. They're debug-logged and dropped instead.
fn is_internal_diagnostic(message: &str) -> bool {
    message.contains("[ede_diagnostic]")
}

const CONTEXT_FULL: &str =
    "The conversation no longer fits the model's context window. Run /compact or start a new chat.";

/// Wording for the `terminal_reason`s that mean the turn failed. Others
/// (`completed`, the aborts, anything newer) are not failures by themselves.
fn terminal_reason_text(reason: &str) -> Option<&'static str> {
    Some(match reason {
        "api_error" => "Claude gave up after repeated API errors.",
        "prompt_too_long" => CONTEXT_FULL,
        // Also how a full context window ends; a rate limit says 429.
        "blocking_limit" => CONTEXT_FULL,
        "malformed_tool_use_exhausted" => "Claude gave up after repeated malformed tool calls.",
        "budget_exhausted" => "Claude stopped: the turn's token budget ran out.",
        "structured_output_retry_exhausted" => {
            "Claude could not produce the requested structured output."
        }
        "tool_deferred_unavailable" => {
            "Claude could not resume a deferred tool call: the tool is no longer available."
        }
        "turn_setup_failed" => "Claude could not start the turn.",
        "rapid_refill_breaker" => {
            "Claude stopped: the context refilled too quickly after compaction."
        }
        "image_error" => "Claude stopped: an image in the conversation could not be processed.",
        "model_error" => "Claude stopped: the model returned an error.",
        _ => return None,
    })
}

/// An abort the CLI files as an ordinary end: a stop, or a `now` steer.
fn aborted(f: &ResultFrame) -> bool {
    matches!(
        f.terminal_reason.as_deref(),
        Some("aborted_streaming" | "aborted_tools")
    )
}

/// A `success` result that is really a failed API call: an invalid key, a
/// rate limit, an overloaded API, a full context window. The CLI's own
/// `result` text says what happened ("Invalid API key · Fix external API
/// key"); a status we recognize gets plainer wording in front of it.
fn api_failure(f: &ResultFrame) -> Option<String> {
    let reason = f.terminal_reason.as_deref().unwrap_or("");
    let status = f.api_error_status;
    if !f.is_error && !matches!(status, Some(429 | 529)) && terminal_reason_text(reason).is_none() {
        return None;
    }
    // The CLI's text carries the specifics (a reset time, the API's own
    // message). Bounded: some are a raw API error body.
    let detail = f
        .result
        .as_deref()
        .map(str::trim)
        .filter(|r| !r.is_empty())
        .map(|r| {
            let mut r = r.to_owned();
            if r.len() > 500 {
                let mut end = 500;
                while !r.is_char_boundary(end) {
                    end -= 1;
                }
                r.truncate(end);
                r.push('…');
            }
            r
        });
    let too_long = detail
        .as_deref()
        .is_some_and(|d| d.to_ascii_lowercase().contains("prompt is too long"));
    let headline = match status {
        Some(429) => Some("Claude rate limit reached — try again after it resets."),
        Some(529) => Some("Claude is overloaded right now — try again shortly."),
        _ if too_long => Some(CONTEXT_FULL),
        _ => terminal_reason_text(reason).filter(|_| reason != "api_error" || detail.is_none()),
    };
    Some(match (headline, detail) {
        (Some(headline), Some(detail)) if headline != CONTEXT_FULL => {
            format!("{headline} {detail}")
        }
        (Some(headline), _) => headline.to_owned(),
        (None, Some(detail)) => detail,
        (None, None) => "Claude's API request failed.".to_owned(),
    })
}

fn str_field(input: &Value, key: &str) -> String {
    input.get(key).and_then(Value::as_str).unwrap_or("").into()
}

fn opt_str_field(input: &Value, key: &str) -> Option<String> {
    input.get(key).and_then(Value::as_str).map(str::to_owned)
}

/// Decode a Claude `tool_use` block (name + input) into a typed [`ToolCall`].
pub(crate) fn decode_tool_use(name: &str, input: &Value) -> ToolCall {
    match name {
        "Bash" => ToolCall::Exec {
            command: str_field(input, "command"),
        },
        "Read" => ToolCall::ReadFile {
            path: str_field(input, "file_path"),
        },
        "Write" => ToolCall::WriteFile {
            path: str_field(input, "file_path"),
            content: opt_str_field(input, "content"),
        },
        "Edit" => ToolCall::EditFile {
            path: str_field(input, "file_path"),
            old_string: opt_str_field(input, "old_string"),
            new_string: opt_str_field(input, "new_string"),
        },
        "Grep" => ToolCall::Search {
            pattern: str_field(input, "pattern"),
            path: opt_str_field(input, "path"),
        },
        "Glob" => ToolCall::Glob {
            pattern: str_field(input, "pattern"),
        },
        "WebFetch" => ToolCall::WebFetch {
            url: str_field(input, "url"),
            prompt: opt_str_field(input, "prompt"),
        },
        "WebSearch" => ToolCall::WebSearch {
            query: str_field(input, "query"),
        },
        "TodoWrite" => ToolCall::Todo {
            items: input
                .get("todos")
                .and_then(Value::as_array)
                .map(|a| a.as_slice())
                .unwrap_or_default()
                .iter()
                .map(|t| {
                    TodoItem::new(
                        str_field(t, "content"),
                        TodoStatus::parse(t.get("status").and_then(Value::as_str).unwrap_or("")),
                    )
                })
                .collect(),
        },
        // The subagent spawn: name the chip — and the tab it opens — after
        // the TASK, not the bare tool ("Agent" alone says nothing in a tab
        // strip; the tool's `description` is where the work is named).
        "Agent" | "Task" => {
            let description = str_field(input, "description");
            ToolCall::Unknown {
                name: if description.is_empty() {
                    "Agent".into()
                } else {
                    format!("Agent: {description}")
                },
                input: (!input.is_null()).then(|| input.clone()),
            }
        }
        // MCP tools arrive as `mcp__<server>__<tool>`.
        _ => match name.strip_prefix("mcp__").and_then(|r| r.split_once("__")) {
            Some((server, tool)) => ToolCall::Mcp {
                server: server.into(),
                tool: tool.into(),
                input: (!input.is_null()).then(|| input.clone()),
            },
            None => ToolCall::Unknown {
                name: name.into(),
                input: (!input.is_null()).then(|| input.clone()),
            },
        },
    }
}

/// A forked skill's chip once it runs: a spawn named after the skill
/// (`Agent: /review`), like an Agent call is after its description.
fn forked_skill_call(input: Value) -> ToolCall {
    let name = match input
        .get("skill")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        Some(skill) => format!("Agent: /{skill}"),
        None => "Agent".into(),
    };
    ToolCall::Unknown {
        name,
        input: (!input.is_null()).then_some(input),
    }
}

fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Wrap an event as subagent-attributed traffic.
fn tag(parent: &str, event: AgentEvent) -> AgentEvent {
    AgentEvent::Subagent {
        parent_tool_use_id: parent.to_owned(),
        event: Box::new(event),
    }
}

/// A subagent's assistant message, attributed. The 2.1.x wire streams NO
/// tagged partial deltas (live-verified): a subagent's text and thinking
/// arrive only as full blocks on its tagged assistant frames — emit them, in
/// block order with the tool calls, or subagent transcripts are
/// tool-chips-only.
fn subagent_message(
    paths: &mut std::collections::HashMap<String, String>,
    parent: &str,
    f: &MessageFrame,
) -> Vec<AgentEvent> {
    let mut out: Vec<AgentEvent> = f
        .message
        .blocks()
        .filter_map(|b: ContentBlock| match b.kind.as_str() {
            "text" if !b.text.is_empty() => Some(tag(
                parent,
                AgentEvent::TextDelta {
                    text: format!("{}\n\n", b.text.trim_end()),
                },
            )),
            "thinking" if !b.thinking.trim().is_empty() => Some(tag(
                parent,
                AgentEvent::ReasoningDelta {
                    text: format!("{}\n\n", b.thinking.trim_end()),
                },
            )),
            "tool_use" => {
                let call = decode_tool_use(&b.name, &b.input);
                remember_tool_file(paths, &b.id, &call);
                Some(tag(
                    parent,
                    AgentEvent::ToolCall {
                        id: b.id.clone(),
                        call,
                    },
                ))
            }
            _ => None,
        })
        .collect();
    if let Some(code) = &f.error {
        out.push(tag(
            parent,
            AgentEvent::Error {
                message: assistant_error_text(code),
            },
        ));
    }
    out
}

fn remember_tool_file(
    paths: &mut std::collections::HashMap<String, String>,
    id: &str,
    call: &ToolCall,
) {
    if id.is_empty() {
        return;
    }
    let path = match call {
        ToolCall::ReadFile { path }
        | ToolCall::WriteFile { path, .. }
        | ToolCall::EditFile { path, .. } => path.as_str(),
        _ => return,
    };
    let name = file_basename(path);
    if name.is_empty() {
        return;
    }
    paths.insert(id.to_owned(), name.to_owned());
}

fn empty_tool_result(id: impl Into<String>, is_error: bool) -> AgentEvent {
    AgentEvent::ToolResult {
        id: id.into(),
        is_error,
        output: None,
        diff: None,
        preview_paths: Vec::new(),
        image_payloads: Vec::new(),
        exit_code: None,
    }
}

/// Records `f`'s Bash calls, so their results carry the command's output.
fn note_commands(f: &MessageFrame, commands: &mut std::collections::HashMap<String, bool>) {
    for b in f.message.blocks() {
        if b.kind == "tool_use" && b.name == "Bash" {
            let background = b
                .input
                .get("run_in_background")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            commands.insert(b.id, background);
        }
    }
}

/// A `tool_result` block as its event. A Bash call's carries its output (the
/// end of it, [`crate::upstream_proto::command_output_cap`]) and exit code: Claude Code
/// leads a failed command's text with `Exit code N`; one that succeeded says
/// nothing, so it exited 0 — unless it went to the background, which says
/// only that it started. Every other tool's output stays off the event.
fn tool_result(
    b: &ContentBlock,
    commands: &mut std::collections::HashMap<String, bool>,
) -> AgentEvent {
    let is_error = b.is_error.unwrap_or(false);
    let (output, exit_code) = match commands.remove(&b.tool_use_id) {
        Some(background) => {
            let text = result_text(&b.content);
            let (code, rest) = match text
                .strip_prefix("Exit code ")
                .map(|r| r.split_once('\n').unwrap_or((r, "")))
                // (`Exit code 137 [killed]` for one stopped alone.)
                .and_then(|(n, rest)| {
                    Some((n.split_whitespace().next()?.parse::<i32>().ok()?, rest))
                }) {
                Some((code, rest)) => (Some(code), rest),
                None => (None, text.as_str()),
            };
            let exit = code.or((!is_error && !background).then_some(0));
            (crate::upstream_proto::command_output_cap(rest), exit)
        }
        None => (None, None),
    };
    AgentEvent::ToolResult {
        id: b.tool_use_id.clone(),
        is_error,
        output,
        diff: None,
        preview_paths: Vec::new(),
        image_payloads: Vec::new(),
        exit_code,
    }
}

/// A `tool_result`'s text: its string, or its text blocks joined.
fn result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// An id the CLI minted, safe in a path.
fn path_safe(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// CLI-synthesized text that rides a user frame but is NOT conversation:
/// `<system-reminder>` context injections and the interruption marker the CLI
/// stamps into the transcript when a turn (or a subagent) is stopped.
///
/// On a TAGGED frame the distinction is load-bearing, not cosmetic. A tagged
/// user message means "the parent steered its subagent", which announces more
/// work and is therefore the one event allowed to resurrect a settled spawn
/// chip. The CLI emits `[Request interrupted by user]` on the child feed
/// immediately AFTER the subagent's `done{interrupted}` — read as a steer it
/// un-settled a chip that nothing would ever settle again, and the spinner ran
/// forever.
/// It announces the opposite of more work.
///
/// Prefix-matched: the CLI ships at least two spellings of the marker
/// (`…by user]` and `…by user for tool use]`).
///
/// Also the CLI's image note, which rides a user frame after a tool result
/// that already carries the pixels: `[Image: source: <path>]` and
/// `[Image: original WxH, displayed at WxH. …]`. On a tagged frame it must
/// not read as a steer.
fn is_synthetic_user_text(text: &str) -> bool {
    let text = text.trim();
    if text.starts_with("<system-reminder>") || text.starts_with("[Request interrupted") {
        return true;
    }
    is_image_note(text)
}

/// The CLI's image note exactly: one line, `[Image: source: <path>]` (the
/// path may be followed by `, original WxH, displayed at WxH. …`), or
/// `[Image: original WxH, displayed at WxH. …]`. A steer that only starts
/// and ends like one (`[Image: original mockup attached]\nmatch it [urgent]`)
/// is not it.
fn is_image_note(text: &str) -> bool {
    if text.contains('\n') {
        return false;
    }
    let Some(rest) = text
        .strip_prefix("[Image: ")
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return false;
    };
    if let Some(path) = rest.strip_prefix("source: ") {
        return !path.trim().is_empty() && !path.contains(['[', ']']);
    }
    rest.strip_prefix("original ").is_some_and(sizes_note)
}

/// `WxH, displayed at WxH` and whatever the CLI says after it.
fn sizes_note(text: &str) -> bool {
    fn dimensions(text: &str) -> Option<&str> {
        let (width, rest) = text.split_once('x')?;
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        (!width.is_empty() && width.bytes().all(|b| b.is_ascii_digit()) && digits > 0)
            .then(|| &rest[digits..])
    }
    dimensions(text)
        .and_then(|rest| rest.strip_prefix(", displayed at "))
        .and_then(dimensions)
        .is_some_and(|rest| !rest.contains(['[', ']']))
}

/// Images kept from one tool result. Matches the chip preview cap.
const MAX_TOOL_RESULT_IMAGES: usize = 4;

fn is_base64_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=')
}

/// Copy `data` when it looks like base64 within the image size cap.
/// Whitespace is removed. Anything else is dropped before it is stored.
fn compact_base64(data: &str) -> Option<String> {
    let limit = crate::upstream_proto::IMAGE_MAX_BYTES.div_ceil(3) * 4;
    let mut count = 0usize;
    for byte in data.bytes() {
        if byte.is_ascii_whitespace() {
            continue;
        }
        if !is_base64_byte(byte) {
            return None;
        }
        count += 1;
        if count > limit {
            return None;
        }
    }
    if count == 0 {
        return None;
    }
    if count == data.len() {
        return Some(data.to_owned());
    }
    Some(
        data.bytes()
            .filter(|byte| !byte.is_ascii_whitespace())
            .map(char::from)
            .collect(),
    )
}

fn push_tool_image(out: &mut Vec<crate::upstream_proto::ToolResultImage>, name: &str, data: &str) {
    if out.len() >= MAX_TOOL_RESULT_IMAGES {
        return;
    }
    let Some(data_base64) = compact_base64(data) else {
        return;
    };
    out.push(crate::upstream_proto::ToolResultImage {
        name: name.to_owned(),
        data_base64,
    });
}

/// Image blocks inside a tool result's `content` array.
fn images_in_content(
    content: Option<&Value>,
    name: &str,
) -> Vec<crate::upstream_proto::ToolResultImage> {
    let mut out = Vec::new();
    let Some(blocks) = content.and_then(Value::as_array) else {
        return out;
    };
    for block in blocks {
        if out.len() >= MAX_TOOL_RESULT_IMAGES {
            break;
        }
        if block.get("type").and_then(Value::as_str) != Some("image") {
            continue;
        }
        if let Some(kind) = block.pointer("/source/type").and_then(Value::as_str)
            && kind != "base64"
        {
            continue;
        }
        let Some(data) = block
            .pointer("/source/data")
            .and_then(Value::as_str)
            .or_else(|| block.get("data").and_then(Value::as_str))
        else {
            continue;
        };
        push_tool_image(&mut out, name, data);
    }
    out
}

/// `toolUseResult` image, used only when the content blocks had none.
fn image_from_tool_use_result(
    value: Option<&Value>,
    name: &str,
) -> Option<crate::upstream_proto::ToolResultImage> {
    let value = value?;
    if value.get("type").and_then(Value::as_str) != Some("image") {
        return None;
    }
    let data = value.pointer("/file/base64").and_then(Value::as_str)?;
    let data_base64 = compact_base64(data)?;
    Some(crate::upstream_proto::ToolResultImage {
        name: name.to_owned(),
        data_base64,
    })
}

fn file_basename(path: &str) -> &str {
    path.rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or("image.png")
}

/// What `--resume <id>` would find in Claude's native history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Transcript {
    /// A transcript with conversation in it.
    Conversation,
    /// A transcript read through to its end without a single user or
    /// assistant line. The CLI refuses to resume it ("No conversation
    /// found") and exits.
    Empty,
    /// The history could not be looked at, or holds no file for the session
    /// — the CLI may keep it under another config root (a wrapper setting
    /// its own `CLAUDE_CONFIG_DIR`): leave it to the CLI, whose "No
    /// conversation found" relaunches the run fresh when it truly is gone.
    Unknown,
}

/// A `/skill` typed as the prompt that runs in a fork (`context: fork`). No
/// tool call announces it: its `task_started` has no tool id, and the CLI
/// holds its transcript back, then replays it tagged `forked-command-<name>`
/// after its `task_notification`, "together and in order after the forked
/// skill finishes" (https://code.claude.com/docs/en/headless.md#follow-subagent-messages;
/// live-verified 2.1.310).
struct TypedFork {
    /// Its chip, `forked-command-<name>-<task>`: the CLI reuses its own id
    /// on every run of the command.
    chip: String,
    /// Its task id, which its notification carries.
    task: String,
    /// The prompt its tab opened with, until the replay echoes it.
    echo: Option<String>,
    /// What its notification said, kept for the turn's end.
    status: Option<DoneStatus>,
    /// Its session, whose history holds its transcript while it runs.
    session: String,
    /// Uuids of the records shown from that transcript.
    seen: std::collections::HashSet<String>,
    /// `None` until its replay begins, then whether the replay repeated a
    /// record `seen`.
    replayed: Option<bool>,
}

/// Per-run normalization state.
///
/// `saw_init` dedupes `system:init` — the CLI re-emits it every time the model
/// is re-invoked WITHIN one session (a background-subagent wake turn, a
/// scheduled wakeup), not just at start (live-verified against 2.1.228: a
/// background subagent finishing produces a second init with the SAME session
/// id, then a second `result`). Downstream, `SessionStarted` is the fold's run
/// boundary (it resets accumulated parts), so one run ⇒ one `SessionStarted`;
/// the wake turn's own frames flow through and the engine's parked-session
/// resume turns them into the done→Working→done wake.
pub(crate) struct Normalizer {
    saw_init: bool,
    last_model: Option<String>,
    /// Background-agent ids (`task_started.task_id`) → the spawning Agent
    /// tool_use id. `SendMessage` steers address the AGENT id; this map
    /// re-keys them onto the spawn chip's feed (the wire never echoes the
    /// steer on the child feed — live-verified 2.1.228).
    agent_tasks: std::collections::HashMap<String, String>,
    /// Later tool calls that wake the same task → its original spawn.
    agent_tool_spawns: std::collections::HashMap<String, String>,
    /// Parent-feed `Skill` calls, tool_use id → input, until their task
    /// starts (a `context: fork` skill runs as a subagent under its Skill
    /// call's id, and its `task_started` re-decodes the chip as a spawn) or
    /// their result shows they ran inline.
    skill_calls: std::collections::HashMap<String, Value>,
    /// tool_use ids of Agent/Task spawn calls, recorded from their own
    /// assistant frames (plus `task_started`'s agent-task pairing). Gates
    /// `task_notification`: background SHELL tasks settle through the same
    /// subtype carrying their Bash call's id, and tagging that Done as
    /// subagent traffic stamped a spawn ref onto an ordinary Run chip —
    /// which then opened as an empty, never-created subagent doc.
    agent_spawn_tools: std::collections::HashSet<String>,
    /// tool_use id → file basename for Read/Write/Edit, so an image result
    /// can be labeled with the file the tool named. Removed when the result
    /// arrives.
    tool_paths: std::collections::HashMap<String, String>,
    /// Bash calls not yet answered, tool_use id → whether it runs in the
    /// background: their results carry the command's output and exit code.
    command_calls: std::collections::HashMap<String, bool>,
    /// The typed fork of the turn under way (a turn runs one at most). Its
    /// replay feeds its chip only until the turn ends: a `forked-command-*`
    /// id after that stays the CLI's own, which no chip carries.
    typed_fork: Option<TypedFork>,
    /// Typed forks' transcripts are read off disk (see `fork_tail`) until a
    /// replay repeats none of the records read.
    fork_transcripts: bool,
    /// Rotates at each assistant-frame close and at each steer; SessionStarted
    /// carries the first value so folds can attribute deltas from the start.
    assistant_message_id: String,
    /// Last session id seen (init or result) — used for synthetic Dones.
    pub session_id: Option<String>,
    /// This turn already showed its failure as an `Error` part (an
    /// assistant-level error code, a blocking rate limit): the result's own
    /// wording of the same failure is not repeated.
    turn_error_shown: bool,
    /// This turn already said it is retrying the API.
    retry_noticed: bool,
}

impl Normalizer {
    pub fn new() -> Self {
        Self {
            saw_init: false,
            last_model: None,
            agent_tasks: std::collections::HashMap::new(),
            agent_tool_spawns: std::collections::HashMap::new(),
            skill_calls: std::collections::HashMap::new(),
            agent_spawn_tools: std::collections::HashSet::new(),
            tool_paths: std::collections::HashMap::new(),
            command_calls: std::collections::HashMap::new(),
            typed_fork: None,
            fork_transcripts: true,
            assistant_message_id: new_message_id(),
            session_id: None,
            turn_error_shown: false,
            retry_noticed: false,
        }
    }

    /// Tool results on a user frame. A Bash call's carries its output tail
    /// and exit code ([`tool_result`]). Image bytes ride the result's content
    /// blocks (or `toolUseResult` when those blocks have none) and stay off
    /// the journal: only `image_payloads` carries them, and the engine strips
    /// that before publish.
    fn tool_result_events(
        &mut self,
        frame: &MessageFrame,
        clear_inline_skills: bool,
    ) -> Vec<AgentEvent> {
        let mut pending = Vec::new();
        for block in frame.message.blocks() {
            if block.kind != "tool_result" {
                continue;
            }
            if clear_inline_skills {
                self.skill_calls.remove(&block.tool_use_id);
            }
            let name = self
                .tool_paths
                .remove(&block.tool_use_id)
                .unwrap_or_else(|| "image.png".into());
            let images = images_in_content(Some(&block.content), &name);
            let event = tool_result(&block, &mut self.command_calls);
            pending.push((event, name, images));
        }
        if pending.len() == 1 && pending[0].2.is_empty() {
            let name = pending[0].1.clone();
            if let Some(image) = image_from_tool_use_result(frame.tool_use_result.as_ref(), &name) {
                pending[0].2.push(image);
            }
        }
        pending
            .into_iter()
            .map(|(mut event, _name, images)| {
                if let AgentEvent::ToolResult { image_payloads, .. } = &mut event {
                    *image_payloads = images;
                }
                event
            })
            .collect()
    }

    /// The CLI reported its session (`system:init`).
    pub fn started(&self) -> bool {
        self.saw_init
    }

    /// Restore spawn identity from one parsed record of Claude's native
    /// history. The CLI does not replay task_started when --resume starts a
    /// new process, so the spawns a chat made are read back from its
    /// transcript, line by line, by the driver.
    pub fn restore_spawn_record(&mut self, record: &Value) {
        // A subagent's own nested spawns are not chips on this chat.
        if record.get("isSidechain").and_then(Value::as_bool) == Some(true)
            || !record.get("parent_tool_use_id").is_none_or(Value::is_null)
        {
            return;
        }
        let Some(blocks) = record.pointer("/message/content").and_then(Value::as_array) else {
            return;
        };
        match record.get("type").and_then(Value::as_str) {
            Some("assistant") => {
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) == Some("tool_use")
                        && matches!(
                            block.get("name").and_then(Value::as_str),
                            Some("Agent" | "Task")
                        )
                        && let Some(id) = block
                            .get("id")
                            .and_then(Value::as_str)
                            .filter(|id| !id.is_empty())
                    {
                        self.agent_spawn_tools.insert(id.to_owned());
                    }
                }
            }
            Some("user") => {
                if let Some(agent) = record
                    .pointer("/toolUseResult/agentId")
                    .and_then(Value::as_str)
                    .filter(|id| !id.is_empty())
                    && let Some(spawn) = blocks
                        .iter()
                        .filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result"))
                        .filter_map(|b| b.get("tool_use_id").and_then(Value::as_str))
                        .find(|id| self.agent_spawn_tools.contains(*id))
                {
                    self.agent_tasks
                        .entry(agent.to_owned())
                        .or_insert_with(|| spawn.to_owned());
                }
            }
            _ => {}
        }
    }

    /// Rotate the assistant message id for a steer boundary; returns
    /// (previous, next) for the `Steered` event.
    pub fn rotate_for_steer(&mut self) -> (String, String) {
        let prev = std::mem::replace(&mut self.assistant_message_id, new_message_id());
        (prev, self.assistant_message_id.clone())
    }

    /// The transcript a tagged frame feeds: the typed fork's chip, for any
    /// `forked-command-*` id while it runs (the name after the prefix "can
    /// differ from the one you typed"); the spawn a later call woke; else the
    /// id itself.
    fn route<'a>(&'a self, parent: &'a str) -> &'a str {
        match &self.typed_fork {
            Some(fork) if parent.starts_with("forked-command-") => &fork.chip,
            _ => self
                .agent_tool_spawns
                .get(parent)
                .map_or(parent, String::as_str),
        }
    }

    /// The turn is ending: its typed fork settles with the status its
    /// notification brought, else the turn's, once (a late notification
    /// finds no spawn). No tool result ever answers the chip, so it is
    /// resolved here too.
    pub(crate) fn settle_typed_fork(&mut self, turn: DoneStatus) -> Vec<AgentEvent> {
        let Some(fork) = self.typed_fork.take() else {
            return Vec::new();
        };
        self.agent_tasks.remove(&fork.task);
        self.agent_spawn_tools.remove(&fork.chip);
        // Nothing promises the replay reuses the file's uuids: when it
        // repeated none, every record showed twice, so this run's later
        // forks wait for their replay.
        if !fork.seen.is_empty() && fork.replayed == Some(false) {
            tracing::warn!(
                target: "reno_harness::claude",
                chip = %fork.chip,
                "the typed fork's replay repeated no record read off disk; no longer reading forks' transcripts"
            );
            self.fork_transcripts = false;
        }
        let status = fork.status.unwrap_or(turn);
        vec![
            tag(
                &fork.chip,
                AgentEvent::Done {
                    status,
                    result: None,
                    error: None,
                    session_id: None,
                },
            ),
            empty_tool_result(&fork.chip, status == DoneStatus::Errored),
        ]
    }

    /// The typed fork whose transcript the driver reads off disk, as
    /// `(chip, session, task)`, until its replay begins.
    pub(crate) fn fork_to_follow(&self) -> Option<(&str, &str, &str)> {
        self.typed_fork
            .as_ref()
            .filter(|fork| {
                self.fork_transcripts
                    && fork.replayed.is_none()
                    && path_safe(&fork.session)
                    && path_safe(&fork.task)
            })
            .map(|fork| {
                (
                    fork.chip.as_str(),
                    fork.session.as_str(),
                    fork.task.as_str(),
                )
            })
    }

    /// One record of the typed fork's transcript on disk: its messages and
    /// tool results, as its replay shows them. Its prompt (the tab opened
    /// with it) and the bookkeeping around them (attachments, API request
    /// records) are not conversation. Once the replay has begun, it brings
    /// the rest.
    pub(crate) fn fork_record(&mut self, record: Value) -> Vec<AgentEvent> {
        let Some(chip) = self
            .typed_fork
            .as_ref()
            .filter(|fork| fork.replayed.is_none())
            .map(|fork| fork.chip.clone())
        else {
            return Vec::new();
        };
        let assistant = match record.get("type").and_then(Value::as_str) {
            Some("assistant") => true,
            Some("user") => false,
            _ => return Vec::new(),
        };
        let Ok(f) = serde_json::from_value::<MessageFrame>(record) else {
            return Vec::new();
        };
        let events = if assistant {
            note_commands(&f, &mut self.command_calls);
            subagent_message(&mut self.tool_paths, &chip, &f)
        } else {
            // Disk replay shows tool results only, the same as before. Image
            // bytes on those results are kept; a scale note is not a message.
            self.tool_result_events(&f, false)
                .into_iter()
                .map(|event| tag(&chip, event))
                .collect()
        };
        if !events.is_empty()
            && let Some(uuid) = f.uuid
            && let Some(fork) = self.typed_fork.as_mut()
        {
            fork.seen.insert(uuid);
        }
        events
    }

    /// A frame of the typed fork's replay, which brings the rest of its
    /// transcript. A record already read off disk shows nothing: the replay
    /// repeats each one under its uuid (live-verified 2.1.310).
    fn fork_repeat(&mut self, parent: &str, uuid: Option<&str>) -> bool {
        let Some(fork) = self.typed_fork.as_mut().filter(|fork| fork.chip == parent) else {
            return false;
        };
        let repeat = uuid.is_some_and(|uuid| fork.seen.contains(uuid));
        fork.replayed = Some(fork.replayed == Some(true) || repeat);
        repeat
    }

    /// Normalize one stdout frame into 0+ unified events. `interrupted` folds
    /// a post-interrupt `result` into `Done { status: Interrupted }`.
    pub fn normalize(&mut self, frame: Frame, interrupted: bool) -> Vec<AgentEvent> {
        match frame {
            Frame::System(f) => {
                // A background subagent's completion arrives as an UNTAGGED
                // `task_notification` carrying the spawning tool's id — the
                // wire's only terminal signal for it (live-verified 2.1.228:
                // no tagged frame follows; the subagent's stream just stops).
                // Surface it as the subagent's tagged Done so the chip flips
                // done/failed and the transcript freezes.
                if f.subtype == "task_notification" {
                    // A resumed task's notification may carry SendMessage's
                    // id, or omit the tool id entirely. Its task id is stable.
                    let parent = f
                        .task_id
                        .as_deref()
                        .and_then(|task| self.agent_tasks.get(task).map(String::as_str))
                        .or_else(|| f.tool_use_id.as_deref().filter(|t| !t.is_empty()));
                    let Some(parent) = parent else {
                        return Vec::new();
                    };
                    // Only a KNOWN spawn settles as a subagent. Background
                    // SHELL tasks (`Bash` with `run_in_background`) settle
                    // through this same subtype carrying the Bash call's id —
                    // tagging that Done would bind a subagent ref onto an
                    // ordinary Run chip. Spawn ids are learned from live
                    // calls/task starts or restored from the native history.
                    if !self.agent_spawn_tools.contains(parent) {
                        return Vec::new();
                    }
                    let status = match f.status.as_deref().unwrap_or("") {
                        "completed" | "complete" | "succeeded" | "success" => DoneStatus::Completed,
                        "failed" | "errored" | "error" => DoneStatus::Errored,
                        "killed" | "cancelled" | "canceled" | "stopped" | "interrupted" => {
                            DoneStatus::Interrupted
                        }
                        // Non-terminal notification shapes: nothing to close.
                        _ => return Vec::new(),
                    };
                    // A typed fork's transcript is still to come: settled
                    // now, its tab would drop the replay as stragglers.
                    if let Some(fork) = self.typed_fork.as_mut().filter(|fork| fork.chip == parent)
                    {
                        fork.status = Some(status);
                        return Vec::new();
                    }
                    return vec![tag(
                        parent,
                        AgentEvent::Done {
                            status,
                            result: None,
                            error: None,
                            session_id: None,
                        },
                    )];
                }
                // An AGENT task starting (subagent_type present — subagent-
                // owned shell tasks carry the same subtype without it):
                // record agentId → spawn id for SendMessage steer re-keying.
                if f.subtype == "task_started"
                    && f.subagent_type.is_some()
                    && let (Some(task), Some(tool)) = (
                        f.task_id.as_deref().filter(|t| !t.is_empty()),
                        f.tool_use_id.as_deref().filter(|t| !t.is_empty()),
                    )
                {
                    let spawn = self
                        .agent_tasks
                        .entry(task.to_owned())
                        .or_insert_with(|| tool.to_owned());
                    self.agent_spawn_tools.insert(spawn.clone());
                    self.agent_tool_spawns
                        .insert(tool.to_owned(), spawn.clone());
                    // A `context: fork` skill runs as a subagent under its
                    // Skill call's id, and this start precedes all of its
                    // traffic (live-verified 2.1.310). Re-decoded as a spawn,
                    // the chip gives that traffic a card and a tab to feed.
                    return self
                        .skill_calls
                        .remove(tool)
                        .map(|input| AgentEvent::ToolCall {
                            id: tool.to_owned(),
                            call: forked_skill_call(input),
                        })
                        .into_iter()
                        .collect();
                }
                // A `/skill` typed as the prompt starts its fork with no tool
                // id, named after the command (see [`TypedFork`]). Its chip,
                // opened with the skill's prompt, gives the fork a row and a
                // tab while the CLI holds its transcript back.
                if f.subtype == "task_started"
                    && f.subagent_type.is_some()
                    && f.tool_use_id.as_deref().is_none_or(str::is_empty)
                    && let Some(task) = f.task_id.as_deref().filter(|t| !t.is_empty())
                    && let Some(name) = f
                        .description
                        .as_deref()
                        .and_then(|d| d.split_whitespace().next())
                        .and_then(|command| command.strip_prefix('/'))
                        .filter(|name| !name.is_empty())
                {
                    // A turn runs one at most. Should another start before
                    // the first settled, the first ends here, not run on as
                    // a chip nothing settles.
                    let mut out = self.settle_typed_fork(DoneStatus::Interrupted);
                    let chip = format!("forked-command-{name}-{task}");
                    self.agent_tasks.insert(task.to_owned(), chip.clone());
                    self.agent_spawn_tools.insert(chip.clone());
                    let prompt = f
                        .prompt
                        .as_ref()
                        .and_then(Value::as_str)
                        .filter(|p| !p.trim().is_empty());
                    out.push(AgentEvent::ToolCall {
                        id: chip.clone(),
                        call: forked_skill_call(serde_json::json!({ "skill": name })),
                    });
                    out.extend(prompt.map(|text| {
                        tag(
                            &chip,
                            AgentEvent::UserMessage {
                                text: text.to_owned(),
                            },
                        )
                    }));
                    self.typed_fork = Some(TypedFork {
                        chip,
                        task: task.to_owned(),
                        echo: prompt.map(str::to_owned),
                        status: None,
                        session: f.session_id.clone(),
                        seen: std::collections::HashSet::new(),
                        replayed: None,
                    });
                    return out;
                }
                // The CLI retrying a failed API call waits between attempts
                // with nothing on the wire: keep the turn visibly alive, and
                // once the retries add up, say why it is slow — once.
                if f.subtype == "api_retry" {
                    let attempt = f.attempt.unwrap_or(1);
                    tracing::info!(
                        target: "reno_harness::claude",
                        attempt,
                        max = ?f.max_retries,
                        status = ?f.error_status,
                        delay_ms = ?f.retry_delay_ms,
                        "Claude API retry"
                    );
                    let mut out = vec![AgentEvent::ReasoningDelta {
                        text: String::new(),
                    }];
                    if attempt >= 3 && !self.retry_noticed {
                        self.retry_noticed = true;
                        let why = match (f.error_status, f.error.as_ref().and_then(Value::as_str)) {
                            (Some(429), _) | (_, Some("rate_limit")) => {
                                "is rate limited".to_owned()
                            }
                            (Some(529), _) | (_, Some("overloaded")) => "is overloaded".to_owned(),
                            (Some(status), _) => format!("returned an error ({status})"),
                            (None, _) => "could not be reached".to_owned(),
                        };
                        let of = f
                            .max_retries
                            .map(|m| format!(" of {m}"))
                            .unwrap_or_default();
                        out.push(AgentEvent::Error {
                            message: format!(
                                "The Claude API {why} — still retrying (attempt {attempt}{of})."
                            ),
                        });
                    }
                    return out;
                }
                // Compaction runs a whole model call with nothing streamed.
                if f.subtype == "status" && f.status.as_deref() == Some("compacting") {
                    return vec![AgentEvent::ReasoningDelta {
                        text: String::new(),
                    }];
                }
                // The context shrank: report the new occupancy now rather
                // than at the next assistant frame.
                if f.subtype == "compact_boundary" {
                    return f
                        .compact_metadata
                        .as_ref()
                        .and_then(|m| m.get("post_tokens"))
                        .and_then(Value::as_u64)
                        .map(|tokens| AgentEvent::ContextUsage {
                            tokens: Some(tokens),
                            window: None,
                        })
                        .into_iter()
                        .collect();
                }
                if f.subtype != "init" || self.saw_init {
                    return Vec::new();
                }
                self.saw_init = true;
                self.session_id = Some(f.session_id.clone());
                vec![AgentEvent::SessionStarted {
                    harness: HarnessId::ClaudeCode,
                    model: f.model,
                    tools: f.tools,
                    cwd: f.cwd,
                    session_id: f.session_id,
                    assistant_message_id: self.assistant_message_id.clone(),
                }]
            }

            // Frames with `parent_tool_use_id` set belong to a SUBAGENT's
            // nested transcript: a background subagent runs concurrently with
            // the parent's own stream, so folding them into the parent feed
            // would split a contiguous text block around phantom tool calls
            // (and a subagent's message boundaries must never rotate the
            // parent's assistant message id). They are wrapped in
            // `AgentEvent::Subagent` instead — the engine routes them to the
            // subagent's own doc.
            Frame::StreamEvent(f) => {
                if f.event.kind != "content_block_delta" {
                    return Vec::new();
                }
                if let Some(parent) = &f.parent_tool_use_id {
                    let parent = self.route(parent);
                    return match f.event.delta.kind.as_str() {
                        "text_delta" => vec![tag(
                            parent,
                            AgentEvent::TextDelta {
                                text: f.event.delta.text,
                            },
                        )],
                        "thinking_delta" if !f.event.delta.thinking.is_empty() => vec![tag(
                            parent,
                            AgentEvent::ReasoningDelta {
                                text: f.event.delta.thinking,
                            },
                        )],
                        // Subagent liveness heartbeats have no consumer; the
                        // parent turn is already done (eager-done policy).
                        _ => Vec::new(),
                    };
                }
                match f.event.delta.kind.as_str() {
                    "text_delta" => vec![AgentEvent::TextDelta {
                        text: f.event.delta.text,
                    }],
                    "thinking_delta" => vec![AgentEvent::ReasoningDelta {
                        text: f.event.delta.thinking,
                    }],
                    // A big tool input (a 90-line Write) streams as a long run
                    // of input_json_delta frames with nothing else — minutes of
                    // apparent silence that reads as a stalled run. Surface
                    // them as empty reasoning deltas: the engine treats those
                    // as pure liveness heartbeats (never journaled/rendered).
                    "input_json_delta" => vec![AgentEvent::ReasoningDelta {
                        text: String::new(),
                    }],
                    _ => Vec::new(),
                }
            }

            Frame::Assistant(f) => {
                note_commands(&f, &mut self.command_calls);
                if let Some(parent) = &f.parent_tool_use_id {
                    let parent = &self.route(parent).to_owned();
                    if self.fork_repeat(parent, f.uuid.as_deref()) {
                        return Vec::new();
                    }
                    return subagent_message(&mut self.tool_paths, parent, &f);
                }
                // Record spawn tool ids up front: `task_notification` keys on
                // them; a task may finish before a `task_started` arrives. A
                // Skill call waits for its task start: only a forked skill
                // gets one.
                for b in f.message.blocks() {
                    if b.kind != "tool_use" {
                        continue;
                    }
                    match b.name.as_str() {
                        "Agent" | "Task" => {
                            self.agent_spawn_tools.insert(b.id.clone());
                        }
                        "Skill" => {
                            self.skill_calls.insert(b.id.clone(), b.input.clone());
                        }
                        _ => {}
                    }
                }
                let mut out: Vec<AgentEvent> = f
                    .message
                    .blocks()
                    .filter(|b: &ContentBlock| b.kind == "tool_use")
                    .flat_map(|b| {
                        let decoded = decode_tool_use(&b.name, &b.input);
                        remember_tool_file(&mut self.tool_paths, &b.id, &decoded);
                        let call = AgentEvent::ToolCall {
                            id: b.id.clone(),
                            call: decoded,
                        };
                        // A spawn's `prompt` is the subagent's opening user
                        // message — the wire never echoes it on the child
                        // feed (child user frames carry tool results and
                        // steers only), so seed it here and the subagent
                        // transcript starts the way every chat does.
                        let opening = matches!(b.name.as_str(), "Agent" | "Task")
                            .then(|| b.input.get("prompt"))
                            .flatten()
                            .and_then(Value::as_str)
                            .filter(|p| !p.trim().is_empty())
                            .map(|prompt| {
                                tag(
                                    &b.id,
                                    AgentEvent::UserMessage {
                                        text: prompt.to_owned(),
                                    },
                                )
                            });
                        // A SendMessage steer never echoes on the child feed
                        // (live-verified) — surface it from the parent's own
                        // call, re-keyed onto the spawn it addresses.
                        let steer = (b.name == "SendMessage")
                            .then(|| {
                                let to = ["to", "recipient"]
                                    .iter()
                                    .find_map(|k| b.input.get(*k))
                                    .and_then(Value::as_str)?;
                                let spawn = self.agent_tasks.get(to)?;
                                let text = ["message", "content"]
                                    .iter()
                                    .find_map(|k| b.input.get(*k))
                                    .and_then(Value::as_str)
                                    .filter(|m| !m.trim().is_empty())?;
                                Some(tag(
                                    spawn,
                                    AgentEvent::UserMessage {
                                        text: text.to_owned(),
                                    },
                                ))
                            })
                            .flatten();
                        std::iter::once(call).chain(opening).chain(steer)
                    })
                    .collect();
                self.last_model = f.message.model.clone().or(self.last_model.take());
                if let Some(usage) = &f.message.usage {
                    let fields = [
                        "input_tokens",
                        "cache_read_input_tokens",
                        "cache_creation_input_tokens",
                    ];
                    if fields
                        .iter()
                        .any(|key| usage.get(*key).and_then(Value::as_u64).is_some())
                    {
                        let tokens = fields
                            .iter()
                            .filter_map(|key| usage.get(*key).and_then(Value::as_u64))
                            .fold(0u64, u64::saturating_add);
                        out.push(AgentEvent::ContextUsage {
                            tokens: Some(tokens),
                            window: None,
                        });
                    }
                }
                // A failed turn (usage limit, billing, auth, overloaded, …)
                // carries a terse `error` code here — often with empty content
                // and no `result` error — so surface it visibly.
                if let Some(code) = &f.error {
                    self.turn_error_shown = true;
                    out.push(AgentEvent::Error {
                        message: assistant_error_text(code),
                    });
                }
                // The enclosing assistant frame closes the streamed message
                // item; rotate so post-boundary deltas get a fresh id.
                let (prev, _next) = self.rotate_for_steer();
                out.push(AgentEvent::AssistantMessageCompleted {
                    assistant_message_id: prev,
                });
                out
            }

            Frame::User(f) => {
                if let Some(parent) = &f.parent_tool_use_id {
                    let parent = &self.route(parent).to_owned();
                    if self.fork_repeat(parent, f.uuid.as_deref()) {
                        return Vec::new();
                    }
                    // A subagent's tool results echo on the main channel too;
                    // they belong to its transcript, attributed like its calls.
                    let mut out = self
                        .tool_result_events(&f, false)
                        .into_iter()
                        .map(|event| tag(parent, event))
                        .collect::<Vec<_>>();
                    // A meta frame's text is bookkeeping (the image scale note
                    // is one). It must not read as a steer.
                    if f.is_meta {
                        return out;
                    }
                    // A tagged user frame's TEXT blocks are the parent
                    // steering its subagent (SendMessage-style follow-ups —
                    // tool results ride their own blocks, filtered above).
                    // Synthetic harness injections are not conversation, and
                    // must not read as a steer — see [`is_synthetic_user_text`].
                    // Nor is a typed fork's replay of the prompt its tab
                    // opened with, dropped once.
                    let mut echo = self
                        .typed_fork
                        .as_mut()
                        .filter(|fork| fork.chip == *parent)
                        .map(|fork| &mut fork.echo);
                    out.extend(
                        f.message
                            .blocks()
                            .filter(|b: &ContentBlock| {
                                b.kind == "text"
                                    && !b.text.trim().is_empty()
                                    && !is_synthetic_user_text(&b.text)
                                    && echo
                                        .as_mut()
                                        .and_then(|echo| {
                                            echo.take_if(|p| p.trim() == b.text.trim())
                                        })
                                        .is_none()
                            })
                            .map(|b| tag(parent, AgentEvent::UserMessage { text: b.text })),
                    );
                    return out;
                }
                // A skill whose result came back without a task start ran
                // inline: it never forks now. `tool_result_events` drops that
                // pending skill when asked.
                self.tool_result_events(&f, true)
            }

            // A claude.ai plan window was hit. A hard `rejected` blocks the
            // turn — make it visible; allowed/allowed_warning stay quiet, and
            // so does a reject the account's overage carries past.
            Frame::RateLimit(f) => {
                if !f.rate_limit_info.blocks() {
                    return Vec::new();
                }
                let window =
                    rate_window_label(f.rate_limit_info.rate_limit_type.as_deref().unwrap_or(""));
                self.turn_error_shown = true;
                vec![AgentEvent::Error {
                    message: format!(
                        "Claude {window} limit reached — the turn was blocked. Try again after it resets."
                    ),
                }]
            }

            Frame::Result(f) => {
                let error_shown = std::mem::take(&mut self.turn_error_shown);
                self.retry_noticed = false;
                // A result line that would not parse (see `parse_frame`):
                // its turn still ends, with what little the line told us.
                if f.unreadable {
                    tracing::warn!(target: "reno_harness::claude", "unreadable result frame");
                    let failed = f.is_error && !interrupted;
                    let status = if interrupted {
                        DoneStatus::Interrupted
                    } else if failed {
                        DoneStatus::Errored
                    } else {
                        DoneStatus::Completed
                    };
                    let mut out = self.settle_typed_fork(status);
                    out.push(AgentEvent::Done {
                        status,
                        result: None,
                        error: (failed && !error_shown)
                            .then(|| "Claude's result for this turn could not be read.".to_owned()),
                        session_id: self.session_id.clone(),
                    });
                    return out;
                }
                let model_usage = self
                    .last_model
                    .as_ref()
                    .and_then(|model| {
                        f.model_usage.get(model).or_else(|| {
                            f.model_usage.values().find(|entry| {
                                entry.get("canonicalModel").and_then(Value::as_str)
                                    == Some(model.as_str())
                            })
                        })
                    })
                    .or_else(|| {
                        (f.model_usage.len() == 1)
                            .then(|| f.model_usage.values().next())
                            .flatten()
                    });
                let window = model_usage
                    .and_then(|entry| entry.get("contextWindow"))
                    .and_then(Value::as_u64)
                    .filter(|n| *n > 0);
                if let Some(id) = &f.session_id {
                    self.session_id = Some(id.clone());
                }
                let usage = AgentEvent::Usage {
                    input_tokens: f.usage.input_tokens,
                    output_tokens: f.usage.output_tokens,
                };
                let failure = (f.subtype == "success" && !interrupted && !aborted(&f))
                    .then(|| api_failure(&f))
                    .flatten();
                let done = if let Some(error) = failure {
                    AgentEvent::Done {
                        status: DoneStatus::Errored,
                        result: None,
                        error: (!error_shown).then_some(error),
                        session_id: f.session_id,
                    }
                } else if f.subtype == "success" {
                    AgentEvent::Done {
                        status: if interrupted || aborted(&f) {
                            DoneStatus::Interrupted
                        } else {
                            DoneStatus::Completed
                        },
                        result: f.result,
                        error: None,
                        session_id: f.session_id,
                    }
                } else {
                    // Split the CLI's internal `[ede_diagnostic]` breadcrumbs
                    // off the real errors: diagnostics are debug-logged, never
                    // surfaced as transcript error parts.
                    let (diagnostics, errors): (Vec<String>, Vec<String>) = f
                        .errors
                        .iter()
                        .map(|e| match e {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        })
                        .partition(|m| is_internal_diagnostic(m));
                    for diagnostic in &diagnostics {
                        tracing::debug!(
                            target: "reno_harness::claude",
                            "internal CLI diagnostic (not surfaced): {diagnostic}"
                        );
                    }
                    let error = if !errors.is_empty() {
                        // Real user-relevant errors — surface verbatim.
                        Some(errors.join("; "))
                    } else {
                        match f.subtype.as_str() {
                            // Known run-failure subtypes stay visible with
                            // their mapped human wording (never blank — a
                            // blank error folds to no part and the failed
                            // turn reads as a silent non-reply).
                            "error_max_turns"
                            | "error_max_budget_usd"
                            | "error_max_structured_output_retries" => {
                                Some(result_error_text(&f.subtype).to_owned())
                            }
                            // Diagnostic-only ends (the CLI's turn-accounting
                            // telemetry, typically `error_during_execution`
                            // after an abort): nothing user-relevant to show.
                            _ if !diagnostics.is_empty() => None,
                            // The failure already has its part; a generic
                            // line under it adds nothing.
                            _ if error_shown => None,
                            _ => Some(result_error_text(&f.subtype).to_owned()),
                        }
                    };
                    let interrupted = interrupted || aborted(&f);
                    AgentEvent::Done {
                        status: if interrupted {
                            DoneStatus::Interrupted
                        } else {
                            DoneStatus::Errored
                        },
                        result: None,
                        error,
                        session_id: f.session_id,
                    }
                };
                let mut out = Vec::new();
                if let Some(window) = window {
                    out.push(AgentEvent::ContextUsage {
                        tokens: None,
                        window: Some(window),
                    });
                }
                out.push(usage);
                if let AgentEvent::Done { status, .. } = &done {
                    out.extend(self.settle_typed_fork(*status));
                }
                out.push(done);
                out
            }

            // Control frames are handled by the run loop, not normalized.
            Frame::ControlRequest(_)
            | Frame::ControlResponse(_)
            | Frame::CommandLifecycle(_)
            | Frame::Other => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reno_proto::{SessionRef, StopReason, ToolCall};
    use serde_json::json;

    fn parse(raw: &str) -> Frame {
        crate::wire::claude::wire::parse_frame(raw).expect("frame parses")
    }

    fn normalize_one(raw: &str) -> Vec<ExecEvent> {
        Normalizer::new().normalize(parse(raw), false)
    }

    fn text(text: &str) -> ExecEvent {
        ExecEvent::Text { text: text.into() }
    }

    fn done_of(events: &[ExecEvent]) -> (StopReason, Option<String>, Option<String>) {
        match events.last() {
            Some(ExecEvent::Done {
                stop,
                result,
                diagnostic,
            }) => (*stop, result.clone(), diagnostic.clone()),
            other => panic!("no done: {other:?}"),
        }
    }

    #[test]
    fn tool_calls_keep_the_harnesss_name_and_input() {
        let events = normalize_one(
            r#"{"type":"assistant","message":{"content":[
                {"type":"text","text":"hi"},
                {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"ls -la"}},
                {"type":"tool_use","id":"t2","name":"mcp__linear__search","input":{"q":"bug"}},
                {"type":"tool_use","id":"t3","name":"Mystery","input":null}]}}"#,
        );
        assert_eq!(
            events,
            vec![
                ExecEvent::Tool {
                    id: "t1".into(),
                    call: ToolCall {
                        name: "Bash".into(),
                        input: json!({"command": "ls -la"}),
                    },
                },
                ExecEvent::Tool {
                    id: "t2".into(),
                    call: ToolCall {
                        name: "mcp__linear__search".into(),
                        input: json!({"q": "bug"}),
                    },
                },
                ExecEvent::Tool {
                    id: "t3".into(),
                    call: ToolCall {
                        name: "Mystery".into(),
                        input: json!(null),
                    },
                },
            ]
        );
    }

    #[test]
    fn session_start_is_deduped_and_a_wake_turn_still_settles_with_its_own_done() {
        // Live-verified 2.1.228 shape: a background subagent gives an eager result, then a
        // second init (same session id) and a second result on completion.
        let mut norm = Normalizer::new();
        let init = r#"{"type":"system","subtype":"init","model":"m","cwd":"/x","session_id":"s1"}"#;
        assert_eq!(
            norm.normalize(parse(init), false),
            vec![ExecEvent::Started {
                session: SessionRef("s1".into()),
                model: "m".into(),
            }]
        );
        assert!(norm.started());
        assert!(
            norm.normalize(parse(init), false).is_empty(),
            "the wake init is deduped"
        );
        let events = norm.normalize(
            parse(r#"{"type":"result","subtype":"success","session_id":"s1"}"#),
            false,
        );
        assert_eq!(done_of(&events).0, StopReason::Completed);
    }

    #[test]
    fn stream_deltas_map_to_text_and_reasoning_and_liveness_noise_is_dropped() {
        assert_eq!(
            normalize_one(
                r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"hmm"}}}"#,
            ),
            vec![ExecEvent::Reasoning { text: "hmm".into() }]
        );
        assert_eq!(
            normalize_one(
                r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"hi"}}}"#,
            ),
            vec![text("hi")]
        );
        // Redacted thinking, a tool input being generated and a signature carry nothing a run
        // consumer reads.
        for raw in [
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"","estimated_tokens":50}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"input_json_delta","partial_json":"{\"file_"}}}"#,
            r#"{"type":"stream_event","event":{"type":"content_block_delta","delta":{"type":"signature_delta","signature":"abc"}}}"#,
        ] {
            assert!(normalize_one(raw).is_empty(), "{raw}");
        }
    }

    #[test]
    fn subagent_frames_are_not_the_runs_own_feed() {
        let mut norm = Normalizer::new();
        for raw in [
            r#"{"type":"stream_event","parent_tool_use_id":"toolu_sub","event":{"type":"content_block_delta","delta":{"type":"text_delta","text":"sub text"}}}"#,
            r#"{"type":"stream_event","parent_tool_use_id":"toolu_sub","event":{"type":"content_block_delta","delta":{"type":"thinking_delta","thinking":"sub thought"}}}"#,
            r#"{"type":"assistant","parent_tool_use_id":"toolu_sub","message":{"content":[{"type":"text","text":"sub"},{"type":"tool_use","id":"s1","name":"Bash","input":{"command":"echo sub"}}]},"error":"rate_limit"}"#,
            r#"{"type":"user","parent_tool_use_id":"toolu_sub","message":{"content":[{"type":"tool_result","tool_use_id":"s1","is_error":false}]}}"#,
        ] {
            assert!(norm.normalize(parse(raw), false).is_empty(), "{raw}");
        }
        // Not even a child's error code becomes the run's failure.
        let done = norm.normalize(parse(r#"{"type":"result","subtype":"error_during_execution","errors":[]}"#), false);
        assert_eq!(
            done_of(&done).2.as_deref(),
            Some("The run ended with an error.")
        );
    }

    #[test]
    fn a_bash_result_carries_its_exit_code_and_last_output_line() {
        let mut norm = Normalizer::new();
        norm.normalize(
            parse(
                r#"{"type":"assistant","message":{"content":[
                    {"type":"tool_use","id":"ok","name":"Bash","input":{"command":"true"}},
                    {"type":"tool_use","id":"bad","name":"Bash","input":{"command":"false"}},
                    {"type":"tool_use","id":"bg","name":"Bash","input":{"command":"sleep 9","run_in_background":true}},
                    {"type":"tool_use","id":"rd","name":"Read","input":{"file_path":"/a"}}]}}"#,
            ),
            false,
        );
        let events = norm.normalize(
            parse(
                r#"{"type":"user","message":{"content":[
                    {"type":"tool_result","tool_use_id":"ok","is_error":false,"content":"compiling\nall 3 tests passed\n"},
                    {"type":"tool_result","tool_use_id":"bad","is_error":true,"content":"Exit code 2\nmake: *** [all] Error 1"},
                    {"type":"tool_result","tool_use_id":"bg","is_error":false,"content":"Command running in background with ID: b1."},
                    {"type":"tool_result","tool_use_id":"rd","is_error":false,"content":"file text"}]}}"#,
            ),
            false,
        );
        assert_eq!(
            events,
            vec![
                ExecEvent::ToolResult {
                    id: "ok".into(),
                    ok: true,
                    summary: Some("exit code 0: all 3 tests passed".into()),
                },
                ExecEvent::ToolResult {
                    id: "bad".into(),
                    ok: false,
                    summary: Some("exit code 2: make: *** [all] Error 1".into()),
                },
                ExecEvent::ToolResult {
                    id: "bg".into(),
                    ok: true,
                    summary: Some("Command running in background with ID: b1.".into()),
                },
                ExecEvent::ToolResult {
                    id: "rd".into(),
                    ok: true,
                    summary: None,
                },
            ]
        );
    }

    #[test]
    fn a_summary_is_one_short_redacted_line() {
        let mut norm = Normalizer::new();
        norm.normalize(
            parse(r#"{"type":"assistant","message":{"content":[{"type":"tool_use","id":"b","name":"Bash","input":{"command":"x"}}]}}"#),
            false,
        );
        let long = "y".repeat(500);
        let events = norm.normalize(
            parse(&format!(
                r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"b","is_error":false,"content":"{long}"}}]}}}}"#
            )),
            false,
        );
        match &events[..] {
            [ExecEvent::ToolResult { summary: Some(s), .. }] => {
                assert!(s.chars().count() < 260, "{} chars", s.chars().count());
                assert!(s.ends_with('…'));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_rejected_window_carried_by_overage_is_not_a_failure() {
        let mut norm = Normalizer::new();
        norm.normalize(
            parse(r#"{"type":"rate_limit_event","rate_limit_info":{"status":"rejected","rateLimitType":"five_hour","overageStatus":"rejected"}}"#),
            false,
        );
        let blocked = norm.normalize(parse(r#"{"type":"result","subtype":"error_during_execution","errors":[]}"#), false);
        assert_eq!(
            done_of(&blocked).2.as_deref(),
            Some("Claude 5-hour limit reached — the turn was blocked. Try again after it resets.")
        );
        for overage in [
            r#""overageStatus":"allowed""#,
            r#""overageStatus":"allowed_warning""#,
            r#""isUsingOverage":true"#,
        ] {
            let mut norm = Normalizer::new();
            let raw = format!(
                r#"{{"type":"rate_limit_event","rate_limit_info":{{"status":"rejected","rateLimitType":"five_hour",{overage}}}}}"#
            );
            assert!(norm.normalize(parse(&raw), false).is_empty(), "{overage}");
            let done = norm.normalize(parse(r#"{"type":"result","subtype":"error_during_execution","errors":[]}"#), false);
            assert_eq!(
                done_of(&done).2.as_deref(),
                Some("The run ended with an error."),
                "{overage}"
            );
        }
    }

    #[test]
    fn a_result_is_usage_then_done() {
        let events = normalize_one(
            r#"{"type":"result","subtype":"success","result":"ok","usage":{"input_tokens":3,"output_tokens":4},"session_id":"s"}"#,
        );
        assert_eq!(
            events,
            vec![
                ExecEvent::Usage {
                    input_tokens: 3,
                    output_tokens: 4
                },
                ExecEvent::Done {
                    stop: StopReason::Completed,
                    result: Some("ok".into()),
                    diagnostic: None,
                },
            ]
        );
    }

    #[test]
    fn ede_diagnostics_never_surface_as_errors() {
        // The CLI's internal turn-accounting breadcrumbs are not a failure's cause.
        let events = normalize_one(
            r#"{"type":"result","subtype":"error_during_execution","errors":["[ede_diagnostic] result_type=user last_content_type=n/a stop_reason=null"]}"#,
        );
        assert_eq!(done_of(&events), (StopReason::Error, None, None));
    }

    #[test]
    fn real_errors_survive_diagnostic_filtering() {
        let events = normalize_one(
            r#"{"type":"result","subtype":"error_during_execution","errors":["[ede_diagnostic] turn aborted (x) stop_reason=null","Something real broke"]}"#,
        );
        assert_eq!(done_of(&events).2.as_deref(), Some("Something real broke"));
    }

    #[test]
    fn known_failure_subtypes_keep_mapped_wording() {
        for raw in [
            r#"{"type":"result","subtype":"error_max_turns","errors":["[ede_diagnostic] turn aborted (max) stop_reason=null"]}"#,
            r#"{"type":"result","subtype":"error_max_turns","errors":[]}"#,
        ] {
            assert_eq!(
                done_of(&normalize_one(raw)).2.as_deref(),
                Some("The run hit the maximum number of turns."),
                "{raw}"
            );
        }
    }

    #[test]
    fn api_failures_filed_as_success_end_in_error() {
        // Live 2.1.309 with an invalid key: the assistant frame carries the error code, and the
        // result says `success` with `is_error`.
        let mut norm = Normalizer::new();
        norm.normalize(
            parse(
                r#"{"type":"assistant","message":{"model":"<synthetic>","content":[{"type":"text","text":"Invalid API key · Fix external API key"}]},"error":"authentication_failed","api_error_status":401}"#,
            ),
            false,
        );
        let invalid_key = r#"{"type":"result","subtype":"success","is_error":true,"result":"Invalid API key · Fix external API key","terminal_reason":"api_error","api_error_status":401,"session_id":"s"}"#;
        // The assistant's code already named the failure: it is the diagnostic, once.
        assert_eq!(
            done_of(&norm.normalize(parse(invalid_key), false)),
            (
                StopReason::Error,
                None,
                Some("Authentication failed — sign in to Claude again.".into())
            )
        );
        // Without the assistant's code, the result's own text is the diagnostic.
        assert_eq!(
            done_of(&normalize_one(invalid_key)),
            (
                StopReason::Error,
                None,
                Some("Invalid API key · Fix external API key".into())
            )
        );

        let (stop, _, error) = done_of(&normalize_one(
            r#"{"type":"result","subtype":"success","is_error":true,"result":"API Error: 429 rate_limit_error (resets 3pm)","api_error_status":429}"#,
        ));
        assert_eq!(stop, StopReason::Error);
        let error = error.unwrap();
        assert!(error.starts_with("Claude rate limit reached"), "{error}");
        assert!(error.contains("resets 3pm"), "{error}");

        let (_, _, error) = done_of(&normalize_one(
            r#"{"type":"result","subtype":"success","is_error":true,"result":"API Error: 529","api_error_status":529}"#,
        ));
        assert!(error.unwrap().contains("overloaded"));

        for full in [
            r#"{"type":"result","subtype":"success","is_error":true,"result":"Prompt is too long","terminal_reason":"prompt_too_long"}"#,
            r#"{"type":"result","subtype":"success","is_error":true,"result":"Prompt is too long"}"#,
        ] {
            let (stop, _, error) = done_of(&normalize_one(full));
            assert_eq!(stop, StopReason::Error);
            assert!(error.unwrap().contains("/compact"), "{full}");
        }
    }

    #[test]
    fn terminal_reasons_decide_how_a_success_ended() {
        for (raw, expected) in [
            (
                r#"{"type":"result","subtype":"success","is_error":false,"result":"hi","terminal_reason":"completed"}"#,
                StopReason::Completed,
            ),
            (
                r#"{"type":"result","subtype":"success","is_error":false,"terminal_reason":"aborted_streaming"}"#,
                StopReason::Interrupted,
            ),
            (
                r#"{"type":"result","subtype":"error_during_execution","is_error":true,"terminal_reason":"aborted_tools","errors":[]}"#,
                StopReason::Interrupted,
            ),
            (
                r#"{"type":"result","subtype":"success","is_error":false,"terminal_reason":"budget_exhausted"}"#,
                StopReason::Error,
            ),
        ] {
            assert_eq!(done_of(&normalize_one(raw)).0, expected, "{raw}");
        }
    }

    #[test]
    fn an_interrupted_run_folds_its_result_into_interrupted() {
        let mut norm = Normalizer::new();
        let events = norm.normalize(
            parse(r#"{"type":"result","subtype":"error_during_execution","errors":["boom"]}"#),
            true,
        );
        assert_eq!(done_of(&events), (StopReason::Interrupted, None, None));
    }

    #[test]
    fn an_unreadable_result_ends_the_run() {
        let ok = normalize_one(r#"{"type":"result","subtype":"success","usage":{"input_tokens":"x"}}"#);
        assert_eq!(done_of(&ok), (StopReason::Completed, None, None));
        let failed = normalize_one(
            r#"{"type":"result","subtype":"success","is_error":true,"usage":{"input_tokens":"x"}}"#,
        );
        assert_eq!(done_of(&failed).0, StopReason::Error);
        assert!(done_of(&failed).2.is_some());
    }

    #[test]
    fn api_retries_and_compaction_say_nothing_to_the_consumer() {
        let mut norm = Normalizer::new();
        for attempt in 1..=4 {
            let events = norm.normalize(
                parse(&format!(
                    r#"{{"type":"system","subtype":"api_retry","attempt":{attempt},"max_retries":10,"retry_delay_ms":4000,"error_status":529,"error":"overloaded"}}"#
                )),
                false,
            );
            assert!(events.is_empty(), "attempt {attempt}");
        }
        for raw in [
            r#"{"type":"system","subtype":"status","status":"compacting"}"#,
            r#"{"type":"system","subtype":"compact_boundary","compact_metadata":{"trigger":"auto","pre_tokens":180000,"post_tokens":9000}}"#,
            r#"{"type":"system","subtype":"informational","content":"x","level":"info"}"#,
            r#"{"type":"system","subtype":"thinking_tokens","tokens":12}"#,
        ] {
            assert!(norm.normalize(parse(raw), false).is_empty(), "{raw}");
        }
    }

    #[test]
    fn a_persistent_api_retry_is_the_diagnostic_if_the_turn_then_fails() {
        let mut norm = Normalizer::new();
        norm.normalize(
            parse(r#"{"type":"system","subtype":"api_retry","attempt":3,"max_retries":10,"retry_delay_ms":4000,"error_status":529,"error":"overloaded"}"#),
            false,
        );
        let done = norm.normalize(
            parse(r#"{"type":"result","subtype":"error_during_execution","errors":[]}"#),
            false,
        );
        let diagnostic = done_of(&done).2.unwrap();
        assert!(diagnostic.contains("overloaded") && diagnostic.contains("3 of 10"), "{diagnostic}");
    }

    #[test]
    fn the_summary_helper_leads_with_the_exit_code() {
        assert_eq!(summarize(Some("a\nb\n"), Some(0)).as_deref(), Some("exit code 0: b"));
        assert_eq!(summarize(None, Some(3)).as_deref(), Some("exit code 3"));
        assert_eq!(summarize(Some("  \n"), None), None);
    }
}
