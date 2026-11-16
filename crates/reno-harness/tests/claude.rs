//! `ClaudeExecutor` integration tests against the fake CLI in `tests/fixtures/fake-claude.sh`
//! (no real `claude` binary involved). Live tests against the real CLI are at the bottom,
//! `#[ignore]`d.
//!
//! Scenarios on the native contract, read as `ExecEvent`s, with approvals as `NeedsApproval` answered through `ExecHandle::decide`.
//! What the contract has no place for (title runs, command discovery, turn stop, background
//! listings, live reconfiguration, subagent chips) is not covered here.

#![cfg(unix)]
// Tests unwrap freely, and probe a leaked pid with `kill(pid, 0)`.
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use reno_harness::exec::{Decision, ExecControls, ExecError, ExecHandle, Executor, InputAnswer};
use reno_harness::{ClaudeExecutor, ProcessRecord};
use reno_proto::{
    AccessMode, ActionClass, ApprovalSubject, ExecEvent, ExecSpec, ExecutorId, Ineligible,
    InstallStatus, ModelSource, NotInstalledReason, Question, RunId, SandboxLevel, SessionRef,
    SignedIn, Steering, StopReason, ToolCall, VersionGate,
};
use tokio::sync::mpsc;

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-claude.sh");
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    path
}

fn executor() -> ClaudeExecutor {
    ClaudeExecutor::new().with_executable(fixture_path())
}

fn spec(prompt: &str) -> ExecSpec {
    ExecSpec {
        run_id: RunId::from_suffix("01HZX9").unwrap(),
        inherit_user_config: false,
        prompt: prompt.into(),
        cwd: std::env::temp_dir(),
        model: None,
        effort: None,
        access: AccessMode::FullAccess,
        sandbox: SandboxLevel::Unrestricted,
        resume: None,
        mcp: Vec::new(),
        env: Default::default(),
        output_schema: None,
        instructions: None,
    }
}

fn spec_with(prompt: &str, access: AccessMode) -> ExecSpec {
    ExecSpec {
        access,
        ..spec(prompt)
    }
}

/// A started run: its events, its handle and the process-record queue.
struct Live {
    stream: BoxStream<'static, ExecEvent>,
    handle: ExecHandle,
    records: mpsc::Receiver<ProcessRecord>,
}

async fn start(executor: &ClaudeExecutor, spec: ExecSpec) -> Live {
    let (controls, handle) = ExecControls::new(8);
    start_with(executor, spec, controls, handle).await
}

async fn start_with(
    executor: &ClaudeExecutor,
    spec: ExecSpec,
    controls: ExecControls,
    handle: ExecHandle,
) -> Live {
    let records = handle
        .take_process_records()
        .expect("records are taken once");
    let stream = executor.start(spec, controls).await.expect("run starts");
    Live {
        stream,
        handle,
        records,
    }
}

/// What a run produced, and what it was asked on the way.
#[derive(Default)]
struct Outcome {
    events: Vec<ExecEvent>,
    approvals: Vec<ApprovalSubject>,
    questions: Vec<Vec<Question>>,
}

/// Drive `live` to its end. Approvals go to `decide`; the first option of every question (or
/// `"A"` for free text) answers `NeedsInput`.
async fn drive(live: &mut Live, mut decide: impl FnMut(&ApprovalSubject) -> Decision) -> Outcome {
    drive_with_messages(live, |subject| (decide(subject), None)).await
}

/// [`drive`], with the message each decision comes with (a policy rule's deny message).
async fn drive_with_messages(
    live: &mut Live,
    mut decide: impl FnMut(&ApprovalSubject) -> (Decision, Option<String>),
) -> Outcome {
    let mut outcome = Outcome::default();
    let run = async {
        while let Some(event) = live.stream.next().await {
            match &event {
                ExecEvent::NeedsApproval {
                    request_id,
                    subject,
                } => {
                    outcome.approvals.push(subject.clone());
                    let (decision, message) = decide(subject);
                    live.handle
                        .decide_with_message(request_id.clone(), decision, message)
                        .await
                        .unwrap();
                }
                ExecEvent::NeedsInput {
                    request_id,
                    questions,
                } => {
                    outcome.questions.push(questions.clone());
                    let answers = questions
                        .iter()
                        .map(|q| InputAnswer {
                            question_id: q.id.clone(),
                            labels: vec![q.options.last().cloned().unwrap_or_else(|| "A".into())],
                        })
                        .collect();
                    live.handle
                        .answer_input(request_id.clone(), answers)
                        .await
                        .unwrap();
                }
                _ => {}
            }
            outcome.events.push(event);
        }
    };
    tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("run finished in time");
    outcome
}

async fn run_to_end(executor: &ClaudeExecutor, spec: ExecSpec) -> Vec<ExecEvent> {
    let mut live = start(executor, spec).await;
    drive(&mut live, |_| Decision::Allow).await.events
}

fn done(events: &[ExecEvent]) -> (StopReason, Option<String>, Option<String>) {
    match events.last() {
        Some(ExecEvent::Done {
            stop,
            result,
            diagnostic,
            ..
        }) => (*stop, result.clone(), diagnostic.clone()),
        other => panic!("the stream did not end with Done: {other:?} in {events:?}"),
    }
}

fn dones(events: &[ExecEvent]) -> usize {
    events
        .iter()
        .filter(|e| matches!(e, ExecEvent::Done { .. }))
        .count()
}

fn text(s: &str) -> ExecEvent {
    ExecEvent::Text { text: s.into() }
}

