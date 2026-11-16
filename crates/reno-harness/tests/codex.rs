//! `CodexExecutor` integration tests against the fake app-server in
//! `tests/fixtures/fake-codex.sh` (no real `codex` binary involved). Live tests against the
//! real CLI are at the bottom, `#[ignore]`d.
//!
//! Scenarios on the native contract, read as `ExecEvent`s, with approvals as `NeedsApproval` answered through
//! `ExecHandle::decide`. What the contract has no place for (title runs, slash commands, turn
//! stop, background listings, live reconfiguration, plan requests, subagent transcripts) lost
//! its tests with its code; the recorded subagent captures are replayed to prove the child's
//! traffic stays out of the run's feed.

#![cfg(unix)]
// Tests unwrap freely, and probe a leaked pid with `kill(pid, 0)`.
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use reno_harness::exec::{Decision, ExecControls, ExecError, ExecHandle, Executor, InputAnswer};
use reno_harness::{CodexExecutor, ProcessRecord};
use reno_proto::{
    AccessMode, ActionClass, ApprovalSubject, Effort, ExecEvent, ExecSpec, ExecutorId, Ineligible,
    InstallStatus, ModelSource, NotInstalledReason, Question, RunId, SandboxLevel, SessionRef,
    SignedIn, Steering, StopReason, ToolCall, VersionGate,
};
use serde_json::json;
use tokio::sync::mpsc;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn fixture_path() -> PathBuf {
    let path = fixture_dir().join("fake-codex.sh");
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    path
}

fn executor() -> CodexExecutor {
    CodexExecutor::new().with_executable(fixture_path())
}

