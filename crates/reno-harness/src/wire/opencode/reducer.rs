//! The turn, as a state machine: bus payloads in, effects out.
//!
//! opencode's own ACP layer ends a prompt on the FIRST `session.status{idle}` it sees after
//! subscribing, uncorrelated with the prompt it sent, and ignores `session.error` and
//! `session.idle` altogether. This reducer ends a turn on the idle of THIS run's session, after
//! that session was seen busy (or retrying, or erroring), and then confirms it:
//!
//! - both idle encodings (`session.idle` and `session.status{idle}`) arrive for one completion,
//!   and the first may let the server start a queued prompt, so one idle asks for a check
//!   ([`Effect::CheckIdle`]: the driver reads the session's status after [`Reducer::poll_delay`])
//!   and only a second idle, or a status that says not running, ends the turn ([`Effect::Idle`]);
//! - busy is re-asserted at the top of every agent-loop iteration, so an idle after a busy is
//!   authoritative, not a lull;
//! - any other event of the session while it is active clears a pending idle: it was a lull.
//!
//! The reducer holds no clock: the driver owns the stall bound, the status poll's timer and every
//! request, and tells the reducer what happened ([`Reducer::prompt_accepted`],
//! [`Reducer::settled_by_status`]).
//!
//! Frames arrive in the 1.x vocabulary (`{type, properties}`): 2.x frames are rewritten first
//! ([`super::v2::Normalizer`]).
//!
//! # What becomes an [`ExecEvent`]
//!
//! | opencode | `ExecEvent` |
//! |---|---|
//! | `message.part.updated` text (an assistant message's) | `Text` (the part of it not yet sent) |
//! | `message.part.updated` reasoning | `Reasoning` |
//! | `message.part.delta` | `Text` or `Reasoning`, by the part's kind |
//! | tool part `running` or with an input | `Tool` (an MCP tool is named `mcp__<server>__<tool>`) |
//! | tool part `completed` / `error` | `ToolResult` |
//! | `message.updated` of a completed assistant message | `Usage` once per message, `ContextUsage` |
//! | `session.status{retry}` from attempt 3, `session.error`, `session.warning` | `Text`, a bracketed note |
//!
//! There is no event for a failure: it ends the run in [`Reducer::ending`], and a note in the
//! transcript says it happened. A subagent's own traffic is not shown (the `task` tool call is);
//! its permission requests and questions are still put to the person, since the run waits on them.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use reno_proto::{ExecEvent, StopReason, ToolCall, ToolOutput};
use serde_json::Value;

use super::known;
use super::permission::{self, Asked, Permission};

/// Provider retries from this attempt on are reported.
pub(crate) const RETRY_REPORT_ATTEMPT: u64 = 3;
/// Provider retries from this attempt on end the turn: opencode itself would retry for ever.
pub(crate) const RETRY_ABORT_ATTEMPT: u64 = 8;

/// What a wedged or silent run usually means for opencode.
pub(crate) const STALL_HINT: &str = "The model provider is likely unreachable or rejecting \
     requests. Check the model and provider setup (`opencode auth list`, opencode.json) or the \
     opencode log (~/.local/share/opencode/log).";

/// The first wait before the status route is read after an idle; it doubles to [`POLL_MAX`].
const POLL_FIRST: Duration = Duration::from_millis(100);
const POLL_MAX: Duration = Duration::from_secs(2);

/// What the driver is to do.
#[derive(Debug, PartialEq)]
pub(crate) enum Effect {
    /// Say it to the run's consumer.
    Event(ExecEvent),
    /// Ask a person to approve; reply `once` or `reject`.
    Permission(Permission),
    /// Put questions to a person.
    Question(Asked),
    /// Abort the session now: the provider has failed too often.
    Abort,
    /// An idle was seen: read the session's status after [`Reducer::poll_delay`].
    CheckIdle,
    /// The turn is over.
    Idle,
    /// The session was interrupted by something other than this run.
    Interrupted,
}

/// How a run ended, from what its turns said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Ending {
    pub(crate) stop: StopReason,
    pub(crate) result: Option<String>,
    pub(crate) diagnostic: Option<String>,
}

#[derive(Default)]
struct PartState {
    kind: &'static str,
    /// Bytes of the part's text already sent (a snapshot resends the whole text).
    emitted: usize,
    tool_started: bool,
    tool_done: bool,
}

struct Turn {
    /// A prompt is in flight, busy expected, idle settles it.
    active: bool,
    /// An idle belongs to this prompt only after its busy or retry, or an error, or an abort: a
    /// trailing idle of the previous turn must not settle a just-posted prompt.
    idle_ready: bool,
    idle_confirmations: u8,
    saw_activity: bool,
    saw_content: bool,
    error: Option<String>,
    retry_reported: bool,
    aborted_for_retry: bool,
    /// The prompt's POST has not been answered: not running proves nothing yet.
    prompt_in_flight: bool,
    poll: Duration,
}

impl Turn {
    fn new(prompt_in_flight: bool) -> Self {
        Self {
            active: true,
            idle_ready: false,
            idle_confirmations: 0,
            saw_activity: false,
            saw_content: false,
            error: None,
            retry_reported: false,
            aborted_for_retry: false,
            prompt_in_flight,
            poll: POLL_FIRST,
        }
    }
}

