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
//! - `Usage` and `Started` feed [`Facts`] for the details pane and make no row.
//! - `Done` is the last row; anything after it is ignored.

use reno_proto::{Approval, ApprovalSubject, ExecEvent, Question, Seq, StopReason};

use crate::summary;

/// Stable identity of a row: the seq of the event that opened it, plus one. The prompt is 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Key(pub(crate) u64);

impl Key {
    const PROMPT: Self = Self(0);

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

/// One tool call in a group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ToolEntry {
    pub(crate) id: String,
    pub(crate) kind: ToolKind,
    /// The command, path or URL, else the tool's name.
    pub(crate) label: String,
    pub(crate) outcome: Outcome,
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
    /// The harness asks questions.
    Question {
        key: Key,
        request_id: String,
        questions: Vec<Question>,
    },
    /// An inline approval card: the approval is pending.
    Approval { key: Key, approval: Approval },
    /// A past approval request, one muted line.
    Asked { key: Key, line: String },
    /// The final status line.
    Done {
        key: Key,
        stop: StopReason,
        result: Option<String>,
        diagnostic: Option<String>,
    },
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
            | Self::Done { key, .. } => *key,
        }
    }
}

/// Token usage so far.
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
}

/// The result of [`build_rows`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Built {
    pub(crate) rows: Vec<Row>,
    pub(crate) facts: Facts,
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
    for (seq, event) in events {
        let key = Key::at(*seq);
        match event {
            ExecEvent::Started { model, .. } => built.facts.model = Some(model.clone()),
            ExecEvent::Usage {
                input_tokens,
                output_tokens,
            } => {
                built.facts.usage = Some(Usage {
                    input: *input_tokens,
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
                let entry = ToolEntry {
                    id: id.clone(),
                    kind: tool_kind(&call.name),
                    label: tool_label(&call.name, &call.input),
                    outcome: Outcome::Pending,
                };
                match built.rows.last_mut() {
                    Some(Row::Tools { calls, .. }) => calls.push(entry),
                    _ => built.rows.push(Row::Tools {
                        key,
                        calls: vec![entry],
                    }),
                }
            }
            ExecEvent::ToolResult { id, ok, summary } => {
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
                    }
                } else {
                    let orphan = ToolEntry {
                        id: id.clone(),
                        kind: ToolKind::Other,
                        label: "tool result".to_owned(),
                        outcome,
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
            ExecEvent::NeedsInput {
                request_id,
                questions,
            } => built.rows.push(Row::Question {
                key,
                request_id: request_id.clone(),
                questions: questions.clone(),
            }),
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
            ExecEvent::Done {
                stop,
                result,
                diagnostic,
            } => {
                built.rows.push(Row::Done {
                    key,
                    stop: *stop,
                    result: result.clone(),
                    diagnostic: diagnostic.clone(),
                });
                break;
            }
        }
    }
    built
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
        command.to_owned()
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
            call: ToolCall {
                name: "bash".into(),
                input: json!({ "command": cmd }),
            },
        }
    }

    fn result(id: &str, ok: bool, s: &str) -> ExecEvent {
        ExecEvent::ToolResult {
            id: id.into(),
            ok,
            summary: Some(s.into()),
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
                },
                ExecEvent::Usage {
                    input_tokens: 30,
                    output_tokens: 9,
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
    fn questions_are_rows() {
        let q = Question {
            id: "a".into(),
            prompt: "Which?".into(),
            options: vec!["x".into()],
            multi_select: false,
        };
        let built = build(
            vec![ExecEvent::NeedsInput {
                request_id: "q1".into(),
                questions: vec![q.clone()],
            }],
            &[],
        );
        assert_eq!(
            built.rows,
            vec![Row::Question {
                key: Key(1),
                request_id: "q1".into(),
                questions: vec![q]
            }]
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
    fn done_ends_the_list() {
        let built = build(
            vec![
                text("all good"),
                ExecEvent::Done {
                    stop: StopReason::Completed,
                    result: Some("fine".into()),
                    diagnostic: None,
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
}