/// A full-access, unrestricted run in `/tmp`: what the `happy` scenario verifies on the wire.
fn spec(prompt: &str) -> ExecSpec {
    ExecSpec {
        run_id: RunId::from_suffix("01HZX9").unwrap(),
        prompt: prompt.into(),
        cwd: PathBuf::from("/tmp"),
        model: None,
        effort: None,
        access: AccessMode::FullAccess,
        sandbox: SandboxLevel::Unrestricted,
        resume: None,
        mcp: Vec::new(),
        env: Default::default(),
        output_schema: None,
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

async fn start(executor: &CodexExecutor, spec: ExecSpec) -> Live {
    let (controls, handle) = ExecControls::new(8);
    start_with(executor, spec, controls, handle).await
}

async fn start_with(
    executor: &CodexExecutor,
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
    approvals: Vec<(String, ApprovalSubject)>,
    questions: Vec<Vec<Question>>,
}

/// Drive `live` to its end. Approvals go to `decide`; the last option of every question (or
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
                    outcome
                        .approvals
                        .push((request_id.clone(), subject.clone()));
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

async fn run_to_end(executor: &CodexExecutor, spec: ExecSpec) -> Vec<ExecEvent> {
    let mut live = start(executor, spec).await;
    drive(&mut live, |_| Decision::Allow).await.events
}

async fn run_with(
    executor: &CodexExecutor,
    spec: ExecSpec,
    decide: impl FnMut(&ApprovalSubject) -> Decision,
) -> Outcome {
    let mut live = start(executor, spec).await;
    drive(&mut live, decide).await
}

fn done(events: &[ExecEvent]) -> (StopReason, Option<String>, Option<String>) {
    match events.last() {
        Some(ExecEvent::Done {
            stop,
            result,
            diagnostic,
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

/// The text of every `Text` event, joined.
fn all_text(events: &[ExecEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            ExecEvent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// The result of a scenario that says how the app-server saw each answer.
fn result_of(events: &[ExecEvent]) -> String {
    match done(events) {
        (StopReason::Completed, Some(result), _) => result,
        other => panic!("run did not complete: {other:?} in {events:?}"),
    }
}

fn started_session(events: &[ExecEvent]) -> Option<String> {
    events.iter().find_map(|e| match e {
        ExecEvent::Started { session, .. } => Some(session.0.clone()),
        _ => None,
    })
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn happy_path_maps_notifications_to_exec_events() {
    let mut spec = spec("scenario:happy");
    spec.model = Some("gpt-5.6-sol".into());
    spec.effort = Some(Effort::High);
    let events = run_to_end(&executor(), spec).await;

    // The fake verifies the wire params (turn and thread), and fails the turn when one is off.
    assert_eq!(
        events,
        vec![
            ExecEvent::Started {
                session: SessionRef("th-1".into()),
                model: "gpt-5.6-sol".into(),
            },
            text("Hello"),
            // Reasoning text and summary are separate parts: kept apart.
            ExecEvent::Reasoning {
                text: "thinking hard".into()
            },
            ExecEvent::Reasoning {
                text: "\n\n".into()
            },
            ExecEvent::Reasoning {
                text: "summary".into()
            },
            ExecEvent::Tool {
                id: "c1".into(),
                call: ToolCall::new("exec", json!({ "command": "ls -la" }), ActionClass::Command),
            },
            ExecEvent::ToolResult {
                id: "c1".into(),
                ok: false,
                summary: Some("exit code 1: no such file".into()),
            },
            ExecEvent::Tool {
                id: "f1".into(),
                call: ToolCall::new(
                    "apply_patch",
                    json!({ "changes": [{ "path": "/tmp/new.rs", "kind": "add" }] }),
                    ActionClass::Edit,
                ),
            },
            ExecEvent::ToolResult {
                id: "f1".into(),
                ok: true,
                summary: None,
            },
            ExecEvent::Tool {
                id: "mcp1".into(),
                call: ToolCall::new(
                    "mcp__linear__search",
                    json!({ "q": "bug" }),
                    ActionClass::McpTool,
                ),
            },
            ExecEvent::ToolResult {
                id: "mcp1".into(),
                ok: false,
                summary: None,
            },
            ExecEvent::Tool {
                id: "w1".into(),
                call: ToolCall::new(
                    "web_search",
                    json!({ "query": "rust" }),
                    ActionClass::Network
                ),
            },
            ExecEvent::ToolResult {
                id: "w1".into(),
                ok: true,
                summary: None,
            },
            // Completion-only: opened and closed together.
            ExecEvent::Tool {
                id: "td1".into(),
                call: ToolCall::new(
                    "todo",
                    json!({ "items": [
                        { "text": "a", "completed": true },
                        { "text": "b", "completed": false },
                    ] }),
                    ActionClass::Other,
                ),
            },
            ExecEvent::ToolResult {
                id: "td1".into(),
                ok: true,
                summary: None,
            },
            // The streamed message is not repeated by its completion; each message closes as
            // a paragraph; a message that never streamed arrives whole.
            text("\n\n"),
            text("unstreamed tail"),
            text("\n\n"),
            ExecEvent::Usage {
                input_tokens: 42,
                output_tokens: 7,
            },
            ExecEvent::Done {
                stop: StopReason::Completed,
                result: Some("unstreamed tail".into()),
                diagnostic: None,
            },
        ]
    );
}

#[tokio::test]
async fn child_reasoning_never_reaches_the_runs_feed() {
    let events = run_to_end(&executor(), spec("scenario:reasoning")).await;
    let reasoning: String = events
        .iter()
        .filter_map(|e| match e {
            ExecEvent::Reasoning { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        reasoning,
        "**Implementing file badges**\n\n**Preparing fixture screenshots**\n\nChecking the final result."
    );
    assert_eq!(dones(&events), 1);
}

#[tokio::test]
async fn image_generation_is_a_tool_call_without_the_inline_payload() {
    for (scenario, ok) in [
        ("scenario:image-success", true),
        ("scenario:image-failure", false),
        ("scenario:image-missing", false),
    ] {
        let events = run_to_end(&executor(), spec(scenario)).await;
        assert!(
            !format!("{events:?}").contains("INLINE_IMAGE_SENTINEL"),
            "{scenario}: {events:?}"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, ExecEvent::Tool { id, .. } if id == "image-1"))
                .count(),
            1,
            "{scenario}: one call, however many notifications: {events:?}"
        );
        assert!(
            events.iter().any(|e| matches!(
                e,
                ExecEvent::ToolResult { id, ok: got, .. } if id == "image-1" && *got == ok
            )),
            "{scenario}: {events:?}"
        );
        assert!(events.iter().any(|e| matches!(
            e,
            ExecEvent::Tool { call, .. } if call.name == "image_generation" && call.class == ActionClass::Other
        )));
        assert_eq!(done(&events).0, StopReason::Completed);
    }
}

#[tokio::test]
async fn view_image_reads_as_a_file_read() {
    let events = run_to_end(&executor(), spec("scenario:view-image")).await;
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ExecEvent::Tool { .. }))
            .count(),
        1
    );
    assert!(events.contains(&ExecEvent::Tool {
        id: "exec-view-1".into(),
        call: ToolCall::new(
            "view_image",
            json!({ "path": "/Users/me/Documents/Codex/mockup.png" }),
            ActionClass::Read,
        ),
    }));
    assert!(events.contains(&ExecEvent::ToolResult {
        id: "exec-view-1".into(),
        ok: true,
        summary: None,
    }));
}

#[tokio::test]
async fn a_failed_turn_is_an_error_done() {
    let events = run_to_end(&executor(), spec("scenario:fail")).await;
    assert_eq!(
        events.last(),
        Some(&ExecEvent::Done {
            stop: StopReason::Error,
            result: None,
            diagnostic: Some("boom".into()),
        })
    );
}

#[tokio::test]
async fn a_turn_interrupted_elsewhere_is_not_a_completed_answer() {
    let events = run_to_end(&executor(), spec("scenario:foreign-interrupt")).await;
    assert_eq!(done(&events).0, StopReason::Interrupted);
    assert_eq!(dones(&events), 1);
}

/// Codex's own retries ("Reconnecting... 2/5") are progress, never a failure, and the final
/// error is reported once, by the Done, in words that say what to do.
#[tokio::test]
async fn retry_notices_stay_out_of_the_run_and_the_final_error_lands_once() {
    let events = run_to_end(&executor(), spec("scenario:retry-errors")).await;
    assert_eq!(dones(&events), 1, "{events:?}");
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    let diagnostic = diagnostic.unwrap();
    assert!(
        diagnostic.starts_with("Codex usage limit reached — it resets in 2h 1"),
        "{diagnostic}"
    );
    assert!(!diagnostic.contains("Reconnecting"), "{diagnostic}");
}

/// A final error the app-server reported before dying mid-turn is not lost with the turn's end:
/// the crash Done carries it, ahead of the exit and the stderr it printed.
#[tokio::test]
async fn a_crash_after_a_final_error_reports_both() {
    let events = run_to_end(&executor(), spec("scenario:crash-after-error")).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    let diagnostic = diagnostic.unwrap();
    assert!(
        diagnostic.starts_with("Codex's credentials were rejected. Run codex login"),
        "{diagnostic}"
    );
    assert!(
        diagnostic.contains("codex app-server exited unexpectedly")
            && diagnostic.contains("lost the model client"),
        "{diagnostic}"
    );
}

#[tokio::test]
async fn a_crash_during_thread_setup_reports_its_stderr() {
    let dir = tempfile::tempdir().unwrap();
    let cwd = dir.path().join("crash-on-thread-start");
    std::fs::create_dir(&cwd).unwrap();
    let mut spec = spec("scenario:happy");
    spec.cwd = cwd;
    let events = run_to_end(&executor(), spec).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    let diagnostic = diagnostic.unwrap();
    assert!(diagnostic.contains("exit code 3"), "{diagnostic}");
    assert!(
        diagnostic.contains("bad toml in mcp_servers"),
        "{diagnostic}"
    );
}

#[tokio::test]
async fn a_refused_notification_opt_out_is_retried_without_it() {
    // The fake refuses the first handshake when asked to, and exits if the retry still opts out.
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("codex");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nFAKE_CODEX_REJECT_OPT_OUT=1 exec '{}' \"$@\"\n",
                fixture_path().display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let events = run_to_end(
        &CodexExecutor::new().with_executable(script),
        spec("scenario:resumed"),
    )
    .await;
    assert_eq!(done(&events).0, StopReason::Completed, "{events:?}");
}

// ---------------------------------------------------------------------------
// Subagents: a child's traffic is not the run's
// ---------------------------------------------------------------------------

/// What the parent's own feed holds after a multi-agent capture: its text, and the spawn calls
/// on its own thread, never the children's.
fn assert_children_stay_out(events: &[ExecEvent], parent_text: &str, hidden: &[&str]) {
    assert_eq!(dones(events), 1, "one Done, the parent's: {events:?}");
    assert_eq!(done(events).0, StopReason::Completed, "{events:?}");
    assert_eq!(all_text(events), parent_text, "{events:?}");
    let rendered = format!("{events:?}");
    for child in hidden {
        assert!(!rendered.contains(child), "{child} leaked: {events:?}");
    }
}

#[tokio::test]
async fn child_thread_routing_never_settles_or_feeds_the_parent() {
    let events = run_to_end(&executor(), spec("scenario:subagent")).await;
    assert_eq!(dones(&events), 1, "one parent Done only: {events:?}");
    // Parent output that follows the child's turn/completed still streams.
    let late = events
        .iter()
        .position(|e| matches!(e, ExecEvent::Text { text } if text == "parent still going"))
        .expect("parent delta after the child's turn end");
    assert!(late < events.len() - 1);
    // The spawn marker lives on the parent's feed and resolves; the root's own activity is no
    // call at all.
    assert!(events.iter().any(|e| matches!(
        e,
        ExecEvent::Tool { id, call } if id == "call_alpha" && call.name == "sub_agent" && call.class == ActionClass::Other
    )));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ExecEvent::ToolResult { id, ok: true, .. } if id == "call_alpha"))
    );
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ExecEvent::Tool { id, .. } if id == "call_root")),
        "{events:?}"
    );
    // Nothing the child did is the parent's.
    let rendered = format!("{events:?}");
    for child in ["child says hi", "echo hi", "cs1", "also check the rebuild"] {
        assert!(!rendered.contains(child), "{child} leaked: {events:?}");
    }
}