/// The result of an approvals scenario, which says how the CLI saw each answer.
fn result_of(events: &[ExecEvent]) -> String {
    match done(events) {
        (StopReason::Completed, Some(result), _) => result,
        other => panic!("run did not complete: {other:?} in {events:?}"),
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn happy_path_maps_frames_to_exec_events_and_drops_subagent_traffic() {
    let events = run_to_end(&executor(), spec("scenario:happy")).await;

    // One Started despite the re-emitted init frame.
    let started: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, ExecEvent::Started { .. }))
        .collect();
    assert_eq!(started.len(), 1, "init must be deduped: {events:?}");
    assert_eq!(
        started[0],
        &ExecEvent::Started {
            session: SessionRef("sess-1".into()),
            model: "claude-fable-5".into(),
        }
    );
    assert!(events.contains(&ExecEvent::Reasoning {
        text: "pondering".into()
    }));
    assert!(events.contains(&text("Hello")));

    // Subagent frames (parent_tool_use_id set) are not the run's own feed: nothing of theirs
    // reaches the stream.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ExecEvent::Text { text } if text.contains("SUBAGENT"))),
        "subagent text leaked: {events:?}"
    );
    assert!(
        !events.iter().any(|e| matches!(
            e,
            ExecEvent::Tool { id, .. } | ExecEvent::ToolResult { id, .. } if id == "sub-tool"
        )),
        "subagent tool frames leaked: {events:?}"
    );

    // Tool calls carry the harness's own name and input, verbatim.
    assert!(events.contains(&ExecEvent::Tool {
        id: "tool-1".into(),
        call: ToolCall::new(
            "Bash",
            serde_json::json!({ "command": "ls -la" }),
            ActionClass::Command,
        ),
    }));
    assert!(events.contains(&ExecEvent::Tool {
        id: "tool-2".into(),
        call: ToolCall::new(
            "mcp__linear__search",
            serde_json::json!({ "q": "bug" }),
            ActionClass::McpTool,
        ),
    }));
    // A Bash result reports its exit code; another tool's reports nothing.
    assert!(events.contains(&ExecEvent::ToolResult {
        id: "tool-1".into(),
        ok: true,
        summary: Some("exit code 0".into()),
    }));
    assert!(events.contains(&ExecEvent::ToolResult {
        id: "tool-2".into(),
        ok: false,
        summary: None,
    }));
    assert!(events.contains(&ExecEvent::Usage {
        input_tokens: 10,
        output_tokens: 20,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    }));
    assert_eq!(
        events.last(),
        Some(&ExecEvent::Done {
            stop: StopReason::Completed,
            result: Some("done!".into()),
            diagnostic: None,
            reason: None,
        })
    );
    assert_eq!(dones(&events), 1);
}

/// The background-subagent shape (live-verified 2.1.228): result #1 arrives eagerly while the
/// subagent still runs, and the CLI later wakes with a second turn. A run is one `Done`: the
/// first ends it, the wake turn's frames never reach the stream, and the child is torn down.
#[tokio::test]
async fn the_first_result_ends_the_run_and_a_wake_turn_never_surfaces() {
    let events = run_to_end(&executor(), spec("scenario:wake")).await;
    assert_eq!(dones(&events), 1, "{events:?}");
    assert!(events.contains(&text("LAUNCHED")));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ExecEvent::Text { text } if text.contains("subagent finished") || text.contains("sub working"))),
        "{events:?}"
    );
    assert_eq!(
        done(&events),
        (StopReason::Completed, Some("LAUNCHED".into()), None)
    );
}

#[tokio::test]
async fn error_codes_map_to_a_readable_diagnostic() {
    let events = run_to_end(&executor(), spec("scenario:error")).await;
    let (stop, result, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert_eq!(result, None);
    // The assistant-level error and the blocked usage window were shown first: they are the
    // cause, so they are the diagnostic; the result's own generic wording is not repeated.
    let diagnostic = diagnostic.expect("a diagnostic");
    assert!(
        diagnostic.contains("Claude usage limit reached — try again after the limit resets."),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains(
            "Claude 5-hour limit reached — the turn was blocked. Try again after it resets."
        ),
        "{diagnostic}"
    );
}

/// A result line that will not parse still ends its turn; a lone surrogate half in a delta is
/// repaired rather than dropped.
#[tokio::test]
async fn an_unreadable_result_still_ends_the_run() {
    let events = run_to_end(&executor(), spec("scenario:unreadable-result")).await;
    assert!(events.contains(&text("half \u{FFFD}")), "{events:?}");
    assert_eq!(done(&events).0, StopReason::Completed);
}

/// Captured frames, replayed through the whole driver path (wire parse, normalize, run loop).
async fn replay_fixture(name: &str) -> Vec<ExecEvent> {
    let frames = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("claude")
        .join(name);
    let dir = tempfile::tempdir().expect("tmp dir");
    let cli = dir.path().join("replay.sh");
    std::fs::write(
        &cli,
        format!(
            "#!/bin/sh\nread -r _first || exit 1\ncat '{}'\n",
            frames.display()
        ),
    )
    .expect("replayer written");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&cli, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut spec = spec("replay");
    // Its own config root, so no transcript of this machine is looked at.
    spec.env
        .insert("CLAUDE_CONFIG_DIR".into(), dir.path().display().to_string());
    run_to_end(&ClaudeExecutor::new().with_executable(&cli), spec).await
}

#[tokio::test]
async fn captured_live_background_subagent_frames_replay_without_the_subagent() {
    // Frames captured verbatim from claude 2.1.228: a turn that spawns a background Agent
    // subagent. Eager result while it runs, tagged subagent traffic, then the wake turn.
    let events = replay_fixture("live-2.1.228-background-subagent.jsonl").await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ExecEvent::Started { .. }))
            .count(),
        1,
        "{events:?}"
    );
    assert_eq!(
        dones(&events),
        1,
        "the eager result is the run's end: {events:?}"
    );
    // The spawn is a plain tool call on the parent feed, under the harness's own tool name.
    assert!(
        events.iter().any(
            |e| matches!(e, ExecEvent::Tool { call, .. } if call.name == "Agent" || call.name == "Task")
        ),
        "{events:?}"
    );
    // The subagent's own Bash call is its interior, and stays out.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ExecEvent::Tool { call, .. } if call.name == "Bash")),
        "{events:?}"
    );
    assert!(matches!(
        events.last(),
        Some(ExecEvent::Done {
            stop: StopReason::Completed,
            ..
        })
    ));
}

#[tokio::test]
async fn captured_live_forwarded_foreground_subagent_text_stays_out_of_the_parent_feed() {
    // Frames from claude 2.1.310 run with CLAUDE_CODE_FORWARD_SUBAGENT_TEXT=1: a foreground
    // Agent told to write a sentence, run `echo hi`, then write another.
    let events = replay_fixture("live-2.1.310-foreground-subagent-forwarded.jsonl").await;
    assert!(
        !events.iter().any(|e| matches!(
            e,
            ExecEvent::Text { text } if text.contains("The sky is blue.") || text.contains("The grass is green.")
        )),
        "{events:?}"
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ExecEvent::Tool { call, .. } if call.name == "Bash")),
        "{events:?}"
    );
    assert_eq!(dones(&events), 1, "{events:?}");
}

