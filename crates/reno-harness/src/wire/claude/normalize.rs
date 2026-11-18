//! Frame to [`ExecEvent`] normalization: init dedupe, subagent exclusion, error-code mapping.
//!
//! Sans-IO (ARCH-6): frames in, events out.

use reno_proto::{ExecEvent, SessionRef, StopReason, ToolCall};
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

/// The longest tool-result summary, in characters.
const SUMMARY_CHARS: usize = 200;

/// A tool result's one-line outcome: the exit code, then the last non-empty line of output,
/// redacted and cut short.
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

/// Records `f`'s Bash calls, so their results carry the command's outcome.
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

/// A `tool_result` block as its event. A Bash call's result carries its exit code and the last
/// line of its output: Claude Code leads a failed command's text with `Exit code N`; one that
/// succeeded says nothing, so it exited 0, unless it went to the background, which says only
/// that it started. Every other tool's output stays off the event.
fn tool_result(
    b: &ContentBlock,
    commands: &mut std::collections::HashMap<String, bool>,
) -> ExecEvent {
    let is_error = b.is_error.unwrap_or(false);
    let summary = commands.remove(&b.tool_use_id).and_then(|background| {
        let text = result_text(&b.content);
        let (code, rest) = match text
            .strip_prefix("Exit code ")
            .map(|r| r.split_once('\n').unwrap_or((r, "")))
            // (`Exit code 137 [killed]` for one stopped alone.)
            .and_then(|(n, rest)| Some((n.split_whitespace().next()?.parse::<i32>().ok()?, rest)))
        {
            Some((code, rest)) => (Some(code), rest),
            None => (None, text.as_str()),
        };
        let exit = code.or((!is_error && !background).then_some(0));
        summarize(Some(rest), exit)
    });
    ExecEvent::ToolResult {
        id: b.tool_use_id.clone(),
        ok: !is_error,
        summary,
        output: None,
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

/// Per-run normalization state.
///
/// `saw_init` dedupes `system:init`: the CLI re-emits it every time the model is re-invoked
/// within one session (a background-subagent wake turn, a scheduled wakeup), not just at start
/// (live-verified against 2.1.228). One run is one `Started`.
///
/// Frames tagged with a `parent_tool_use_id` are a subagent's interior. A background subagent
/// runs concurrently with the run's own stream, so folding them in would split a contiguous
/// text block around phantom tool calls; `ExecEvent` has no place for them (ARCHITECTURE
/// section 5.1), and the spawning tool call and its result on the main feed stay.
pub(crate) struct Normalizer {
    saw_init: bool,
    /// Bash calls not yet answered, tool_use id to whether it runs in the background: their
    /// results carry the command's exit code.
    command_calls: std::collections::HashMap<String, bool>,
    /// What this turn has already said went wrong (an assistant-level error code, a blocking
    /// rate limit, a retrying API). If the turn then fails, these are its diagnostic.
    notices: Vec<String>,
    /// A notice already says the failure that the result would repeat.
    failure_noticed: bool,
    /// This turn already said it is retrying the API.
    retry_noticed: bool,
}

impl Normalizer {
    pub(crate) fn new() -> Self {
        Self {
            saw_init: false,
            command_calls: std::collections::HashMap::new(),
            notices: Vec::new(),
            failure_noticed: false,
            retry_noticed: false,
        }
    }

    /// The CLI reported its session (`system:init`).
    pub(crate) fn started(&self) -> bool {
        self.saw_init
    }

    /// The diagnostic of a failing turn: what it noticed on the way, then `error`.
    fn diagnostic(&mut self, error: Option<String>) -> Option<String> {
        let mut parts = std::mem::take(&mut self.notices);
        parts.extend(error);
        (!parts.is_empty()).then(|| parts.join(" "))
    }

    /// The turn ended: what it noticed is no longer anyone's.
    fn end_turn(&mut self) {
        self.notices.clear();
        self.failure_noticed = false;
        self.retry_noticed = false;
    }

    /// Normalize one stdout frame into 0+ events. `interrupted` folds a post-interrupt `result`
    /// into `Done { stop: Interrupted }`.
    pub(crate) fn normalize(&mut self, frame: Frame, interrupted: bool) -> Vec<ExecEvent> {
        match frame {
            Frame::System(f) => {
                // The CLI retrying a failed API call waits between attempts with nothing on
                // the wire. Once the retries add up, remember why the turn is slow.
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
                        self.notices.push(format!(
                            "The Claude API {why} — still retrying (attempt {attempt}{of})."
                        ));
                    }
                    return Vec::new();
                }
                if f.subtype != "init" || self.saw_init {
                    return Vec::new();
                }
                self.saw_init = true;
                vec![ExecEvent::Started {
                    session: SessionRef(f.session_id),
                    model: f.model,
                }]
            }

            Frame::StreamEvent(f) => {
                if f.event.kind != "content_block_delta" || f.parent_tool_use_id.is_some() {
                    return Vec::new();
                }
                match f.event.delta.kind.as_str() {
                    "text_delta" if !f.event.delta.text.is_empty() => vec![ExecEvent::Text {
                        text: f.event.delta.text,
                    }],
                    "thinking_delta" if !f.event.delta.thinking.is_empty() => {
                        vec![ExecEvent::Reasoning {
                            text: f.event.delta.thinking,
                        }]
                    }
                    // Redacted thinking, tool input being generated, signatures: liveness
                    // noise a run consumer has no use for.
                    _ => Vec::new(),
                }
            }

            Frame::Assistant(f) => {
                if f.parent_tool_use_id.is_some() {
                    return Vec::new();
                }
                note_commands(&f, &mut self.command_calls);
                let out = f
                    .message
                    .blocks()
                    .filter(|b: &ContentBlock| b.kind == "tool_use")
                    .map(|b| {
                        let class = super::permission::action_class(&b.name);
                        ExecEvent::Tool {
                            id: b.id,
                            call: ToolCall::new(b.name, b.input, class),
                        }
                    })
                    .collect();
                // A failed turn (usage limit, billing, auth, overloaded, ...) carries a terse
                // `error` code here, often with empty content and no `result` error.
                if let Some(code) = &f.error {
                    self.failure_noticed = true;
                    self.notices.push(assistant_error_text(code));
                }
                out
            }

            Frame::User(f) => {
                if f.parent_tool_use_id.is_some() {
                    return Vec::new();
                }
                f.message
                    .blocks()
                    .filter(|b| b.kind == "tool_result")
                    .map(|b| tool_result(&b, &mut self.command_calls))
                    .collect()
            }

            // A claude.ai plan window was hit. A hard `rejected` blocks the turn; allowed and
            // allowed_warning stay quiet, and so does a reject the account's overage carries
            // past.
            Frame::RateLimit(f) => {
                if f.rate_limit_info.blocks() {
                    let window = rate_window_label(
                        f.rate_limit_info.rate_limit_type.as_deref().unwrap_or(""),
                    );
                    self.failure_noticed = true;
                    self.notices.push(format!(
                        "Claude {window} limit reached — the turn was blocked. Try again after it resets."
                    ));
                }
                Vec::new()
            }

            Frame::Result(f) => {
                let noticed = self.failure_noticed;
                // A result line that would not parse (see `parse_frame`): its turn still ends,
                // with what little the line told us.
                if f.unreadable {
                    tracing::warn!(target: "reno_harness::claude", "unreadable result frame");
                    let failed = f.is_error && !interrupted;
                    let done = if interrupted {
                        self.end_turn();
                        done(StopReason::Interrupted, None, None)
                    } else if failed {
                        let error = (!noticed)
                            .then(|| "Claude's result for this turn could not be read.".to_owned());
                        let diagnostic = self.diagnostic(error);
                        self.end_turn();
                        done(StopReason::Error, None, diagnostic)
                    } else {
                        self.end_turn();
                        done(StopReason::Completed, None, None)
                    };
                    return vec![done];
                }
                let usage = ExecEvent::Usage {
                    input_tokens: f.usage.input_tokens,
                    output_tokens: f.usage.output_tokens,
                    // The CLI's `input_tokens` leaves the cached input out: read from and
                    // written to the cache are separate counts, so nothing is counted twice.
                    // Only the result's usage is read, never a per-message one.
                    cache_read_tokens: f.usage.cache_read_input_tokens,
                    cache_write_tokens: f.usage.cache_creation_input_tokens,
                };
                let failure = (f.subtype == "success" && !interrupted && !aborted(&f))
                    .then(|| api_failure(&f))
                    .flatten();
                let done = if let Some(error) = failure {
                    let diagnostic = self.diagnostic((!noticed).then_some(error));
                    done(StopReason::Error, None, diagnostic)
                } else if f.subtype == "success" {
                    let stop = if interrupted || aborted(&f) {
                        StopReason::Interrupted
                    } else {
                        StopReason::Completed
                    };
                    done(stop, f.result, None)
                } else {
                    // Split the CLI's internal `[ede_diagnostic]` breadcrumbs off the real
                    // errors: diagnostics are debug-logged, never reported as the cause.
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
                        // Real user-relevant errors: verbatim.
                        Some(errors.join("; "))
                    } else {
                        match f.subtype.as_str() {
                            // Known run-failure subtypes keep their mapped human wording
                            // (never blank: a blank error reads as a silent non-reply).
                            "error_max_turns"
                            | "error_max_budget_usd"
                            | "error_max_structured_output_retries" => {
                                Some(result_error_text(&f.subtype).to_owned())
                            }
                            // Diagnostic-only ends (the CLI's turn-accounting telemetry,
                            // typically `error_during_execution` after an abort): nothing
                            // user-relevant to say.
                            _ if !diagnostics.is_empty() => None,
                            // The failure already has its notice; a generic line under it adds
                            // nothing.
                            _ if noticed => None,
                            _ => Some(result_error_text(&f.subtype).to_owned()),
                        }
                    };
                    if interrupted || aborted(&f) {
                        done(StopReason::Interrupted, None, None)
                    } else {
                        let diagnostic = self.diagnostic(error);
                        done(StopReason::Error, None, diagnostic)
                    }
                };
                self.end_turn();
                vec![usage, done]
            }

            // Control frames are handled by the run loop, not normalized.
            Frame::ControlRequest(_)
            | Frame::ControlResponse(_)
            | Frame::CommandLifecycle(_)
            | Frame::Other
            | Frame::Unknown(_) => Vec::new(),
        }
    }
}