#[tokio::test]
async fn recorded_v1_subagents_stay_out_of_the_parents_feed() {
    let events = run_to_end(&executor(), spec("scenario:v1-subagents")).await;
    assert_children_stay_out(
        &events,
        "parent answer",
        &["alpha answer", "beta answer", "echo beta", "beta-input"],
    );
    // The parent's own collaboration calls are on its feed.
    for (id, name) in [
        ("spawn-alpha", "spawn_agent"),
        ("spawn-beta", "spawn_agent"),
        ("message-beta", "send_input"),
        ("wait-both", "wait"),
    ] {
        assert!(
            events.iter().any(|e| matches!(
                e,
                ExecEvent::Tool { id: got, call } if got == id && call.name == name && call.class == ActionClass::Other
            )),
            "{id}: {events:?}"
        );
    }
}

#[tokio::test]
async fn recorded_v2_lifecycle_stays_out_of_the_parents_feed() {
    let events = run_to_end(&executor(), spec("scenario:v2-lifecycle")).await;
    assert_children_stay_out(
        &events,
        "parent progress\n\nparent answer\n\n",
        &[
            "first alpha",
            "second alpha",
            "beta answer",
            "First assignment",
        ],
    );
}

#[tokio::test]
async fn a_child_that_acts_before_its_spawn_is_announced_stays_out() {
    let events = run_to_end(&executor(), spec("scenario:child-identity")).await;
    // The parent's text arrives as deltas, here without a completed item.
    assert_children_stay_out(
        &events,
        "parent output",
        &[
            "early alpha",
            "later alpha",
            "beta output",
            "beta continues",
        ],
    );
}