/// One run's reducer. See the module docs.
pub(crate) struct Reducer {
    session: String,
    mcp_servers: Vec<String>,
    windows: HashMap<String, u64>,
    /// message id to whether it is an assistant's (a user prompt's echo must not render).
    messages: HashMap<String, bool>,
    /// Parts whose message role is not known yet, replayed when it lands.
    held_parts: Vec<Value>,
    parts: HashMap<String, PartState>,
    /// Sessions of subagents of this run.
    children: HashSet<String>,
    /// Permission and question ids already handled: a reconnect's sweep lists them again.
    seen_requests: HashSet<String>,
    usage_seen: HashSet<String>,
    last_context: Option<(Option<u64>, Option<u64>)>,
    unknown: Vec<String>,
    /// The assistant message the result text belongs to, and the text so far.
    result_message: String,
    result_text: String,
    turn: Turn,
    /// After an abort: only the session's end is read.
    aborting: bool,
}

impl Reducer {
    /// A reducer for `session`. `mcp_servers` are the names of the run's MCP servers;
    /// `windows` the context window of each `provider/model`.
    pub(crate) fn new(
        session: &str,
        mcp_servers: Vec<String>,
        windows: HashMap<String, u64>,
    ) -> Self {
        Self {
            session: session.to_owned(),
            mcp_servers,
            windows,
            messages: HashMap::new(),
            held_parts: Vec::new(),
            parts: HashMap::new(),
            children: HashSet::new(),
            seen_requests: HashSet::new(),
            usage_seen: HashSet::new(),
            last_context: None,
            unknown: Vec::new(),
            result_message: String::new(),
            result_text: String::new(),
            turn: Turn::new(false),
            aborting: false,
        }
    }

    /// A new turn begins (a prompt is being posted). `prompt_in_flight`: its POST's answer will be
    /// reported through [`Self::prompt_accepted`].
    pub(crate) fn begin_turn(&mut self, prompt_in_flight: bool) {
        self.turn = Turn::new(prompt_in_flight);
        self.aborting = false;
    }

    /// The server answered the prompt's POST.
    pub(crate) fn prompt_accepted(&mut self) {
        self.turn.prompt_in_flight = false;
    }

    /// The prompt's POST failed, but the bus already shows the turn running: the server has it.
    pub(crate) fn running_anyway(&self) -> bool {
        self.turn.idle_ready
    }

    /// Whether the session has sent any event since the prompt: the stall bound's question.
    pub(crate) fn saw_activity(&self) -> bool {
        self.turn.saw_activity
    }

    /// Whether the prompt's POST is still unanswered.
    pub(crate) fn prompt_in_flight(&self) -> bool {
        self.turn.prompt_in_flight
    }

    /// Whether a turn is running.
    pub(crate) fn active(&self) -> bool {
        self.turn.active
    }

    /// An abort was sent: from here only the session's end is read, and the first idle ends the
    /// turn without a confirmation.
    pub(crate) fn abort_sent(&mut self) {
        self.aborting = true;
        self.turn.idle_ready = true;
    }

    /// How long to wait before reading the status route after [`Effect::CheckIdle`].
    pub(crate) fn poll_delay(&self) -> Duration {
        self.turn.poll
    }

    /// The status route was read: the session is (not) running. A session that is not running
    /// ends the turn, unless the prompt has not been answered yet; one that is, lengthens the wait.
    pub(crate) fn settled_by_status(&mut self, running: bool) -> Option<Effect> {
        if !self.turn.active {
            return None;
        }
        if running || self.turn.prompt_in_flight {
            self.turn.poll = (self.turn.poll * 2).min(POLL_MAX);
            return None;
        }
        Some(self.settle())
    }

    /// The bus came back after a gap: any idle it swallowed is read from the status route, and
    /// requests asked in the gap are re-listed by the driver.
    pub(crate) fn reconnected(&self) -> bool {
        self.turn.active && !self.turn.prompt_in_flight
    }

    /// Frame kinds met that the driver does not know, since the last call.
    pub(crate) fn take_unknown(&mut self) -> Vec<String> {
        std::mem::take(&mut self.unknown)
    }

    /// The subagent sessions seen, to abort with the run.
    pub(crate) fn children(&self) -> Vec<String> {
        self.children.iter().cloned().collect()
    }

    /// Every session whose requests the run waits on: its own and its subagents'.
    pub(crate) fn owned_sessions(&self) -> Vec<String> {
        std::iter::once(self.session.clone())
            .chain(self.children.iter().cloned())
            .collect()
    }

    /// How the run ended, so far.
    pub(crate) fn ending(&self, interrupted: bool) -> Ending {
        if interrupted {
            return Ending {
                stop: StopReason::Interrupted,
                result: None,
                diagnostic: None,
            };
        }
        let failed =
            self.turn.aborted_for_retry || (self.turn.error.is_some() && !self.turn.saw_content);
        if failed {
            return Ending {
                stop: StopReason::Error,
                result: None,
                diagnostic: self.turn.error.clone(),
            };
        }
        Ending {
            stop: StopReason::Completed,
            result: (!self.result_text.is_empty()).then(|| self.result_text.clone()),
            diagnostic: None,
        }
    }

    fn settle(&mut self) -> Effect {
        self.turn.active = false;
        Effect::Idle
    }

    fn owns(&self, session: &str) -> bool {
        session == self.session || self.children.contains(session)
    }

