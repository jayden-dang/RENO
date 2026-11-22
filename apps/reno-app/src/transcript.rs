//! The row builder: the one place the meaning of a run's events lives.
//!
//! [`build_rows`] is a pure function from a run's `(Seq, ExecEvent)` slice (plus the approvals the
//! state says are pending for the run and the run's prompt) to the rows the Run screen lists. It
//! reads no clock and no GPUI state, so every rule below is a plain unit test.
//!
//! Rules:
//!
//! - The prompt, when known, is the first row: a user bubble. A `UserMessage` (a steer) is a user
//!   bubble too.
//! - Consecutive `Text` events are one prose row, and consecutive `Reasoning` events one
//!   reasoning row: the harness streams deltas, so the text is concatenated as is. The row's key
//!   is the first event's seq, which stays put while the row grows (so per-row caches hold).
//! - `Tool` and `ToolResult` events fold into one tool group. A result joins its call by id. The
//!   group ends at the next event that makes a row; `Started` and `Usage` make none and do not
//!   end it.
//! - `NeedsApproval` is a card only while a pending approval of this run has the same subject.
//!   After that it is a one-line "asked to approve" note.
//! - `ApprovalResolved` is one muted status line saying who decided and why ("Approved by rule
//!   cargo in the worktree", "Denied: nobody answered in time"). It follows the note or card of
//!   the request it answers.
//! - `Usage` and `Started` feed [`Facts`] for the details pane and make no row.
//! - `Done` is the last row; anything after it is ignored.

use reno_proto::{
    Approval, ApprovalOutcome, ApprovalSubject, ExecEvent, ExecutorId, Question, ReportStatus,
    ResolvedBy, Seq, StopReason, ToolOutput,
};

use crate::summary;
use crate::vm::routing::RoutingView;

/// Stable identity of a row: the seq of the event that opened it, plus one. The prompt is 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Key(pub(crate) u64);

impl Key {
    const PROMPT: Self = Self(0);
    /// The routing card: not an event, so it has a key no event can have.
    pub(crate) const ROUTING: Self = Self(u64::MAX);

    fn at(seq: Seq) -> Self {
        Self(seq.0.saturating_add(1))
    }

    /// The key of a row of the `segment`th run of a thread's transcript: the run's index in the
    /// high bits, so rows of different runs never share a key. The routing key keeps its place
    /// at the top of the range, one step down per segment.
    pub(crate) fn in_segment(self, segment: usize) -> Self {
        let n = u64::try_from(segment).unwrap_or(0);
        if self.0 >= u64::MAX - 0xffff {
            Self(self.0.saturating_sub(n))
        } else {
            Self(self.0 | (n << 40))
        }
    }

    /// The key of the fold that holds the rows from `first` on: `first` with a bit no seq reaches.
    pub(crate) fn fold(first: Self) -> Self {
        Self(first.0 | (1 << 39))
    }

    /// The key of the `n`th agent-posted visual of a thread: not a row's, and no row can have it.
    pub(crate) const fn visual(n: usize) -> Self {
        Self(0xfd00_0000_0000_0000 | n as u64)
    }

    /// The key of a divider before the `segment`th run.
    pub(crate) fn divider(segment: usize) -> Self {
        Self((u64::try_from(segment).unwrap_or(0) << 40) | 0xff_ffff_ffff)
    }
}

/// What kind of work a tool call did, for the group's title.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ToolKind {
    Command,
    Edit,
    Other,
}

/// How a tool call ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// No result yet.
    Pending,
    Ok(Option<String>),
    Failed(Option<String>),
}

impl ToolEntry {
    /// Whether the call has its result.
    pub(crate) fn finished(&self) -> bool {
        self.outcome != Outcome::Pending
    }
}

/// One tool call in a group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToolEntry {
    pub(crate) id: String,
    pub(crate) kind: ToolKind,
    /// The command, path or URL, else the tool's name.
    pub(crate) label: String,
    pub(crate) outcome: Outcome,
    /// What the tool printed, as the daemon kept it; none from an older daemon or a tool that
    /// printed nothing.
    pub(crate) output: Option<ToolOutput>,
}

/// One row of the transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Row {
    /// A user bubble, right-aligned. `steer` marks a message sent mid-run.
    User {
        key: Key,
        text: String,
        steer: bool,
        /// When it was sent, where the client knows (the prompt: the run's start).
        at_ms: Option<u64>,
    },
    /// Assistant prose, rendered as streaming markdown.
    Prose { key: Key, text: String },
    /// Reasoning, collapsed by default.
    Reasoning { key: Key, text: String },
    /// "Ran N commands", expandable.
    Tools { key: Key, calls: Vec<ToolEntry> },
    /// The harness asks questions; once resolved, the same card shows how it ended.
    Question {
        key: Key,
        request_id: String,
        questions: Vec<Question>,
        resolution: Resolution,
    },
    /// An inline approval card: the approval is pending.
    Approval { key: Key, approval: Approval },
    /// A past approval request or its resolution, one muted line.
    Asked { key: Key, line: String },
    /// Where the run was routed and why: a card after the prompt, with the "why" collapsed.
    Routing { key: Key, view: RoutingView },
    /// The run's model was changed while it ran: "Switched to Haiku 5.5". `model` is the id the
    /// harness reported.
    ModelChanged { key: Key, model: String },
    /// Where a follow-up run of the thread begins: "New run · Claude Code · sonnet", with when.
    Divider {
        key: Key,
        executor: ExecutorId,
        model: Option<String>,
        at_ms: u64,
    },
    /// A finished turn's interim text and steps, folded behind "Worked for Ns ›". `secs` is the
    /// turn's real elapsed time where the client has it.
    Worked {
        key: Key,
        secs: Option<u64>,
        rows: Vec<Row>,
    },
    /// The final status line.
    Done {
        key: Key,
        stop: StopReason,
        result: Option<String>,
        diagnostic: Option<String>,
        /// The typed reason the daemon gave, which the line shows in words.
        reason: Option<reno_proto::StopDetail>,
    },
}

/// How a question ended. The journal has an event for a declined question
/// (`InputExpired`) but none for an answered one, so an answer is read from the result of the
/// harness's input tool (`AskUserQuestion`, `request_user_input`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Resolution {
    /// Nobody has answered yet.
    Pending,
    /// The time limit passed and the harness was told there is no answer.
    Declined,
    /// The run moved on or ended with the question open and no answer recorded.
    Unanswered,
    /// Answered; one entry per question, `None` where the answer is not known.
    Answered(Vec<Option<String>>),
}

/// Whether a tool is the harness asking a person (its result carries the answer).
pub(crate) fn is_input_tool(name: &str) -> bool {
    name.eq_ignore_ascii_case("AskUserQuestion") || name.eq_ignore_ascii_case("request_user_input")
}

/// What each question was answered, read from the input tool's result text: the options it
/// names, else (one free-text question) the text itself.
fn read_answers(questions: &[Question], summary: &str) -> Vec<Option<String>> {
    questions
        .iter()
        .map(|q| {
            let chosen: Vec<&str> = q
                .options
                .iter()
                .filter(|o| !o.is_empty() && summary.contains(&format!("\"{o}\"")))
                .map(String::as_str)
                .collect();
            if !chosen.is_empty() {
                Some(chosen.join(", "))
            } else if q.options.is_empty() && questions.len() == 1 {
                Some(summary.trim().to_owned()).filter(|t| !t.is_empty())
            } else {
                None
            }
        })
        .collect()
}

impl Row {
    pub(crate) fn key(&self) -> Key {
        match self {
            Self::User { key, .. }
            | Self::Prose { key, .. }
            | Self::Reasoning { key, .. }
            | Self::Tools { key, .. }
            | Self::Question { key, .. }
            | Self::Approval { key, .. }
            | Self::Asked { key, .. }
            | Self::Routing { key, .. }
            | Self::ModelChanged { key, .. }
            | Self::Divider { key, .. }
            | Self::Worked { key, .. }
            | Self::Done { key, .. } => *key,
        }
    }

    /// The row with its key moved into the `segment`th run of a thread's transcript, so keys of
    /// different runs never meet. Segment 0 is the identity.
    pub(crate) fn in_segment(mut self, segment: usize) -> Self {
        if segment == 0 {
            return self;
        }
        let (Self::User { key, .. }
        | Self::Prose { key, .. }
        | Self::Reasoning { key, .. }
        | Self::Tools { key, .. }
        | Self::Question { key, .. }
        | Self::Approval { key, .. }
        | Self::Asked { key, .. }
        | Self::Routing { key, .. }
        | Self::ModelChanged { key, .. }
        | Self::Divider { key, .. }
        | Self::Worked { key, .. }
        | Self::Done { key, .. }) = &mut self;
        *key = key.in_segment(segment);
        if let Self::Worked { rows, .. } = &mut self {
            *rows = std::mem::take(rows)
                .into_iter()
                .map(|row| row.in_segment(segment))
                .collect();
        }
        self
    }
}

/// Token usage so far. `input` is the whole input of the calls, the cached part included: the
/// harness reports the uncached remainder apart, and showing only that would undercount a long
/// run by orders of magnitude.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Usage {
    pub(crate) input: u64,
    pub(crate) output: u64,
}

impl Usage {
    pub(crate) fn total(self) -> u64 {
        self.input.saturating_add(self.output)
    }
}

/// What the events say about the run besides rows: shown in the details pane.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    /// The latest `Usage` event (the harness reports totals so far).
    pub(crate) usage: Option<Usage>,
    /// The model the harness reported in `Started`.
    pub(crate) model: Option<String>,
    /// Why the run failed, when `Done` says so in words.
    pub(crate) diagnostic: Option<String>,
    /// The typed reason `Done` gave.
    pub(crate) reason: Option<reno_proto::StopDetail>,
}

/// Puts the routing card after the prompt (first, when the prompt is not known), if there is a
/// decision to show.
pub(crate) fn with_routing(mut rows: Vec<Row>, routing: Option<RoutingView>) -> Vec<Row> {
    if let Some(view) = routing {
        let at = usize::from(matches!(rows.first(), Some(Row::User { steer: false, .. })));
        rows.insert(
            at,
            Row::Routing {
                key: Key::ROUTING,
                view,
            },
        );
    }
    rows
}