/// The notifications of a real codex-cli 0.146.1 multi-agent run (the first two lines of the
/// capture are its responses, which the fake already sent).
#[tokio::test]
async fn the_live_multi_agent_capture_replays_to_one_run() {
    let events = run_to_end(&executor(), spec("scenario:live-multi-agent")).await;
    assert_eq!(dones(&events), 1, "{events:?}");
    assert_eq!(done(&events).0, StopReason::Completed, "{events:?}");
    assert!(
        events.iter().any(|e| matches!(e, ExecEvent::Usage { .. })),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_childs_approval_request_is_answered_like_the_parents() {
    let outcome = run_with(
        &executor(),
        spec_with("scenario:child-approval", AccessMode::Supervised),
        |_| Decision::Deny,
    )
    .await;
    assert_eq!(outcome.approvals.len(), 1, "{:?}", outcome.events);
    let subject = &outcome.approvals[0].1;
    assert_eq!(subject.action, ActionClass::Command);
    assert_eq!(subject.command.as_deref(), Some("make deploy"));
    // The request says which thread asked.
    assert_eq!(subject.raw["threadId"], "child-1");
    assert_eq!(result_of(&outcome.events), "child refused");
    assert_eq!(dones(&outcome.events), 1);
}

// ---------------------------------------------------------------------------
// Structured approvals
// ---------------------------------------------------------------------------

#[tokio::test]
async fn command_and_file_change_approvals_are_structured_and_answered_by_decision() {
    let outcome = run_with(
        &executor(),
        spec_with("scenario:approve", AccessMode::Supervised),
        |_| Decision::Allow,
    )
    .await;
    assert!(outcome.questions.is_empty(), "approvals are not questions");
    let [(_, command), (_, file)] = outcome.approvals.as_slice() else {
        panic!("two approvals expected: {:?}", outcome.approvals);
    };

    assert_eq!(command.action, ActionClass::Command);
    assert_eq!(command.tool, "exec");
    assert_eq!(command.command.as_deref(), Some("rm -rf /tmp/x"));
    assert_eq!(command.paths, vec![PathBuf::from("/tmp")]);
    // The request as Codex sent it, for the audit record.
    assert_eq!(command.raw["itemId"], "c1");
    assert_eq!(command.raw["command"], "rm -rf /tmp/x");

    // The request names only its item; the paths came with the item's own notification.
    assert_eq!(file.action, ActionClass::Edit);
    assert_eq!(file.tool, "apply_patch");
    assert_eq!(file.command, None);
    assert_eq!(file.paths, vec![PathBuf::from("/tmp/a.rs")]);
    assert_eq!(file.raw["reason"], "write outside the sandbox");

    assert_eq!(result_of(&outcome.events), "command=accept file=accept");
}

#[tokio::test]
async fn a_deny_round_trips_as_decline_and_never_as_a_session_grant() {
    let outcome = run_with(
        &executor(),
        spec_with("scenario:decline", AccessMode::Supervised),
        |_| Decision::Deny,
    )
    .await;
    // An argv command is shown as one line.
    assert_eq!(outcome.approvals[0].1.command.as_deref(), Some("rm -rf /"));
    assert_eq!(result_of(&outcome.events), "declined");
}

/// The app-server's approval reply is `{decision}` and has no field for a reason, so a policy
/// rule's deny message cannot reach the model through Codex: the run is declined all the same,
/// with the message only logged. This pins that a message neither changes the reply nor breaks
/// the decline.
#[tokio::test]
async fn a_policy_deny_message_cannot_ride_the_decline_but_the_decline_holds() {
    let mut live = start(
        &executor(),
        spec_with("scenario:decline", AccessMode::Supervised),
    )
    .await;
    let outcome = drive_with_messages(&mut live, |_| {
        (
            Decision::Deny,
            Some("denied by policy: no destructive commands".into()),
        )
    })
    .await;
    assert_eq!(outcome.approvals.len(), 1);
    assert_eq!(result_of(&outcome.events), "declined");
}

#[tokio::test]
async fn a_file_change_request_carrying_its_changes_needs_no_earlier_item() {
    let supervised = spec_with("scenario:file-change-inline", AccessMode::Supervised);
    let outcome = run_with(&executor(), supervised.clone(), |_| Decision::Deny).await;
    assert_eq!(
        outcome.approvals[0].1.paths,
        vec![PathBuf::from("/tmp/a.rs"), PathBuf::from("/tmp/b.rs")]
    );
    assert_eq!(result_of(&outcome.events), "file=decline");
    let outcome = run_with(&executor(), supervised, |_| Decision::Allow).await;
    assert_eq!(result_of(&outcome.events), "file=accept");
}

#[tokio::test]
async fn full_access_asks_nothing() {
    for scenario in ["scenario:one-approval", "scenario:file-change-inline"] {
        let outcome = run_with(&executor(), spec(scenario), |_| Decision::Deny).await;
        assert!(outcome.approvals.is_empty(), "{scenario}");
        assert!(
            ["ran", "file=accept"].contains(&result_of(&outcome.events).as_str()),
            "{scenario}: {:?}",
            outcome.events
        );
    }
}

/// A request Reno was never asked about and a person never saw is not allowed because it
/// arrived under full access with a narrower sandbox than the spec's: permissions Codex asks to
/// widen are the sandbox's whole point.
#[tokio::test]
async fn a_permission_widening_is_asked_unless_nothing_is_confined() {
    // Unrestricted sandbox under full access: nothing to widen, answered here.
    let outcome = run_with(&executor(), spec("scenario:permissions"), |_| {
        Decision::Deny
    })
    .await;
    assert!(outcome.approvals.is_empty());
    assert_eq!(result_of(&outcome.events), "permissions=turn");

    // Confined sandbox: Other, with what was asked in `raw`.
    let mut confined = spec("scenario:permissions");
    confined.sandbox = SandboxLevel::WorkspaceWrite;
    for (decision, expected) in [
        (Decision::Allow, "permissions=turn"),
        (Decision::Deny, "permissions=none"),
    ] {
        let outcome = run_with(&executor(), confined.clone(), |_| decision).await;
        let [(_, subject)] = outcome.approvals.as_slice() else {
            panic!("one approval expected: {:?}", outcome.approvals);
        };
        assert_eq!(subject.action, ActionClass::Other);
        assert_eq!(subject.tool, "permissions");
        assert_eq!(subject.raw["permissions"]["network"]["enabled"], true);
        assert_eq!(
            subject.raw["permissions"]["fileSystem"]["write"],
            json!(["/repo/.cache"])
        );
        assert_eq!(subject.raw["reason"], "fetch deps");
        assert_eq!(result_of(&outcome.events), expected);
    }
}

#[tokio::test]
async fn an_mcp_tool_approval_is_an_mcp_tool_action() {
    for (decision, expected) in [
        (Decision::Allow, "tool=accept"),
        (Decision::Deny, "tool=decline"),
    ] {
        let outcome = run_with(
            &executor(),
            spec_with("scenario:mcp-approval", AccessMode::Supervised),
            |_| decision,
        )
        .await;
        assert!(outcome.questions.is_empty());
        let [(_, subject)] = outcome.approvals.as_slice() else {
            panic!("one approval expected: {:?}", outcome.approvals);
        };
        assert_eq!(subject.action, ActionClass::McpTool);
        assert_eq!(subject.tool, "mcp__reno__board_post");
        assert_eq!(subject.raw["serverName"], "reno");
        assert_eq!(subject.raw["_meta"]["tool_params"]["text"], "hi");
        assert_eq!(result_of(&outcome.events), expected);
    }
}

#[tokio::test]
async fn a_form_elicitation_is_a_question() {
    let outcome = run_with(
        &executor(),
        spec_with("scenario:elicitation-form", AccessMode::Supervised),
        |_| Decision::Deny,
    )
    .await;
    assert!(outcome.approvals.is_empty());
    let [questions] = outcome.questions.as_slice() else {
        panic!("one question set expected: {:?}", outcome.questions);
    };
    assert_eq!(questions.len(), 1);
    assert!(questions[0].prompt.contains("Ship it?"), "{questions:?}");
    assert_eq!(questions[0].options, ["Yes", "No"]);
    // The driver answers with the last option ("No"): an accepted form whose boolean is false.
    assert_eq!(result_of(&outcome.events), "form=accepted-false");
}

#[tokio::test]
async fn a_form_elicitation_answered_yes_is_accepted_with_typed_content() {
    let mut live = start(
        &executor(),
        spec_with("scenario:elicitation-form", AccessMode::Supervised),
    )
    .await;
    let mut events = Vec::new();
    while let Some(event) = live.stream.next().await {
        if let ExecEvent::NeedsInput {
            request_id,
            questions,
        } = &event
        {
            live.handle
                .answer_input(
                    request_id.clone(),
                    vec![InputAnswer {
                        question_id: questions[0].id.clone(),
                        labels: vec!["Yes".into()],
                    }],
                )
                .await
                .unwrap();
        }
        events.push(event);
    }
    assert_eq!(result_of(&events), "form=accepted");
}

#[tokio::test]
async fn an_agent_question_is_a_question_answered_by_id() {
    let outcome = run_with(
        &executor(),
        spec_with("scenario:user-input", AccessMode::Supervised),
        |_| Decision::Deny,
    )
    .await;
    assert!(outcome.approvals.is_empty());
    let [questions] = outcome.questions.as_slice() else {
        panic!("one question set expected: {:?}", outcome.questions);
    };
    assert_eq!(questions[0].prompt, "Mode: Which mode?");
    assert_eq!(questions[0].options, ["fast", "safe"]);
    assert!(!questions[0].multi_select);
    // The driver answered with the last option.
    assert_eq!(result_of(&outcome.events), "mode=safe");
}

#[tokio::test]
async fn no_answers_declines_a_question() {
    let mut live = start(
        &executor(),
        spec_with("scenario:user-input", AccessMode::Supervised),
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
    assert_eq!(result_of(&events), "mode=none");
}

#[tokio::test]
async fn a_wrong_kind_of_answer_never_approves() {
    let mut live = start(
        &executor(),
        spec_with("scenario:one-approval", AccessMode::Supervised),
    )
    .await;
    let mut events = Vec::new();
    while let Some(event) = live.stream.next().await {
        if let ExecEvent::NeedsApproval { request_id, .. } = &event {
            // "Yes", typed as an answer to a question, is not a decision: the request stays open
            // and the real decision still lands.
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
            live.handle
                .decide(request_id.clone(), Decision::Deny)
                .await
                .unwrap();
        }
        events.push(event);
    }
    assert_eq!(result_of(&events), "refused");
}

/// Under auto-accept edits Codex still asks about everything it does not know to be safe, file
/// changes included. A change whose every path is inside the working directory is accepted
/// without asking; every other request reaches Reno.
#[tokio::test]
async fn auto_accept_edits_accepts_only_changes_inside_the_working_directory() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = spec_with("scenario:auto-edits", AccessMode::AutoAcceptEdits);
    spec.cwd = dir.path().to_path_buf();
    spec.sandbox = SandboxLevel::WorkspaceWrite;
    let outcome = run_with(&executor(), spec, |_| Decision::Deny).await;
    let asked: Vec<_> = outcome
        .approvals
        .iter()
        .map(|(_, s)| (s.action, s.paths.clone()))
        .collect();
    assert_eq!(
        asked,
        vec![
            (ActionClass::Edit, vec![PathBuf::from("/outside/a.rs")]),
            (ActionClass::Edit, vec![dir.path().join("../escape.rs")]),
            // Never seen: no paths known, so asked.
            (ActionClass::Edit, Vec::new()),
            // A session-wide grant is asked even for the working directory itself.
            (
                ActionClass::Edit,
                vec![dir.path().join("c.rs"), dir.path().to_path_buf()]
            ),
            (ActionClass::Command, vec![PathBuf::from("/work")]),
            (ActionClass::Other, Vec::new()),
        ],
        "{:?}",
        outcome.events
    );
    assert_eq!(result_of(&outcome.events), "all asked");
}

/// A file change counts as inside the working directory only once symlinks are followed: a link
/// inside it that points out is outside, a `..` after a link climbs from the link's target, and
/// a path that does not exist yet is judged by its deepest ancestor that does.
#[tokio::test]
async fn auto_accept_edits_follows_symlinks_before_accepting() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("real")).unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
    let mut spec = spec_with("scenario:edits-through-links", AccessMode::AutoAcceptEdits);
    spec.cwd = dir.path().to_path_buf();
    spec.sandbox = SandboxLevel::WorkspaceWrite;
    let outcome = run_with(&executor(), spec, |_| Decision::Deny).await;
    let asked: Vec<_> = outcome
        .approvals
        .iter()
        .flat_map(|(_, s)| s.paths.clone())
        .collect();
    assert_eq!(
        asked,
        vec![
            dir.path().join("link/b.rs"),
            dir.path().join("link/../c.rs"),
            dir.path().join("missing/../e.rs"),
        ],
        "{:?}",
        outcome.events
    );
    assert_eq!(result_of(&outcome.events), "links resolved");
}