/// One run, one `Started`, one `Done`, and the parent feed's own tool calls only.
fn assert_one_run_of(events: &[ExecEvent]) {
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ExecEvent::Started { .. }))
            .count(),
        1,
        "{events:?}"
    );
    assert_eq!(dones(events), 1, "{events:?}");
    assert!(matches!(
        events.last(),
        Some(ExecEvent::Done {
            stop: StopReason::Completed,
            ..
        })
    ));
}

fn tool_names(events: &[ExecEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| match e {
            ExecEvent::Tool { call, .. } => Some(call.name.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn captured_live_forked_skill_frames_show_the_skill_call_and_nothing_of_its_agents() {
    // Frames from claude 2.1.310: a `context: fork` skill that spawns two background agents,
    // which run Bash. The parent feed has the Skill call only.
    let events = replay_fixture("live-2.1.310-forked-skill.jsonl").await;
    assert_one_run_of(&events);
    assert_eq!(tool_names(&events), ["Skill"], "{events:?}");
}

#[tokio::test]
async fn captured_live_typed_fork_frames_leave_the_forks_interior_out() {
    // Frames from claude 2.1.310: `/fanout` typed as the prompt runs in a fork that spawns two
    // agents; `/f-slow-steps` runs Bash three times. Everything the forks do is tagged with a
    // `forked-command-*` id, so the parent feed carries no tool call at all.
    for fixture in [
        "live-2.1.310-typed-forked-command.jsonl",
        "live-2.1.310-typed-fork-slow-steps.jsonl",
    ] {
        let events = replay_fixture(fixture).await;
        assert_one_run_of(&events);
        assert!(tool_names(&events).is_empty(), "{fixture}: {events:?}");
    }
}

// ---------------------------------------------------------------------------
// Structured approvals
// ---------------------------------------------------------------------------

fn subject(
    action: ActionClass,
    tool: &str,
    command: Option<&str>,
    paths: &[&str],
    raw: serde_json::Value,
) -> ApprovalSubject {
    ApprovalSubject {
        action,
        tool: tool.into(),
        command: command.map(str::to_owned),
        paths: paths.iter().map(PathBuf::from).collect(),
        raw,
    }
}

/// Every `can_use_tool` request reaches Reno as a structured subject: the action class from
/// the tool, the command or paths from its input, and the harness's own request as `raw`. No
/// prose is read or written anywhere.
#[tokio::test]
async fn can_use_tool_requests_become_structured_approval_subjects() {
    let mut live = start(
        &executor(),
        spec_with("scenario:approval-subjects", AccessMode::Supervised),
    )
    .await;
    let outcome = drive(&mut live, |_| Decision::Allow).await;

    assert_eq!(
        outcome.approvals,
        vec![
            subject(
                ActionClass::Command,
                "Bash",
                Some("cargo test --workspace"),
                &[],
                serde_json::json!({
                    "subtype": "can_use_tool",
                    "tool_name": "Bash",
                    "display_name": "Bash",
                    "description": "Run the tests",
                    "input": {"command": "cargo test --workspace", "description": "Run the tests"},
                }),
            ),
            subject(
                ActionClass::Edit,
                "Edit",
                None,
                &["/work/src/lib.rs"],
                serde_json::json!({
                    "subtype": "can_use_tool",
                    "tool_name": "Edit",
                    "input": {"file_path": "/work/src/lib.rs", "old_string": "a", "new_string": "b"},
                }),
            ),
            subject(
                ActionClass::McpTool,
                "mcp__x__y",
                None,
                &[],
                serde_json::json!({
                    "subtype": "can_use_tool",
                    "tool_name": "mcp__x__y",
                    "input": {"q": "bug"},
                }),
            ),
            subject(
                ActionClass::Network,
                "WebFetch",
                None,
                &[],
                serde_json::json!({
                    "subtype": "can_use_tool",
                    "tool_name": "WebFetch",
                    "input": {"url": "https://example.com", "prompt": "summarise"},
                }),
            ),
            subject(
                ActionClass::Read,
                "Read",
                None,
                &["/etc/hosts"],
                serde_json::json!({
                    "subtype": "can_use_tool",
                    "tool_name": "Read",
                    "input": {"file_path": "/etc/hosts"},
                }),
            ),
            subject(
                ActionClass::Other,
                "TodoWrite",
                None,
                &[],
                serde_json::json!({
                    "subtype": "can_use_tool",
                    "tool_name": "TodoWrite",
                    "input": {"todos": []},
                }),
            ),
        ]
    );
    // An approval is not an input request.
    assert!(outcome.questions.is_empty());
    assert_eq!(
        result_of(&outcome.events),
        "bash=allow edit=allow mcp=allow net=allow read=allow other=allow"
    );
}

/// A deny reaches the CLI as a deny control response carrying a message the model reads.
#[tokio::test]
async fn a_deny_decision_round_trips_a_deny_control_response_with_a_message() {
    let mut live = start(
        &executor(),
        spec_with("scenario:approval-subjects", AccessMode::Supervised),
    )
    .await;
    let outcome = drive(&mut live, |subject| {
        if subject.action == ActionClass::Edit {
            Decision::Deny
        } else {
            Decision::Allow
        }
    })
    .await;
    assert_eq!(
        result_of(&outcome.events),
        "bash=allow edit=deny:The user did not allow this. mcp=allow net=allow read=allow other=allow"
    );
}

/// A policy rule's deny message is what the model reads in the deny response, in place of the
/// generic refusal; an allow ignores any message, and an empty one is no message.
#[tokio::test]
async fn a_policy_deny_message_reaches_the_model_in_the_deny_response() {
    let mut live = start(
        &executor(),
        spec_with("scenario:approval-subjects", AccessMode::Supervised),
    )
    .await;
    let outcome = drive_with_messages(&mut live, |subject| match subject.action {
        ActionClass::Edit => (
            Decision::Deny,
            Some("denied by policy: no edits in vendor/".into()),
        ),
        ActionClass::Command => (Decision::Deny, Some("  ".into())),
        _ => (Decision::Allow, Some("ignored on an allow".into())),
    })
    .await;
    assert_eq!(
        result_of(&outcome.events),
        "bash=deny:The user did not allow this. \
         edit=deny:denied by policy: no edits in vendor/ \
         mcp=allow net=allow read=allow other=allow"
    );
}

/// `AskUserQuestion` is a genuine question: `NeedsInput`, answered through `answer_input`, and
/// never an approval.
#[tokio::test]
async fn ask_user_question_becomes_needs_input_and_is_answered_through_answer_input() {
    let mut live = start(
        &executor(),
        spec_with("scenario:askuser", AccessMode::Supervised),
    )
    .await;
    // The plain Bash request that precedes it is an approval, answered separately.
    let outcome = drive(&mut live, |_| Decision::Allow).await;

    assert_eq!(outcome.questions.len(), 1, "{:?}", outcome.events);
    let questions = &outcome.questions[0];
    assert_eq!(questions.len(), 1);
    assert_eq!(questions[0].prompt, "Choice: Pick one");
    assert_eq!(questions[0].options, vec!["A".to_string(), "B".to_string()]);
    assert!(!questions[0].multi_select);
    assert_eq!(outcome.approvals.len(), 1);
    assert_eq!(outcome.approvals[0].tool, "Bash");
    assert_eq!(outcome.approvals[0].action, ActionClass::Command);

    // "answered" proves both control round-trips: the Bash allow, and the answers reaching the
    // CLI as updatedInput.answers keyed by question text ("B" is the driver's pick: the last
    // option).
    assert_eq!(
        done(&outcome.events),
        (StopReason::Completed, Some("answered".into()), None)
    );
}

#[tokio::test]
async fn questions_closed_unanswered_deny_the_tool() {
    // No answers is the person closing the questions: the CLI must read a deny that says so,
    // not an allow with empty picks.
    let (controls, handle) = ExecControls::new(8);
    let mut live = start_with(
        &executor(),
        spec("scenario:closed-question"),
        controls,
        handle,
    )
    .await;
    let mut events = Vec::new();
    while let Some(event) = live.stream.next().await {
        if let ExecEvent::NeedsInput { request_id, .. } = &event {
            live.handle
                .answer_input(request_id.clone(), Vec::new())
                .await
                .unwrap();
        }
        events.push(event);
    }
    assert_eq!(done(&events).1.as_deref(), Some("declined"));
}

/// What each access mode asks, and the launch flags it implies (see the table on
/// `ClaudeExecutor`). The fake CLI reports its flags and how each request was answered.
#[tokio::test]
async fn access_modes_decide_what_reaches_reno() {
    // Full access: nothing is asked; the CLI launches in `default` with no skip flag.
    let mut live = start(
        &executor(),
        spec_with("scenario:approvals", AccessMode::FullAccess),
    )
    .await;
    let outcome = drive(&mut live, |_| Decision::Deny).await;
    assert_eq!(
        result_of(&outcome.events),
        "default none bash=allow edit=allow plan=allow:default"
    );
    assert!(outcome.approvals.is_empty());

    // Auto-accept edits: the CLI's own acceptEdits mode. Nothing is answered here: an edit
    // request that reaches the driver is one acceptEdits did not cover, so it is asked like
    // the command and the plan, and a deny refuses each.
    let mut live = start(
        &executor(),
        spec_with("scenario:approvals", AccessMode::AutoAcceptEdits),
    )
    .await;
    let outcome = drive(&mut live, |_| Decision::Deny).await;
    assert_eq!(
        result_of(&outcome.events),
        "acceptEdits none bash=deny edit=deny plan=deny"
    );
    let asked: Vec<_> = outcome
        .approvals
        .iter()
        .map(|s| (s.action, s.tool.as_str()))
        .collect();
    assert_eq!(
        asked,
        [
            (ActionClass::Command, "Bash"),
            (ActionClass::Edit, "Edit"),
            (ActionClass::Other, "ExitPlanMode")
        ]
    );

    // Supervised asks about each; an allow lets it through, and an approved plan continues in
    // the access mode's own permission mode.
    let mut live = start(
        &executor(),
        spec_with("scenario:approvals", AccessMode::Supervised),
    )
    .await;
    let outcome = drive(&mut live, |_| Decision::Allow).await;
    assert_eq!(
        result_of(&outcome.events),
        "default none bash=allow edit=allow plan=allow:default"
    );
    let asked: Vec<_> = outcome.approvals.iter().map(|s| s.action).collect();
    assert_eq!(
        asked,
        [ActionClass::Command, ActionClass::Edit, ActionClass::Other]
    );
}

/// A request still open when the run is cancelled is denied, never left to the CLI as an
/// allow; and an answer of the wrong kind never approves.
#[tokio::test]
async fn open_requests_are_denied_on_cancel_and_a_wrong_kind_answer_never_approves() {
    let dir = tempfile::tempdir().unwrap();
    let record = dir.path().join("answer.json");
    let mut spec = spec_with("scenario:hold-approval", AccessMode::Supervised);
    spec.env.insert(
        "RENO_TEST_RECORD".into(),
        record.to_string_lossy().into_owned(),
    );
    let executor = executor().with_graces(Duration::from_millis(500), Duration::from_secs(2));
    let mut live = start(&executor, spec).await;

    let request_id = loop {
        match live.stream.next().await.expect("stream open") {
            ExecEvent::NeedsApproval { request_id, .. } => break request_id,
            _ => continue,
        }
    };
    // An input answer is not a decision: the request stays open.
    live.handle
        .answer_input(
            request_id.clone(),
            vec![InputAnswer {
                question_id: "x".into(),
                labels: vec!["Yes".into()],
            }],
        )
        .await
        .unwrap();
    live.handle.cancel();
    let rest: Vec<_> = tokio::time::timeout(Duration::from_secs(20), live.stream.collect())
        .await
        .expect("cancel completes");
    assert_eq!(done(&rest).0, StopReason::Interrupted, "{rest:?}");

    let answer = std::fs::read_to_string(&record).expect("the CLI saw an answer");
    assert!(answer.contains("\"behavior\":\"deny\""), "{answer}");
    assert!(!answer.contains("\"behavior\":\"allow\""), "{answer}");
}

/// A request that reaches the driver after the run was cancelled is denied at once: nothing new
/// is allowed (not even under full access) and nobody is left to ask.
#[tokio::test]
async fn a_request_after_cancel_is_denied_and_never_asked() {
    for access in [AccessMode::Supervised, AccessMode::FullAccess] {
        let dir = tempfile::tempdir().unwrap();
        let record = dir.path().join("answer.json");
        let mut spec = spec_with("scenario:request-after-cancel", access);
        spec.env.insert(
            "RENO_TEST_RECORD".into(),
            record.to_string_lossy().into_owned(),
        );
        let executor = executor().with_graces(Duration::from_millis(500), Duration::from_secs(2));
        let mut live = start(&executor, spec).await;
        let mut events = Vec::new();
        let finished = tokio::time::timeout(Duration::from_secs(20), async {
            while let Some(event) = live.stream.next().await {
                if matches!(event, ExecEvent::Started { .. }) {
                    live.handle.cancel();
                }
                events.push(event);
            }
        })
        .await;
        assert!(finished.is_ok(), "{access:?}: {events:?}");
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, ExecEvent::NeedsApproval { .. })),
            "{access:?}: {events:?}"
        );
        assert_eq!(done(&events).0, StopReason::Interrupted, "{access:?}");
        let answer = std::fs::read_to_string(&record).expect("the CLI saw an answer");
        assert!(
            answer.contains("\"behavior\":\"deny\""),
            "{access:?}: {answer}"
        );
    }
}

// ---------------------------------------------------------------------------
// Controls: steering, cancel, process records
// ---------------------------------------------------------------------------

#[tokio::test]
async fn steering_lines_are_written_to_stdin_mid_run() {
    let (controls, handle) = ExecControls::new(8);
    handle.steer("redirect please").await.unwrap();
    let mut live = start_with(&executor(), spec("scenario:steer"), controls, handle).await;
    let events = drive(&mut live, |_| Decision::Allow).await.events;

    // Sending input must not split unconsumed response text.
    let continuation = events
        .iter()
        .position(|e| matches!(e, ExecEvent::Text { text } if text == "-still-first"))
        .expect("continuation");
    let echo = events
        .iter()
        .position(|e| matches!(e, ExecEvent::Text { text } if text == "steered:redirect please"))
        .expect("the fake CLI echoes the steer line's content back");
    assert!(continuation < echo, "{events:?}");
    assert_eq!(done(&events).0, StopReason::Completed);
}

#[tokio::test]
async fn cancel_escalates_to_sigterm_and_ends_with_an_interrupted_done() {
    let executor = executor().with_graces(Duration::from_millis(100), Duration::from_millis(500));
    let mut live = start(&executor, spec("scenario:interrupt")).await;
    let mut events = Vec::new();
    let finished = tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = live.stream.next().await {
            if matches!(event, ExecEvent::Started { .. }) {
                live.handle.cancel(); // as soon as the session is up
            }
            events.push(event);
        }
    })
    .await;
    assert!(finished.is_ok(), "interrupt completed in time: {events:?}");
    assert_eq!(
        events.last(),
        Some(&ExecEvent::Done {
            stop: StopReason::Interrupted,
            result: None,
            diagnostic: None,
            reason: None,
        })
    );
    assert_eq!(dones(&events), 1);
}