/// Folds a finished turn's interim text and steps into [`Row::Worked`]. A turn is finished when
/// it has its `Done` row; the final message (the prose just before it) stays out, so it reads
/// bare under the fold. A user message, a question or the routing line ends a stretch, so each
/// stretch folds on its own; the time (`elapsed_ms`, the run's) goes on the fold only when the
/// turn has one stretch, since that is the only fold the whole span belongs to.
pub(crate) fn fold_turns(mut rows: Vec<Row>, elapsed_ms: Option<u64>) -> Vec<Row> {
    // A group of only the harness's questions has no step to show: the question row is its trace.
    rows.retain(
        |row| !matches!(row, Row::Tools { calls, .. } if !crate::vm::convo::has_steps(calls)),
    );
    let foldable = |row: &Row| {
        matches!(
            row,
            Row::Prose { .. }
                | Row::Reasoning { .. }
                | Row::Tools { .. }
                | Row::Asked { .. }
                | Row::ModelChanged { .. }
        )
    };
    // Which rows go behind a fold: those of a finished turn, bar its final message.
    let mut folded = vec![false; rows.len()];
    let mut turn_start = 0;
    for (done, row) in rows.iter().enumerate() {
        if !matches!(row, Row::Done { .. }) {
            continue;
        }
        let last = match done.checked_sub(1) {
            Some(at) if at >= turn_start && matches!(rows[at], Row::Prose { .. }) => at,
            _ => done,
        };
        for at in turn_start..last {
            folded[at] = foldable(&rows[at]);
        }
        turn_start = done + 1;
    }
    let stretches = (0..rows.len())
        .filter(|&at| folded[at] && (at == 0 || !folded[at - 1]))
        .count();
    let secs = elapsed_ms.filter(|_| stretches == 1).map(|ms| ms / 1000);
    let mut out: Vec<Row> = Vec::with_capacity(rows.len());
    let mut open: Option<Vec<Row>> = None;
    let close = |out: &mut Vec<Row>, open: &mut Option<Vec<Row>>| {
        if let Some(inner) = open.take() {
            out.push(Row::Worked {
                key: Key::fold(inner[0].key()),
                secs,
                rows: inner,
            });
        }
    };
    for (at, row) in rows.into_iter().enumerate() {
        if folded[at] {
            open.get_or_insert_with(Vec::new).push(row);
        } else {
            close(&mut out, &mut open);
            out.push(row);
        }
    }
    close(&mut out, &mut open);
    out
}

/// Says when the prompt was sent: the run's start. A steer has no time of its own.
pub(crate) fn stamp_prompt(rows: &mut [Row], at_ms: u64) {
    for row in rows {
        if let Row::User {
            key,
            steer: false,
            at_ms: stamp,
            ..
        } = row
            && *key == Key::PROMPT
        {
            *stamp = Some(at_ms);
        }
    }
}

/// The result of [`build_rows`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Built {
    pub(crate) rows: Vec<Row>,
    pub(crate) facts: Facts,
}

/// The input tool finished: the latest open question is answered (its result names the
/// choice) or, when the tool failed, closed without an answer.
fn resolve_open_question(rows: &mut [Row], ok: bool, summary: Option<&str>) {
    let open = rows.iter_mut().rev().find_map(|row| match row {
        Row::Question {
            questions,
            resolution,
            ..
        } if *resolution == Resolution::Pending => Some((questions, resolution)),
        _ => None,
    });
    if let Some((questions, resolution)) = open {
        *resolution = match (ok, summary) {
            (true, Some(text)) => Resolution::Answered(read_answers(questions, text)),
            _ => Resolution::Unanswered,
        };
    }
}

/// Builds the rows. `prompt` is the run's prompt, `pending` the approvals the state
/// holds for this run.
pub(crate) fn build_rows(
    prompt: Option<&str>,
    events: &[(Seq, ExecEvent)],
    pending: &[Approval],
) -> Built {
    let mut built = Built::default();
    if let Some(text) = prompt.filter(|t| !t.trim().is_empty()) {
        built.rows.push(Row::User {
            key: Key::PROMPT,
            text: text.to_owned(),
            steer: false,
            at_ms: None,
        });
    }
    let mut used = vec![false; pending.len()];
    // A `Done` ends the list, unless the run is resumed: then its next attempt goes on below.
    let mut ended = false;
    let mut input_calls: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (seq, event) in events {
        if ended {
            if !matches!(event, ExecEvent::Resumed { .. }) {
                continue;
            }
            ended = false;
        }
        let key = Key::at(*seq);
        match event {
            ExecEvent::Started { model, .. } => built.facts.model = Some(model.clone()),
            ExecEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
            } => {
                built.facts.usage = Some(Usage {
                    input: input_tokens
                        .saturating_add(*cache_read_tokens)
                        .saturating_add(*cache_write_tokens),
                    output: *output_tokens,
                });
            }
            // Context occupancy is a gauge (read with `ContextUsage::latest`) and a changed
            // configuration shows in the run's summary, not as a transcript row.
            ExecEvent::ContextUsage { .. } | ExecEvent::ConfigChanged { .. } => {}
            ExecEvent::Text { text } => match built.rows.last_mut() {
                Some(Row::Prose { text: open, .. }) => open.push_str(text),
                _ => built.rows.push(Row::Prose {
                    key,
                    text: text.clone(),
                }),
            },
            ExecEvent::Reasoning { text } => match built.rows.last_mut() {
                Some(Row::Reasoning { text: open, .. }) => open.push_str(text),
                _ => built.rows.push(Row::Reasoning {
                    key,
                    text: text.clone(),
                }),
            },
            ExecEvent::Tool { id, call } => {
                if is_input_tool(&call.name) {
                    input_calls.insert(id.clone());
                }
                let entry = ToolEntry {
                    id: id.clone(),
                    kind: tool_kind(&call.name),
                    label: tool_label(&call.name, &call.input),
                    outcome: Outcome::Pending,
                    output: None,
                };
                match built.rows.last_mut() {
                    Some(Row::Tools { calls, .. }) => calls.push(entry),
                    _ => built.rows.push(Row::Tools {
                        key,
                        calls: vec![entry],
                    }),
                }
            }
            ExecEvent::ToolResult {
                id,
                ok,
                summary,
                output,
            } => {
                if input_calls.contains(id) {
                    resolve_open_question(&mut built.rows, *ok, summary.as_deref());
                }
                let outcome = if *ok {
                    Outcome::Ok(summary.clone())
                } else {
                    Outcome::Failed(summary.clone())
                };
                // The call may sit in an earlier group (an approval card came between).
                let target = built
                    .rows
                    .iter()
                    .enumerate()
                    .rev()
                    .find_map(|(r, row)| match row {
                        Row::Tools { calls, .. } => calls
                            .iter()
                            .rposition(|c| &c.id == id && c.outcome == Outcome::Pending)
                            .map(|c| (r, c)),
                        _ => None,
                    });
                if let Some((r, c)) = target {
                    if let Row::Tools { calls, .. } = &mut built.rows[r] {
                        calls[c].outcome = outcome;
                        calls[c].output = output.clone().filter(|o| !o.text.is_empty());
                    }
                } else {
                    let orphan = ToolEntry {
                        id: id.clone(),
                        kind: ToolKind::Other,
                        label: "tool result".to_owned(),
                        outcome,
                        output: output.clone().filter(|o| !o.text.is_empty()),
                    };
                    match built.rows.last_mut() {
                        Some(Row::Tools { calls, .. }) => calls.push(orphan),
                        _ => built.rows.push(Row::Tools {
                            key,
                            calls: vec![orphan],
                        }),
                    }
                }
            }
            ExecEvent::UserMessage { text } => built.rows.push(Row::User {
                key,
                text: text.clone(),
                steer: true,
                at_ms: None,
            }),
            ExecEvent::AgentReport {
                status, summary, ..
            } => built.rows.push(Row::Asked {
                key,
                line: format!("Reported {}: {summary}", report_word(*status)),
            }),
            ExecEvent::NeedsInput {
                request_id,
                questions,
            } => built.rows.push(Row::Question {
                key,
                request_id: request_id.clone(),
                questions: questions.clone(),
                resolution: Resolution::Pending,
            }),
            ExecEvent::ModelChanged { model } => built.rows.push(Row::ModelChanged {
                key,
                model: model.clone(),
            }),
            ExecEvent::InputExpired { request_id } => {
                let card = built.rows.iter_mut().find_map(|row| match row {
                    Row::Question {
                        request_id: id,
                        resolution,
                        ..
                    } if id == request_id && *resolution == Resolution::Pending => Some(resolution),
                    _ => None,
                });
                if let Some(resolution) = card {
                    *resolution = Resolution::Declined;
                } else {
                    built.rows.push(Row::Asked {
                        key,
                        line: "Nobody answered in time; the question was declined.".to_owned(),
                    });
                }
            }
            ExecEvent::NeedsApproval { subject, .. } => {
                let held = pending
                    .iter()
                    .enumerate()
                    .find(|(i, a)| !used[*i] && &a.subject == subject);
                if let Some((i, approval)) = held {
                    used[i] = true;
                    built.rows.push(Row::Approval {
                        key,
                        approval: approval.clone(),
                    });
                } else {
                    built.rows.push(Row::Asked {
                        key,
                        line: asked_line(subject),
                    });
                }
            }
            ExecEvent::ApprovalResolved {
                decision,
                by,
                message,
                ..
            } => built.rows.push(Row::Asked {
                key,
                line: resolved_line(*decision, by, message.as_deref()),
            }),
            ExecEvent::Done {
                stop,
                result,
                diagnostic,
                reason,
            } => {
                built.facts.reason.clone_from(reason);
                built.facts.diagnostic.clone_from(diagnostic);
                for row in &mut built.rows {
                    if let Row::Question { resolution, .. } = row
                        && *resolution == Resolution::Pending
                    {
                        *resolution = Resolution::Unanswered;
                    }
                }
                // A result that is the last answer, already drawn above, is not said twice.
                let repeats = |result: &String| {
                    built.rows.iter().rev().find_map(|row| match row {
                        Row::Prose { text, .. } => Some(text.trim() == result.trim()),
                        _ => None,
                    }) == Some(true)
                };
                let result = result.clone().filter(|result| !repeats(result));
                built.rows.push(Row::Done {
                    key,
                    stop: *stop,
                    result,
                    diagnostic: diagnostic.clone(),
                    reason: reason.clone(),
                });
                ended = true;
            }
            ExecEvent::Resumed { from_attempt, .. } => {
                built.facts.reason = None;
                built.facts.diagnostic = None;
                built.rows.push(Row::Asked {
                    key,
                    line: format!(
                        "Resumed: attempt {} continues the session",
                        from_attempt.saturating_add(1)
                    ),
                });
            }
        }
    }
    built
}