#[tokio::test]
async fn cancel_declines_every_open_request_before_interrupting() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = spec_with("scenario:open-then-cancel", AccessMode::Supervised);
    spec.cwd = dir.path().to_path_buf();
    let executor = executor().with_graces(Duration::from_secs(5), Duration::from_secs(5));
    let mut live = start(&executor, spec).await;
    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(event) = live.stream.next().await {
            if matches!(event, ExecEvent::NeedsApproval { .. }) {
                live.handle.cancel();
            }
            events.push(event);
        }
    })
    .await
    .expect("cancelled run ends");
    assert_eq!(done(&events).0, StopReason::Interrupted, "{events:?}");
    assert_eq!(dones(&events), 1);
    let seen = std::fs::read_to_string(dir.path().join("open-request.txt")).unwrap();
    assert_eq!(seen.trim(), "declined");
}

#[tokio::test]
async fn a_request_that_arrives_after_cancel_is_declined_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = spec_with("scenario:request-after-cancel", AccessMode::Supervised);
    spec.cwd = dir.path().to_path_buf();
    let executor = executor().with_graces(Duration::from_secs(5), Duration::from_secs(5));
    let mut live = start(&executor, spec).await;
    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(20), async {
        while let Some(event) = live.stream.next().await {
            // Once the turn is running: only then is there a turn to interrupt.
            if matches!(&event, ExecEvent::Text { text } if text == "working") {
                live.handle.cancel();
            }
            events.push(event);
        }
    })
    .await
    .expect("cancelled run ends");
    assert_eq!(done(&events).0, StopReason::Interrupted, "{events:?}");
    assert!(
        !events.iter().any(|e| matches!(
            e,
            ExecEvent::NeedsApproval { .. } | ExecEvent::NeedsInput { .. }
        )),
        "nothing is asked of a run that was cancelled: {events:?}"
    );
    let seen = std::fs::read_to_string(dir.path().join("late-request.txt")).unwrap();
    assert_eq!(seen.trim(), "declined");
}

// ---------------------------------------------------------------------------
// Access modes and the sandbox
// ---------------------------------------------------------------------------

#[tokio::test]
async fn each_access_mode_launches_with_its_policy_and_never_a_looser_sandbox_than_asked() {
    for (access, sandbox, expected) in [
        (
            AccessMode::FullAccess,
            SandboxLevel::Unrestricted,
            "thread=never/danger-full-access turn=never/dangerFullAccess",
        ),
        // The spec's sandbox is a ceiling: full access never asks, but stays confined.
        (
            AccessMode::FullAccess,
            SandboxLevel::WorkspaceWrite,
            "thread=never/workspace-write turn=never/workspaceWrite",
        ),
        (
            AccessMode::AutoAcceptEdits,
            SandboxLevel::Unrestricted,
            "thread=untrusted/workspace-write turn=untrusted/workspaceWrite",
        ),
        (
            AccessMode::AutoAcceptEdits,
            SandboxLevel::ReadOnly,
            "thread=untrusted/read-only turn=untrusted/readOnly",
        ),
        // A mode that asks never gets more than read-only without the person's say.
        (
            AccessMode::Supervised,
            SandboxLevel::Unrestricted,
            "thread=untrusted/read-only turn=untrusted/readOnly",
        ),
        (
            AccessMode::Supervised,
            SandboxLevel::ReadOnly,
            "thread=untrusted/read-only turn=untrusted/readOnly",
        ),
    ] {
        let mut spec = spec_with("scenario:policy", access);
        spec.sandbox = sandbox;
        let events = run_to_end(&executor(), spec).await;
        assert_eq!(
            done(&events).1.as_deref(),
            Some(expected),
            "{access:?} + {sandbox:?}: {events:?}"
        );
    }
}

#[tokio::test]
async fn reno_s_own_mcp_server_is_pre_approved_only_when_nobody_is_asked() {
    for access in [
        AccessMode::FullAccess,
        AccessMode::AutoAcceptEdits,
        AccessMode::Supervised,
    ] {
        let mut spec = spec_with("scenario:mcp-config", access);
        spec.mcp = vec![
            reno_proto::McpServerSpec {
                name: "reno".into(),
                command: "/opt/reno/reno".into(),
                args: vec!["mcp".into()],
                env: [("RENO_RUN".to_owned(), "r1".to_owned())].into(),
            },
            reno_proto::McpServerSpec {
                name: "plugin-focus".into(),
                command: "/opt/focus/server".into(),
                args: Vec::new(),
                env: Default::default(),
            },
        ];
        let events = run_to_end(&executor(), spec).await;
        assert_eq!(result_of(&events), "configured", "{access:?}: {events:?}");
    }
}

/// ARCH-15: an access mode this executor will not honour is refused before anything is spawned.
#[tokio::test]
async fn an_unsupported_access_mode_fails_before_anything_is_spawned() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("spawned");
    let script = dir.path().join("codex");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let executor = CodexExecutor::new()
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
    spec.output_schema = Some(json!({"type": "object"}));
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

#[tokio::test]
async fn a_missing_binary_is_not_installed_and_nothing_is_started() {
    let executor = CodexExecutor::new().with_executable("/nonexistent/codex-nowhere");
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

// ---------------------------------------------------------------------------
// Controls: steering, cancel, process records
// ---------------------------------------------------------------------------

#[tokio::test]
async fn steering_uses_turn_steer_with_the_expected_turn_id() {
    let (controls, handle) = ExecControls::new(8);
    handle.steer("redirect please").await.unwrap();
    let mut live = start_with(&executor(), spec("scenario:steer"), controls, handle).await;
    let events = drive(&mut live, |_| Decision::Allow).await.events;
    // The fake only continues when `turn/steer` carried `expectedTurnId` and the text.
    assert_eq!(all_text(&events), "firststeered");
    assert_eq!(done(&events).0, StopReason::Completed);
    assert_eq!(dones(&events), 1);
}

#[tokio::test]
async fn an_accepted_steer_joins_the_turn_and_the_run_ends_once() {
    let (controls, handle) = ExecControls::new(8);
    handle.steer("redirect please").await.unwrap();
    let mut live = start_with(&executor(), spec("scenario:steer-item"), controls, handle).await;
    let events = drive(&mut live, |_| Decision::Allow).await.events;
    assert_eq!(done(&events).0, StopReason::Completed, "{events:?}");
    assert_eq!(dones(&events), 1);
    let first = events
        .iter()
        .position(|e| matches!(e, ExecEvent::Text { text } if text == "-still-first"))
        .expect("the reply streaming ahead of the steer finishes first");
    let steered = events
        .iter()
        .position(|e| matches!(e, ExecEvent::Text { text } if text == "steered"))
        .expect("the steered reply");
    assert!(first < steered, "{events:?}");
}

/// A steer that lost the race with the turn's end starts the next turn, and the first turn's
/// completion is not the run's end.
#[tokio::test]
async fn a_rejected_steer_falls_back_to_a_follow_up_turn_inside_one_run() {
    let (controls, handle) = ExecControls::new(8);
    handle.steer("redirect please").await.unwrap();
    let mut live = start_with(&executor(), spec("scenario:steer-race"), controls, handle).await;
    let events = drive(&mut live, |_| Decision::Allow).await.events;
    assert_eq!(all_text(&events), "fallback", "{events:?}");
    assert_eq!(dones(&events), 1, "{events:?}");
    assert_eq!(done(&events).0, StopReason::Completed);
}

#[tokio::test]
async fn a_stale_turn_completion_never_settles_the_running_turn() {
    let events = run_to_end(&executor(), spec("scenario:stale-completion")).await;
    assert_eq!(dones(&events), 1, "{events:?}");
    assert_eq!(all_text(&events), "beforeafter");
    assert_eq!(done(&events).0, StopReason::Completed);
}

#[tokio::test]
async fn cancel_sends_turn_interrupt_and_ends_with_an_interrupted_done() {
    let mut live = start(&executor(), spec("scenario:interrupt")).await;
    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = live.stream.next().await {
            if matches!(&event, ExecEvent::Text { text } if text == "working") {
                live.handle.cancel();
            }
            events.push(event);
        }
    })
    .await
    .expect("interrupt completed in time");
    assert_eq!(
        events.last(),
        Some(&ExecEvent::Done {
            stop: StopReason::Interrupted,
            result: None,
            diagnostic: None,
        })
    );
    assert_eq!(dones(&events), 1);
}

