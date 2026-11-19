//! All runs: one lane per live run (opened from "N live" and ⌘K). A lane carries a status
//! stripe, the executor, the title, a live one-line tail, an "asks" badge, the state in words
//! and how long the run has been going.

use reno_client::{State, Transcript};
use reno_proto::{ExecEvent, RunId, RunStatus, RunSummary};

use super::projects::{self, Badge};
use super::shell;
use crate::summary;

/// The longest a tail runs before it is cut.
const TAIL_CHARS: usize = 140;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Lane {
    pub(crate) run: RunId,
    pub(crate) title: String,
    pub(crate) executor: String,
    pub(crate) status: RunStatus,
    /// "running", "needs input", "held for you": never wraps.
    pub(crate) status_label: &'static str,
    /// What the run did last, on one line; `None` until the app has seen an event.
    pub(crate) tail: Option<String>,
    /// What it waits for the person to do.
    pub(crate) asks: Option<Badge>,
    /// How long ago it started: "4m". Never wraps.
    pub(crate) time: String,
}

/// The last thing a run did, on one line: its last tool call, or its last line of text, or the
/// question or approval it waits on.
pub(crate) fn tail_of(transcript: &Transcript) -> Option<String> {
    let events = &transcript.events;
    let last = events.iter().rposition(|(_, event)| {
        matches!(
            event,
            ExecEvent::Tool { .. }
                | ExecEvent::Text { .. }
                | ExecEvent::NeedsInput { .. }
                | ExecEvent::NeedsApproval { .. }
        )
    })?;
    let line = match &events[last].1 {
        ExecEvent::Tool { call, .. } => crate::transcript::tool_label(&call.name, &call.input),
        ExecEvent::NeedsInput { questions, .. } => questions.first()?.prompt.trim().to_owned(),
        ExecEvent::NeedsApproval { subject, .. } => summary::subject_line(subject),
        _ => {
            // Text arrives in chunks that can end mid-word: join the run of chunks first.
            let start = events[..last]
                .iter()
                .rposition(|(_, event)| !matches!(event, ExecEvent::Text { .. }))
                .map_or(0, |at| at + 1);
            let text: String = events[start..=last]
                .iter()
                .filter_map(|(_, event)| match event {
                    ExecEvent::Text { text } => Some(text.as_str()),
                    _ => None,
                })
                .collect();
            text.lines()
                .rev()
                .map(str::trim)
                .find(|l| !l.is_empty())?
                .to_owned()
        }
    };
    Some(one_line(&line))
}

fn one_line(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= TAIL_CHARS {
        return flat;
    }
    let cut: String = flat.chars().take(TAIL_CHARS - 1).collect();
    format!("{}\u{2026}", cut.trim_end())
}