/// What a [`TurnBuilder`] event did to the rows. Each names the first top-level row that is not
/// what it was: that row and every row after it may differ, the rows before it do not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Change {
    /// The event touched no row (usage, the model, anything after the run ended).
    Unchanged,
    /// The row with this key grew or was patched; there are as many rows as before.
    Grew(Key),
    /// The row with this key and those after it are new, or the first of them is.
    Added(Key),
    /// A `Done` folded its turn: the rows from `turn.start` on were made again, and `fold` is
    /// the first fold head among them (`turn.start` is its row when the turn begins with one).
    Folded {
        turn: std::ops::Range<usize>,
        fold: Key,
    },
}

/// A tool call as the call index holds it.
#[derive(Clone, Copy, Debug)]
#[allow(dead_code)] // The tool cards read it; only the builder's tests do today.
pub(crate) struct CallInfo<'a> {
    pub(crate) input: &'a reno_proto::ToolCall,
    /// The seq of the event that gave the call its result; none while it has none.
    pub(crate) result_seq: Option<Seq>,
}

/// One run's rows, made as its events arrive: the same rows [`build_rows`] and [`fold_turns`]
/// make from the whole list, held as they stand after each event. A delta grows the last row, a
/// finished turn replaces its loose rows with its fold, and no event makes the rows of an
/// earlier turn again. The run's prompt, its divider and its routing card are not events: the
/// screen puts them before these rows. Approval cards, which depend on what is pending beyond the
/// events, are not made here: an approval request is the "asked to approve" line.
#[derive(Debug)]
pub(crate) struct TurnBuilder {
    segment: usize,
    rows: Vec<Row>,
    facts: Facts,
    /// A tool group of only the harness's questions has no step to show, so it is no row of the
    /// list; until another row follows it, though, it is still where the next call goes.
    hidden: Option<(Key, Vec<ToolEntry>)>,
    /// Calls of such groups that were left behind, waiting for their result.
    hidden_calls: std::collections::HashSet<String>,
    input_calls: std::collections::HashSet<String>,
    calls: std::collections::HashMap<String, (reno_proto::ToolCall, Option<Seq>)>,
    /// The `Done` ended the run's attempt; only a resume goes on.
    ended: bool,
    /// Where the turn that is not finished begins.
    turn_start: usize,
    /// No steer, resume, approval or question has been seen: the run's span is one turn's work.
    one_stretch: bool,
    elapsed_ms: Option<u64>,
}

/// Where a row is: its top-level place and, inside a fold, its place in it.
type Place = (usize, Option<usize>);

fn visit_order(len: usize, from_end: bool) -> Box<dyn Iterator<Item = usize>> {
    if from_end {
        Box::new((0..len).rev())
    } else {
        Box::new(0..len)
    }
}

/// The first row `matches` accepts, in the order the rows read (a fold's rows where the fold is),
/// from the end or from the start.
fn find_row(rows: &[Row], from_end: bool, matches: impl Fn(&Row) -> bool) -> Option<Place> {
    for top in visit_order(rows.len(), from_end) {
        match &rows[top] {
            Row::Worked { rows: inner, .. } => {
                if let Some(at) = visit_order(inner.len(), from_end).find(|&at| matches(&inner[at]))
                {
                    return Some((top, Some(at)));
                }
            }
            row if matches(row) => return Some((top, None)),
            _ => {}
        }
    }
    None
}

fn row_at(rows: &mut [Row], (top, inner): Place) -> &mut Row {
    match (&mut rows[top], inner) {
        (Row::Worked { rows, .. }, Some(at)) => &mut rows[at],
        (row, _) => row,
    }
}

fn touch(first: &mut Option<usize>, top: usize) {
    *first = Some(first.map_or(top, |was| was.min(top)));
}

impl TurnBuilder {
    /// A builder for the `segment`th run of a thread's transcript (0 for a thread of one run).
    pub(crate) fn new(segment: usize) -> Self {
        Self {
            segment,
            rows: Vec::new(),
            facts: Facts::default(),
            hidden: None,
            hidden_calls: std::collections::HashSet::new(),
            input_calls: std::collections::HashSet::new(),
            calls: std::collections::HashMap::new(),
            ended: false,
            turn_start: 0,
            one_stretch: true,
            elapsed_ms: None,
        }
    }

    /// The rows as [`fold_turns`] leaves them, keyed into the segment.
    pub(crate) fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub(crate) fn facts(&self) -> &Facts {
        &self.facts
    }

