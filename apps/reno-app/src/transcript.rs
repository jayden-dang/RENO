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
    Approval, ApprovalOutcome, ApprovalSubject, ExecEvent, Question, ReportStatus, ResolvedBy, Seq,
    StopReason, ToolOutput,
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
    User { key: Key, text: String, steer: bool },
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
fn is_input_tool(name: &str) -> bool {
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
            | Self::Done { key, .. } => *key,
        }
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

/// The status line of a resolved approval: the outcome, then who decided.
fn resolved_line(decision: ApprovalOutcome, by: &ResolvedBy, message: Option<&str>) -> String {
    let outcome = match decision {
        ApprovalOutcome::Approved => "Approved",
        ApprovalOutcome::Denied => "Denied",
    };
    match by {
        ResolvedBy::Human => format!("{outcome} by you"),
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
fn tool_label(name: &str, input: &serde_json::Value) -> String {
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

/// "Ran 3 commands", "Edited 14 files, ran 2 commands", "Used 1 tool".
pub(crate) fn group_title(calls: &[ToolEntry]) -> String {
    let count = |kind| calls.iter().filter(|c| c.kind == kind).count();
    let plural = |n: usize, one: &str| format!("{n} {one}{}", if n == 1 { "" } else { "s" });
    let mut parts = Vec::new();
    let edits = count(ToolKind::Edit);
    if edits > 0 {
        parts.push(format!("edited {}", plural(edits, "file")));
    }
    let commands = count(ToolKind::Command);
    if commands > 0 {
        parts.push(format!("ran {}", plural(commands, "command")));
    }
    let other = count(ToolKind::Other);
    if other > 0 {
        parts.push(format!("used {}", plural(other, "tool")));
    }
    let mut title = parts.join(", ");
    if let Some(first) = title.get_mut(..1) {
        first.make_ascii_uppercase();
    }
    title
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
        assert_eq!(group_title(calls), "Ran 2 commands");
        let Row::Tools { calls, .. } = &built.rows[2] else {
            panic!("expected a second group");
        };
        assert_eq!(calls[0].outcome, Outcome::Pending, "no result yet");
        assert_eq!(group_title(calls), "Ran 1 command");
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
    fn group_titles_mix_kinds() {
        let entry = |kind| ToolEntry {
            id: String::new(),
            kind,
            label: String::new(),
            outcome: Outcome::Pending,
            output: None,
        };
        let mixed = [
            entry(ToolKind::Edit),
            entry(ToolKind::Edit),
            entry(ToolKind::Command),
            entry(ToolKind::Other),
        ];
        assert_eq!(
            group_title(&mixed),
            "Edited 2 files, ran 1 command, used 1 tool"
        );
        assert_eq!(group_title(&mixed[3..]), "Used 1 tool");
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
}