/// ARCH-11: every task a run spawned is the executor's, and ends with the run's stream.
#[tokio::test]
async fn shutdown_waits_for_the_tasks_of_runs_whose_streams_were_dropped() {
    let executor = executor().with_graces(Duration::from_millis(100), Duration::from_millis(500));
    let mut live = start(&executor, spec("scenario:interrupt")).await;
    while let Some(event) = live.stream.next().await {
        if matches!(event, ExecEvent::Started { .. }) {
            break;
        }
    }
    drop(live.stream);
    tokio::time::timeout(Duration::from_secs(20), executor.shutdown())
        .await
        .expect("the run's tasks end once its stream is dropped");
}

#[tokio::test]
async fn the_spawned_childs_process_record_is_forwarded() {
    let mut live = start(&executor(), spec("scenario:happy")).await;
    let record = tokio::time::timeout(Duration::from_secs(10), live.records.recv())
        .await
        .expect("a record in time")
        .expect("the child was reported");
    assert!(record.pid > 0);
    // The CLI is spawned as its own group leader.
    assert_eq!(record.pgid, record.pid);
    assert!(record.start_time > 0);
    // And it is the record of a real process: the same reading, taken while it lives or not.
    let _ = drive(&mut live, |_| Decision::Allow).await;
}

#[tokio::test]
async fn a_relaunched_child_is_reported_too() {
    let config = tempfile::tempdir().unwrap();
    let mut live = start(
        &executor(),
        resume_spec("stale-1", &config.path().join("missing")),
    )
    .await;
    let mut pids = Vec::new();
    let _ = drive(&mut live, |_| Decision::Allow).await;
    while let Ok(record) = live.records.try_recv() {
        pids.push(record.pid);
    }
    assert_eq!(
        pids.len(),
        2,
        "the first child and its fresh replacement: {pids:?}"
    );
    assert_ne!(pids[0], pids[1]);
}