    /// The tool call `id` as the events gave it, and where its result came.
    #[allow(dead_code)] // The tool cards read it; only the builder's tests do today.
    pub(crate) fn call(&self, id: &str) -> Option<CallInfo<'_>> {
        let (input, result_seq) = self.calls.get(id)?;
        Some(CallInfo {
            input,
            result_seq: *result_seq,
        })
    }

    /// Says how long the run took, from the run's own times: the time goes on the one fold of a
    /// run that was one turn's work (see [`crate::vm::convo::run_elapsed_ms`]) and on no other.
    pub(crate) fn set_run_time(&mut self, ended: bool, created_ms: u64, updated_ms: u64) -> Change {
        self.elapsed_ms = self
            .one_stretch
            .then(|| crate::vm::convo::run_elapsed_ms(ended, created_ms, updated_ms, &[]))
            .flatten();
        self.apply_elapsed()
    }

    fn apply_elapsed(&mut self) -> Change {
        let folds = self
            .rows
            .iter()
            .filter(|row| matches!(row, Row::Worked { .. }))
            .count();
        let want = self.elapsed_ms.filter(|_| folds == 1).map(|ms| ms / 1000);
        let mut first = None;
        for (top, row) in self.rows.iter_mut().enumerate() {
            if let Row::Worked { secs, .. } = row
                && *secs != want
            {
                *secs = want;
                touch(&mut first, top);
            }
        }
        self.change(first, self.rows.len())
    }

    fn key_at(&self, seq: Seq) -> Key {
        Key::at(seq).in_segment(self.segment)
    }

    fn change(&self, first: Option<usize>, before: usize) -> Change {
        match first.and_then(|top| self.rows.get(top)) {
            None => Change::Unchanged,
            Some(row) if self.rows.len() > before => Change::Added(row.key()),
            Some(row) => Change::Grew(row.key()),
        }
    }

    /// The group a call is added to, or a new one; the group is no row while it only holds the
    /// harness's questions.
    fn add_call(&mut self, entry: ToolEntry, key: Key, first: &mut Option<usize>) {
        if let Some((_, calls)) = self.hidden.as_mut() {
            calls.push(entry);
            if crate::vm::convo::has_steps(calls) {
                let (key, calls) = self.hidden.take().unwrap_or((key, Vec::new()));
                self.rows.push(Row::Tools { key, calls });
                touch(first, self.rows.len() - 1);
            }
            return;
        }
        if let Some(Row::Tools { calls, .. }) = self.rows.last_mut() {
            calls.push(entry);
            touch(first, self.rows.len() - 1);
        } else if crate::vm::convo::has_steps(std::slice::from_ref(&entry)) {
            self.rows.push(Row::Tools {
                key,
                calls: vec![entry],
            });
            touch(first, self.rows.len() - 1);
        } else {
            self.hidden = Some((key, vec![entry]));
        }
    }

    /// Another row follows: a group of only questions is left behind.
    fn leave_hidden(&mut self) {
        if let Some((_, calls)) = self.hidden.take() {
            self.hidden_calls.extend(
                calls
                    .into_iter()
                    .filter(|c| c.outcome == Outcome::Pending)
                    .map(|c| c.id),
            );
        }
    }

    fn push_row(&mut self, row: Row, first: &mut Option<usize>) {
        self.leave_hidden();
        self.rows.push(row);
        touch(first, self.rows.len() - 1);
    }

    /// The input tool finished: the latest open question is answered, or closed without an answer.
    fn resolve_open_question(
        &mut self,
        ok: bool,
        summary: Option<&str>,
        first: &mut Option<usize>,
    ) {
        let found = find_row(
            &self.rows,
            true,
            |row| matches!(row, Row::Question { resolution, .. } if *resolution == Resolution::Pending),
        );
        if let Some(place) = found {
            touch(first, place.0);
            if let Row::Question {
                questions,
                resolution,
                ..
            } = row_at(&mut self.rows, place)
            {
                *resolution = match (ok, summary) {
                    (true, Some(text)) => Resolution::Answered(read_answers(questions, text)),
                    _ => Resolution::Unanswered,
                };
            }
        }
    }

    /// Takes in the next event of the run, in order, and says what it did to the rows.
    pub(crate) fn push(&mut self, seq: Seq, event: &ExecEvent) -> Change {
        if matches!(
            event,
            ExecEvent::UserMessage { .. }
                | ExecEvent::Resumed { .. }
                | ExecEvent::NeedsApproval { .. }
                | ExecEvent::NeedsInput { .. }
        ) {
            self.one_stretch = false;
        }
        if self.ended {
            if !matches!(event, ExecEvent::Resumed { .. }) {
                return Change::Unchanged;
            }
            self.ended = false;
        }
        let key = self.key_at(seq);
        let before = self.rows.len();
        let mut first: Option<usize> = None;
        match event {
            ExecEvent::Started { model, .. } => self.facts.model = Some(model.clone()),
            ExecEvent::Usage {
                input_tokens,
                output_tokens,
                cache_read_tokens,
                cache_write_tokens,
            } => {
                self.facts.usage = Some(Usage {
                    input: input_tokens
                        .saturating_add(*cache_read_tokens)
                        .saturating_add(*cache_write_tokens),
                    output: *output_tokens,
                });
            }
            ExecEvent::ContextUsage { .. } | ExecEvent::ConfigChanged { .. } => {}
            ExecEvent::Text { text } => match self.rows.last_mut() {
                Some(Row::Prose { text: open, .. }) if self.hidden.is_none() => {
                    open.push_str(text);
                    touch(&mut first, self.rows.len() - 1);
                }
                _ => self.push_row(
                    Row::Prose {
                        key,
                        text: text.clone(),
                    },
                    &mut first,
                ),
            },
            ExecEvent::Reasoning { text } => match self.rows.last_mut() {
                Some(Row::Reasoning { text: open, .. }) if self.hidden.is_none() => {
                    open.push_str(text);
                    touch(&mut first, self.rows.len() - 1);
                }
                _ => self.push_row(
                    Row::Reasoning {
                        key,
                        text: text.clone(),
                    },
                    &mut first,
                ),
            },
            ExecEvent::Tool { id, call } => {
                if is_input_tool(&call.name) {
                    self.input_calls.insert(id.clone());
                }
                self.calls.insert(id.clone(), (call.clone(), None));
                let entry = ToolEntry {
                    id: id.clone(),
                    kind: tool_kind(&call.name),
                    label: tool_label(&call.name, &call.input),
                    outcome: Outcome::Pending,
                    output: None,
                };
                self.add_call(entry, key, &mut first);
            }
            ExecEvent::ToolResult {
                id,
                ok,
                summary,
                output,
            } => {
                if let Some((_, result_seq)) = self.calls.get_mut(id)
                    && result_seq.is_none()
                {
                    *result_seq = Some(seq);
                }
                if self.input_calls.contains(id) {
                    self.resolve_open_question(*ok, summary.as_deref(), &mut first);
                }
                let outcome = if *ok {
                    Outcome::Ok(summary.clone())
                } else {
                    Outcome::Failed(summary.clone())
                };
                let output = output.clone().filter(|o| !o.text.is_empty());
                let pending_call = |calls: &[ToolEntry]| {
                    calls
                        .iter()
                        .rposition(|c| &c.id == id && c.outcome == Outcome::Pending)
                };
                let held = self
                    .hidden
                    .as_mut()
                    .and_then(|(_, calls)| pending_call(calls).map(|at| &mut calls[at]));
                if let Some(call) = held {
                    call.outcome = outcome;
                    call.output = output;
                } else if let Some(place) = find_row(
                    &self.rows,
                    true,
                    |row| matches!(row, Row::Tools { calls, .. } if pending_call(calls).is_some()),
                ) {
                    touch(&mut first, place.0);
                    if let Row::Tools { calls, .. } = row_at(&mut self.rows, place)
                        && let Some(at) = pending_call(calls)
                    {
                        calls[at].outcome = outcome;
                        calls[at].output = output;
                    }
                } else if !self.hidden_calls.remove(id) {
                    let orphan = ToolEntry {
                        id: id.clone(),
                        kind: ToolKind::Other,
                        label: "tool result".to_owned(),
                        outcome,
                        output,
                    };
                    self.add_call(orphan, key, &mut first);
                }
            }
            ExecEvent::UserMessage { text } => self.push_row(
                Row::User {
                    key,
                    text: text.clone(),
                    steer: true,
                    at_ms: None,
                },
                &mut first,
            ),
            ExecEvent::AgentReport {
                status, summary, ..
            } => self.push_row(
                Row::Asked {
                    key,
                    line: format!("Reported {}: {summary}", report_word(*status)),
                },
                &mut first,
            ),
            ExecEvent::NeedsInput {
                request_id,
                questions,
            } => self.push_row(
                Row::Question {
                    key,
                    request_id: request_id.clone(),
                    questions: questions.clone(),
                    resolution: Resolution::Pending,
                },
                &mut first,
            ),
            ExecEvent::ModelChanged { model } => self.push_row(
                Row::ModelChanged {
                    key,
                    model: model.clone(),
                },
                &mut first,
            ),
            ExecEvent::InputExpired { request_id } => {
                let card = find_row(&self.rows, false, |row| {
                    matches!(row, Row::Question { request_id: id, resolution, .. }
                        if id == request_id && *resolution == Resolution::Pending)
                });
                if let Some(place) = card {
                    touch(&mut first, place.0);
                    if let Row::Question { resolution, .. } = row_at(&mut self.rows, place) {
                        *resolution = Resolution::Declined;
                    }
                } else {
                    self.push_row(
                        Row::Asked {
                            key,
                            line: "Nobody answered in time; the question was declined.".to_owned(),
                        },
                        &mut first,
                    );
                }
            }
            ExecEvent::NeedsApproval { subject, .. } => self.push_row(
                Row::Asked {
                    key,
                    line: asked_line(subject),
                },
                &mut first,
            ),
            ExecEvent::ApprovalResolved {
                decision,
                by,
                message,
                ..
            } => self.push_row(
                Row::Asked {
                    key,
                    line: resolved_line(*decision, by, message.as_deref()),
                },
                &mut first,
            ),
            ExecEvent::Done {
                stop,
                result,
                diagnostic,
                reason,
            } => {
                self.facts.reason.clone_from(reason);
                self.facts.diagnostic.clone_from(diagnostic);
                for (top, row) in self.rows.iter_mut().enumerate() {
                    if let Row::Question { resolution, .. } = row
                        && *resolution == Resolution::Pending
                    {
                        *resolution = Resolution::Unanswered;
                        touch(&mut first, top);
                    }
                }
                let repeats = |result: &String| {
                    find_row(&self.rows, true, |row| matches!(row, Row::Prose { .. })).is_some_and(
                        |(top, inner)| match (&self.rows[top], inner) {
                            (Row::Worked { rows, .. }, Some(at)) => {
                                matches!(&rows[at], Row::Prose { text, .. } if text.trim() == result.trim())
                            }
                            (Row::Prose { text, .. }, _) => text.trim() == result.trim(),
                            _ => false,
                        },
                    )
                };
                let result = result.clone().filter(|result| !repeats(result));
                self.push_row(
                    Row::Done {
                        key,
                        stop: *stop,
                        result,
                        diagnostic: diagnostic.clone(),
                        reason: reason.clone(),
                    },
                    &mut first,
                );
                self.ended = true;
                return self.fold_turn(first);
            }
            ExecEvent::Resumed { from_attempt, .. } => {
                self.facts.reason = None;
                self.facts.diagnostic = None;
                self.push_row(
                    Row::Asked {
                        key,
                        line: format!(
                            "Resumed: attempt {} continues the session",
                            from_attempt.saturating_add(1)
                        ),
                    },
                    &mut first,
                );
            }
        }
        self.change(first, before)
    }

    /// The turn that just ended folds, as [`fold_turns`] folds it.
    fn fold_turn(&mut self, changed: Option<usize>) -> Change {
        let start = self.turn_start.min(self.rows.len());
        let turn = self.rows.split_off(start);
        self.rows.extend(fold_turns(turn, None));
        self.turn_start = self.rows.len();
        let start = changed.map_or(start, |at| at.min(start));
        let fold = self.rows[start..]
            .iter()
            .find(|row| matches!(row, Row::Worked { .. }))
            .map(Row::key);
        self.apply_elapsed();
        match fold {
            Some(fold) => Change::Folded {
                turn: start..self.rows.len(),
                fold,
            },
            None => Change::Added(self.rows[start].key()),
        }
    }
}

/// The status line of a resolved approval: the outcome, then who decided.
fn resolved_line(decision: ApprovalOutcome, by: &ResolvedBy, message: Option<&str>) -> String {
    let outcome = match decision {
        ApprovalOutcome::Approved => "Approved",
        ApprovalOutcome::Denied => "Denied",
    };
    match by {
        ResolvedBy::Human => match message {
            Some(note) => format!("{outcome} by you \u{b7} {note}"),
            None => format!("{outcome} by you"),
        },
        ResolvedBy::Rule { name } => {
            let rule = crate::summary::rule_words(name);
            match message {
                Some(message) => format!("{outcome} by {rule}: {message}"),
                None => format!("{outcome} by {rule}"),
            }
        }
        // Closed because the run ended, not because nobody answered.
        ResolvedBy::Expired => match summary::withdrawn_reason(message) {
            Some(reason) => format!("Withdrawn: {reason}"),
            None => format!("{outcome}: nobody answered in time"),
        },
        ResolvedBy::RepeatDenial => format!("{outcome}: it was denied earlier in this run"),
    }
}

/// The word a report's status goes by in the transcript.
fn report_word(status: ReportStatus) -> &'static str {
    match status {
        ReportStatus::Progress => "progress",
        ReportStatus::Done => "done",
        ReportStatus::Blocked => "blocked",
        ReportStatus::Failed => "failed",
    }
}

fn asked_line(subject: &ApprovalSubject) -> String {
    format!("Asked to approve {}", summary::subject_line(subject))
}

fn tool_kind(name: &str) -> ToolKind {
    let name = name.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| name.contains(w));
    if has(&["bash", "shell", "exec", "command", "terminal"]) {
        ToolKind::Command
    } else if has(&["edit", "write", "patch", "replace"]) {
        ToolKind::Edit
    } else {
        ToolKind::Other
    }
}

/// What a tool call shows: its command, path or URL; otherwise the tool's name.
pub(crate) fn tool_label(name: &str, input: &serde_json::Value) -> String {
    let field = |keys: &[&str]| {
        keys.iter()
            .find_map(|k| input.get(*k).and_then(serde_json::Value::as_str))
    };
    if let Some(command) = field(&["command", "cmd"]) {
        crate::summary::unwrap_shell(command)
    } else if let Some(url) = field(&["url"]) {
        format!("GET {url}")
    } else if let Some(path) = field(&["file_path", "path"]) {
        format!("{name} {path}")
    } else {
        name.to_owned()
    }
}