    /// One bus frame (1.x payload, or a 1.x frame with its `payload` wrapper).
    pub(crate) fn frame(&mut self, frame: &Value) -> Vec<Effect> {
        let payload = frame.get("payload").unwrap_or(frame);
        let kind = payload.get("type").and_then(Value::as_str).unwrap_or("");
        if kind == "sync" || kind.is_empty() {
            return Vec::new();
        }
        if !known::is_known(kind) {
            self.unknown.push(format!("type:{kind}"));
            return Vec::new();
        }
        let props = payload.get("properties").unwrap_or(&Value::Null);
        let event_session = props
            .get("sessionID")
            .and_then(Value::as_str)
            .or_else(|| {
                props
                    .get("info")
                    .and_then(|i| i.get("sessionID").or_else(|| i.get("id")))
                    .and_then(Value::as_str)
            })
            .or_else(|| {
                props
                    .get("part")
                    .and_then(|p| p.get("sessionID"))
                    .and_then(Value::as_str)
            });
        let ours = event_session == Some(self.session.as_str());
        let status = props.pointer("/status/type").and_then(Value::as_str);
        let idle = kind == "session.idle" || (kind == "session.status" && status == Some("idle"));

        if self.aborting {
            // Only the session's own end counts after an abort: late content and usage are dropped.
            return match (ours, kind) {
                (true, "session.interrupted") => vec![self.settle_interrupted()],
                (true, _) if idle => vec![self.settle()],
                _ => Vec::new(),
            };
        }
        if ours && kind == "session.interrupted" {
            return vec![self.settle_interrupted()];
        }
        if ours && idle {
            return self.idle();
        }
        if ours && self.turn.active {
            self.turn.idle_confirmations = 0;
            self.turn.poll = POLL_FIRST;
            self.turn.saw_activity = true;
        }
        match kind {
            "session.status" if ours => self.status(props),
            "session.error" | "session.warning" => {
                // A failure naming another session is not this run's; one naming none still is
                // (a provider failure of the whole server).
                if event_session.is_some() && !ours {
                    return Vec::new();
                }
                self.failure(kind, props)
            }
            "session.created" => {
                let info = props.get("info").unwrap_or(&Value::Null);
                if let (Some(id), Some(parent)) = (
                    info.get("id").and_then(Value::as_str),
                    info.get("parentID").and_then(Value::as_str),
                ) && self.owns(parent)
                {
                    self.children.insert(id.to_owned());
                }
                Vec::new()
            }
            "message.updated" => self.message_updated(props),
            "message.part.updated" if ours => {
                let part = props.get("part").unwrap_or(&Value::Null);
                self.part_snapshot(part)
            }
            "message.part.delta" if ours => self.part_delta(props),
            "permission.asked" => {
                if !event_session.is_some_and(|s| self.owns(s)) {
                    return Vec::new();
                }
                match permission::parse_permission(props, &self.mcp_servers) {
                    Some(asked) if self.seen_requests.insert(asked.id.clone()) => {
                        vec![Effect::Permission(asked)]
                    }
                    _ => Vec::new(),
                }
            }
            "question.asked" => {
                if !event_session.is_some_and(|s| self.owns(s)) {
                    return Vec::new();
                }
                match permission::parse_question(props) {
                    Some(asked) if self.seen_requests.insert(asked.id.clone()) => {
                        vec![Effect::Question(asked)]
                    }
                    _ => Vec::new(),
                }
            }
            _ => Vec::new(),
        }
    }

    fn settle_interrupted(&mut self) -> Effect {
        self.turn.active = false;
        Effect::Interrupted
    }

    fn idle(&mut self) -> Vec<Effect> {
        if !self.turn.active {
            return Vec::new();
        }
        // OpenCode can emit both idle encodings for one completion, and the first may start a
        // queued prompt before the second is read: until that prompt starts (or fails, or is
        // aborted) the second is stale.
        if self.turn.idle_ready || self.turn.error.is_some() {
            self.turn.idle_confirmations += 1;
            if self.turn.idle_confirmations >= 2 {
                return vec![self.settle()];
            }
        }
        vec![Effect::CheckIdle]
    }

    fn status(&mut self, props: &Value) -> Vec<Effect> {
        let status = props.get("status").unwrap_or(&Value::Null);
        match status.get("type").and_then(Value::as_str) {
            Some("busy") => {
                self.turn.idle_ready = true;
                Vec::new()
            }
            Some("retry") => {
                self.turn.idle_ready = true;
                let attempt = status.get("attempt").and_then(Value::as_u64).unwrap_or(0);
                let message = status
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("provider error");
                if attempt >= RETRY_ABORT_ATTEMPT && !self.turn.aborted_for_retry {
                    self.turn.aborted_for_retry = true;
                    self.turn.error = Some(format!(
                        "the provider kept failing after {attempt} attempts: {message}"
                    ));
                    vec![
                        note(format!(
                            "Giving up after {attempt} provider retries: {message}. {STALL_HINT}"
                        )),
                        Effect::Abort,
                    ]
                } else if attempt >= RETRY_REPORT_ATTEMPT && !self.turn.retry_reported {
                    self.turn.retry_reported = true;
                    vec![note(format!(
                        "The provider is failing and opencode is retrying (attempt {attempt}): {message}"
                    ))]
                } else {
                    Vec::new()
                }
            }
            _ => Vec::new(),
        }
    }