#[tokio::test]
async fn processes_the_cli_started_end_with_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("grandchild.pid");
    let mut spec = spec("scenario:grandchild");
    spec.env.insert(
        "RENO_TEST_PIDFILE".into(),
        pidfile.to_string_lossy().into_owned(),
    );
    let events = run_to_end(&executor(), spec).await;
    assert_eq!(done(&events).0, StopReason::Completed);
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // The stream ends before teardown finishes; give it its grace.
    let deadline = std::time::Instant::now() + Duration::from_secs(8);
    // SAFETY: signal 0 only probes whether the pid exists.
    while unsafe { libc::kill(pid, 0) } == 0 {
        if std::time::Instant::now() > deadline {
            // SAFETY: the pid is the test's own leaked child.
            unsafe { libc::kill(pid, libc::SIGKILL) };
            panic!("the CLI's child outlived the run");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Steering boundaries
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_replay_confirms_superseded_steers_and_the_run_ends_once() {
    let (controls, handle) = ExecControls::new(8);
    for prompt in ["first steer", "second steer"] {
        handle.steer(prompt).await.unwrap();
    }
    let mut live = start_with(
        &executor(),
        spec("scenario:superseded-steers"),
        controls,
        handle,
    )
    .await;
    let events = drive(&mut live, |_| Decision::Allow).await.events;
    assert_eq!(dones(&events), 1, "{events:?}");
    assert!(events.contains(&text("answered-both")), "{events:?}");
    assert_eq!(done(&events).0, StopReason::Completed);
}

/// The premature-"done" bug: a `now` steer aborts the streaming turn, whose result was held for
/// the steer. The steered turn runs a long tool; no `Done` may land before it finishes.
#[tokio::test]
async fn a_steered_turn_running_a_long_tool_is_not_marked_done() {
    let (controls, handle) = ExecControls::new(8);
    handle.steer("stop and sleep").await.unwrap();
    let mut live = start_with(
        &executor(),
        spec("scenario:lifecycle-now-steer"),
        controls,
        handle,
    )
    .await;
    let events = drive(&mut live, |_| Decision::Allow).await.events;
    assert_eq!(
        dones(&events),
        1,
        "exactly the steered turn's own end: {events:?}"
    );
    let done_at = events.len() - 1;
    let tool_result_at = events
        .iter()
        .position(|e| matches!(e, ExecEvent::ToolResult { id, .. } if id == "tool-sleep"))
        .expect("tool result");
    assert!(
        done_at > tool_result_at,
        "Done before the tool finished: {events:?}"
    );
    assert_eq!(
        done(&events),
        (StopReason::Completed, Some("DONE".into()), None)
    );
}

/// A message queued behind a turn that just finished: that turn's result is a boundary; the
/// run's one `Done` is the queued message's own.
#[tokio::test]
async fn a_message_queued_behind_a_finishing_turn_owns_the_run_end() {
    let (controls, handle) = ExecControls::new(8);
    handle.steer("follow up").await.unwrap();
    let mut live = start_with(
        &executor(),
        spec("scenario:lifecycle-queued-behind-end"),
        controls,
        handle,
    )
    .await;
    let events = drive(&mut live, |_| Decision::Allow).await.events;
    assert_eq!(dones(&events), 1, "{events:?}");
    assert!(events.contains(&text("second answer")), "{events:?}");
    assert_eq!(done(&events).1.as_deref(), Some("second answer"));
}

#[tokio::test]
async fn a_cancelled_queued_message_releases_the_run_end_immediately() {
    let (controls, handle) = ExecControls::new(8);
    handle.steer("never started").await.unwrap();
    let started = std::time::Instant::now();
    let mut live = start_with(
        &executor(),
        spec("scenario:lifecycle-queued-cancelled"),
        controls,
        handle,
    )
    .await;
    let events = drive(&mut live, |_| Decision::Allow).await.events;
    assert_eq!(done(&events).0, StopReason::Completed);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "{:?}",
        started.elapsed()
    );
}

/// A steer the CLI never replays must not hold the run end forever.
#[tokio::test]
async fn an_unreplayed_steer_releases_the_run_end() {
    let (controls, handle) = ExecControls::new(8);
    handle.steer("absorbed steer").await.unwrap();
    let started = std::time::Instant::now();
    let mut live = start_with(
        &executor(),
        spec("scenario:absorbed-steer"),
        controls,
        handle,
    )
    .await;
    let events = drive(&mut live, |_| Decision::Allow).await.events;
    assert_eq!(done(&events).0, StopReason::Completed);
    assert!(started.elapsed() >= Duration::from_secs(4));
}

/// "Marked done the moment I sent it": resuming a session whose background tasks died with its
/// old process, the CLI settles them with empty results while the new prompt is still queued.
/// Those are not the prompt's end.
#[tokio::test]
async fn results_settling_killed_tasks_on_resume_do_not_end_the_run() {
    let events = run_to_end(&executor(), spec("Reply PONG scenario:resume-killed-tasks")).await;
    assert_eq!(dones(&events), 1, "{events:?}");
    assert_eq!(
        done(&events),
        (StopReason::Completed, Some("PONG".into()), None)
    );
    let pong = events
        .iter()
        .position(|e| *e == text("PONG"))
        .expect("reply");
    assert!(pong < events.len() - 1);
}

// ---------------------------------------------------------------------------
// The owner's personal Claude Code config
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_run_is_isolated_from_the_owners_personal_config_by_default() {
    let events = run_to_end(&executor(), spec("scenario:user-config")).await;
    assert_eq!(result_of(&events), "user-config:isolated");
}

#[tokio::test]
async fn a_bot_that_opts_in_gets_the_owners_personal_config() {
    let opted = ExecSpec {
        inherit_user_config: true,
        ..spec("scenario:user-config")
    };
    let events = run_to_end(&executor(), opted).await;
    assert_eq!(result_of(&events), "user-config:inherited");
}

#[tokio::test]
async fn a_resumed_run_stays_isolated_too() {
    let resumed = ExecSpec {
        resume: Some(SessionRef("sess-uc".into())),
        ..spec("scenario:user-config")
    };
    let events = run_to_end(&executor(), resumed).await;
    assert_eq!(result_of(&events), "user-config:isolated");
}

#[test]
fn the_descriptor_claims_the_isolation_it_enforces() {
    assert!(executor().descriptor().caps.isolates_user_config);
}

// ---------------------------------------------------------------------------
// Resume, crashes, launch failures
// ---------------------------------------------------------------------------

/// A resumed run against `config_dir` (the CLI's history root).
fn resume_spec(session: &str, config_dir: &Path) -> ExecSpec {
    let mut spec = spec("scenario:resume-check");
    spec.resume = Some(SessionRef(session.into()));
    spec.env.insert(
        "CLAUDE_CONFIG_DIR".into(),
        config_dir.to_string_lossy().into_owned(),
    );
    spec
}

fn started_session(events: &[ExecEvent]) -> Option<String> {
    events.iter().find_map(|e| match e {
        ExecEvent::Started { session, .. } => Some(session.0.clone()),
        _ => None,
    })
}

/// The CLI has no conversation for a resumed session: it says so and exits, and reused forever
/// that id broke the chat for good. The run carries on in a fresh session, which `Started` then
/// names.
#[tokio::test]
async fn an_unrestorable_session_is_relaunched_fresh() {
    let unknown = tempfile::tempdir().unwrap();
    for session in ["stale-1", "stderr-only-1"] {
        let events = run_to_end(
            &executor(),
            resume_spec(session, &unknown.path().join("missing")),
        )
        .await;
        assert_eq!(
            started_session(&events).as_deref(),
            Some("sess-fresh"),
            "{session}"
        );
        assert_eq!(
            done(&events),
            (StopReason::Completed, Some("resumed=none".into()), None),
            "{session}"
        );
    }
}

/// Claude's own history says there is nothing to resume: the CLI is never asked to.
#[tokio::test]
async fn a_session_without_conversation_is_not_resumed() {
    let config = tempfile::tempdir().unwrap();
    let project = config.path().join("projects").join("-tmp");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(
        project.join("stale-2.jsonl"),
        "{\"type\":\"queue-operation\",\"operation\":\"enqueue\"}\n",
    )
    .unwrap();
    std::fs::write(
        project.join("live-1.jsonl"),
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\n",
    )
    .unwrap();

    let events = run_to_end(&executor(), resume_spec("stale-2", config.path())).await;
    assert_eq!(done(&events).1.as_deref(), Some("resumed=none"));
    assert_eq!(started_session(&events).as_deref(), Some("sess-fresh"));

    // A real conversation resumes as ever.
    let events = run_to_end(&executor(), resume_spec("live-1", config.path())).await;
    assert_eq!(done(&events).1.as_deref(), Some("resumed=live-1"));
    assert_eq!(started_session(&events).as_deref(), Some("live-1"));

    // No transcript here is no verdict: the CLI may keep its history under another config
    // root, so it is still asked.
    let events = run_to_end(&executor(), resume_spec("kept-1", config.path())).await;
    assert_eq!(done(&events).1.as_deref(), Some("resumed=kept-1"));
    // Truly gone, the CLI's own answer still starts it fresh.
    let events = run_to_end(&executor(), resume_spec("stale-3", config.path())).await;
    assert_eq!(done(&events).1.as_deref(), Some("resumed=none"));
}

/// A crash must still end the stream with a `Done`, carrying the exit and the last stderr line,
/// which land just after stdout closes.
#[tokio::test]
async fn a_crash_mid_turn_ends_the_run_with_the_reason() {
    let events = run_to_end(&executor(), spec("scenario:crash-mid-turn")).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error, "{events:?}");
    let diagnostic = diagnostic.expect("crash message");
    assert!(diagnostic.contains("exit code 3"), "{diagnostic}");
    assert!(diagnostic.contains("heap out of memory"), "{diagnostic}");
    assert_eq!(dones(&events), 1);
}