#[cfg(test)]
mod tests {
    use reno_proto::{ActionClass, RunId, SessionRef, ToolCall};
    use serde_json::json;

    use super::*;

    fn at(events: Vec<ExecEvent>) -> Vec<(Seq, ExecEvent)> {
        events
            .into_iter()
            .enumerate()
            .map(|(i, e)| (Seq(i as u64), e))
            .collect()
    }

    fn text(s: &str) -> ExecEvent {
        ExecEvent::Text { text: s.into() }
    }

    fn bash(id: &str, cmd: &str) -> ExecEvent {
        ExecEvent::Tool {
            id: id.into(),
            call: ToolCall::new("bash", json!({ "command": cmd }), ActionClass::Command),
        }
    }

    fn result(id: &str, ok: bool, s: &str) -> ExecEvent {
        ExecEvent::ToolResult {
            id: id.into(),
            ok,
            summary: Some(s.into()),
            output: None,
        }
    }

    fn subject(cmd: &str) -> ApprovalSubject {
        ApprovalSubject {
            action: ActionClass::Command,
            tool: "bash".into(),
            command: Some(cmd.into()),
            paths: vec![],
            raw: json!({ "command": cmd }),
        }
    }

    fn approval(cmd: &str) -> Approval {
        Approval {
            id: "appr_a1".parse().unwrap(),
            run_id: RunId::from_suffix("r1").unwrap(),
            subject: subject(cmd),
            requested_at_ms: 1,
        }
    }

    fn build(events: Vec<ExecEvent>, pending: &[Approval]) -> Built {
        build_rows(None, &at(events), pending)
    }

    #[test]
    fn the_prompt_is_the_first_row_and_blank_prompts_make_none() {
        let built = build_rows(Some("do it"), &at(vec![text("ok")]), &[]);
        assert!(matches!(&built.rows[0], Row::User { text, steer: false, .. } if text == "do it"));
        assert_eq!(built.rows.len(), 2);
        assert!(build_rows(Some("  "), &[], &[]).rows.is_empty());
        assert!(build_rows(None, &[], &[]).rows.is_empty());
    }

    #[test]
    fn started_and_usage_make_facts_not_rows() {
        let built = build(
            vec![
                ExecEvent::Started {
                    session: SessionRef("s".into()),
                    model: "opus".into(),
                },
                ExecEvent::Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
                ExecEvent::Usage {
                    input_tokens: 30,
                    output_tokens: 9,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
            ],
            &[],
        );
        assert!(built.rows.is_empty());
        assert_eq!(built.facts.model.as_deref(), Some("opus"));
        let usage = built.facts.usage.unwrap();
        assert_eq!((usage.input, usage.output, usage.total()), (30, 9, 39));
    }

    #[test]
    fn a_usage_with_cache_tokens_shows_the_whole_input() {
        let built = build(
            vec![ExecEvent::Usage {
                input_tokens: 16,
                output_tokens: 9_120,
                cache_read_tokens: 1_270_554,
                cache_write_tokens: 48_213,
            }],
            &[],
        );
        let usage = built.facts.usage.unwrap();
        assert_eq!(usage.input, 1_318_783);
        assert_eq!(usage.total(), 1_327_903);
    }

    #[test]
    fn text_deltas_join_into_one_prose_row_keyed_by_the_first_seq() {
        let built = build(vec![text("Hel"), text("lo "), text("**there**")], &[]);
        assert_eq!(
            built.rows,
            vec![Row::Prose {
                key: Key(1),
                text: "Hello **there**".into()
            }]
        );
        // Growing the stream keeps the key.
        let more = build(
            vec![text("Hel"), text("lo "), text("**there**"), text("!")],
            &[],
        );
        assert_eq!(more.rows[0].key(), Key(1));
    }

    #[test]
    fn reasoning_is_its_own_row_and_text_after_it_starts_a_new_one() {
        let built = build(
            vec![
                ExecEvent::Reasoning {
                    text: "think".into(),
                },
                ExecEvent::Reasoning { text: "ing".into() },
                text("answer"),
            ],
            &[],
        );
        assert_eq!(built.rows.len(), 2);
        assert!(matches!(&built.rows[0], Row::Reasoning { text, .. } if text == "thinking"));
        assert!(matches!(&built.rows[1], Row::Prose { text, .. } if text == "answer"));
    }

    #[test]
    fn tools_group_and_results_join_their_calls() {
        let built = build(
            vec![
                bash("t1", "ls"),
                bash("t2", "cargo test"),
                result("t2", false, "1 failed"),
                ExecEvent::Usage {
                    input_tokens: 1,
                    output_tokens: 1,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
                result("t1", true, "Cargo.toml"),
                text("done"),
                bash("t3", "git status"),
            ],
            &[],
        );
        assert_eq!(built.rows.len(), 3, "{:#?}", built.rows);
        let Row::Tools { calls, .. } = &built.rows[0] else {
            panic!("expected a group");
        };
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].label, "ls");
        assert_eq!(calls[0].outcome, Outcome::Ok(Some("Cargo.toml".into())));
        assert_eq!(calls[1].outcome, Outcome::Failed(Some("1 failed".into())));
        let Row::Tools { calls, .. } = &built.rows[2] else {
            panic!("expected a second group");
        };
        assert_eq!(calls[0].outcome, Outcome::Pending, "no result yet");
    }

    #[test]
    fn a_tools_output_stays_with_its_call_and_its_outcome() {
        let out = |text: &str, truncated| {
            Some(ToolOutput {
                text: text.into(),
                bytes: 9_000,
                truncated,
            })
        };
        let with = |id: &str, ok, output| ExecEvent::ToolResult {
            id: id.into(),
            ok,
            summary: Some("s".into()),
            output,
        };
        let built = build(
            vec![
                bash("a", "ls"),
                bash("b", "cargo test"),
                bash("c", "true"),
                with("a", true, out("Cargo.toml\n", false)),
                with("b", false, out("FAILED\n", true)),
                // An empty output is no output.
                with("c", true, out("", false)),
            ],
            &[],
        );
        let Row::Tools { calls, .. } = &built.rows[0] else {
            panic!("expected a group");
        };
        assert_eq!(calls[0].output.as_ref().unwrap().text, "Cargo.toml\n");
        assert!(matches!(calls[0].outcome, Outcome::Ok(_)));
        let failed = calls[1].output.as_ref().unwrap();
        assert!(failed.truncated && matches!(calls[1].outcome, Outcome::Failed(_)));
        assert_eq!(calls[2].output, None);
    }

    #[test]
    fn an_orphan_result_still_shows() {
        let built = build(vec![result("zz", true, "ok")], &[]);
        let Row::Tools { calls, .. } = &built.rows[0] else {
            panic!("expected a group");
        };
        assert_eq!(calls[0].outcome, Outcome::Ok(Some("ok".into())));
    }

    #[test]
    fn tool_kinds_and_labels_come_from_the_call() {
        assert_eq!(tool_kind("apply_patch"), ToolKind::Edit);
        assert_eq!(tool_kind("Bash"), ToolKind::Command);
        assert_eq!(tool_kind("web_fetch"), ToolKind::Other);
        assert_eq!(
            tool_label("web_fetch", &json!({ "url": "https://x.test" })),
            "GET https://x.test"
        );
        assert_eq!(
            tool_label("edit", &json!({ "file_path": "a.rs" })),
            "edit a.rs"
        );
        assert_eq!(tool_label("mystery", &json!(null)), "mystery");
        assert_eq!(
            tool_label("shell", &json!({ "command": "/bin/zsh -lc 'touch x'" })),
            "touch x"
        );
    }

    #[test]
    fn steers_are_user_bubbles() {
        let built = build(
            vec![ExecEvent::UserMessage {
                text: "go left".into(),
            }],
            &[],
        );
        assert!(matches!(&built.rows[0], Row::User { text, steer: true, .. } if text == "go left"));
    }

    #[test]
    fn a_report_is_a_one_line_note_with_its_status() {
        let built = build(
            vec![ExecEvent::AgentReport {
                status: ReportStatus::Blocked,
                summary: "need the staging key".into(),
                details: Some("tried the vault".into()),
            }],
            &[],
        );
        assert!(matches!(
            &built.rows[0],
            Row::Asked { line, .. } if line == "Reported blocked: need the staging key"
        ));
    }

    fn pick() -> Question {
        Question {
            id: "a".into(),
            prompt: "Which colour?".into(),
            options: vec!["Red".into(), "Blue".into()],
            multi_select: false,
        }
    }

    fn asks(request_id: &str, questions: Vec<Question>) -> ExecEvent {
        ExecEvent::NeedsInput {
            request_id: request_id.into(),
            questions,
        }
    }

    fn ask_tool(id: &str) -> ExecEvent {
        ExecEvent::Tool {
            id: id.into(),
            call: ToolCall::new("AskUserQuestion", json!({}), ActionClass::Other),
        }
    }