    fn failure(&mut self, kind: &str, props: &Value) -> Vec<Effect> {
        let error = props.get("error").unwrap_or(&Value::Null);
        let name = error.get("name").and_then(Value::as_str).unwrap_or("");
        if name == "MessageAbortedError" {
            // The abort's echo, not a failure.
            return Vec::new();
        }
        let message = error
            .pointer("/data/message")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| {
                if name.is_empty() {
                    "opencode reported an error".to_owned()
                } else {
                    name.to_owned()
                }
            });
        // opencode reports the same failure twice (once bare, once wrapped with an exception
        // name and a stack): one note per failure, whichever first line contains the other.
        let first = |m: &str| m.lines().next().unwrap_or(m).trim().to_owned();
        let line = first(&message);
        let duplicate = self.turn.error.as_deref().is_some_and(|previous| {
            let previous = first(previous);
            !line.is_empty() && (previous.contains(&line) || line.contains(&previous))
        });
        if kind == "session.error" {
            self.turn.error = Some(message.clone());
        }
        if duplicate {
            Vec::new()
        } else {
            vec![note(format!("opencode: {message}"))]
        }
    }

    fn message_updated(&mut self, props: &Value) -> Vec<Effect> {
        let info = props.get("info").unwrap_or(&Value::Null);
        let (Some(session), Some(message), Some(role)) = (
            info.get("sessionID").and_then(Value::as_str),
            info.get("id").and_then(Value::as_str),
            info.get("role").and_then(Value::as_str),
        ) else {
            return Vec::new();
        };
        if session != self.session {
            return Vec::new();
        }
        let assistant = role == "assistant";
        self.messages.entry(message.to_owned()).or_insert(assistant);
        let mut effects = Vec::new();
        if assistant && let Some(tokens) = info.get("tokens") {
            if let Some(context) = self.context_usage(info) {
                effects.push(Effect::Event(context));
            }
            if info.pointer("/time/completed").is_some()
                && self.usage_seen.insert(message.to_owned())
            {
                let count = |value: Option<&Value>| value.and_then(Value::as_u64).unwrap_or(0);
                let (input, output) = (count(tokens.get("input")), count(tokens.get("output")));
                let reasoning = count(tokens.get("reasoning"));
                let (read, write) = (
                    count(tokens.pointer("/cache/read")),
                    count(tokens.pointer("/cache/write")),
                );
                if input + output + reasoning + read + write > 0 {
                    effects.push(Effect::Event(ExecEvent::Usage {
                        input_tokens: input,
                        output_tokens: output + reasoning,
                        cache_read_tokens: read,
                        cache_write_tokens: write,
                    }));
                }
            }
        }
        // Parts that raced ahead of their message's role.
        let (ready, rest): (Vec<Value>, Vec<Value>) = std::mem::take(&mut self.held_parts)
            .into_iter()
            .partition(|part| part.get("messageID").and_then(Value::as_str) == Some(message));
        self.held_parts = rest;
        for part in ready {
            effects.extend(self.part_snapshot(&part));
        }
        effects
    }

    /// The context's occupancy and window (the same total zeron reported: the step's tokens
    /// including the cache), once per change. A placeholder with zero counters, before the
    /// request ran, says nothing.
    fn context_usage(&mut self, info: &Value) -> Option<ExecEvent> {
        let tokens = info.get("tokens")?;
        let counts: Vec<u64> = [
            tokens.get("input"),
            tokens.get("output"),
            tokens.pointer("/cache/read"),
            tokens.pointer("/cache/write"),
        ]
        .into_iter()
        .flatten()
        .filter_map(Value::as_u64)
        .collect();
        let total = tokens
            .get("total")
            .and_then(Value::as_u64)
            .filter(|n| *n > 0)
            .or_else(|| {
                (!counts.is_empty()).then(|| counts.into_iter().fold(0u64, u64::saturating_add))
            });
        if total == Some(0) && info.pointer("/time/completed").is_none() {
            return None;
        }
        let window = info
            .get("providerID")
            .and_then(Value::as_str)
            .zip(info.get("modelID").and_then(Value::as_str))
            .and_then(|(provider, model)| {
                self.windows.get(&format!("{provider}/{model}")).copied()
            });
        if total.is_none() && window.is_none() {
            return None;
        }
        if self.last_context == Some((total, window)) {
            return None;
        }
        self.last_context = Some((total, window));
        Some(ExecEvent::ContextUsage {
            tokens: total,
            window,
        })
    }

    fn part_snapshot(&mut self, part: &Value) -> Vec<Effect> {
        let Some(part_id) = part.get("id").and_then(Value::as_str) else {
            return Vec::new();
        };
        let message = part
            .get("messageID")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let kind = part.get("type").and_then(Value::as_str).unwrap_or_default();
        match kind {
            "text" | "reasoning" => {
                match self.messages.get(message) {
                    // Role unknown: hold the part (snapshots re-deliver; dedup by part id).
                    None => {
                        self.hold(part, part_id);
                        return Vec::new();
                    }
                    // A user message's text is our own prompt echoed.
                    Some(false) => return Vec::new(),
                    Some(true) => {}
                }
                let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                let kind: &'static str = if kind == "text" { "text" } else { "reasoning" };
                let state = self
                    .parts
                    .entry(part_id.to_owned())
                    .or_insert_with(|| PartState {
                        kind,
                        ..PartState::default()
                    });
                // A shorter (or mid-character) snapshot is a rewrite this decoder does not
                // model: dropped rather than duplicated.
                let Some(suffix) = text
                    .get(state.emitted..)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                else {
                    return Vec::new();
                };
                state.emitted = text.len();
                let state_kind = state.kind;
                self.content(message, state_kind, suffix)
            }
            "tool" => self.tool_part(part, part_id),
            // step-start, step-finish, snapshot and patch bookkeeping are not transcript.
            _ => Vec::new(),
        }
    }

    fn hold(&mut self, part: &Value, part_id: &str) {
        if !self
            .held_parts
            .iter()
            .any(|p| p.get("id").and_then(Value::as_str) == Some(part_id))
        {
            self.held_parts.push(part.clone());
        }
    }

    fn part_delta(&mut self, props: &Value) -> Vec<Effect> {
        let (Some(part_id), Some(delta)) = (
            props.get("partID").and_then(Value::as_str),
            props.get("delta").and_then(Value::as_str),
        ) else {
            return Vec::new();
        };
        if props.get("field").and_then(Value::as_str) != Some("text") || delta.is_empty() {
            return Vec::new();
        }
        let message = props
            .get("messageID")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if self.messages.get(message) != Some(&true) {
            return Vec::new();
        }
        let state = self
            .parts
            .entry(part_id.to_owned())
            .or_insert_with(|| PartState {
                kind: "text",
                ..PartState::default()
            });
        if state.kind == "tool" {
            return Vec::new();
        }
        state.emitted += delta.len();
        let kind = state.kind;
        self.content(message, kind, delta.to_owned())
    }

    /// Text or reasoning for the consumer; text also becomes the run's result when it is the
    /// last assistant message's.
    fn content(&mut self, message: &str, kind: &str, text: String) -> Vec<Effect> {
        self.turn.saw_content = true;
        if kind == "reasoning" {
            return vec![Effect::Event(ExecEvent::Reasoning { text })];
        }
        if self.result_message != message {
            message.clone_into(&mut self.result_message);
            self.result_text.clear();
        }
        self.result_text.push_str(&text);
        vec![Effect::Event(ExecEvent::Text { text })]
    }

    fn tool_part(&mut self, part: &Value, part_id: &str) -> Vec<Effect> {
        let tool = part.get("tool").and_then(Value::as_str).unwrap_or_default();
        let status = part
            .pointer("/state/status")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let input = part.pointer("/state/input").cloned().unwrap_or(Value::Null);
        let call_id = part
            .get("callID")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(part_id)
            .to_owned();
        let state = self
            .parts
            .entry(part_id.to_owned())
            .or_insert_with(|| PartState {
                kind: "tool",
                ..PartState::default()
            });
        let mut effects = Vec::new();
        let has_input = input.as_object().is_some_and(|o| !o.is_empty());
        // `running` means the input is final, a tool with no arguments included.
        if !state.tool_started && (has_input || matches!(status, "running" | "completed" | "error"))
        {
            state.tool_started = true;
            self.turn.saw_content = true;
            let (name, class) = permission::canonical(tool, &self.mcp_servers);
            effects.push(Effect::Event(ExecEvent::Tool {
                id: call_id.clone(),
                call: ToolCall::new(name, input, class),
            }));
        }
        let state = self.parts.entry(part_id.to_owned()).or_default();
        if state.tool_started && !state.tool_done && matches!(status, "completed" | "error") {
            state.tool_done = true;
            let ok = status == "completed";
            let text = part
                .pointer("/state/output")
                .or_else(|| part.pointer("/state/error"))
                .and_then(Value::as_str)
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
            effects.push(Effect::Event(ExecEvent::ToolResult {
                id: call_id,
                ok,
                summary,
                output: ToolOutput::from_raw_opt(text),
            }));
        }
        effects
    }
}