fn done(stop: StopReason, result: Option<String>, diagnostic: Option<String>) -> ExecEvent {
    ExecEvent::Done {
        stop,
        result,
        diagnostic,
        reason: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reno_proto::{ActionClass, SessionRef, StopReason, ToolCall};
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
                ..
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
                    call: ToolCall::new("Bash", json!({"command": "ls -la"}), ActionClass::Command),
                },
                ExecEvent::Tool {
                    id: "t2".into(),
                    call: ToolCall::new(
                        "mcp__linear__search",
                        json!({"q": "bug"}),
                        ActionClass::McpTool,
                    ),
                },
                ExecEvent::Tool {
                    id: "t3".into(),
                    call: ToolCall::new("Mystery", json!(null), ActionClass::Other),
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
        let done = norm.normalize(
            parse(r#"{"type":"result","subtype":"error_during_execution","errors":[]}"#),
            false,
        );
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
                    output: None
                },
                ExecEvent::ToolResult {
                    id: "bad".into(),
                    ok: false,
                    summary: Some("exit code 2: make: *** [all] Error 1".into()),
                    output: None
                },
                ExecEvent::ToolResult {
                    id: "bg".into(),
                    ok: true,
                    summary: Some("Command running in background with ID: b1.".into()),
                    output: None
                },
                ExecEvent::ToolResult {
                    id: "rd".into(),
                    ok: true,
                    summary: None,
                    output: None
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
            [
                ExecEvent::ToolResult {
                    summary: Some(s), ..
                },
            ] => {
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
        let blocked = norm.normalize(
            parse(r#"{"type":"result","subtype":"error_during_execution","errors":[]}"#),
            false,
        );
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
            let done = norm.normalize(
                parse(r#"{"type":"result","subtype":"error_during_execution","errors":[]}"#),
                false,
            );
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
                    output_tokens: 4,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
                ExecEvent::Done {
                    stop: StopReason::Completed,
                    result: Some("ok".into()),
                    diagnostic: None,
                    reason: None,
                },
            ]
        );
    }

    #[test]
    fn a_real_shaped_result_counts_cache_reads_and_writes_apart_from_input() {
        // The shape of a long CLI run's result: `input_tokens` is only the uncached remainder.
        let events = normalize_one(
            r#"{"type":"result","subtype":"success","is_error":false,"result":"ok","session_id":"s","total_cost_usd":1.9,"usage":{"input_tokens":16,"cache_creation_input_tokens":48213,"cache_read_input_tokens":1270554,"output_tokens":9120,"server_tool_use":{"web_search_requests":0},"service_tier":"standard","cache_creation":{"ephemeral_1h_input_tokens":0,"ephemeral_5m_input_tokens":48213}},"modelUsage":{"claude-opus-5-5":{"inputTokens":16,"outputTokens":9120,"cacheReadInputTokens":1270554,"cacheCreationInputTokens":48213}}}"#,
        );
        assert_eq!(
            events[0],
            ExecEvent::Usage {
                input_tokens: 16,
                output_tokens: 9120,
                cache_read_tokens: 1_270_554,
                cache_write_tokens: 48_213,
            }
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
        let ok =
            normalize_one(r#"{"type":"result","subtype":"success","usage":{"input_tokens":"x"}}"#);
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
        assert!(
            diagnostic.contains("overloaded") && diagnostic.contains("3 of 10"),
            "{diagnostic}"
        );
    }

    #[test]
    fn the_summary_helper_leads_with_the_exit_code() {
        assert_eq!(
            summarize(Some("a\nb\n"), Some(0)).as_deref(),
            Some("exit code 0: b")
        );
        assert_eq!(summarize(None, Some(3)).as_deref(), Some("exit code 3"));
        assert_eq!(summarize(Some("  \n"), None), None);
    }
}