    fn question_rows(built: &Built) -> Vec<(&str, &Resolution)> {
        built
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Question {
                    request_id,
                    resolution,
                    ..
                } => Some((request_id.as_str(), resolution)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn an_expired_question_resolves_its_own_card_with_no_status_row() {
        let built = build(
            vec![
                asks("q1", vec![pick()]),
                ExecEvent::InputExpired {
                    request_id: "q1".into(),
                },
            ],
            &[],
        );
        assert_eq!(built.rows.len(), 1, "{:?}", built.rows);
        assert_eq!(question_rows(&built), [("q1", &Resolution::Declined)]);
    }

    #[test]
    fn an_expiry_for_a_question_not_in_the_list_stays_a_status_line() {
        let built = build(
            vec![ExecEvent::InputExpired {
                request_id: "gone".into(),
            }],
            &[],
        );
        assert_eq!(
            built.rows,
            vec![Row::Asked {
                key: Key(1),
                line: "Nobody answered in time; the question was declined.".into()
            }]
        );
    }

    #[test]
    fn a_question_open_when_the_run_ends_is_not_answered() {
        let done = ExecEvent::Done {
            stop: StopReason::Completed,
            result: None,
            diagnostic: None,
            reason: None,
        };
        let built = build(vec![asks("q1", vec![pick()]), done], &[]);
        assert_eq!(question_rows(&built), [("q1", &Resolution::Unanswered)]);
    }

    #[test]
    fn a_chosen_option_in_the_tool_result_answers_the_question() {
        let built = build(
            vec![
                ask_tool("t1"),
                asks("q1", vec![pick()]),
                result("t1", true, "User has answered: \"Which colour?\"=\"Blue\""),
            ],
            &[],
        );
        assert_eq!(
            question_rows(&built),
            [("q1", &Resolution::Answered(vec![Some("Blue".into())]))]
        );
    }

    #[test]
    fn a_free_text_answer_is_the_result_text() {
        let free = Question {
            id: "n".into(),
            prompt: "Name it".into(),
            options: vec![],
            multi_select: false,
        };
        let built = build(
            vec![
                ask_tool("t1"),
                asks("q1", vec![free]),
                result("t1", true, "Ferris"),
            ],
            &[],
        );
        assert_eq!(
            question_rows(&built),
            [("q1", &Resolution::Answered(vec![Some("Ferris".into())]))]
        );
    }

    #[test]
    fn each_of_several_questions_gets_its_own_answer() {
        let size = Question {
            id: "s".into(),
            prompt: "Size?".into(),
            options: vec!["S".into(), "L".into()],
            multi_select: false,
        };
        let built = build(
            vec![
                ask_tool("t1"),
                asks("q1", vec![pick(), size]),
                result("t1", true, "\"Which colour?\"=\"Red\", \"Size?\"=\"L\""),
            ],
            &[],
        );
        let Resolution::Answered(answers) = question_rows(&built)[0].1 else {
            panic!("expected answered");
        };
        assert_eq!(answers, &[Some("Red".to_owned()), Some("L".to_owned())]);
    }

    #[test]
    fn a_failed_input_tool_leaves_the_question_not_answered() {
        let built = build(
            vec![
                ask_tool("t1"),
                asks("q1", vec![pick()]),
                result("t1", false, "closed"),
            ],
            &[],
        );
        assert_eq!(question_rows(&built), [("q1", &Resolution::Unanswered)]);
    }

    #[test]
    fn questions_are_rows() {
        let q = pick();
        let built = build(vec![asks("q1", vec![q.clone()])], &[]);
        assert_eq!(
            built.rows,
            vec![Row::Question {
                key: Key(1),
                request_id: "q1".into(),
                questions: vec![q],
                resolution: Resolution::Pending,
            }]
        );
    }

    #[test]
    fn a_question_nobody_resolved_yet_is_pending() {
        let built = build(vec![asks("q1", vec![pick()])], &[]);
        assert_eq!(question_rows(&built), [("q1", &Resolution::Pending)]);
    }

    #[test]
    fn a_resolved_approval_is_one_status_line_saying_who_decided() {
        let resolved = |decision, by, message: Option<&str>| ExecEvent::ApprovalResolved {
            request_id: "req".into(),
            decision,
            by,
            message: message.map(str::to_owned),
        };
        let rule = |name: &str| ResolvedBy::Rule { name: name.into() };
        let cases = [
            (
                resolved(
                    ApprovalOutcome::Approved,
                    rule("cargo in the worktree"),
                    None,
                ),
                "Approved by rule cargo in the worktree",
            ),
            (
                resolved(
                    ApprovalOutcome::Denied,
                    rule("force push"),
                    Some("denied by policy: force push"),
                ),
                "Denied by rule force push: denied by policy: force push",
            ),
            (
                resolved(ApprovalOutcome::Approved, rule("rules[7]"), None),
                "Approved by policy rule 8",
            ),
            (
                resolved(
                    ApprovalOutcome::Approved,
                    rule("standing:rule_001abc"),
                    None,
                ),
                "Approved by your rule",
            ),
            (
                resolved(ApprovalOutcome::Approved, ResolvedBy::Human, None),
                "Approved by you",
            ),
            (
                resolved(ApprovalOutcome::Denied, ResolvedBy::Human, None),
                "Denied by you",
            ),
            (
                resolved(ApprovalOutcome::Denied, ResolvedBy::Expired, None),
                "Denied: nobody answered in time",
            ),
            (
                resolved(
                    ApprovalOutcome::Denied,
                    ResolvedBy::Expired,
                    Some("withdrawn: the run was cancelled"),
                ),
                "Withdrawn: the run was cancelled",
            ),
            (
                resolved(ApprovalOutcome::Denied, ResolvedBy::RepeatDenial, None),
                "Denied: it was denied earlier in this run",
            ),
        ];
        for (event, line) in cases {
            let built = build(
                vec![
                    ExecEvent::NeedsApproval {
                        request_id: "req".into(),
                        subject: subject("cargo test"),
                    },
                    event,
                ],
                &[],
            );
            // The request's own note, then the resolution; nothing else.
            assert_eq!(built.rows.len(), 2, "{line}");
            assert!(matches!(
                &built.rows[0],
                Row::Asked { line, .. } if line == "Asked to approve cargo test"
            ));
            assert_eq!(
                built.rows[1],
                Row::Asked {
                    key: Key(2),
                    line: line.to_owned()
                }
            );
        }
    }

    #[test]
    fn a_human_denial_shows_the_note_that_went_with_it() {
        let line = |message: Option<&str>| {
            resolved_line(ApprovalOutcome::Denied, &ResolvedBy::Human, message)
        };
        assert_eq!(line(None), "Denied by you");
        assert_eq!(
            line(Some("use the mirror")),
            "Denied by you \u{b7} use the mirror"
        );
    }

    #[test]
    fn an_approval_card_shows_only_while_that_approval_is_pending() {
        let events = || {
            vec![
                bash("t1", "git push"),
                ExecEvent::NeedsApproval {
                    request_id: "req".into(),
                    subject: subject("git push"),
                },
            ]
        };
        let held = build(events(), &[approval("git push")]);
        assert!(matches!(held.rows.last(), Some(Row::Approval { .. })));

        // Resolved: no card, a muted line.
        let resolved = build(events(), &[]);
        assert!(matches!(
            resolved.rows.last(),
            Some(Row::Asked { line, .. }) if line == "Asked to approve git push"
        ));
        assert!(
            !resolved
                .rows
                .iter()
                .any(|r| matches!(r, Row::Approval { .. }))
        );

        // An approval for another action does not make this one a card.
        let other = build(events(), &[approval("rm -rf /")]);
        assert!(matches!(other.rows.last(), Some(Row::Asked { .. })));

        // One pending approval backs one card, not two identical requests.
        let twice = build(
            vec![
                ExecEvent::NeedsApproval {
                    request_id: "a".into(),
                    subject: subject("git push"),
                },
                ExecEvent::NeedsApproval {
                    request_id: "b".into(),
                    subject: subject("git push"),
                },
            ],
            &[approval("git push")],
        );
        assert!(matches!(twice.rows[0], Row::Approval { .. }));
        assert!(matches!(twice.rows[1], Row::Asked { .. }));
    }

    #[test]
    fn a_result_that_is_the_final_answer_is_not_repeated_on_the_finished_line() {
        let done = |result: &str| ExecEvent::Done {
            stop: StopReason::Completed,
            result: Some(result.into()),
            diagnostic: None,
            reason: None,
        };
        let result_of = |built: &Built| match built.rows.last() {
            Some(Row::Done { result, .. }) => result.clone(),
            other => panic!("not done: {other:?}"),
        };
        let answer = "Here is **the answer**.\n\n- one\n- two";
        // The harness's result is the last message, said again.
        let same = build(
            vec![
                text("Here is **the answer**."),
                text("\n\n- one\n- two"),
                done(answer),
            ],
            &[],
        );
        assert_eq!(result_of(&same), None);
        // Trailing whitespace is not a difference.
        let spaced = build(vec![text(answer), done(&format!("{answer}\n"))], &[]);
        assert_eq!(result_of(&spaced), None);
        // A short summary that is not the message stays.
        let summary = build(vec![text(answer), done("mock run complete")], &[]);
        assert_eq!(result_of(&summary).as_deref(), Some("mock run complete"));
    }

    #[test]
    fn done_ends_the_list() {
        let built = build(
            vec![
                text("all good"),
                ExecEvent::Done {
                    stop: StopReason::Completed,
                    result: Some("fine".into()),
                    diagnostic: None,
                    reason: None,
                },
                text("ignored"),
                bash("t9", "ls"),
            ],
            &[],
        );
        assert_eq!(built.rows.len(), 2);
        assert!(matches!(
            built.rows.last(),
            Some(Row::Done { stop: StopReason::Completed, result: Some(r), .. }) if r == "fine"
        ));
    }

    #[test]
    fn a_resumed_run_goes_on_below_the_attempt_that_ended() {
        let built = build(
            vec![
                text("first"),
                ExecEvent::Done {
                    stop: StopReason::Interrupted,
                    result: None,
                    diagnostic: Some("cancelled".into()),
                    reason: Some(reno_proto::StopDetail::Shutdown),
                },
                text("ignored: nothing resumed yet"),
                ExecEvent::Resumed {
                    from_attempt: 1,
                    session: reno_proto::SessionRef("s".into()),
                },
                text("second"),
                ExecEvent::Done {
                    stop: StopReason::Completed,
                    result: Some("fine".into()),
                    diagnostic: None,
                    reason: None,
                },
            ],
            &[],
        );
        let shape: Vec<&str> = built
            .rows
            .iter()
            .map(|row| match row {
                Row::Prose { .. } => "prose",
                Row::Done { .. } => "done",
                Row::Asked { .. } => "resumed",
                other => panic!("{other:?}"),
            })
            .collect();
        assert_eq!(shape, ["prose", "done", "resumed", "prose", "done"]);
        // The facts are the last attempt's: the first one's stop reason is not carried over.
        assert_eq!(built.facts.reason, None);
        assert_eq!(built.facts.diagnostic, None);
    }

    #[test]
    fn every_event_kind_is_covered() {
        // One of each kind, in one transcript: nothing panics and each kind lands somewhere.
        let built = build(
            vec![
                ExecEvent::Started {
                    session: SessionRef("s".into()),
                    model: "m".into(),
                },
                ExecEvent::UserMessage { text: "u".into() },
                ExecEvent::Reasoning { text: "r".into() },
                text("t"),
                bash("t1", "ls"),
                result("t1", true, "ok"),
                ExecEvent::Usage {
                    input_tokens: 1,
                    output_tokens: 2,
                    cache_read_tokens: 0,
                    cache_write_tokens: 0,
                },
                ExecEvent::NeedsInput {
                    request_id: "q".into(),
                    questions: vec![],
                },
                ExecEvent::NeedsApproval {
                    request_id: "a".into(),
                    subject: subject("ls"),
                },
                ExecEvent::Done {
                    stop: StopReason::Error,
                    result: None,
                    diagnostic: Some("boom".into()),
                    reason: None,
                },
            ],
            &[],
        );
        let kinds: Vec<&str> = built
            .rows
            .iter()
            .map(|r| match r {
                Row::User { .. } => "user",
                Row::Prose { .. } => "prose",
                Row::Reasoning { .. } => "reasoning",
                Row::Tools { .. } => "tools",
                Row::Question { .. } => "question",
                Row::Approval { .. } => "approval",
                Row::Asked { .. } => "asked",
                Row::Routing { .. } => "routing",
                Row::ModelChanged { .. } => "model",
                Row::Divider { .. } => "divider",
                Row::Worked { .. } => "worked",
                Row::Done { .. } => "done",
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "user",
                "reasoning",
                "prose",
                "tools",
                "question",
                "asked",
                "done"
            ]
        );
        assert_eq!(built.facts.model.as_deref(), Some("m"));
        assert_eq!(
            built.facts.usage,
            Some(Usage {
                input: 1,
                output: 2
            })
        );
    }

    fn fold_kinds(rows: &[Row]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                Row::User { .. } => "user".to_owned(),
                Row::Prose { .. } => "prose".to_owned(),
                Row::Reasoning { .. } => "reasoning".to_owned(),
                Row::Tools { .. } => "tools".to_owned(),
                Row::Question { .. } => "question".to_owned(),
                Row::Done { .. } => "done".to_owned(),
                Row::Worked { rows, secs, .. } => {
                    format!("worked[{}]{secs:?}", fold_kinds(rows).join(","))
                }
                _ => "other".to_owned(),
            })
            .collect()
    }