/// The CLI waits for an answer to every control request: one we do not serve gets an error
/// reply rather than silence.
#[tokio::test]
async fn unknown_control_requests_are_answered() {
    let events = run_to_end(&executor(), spec("scenario:unknown-control")).await;
    assert_eq!(done(&events).1.as_deref(), Some("answered"));
}

#[tokio::test]
async fn a_missing_binary_is_not_installed_and_nothing_is_started() {
    let executor = ClaudeExecutor::new().with_executable("/nonexistent/claude-nowhere");
    let (controls, _handle) = ExecControls::new(8);
    let error = executor
        .start(spec("scenario:happy"), controls)
        .await
        .err()
        .expect("start fails");
    assert!(matches!(error, ExecError::NotInstalled(_)), "{error:?}");
}

#[tokio::test]
async fn a_missing_working_directory_is_a_failed_run_not_a_missing_binary() {
    let tmp = tempfile::tempdir().unwrap();
    let mut spec = spec("scenario:happy");
    spec.cwd = tmp.path().join("deleted-project");
    let events = run_to_end(&executor(), spec).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(
        diagnostic
            .as_deref()
            .unwrap_or_default()
            .contains("does not exist"),
        "{diagnostic:?}"
    );
}

/// ARCH-15: an access mode this executor will not honour is refused before anything is spawned.
#[tokio::test]
async fn an_unsupported_access_mode_fails_before_anything_is_spawned() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("spawned");
    let script = dir.path().join("claude");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let executor = ClaudeExecutor::new()
        .with_executable(&script)
        .with_access_modes(vec![AccessMode::Supervised]);
    assert_eq!(
        executor.descriptor().caps.access_modes,
        vec![AccessMode::Supervised]
    );

    let (controls, _handle) = ExecControls::new(8);
    let error = executor
        .start(spec_with("hello", AccessMode::FullAccess), controls)
        .await
        .err()
        .expect("FullAccess is refused");
    match error {
        ExecError::UnsupportedAccess {
            requested,
            supported,
        } => {
            assert_eq!(requested, AccessMode::FullAccess);
            assert_eq!(supported, vec![AccessMode::Supervised]);
        }
        other => panic!("{other:?}"),
    }
    assert!(!marker.exists(), "the CLI was started");
}