fn note(text: String) -> Effect {
    Effect::Event(ExecEvent::Text {
        text: format!("\n[{text}]\n"),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use serde_json::json;

    use super::*;

    fn reducer() -> Reducer {
        let mut r = Reducer::new(
            "ses_a",
            vec!["reno".into()],
            HashMap::from([("opencode/big-pickle".to_owned(), 200_000)]),
        );
        r.begin_turn(false);
        r
    }

    fn frame(kind: &str, props: Value) -> Value {
        json!({"type": kind, "properties": props})
    }

    fn busy() -> Value {
        frame(
            "session.status",
            json!({"sessionID": "ses_a", "status": {"type": "busy"}}),
        )
    }

    fn idle_status() -> Value {
        frame(
            "session.status",
            json!({"sessionID": "ses_a", "status": {"type": "idle"}}),
        )
    }

    fn idle_event() -> Value {
        frame("session.idle", json!({"sessionID": "ses_a"}))
    }

    fn assistant(id: &str) -> Value {
        frame(
            "message.updated",
            json!({"info": {"sessionID": "ses_a", "id": id, "role": "assistant"}}),
        )
    }

    fn text_part(id: &str, message: &str, text: &str) -> Value {
        frame(
            "message.part.updated",
            json!({"part": {"sessionID": "ses_a", "id": id, "messageID": message, "type": "text", "text": text}}),
        )
    }

    fn events(effects: Vec<Effect>) -> Vec<ExecEvent> {
        effects
            .into_iter()
            .filter_map(|e| match e {
                Effect::Event(event) => Some(event),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_idle_before_the_session_was_busy_is_a_trailing_one_and_ends_nothing() {
        let mut r = reducer();
        // Two stale idles of the previous turn: never enough without a busy.
        assert_eq!(r.frame(&idle_status()), [Effect::CheckIdle]);
        assert_eq!(r.frame(&idle_event()), [Effect::CheckIdle]);
        assert!(r.active());
        r.frame(&busy());
        assert_eq!(r.frame(&idle_status()), [Effect::CheckIdle]);
        assert_eq!(r.frame(&idle_event()), [Effect::Idle]);
        assert!(!r.active());
    }

    #[test]
    fn another_event_after_an_idle_clears_it_as_a_lull() {
        let mut r = reducer();
        r.frame(&busy());
        assert_eq!(r.frame(&idle_status()), [Effect::CheckIdle]);
        r.frame(&busy());
        // The first idle was forgotten: this one is a first again.
        assert_eq!(r.frame(&idle_event()), [Effect::CheckIdle]);
    }

    #[test]
    fn a_status_that_says_not_running_ends_the_turn_once_the_prompt_is_answered() {
        let mut r = reducer();
        r.begin_turn(true);
        r.frame(&busy());
        r.frame(&idle_status());
        assert_eq!(
            r.settled_by_status(false),
            None,
            "the prompt's POST is unanswered"
        );
        let first = r.poll_delay();
        r.prompt_accepted();
        assert_eq!(r.settled_by_status(true), None);
        assert!(
            r.poll_delay() > first,
            "a running session lengthens the wait"
        );
        assert_eq!(r.settled_by_status(false), Some(Effect::Idle));
        assert_eq!(r.settled_by_status(false), None, "the turn is over");
    }

    #[test]
    fn the_poll_wait_doubles_to_two_seconds() {
        let mut r = reducer();
        for _ in 0..8 {
            r.settled_by_status(true);
        }
        assert_eq!(r.poll_delay(), Duration::from_secs(2));
    }

    #[test]
    fn a_session_error_makes_one_idle_enough_to_check_and_two_to_end() {
        let mut r = reducer();
        let out = r.frame(&frame(
            "session.error",
            json!({"sessionID": "ses_a", "error": {"name": "APIError", "data": {"message": "bad key"}}}),
        ));
        assert_eq!(
            events(out),
            [ExecEvent::Text {
                text: "\n[opencode: bad key]\n".into()
            }]
        );
        assert_eq!(r.frame(&idle_status()), [Effect::CheckIdle]);
        assert_eq!(r.frame(&idle_event()), [Effect::Idle]);
        let ending = r.ending(false);
        assert_eq!(ending.stop, StopReason::Error);
        assert_eq!(ending.diagnostic.as_deref(), Some("bad key"));
    }

    #[test]
    fn the_same_failure_reported_twice_is_one_note() {
        let mut r = reducer();
        let error = |message: &str| {
            frame(
                "session.error",
                json!({"sessionID": "ses_a", "error": {"name": "E", "data": {"message": message}}}),
            )
        };
        assert_eq!(events(r.frame(&error("boom"))).len(), 1);
        assert!(events(r.frame(&error("UnknownError: boom\n  at stack"))).is_empty());
        assert!(events(r.frame(&error("boom"))).is_empty());
        assert_eq!(events(r.frame(&error("a different failure"))).len(), 1);
    }

    #[test]
    fn an_abort_echo_is_not_an_error_and_another_sessions_error_is_not_ours() {
        let mut r = reducer();
        assert!(
            r.frame(&frame(
                "session.error",
                json!({"sessionID": "ses_a", "error": {"name": "MessageAbortedError"}})
            ))
            .is_empty()
        );
        assert!(
            r.frame(&frame(
                "session.error",
                json!({"sessionID": "ses_other", "error": {"data": {"message": "x"}}})
            ))
            .is_empty()
        );
        assert_eq!(r.ending(false).stop, StopReason::Completed);
    }

    #[test]
    fn an_error_after_output_still_completes_and_one_before_any_does_not() {
        let mut r = reducer();
        r.frame(&assistant("msg_1"));
        r.frame(&text_part("prt_1", "msg_1", "partial"));
        r.frame(&frame(
            "session.error",
            json!({"sessionID": "ses_a", "error": {"data": {"message": "late"}}}),
        ));
        assert_eq!(r.ending(false).stop, StopReason::Completed);
        assert_eq!(r.ending(false).result.as_deref(), Some("partial"));
        assert_eq!(r.ending(true).stop, StopReason::Interrupted);
    }

    #[test]
    fn retries_are_reported_from_the_third_and_end_the_turn_at_the_eighth() {
        let mut r = reducer();
        let retry = |attempt: u64| {
            frame(
                "session.status",
                json!({"sessionID": "ses_a", "status": {"type": "retry", "attempt": attempt, "message": "overloaded"}}),
            )
        };
        assert!(r.frame(&retry(1)).is_empty());
        assert!(r.frame(&retry(2)).is_empty());
        assert_eq!(events(r.frame(&retry(3))).len(), 1);
        assert!(r.frame(&retry(4)).is_empty(), "reported once");
        let out = r.frame(&retry(8));
        assert!(out.contains(&Effect::Abort));
        assert!(
            matches!(&events(out)[0], ExecEvent::Text { text } if text.contains("Giving up after 8"))
        );
        assert!(r.frame(&retry(9)).is_empty(), "aborted once");
        let ending = r.ending(false);
        assert_eq!(ending.stop, StopReason::Error);
        assert!(ending.diagnostic.unwrap().contains("after 8 attempts"));
    }

    #[test]
    fn after_an_abort_only_the_sessions_end_counts() {
        let mut r = reducer();
        r.frame(&busy());
        r.abort_sent();
        assert!(r.frame(&text_part("p", "m", "late")).is_empty());
        assert!(r.frame(&idle_event_for("ses_other")).is_empty());
        assert_eq!(r.frame(&idle_status()), [Effect::Idle]);
        let mut r = reducer();
        r.abort_sent();
        assert_eq!(
            r.frame(&frame("session.interrupted", json!({"sessionID": "ses_a"}))),
            [Effect::Interrupted]
        );
    }

    fn idle_event_for(session: &str) -> Value {
        frame("session.idle", json!({"sessionID": session}))
    }

    #[test]
    fn a_session_interrupted_by_someone_else_is_an_interruption() {
        let mut r = reducer();
        assert_eq!(
            r.frame(&frame("session.interrupted", json!({"sessionID": "ses_a"}))),
            [Effect::Interrupted]
        );
        assert!(!r.active());
    }

    #[test]
    fn a_snapshot_and_a_delta_never_repeat_text_and_a_user_echo_never_renders() {
        let mut r = reducer();
        // The user's own prompt, echoed, and a part that raced ahead of its message.
        r.frame(&frame(
            "message.updated",
            json!({"info": {"sessionID": "ses_a", "id": "msg_u", "role": "user"}}),
        ));
        assert!(
            r.frame(&text_part("prt_u", "msg_u", "my prompt"))
                .is_empty()
        );
        assert!(r.frame(&text_part("prt_1", "msg_1", "Hel")).is_empty());
        let out = events(r.frame(&assistant("msg_1")));
        assert_eq!(out, [ExecEvent::Text { text: "Hel".into() }]);
        let delta = |d: &str| {
            frame(
                "message.part.delta",
                json!({"sessionID": "ses_a", "messageID": "msg_1", "partID": "prt_1", "field": "text", "delta": d}),
            )
        };
        assert_eq!(
            events(r.frame(&delta("lo "))),
            [ExecEvent::Text { text: "lo ".into() }]
        );
        // The snapshot after the deltas sends only what it adds.
        let out = events(r.frame(&text_part("prt_1", "msg_1", "Hello world")));
        assert_eq!(
            out,
            [ExecEvent::Text {
                text: "world".into()
            }]
        );
        assert!(
            r.frame(&text_part("prt_1", "msg_1", "Hello world"))
                .is_empty()
        );
        assert_eq!(r.ending(false).result.as_deref(), Some("Hello world"));
    }

    #[test]
    fn the_result_is_the_last_assistant_messages_text() {
        let mut r = reducer();
        r.frame(&assistant("msg_1"));
        r.frame(&text_part("prt_1", "msg_1", "first"));
        r.frame(&assistant("msg_2"));
        r.frame(&text_part("prt_2", "msg_2", "second"));
        assert_eq!(r.ending(false).result.as_deref(), Some("second"));
    }

    #[test]
    fn reasoning_streams_as_reasoning() {
        let mut r = reducer();
        r.frame(&assistant("msg_1"));
        let out = events(r.frame(&frame(
            "message.part.updated",
            json!({"part": {"sessionID": "ses_a", "id": "prt_r", "messageID": "msg_1", "type": "reasoning", "text": "hmm"}}),
        )));
        assert_eq!(out, [ExecEvent::Reasoning { text: "hmm".into() }]);
        assert_eq!(r.ending(false).result, None, "reasoning is not the result");
    }

    fn tool(status: &str, input: Value, output: Option<&str>) -> Value {
        let mut state = json!({"status": status, "input": input});
        if let Some(output) = output {
            state[if status == "error" { "error" } else { "output" }] = json!(output);
        }
        frame(
            "message.part.updated",
            json!({"part": {"sessionID": "ses_a", "id": "prt_t", "callID": "call_1", "messageID": "msg_1", "type": "tool", "tool": "bash", "state": state}}),
        )
    }

    #[test]
    fn a_tool_opens_once_and_closes_with_its_output() {
        let mut r = reducer();
        let open = events(r.frame(&tool("running", json!({"command": "ls"}), None)));
        match &open[..] {
            [ExecEvent::Tool { id, call }] => {
                assert_eq!(id, "call_1");
                assert_eq!(call.name, "bash");
                assert_eq!(call.class, reno_proto::ActionClass::Command);
                assert_eq!(call.input["command"], "ls");
            }
            other => panic!("{other:?}"),
        }
        assert!(
            r.frame(&tool("running", json!({"command": "ls"}), None))
                .is_empty()
        );
        let done = events(r.frame(&tool("completed", json!({"command": "ls"}), Some("a\nb"))));
        match &done[..] {
            [
                ExecEvent::ToolResult {
                    id,
                    ok,
                    summary,
                    output,
                },
            ] => {
                assert_eq!(id, "call_1");
                assert!(ok);
                assert!(summary.is_none());
                assert_eq!(output.as_ref().unwrap().text, "a\nb");
            }
            other => panic!("{other:?}"),
        }
        assert!(
            r.frame(&tool("completed", json!({"command": "ls"}), Some("a\nb")))
                .is_empty()
        );
    }

    #[test]
    fn a_failed_tool_summarises_its_first_line_and_a_pending_one_with_no_input_waits() {
        let mut r = reducer();
        assert!(r.frame(&tool("pending", json!({}), None)).is_empty());
        r.frame(&tool("running", json!({}), None));
        let done = events(r.frame(&tool("error", json!({}), Some("\nboom: it broke\nmore"))));
        assert!(
            matches!(&done[0], ExecEvent::ToolResult { ok: false, summary: Some(s), .. } if s == "boom: it broke")
        );
    }

    #[test]
    fn an_mcp_tool_is_named_the_way_the_kernel_knows_it() {
        let mut r = reducer();
        let out = events(r.frame(&frame(
            "message.part.updated",
            json!({"part": {"sessionID": "ses_a", "id": "p", "callID": "c", "messageID": "m", "type": "tool", "tool": "reno_report", "state": {"status": "running", "input": {"status": "done"}}}}),
        )));
        match &out[0] {
            ExecEvent::Tool { call, .. } => {
                assert_eq!(call.name, "mcp__reno__report");
                assert_eq!(call.class, reno_proto::ActionClass::McpTool);
            }
            other => panic!("{other:?}"),
        }
    }

    fn tokens_message(id: &str, completed: bool, tokens: Value) -> Value {
        let mut info = json!({"sessionID": "ses_a", "id": id, "role": "assistant", "tokens": tokens, "providerID": "opencode", "modelID": "big-pickle"});
        if completed {
            info["time"] = json!({"completed": 1});
        }
        frame("message.updated", json!({"info": info}))
    }

    #[test]
    fn usage_is_one_report_per_completed_message_with_the_cache_split_out() {
        let mut r = reducer();
        let tokens =
            json!({"input": 10, "output": 20, "reasoning": 5, "cache": {"read": 7, "write": 3}});
        // An unfinished message reports context only.
        let live = events(r.frame(&tokens_message("msg_1", false, tokens.clone())));
        assert_eq!(
            live,
            [ExecEvent::ContextUsage {
                tokens: Some(40),
                window: Some(200_000)
            }]
        );
        let done = events(r.frame(&tokens_message("msg_1", true, tokens.clone())));
        assert_eq!(
            done,
            [ExecEvent::Usage {
                input_tokens: 10,
                output_tokens: 25,
                cache_read_tokens: 7,
                cache_write_tokens: 3
            }]
        );
        // The same completed message again adds nothing, and nor does an unchanged context.
        assert!(events(r.frame(&tokens_message("msg_1", true, tokens))).is_empty());
        let second = events(r.frame(&tokens_message(
            "msg_2",
            true,
            json!({"input": 1, "output": 1}),
        )));
        assert!(second.iter().any(|e| matches!(
            e,
            ExecEvent::Usage {
                input_tokens: 1,
                ..
            }
        )));
    }

    #[test]
    fn a_placeholder_with_zero_counters_says_nothing() {
        let mut r = reducer();
        assert!(
            events(r.frame(&tokens_message(
                "msg_1",
                false,
                json!({"input": 0, "output": 0})
            )))
            .is_empty()
        );
    }

    #[test]
    fn a_permission_of_the_run_or_a_subagent_is_asked_once_and_a_strangers_is_not() {
        let mut r = reducer();
        r.frame(&frame(
            "session.created",
            json!({"info": {"id": "ses_child", "parentID": "ses_a"}}),
        ));
        r.frame(&frame(
            "session.created",
            json!({"info": {"id": "ses_grand", "parentID": "ses_child"}}),
        ));
        r.frame(&frame(
            "session.created",
            json!({"info": {"id": "ses_x", "parentID": "ses_elsewhere"}}),
        ));
        let ask = |id: &str, session: &str| {
            frame(
                "permission.asked",
                json!({"id": id, "sessionID": session, "permission": "bash", "patterns": ["ls"]}),
            )
        };
        for session in ["ses_a", "ses_child", "ses_grand"] {
            let id = format!("per_{session}");
            let out = r.frame(&ask(&id, session));
            assert!(
                matches!(&out[..], [Effect::Permission(p)] if p.id == id && p.session_id == session),
                "{session}"
            );
            assert!(
                r.frame(&ask(&id, session)).is_empty(),
                "asked once: a reconnect lists it again"
            );
        }
        assert!(r.frame(&ask("per_x", "ses_x")).is_empty());
        assert!(r.frame(&ask("per_y", "ses_unknown")).is_empty());
        let mut owned = r.owned_sessions();
        owned.sort();
        assert_eq!(owned, ["ses_a", "ses_child", "ses_grand"]);
    }

    #[test]
    fn a_question_is_asked_once_for_the_run_and_its_subagents() {
        let mut r = reducer();
        let ask = frame(
            "question.asked",
            json!({"id": "que_1", "sessionID": "ses_a", "questions": [{"question": "Which?", "options": [{"label": "A"}]}]}),
        );
        assert!(matches!(&r.frame(&ask)[..], [Effect::Question(q)] if q.id == "que_1"));
        assert!(r.frame(&ask).is_empty());
        let stranger = frame(
            "question.asked",
            json!({"id": "que_2", "sessionID": "ses_other", "questions": [{"question": "?"}]}),
        );
        assert!(r.frame(&stranger).is_empty());
    }

    #[test]
    fn a_frame_the_table_lacks_is_counted_and_chatter_is_not() {
        let mut r = reducer();
        r.frame(&frame("server.heartbeat", json!({})));
        r.frame(&frame("lsp.client.diagnostics", json!({})));
        r.frame(&frame("session.teleported", json!({"sessionID": "ses_a"})));
        r.frame(&json!({"payload": {"type": "session.teleported", "properties": {}}}));
        assert_eq!(
            r.take_unknown(),
            ["type:session.teleported", "type:session.teleported"]
        );
        assert!(r.take_unknown().is_empty());
    }

    #[test]
    fn the_v1_global_bus_wraps_each_frame_in_a_payload() {
        let mut r = reducer();
        r.frame(&json!({"directory": "/w", "payload": busy()}));
        assert_eq!(
            r.frame(&json!({"payload": idle_status()})),
            [Effect::CheckIdle]
        );
        assert_eq!(r.frame(&json!({"payload": idle_event()})), [Effect::Idle]);
    }

    #[test]
    fn an_idle_between_turns_is_nobodys() {
        let mut r = reducer();
        r.frame(&busy());
        r.frame(&idle_status());
        r.frame(&idle_event());
        assert!(r.frame(&idle_event()).is_empty());
        r.begin_turn(true);
        assert!(r.active() && r.prompt_in_flight() && !r.saw_activity());
        r.frame(&busy());
        assert!(r.saw_activity() && r.running_anyway());
    }
}
