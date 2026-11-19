//! The conversation in words: what the transcript's rows say, as plain functions.
//!
//! A tool group becomes one muted line per step ("Ran cargo test", "Edited 3 files"), a long user
//! message collapses behind "Show more", a finished turn folds behind "Worked for 12s", and a
//! question that can no longer be answered is one line. None of this draws or reads a clock, so
//! every rule is a unit test; `screens/run/transcript.rs` only draws what these return.

use std::collections::HashSet;

use reno_proto::{ExecEvent, Question, Seq};

use crate::transcript::{Outcome, Resolution, Row, ToolEntry, ToolKind};
use crate::vm::routing::RoutingView;

/// A user message longer than this many characters collapses.
pub(crate) const COLLAPSE_CHARS: usize = 320;

/// A user message as shown: the text, and the toggle under it when it is long.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct UserText {
    pub(crate) shown: String,
    /// "Show more" or "Show less"; none for a message that fits.
    pub(crate) toggle: Option<&'static str>,
}

pub(crate) fn user_text(text: &str, expanded: bool) -> UserText {
    let Some((cut, _)) = text.char_indices().nth(COLLAPSE_CHARS) else {
        return UserText {
            shown: text.to_owned(),
            toggle: None,
        };
    };
    if expanded {
        return UserText {
            shown: text.to_owned(),
            toggle: Some("Show less"),
        };
    }
    // Back to the last space, so a word is not cut in two (unless it is one long word).
    let head = &text[..cut];
    let head = head
        .rfind(char::is_whitespace)
        .filter(|&at| at > cut / 2)
        .map_or(head, |at| &head[..at]);
    UserText {
        shown: format!("{}\u{2026}", head.trim_end()),
        toggle: Some("Show more"),
    }
}

/// The leading icon of a step line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StepIcon {
    Terminal,
    Read,
    Edit,
    Fetch,
    Tool,
}

/// One muted line of tool activity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StepLine {
    pub(crate) icon: StepIcon,
    pub(crate) text: String,
    /// The step has not finished: it shimmers.
    pub(crate) live: bool,
    pub(crate) failed: bool,
    /// The call whose output opens from this line (none for a line of several edits).
    pub(crate) call: Option<String>,
}

fn plural(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

/// What a call that is not a command or an edit did, from its label ("Read a.rs", "GET url",
/// "grep", "mystery"): the icon, the verb in the past and in the present, and what it did it to.
fn other_step(label: &str) -> (StepIcon, &'static str, &'static str, &str) {
    if let Some(url) = label.strip_prefix("GET ") {
        return (StepIcon::Fetch, "Fetched", "Fetching", url);
    }
    let (name, rest) = label.split_once(' ').unwrap_or((label, ""));
    let lower = name.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| lower.contains(w));
    if !rest.is_empty() && has(&["read", "view", "cat"]) {
        (StepIcon::Read, "Read", "Reading", rest)
    } else if has(&["grep", "glob", "search", "find"]) {
        (StepIcon::Read, "Searched", "Searching", rest)
    } else {
        (StepIcon::Tool, "Used", "Using", label)
    }
}

/// Whether a call is the harness asking a person (its label is the tool's name).
fn is_input_tool(label: &str) -> bool {
    label.eq_ignore_ascii_case("AskUserQuestion")
        || label.eq_ignore_ascii_case("request_user_input")
}