    fn finish() -> ExecEvent {
        ExecEvent::Done {
            stop: StopReason::Completed,
            result: None,
            diagnostic: None,
            reason: None,
        }
    }

    #[test]
    fn a_finished_turn_folds_its_interim_steps_and_leaves_the_final_message() {
        let built = build_rows(
            Some("do it"),
            &at(vec![
                text("Let me look."),
                bash("t1", "ls"),
                result("t1", true, "ok"),
                text("All done."),
                finish(),
            ]),
            &[],
        );
        let rows = fold_turns(built.rows, Some(12_500));
        assert_eq!(
            fold_kinds(&rows),
            ["user", "worked[prose,tools]Some(12)", "prose", "done"]
        );
        let Row::Prose { text, .. } = &rows[2] else {
            panic!("the final message is bare");
        };
        assert_eq!(text, "All done.");
    }

    #[test]
    fn a_turn_without_its_done_row_is_not_folded() {
        let built = build_rows(
            Some("do it"),
            &at(vec![text("Let me look."), bash("t1", "ls"), text("More.")]),
            &[],
        );
        assert_eq!(
            fold_kinds(&fold_turns(built.rows.clone(), Some(1))),
            fold_kinds(&built.rows)
        );
    }

    #[test]
    fn a_turn_of_one_message_has_nothing_to_fold() {
        let built = build_rows(Some("hi"), &at(vec![text("Hello."), finish()]), &[]);
        assert_eq!(
            fold_kinds(&fold_turns(built.rows, Some(3_000))),
            ["user", "prose", "done"]
        );
    }

    #[test]
    fn a_turn_that_ends_on_a_step_folds_all_of_it() {
        let built = build_rows(
            Some("do it"),
            &at(vec![bash("t1", "ls"), result("t1", true, "ok"), finish()]),
            &[],
        );
        assert_eq!(
            fold_kinds(&fold_turns(built.rows, None)),
            ["user", "worked[tools]None", "done"]
        );
    }

    #[test]
    fn a_question_splits_the_turn_into_folds_that_carry_no_time() {
        let built = build_rows(
            Some("do it"),
            &at(vec![
                bash("t1", "ls"),
                asks("q1", vec![pick()]),
                ExecEvent::InputExpired {
                    request_id: "q1".into(),
                },
                bash("t2", "pwd"),
                text("Done."),
                finish(),
            ]),
            &[],
        );
        assert_eq!(
            fold_kinds(&fold_turns(built.rows, Some(9_000))),
            [
                "user",
                "worked[tools]None",
                "question",
                "worked[tools]None",
                "prose",
                "done"
            ]
        );
    }

    #[test]
    fn a_fold_has_its_own_key_and_moves_its_rows_into_the_segment() {
        let built = build_rows(
            Some("do it"),
            &at(vec![bash("t1", "ls"), text("Done."), finish()]),
            &[],
        );
        let rows = fold_turns(built.rows, None);
        let Row::Worked {
            key, rows: inner, ..
        } = rows[1].clone().in_segment(2)
        else {
            panic!("a fold");
        };
        assert_ne!(key, inner[0].key());
        assert_ne!(key, Key::fold(Key(1)), "moved into segment 2");
        assert_eq!(inner[0].key(), Key(1).in_segment(2));
        assert_ne!(rows[1].key(), rows[0].key());
        assert_ne!(Key::fold(Key(5)), Key(5));
    }

    #[test]
    fn the_prompt_is_stamped_with_the_runs_start_and_a_steer_is_not() {
        let mut rows = build_rows(
            Some("do it"),
            &at(vec![ExecEvent::UserMessage { text: "and".into() }]),
            &[],
        )
        .rows;
        stamp_prompt(&mut rows, 5_000);
        assert!(
            matches!(
                &rows[0],
                Row::User {
                    at_ms: Some(5_000),
                    ..
                }
            ),
            "{rows:?}"
        );
        assert!(
            matches!(
                &rows[1],
                Row::User {
                    at_ms: None,
                    steer: true,
                    ..
                }
            ),
            "{rows:?}"
        );
    }

    #[test]
    fn the_harness_asking_a_person_leaves_no_empty_fold_behind() {
        let built = build_rows(
            Some("do it"),
            &at(vec![
                asks("q1", vec![pick()]),
                ask_tool("a1"),
                result("a1", true, "User answered: \"Red\""),
                text("Done."),
                finish(),
            ]),
            &[],
        );
        assert_eq!(
            fold_kinds(&fold_turns(built.rows, Some(4_000))),
            ["user", "question", "prose", "done"]
        );
    }

    fn routing_view() -> RoutingView {
        use reno_proto::{RoutingDecision, RuleMatched};
        crate::vm::routing::view(&RoutingDecision {
            chosen: Some("codex".parse().unwrap()),
            ranked: vec!["codex".parse().unwrap()],
            rejected: vec![],
            rule_matched: RuleMatched::Default,
            fallbacks: vec![],
            at_ms: 1,
            warnings: Vec::new(),
        })
    }

    #[test]
    fn the_routing_card_follows_the_prompt() {
        let built = build_rows(Some("do it"), &at(vec![text("ok")]), &[]);
        let rows = with_routing(built.rows, Some(routing_view()));
        assert!(matches!(rows[0], Row::User { .. }));
        assert!(matches!(&rows[1], Row::Routing { key, .. } if *key == Key::ROUTING));
        assert!(matches!(rows[2], Row::Prose { .. }));
        // With no prompt known it leads; with no decision there is no card.
        let built = build_rows(None, &at(vec![text("ok")]), &[]);
        let rows = with_routing(built.rows.clone(), Some(routing_view()));
        assert!(matches!(rows[0], Row::Routing { .. }));
        assert_eq!(with_routing(built.rows.clone(), None), built.rows);
    }

    mod turn_builder {
        use super::*;

        /// What the one-shot builders make of `events`: the rows a `TurnBuilder` must hold.
        fn whole(events: &[(Seq, ExecEvent)], segment: usize, elapsed: Option<u64>) -> Built {
            let built = build_rows(None, events, &[]);
            let rows = fold_turns(built.rows, elapsed)
                .into_iter()
                .map(|row| row.in_segment(segment))
                .collect();
            Built {
                rows,
                facts: built.facts,
            }
        }

        /// The first top-level row a change says is not what it was.
        fn first_changed(rows: &[Row], change: &Change) -> Option<usize> {
            let key = match change {
                Change::Grew(key) | Change::Added(key) => *key,
                Change::Folded { turn, .. } => return Some(turn.start),
                Change::Unchanged => return None,
            };
            rows.iter().position(|row| row.key() == key)
        }

        /// Feeds `events` one at a time and, after each, holds the builder to the one-shot result
        /// and to its own change: the rows before the reported one are as they were.
        fn matches_event_by_event(events: &[(Seq, ExecEvent)], segment: usize) {
            let mut builder = TurnBuilder::new(segment);
            let mut before: Vec<Row> = Vec::new();
            for (i, (seq, event)) in events.iter().enumerate() {
                let change = builder.push(*seq, event);
                let expected = whole(&events[..=i], segment, None);
                assert_eq!(
                    builder.rows(),
                    expected.rows,
                    "rows after event {i}: {event:?}"
                );
                assert_eq!(builder.facts(), &expected.facts, "facts after event {i}");
                let reported = first_changed(builder.rows(), &change);
                let stable = match &change {
                    Change::Unchanged => before.len().min(builder.rows().len()),
                    _ => reported.unwrap_or(0),
                };
                assert!(
                    reported.is_some() || change == Change::Unchanged,
                    "a change names a row of the list: {change:?} after event {i}"
                );
                assert_eq!(
                    before[..stable.min(before.len())],
                    builder.rows()[..stable.min(before.len())],
                    "rows before the reported one moved after event {i}: {change:?}"
                );
                if change == Change::Unchanged {
                    assert_eq!(before, builder.rows(), "unchanged but moved at event {i}");
                }
                before = builder.rows().to_vec();
            }
        }