#[tokio::test]
async fn structured_output_is_refused_before_anything_is_spawned() {
    let (controls, _handle) = ExecControls::new(8);
    let mut spec = spec("hello");
    spec.output_schema = Some(serde_json::json!({"type": "object"}));
    let error = executor()
        .start(spec, controls)
        .await
        .err()
        .expect("refused");
    assert!(
        matches!(error, ExecError::UnsupportedOutputSchema),
        "{error:?}"
    );
}

// ---------------------------------------------------------------------------
// Descriptor and probe
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_descriptor_declares_what_the_driver_really_does() {
    use reno_harness::claude::{MIN_CLI_VERSION, VERIFIED_CLI_VERSION};
    let descriptor = executor().descriptor();
    assert_eq!(descriptor.id, "claude-code".parse::<ExecutorId>().unwrap());
    assert_eq!(descriptor.cli_verified.min, MIN_CLI_VERSION);
    assert_eq!(descriptor.cli_verified.verified, VERIFIED_CLI_VERSION);
    assert!(
        descriptor.adapter_version.contains("claude"),
        "{descriptor:?}"
    );
    let caps = descriptor.caps;
    assert_eq!(caps.steering, Steering::StepBoundary);
    assert!(caps.resume);
    assert_eq!(
        caps.access_modes,
        vec![
            AccessMode::Supervised,
            AccessMode::AutoAcceptEdits,
            AccessMode::FullAccess
        ]
    );
    // The CLI has no sandbox of its own that Reno can set: only "no sandbox" is honoured.
    assert_eq!(caps.sandbox_levels, vec![SandboxLevel::Unrestricted]);
    assert!(caps.mcp_injection);
    assert!(!caps.structured_output);
    assert!(caps.deterministic_turn_end);
    // Not requestable through `ExecSpec` yet, so not declared (ARCH-15).
    assert!(!caps.plan_mode);
    assert!(!caps.models.is_empty());
    assert!(caps.models.iter().all(|m| !m.id.is_empty()));
}