/// One line for each step; a run of edits is one line, "Edited 3 files".
pub(crate) fn step_lines(calls: &[ToolEntry]) -> Vec<StepLine> {
    let mut lines = Vec::new();
    let mut at = 0;
    while at < calls.len() {
        let call = &calls[at];
        // The harness asking a person is the question row's, not a step.
        if call.kind == ToolKind::Other && is_input_tool(&call.label) {
            at += 1;
            continue;
        }
        let live = call.outcome == Outcome::Pending;
        let failed = matches!(call.outcome, Outcome::Failed(_));
        match call.kind {
            ToolKind::Edit => {
                let run = calls[at..]
                    .iter()
                    .take_while(|c| c.kind == ToolKind::Edit)
                    .collect::<Vec<_>>();
                let files: HashSet<&str> = run
                    .iter()
                    .map(|c| c.label.split_once(' ').map_or(c.label.as_str(), |(_, p)| p))
                    .collect();
                let any_live = run.iter().any(|c| c.outcome == Outcome::Pending);
                let files = plural(files.len(), "file");
                lines.push(StepLine {
                    icon: StepIcon::Edit,
                    text: format!("{} {files}", if any_live { "Editing" } else { "Edited" }),
                    live: any_live,
                    failed: run.iter().any(|c| matches!(c.outcome, Outcome::Failed(_))),
                    call: (run.len() == 1).then(|| call.id.clone()),
                });
                at += run.len();
                continue;
            }
            ToolKind::Command => lines.push(StepLine {
                icon: StepIcon::Terminal,
                text: format!("{} {}", if live { "Running" } else { "Ran" }, call.label),
                live,
                failed,
                call: Some(call.id.clone()),
            }),
            ToolKind::Other => {
                let (icon, past, present, target) = other_step(&call.label);
                lines.push(StepLine {
                    icon,
                    text: format!("{} {target}", if live { present } else { past })
                        .trim_end()
                        .to_owned(),
                    live,
                    failed,
                    call: Some(call.id.clone()),
                });
            }
        }
        at += 1;
    }
    lines
}

/// "Worked for 12s", "Worked for 1m 5s", or "Worked" when the turn has no timing.
pub(crate) fn worked_label(secs: Option<u64>) -> String {
    match secs {
        None => "Worked".to_owned(),
        Some(0) => "Worked for <1s".to_owned(),
        Some(s) if s < 60 => format!("Worked for {s}s"),
        Some(s) if s % 60 == 0 => format!("Worked for {}m", s / 60),
        Some(s) => format!("Worked for {}m {}s", s / 60, s % 60),
    }
}

/// How long a finished run worked, from the run's own times: only where they bound exactly one
/// turn. `None` for a run still going, one that was steered or resumed (its span holds more than
/// one turn's work), or times that run backwards.
pub(crate) fn run_elapsed_ms(
    ended: bool,
    created_ms: u64,
    updated_ms: u64,
    events: &[(Seq, ExecEvent)],
) -> Option<u64> {
    let more_than_one_turn = events
        .iter()
        .any(|(_, e)| matches!(e, ExecEvent::UserMessage { .. } | ExecEvent::Resumed { .. }));
    (ended && !more_than_one_turn && updated_ms >= created_ms).then(|| updated_ms - created_ms)
}

/// What a question shows as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum QuestionView {
    /// Still open: the card.
    Card,
    /// Answered: each question muted above its answer, in a user bubble.
    Answered(Vec<(String, Option<String>)>),
    /// No longer answerable: one muted line.
    Closed(String),
}

pub(crate) fn question_view(questions: &[Question], resolution: &Resolution) -> QuestionView {
    match resolution {
        Resolution::Pending => QuestionView::Card,
        Resolution::Answered(answers) => QuestionView::Answered(
            questions
                .iter()
                .enumerate()
                .map(|(i, q)| (q.prompt.clone(), answers.get(i).cloned().flatten()))
                .collect(),
        ),
        Resolution::Declined => {
            QuestionView::Closed("Question closed: nobody answered in time".to_owned())
        }
        Resolution::Unanswered => {
            QuestionView::Closed("Question closed: the run ended without an answer".to_owned())
        }
    }
}

/// "Routed to Codex \u{b7} Claude Code was signed out": the headline and the reason (the rule
/// that ordered the candidates when nothing was passed over).
pub(crate) fn routing_line(view: &RoutingView) -> String {
    let reason = view.because.as_deref().unwrap_or(view.rule.as_str());
    format!("{} \u{b7} {reason}", view.headline)
}

/// Whether the "Thinking" shimmer shows: the run is live and the agent is between steps, not
/// streaming text, not running a step, not waiting on a person.
pub(crate) fn thinking(rows: &[Row], live: bool) -> bool {
    live && match rows
        .iter()
        .rev()
        .find(|r| !matches!(r, Row::Routing { .. }))
    {
        None | Some(Row::User { .. } | Row::Reasoning { .. }) => true,
        Some(Row::Tools { calls, .. }) => calls.iter().all(ToolEntry::finished),
        Some(_) => false,
    }
}