#[tokio::test]
async fn an_unresponsive_child_is_reaped_with_an_interrupted_done() {
    let executor = executor().with_graces(Duration::from_millis(100), Duration::from_millis(500));
    let mut live = start(&executor, spec("scenario:wedge")).await;
    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = live.stream.next().await {
            if matches!(&event, ExecEvent::Text { text } if text == "working") {
                live.handle.cancel();
            }
            events.push(event);
        }
    })
    .await
    .expect("escalation completed in time");
    assert_eq!(
        events.last(),
        Some(&ExecEvent::Done {
            stop: StopReason::Interrupted,
            result: None,
            diagnostic: None,
        })
    );
}

/// A turn the app-server never acknowledges is not a run that cannot be cancelled.
#[tokio::test]
async fn cancel_ends_a_run_whose_turn_start_is_never_answered() {
    let executor = executor().with_graces(Duration::from_millis(100), Duration::from_millis(500));
    let mut live = start(&executor, spec("scenario:silent-start")).await;
    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(event) = live.stream.next().await {
            if matches!(event, ExecEvent::Started { .. }) {
                live.handle.cancel();
            }
            events.push(event);
        }
    })
    .await
    .expect("the cancelled run ended");
    assert_eq!(done(&events).0, StopReason::Interrupted, "{events:?}");
    assert_eq!(dones(&events), 1);
}

/// ARCH-11: every task a run spawned is the executor's, and ends with the run's stream.
#[tokio::test]
async fn shutdown_waits_for_the_tasks_of_runs_whose_streams_were_dropped() {
    let executor = executor().with_graces(Duration::from_millis(100), Duration::from_millis(500));
    let mut live = start(&executor, spec("scenario:wedge")).await;
    while let Some(event) = live.stream.next().await {
        if matches!(&event, ExecEvent::Text { text } if text == "working") {
            break;
        }
    }
    drop(live.stream);
    tokio::time::timeout(Duration::from_secs(20), executor.shutdown())
        .await
        .expect("the run's tasks end once its stream is dropped");
}