        fn started() -> ExecEvent {
            ExecEvent::Started {
                session: SessionRef("s".into()),
                model: "m".into(),
            }
        }

        fn usage() -> ExecEvent {
            ExecEvent::Usage {
                input_tokens: 1,
                output_tokens: 2,
                cache_read_tokens: 3,
                cache_write_tokens: 4,
            }
        }

        #[test]
        fn a_turn_builder_matches_build_rows_event_by_event() {
            let events = reno_client::fake::transcript::seeded_run_events("acp3");
            assert!(events.len() > 10, "the seeded run has events");
            matches_event_by_event(&events, 0);
            matches_event_by_event(&events, 2);
            // The turn after the first Done, which `Resumed` opens.
            let events = reno_client::fake::transcript::seeded_run_events("render2");
            assert!(events.len() > 10, "the second render run has events");
            matches_event_by_event(&events, 1);
        }

        #[test]
        fn a_turn_builder_matches_on_hand_built_runs_of_every_event_kind() {
            let resumed = |from: u32| ExecEvent::Resumed {
                from_attempt: from,
                session: SessionRef("s".into()),
            };
            let runs: Vec<Vec<ExecEvent>> = vec![
                // A question-only tool group ends a prose run, and the next call joins it.
                vec![
                    text("a"),
                    ask_tool("a1"),
                    text("b"),
                    ask_tool("a2"),
                    bash("t1", "ls"),
                    result("a2", true, "User answered: \"Red\""),
                    result("t1", true, "ok"),
                    text("c"),
                    finish(),
                ],
                // The call comes first, the question after it, and the result last: the group
                // that held the call is already left behind.
                vec![
                    ask_tool("a1"),
                    asks("q1", vec![pick()]),
                    result("a1", true, "User answered: \"Red\""),
                    result("a1", true, "said twice"),
                    text("ok"),
                    finish(),
                ],
                // A question, its result, then a turn that goes on past it.
                vec![
                    bash("t0", "ls"),
                    asks("q1", vec![pick()]),
                    ask_tool("a1"),
                    result("a1", true, "User answered: \"Red\""),
                    asks("q2", vec![pick()]),
                    ExecEvent::InputExpired {
                        request_id: "q2".into(),
                    },
                    ExecEvent::InputExpired {
                        request_id: "nobody".into(),
                    },
                    bash("t1", "pwd"),
                    result("t1", false, "no"),
                    text("end"),
                    finish(),
                ],
                // A result for a call from a turn that has folded, after a resume; the last
                // prose a result repeats is in a fold.
                vec![
                    bash("t1", "sleep 9"),
                    text("waiting"),
                    ExecEvent::Done {
                        stop: StopReason::Interrupted,
                        result: None,
                        diagnostic: None,
                        reason: None,
                    },
                    text("ignored after the end"),
                    resumed(1),
                    result("t1", true, "late"),
                    result("orphan", true, "nothing called it"),
                    ExecEvent::Done {
                        stop: StopReason::Completed,
                        result: Some("waiting".into()),
                        diagnostic: None,
                        reason: None,
                    },
                ],
                // A question left open when the run ends, a steer, an approval, a report.
                vec![
                    started(),
                    ExecEvent::Reasoning { text: "hm".into() },
                    ExecEvent::Reasoning { text: "mm".into() },
                    usage(),
                    ExecEvent::UserMessage {
                        text: "also".into(),
                    },
                    asks("q1", vec![pick()]),
                    ExecEvent::NeedsApproval {
                        request_id: "r".into(),
                        subject: subject("ls"),
                    },
                    ExecEvent::ApprovalResolved {
                        request_id: "r".into(),
                        decision: ApprovalOutcome::Approved,
                        by: ResolvedBy::Human,
                        message: None,
                    },
                    ExecEvent::AgentReport {
                        status: ReportStatus::Progress,
                        summary: "halfway".into(),
                        details: None,
                    },
                    ExecEvent::ModelChanged { model: "n".into() },
                    finish(),
                ],
                vec![],
            ];
            for events in runs {
                matches_event_by_event(&at(events.clone()), 0);
                matches_event_by_event(&at(events), 3);
            }
        }

        #[test]
        fn a_done_folds_the_turn_as_one_change() {
            let events = at(vec![
                text("Let me look."),
                bash("t1", "ls"),
                result("t1", true, "ok"),
                text("All done."),
            ]);
            let mut builder = TurnBuilder::new(0);
            for (seq, event) in &events {
                builder.push(*seq, event);
            }
            assert_eq!(builder.rows().len(), 3, "the turn is still loose");
            let change = builder.push(Seq(9), &finish());
            let Change::Folded { turn, fold } = change else {
                panic!("the Done folds the turn: {change:?}");
            };
            assert_eq!(turn, 0..3, "the fold, the final message and the Done");
            assert_eq!(builder.rows()[0].key(), fold);
            assert!(matches!(&builder.rows()[0], Row::Worked { rows, .. } if rows.len() == 2));
            assert!(matches!(builder.rows()[1], Row::Prose { .. }));
            assert!(matches!(builder.rows()[2], Row::Done { .. }));
        }

        #[test]
        fn a_delta_grows_the_last_row_and_a_new_kind_of_event_adds_one() {
            let mut builder = TurnBuilder::new(0);
            let first = builder.push(Seq(1), &text("Hel"));
            assert_eq!(first, Change::Added(Key(2)));
            assert_eq!(builder.push(Seq(2), &text("lo")), Change::Grew(Key(2)));
            assert_eq!(builder.push(Seq(3), &started()), Change::Unchanged);
            assert_eq!(builder.push(Seq(4), &usage()), Change::Unchanged);
            assert_eq!(
                builder.push(Seq(5), &bash("t1", "ls")),
                Change::Added(Key(6))
            );
            // Its result patches the group in place.
            assert_eq!(
                builder.push(Seq(6), &result("t1", true, "ok")),
                Change::Grew(Key(6))
            );
            assert_eq!(builder.facts().model.as_deref(), Some("m"));
        }

        #[test]
        fn the_runs_time_goes_on_a_lone_fold_and_only_while_the_run_is_one_stretch() {
            let turn = [text("a"), bash("t1", "ls"), text("b"), finish()];
            let events = at(turn.to_vec());
            let mut builder = TurnBuilder::new(0);
            for (seq, event) in &events {
                builder.push(*seq, event);
            }
            assert_eq!(
                fold_kinds(builder.rows()),
                ["worked[prose,tools]None", "prose", "done"]
            );
            let change = builder.set_run_time(true, 1_000, 8_000);
            assert!(matches!(change, Change::Grew(_)), "{change:?}");
            let expected = fold_turns(
                build_rows(None, &events, &[]).rows,
                crate::vm::convo::run_elapsed_ms(true, 1_000, 8_000, &events),
            );
            assert_eq!(builder.rows(), expected);
            assert_eq!(fold_kinds(builder.rows())[0], "worked[prose,tools]Some(7)");
            assert_eq!(builder.set_run_time(true, 1_000, 8_000), Change::Unchanged);
            // A run that is still going has no time; neither has one a person was asked in.
            assert!(matches!(
                builder.set_run_time(false, 1_000, 8_000),
                Change::Grew(_)
            ));
            assert_eq!(fold_kinds(builder.rows())[0], "worked[prose,tools]None");
            let mut asked = TurnBuilder::new(0);
            for (seq, event) in &at(vec![
                bash("t1", "ls"),
                ExecEvent::UserMessage { text: "and".into() },
                text("b"),
                finish(),
            ]) {
                asked.push(*seq, event);
            }
            assert_eq!(asked.set_run_time(true, 0, 9_000), Change::Unchanged);
        }

        #[test]
        fn a_fold_made_after_the_time_is_known_carries_it() {
            let mut builder = TurnBuilder::new(0);
            builder.push(Seq(1), &bash("t1", "ls"));
            builder.push(Seq(2), &text("done"));
            builder.set_run_time(true, 0, 3_000);
            builder.push(Seq(3), &finish());
            assert_eq!(fold_kinds(builder.rows())[0], "worked[tools]Some(3)");
        }

        #[test]
        fn the_call_index_finds_each_tools_input() {
            let events = reno_client::fake::transcript::seeded_run_events("render2");
            let mut builder = TurnBuilder::new(0);
            for (seq, event) in &events {
                builder.push(*seq, event);
            }
            let mut calls = 0;
            for (seq, event) in &events {
                if let ExecEvent::Tool { id, call } = event {
                    calls += 1;
                    let found = builder.call(id).expect("every call is indexed");
                    assert_eq!(found.input, call, "the call {id}");
                    let result = events.iter().find_map(|(s, e)| match e {
                        ExecEvent::ToolResult { id: rid, .. } if rid == id && s > seq => Some(*s),
                        _ => None,
                    });
                    assert_eq!(found.result_seq, result, "the result of {id}");
                }
            }
            assert!(calls >= 5, "the run calls several tools");
            assert!(builder.call("no such call").is_none());
        }

        #[test]
        fn a_call_that_ended_before_it_answered_has_no_result_seq() {
            let mut builder = TurnBuilder::new(0);
            builder.push(Seq(1), &bash("t1", "sleep 9"));
            let found = builder.call("t1").unwrap();
            assert_eq!(found.result_seq, None);
            builder.push(Seq(2), &result("t1", true, "ok"));
            assert_eq!(builder.call("t1").unwrap().result_seq, Some(Seq(2)));
        }
    }
}