/// The lanes: what waits for you first, then the newest. (The daemon records no lead and
/// children among live runs, so the lanes are flat.)
pub(crate) fn lanes(state: &State, now_ms: u64) -> Vec<Lane> {
    let mut live = shell::live_runs(state);
    live.sort_by(|a, b| {
        let waiting = |r: &RunSummary| projects::badge(r).is_some();
        waiting(b)
            .cmp(&waiting(a))
            .then_with(|| b.created_at_ms.cmp(&a.created_at_ms))
            .then_with(|| a.run_id.cmp(&b.run_id))
    });
    live.into_iter()
        .map(|run| Lane {
            run: run.run_id.clone(),
            title: summary::parse_title(&run.title).what.to_owned(),
            executor: run.executor.as_str().to_owned(),
            status: run.status,
            status_label: summary::status_label(run.status),
            tail: state.transcript(&run.run_id).and_then(tail_of),
            asks: projects::badge(run),
            time: summary::age_label(now_ms.saturating_sub(run.created_at_ms)),
        })
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use reno_client::Update;
    use reno_proto::{ActionClass, ApprovalSubject, Question, Seq, TaskId, ToolCall};

    use super::*;

    fn run(id: &str, status: RunStatus, title: &str, created: u64) -> RunSummary {
        RunSummary {
            model_options: Default::default(),
            effort: None,
            run_id: RunId::from_suffix(id).unwrap(),
            task_id: TaskId::from_suffix(id).unwrap(),
            executor: "codex".parse().unwrap(),
            status,
            attempt: 1,
            title: title.to_owned(),
            prompt: None,
            model: None,
            stop_reason: None,
            stop: None,
            not_resumed: None,
            session: None,
            bot: None,
            cwd: None,
            report: None,
            created_at_ms: created,
            updated_at_ms: created,
        }
    }

    fn rid(id: &str) -> RunId {
        RunId::from_suffix(id).unwrap()
    }

    fn event(state: &mut State, id: &str, seq: u64, event: ExecEvent) {
        state.apply(Update::RunEvent {
            run: rid(id),
            seq: Seq(seq),
            event,
        });
    }

    fn state(runs: Vec<RunSummary>) -> State {
        let mut state = State::new();
        state.apply(Update::Runs(runs));
        state
    }

    #[test]
    fn a_lane_per_live_run_waiting_ones_first_then_the_newest() {
        let s = state(vec![
            run("old", RunStatus::Running, "older", 100),
            run("new", RunStatus::Running, "newer", 900),
            run("ask", RunStatus::WaitingInput, "asking", 50),
            run("held", RunStatus::WaitingApproval, "held", 60),
            run("done", RunStatus::Succeeded, "finished", 999),
        ]);
        let order: Vec<_> = lanes(&s, 1_000).iter().map(|l| l.run.to_string()).collect();
        assert_eq!(order, ["run_held", "run_ask", "run_new", "run_old"]);
    }

    #[test]
    fn a_lane_says_its_state_in_words_its_asks_and_how_long_it_has_gone() {
        let s = state(vec![
            run("ask", RunStatus::WaitingInput, "Pick a name", 0),
            run("held", RunStatus::WaitingApproval, "Push", 0),
            run("go", RunStatus::Running, "Build", 0),
        ]);
        let lanes = lanes(&s, 4 * 60_000);
        let by = |id: &str| lanes.iter().find(|l| l.run.to_string() == id).unwrap();
        assert_eq!(by("run_ask").status_label, "needs input");
        assert_eq!(by("run_ask").asks, Some(Badge::NeedsInput));
        assert_eq!(by("run_held").status_label, "held for you");
        assert_eq!(by("run_held").asks, Some(Badge::NeedsApproval));
        assert_eq!(by("run_go").status_label, "running");
        assert_eq!(by("run_go").asks, None);
        assert_eq!(by("run_go").time, "4m");
        assert_eq!(by("run_go").executor, "codex");
        assert_eq!(by("run_go").title, "Build");
    }

    #[test]
    fn the_tail_is_the_last_thing_the_run_did_on_one_line() {
        let mut s = state(vec![run("go", RunStatus::Running, "Build", 0)]);
        assert_eq!(lanes(&s, 0)[0].tail, None, "nothing seen yet");
        event(
            &mut s,
            "go",
            1,
            ExecEvent::Text {
                text: "Reading the config.\n\nThen the tests.\n".into(),
            },
        );
        assert_eq!(lanes(&s, 0)[0].tail.as_deref(), Some("Then the tests."));
        event(
            &mut s,
            "go",
            2,
            ExecEvent::Tool {
                id: "t1".into(),
                call: ToolCall::new(
                    "Bash",
                    serde_json::json!({"command": "cargo test --workspace"}),
                    ActionClass::Command,
                ),
            },
        );
        assert_eq!(
            lanes(&s, 0)[0].tail.as_deref(),
            Some("cargo test --workspace")
        );
        // Usage is not something to show: the tail stays on the tool.
        event(
            &mut s,
            "go",
            3,
            ExecEvent::Usage {
                input_tokens: 1,
                output_tokens: 1,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
        );
        assert_eq!(
            lanes(&s, 0)[0].tail.as_deref(),
            Some("cargo test --workspace")
        );
    }

    #[test]
    fn a_waiting_run_shows_what_it_waits_on_and_a_long_tail_is_cut() {
        let mut s = state(vec![
            run("ask", RunStatus::WaitingInput, "q", 0),
            run("held", RunStatus::WaitingApproval, "h", 0),
        ]);
        event(
            &mut s,
            "ask",
            1,
            ExecEvent::NeedsInput {
                request_id: "r".into(),
                questions: vec![Question {
                    id: "q".into(),
                    prompt: "Which\nbranch?".into(),
                    options: vec![],
                    multi_select: false,
                }],
            },
        );
        event(
            &mut s,
            "held",
            1,
            ExecEvent::NeedsApproval {
                request_id: "r".into(),
                subject: ApprovalSubject {
                    action: ActionClass::Command,
                    tool: "Bash".into(),
                    command: Some("git push origin main".into()),
                    paths: vec![],
                    raw: serde_json::Value::Null,
                },
            },
        );
        let lanes = lanes(&s, 0);
        let by = |id: &str| lanes.iter().find(|l| l.run.to_string() == id).unwrap();
        assert_eq!(by("run_ask").tail.as_deref(), Some("Which branch?"));
        assert_eq!(by("run_held").tail.as_deref(), Some("git push origin main"));
        let long = "word ".repeat(100);
        let cut = one_line(&long);
        assert_eq!(cut.chars().count(), TAIL_CHARS);
        assert!(cut.ends_with('\u{2026}'));
    }

    #[test]
    fn text_streams_in_chunks_so_the_tail_joins_them_before_taking_the_last_line() {
        let mut s = state(vec![run("go", RunStatus::Running, "Build", 0)]);
        for (seq, chunk) in ["Running the te", "sts now.\nAll thr", "ee suites pass"]
            .into_iter()
            .enumerate()
        {
            event(
                &mut s,
                "go",
                seq as u64 + 1,
                ExecEvent::Text { text: chunk.into() },
            );
        }
        assert_eq!(
            lanes(&s, 0)[0].tail.as_deref(),
            Some("All three suites pass")
        );
        // A tool call ends the run of chunks: what came before it is not joined to what follows.
        event(
            &mut s,
            "go",
            4,
            ExecEvent::Tool {
                id: "t".into(),
                call: ToolCall::new(
                    "Read",
                    serde_json::json!({"path": "a.rs"}),
                    ActionClass::Read,
                ),
            },
        );
        event(
            &mut s,
            "go",
            5,
            ExecEvent::Text {
                text: "Done".into(),
            },
        );
        assert_eq!(lanes(&s, 0)[0].tail.as_deref(), Some("Done"));
    }

    #[test]
    fn no_live_run_means_no_lane() {
        let s = state(vec![run("done", RunStatus::Succeeded, "x", 1)]);
        assert!(lanes(&s, 10).is_empty());
    }
}