/// The fake CLI behind a wrapper that sets `env`, so each test picks the CLI's version and
/// sign-in state without touching this process's environment.
fn shim(dir: &Path, env: &[(&str, &str)]) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let exports: String = env
        .iter()
        .map(|(key, value)| format!("export {key}='{value}'\n"))
        .collect();
    let path = dir.join("claude");
    std::fs::write(
        &path,
        format!(
            "#!/bin/sh\n{exports}exec '{}' \"$@\"\n",
            fixture_path().display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// An executor on the fake CLI in the given state, blind to credentials in this machine's
/// environment.
fn probed(env: &[(&str, &str)]) -> (ClaudeExecutor, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let executor = ClaudeExecutor::new()
        .with_executable(shim(dir.path(), env))
        .with_probe_env(|_| None)
        .with_probe_budget(Duration::from_secs(2));
    (executor, dir)
}

#[tokio::test]
async fn probe_reports_where_the_cli_is_its_version_and_that_it_is_signed_in() {
    let (executor, _dir) = probed(&[]);
    let health = executor.probe().await;
    let InstallStatus::Installed { path, version } = &health.installed else {
        panic!("{health:?}");
    };
    assert!(path.ends_with("claude"));
    assert_eq!(version.as_deref(), Some("2.1.325"));
    assert_eq!(health.version_gate, VersionGate::Ok);
    assert_eq!(
        health.signed_in,
        SignedIn::Yes {
            account: Some("me@example.com".into())
        }
    );
    assert_eq!(health.models.source, ModelSource::Curated);
    assert!(!health.models.models.is_empty());
    assert_eq!(health.ineligible(), None);
    // The descriptor now carries the version the probe read.
    assert!(
        executor
            .descriptor()
            .adapter_version
            .contains("CLI 2.1.325")
    );
}

#[tokio::test]
async fn a_signed_out_cli_is_ineligible_and_says_how_to_sign_in() {
    let (executor, _dir) = probed(&[("FAKE_CLAUDE_AUTH", "out")]);
    let health = executor.probe().await;
    assert_eq!(
        health.signed_in,
        SignedIn::No {
            hint: "run `claude auth login`".into()
        }
    );
    assert!(matches!(
        health.ineligible(),
        Some(Ineligible::SignedOut(_))
    ));
}

#[tokio::test]
async fn a_credential_in_the_environment_is_a_login_and_asks_the_cli_nothing() {
    let dir = tempfile::tempdir().unwrap();
    // The CLI would say signed out; the key wins, as it does for the CLI itself.
    let executor = ClaudeExecutor::new()
        .with_executable(shim(dir.path(), &[("FAKE_CLAUDE_AUTH", "out")]))
        .with_probe_env(|name| (name == "ANTHROPIC_API_KEY").then(|| "sk-test".into()));
    assert_eq!(
        executor.probe().await.signed_in,
        SignedIn::Yes { account: None }
    );
}

#[tokio::test]
async fn sign_in_is_unknown_when_the_cli_cannot_tell_in_time() {
    for state in ["garbage", "broken", "hang"] {
        let (executor, _dir) = probed(&[("FAKE_CLAUDE_AUTH", state)]);
        let health = executor
            .with_probe_budget(Duration::from_millis(300))
            .probe()
            .await;
        assert_eq!(health.signed_in, SignedIn::Unknown, "{state}");
        assert_eq!(health.ineligible(), None, "{state}: unknown stays eligible");
    }
}

#[tokio::test]
async fn an_old_cli_is_too_old_and_is_not_asked_about_its_login() {
    let (executor, _dir) = probed(&[
        ("FAKE_CLAUDE_VERSION", "2.1.100"),
        ("FAKE_CLAUDE_AUTH", "out"),
    ]);
    let health = executor.probe().await;
    assert!(matches!(
        &health.version_gate,
        VersionGate::TooOld { min } if min == "2.1.228"
    ));
    assert_eq!(health.signed_in, SignedIn::Unknown);
    assert!(matches!(
        health.ineligible(),
        Some(Ineligible::TooOld { .. })
    ));
}

#[tokio::test]
async fn a_newer_cli_than_the_verified_one_is_untested_but_eligible() {
    let (executor, _dir) = probed(&[("FAKE_CLAUDE_VERSION", "2.9.0")]);
    let health = executor.probe().await;
    assert!(matches!(health.version_gate, VersionGate::Untested { .. }));
    assert_eq!(health.ineligible(), None);
}

#[tokio::test]
async fn a_cli_that_is_not_there_is_not_installed() {
    let missing = ClaudeExecutor::new()
        .with_executable("/nonexistent/claude-nowhere")
        .probe()
        .await;
    assert!(matches!(
        missing.installed,
        InstallStatus::NotInstalled {
            why: NotInstalledReason::Unusable { .. }
        }
    ));
    assert_eq!(missing.version_gate, VersionGate::Unchecked);
    assert!(matches!(
        missing.ineligible(),
        Some(Ineligible::NotInstalled(_))
    ));
}

// ---------------------------------------------------------------------------
// Live: the real CLI. Opt in with `--ignored`.
// ---------------------------------------------------------------------------

/// Live smoke against the REAL claude CLI (2.1.x, must be installed and authed): one trivial
/// turn through the stdio permission channel, ending on the result frame.
/// `cargo test -p reno-harness --test claude -- --ignored`.
#[tokio::test]
#[ignore = "spawns the real claude CLI; needs install + auth + network"]
async fn live_real_cli_single_turn() {
    let mut spec = spec_with("Reply with exactly the word: pong", AccessMode::Supervised);
    spec.model = Some("haiku".into());
    let mut live = start(&ClaudeExecutor::new(), spec).await;
    let outcome = tokio::time::timeout(
        Duration::from_secs(120),
        drive(&mut live, |_| Decision::Allow),
    )
    .await
    .expect("live turn finished in time");
    assert!(
        outcome
            .events
            .iter()
            .any(|e| matches!(e, ExecEvent::Started { .. })),
        "{:?}",
        outcome.events
    );
    assert_eq!(done(&outcome.events).0, StopReason::Completed);
}

/// The real CLI, resuming a session it has never seen: it refuses, and the run continues in a
/// fresh session. An invalid key keeps it free: the fresh session fails its API call and ends in
/// error, which is the point: it ran. `cargo test -p reno-harness --test claude -- --ignored
/// live_unrestorable`.
#[tokio::test]
#[ignore = "spawns the real claude CLI"]
async fn live_unrestorable_session_relaunches_fresh() {
    let config = tempfile::tempdir().unwrap();
    let mut spec = spec("Reply PONG");
    spec.cwd = config.path().to_path_buf();
    spec.resume = Some(SessionRef("11111111-2222-3333-4444-555555555555".into()));
    // No `projects` dir yet: the history cannot be checked, so the CLI is asked to resume and
    // the relaunch path runs.
    spec.env.insert(
        "CLAUDE_CONFIG_DIR".into(),
        config.path().join("cfg").to_string_lossy().into_owned(),
    );
    spec.env
        .insert("ANTHROPIC_API_KEY".into(), "sk-ant-invalid".into());
    let mut live = start(&ClaudeExecutor::new(), spec).await;
    let outcome = tokio::time::timeout(
        Duration::from_secs(60),
        drive(&mut live, |_| Decision::Allow),
    )
    .await
    .expect("run finished in time");
    eprintln!("{:#?}", outcome.events);
    assert!(
        started_session(&outcome.events)
            .is_some_and(|s| s != "11111111-2222-3333-4444-555555555555")
    );
    assert_eq!(done(&outcome.events).0, StopReason::Error);
}