/// The run ends with its `Done`: the app-server, which ignores SIGTERM here, is reaped with it.
#[tokio::test]
async fn the_app_server_is_reaped_when_the_run_ends() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = spec("scenario:lease-shutdown");
    spec.cwd = dir.path().to_path_buf();
    let executor = executor().with_graces(Duration::from_millis(20), Duration::from_millis(200));
    let events = run_to_end(&executor, spec).await;
    assert_eq!(done(&events).0, StopReason::Completed);
    tokio::time::timeout(Duration::from_secs(10), executor.shutdown())
        .await
        .expect("shutdown waits for the teardown");
    let pid: i32 = std::fs::read_to_string(dir.path().join("lease-child.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: signal 0 only probes whether the pid exists.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "the app-server lives on");
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

#[tokio::test]
async fn the_spawned_childs_process_record_is_forwarded() {
    let mut live = start(&executor(), spec("scenario:happy")).await;
    let record = tokio::time::timeout(Duration::from_secs(10), live.records.recv())
        .await
        .expect("a record in time")
        .expect("the child was reported");
    assert!(record.pid > 0);
    // The app-server is spawned as its own group leader.
    assert_eq!(record.pgid, record.pid);
    assert!(record.start_time > 0);
    let _ = drive(&mut live, |_| Decision::Allow).await;
}

// ---------------------------------------------------------------------------
// Relaunch and resume
// ---------------------------------------------------------------------------

/// A wrapper around the fake app-server that dies (printing `reason`) on its first `fail`
/// launches, then runs the fake. Counts launches in `count`.
fn flaky_app_server(dir: &Path, fail: u32, reason: &str) -> (PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let count = dir.join("launches");
    let script = dir.join("codex");
    std::fs::write(
        &script,
        format!(
            "#!/bin/sh\nn=$(cat '{count}' 2>/dev/null || echo 0)\nn=$((n+1))\necho $n > '{count}'\n\
             if [ \"$n\" -le {fail} ]; then echo '{reason}' >&2; exit 1; fi\nexec '{fake}' \"$@\"\n",
            count = count.display(),
            fake = fixture_path().display(),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    (script, count)
}

/// Every launch is a child the orphan sweep must know: the first, and each relaunch.
#[tokio::test]
async fn an_app_server_that_dies_before_answering_is_relaunched_and_every_launch_is_reported() {
    let dir = tempfile::tempdir().unwrap();
    let (script, count) = flaky_app_server(dir.path(), 1, "cold start: not ready");
    let executor = CodexExecutor::new()
        .with_executable(script)
        .with_startup_retry_delays(vec![Duration::from_millis(10)]);
    let mut spec = spec("scenario:happy");
    spec.model = Some("gpt-5.6-sol".into());
    spec.effort = Some(Effort::High);
    let mut live = start(&executor, spec).await;
    let events = drive(&mut live, |_| Decision::Allow).await.events;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ExecEvent::Started { .. })),
        "the second launch runs the turn: {events:?}"
    );
    assert_eq!(done(&events).0, StopReason::Completed, "{events:?}");
    assert_eq!(std::fs::read_to_string(count).unwrap().trim(), "2");
    let mut pids = Vec::new();
    while let Ok(record) = live.records.try_recv() {
        pids.push(record.pid);
    }
    assert_eq!(
        pids.len(),
        2,
        "the dead launch and its replacement: {pids:?}"
    );
    assert_ne!(pids[0], pids[1]);
}

#[tokio::test]
async fn an_app_server_that_keeps_dying_reports_what_it_printed() {
    let dir = tempfile::tempdir().unwrap();
    let (script, count) = flaky_app_server(dir.path(), 99, "missing libssl.so.3");
    let executor = CodexExecutor::new()
        .with_executable(script)
        .with_startup_retry_delays(vec![Duration::from_millis(10); 3]);
    let mut live = start(&executor, spec("scenario:happy")).await;
    // Three relaunches.
    let events = tokio::time::timeout(Duration::from_secs(30), async {
        drive(&mut live, |_| Decision::Allow).await.events
    })
    .await
    .expect("gives up in time");
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    let diagnostic = diagnostic.unwrap();
    assert!(diagnostic.starts_with("initialize:"), "{diagnostic}");
    assert!(
        diagnostic.contains("missing libssl.so.3"),
        "the stderr tail: {diagnostic}"
    );
    assert_eq!(
        std::fs::read_to_string(count).unwrap().trim(),
        "4",
        "one launch + three relaunches"
    );
    let mut reported = 0;
    while live.records.try_recv().is_ok() {
        reported += 1;
    }
    assert_eq!(reported, 4, "every launch is reported");
}

/// A missing rollout, and an empty one (unreadable for good), both resume as a fresh thread; the
/// `Started` event names the thread the run really has.
#[tokio::test]
async fn resume_falls_back_to_a_fresh_thread() {
    for resume in ["resume-fail", "resume-empty"] {
        let mut spec = spec("scenario:resumed");
        spec.resume = Some(SessionRef(resume.into()));
        let events = run_to_end(&executor(), spec).await;
        assert_eq!(started_session(&events).as_deref(), Some("th-fresh"));
        assert_eq!(done(&events).0, StopReason::Completed, "{events:?}");
    }
}

#[tokio::test]
async fn resume_reuses_the_existing_thread() {
    let mut spec = spec("scenario:resumed");
    spec.resume = Some(SessionRef("some-thread".into()));
    let events = run_to_end(&executor(), spec).await;
    assert_eq!(started_session(&events).as_deref(), Some("th-resumed"));
    assert_eq!(done(&events).0, StopReason::Completed);
}

#[tokio::test]
async fn an_archived_thread_is_unarchived_and_resumed() {
    let mut spec = spec("scenario:resumed");
    spec.resume = Some(SessionRef("resume-archived".into()));
    let events = run_to_end(&executor(), spec).await;
    assert_eq!(started_session(&events).as_deref(), Some("th-unarchived"));
    assert_eq!(done(&events).0, StopReason::Completed, "{events:?}");
}

#[tokio::test]
async fn a_resume_refusal_fails_the_run_instead_of_starting_fresh() {
    let mut spec = spec("scenario:resumed");
    spec.resume = Some(SessionRef("resume-broken".into()));
    let events = run_to_end(&executor(), spec).await;
    assert_ne!(
        started_session(&events).as_deref(),
        Some("th-wrongly-fresh")
    );
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(
        diagnostic
            .unwrap_or_default()
            .contains("failed to load configuration")
    );
}

/// A resumed thread still in plan mode, on a run that never asks for it: the turn names `default`
/// with the model and effort the thread reported (the fake fails the turn otherwise).
#[tokio::test]
async fn a_resumed_thread_left_in_plan_mode_is_taken_out_of_it() {
    let mut spec = spec("scenario:leave-plan");
    spec.resume = Some(SessionRef("resume-in-plan".into()));
    let events = run_to_end(&executor(), spec).await;
    assert_eq!(done(&events).0, StopReason::Completed, "{events:?}");
}

// ---------------------------------------------------------------------------
// Descriptor, probe, models
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_descriptor_declares_what_the_driver_really_does() {
    use reno_harness::codex::{MIN_CLI_VERSION, VERIFIED_CLI_VERSION};
    let descriptor = executor().descriptor();
    assert_eq!(descriptor.id, "codex".parse::<ExecutorId>().unwrap());
    assert_eq!(descriptor.cli_verified.min, "0.146.0");
    assert_eq!(descriptor.cli_verified.verified, "0.169.0");
    assert_eq!(descriptor.cli_verified.min, MIN_CLI_VERSION);
    assert_eq!(descriptor.cli_verified.verified, VERIFIED_CLI_VERSION);
    assert!(
        descriptor.adapter_version.contains("codex"),
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
    // The spec's level is a ceiling the driver never exceeds, so each is honoured.
    assert_eq!(
        caps.sandbox_levels,
        vec![
            SandboxLevel::ReadOnly,
            SandboxLevel::WorkspaceWrite,
            SandboxLevel::Unrestricted
        ]
    );
    assert!(caps.mcp_injection);
    assert!(!caps.structured_output);
    assert!(caps.deterministic_turn_end);
    // Not requestable through `ExecSpec`, so not declared (ARCH-15).
    assert!(!caps.plan_mode);
    assert!(!caps.models.is_empty());
    assert!(caps.models.iter().all(|m| !m.id.is_empty()));
    assert_eq!(caps.models[0].id, "gpt-6-astra");
}

/// The fake app-server behind a wrapper that sets `env`: its version and its account state.
fn shim(dir: &Path, env: &[(&str, &str)]) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let exports: String = env
        .iter()
        .map(|(key, value)| format!("export {key}='{value}'\n"))
        .collect();
    let path = dir.join("codex");
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

fn probed(env: &[(&str, &str)]) -> (CodexExecutor, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let executor = CodexExecutor::new()
        .with_executable(shim(dir.path(), env))
        .with_graces(Duration::from_millis(50), Duration::from_millis(500))
        .with_probe_budget(Duration::from_secs(5));
    (executor, dir)
}

#[tokio::test]
async fn probe_reports_the_cli_a_login_and_the_live_models() {
    let (executor, _dir) = probed(&[]);
    let health = executor.probe().await;
    let InstallStatus::Installed { path, version } = &health.installed else {
        panic!("{health:?}");
    };
    assert!(path.ends_with("codex"));
    assert_eq!(version.as_deref(), Some("0.169.0"));
    assert_eq!(health.version_gate, VersionGate::Ok);
    assert_eq!(
        health.signed_in,
        SignedIn::Yes {
            account: Some("me@example.com".into())
        }
    );
    // The account's catalog replaces the curated list, and says so.
    assert_eq!(health.models.source, ModelSource::Live);
    let ids: Vec<_> = health.models.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["gpt-6-astra", "gpt-5.6-terra", "gpt-5.6-sol"]);
    assert_eq!(executor.descriptor().caps.models, health.models.models);
    assert_eq!(health.ineligible(), None);
}

#[tokio::test]
async fn a_codex_with_no_login_is_ineligible() {
    let (executor, _dir) = probed(&[("FAKE_CODEX_ACCOUNT", "signedout")]);
    let health = executor.probe().await;
    assert_eq!(
        health.signed_in,
        SignedIn::No {
            hint: "run `codex login`".into()
        }
    );
    assert!(matches!(
        health.ineligible(),
        Some(Ineligible::SignedOut(_))
    ));
}

#[tokio::test]
async fn an_api_key_is_a_login_and_a_provider_needing_none_is_unknown() {
    let (executor, _dir) = probed(&[("FAKE_CODEX_ACCOUNT", "apikey")]);
    assert_eq!(
        executor.probe().await.signed_in,
        SignedIn::Yes { account: None }
    );
    let (executor, _dir) = probed(&[("FAKE_CODEX_ACCOUNT", "noauth")]);
    assert_eq!(executor.probe().await.signed_in, SignedIn::Unknown);
}

#[tokio::test]
async fn a_method_the_app_server_lacks_leaves_sign_in_unknown_and_the_models_live() {
    let (executor, _dir) = probed(&[("FAKE_CODEX_ACCOUNT", "error")]);
    let health = executor.probe().await;
    assert_eq!(health.signed_in, SignedIn::Unknown);
    assert_eq!(health.models.source, ModelSource::Live);
}

#[tokio::test]
async fn an_app_server_that_hangs_is_bounded_and_leaves_the_curated_models() {
    let (executor, _dir) = probed(&[("FAKE_CODEX_ACCOUNT", "hang")]);
    let health = executor
        .with_probe_budget(Duration::from_millis(400))
        .probe()
        .await;
    assert!(matches!(health.installed, InstallStatus::Installed { .. }));
    assert_eq!(health.signed_in, SignedIn::Unknown);
    assert_eq!(health.models.source, ModelSource::Curated);
    assert_eq!(health.models.models.len(), 9);
}

#[tokio::test]
async fn an_old_codex_is_too_old_and_its_app_server_is_not_started() {
    // The wrapper would hang an app-server; a too-old CLI is never asked.
    let (executor, _dir) = probed(&[
        ("FAKE_CODEX_VERSION", "0.120.0"),
        ("FAKE_CODEX_ACCOUNT", "hang"),
    ]);
    let health = executor.probe().await;
    assert!(matches!(
        &health.version_gate,
        VersionGate::TooOld { min } if min == "0.146.0"
    ));
    assert_eq!(health.signed_in, SignedIn::Unknown);
    assert_eq!(health.models.source, ModelSource::Curated);
    assert!(matches!(
        health.ineligible(),
        Some(Ineligible::TooOld { .. })
    ));
}

#[tokio::test]
async fn a_codex_that_is_not_there_is_not_installed() {
    let missing = CodexExecutor::new()
        .with_executable("/nonexistent/codex-nowhere")
        .probe()
        .await;
    assert!(matches!(
        missing.installed,
        InstallStatus::NotInstalled {
            why: NotInstalledReason::Unusable { .. }
        }
    ));
    assert_eq!(missing.signed_in, SignedIn::Unknown);
}

#[tokio::test]
async fn a_later_failed_probe_keeps_the_last_live_models() {
    let (executor, dir) = probed(&[]);
    assert_eq!(executor.probe().await.models.source, ModelSource::Live);
    // The same executor, its CLI now an app-server that fails: the last live list stays.
    std::fs::write(dir.path().join("codex"), "#!/bin/sh\nexit 1\n").unwrap();
    let health = executor.probe().await;
    assert_eq!(health.models.source, ModelSource::Live);
    assert_eq!(health.models.models.len(), 3);
    assert_eq!(health.signed_in, SignedIn::Unknown);
}

#[tokio::test]
async fn discovered_models_replace_the_curated_list_and_a_failed_probe_keeps_the_last() {
    let executor = executor();
    assert_eq!(executor.descriptor().caps.models.len(), 9, "curated list");
    let found = executor.discover_models().await.expect("discovery");
    // Hidden models are not offered; the account's default leads.
    let ids: Vec<_> = found.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["gpt-6-astra", "gpt-5.6-terra", "gpt-5.6-sol"]);
    assert_eq!(found[0].display_name.as_deref(), Some("GPT-6-Astra"));
    assert_eq!(executor.descriptor().caps.models, found);

    // A later failure leaves what was found.
    let failing = tempfile::tempdir().unwrap();
    let exe = failing.path().join("codex");
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(&exe, "#!/bin/sh\nexit 1\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let broken = CodexExecutor::new().with_executable(&exe);
    assert!(broken.discover_models().await.is_err());
    assert_eq!(
        broken.descriptor().caps.models.len(),
        9,
        "the curated list stays when nothing was ever found"
    );
}

/// An empty catalog is a failure, never an empty offer; and the discovery child is gone either
/// way.
#[tokio::test]
async fn an_empty_catalog_is_an_error_and_the_discovery_child_is_reaped() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("codex");
    std::fs::write(
        &exe,
        format!(
            "#!/bin/sh\necho $$ >> '{pids}'\n\
             rid() {{ printf '%s' \"$1\" | sed 's/.*\"id\":\\([0-9]*\\).*/\\1/'; }}\n\
             read -r l; printf '{{\"id\":%s,\"result\":{{}}}}\\n' \"$(rid \"$l\")\"\n\
             read -r l\n\
             read -r l; printf '{{\"id\":%s,\"result\":{{\"data\":[],\"nextCursor\":null}}}}\\n' \"$(rid \"$l\")\"\n\
             exec sleep 30\n",
            pids = dir.path().join("pids").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
    let executor = CodexExecutor::new()
        .with_executable(&exe)
        .with_graces(Duration::from_millis(50), Duration::from_millis(500));
    let error = executor.discover_models().await.unwrap_err();
    assert!(error.to_string().contains("empty"), "{error}");
    assert_eq!(executor.descriptor().caps.models.len(), 9);
    let pid: i32 = std::fs::read_to_string(dir.path().join("pids"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: signal 0 only probes whether the pid exists.
    assert_eq!(unsafe { libc::kill(pid, 0) }, -1, "discovery child {pid}");
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ESRCH)
    );
}

// ---------------------------------------------------------------------------
// Live: the real CLI. Opt in with `--ignored`.
// ---------------------------------------------------------------------------

/// Live smoke against the REAL codex app-server (must be installed and signed in): one trivial
/// turn. `cargo test -p reno-harness --test codex -- --ignored live_real_app_server`.
#[tokio::test]
#[ignore = "spawns the real codex app-server; needs install + auth + network"]
async fn live_real_app_server_single_turn() {
    let mut spec = spec_with("Reply with exactly the word: pong", AccessMode::Supervised);
    spec.sandbox = SandboxLevel::ReadOnly;
    spec.cwd = std::env::temp_dir();
    let mut live = start(&CodexExecutor::new(), spec).await;
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

/// The real app-server's approval request for a command: confirms the 0.169.0 wire shape the
/// fixtures only assume. Needs a model that runs a shell command when asked.
#[tokio::test]
#[ignore = "spawns the real codex app-server; needs install + auth + network"]
async fn live_supervised_command_asks_through_a_structured_approval() {
    let mut spec = spec_with(
        "Run the shell command `echo reno-approval-probe` and report its output.",
        AccessMode::Supervised,
    );
    spec.cwd = std::env::temp_dir();
    let mut live = start(&CodexExecutor::new(), spec).await;
    let outcome = tokio::time::timeout(
        Duration::from_secs(180),
        drive(&mut live, |_| Decision::Deny),
    )
    .await
    .expect("live turn finished in time");
    assert!(
        outcome
            .approvals
            .iter()
            .any(|(_, s)| s.action == ActionClass::Command),
        "{:?}",
        outcome.events
    );
}