/// The row that is the final agent message of a finished turn (it carries the quiet actions).
pub(crate) fn final_message_ix(rows: &[Row]) -> Option<usize> {
    let done = rows.iter().rposition(|r| matches!(r, Row::Done { .. }))?;
    let at = done.checked_sub(1)?;
    matches!(rows[at], Row::Prose { .. }).then_some(at)
}

/// The tick rail: the index of each user message's row.
pub(crate) fn ticks(rows: &[Row]) -> Vec<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, r)| matches!(r, Row::User { .. }))
        .map(|(i, _)| i)
        .collect()
}

/// A shimmer's opacity at `phase` (0..1); fixed while motion is reduced.
pub(crate) fn shimmer_opacity(phase: f32, reduced: bool) -> f32 {
    if reduced {
        return 0.8;
    }
    let wave = (phase * std::f32::consts::TAU).sin().mul_add(0.5, 0.5);
    0.45 + 0.55 * wave
}

/// The wall-clock time of a message, "14:05".
pub(crate) fn clock_label<Tz: chrono::TimeZone>(at_ms: u64, zone: &Tz) -> String
where
    Tz::Offset: std::fmt::Display,
{
    i64::try_from(at_ms)
        .ok()
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|t| t.with_timezone(zone).format("%H:%M").to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use reno_proto::{ActionClass, ExecEvent, Question, Seq, StopReason, ToolCall};
    use serde_json::json;

    use super::*;
    use crate::transcript::{Key, Row};

    fn call(id: &str, kind: ToolKind, label: &str, outcome: Outcome) -> ToolEntry {
        ToolEntry {
            id: id.into(),
            kind,
            label: label.into(),
            outcome,
            output: None,
        }
    }

    fn ok() -> Outcome {
        Outcome::Ok(None)
    }

    #[test]
    fn a_short_user_message_shows_whole_with_no_toggle() {
        let text = "x".repeat(COLLAPSE_CHARS);
        let shown = user_text(&text, false);
        assert_eq!(shown.shown, text);
        assert_eq!(shown.toggle, None);
    }

    #[test]
    fn a_long_user_message_collapses_behind_show_more_and_opens() {
        let text = "word ".repeat(100);
        let closed = user_text(&text, false);
        assert!(
            closed.shown.chars().count() <= COLLAPSE_CHARS + 1,
            "{closed:?}"
        );
        assert!(closed.shown.starts_with("word word"));
        assert!(closed.shown.ends_with('\u{2026}'));
        assert_eq!(closed.toggle, Some("Show more"));
        let open = user_text(&text, true);
        assert_eq!(open.shown, text);
        assert_eq!(open.toggle, Some("Show less"));
        // Multi-byte text is cut on a character, not in the middle of one.
        let wide = "\u{e9}".repeat(COLLAPSE_CHARS + 5);
        assert_eq!(user_text(&wide, false).toggle, Some("Show more"));
    }

    #[test]
    fn a_collapsed_message_is_cut_between_words_not_in_one() {
        let text = "abcdefghij ".repeat(40);
        let closed = user_text(&text, false);
        assert!(
            closed.shown.ends_with("abcdefghij\u{2026}"),
            "{:?}",
            closed.shown
        );
    }

    #[test]
    fn the_harness_asking_a_person_is_not_a_step() {
        let lines = step_lines(&[
            call("1", ToolKind::Other, "AskUserQuestion", ok()),
            call("2", ToolKind::Command, "ls", ok()),
            call("3", ToolKind::Other, "request_user_input", Outcome::Pending),
        ]);
        let texts: Vec<_> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["Ran ls"]);
    }

    #[test]
    fn steps_read_as_one_muted_line_each() {
        let lines = step_lines(&[
            call("1", ToolKind::Command, "cargo test", ok()),
            call("2", ToolKind::Other, "Read src/main.rs", ok()),
            call("3", ToolKind::Other, "GET https://x.test/a", ok()),
            call("4", ToolKind::Other, "mystery", ok()),
        ]);
        let texts: Vec<_> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "Ran cargo test",
                "Read src/main.rs",
                "Fetched https://x.test/a",
                "Used mystery"
            ]
        );
        let icons: Vec<_> = lines.iter().map(|l| l.icon).collect();
        assert_eq!(
            icons,
            [
                StepIcon::Terminal,
                StepIcon::Read,
                StepIcon::Fetch,
                StepIcon::Tool
            ]
        );
        assert_eq!(lines[0].call.as_deref(), Some("1"));
    }

    #[test]
    fn edits_run_together_into_one_line_of_n_files() {
        let lines = step_lines(&[
            call("1", ToolKind::Edit, "edit a.rs", ok()),
            call("2", ToolKind::Edit, "write b.rs", ok()),
            call("3", ToolKind::Edit, "edit a.rs", ok()),
            call("4", ToolKind::Command, "ls", ok()),
            call("5", ToolKind::Edit, "edit c.rs", ok()),
        ]);
        let texts: Vec<_> = lines.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, ["Edited 2 files", "Ran ls", "Edited 1 file"]);
        assert_eq!(lines[0].icon, StepIcon::Edit);
        assert_eq!(lines[0].call, None);
    }

    #[test]
    fn a_live_step_says_running_and_a_failed_one_is_marked() {
        let lines = step_lines(&[
            call("1", ToolKind::Command, "cargo test", Outcome::Pending),
            call("2", ToolKind::Other, "Read a.rs", Outcome::Pending),
            call("3", ToolKind::Edit, "edit a.rs", Outcome::Pending),
            call("4", ToolKind::Command, "false", Outcome::Failed(None)),
        ]);
        assert_eq!(lines[0].text, "Running cargo test");
        assert!(lines[0].live);
        assert_eq!(lines[1].text, "Reading a.rs");
        assert_eq!(lines[2].text, "Editing 1 file");
        assert!(lines[2].live);
        assert_eq!(lines[3].text, "Ran false");
        assert!(lines[3].failed && !lines[3].live);
    }

    #[test]
    fn the_fold_label_has_a_number_only_when_it_has_timing() {
        assert_eq!(worked_label(Some(12)), "Worked for 12s");
        assert_eq!(worked_label(Some(65)), "Worked for 1m 5s");
        assert_eq!(worked_label(Some(120)), "Worked for 2m");
        assert_eq!(worked_label(Some(0)), "Worked for <1s");
        assert_eq!(worked_label(None), "Worked");
    }

    fn started() -> (Seq, ExecEvent) {
        (Seq(1), ExecEvent::Text { text: "hi".into() })
    }

    #[test]
    fn elapsed_comes_from_the_run_times_only_for_one_turn() {
        let one = [started()];
        assert_eq!(run_elapsed_ms(true, 1_000, 13_500, &one), Some(12_500));
        // Still going: no end yet.
        assert_eq!(run_elapsed_ms(false, 1_000, 13_500, &one), None);
        // Times that run backwards are not a duration.
        assert_eq!(run_elapsed_ms(true, 9_000, 1_000, &one), None);
        // A steer or a resume puts more than one turn's work in the span.
        let steered = [
            started(),
            (Seq(2), ExecEvent::UserMessage { text: "and".into() }),
        ];
        assert_eq!(run_elapsed_ms(true, 1_000, 13_500, &steered), None);
        let resumed = [
            started(),
            (
                Seq(2),
                ExecEvent::Resumed {
                    from_attempt: 0,
                    session: reno_proto::SessionRef("s".into()),
                },
            ),
        ];
        assert_eq!(run_elapsed_ms(true, 1_000, 13_500, &resumed), None);
    }

    fn q() -> Question {
        Question {
            id: "a".into(),
            prompt: "Which colour?".into(),
            options: vec!["Red".into(), "Blue".into()],
            multi_select: false,
        }
    }

    #[test]
    fn a_question_is_a_card_while_open_a_bubble_when_answered_a_line_when_closed() {
        assert_eq!(
            question_view(&[q()], &Resolution::Pending),
            QuestionView::Card
        );
        assert_eq!(
            question_view(&[q()], &Resolution::Answered(vec![Some("Red".into())])),
            QuestionView::Answered(vec![("Which colour?".into(), Some("Red".into()))])
        );
        assert_eq!(
            question_view(&[q()], &Resolution::Declined),
            QuestionView::Closed("Question closed: nobody answered in time".into())
        );
        assert_eq!(
            question_view(&[q()], &Resolution::Unanswered),
            QuestionView::Closed("Question closed: the run ended without an answer".into())
        );
    }

    fn user(key: u64) -> Row {
        Row::User {
            key: Key(key),
            text: "hi".into(),
            steer: false,
            at_ms: None,
        }
    }
    fn prose(key: u64) -> Row {
        Row::Prose {
            key: Key(key),
            text: "p".into(),
        }
    }
    fn tools(key: u64, outcome: Outcome) -> Row {
        Row::Tools {
            key: Key(key),
            calls: vec![call("t", ToolKind::Command, "ls", outcome)],
        }
    }
    fn done(key: u64) -> Row {
        Row::Done {
            key: Key(key),
            stop: StopReason::Completed,
            result: None,
            diagnostic: None,
            reason: None,
        }
    }

    #[test]
    fn thinking_shows_between_steps_of_a_live_run_only() {
        assert!(thinking(&[user(0)], true));
        assert!(thinking(&[user(0), tools(1, ok())], true));
        assert!(thinking(
            &[
                user(0),
                Row::Reasoning {
                    key: Key(1),
                    text: "hm".into()
                }
            ],
            true
        ));
        assert!(!thinking(&[user(0)], false), "a run that is not live");
        assert!(!thinking(&[user(0), prose(1)], true), "text is streaming");
        assert!(
            !thinking(&[user(0), tools(1, Outcome::Pending)], true),
            "the running step shimmers itself"
        );
        assert!(!thinking(&[user(0), done(2)], true));
    }

    #[test]
    fn only_the_last_prose_before_done_is_the_final_message() {
        assert_eq!(
            final_message_ix(&[user(0), prose(1), tools(2, ok()), prose(3), done(4)]),
            Some(3)
        );
        assert_eq!(final_message_ix(&[user(0), prose(1), done(2)]), Some(1));
        // Live, or ending on a step: no final message.
        assert_eq!(final_message_ix(&[user(0), prose(1)]), None);
        assert_eq!(
            final_message_ix(&[user(0), prose(1), tools(2, ok()), done(3)]),
            None
        );
    }

    #[test]
    fn the_tick_rail_has_one_tick_per_user_message() {
        let rows = [user(0), prose(1), user(2), tools(3, ok()), user(4)];
        assert_eq!(ticks(&rows), [0, 2, 4]);
        assert!(ticks(&[prose(1)]).is_empty());
    }

    #[test]
    fn the_shimmer_is_still_when_motion_is_reduced() {
        let still = shimmer_opacity(0.0, true);
        for phase in [0.1, 0.25, 0.5, 0.9] {
            assert_eq!(shimmer_opacity(phase, true), still);
        }
        assert_ne!(shimmer_opacity(0.0, false), shimmer_opacity(0.5, false));
        for phase in [0.0, 0.3, 0.5, 0.8] {
            let o = shimmer_opacity(phase, false);
            assert!((0.2..=1.0).contains(&o), "{o}");
        }
    }

    #[test]
    fn a_time_is_read_on_the_zone_clock() {
        // 2026-11-19 14:05:00 UTC.
        let at = 1_795_097_100_000;
        assert_eq!(clock_label(at, &chrono::Utc), "14:05");
        let plus_two = chrono::FixedOffset::east_opt(2 * 3600).unwrap();
        assert_eq!(clock_label(at, &plus_two), "16:05");
    }

    #[test]
    fn a_routing_line_gives_the_headline_and_the_reason() {
        use reno_proto::{RoutingDecision, RuleMatched};
        let view = crate::vm::routing::view(&RoutingDecision {
            chosen: Some("codex".parse().unwrap()),
            ranked: vec!["codex".parse().unwrap()],
            rejected: vec![],
            rule_matched: RuleMatched::Default,
            fallbacks: vec![],
            at_ms: 1,
            warnings: Vec::new(),
        });
        let line = routing_line(&view);
        assert!(line.starts_with("Routed to Codex \u{b7} "), "{line}");
        assert!(line.len() > "Routed to Codex \u{b7} ".len(), "{line}");
        let _ = (
            ExecEvent::Text {
                text: String::new(),
            },
            json!(null),
            ToolCall::new("x", json!(null), ActionClass::Other),
        );
    }
}
