//! `AcpExecutor` integration tests against the fake ACP agents in `tests/fixtures` (no real
//! `grok`, `devin`, `hermes` or Antigravity binary is involved, ARCH-16).
//!
//! Scenarios on the native contract, read
//! as `ExecEvent`s, with approvals as `NeedsApproval` answered through `ExecHandle::decide` and
//! questions as `NeedsInput`. What the contract has no place for (model and command discovery,
//! sign-in outside a run, turn stop, live reconfiguration, subagent chips, an idle steer between
//! turns) is not covered here. One run is one `Done`.

#![cfg(unix)]
// Tests unwrap freely, and probe a leaked pid with `kill(pid, 0)`.
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use reno_harness::RegistryAgent;
use reno_harness::exec::{Decision, ExecControls, ExecError, ExecHandle, Executor, InputAnswer};
use reno_harness::{AcpExecutor, ProcessRecord};
use reno_proto::{
    AccessMode, ActionClass, ApprovalSubject, Effort, ExecEvent, ExecSpec, ExecutorId, Ineligible,
    InstallStatus, McpServerSpec, ModelSource, NotInstalledReason, Question, RunId, SandboxLevel,
    SessionRef, SignedIn, Steering, StopReason, ToolCall, VersionGate,
};
use tokio::sync::mpsc;

fn fixture(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    path
}

/// Grok over the fake agent. Grok honors full access only.
fn grok() -> AcpExecutor {
    AcpExecutor::grok().with_executable(fixture("fake-acp.sh"))
}

/// A registry agent over the fake agent: it honors `Supervised` and `FullAccess`, so approvals
/// reach Reno.
fn registry() -> AcpExecutor {
    AcpExecutor::registry(RegistryAgent::new("fake-agent").unwrap(), "Fake agent")
        .unwrap()
        .with_executable(fixture("fake-acp.sh"))
}

fn antigravity() -> AcpExecutor {
    AcpExecutor::antigravity().with_executable(fixture("fake-antigravity-acp.sh"))
}

fn spec(prompt: &str) -> ExecSpec {
    ExecSpec {
        run_id: RunId::from_suffix("01HZX9").unwrap(),
        inherit_user_config: false,
        prompt: prompt.into(),
        cwd: PathBuf::from("/tmp"),
        model: Some("grok-4.5".into()),
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

/// A spec for a run with no model, in `dir` (the Antigravity fixture reads its scenario from the
/// directory's name).
fn spec_in(prompt: &str, dir: &tempfile::TempDir) -> ExecSpec {
    ExecSpec {
        model: None,
        cwd: dir.path().to_path_buf(),
        ..spec(prompt)
    }
}

fn workspace(prefix: &str) -> tempfile::TempDir {
    tempfile::Builder::new().prefix(prefix).tempdir().unwrap()
}

/// A started run: its events, its handle and the process-record queue.
struct Live {
    stream: BoxStream<'static, ExecEvent>,
    handle: ExecHandle,
    records: mpsc::Receiver<ProcessRecord>,
}

async fn start(executor: &AcpExecutor, spec: ExecSpec) -> Live {
    let (controls, handle) = ExecControls::new(8);
    start_with(executor, spec, controls, handle).await
}

async fn start_with(
    executor: &AcpExecutor,
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

/// How a run's questions are answered: the labels chosen for each, or `None` to decline.
type Answerer = fn(&Question) -> Option<String>;

fn first_option(question: &Question) -> Option<String> {
    question.options.first().cloned()
}

/// Drive `live` to its end. Approvals go to `decide`; questions go to `answer`.
async fn drive_with(
    live: &mut Live,
    mut decide: impl FnMut(&ApprovalSubject) -> Decision,
    answer: Answerer,
) -> Outcome {
    drive_deciding(live, |subject| (decide(subject), None), answer).await
}

/// [`drive`], with the message each decision comes with (a policy rule's deny message).
async fn drive_with_messages(
    live: &mut Live,
    decide: impl FnMut(&ApprovalSubject) -> (Decision, Option<String>),
) -> Outcome {
    drive_deciding(live, decide, first_option).await
}

async fn drive_deciding(
    live: &mut Live,
    mut decide: impl FnMut(&ApprovalSubject) -> (Decision, Option<String>),
    answer: Answerer,
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
                        .filter_map(|q| {
                            answer(q).map(|label| InputAnswer {
                                question_id: q.id.clone(),
                                labels: vec![label],
                            })
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

async fn drive(live: &mut Live, decide: impl FnMut(&ApprovalSubject) -> Decision) -> Outcome {
    drive_with(live, decide, first_option).await
}

async fn run_to_end(executor: &AcpExecutor, spec: ExecSpec) -> Vec<ExecEvent> {
    let mut live = start(executor, spec).await;
    drive(&mut live, |_| Decision::Allow).await.events
}

/// Run until the first `Done` or until the stream ends, with `act` called for every event.
async fn run_acting(
    executor: &AcpExecutor,
    spec: ExecSpec,
    mut act: impl FnMut(&ExecEvent, &ExecHandle),
) -> Vec<ExecEvent> {
    let mut live = start(executor, spec).await;
    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(event) = live.stream.next().await {
            act(&event, &live.handle);
            events.push(event);
        }
    })
    .await
    .expect("run finished in time");
    events
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

/// The run ended once, `Completed`, with no diagnostic.
fn assert_completed(events: &[ExecEvent]) {
    assert_eq!(dones(events), 1, "{events:?}");
    assert_eq!(
        done(events),
        (StopReason::Completed, None, None),
        "{events:?}"
    );
}

fn text(s: &str) -> ExecEvent {
    ExecEvent::Text { text: s.into() }
}

fn texts(events: &[ExecEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| match e {
            ExecEvent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn diagnostic(events: &[ExecEvent]) -> String {
    done(events).2.unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

#[tokio::test]
async fn happy_path_maps_updates_to_exec_events_with_classified_tools() {
    let events = run_to_end(&grok(), spec("scenario:happy")).await;

    // Started names the session and the model the run selected.
    assert_eq!(
        events[0],
        ExecEvent::Started {
            session: SessionRef("s-1".into()),
            model: "grok-4.5".into(),
        }
    );
    // Chunks; the wrong-session and non-text chunks never surface.
    assert!(events.contains(&text("Hello")));
    assert!(events.contains(&ExecEvent::Reasoning {
        text: "thinking".into()
    }));
    assert!(
        !texts(&events).iter().any(|t| t.contains("WRONG")),
        "{events:?}"
    );

    // An execute call: the kind names the tool, the raw input rides verbatim, the class is
    // Command; its completed update resolves it with the last line of its output.
    assert!(events.contains(&ExecEvent::Tool {
        id: "t1".into(),
        call: ToolCall::new(
            "execute",
            serde_json::json!({ "command": "cargo test -p reno-harness" }),
            ActionClass::Command,
        ),
    }));
    assert!(events.contains(&ExecEvent::ToolResult {
        id: "t1".into(),
        ok: true,
        summary: Some("test result: ok. 13 passed; 0 failed; 0 ignored".into()),
        output: None
    }));
    // An edit resolved in one shot.
    assert!(events.contains(&ExecEvent::Tool {
        id: "t2".into(),
        call: ToolCall::new("edit", serde_json::Value::Null, ActionClass::Edit),
    }));
    assert!(events.contains(&ExecEvent::ToolResult {
        id: "t2".into(),
        ok: true,
        summary: None,
        output: None
    }));

    // Plans, command lists, context gauges and unknown updates have no place in the contract.
    let known = |e: &ExecEvent| {
        matches!(
            e,
            ExecEvent::Started { .. }
                | ExecEvent::Text { .. }
                | ExecEvent::Reasoning { .. }
                | ExecEvent::Tool { .. }
                | ExecEvent::ToolResult { .. }
                | ExecEvent::Done { .. }
        )
    };
    assert!(events.iter().all(known), "{events:?}");
    assert_completed(&events);
}

#[tokio::test]
async fn a_tool_call_is_classified_by_its_kind_and_a_late_update_cannot_change_it() {
    let events = run_to_end(&grok(), spec("scenario:many")).await;
    let reads: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            ExecEvent::Tool { id, call } => Some((id.as_str(), call.class)),
            _ => None,
        })
        .collect();
    assert_eq!(reads.len(), 14, "{events:?}");
    assert!(reads.iter().all(|(_, class)| *class == ActionClass::Read));
    // Resolved by the same update that opens them: one result each.
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ExecEvent::ToolResult { ok: true, .. }))
            .count(),
        14
    );
    assert_completed(&events);
}

#[tokio::test]
async fn config_options_apply_requested_model_and_effort() {
    let mut spec = spec("scenario:config");
    spec.effort = Some(Effort::Medium);
    let events = run_to_end(&grok(), spec).await;
    // The fixture answers refusal unless BOTH set_config_option calls (model grok-4.5, effort
    // medium) arrived before the prompt.
    assert!(events.contains(&text("configured")), "{events:?}");
    assert_completed(&events);
}

#[tokio::test]
async fn resumed_first_class_model_is_switched_before_prompt() {
    let mut spec = spec("scenario:model-api");
    spec.resume = Some(SessionRef("existing-grok-session".into()));
    let events = run_to_end(&grok(), spec).await;
    assert!(events.contains(&text("model switched")), "{events:?}");
    assert_completed(&events);
}

/// Tearing the runtime down ends everything the agent started, a command it ran in its own
/// session included (live, Devin: it outlived quitting the app).
#[tokio::test]
async fn cancel_ends_a_command_the_agent_detached() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("detached.pid");
    let mut live = start(
        &grok(),
        spec(&format!("scenario:detached {}", pidfile.display())),
    )
    .await;
    loop {
        let event = tokio::time::timeout(Duration::from_secs(10), live.stream.next())
            .await
            .expect("event in time")
            .expect("stream open");
        if text("started") == event {
            break;
        }
    }
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: signal 0 only probes for existence.
    let alive = || unsafe { libc::kill(pid, 0) } == 0;
    assert!(alive(), "the detached command runs");
    live.handle.cancel();
    while tokio::time::timeout(Duration::from_secs(10), live.stream.next())
        .await
        .expect("teardown in time")
        .is_some()
    {}
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while alive() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!alive(), "the detached command outlived the runtime");
}

#[tokio::test]
async fn a_run_that_ends_takes_what_the_agent_started_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("detached.pid");
    let events = run_to_end(
        &grok(),
        spec(&format!("scenario:detached {}", pidfile.display())),
    )
    .await;
    assert_completed(&events);
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: signal 0 only probes for existence.
    let alive = || unsafe { libc::kill(pid, 0) } == 0;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while alive() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(!alive(), "the run's end left the detached command running");
}

// ---------------------------------------------------------------------------
// Structured approvals
// ---------------------------------------------------------------------------

/// Run one tool permission of `kind` (the fixture's `scenario:permission`) under `access` and
/// say how it was decided: what reached Reno, and which option the agent was told.
async fn permission_of(
    kind: &str,
    access: AccessMode,
    decision: Decision,
) -> (Vec<ApprovalSubject>, String) {
    let mut live = start(
        &registry(),
        spec_with(&format!("scenario:permission {kind}"), access),
    )
    .await;
    let outcome = drive(&mut live, |_| decision).await;
    assert_eq!(dones(&outcome.events), 1, "{:?}", outcome.events);
    let picked = texts(&outcome.events)
        .iter()
        .find_map(|t| t.strip_prefix("picked:"))
        .unwrap_or("nothing")
        .to_owned();
    (outcome.approvals, picked)
}

/// The mapping from the request's `toolCall` to the subject: kind to class, command from
/// `rawInput`, paths from `locations`, the whole request as `raw`.
#[tokio::test]
async fn a_permission_request_becomes_a_structured_approval_subject() {
    let (approvals, picked) =
        permission_of("execute", AccessMode::Supervised, Decision::Allow).await;
    assert_eq!(approvals.len(), 1);
    let subject = &approvals[0];
    assert_eq!(subject.action, ActionClass::Command);
    assert_eq!(subject.tool, "execute");
    assert_eq!(subject.command.as_deref(), Some("rm -rf build"));
    assert!(subject.paths.is_empty(), "{subject:?}");
    // The whole request is kept for the audit record, options and all.
    assert_eq!(subject.raw["toolCall"]["kind"], "execute");
    assert_eq!(
        subject.raw["toolCall"]["rawInput"]["command"],
        "rm -rf build"
    );
    assert_eq!(subject.raw["options"].as_array().map(Vec::len), Some(4));
    assert_eq!(picked, "once");

    for kind in ["edit", "delete", "move"] {
        let (approvals, picked) =
            permission_of(kind, AccessMode::Supervised, Decision::Allow).await;
        assert_eq!(approvals.len(), 1, "{kind}");
        let subject = &approvals[0];
        assert_eq!(subject.action, ActionClass::Edit, "{kind}");
        assert_eq!(subject.tool, kind);
        assert_eq!(
            subject.paths,
            [PathBuf::from("/w/a.rs"), PathBuf::from("/w/b.rs")],
            "{kind}"
        );
        // The request carries a command too; an edit is not a command.
        assert_eq!(subject.command, None, "{kind}");
        assert_eq!(picked, "once", "{kind}");
    }

    for (kind, class) in [
        ("fetch", ActionClass::Network),
        ("other", ActionClass::Other),
        ("switch_mode", ActionClass::Other),
    ] {
        let (approvals, _) = permission_of(kind, AccessMode::Supervised, Decision::Allow).await;
        assert_eq!(approvals.len(), 1, "{kind}");
        assert_eq!(approvals[0].action, class, "{kind}");
        assert_eq!(approvals[0].command, None, "{kind}");
        assert!(approvals[0].paths.is_empty(), "{kind}");
    }
}

/// A decision selects one of the request's own options by its kind: allow is `allow_once` and
/// never the `allow_always` that sits first; deny is `reject_once`, never `reject_always`.
#[tokio::test]
async fn a_decision_selects_allow_once_or_reject_once_and_never_the_lasting_options() {
    let (_, allowed) = permission_of("execute", AccessMode::Supervised, Decision::Allow).await;
    assert_eq!(allowed, "once");
    let (approvals, denied) =
        permission_of("execute", AccessMode::Supervised, Decision::Deny).await;
    assert_eq!(approvals.len(), 1);
    assert_eq!(denied, "no");
}

/// A refusal's reason rides in the response's `_meta.denyMessage` (ACP's permission response has
/// no message field); an allow, a refusal without a reason and a blank one carry nothing.
#[tokio::test]
async fn a_policy_deny_message_rides_in_the_response_meta() {
    async fn reason_for(decision: Decision, message: Option<&str>) -> Option<String> {
        let mut live = start(
            &registry(),
            spec_with("scenario:permission execute", AccessMode::Supervised),
        )
        .await;
        let outcome =
            drive_with_messages(&mut live, |_| (decision, message.map(str::to_owned))).await;
        assert_eq!(dones(&outcome.events), 1, "{:?}", outcome.events);
        texts(&outcome.events)
            .iter()
            .find_map(|t| t.strip_prefix("reason:").map(str::to_owned))
    }
    assert_eq!(
        reason_for(Decision::Deny, Some("denied by policy: no rm -rf")).await,
        Some("denied by policy: no rm -rf".into())
    );
    assert_eq!(reason_for(Decision::Deny, None).await, None);
    assert_eq!(reason_for(Decision::Deny, Some("  ")).await, None);
    assert_eq!(reason_for(Decision::Allow, Some("ignored")).await, None);
}

/// Reading, searching and thinking are allowed in every mode (the access table on the type), and
/// answered with `allow_once`: nothing reaches Reno.
#[tokio::test]
async fn looking_around_is_answered_without_asking() {
    for kind in ["read", "search", "think"] {
        let (approvals, picked) = permission_of(kind, AccessMode::Supervised, Decision::Deny).await;
        assert!(approvals.is_empty(), "{kind}: {approvals:?}");
        assert_eq!(picked, "once", "{kind}");
    }
}

#[tokio::test]
async fn full_access_on_an_agent_that_can_ask_surfaces_every_permission_and_never_picks_allow_always()
 {
    for kind in ["execute", "edit", "fetch", "other"] {
        // The kernel (here the test) answers; a refusal is honoured in full access too.
        let (approvals, picked) = permission_of(kind, AccessMode::FullAccess, Decision::Deny).await;
        assert_eq!(approvals.len(), 1, "{kind}: {approvals:?}");
        assert_eq!(picked, "no", "{kind}");
        let (approvals, picked) =
            permission_of(kind, AccessMode::FullAccess, Decision::Allow).await;
        assert_eq!(approvals.len(), 1, "{kind}: {approvals:?}");
        assert_eq!(picked, "once", "{kind}");
    }
    // Grok honors full access only and cannot ask: it answers itself (the residual gap).
    let mut live = start(&grok(), spec("scenario:permission execute")).await;
    let outcome = drive(&mut live, |_| Decision::Deny).await;
    assert!(outcome.approvals.is_empty());
    assert!(texts(&outcome.events).contains(&"picked:once"));
}

/// An option set that is a question (options without allow/reject kinds) is `NeedsInput`, in
/// every mode, never an approval.
#[tokio::test]
async fn a_question_shaped_request_becomes_needs_input_and_is_answered_by_option_name() {
    for access in [AccessMode::Supervised, AccessMode::FullAccess] {
        let mut live = start(&registry(), spec_with("scenario:question", access)).await;
        let outcome = drive(&mut live, |_| Decision::Allow).await;
        assert!(outcome.approvals.is_empty(), "{access:?}");
        assert_eq!(outcome.questions.len(), 1, "{access:?}");
        let questions = &outcome.questions[0];
        assert_eq!(questions.len(), 1);
        assert_eq!(questions[0].prompt, "Which async runtime should I use?");
        assert_eq!(questions[0].options, ["Use tokio", "Use smol"]);
        assert!(!questions[0].multi_select);
        // "Use tokio" selected the option named so: the fixture says it saw that answer.
        assert!(texts(&outcome.events).contains(&"answered"), "{access:?}");
        assert_completed(&outcome.events);
    }
}

#[tokio::test]
async fn a_declined_question_is_cancelled_not_answered() {
    let mut live = start(
        &registry(),
        spec_with("scenario:question", AccessMode::Supervised),
    )
    .await;
    let outcome = drive_with(&mut live, |_| Decision::Allow, |_| None).await;
    assert_eq!(outcome.questions.len(), 1);
    // The fixture refuses a turn whose question was not answered with an option.
    assert_eq!(done(&outcome.events).0, StopReason::Refusal);
}

/// A request still open when the run is cancelled is answered `cancelled` (ACP requires it),
/// one that arrives afterwards is cancelled at once, and an answer of the wrong kind never
/// approves anything.
#[tokio::test]
async fn open_requests_are_cancelled_and_a_wrong_kind_answer_never_approves() {
    let mut live = start(
        &registry(),
        spec_with("scenario:permission-hold", AccessMode::Supervised),
    )
    .await;
    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(15), async {
        while let Some(event) = live.stream.next().await {
            if let ExecEvent::NeedsApproval { request_id, .. } = &event {
                // "Yes" typed as an input answer is not a decision.
                live.handle
                    .answer_input(
                        request_id.clone(),
                        vec![InputAnswer {
                            question_id: "q".into(),
                            labels: vec!["Yes".into()],
                        }],
                    )
                    .await
                    .unwrap();
                live.handle.cancel();
            }
            events.push(event);
        }
    })
    .await
    .expect("run finished in time");
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ExecEvent::NeedsApproval { .. })),
        "{events:?}"
    );
    assert!(
        texts(&events).contains(&"open:cancelled;late:cancelled"),
        "{events:?}"
    );
    assert_eq!(done(&events).0, StopReason::Interrupted);
}

// ---------------------------------------------------------------------------
// Antigravity: the agent that honors every access mode
// ---------------------------------------------------------------------------

/// A run of the fake server's `permissions` prompt (read, edit, then run `cargo test`) in
/// `access`, decided by `decide`: its reply text, what reached Reno, and how the run ended.
async fn antigravity_permissions(
    access: AccessMode,
    dir_prefix: &str,
    mut decide: impl FnMut(&ApprovalSubject) -> Decision,
) -> (String, Vec<ApprovalSubject>, Vec<ExecEvent>) {
    let dir = workspace(dir_prefix);
    let mut spec = spec_in("permissions", &dir);
    spec.model = Some("gemini-3.7-flash".into());
    spec.effort = Some(Effort::Medium);
    spec.access = access;
    let mut live = start(&antigravity(), spec).await;
    let outcome = drive(&mut live, &mut decide).await;
    let reply = texts(&outcome.events).join("");
    (reply, outcome.approvals, outcome.events)
}

fn kinds_and_commands(approvals: &[ApprovalSubject]) -> Vec<(ActionClass, Option<&str>)> {
    approvals
        .iter()
        .map(|s| (s.action, s.command.as_deref()))
        .collect()
}

#[tokio::test]
async fn antigravity_full_access_stays_in_default_and_surfaces_its_asks_for_the_kernel() {
    // Full access is routed like supervised on an agent that can ask: it stays in `default`
    // (not `yolo`) and its edits and commands reach the kernel, which allows them at once
    // unless policy refuses, so a refusal is honoured here too.
    let (reply, approvals, events) =
        antigravity_permissions(AccessMode::FullAccess, "full", |_| Decision::Deny).await;
    assert_eq!(
        reply,
        "sets:model=gemini-3.7-flash-medium; perms:read=once;edit=no;execute=no;"
    );
    assert_eq!(approvals.len(), 2, "{approvals:?}");
    assert_completed(&events);
}

#[tokio::test]
async fn antigravity_supervised_stays_in_default_and_asks_before_edits_and_commands() {
    let (reply, approvals, events) =
        antigravity_permissions(AccessMode::Supervised, "supervised", |_| Decision::Deny).await;
    // No mode set: the session opens in `default`. Reading is never asked.
    assert_eq!(
        reply,
        "sets:model=gemini-3.7-flash-medium; perms:read=once;edit=no;execute=no;"
    );
    assert_eq!(
        kinds_and_commands(&approvals),
        [
            (ActionClass::Edit, None),
            (ActionClass::Command, Some("cargo test"))
        ]
    );
    assert_eq!(approvals[0].paths, [PathBuf::from("/w/src/lib.rs")]);
    assert_eq!(approvals[0].tool, "edit");
    assert_completed(&events);

    let (reply, approvals, _) =
        antigravity_permissions(AccessMode::Supervised, "supervised", |_| Decision::Allow).await;
    assert_eq!(
        reply,
        "sets:model=gemini-3.7-flash-medium; perms:read=once;edit=once;execute=once;"
    );
    assert_eq!(approvals.len(), 2);

    // Each request is decided on its own.
    let (reply, _, _) = antigravity_permissions(AccessMode::Supervised, "supervised", |s| {
        if s.action == ActionClass::Edit {
            Decision::Allow
        } else {
            Decision::Deny
        }
    })
    .await;
    assert_eq!(
        reply,
        "sets:model=gemini-3.7-flash-medium; perms:read=once;edit=once;execute=no;"
    );
}

#[tokio::test]
async fn antigravity_auto_accept_edits_allows_edits_and_asks_before_commands() {
    let (reply, approvals, events) =
        antigravity_permissions(AccessMode::AutoAcceptEdits, "edits", |_| Decision::Deny).await;
    assert_eq!(
        reply,
        "sets:model=gemini-3.7-flash-medium; perms:read=once;edit=once;execute=no;"
    );
    assert_eq!(
        kinds_and_commands(&approvals),
        [(ActionClass::Command, Some("cargo test"))]
    );
    assert_completed(&events);
}

#[tokio::test]
async fn antigravity_supervised_leaves_yolo_or_does_not_start() {
    let (reply, _, events) =
        antigravity_permissions(AccessMode::Supervised, "yolo-current", |_| Decision::Deny).await;
    assert_eq!(
        reply,
        "sets:model=gemini-3.7-flash-medium;mode=default; perms:read=once;edit=no;execute=no;"
    );
    assert_completed(&events);

    // Stuck in yolo, the run would never be asked anything: it does not start.
    let (reply, approvals, events) =
        antigravity_permissions(AccessMode::Supervised, "yolo-current-reject-mode-", |_| {
            Decision::Allow
        })
        .await;
    assert!(reply.is_empty(), "{reply:?}");
    assert!(approvals.is_empty(), "{approvals:?}");
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(
        diagnostic
            .as_deref()
            .is_some_and(|d| d.contains("rejected session mode")),
        "{events:?}"
    );
}

/// A session that opens in a mode that approves on its own and offers no mode that asks cannot
/// run under Supervised: nothing would ever be asked.
#[tokio::test]
async fn a_supervised_run_does_not_start_in_a_session_that_cannot_ask() {
    let dir = workspace("no-asking-mode-");
    let mut spec = spec_in("permissions", &dir);
    spec.access = AccessMode::Supervised;
    let events = run_to_end(&antigravity(), spec).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error, "{events:?}");
    assert!(
        diagnostic.unwrap().contains("approves on its own"),
        "{events:?}"
    );
    assert!(texts(&events).is_empty(), "{events:?}");
    // In full access the same session is simply run.
    let mut spec = spec_in("permissions", &dir);
    spec.access = AccessMode::FullAccess;
    let events = run_to_end(&antigravity(), spec).await;
    assert_completed(&events);
}

/// Fetches, unknown kinds and deletes are asked about and classified by their kind; thinking is
/// answered here like reading.
#[tokio::test]
async fn antigravity_classifies_every_kind_it_is_asked_about() {
    let dir = workspace("kinds");
    let mut spec = spec_in("kinds", &dir);
    spec.access = AccessMode::Supervised;
    let mut live = start(&antigravity(), spec).await;
    let outcome = drive(&mut live, |_| Decision::Allow).await;
    assert_eq!(
        texts(&outcome.events),
        ["kinds:fetch=once;other=once;think=once;delete=once;"]
    );
    let asked: Vec<_> = outcome
        .approvals
        .iter()
        .map(|s| (s.action, s.tool.as_str(), s.paths.len()))
        .collect();
    assert_eq!(
        asked,
        [
            (ActionClass::Network, "fetch", 0),
            (ActionClass::Other, "spawn_subagent", 0),
            (ActionClass::Edit, "delete", 1),
        ]
    );
}

async fn antigravity_config_sets(model: &str, effort: Option<Effort>) -> Vec<ExecEvent> {
    let dir = workspace("sets");
    let mut spec = spec_in("hi", &dir);
    spec.model = Some(model.into());
    spec.effort = effort;
    run_to_end(&antigravity(), spec).await
}

#[tokio::test]
async fn antigravity_runs_the_picked_effort_variant_unattended() {
    let events = antigravity_config_sets("gemini-3.7-flash", Some(Effort::Medium)).await;
    assert!(
        events.contains(&text("sets:model=gemini-3.7-flash-medium;")),
        "{events:?}"
    );
    assert_completed(&events);
}

#[tokio::test]
async fn antigravity_stops_before_prompt_when_the_model_is_rejected() {
    let dir = workspace("reject-model");
    let mut spec = spec_in("hi", &dir);
    spec.model = Some("gemini-3.7-flash".into());
    spec.effort = Some(Effort::Medium);
    let events = run_to_end(&antigravity(), spec).await;
    assert!(texts(&events).is_empty(), "{events:?}");
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(
        diagnostic
            .as_deref()
            .is_some_and(|d| d.contains("rejected requested model")),
        "{events:?}"
    );
}

#[tokio::test]
async fn antigravity_clamps_to_an_offered_level_and_keeps_saved_variant_ids() {
    let clamped = antigravity_config_sets("gemini-3.1-pro", Some(Effort::Medium)).await;
    assert!(
        clamped.contains(&text("sets:model=gemini-pro-agent;")),
        "{clamped:?}"
    );
    let saved = antigravity_config_sets("gemini-3.7-flash-low", Some(Effort::High)).await;
    assert!(
        saved.contains(&text("sets:model=gemini-3.7-flash-low;")),
        "{saved:?}"
    );
}

#[tokio::test]
async fn antigravity_unknown_saved_model_fails_clearly() {
    let events = antigravity_config_sets("unknown-saved-model", None).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(
        diagnostic.unwrap().contains("unknown-saved-model"),
        "{events:?}"
    );
}

#[tokio::test]
async fn antigravity_empty_reply_completes_once() {
    let dir = workspace("empty-reply");
    let events = run_to_end(&antigravity(), spec_in("hi", &dir)).await;
    assert_completed(&events);
}

#[tokio::test]
async fn antigravity_strips_echoed_background_task_wakeups_from_the_reply() {
    let dir = workspace("echo");
    let events = run_to_end(&antigravity(), spec_in("echo-wakeup", &dir)).await;
    assert_eq!(
        texts(&events).join(""),
        "Waiting for the build.\n\n\n\nThe build finished."
    );
    assert_completed(&events);
}

#[tokio::test]
async fn antigravity_run_without_sign_in_says_it_is_not_signed_in() {
    for resume in [None, Some("expired-session")] {
        let dir = workspace(if resume.is_some() {
            "prompt-auth"
        } else {
            "needs-login"
        });
        let mut spec = spec_in("hi", &dir);
        spec.resume = resume.map(|s| SessionRef(s.into()));
        let events = run_to_end(&antigravity(), spec).await;
        let (stop, _, diagnostic) = done(&events);
        assert_eq!(stop, StopReason::Error, "{events:?}");
        assert!(
            diagnostic.unwrap_or_default().contains("isn't signed in"),
            "{events:?}"
        );
    }
}

#[tokio::test]
async fn antigravity_auth_prompt_after_cancel_emits_one_interrupted_done() {
    let events = run_acting(
        &antigravity(),
        ExecSpec {
            model: None,
            ..spec("auth-prompt-after-cancel")
        },
        |event, handle| {
            if *event == text("Ready to cancel") {
                handle.cancel();
            }
        },
    )
    .await;
    assert_eq!(dones(&events), 1, "{events:?}");
    assert_eq!(done(&events).0, StopReason::Interrupted, "{events:?}");
}

#[tokio::test]
async fn antigravity_wedge_emits_one_interrupted_done() {
    let executor = AcpExecutor::antigravity()
        .with_executable(fixture("fake-acp.sh"))
        .with_graces(Duration::from_millis(100), Duration::from_millis(200));
    let events = run_acting(
        &executor,
        spec_with("scenario:wedge", AccessMode::FullAccess),
        |event, handle| {
            if *event == text("working") {
                handle.cancel();
            }
        },
    )
    .await;
    assert_eq!(dones(&events), 1, "{events:?}");
    assert_eq!(done(&events).0, StopReason::Interrupted);
}

/// A sign-in link the server prints, on either stream, ends the run with a "not signed in"
/// diagnostic instead of waiting out the handshake; so does one printed mid-turn. The server's
/// sibling environment reaches every spawn.
#[tokio::test]
async fn antigravity_stale_login_ends_the_run_with_a_sign_in_diagnostic() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let source = fixture("fake-antigravity-acp.sh");
    let wrap = |name: &str, env: &str| {
        let server = dir.path().join(name);
        std::fs::write(
            &server,
            format!("#!/bin/sh\n{env} exec '{}' \"$@\"\n", source.display()),
        )
        .unwrap();
        std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
        AcpExecutor::antigravity().with_executable(server)
    };
    let prompted = dir.path().join("prompted");
    for (name, env) in [
        (
            "stale-stderr",
            format!("FAKE_AGY_STALE_LOGIN='{}'", prompted.display()),
        ),
        (
            "stale-stdout",
            format!(
                "FAKE_AGY_STALE_LOGIN='{}' FAKE_AGY_STALE_LOGIN_STREAM=stdout",
                prompted.display()
            ),
        ),
        ("stale-turn", "FAKE_AGY_STALE_TURN=1".to_owned()),
    ] {
        let executor = wrap(name, &env);
        let events = tokio::time::timeout(
            Duration::from_secs(10),
            run_to_end(
                &executor,
                ExecSpec {
                    model: None,
                    ..spec("hi")
                },
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("{name}: the run waited out the handshake"));
        assert_eq!(dones(&events), 1, "{name}: {events:?}");
        assert_eq!(done(&events).0, StopReason::Error, "{name}");
        assert!(
            diagnostic(&events).contains("isn't signed in"),
            "{name}: {events:?}"
        );
    }
    // Each stale-login launch ran the server once: no retry relaunched it into another prompt.
    assert_eq!(std::fs::read_to_string(&prompted).unwrap(), "xx");
}

/// The server's sibling harness binary and `PYTHONUNBUFFERED` reach the launch, and a sign-in
/// link on stdout is noticed.
#[tokio::test]
async fn antigravity_launch_environment_includes_the_sibling_harness() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let server = dir.path().join("server");
    let sibling = dir.path().join("localharness_external");
    std::fs::write(&sibling, "fixture").unwrap();
    let original = std::fs::read_to_string(fixture("fake-antigravity-acp.sh")).unwrap();
    let checks = format!(
        "[ \"$ANTIGRAVITY_HARNESS_PATH\" = '{}' ] || exit 3\n[ \"$PYTHONUNBUFFERED\" = 1 ] || exit 4\n",
        sibling.display(),
    );
    std::fs::write(
        &server,
        original.replacen("#!/bin/sh\n", &format!("#!/bin/sh\n{checks}"), 1),
    )
    .unwrap();
    std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
    let executor = AcpExecutor::antigravity().with_executable(&server);
    let events = run_to_end(
        &executor,
        ExecSpec {
            model: None,
            cwd: dir.path().to_path_buf(),
            ..spec("hello")
        },
    )
    .await;
    assert_completed(&events);
}

// ---------------------------------------------------------------------------
// Lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn interrupt_sends_session_cancel_and_ends_interrupted() {
    let events = run_acting(&grok(), spec("scenario:interrupt"), |event, handle| {
        if *event == text("working") {
            handle.cancel();
        }
    })
    .await;
    assert_eq!(dones(&events), 1);
    assert_eq!(done(&events), (StopReason::Interrupted, None, None));
}

#[tokio::test]
async fn wedged_agent_escalates_to_signals_and_still_ends_interrupted() {
    let executor = grok().with_graces(Duration::from_millis(100), Duration::from_millis(200));
    let events = run_acting(&executor, spec("scenario:wedge"), |event, handle| {
        if *event == text("working") {
            handle.cancel();
        }
    })
    .await;
    assert_eq!(dones(&events), 1, "{events:?}");
    assert_eq!(done(&events).0, StopReason::Interrupted);
}

#[tokio::test]
async fn refusal_maps_to_a_refusal_stop_with_a_diagnostic() {
    let events = run_to_end(&grok(), spec("scenario:refusal")).await;
    let (stop, result, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Refusal);
    assert_eq!(result, None);
    assert!(diagnostic.unwrap().contains("refused"));
}

#[tokio::test]
async fn resume_loads_the_session_and_drops_replayed_history() {
    let mut spec = spec("scenario:resumed");
    spec.resume = Some(SessionRef("s-loaded".into()));
    let events = run_to_end(&grok(), spec).await;
    // The 600-update replay is drained without surfacing...
    assert!(
        !texts(&events).iter().any(|t| t.contains("old reply")),
        "{events:?}"
    );
    // ...the loaded session id sticks, and the live turn still streams.
    assert!(matches!(
        &events[0],
        ExecEvent::Started { session, .. } if session.0 == "s-loaded"
    ));
    assert!(events.contains(&text("back again")));
    assert_completed(&events);
}

/// A load that outlasts its budget, is refused for a reason other than a missing session, or
/// loses the agent errors the run and keeps the session: no session/new, no `Started`.
#[tokio::test]
async fn an_unfinished_restore_errors_and_keeps_the_session() {
    for (resume, says) in [
        ("load-hang", Some("did not finish within")),
        ("load-busy", Some("session store is locked")),
        ("load-exit", None),
    ] {
        let mut spec = spec("scenario:echo-prompt");
        spec.resume = Some(SessionRef(resume.into()));
        let executor = grok().with_session_load_timeout(Duration::from_millis(300));
        let events = run_to_end(&executor, spec).await;
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, ExecEvent::Started { .. })),
            "{resume}: {events:?}"
        );
        assert_eq!(dones(&events), 1, "{resume}: {events:?}");
        let (stop, _, diagnostic) = done(&events);
        assert_eq!(stop, StopReason::Error, "{resume}");
        if let Some(says) = says {
            let diagnostic = diagnostic.unwrap_or_default();
            assert!(
                diagnostic.contains(&format!("could not restore session {resume}"))
                    && diagnostic.contains(says),
                "{resume}: {diagnostic}"
            );
        }
    }
}

/// An agent that no longer has the session gets a fresh one; `Started` names it.
#[tokio::test]
async fn an_unknown_session_starts_fresh_and_started_names_the_new_one() {
    let mut spec = spec("scenario:echo-prompt");
    spec.resume = Some(SessionRef("load-fail".into()));
    let events = run_to_end(&grok(), spec).await;
    assert!(matches!(
        &events[0],
        ExecEvent::Started { session, .. } if session.0 == "s-fresh"
    ));
    assert_completed(&events);
}

#[tokio::test]
async fn hung_handshake_errors_instead_of_waiting_forever() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let hang = dir.path().join("hang");
    std::fs::write(&hang, "#!/bin/sh\nexec sleep 30\n").unwrap();
    std::fs::set_permissions(&hang, std::fs::Permissions::from_mode(0o755)).unwrap();
    let executor = AcpExecutor::grok()
        .with_executable(&hang)
        .with_handshake_timeout(Duration::from_millis(400));
    let started = std::time::Instant::now();
    let events = run_to_end(&executor, spec("hello")).await;
    assert!(started.elapsed() < Duration::from_secs(10));
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(
        diagnostic
            .unwrap()
            .contains("did not complete the ACP handshake"),
        "{events:?}"
    );
}

#[tokio::test]
async fn a_missing_binary_is_not_installed_and_nothing_is_started() {
    let executor = AcpExecutor::grok().with_executable("/nonexistent/grok-nowhere");
    assert!(!executor.installed());
    let (controls, _handle) = ExecControls::new(8);
    let error = executor
        .start(spec("hello"), controls)
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
    let events = run_to_end(&grok(), spec).await;
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

/// ARCH-15: an access mode the agent does not honor is refused before anything is resolved or
/// spawned: not a silent upgrade to full access, and not `NotInstalled` for
/// a binary that is missing too.
#[tokio::test]
async fn an_unsupported_access_mode_fails_before_anything_is_spawned() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("spawned");
    let script = dir.path().join("grok");
    std::fs::write(
        &script,
        format!("#!/bin/sh\ntouch '{}'\nexit 1\n", marker.display()),
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    for executor in [
        AcpExecutor::grok().with_executable(&script),
        AcpExecutor::devin().with_executable(&script),
        AcpExecutor::hermes().with_executable(&script),
        // And not even a binary that is not there.
        AcpExecutor::grok().with_executable("/nonexistent/grok-nowhere"),
    ] {
        let name = executor.descriptor().id;
        for access in [AccessMode::Supervised, AccessMode::AutoAcceptEdits] {
            let (controls, _handle) = ExecControls::new(8);
            let error = executor
                .start(spec_with("hello", access), controls)
                .await
                .err()
                .expect("refused");
            match error {
                ExecError::UnsupportedAccess {
                    requested,
                    supported,
                } => {
                    assert_eq!(requested, access, "{name}");
                    assert_eq!(supported, vec![AccessMode::FullAccess], "{name}");
                }
                other => panic!("{name}: {other:?}"),
            }
        }
    }
    // Antigravity honors Supervised; a narrowed instance refuses what it narrowed away.
    let narrowed = AcpExecutor::antigravity()
        .with_executable(&script)
        .with_access_modes(vec![AccessMode::Supervised]);
    assert_eq!(
        narrowed.descriptor().caps.access_modes,
        vec![AccessMode::Supervised]
    );
    let (controls, _handle) = ExecControls::new(8);
    assert!(matches!(
        narrowed
            .start(spec_with("hello", AccessMode::FullAccess), controls)
            .await
            .err()
            .expect("refused"),
        ExecError::UnsupportedAccess { .. }
    ));
    // An agent's own table is a ceiling: narrowing never widens it.
    let widened =
        AcpExecutor::grok().with_access_modes(vec![AccessMode::Supervised, AccessMode::FullAccess]);
    assert_eq!(
        widened.descriptor().caps.access_modes,
        vec![AccessMode::FullAccess]
    );
    assert!(!marker.exists(), "an agent was started");
}

#[tokio::test]
async fn structured_output_is_refused_before_anything_is_spawned() {
    let (controls, _handle) = ExecControls::new(8);
    let mut spec = spec("hello");
    spec.output_schema = Some(serde_json::json!({"type": "object"}));
    let error = grok().start(spec, controls).await.err().expect("refused");
    assert!(
        matches!(error, ExecError::UnsupportedOutputSchema),
        "{error:?}"
    );
}

#[tokio::test]
async fn shutdown_waits_for_the_tasks_of_runs_whose_streams_were_dropped() {
    let executor = grok().with_graces(Duration::from_millis(100), Duration::from_millis(500));
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

// ---------------------------------------------------------------------------
// Process records
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_spawned_agents_process_record_is_forwarded() {
    let mut live = start(&grok(), spec("scenario:happy")).await;
    let record = tokio::time::timeout(Duration::from_secs(10), live.records.recv())
        .await
        .expect("a record in time")
        .expect("the agent was reported");
    assert!(record.pid > 0);
    // The agent is spawned as its own group leader.
    assert_eq!(record.pgid, record.pid);
    assert!(record.start_time > 0);
    let _ = drive(&mut live, |_| Decision::Allow).await;
    // One child, one record: nothing else a run spawns goes unreported or twice.
    assert!(live.records.try_recv().is_err());
}

// ---------------------------------------------------------------------------
// Steering
// ---------------------------------------------------------------------------

fn steer_on(trigger: &'static str) -> impl FnMut(&ExecEvent, &ExecHandle) {
    let mut sent = false;
    move |event, handle| {
        if *event == text(trigger) && !std::mem::replace(&mut sent, true) {
            handle.try_steer("redirect please").unwrap();
        }
    }
}

#[tokio::test]
async fn steering_extension_injects_mid_turn() {
    let events = run_acting(&grok(), spec("scenario:steer-ext"), steer_on("first")).await;
    assert!(events.contains(&text("steered")), "{events:?}");
    assert_completed(&events);
}

/// The steering response racing the turn's own end: the injection landed in the dying turn and
/// the prompt response reached the wire first. One run, one `Done`, and nothing after it.
#[tokio::test]
async fn steer_racing_the_turn_end_ends_the_run_once() {
    let events = run_acting(&grok(), spec("scenario:steer-race"), steer_on("first")).await;
    assert_completed(&events);
    assert!(events.contains(&text("steered")), "{events:?}");
}

/// A steer the agent rejects waits for the turn boundary and is delivered as the next prompt of
/// the same run: no `Done` for the turn it continues, one at the very end.
#[tokio::test]
async fn rejected_steer_queues_and_delivers_at_the_turn_boundary() {
    let events = run_acting(&grok(), spec("scenario:steer-queue"), steer_on("first")).await;
    assert!(events.contains(&text("boundary")), "{events:?}");
    assert_completed(&events);
    let boundary = events.iter().position(|e| *e == text("boundary")).unwrap();
    assert!(boundary < events.len() - 1, "the Done comes last");
}

#[tokio::test]
async fn a_steer_that_loses_the_race_to_a_dead_turn_is_promoted_to_a_fresh_prompt() {
    let events = run_acting(&grok(), spec("scenario:starve"), {
        let mut sent = false;
        move |event, handle| {
            if *event == text("working") && !std::mem::replace(&mut sent, true) {
                handle.try_steer("what about now").unwrap();
            }
        }
    })
    .await;
    assert!(events.contains(&text("promoted")), "{events:?}");
    assert_completed(&events);
}

/// An agent without the extension queues a live steer once; a step-boundary agent (Devin)
/// continues the run with it and reports one `Done`.
#[tokio::test]
async fn without_the_steering_extension_a_live_steer_is_delivered_once() {
    let executor = AcpExecutor::devin().with_executable(fixture("fake-robust-acp.py"));
    let events = run_acting(
        &executor,
        ExecSpec {
            model: None,
            cwd: std::env::temp_dir(),
            ..spec("steer-live")
        },
        |event, handle| {
            if *event == text("first") {
                let _ = handle.try_steer("second");
            }
        },
    )
    .await;
    assert_eq!(texts(&events).iter().filter(|t| **t == "second").count(), 1);
    assert_completed(&events);
}

// ---------------------------------------------------------------------------
// Prompt completion and the stall watchdog
// ---------------------------------------------------------------------------

#[tokio::test]
async fn prompt_complete_extension_settles_a_hung_prompt_response() {
    // The grok field hang: `_x.ai/session/prompt_complete` fires (echoing the minted
    // _meta.promptId) but the session/prompt RPC never answers.
    let events = run_to_end(&grok(), spec("scenario:prompt-complete-hang")).await;
    assert!(events.contains(&text("pong")), "{events:?}");
    assert_completed(&events);
}

#[tokio::test]
async fn stale_prompt_complete_never_settles_a_newer_turn() {
    let events = run_to_end(&grok(), spec("scenario:prompt-complete-stale")).await;
    // Exactly one Done, AFTER the real content: the stale and foreign completions (emitted
    // before the 1s pause) must not have settled first, and must not have marked the turn
    // interrupted.
    let real = events
        .iter()
        .position(|e| *e == text("real answer"))
        .unwrap();
    assert_eq!(real, events.len() - 3, "{events:?}");
    assert_completed(&events);
    // Grok-style `_meta` usage on the response is captured.
    assert!(events.contains(&ExecEvent::Usage {
        input_tokens: 9,
        output_tokens: 4,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
    }));
}

// ---------------------------------------------------------------------------
// Specs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn devin_spec_drives_the_shared_acp_wire() {
    let devin = AcpExecutor::devin().with_executable(fixture("fake-acp.sh"));
    let events = run_to_end(&devin, spec("scenario:happy")).await;
    assert!(events.contains(&text("Hello")));
    assert_completed(&events);
}

#[tokio::test]
async fn fx_selects_models_and_enforces_policy_on_new_and_loaded_sessions() {
    let fx = AcpExecutor::fx().with_executable(fixture("fake-fx.py"));
    for resume in [None, Some(SessionRef("fx-session".into()))] {
        let mut spec = spec_with("hello", AccessMode::Supervised);
        spec.model = Some("model-b".into());
        spec.resume = resume;
        let events = run_to_end(&fx, spec).await;
        assert_completed(&events);
    }
}

#[tokio::test]
async fn fx_rejected_model_never_falls_back_to_another_model() {
    let fx = AcpExecutor::fx().with_executable(fixture("fake-fx.py"));
    let mut spec = spec("hello");
    spec.model = Some("rejected-model".into());
    let events = run_to_end(&fx, spec).await;
    assert_eq!(done(&events).0, StopReason::Error, "{events:?}");
}

#[tokio::test]
async fn fx_permissions_follow_reno_access_mode() {
    let fx = AcpExecutor::fx().with_executable(fixture("fake-fx.py"));
    for (access, decision, expected, asked) in [
        (AccessMode::Supervised, Decision::Deny, "deny", 1),
        (AccessMode::Supervised, Decision::Allow, "allow", 1),
        // Full access asks too (the kernel answers at once; a refusal holds).
        (AccessMode::FullAccess, Decision::Deny, "deny", 1),
        (AccessMode::FullAccess, Decision::Allow, "allow", 1),
    ] {
        let mut spec = spec_with("permission", access);
        spec.model = Some("model-b".into());
        let mut live = start(&fx, spec).await;
        let outcome = drive(&mut live, |_| decision).await;
        assert_eq!(outcome.approvals.len(), asked, "{access:?}");
        if asked == 1 {
            assert_eq!(outcome.approvals[0].action, ActionClass::Edit);
        }
        assert!(texts(&outcome.events).contains(&expected), "{access:?}");
    }
}

// ---------------------------------------------------------------------------
// MCP injection
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mcp_servers_from_the_spec_reach_session_new_for_every_spec_new_resumed_and_fallback() {
    for executor in [
        AcpExecutor::grok(),
        AcpExecutor::devin(),
        AcpExecutor::hermes(),
        AcpExecutor::antigravity(),
    ] {
        let executor = executor.with_executable(fixture("fake-acp.sh"));
        for resume in [None, Some("mcp-loaded"), Some("load-fail")] {
            let mut spec = spec("scenario:mcp");
            spec.model = None;
            spec.resume = resume.map(|s| SessionRef(s.into()));
            spec.mcp = vec![McpServerSpec {
                name: "reno".into(),
                command: "/path with spaces/reno".into(),
                args: vec!["mcp".into()],
                env: [
                    ("RENO_CHAT_ID".to_owned(), "origin-chat".to_owned()),
                    ("RENO_IPC_PORT".to_owned(), "27699".to_owned()),
                ]
                .into(),
            }];
            let events = run_to_end(&executor, spec).await;
            let id = executor.descriptor().id;
            assert!(
                events.contains(&text("mcp configured")),
                "{id} {resume:?}: {events:?}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Process lifecycle against the lifecycle and robust fixtures
// ---------------------------------------------------------------------------

fn lifecycle() -> AcpExecutor {
    AcpExecutor::hermes()
        .with_executable(fixture("fake-lifecycle-acp.sh"))
        .with_graces(Duration::from_millis(100), Duration::from_millis(150))
}

fn lifecycle_spec(prompt: &str) -> ExecSpec {
    ExecSpec {
        model: None,
        ..spec(prompt)
    }
}

#[tokio::test]
async fn acp_crash_reports_status_and_stderr_once() {
    for (prompt, status, tail) in [
        ("crash", "exit code 23", "last stderr context"),
        ("signal-crash", "signal 9", "signal context"),
        (
            "inherited-pipe-crash",
            "exit code 25",
            "inherited pipe context",
        ),
    ] {
        let events = run_to_end(&lifecycle(), lifecycle_spec(prompt)).await;
        assert_eq!(dones(&events), 1, "{prompt}");
        let (stop, _, diagnostic) = done(&events);
        assert_eq!(stop, StopReason::Error);
        let diagnostic = diagnostic.unwrap();
        assert!(
            diagnostic.contains(status) && diagnostic.contains(tail),
            "{diagnostic}"
        );
    }
}

#[tokio::test]
async fn acp_idle_crash_then_load_preserves_session() {
    let events = run_to_end(&lifecycle(), lifecycle_spec("idle-crash")).await;
    assert_completed(&events);
    let ExecEvent::Started { session, .. } = &events[0] else {
        panic!("no Started: {events:?}")
    };
    let mut spec = lifecycle_spec("resumed");
    spec.resume = Some(session.clone());
    let events = run_to_end(&lifecycle(), spec).await;
    assert_completed(&events);
    assert!(events.contains(&text("reply:resumed")));
}

#[tokio::test]
async fn acp_failed_load_starts_a_fresh_session() {
    let mut spec = lifecycle_spec("fresh");
    spec.resume = Some(SessionRef("missing".into()));
    let events = run_to_end(&lifecycle(), spec).await;
    assert_completed(&events);
}

#[tokio::test]
async fn acp_frames_and_model_effort_round_trip() {
    let mut spec = lifecycle_spec("frames");
    spec.model = Some("mock/model".into());
    spec.effort = Some(Effort::High);
    let events = run_to_end(&lifecycle(), spec).await;
    assert!(
        events
            .iter()
            .any(|e| matches!(e, ExecEvent::Text { text } if text.len() == 1024 * 1024 + 17))
    );
    assert_completed(&events);
}

#[tokio::test]
async fn acp_error_stop_reason_is_failed() {
    let events = run_to_end(&lifecycle(), lifecycle_spec("error")).await;
    assert_eq!(done(&events).0, StopReason::Error);
}

#[tokio::test]
async fn acp_interrupt_error_and_duplicate_terminal_settle_once() {
    let events = run_acting(
        &lifecycle(),
        lifecycle_spec("interrupt-error"),
        |event, handle| {
            if *event == text("working") {
                handle.cancel();
            }
        },
    )
    .await;
    assert_eq!(dones(&events), 1);
    assert_eq!(done(&events), (StopReason::Interrupted, None, None));
}

fn alive_unless_zombie(pid: i32) -> bool {
    // A zombie awaiting the host reaper is dead; no tool may remain running.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
    !(stat.is_empty() || stat.split_whitespace().nth(2) == Some("Z"))
}

#[tokio::test]
async fn acp_interrupt_kills_tool_process_group() {
    let mut tree_pids = Vec::new();
    let events = run_acting(&lifecycle(), lifecycle_spec("tree"), |event, handle| {
        if let ExecEvent::Text { text } = event
            && let Some(pid) = text.strip_prefix("tree:")
        {
            tree_pids.push(pid.parse::<i32>().unwrap());
            handle.cancel();
        }
    })
    .await;
    assert_eq!(done(&events).0, StopReason::Interrupted);
    assert_eq!(tree_pids.len(), 2);
    for pid in tree_pids {
        assert!(!alive_unless_zombie(pid), "{pid} still runs");
    }
}

/// A model the agent advertises but refuses to switch to fails the run before it prompts: the
/// agent's default is never run in its place.
#[tokio::test]
async fn a_rejected_model_switch_fails_the_run_and_never_prompts() {
    let mut spec = lifecycle_spec("config-rejected");
    spec.model = Some("mock/reject".into());
    let events = run_to_end(&lifecycle(), spec).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error, "{events:?}");
    assert!(
        diagnostic
            .unwrap()
            .contains("rejected requested model mock/reject"),
        "{events:?}"
    );
    assert!(texts(&events).is_empty(), "{events:?}");
}

/// A model the session does not advertise at all fails the same way, for an agent that had no
/// strictness of its own before.
#[tokio::test]
async fn an_unadvertised_model_fails_the_run_for_every_agent() {
    let mut spec = lifecycle_spec("hello");
    spec.model = Some("mock/nope".into());
    let events = run_to_end(&lifecycle(), spec).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error, "{events:?}");
    assert!(
        diagnostic
            .unwrap()
            .contains("does not advertise requested model mock/nope"),
        "{events:?}"
    );
}

#[tokio::test]
async fn acp_dropping_stream_terminates_tool_tree() {
    let mut live = start(&lifecycle(), lifecycle_spec("tree")).await;
    let mut pids = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while pids.len() < 2 {
            if let Some(ExecEvent::Text { text }) = live.stream.next().await
                && let Some(pid) = text.strip_prefix("tree:")
            {
                pids.push(pid.parse::<i32>().unwrap());
            }
        }
    })
    .await
    .unwrap();
    drop(live.stream);
    tokio::time::timeout(Duration::from_secs(5), async {
        while pids.iter().any(|pid| alive_unless_zombie(*pid)) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("consumer shutdown must terminate the tool tree");
}

fn robust() -> AcpExecutor {
    AcpExecutor::grok().with_executable(fixture("fake-robust-acp.py"))
}

fn robust_spec(prompt: &str) -> ExecSpec {
    ExecSpec {
        model: None,
        cwd: std::env::temp_dir(),
        ..spec(prompt)
    }
}

#[tokio::test]
async fn noise_and_large_crlf_frame_preserve_the_complete_turn() {
    let events = run_to_end(&robust(), robust_spec("frames")).await;
    assert_eq!(texts(&events).join(""), "x".repeat(2 * 1024 * 1024));
    assert_completed(&events);
}

#[tokio::test]
async fn queued_updates_are_drained_before_completion_under_backpressure() {
    let mut live = start(&robust(), robust_spec("burst")).await;
    // Fill both bounded queues before allowing the consumer to progress.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut text = String::new();
    let mut events = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = live.stream.next().await {
            if let ExecEvent::Text { text: chunk } = &event {
                assert!(
                    !events.iter().any(|e| matches!(e, ExecEvent::Done { .. })),
                    "text after Done"
                );
                text.push_str(chunk);
            }
            events.push(event);
        }
    })
    .await
    .unwrap();
    assert_eq!(text, (0..300).map(|i| format!("{i},")).collect::<String>());
    assert_completed(&events);
}

#[tokio::test]
async fn foreign_notifications_and_permissions_cannot_affect_parent_turn() {
    let events = run_to_end(&robust(), robust_spec("foreign")).await;
    assert_eq!(texts(&events).join(""), "foreign permission rejected");
    assert_completed(&events);
}

#[tokio::test]
async fn cancel_watchdog_ignores_late_settlement_for_all_acp_specs() {
    for executor in [
        AcpExecutor::grok(),
        AcpExecutor::devin(),
        AcpExecutor::antigravity(),
    ] {
        let executor = executor
            .with_executable(fixture("fake-robust-acp.py"))
            .with_graces(Duration::from_millis(50), Duration::from_millis(50));
        for scenario in ["wedge", "late-settle"] {
            let events = run_acting(&executor, robust_spec(scenario), |event, handle| {
                if *event == text("ready") {
                    handle.cancel();
                }
            })
            .await;
            assert!(
                !events.iter().any(|e| matches!(e, ExecEvent::Usage { .. })),
                "late usage in {scenario}"
            );
            assert_eq!(dones(&events), 1, "{scenario}");
            assert_eq!(done(&events).0, StopReason::Interrupted, "{scenario}");
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn dropping_a_stream_reaps_the_agent() {
    let executor = robust().with_graces(Duration::from_millis(50), Duration::from_millis(50));
    let mut live = start(&executor, robust_spec("idle-pid")).await;
    let mut pid = None;
    tokio::time::timeout(Duration::from_secs(5), async {
        while let Some(event) = live.stream.next().await {
            match event {
                ExecEvent::Text { text } => pid = Some(text.parse::<u32>().unwrap()),
                ExecEvent::Done { .. } => break,
                _ => {}
            }
        }
    })
    .await
    .unwrap();
    let path = PathBuf::from(format!("/proc/{}", pid.unwrap()));
    drop(live.stream);
    tokio::time::timeout(Duration::from_secs(2), async {
        while path.exists() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("a dropped consumer reaps the child");
}

// ---------------------------------------------------------------------------
// Descriptors
// ---------------------------------------------------------------------------

#[test]
fn every_agent_declares_its_own_id_and_what_it_honors() {
    use AccessMode::{AutoAcceptEdits, FullAccess, Supervised};
    let registry_agent =
        AcpExecutor::registry(RegistryAgent::new("gemini").unwrap(), "Gemini").unwrap();
    // (executor, id, steering, access modes)
    for (executor, id, steering, access) in [
        (
            AcpExecutor::grok(),
            "acp.grok",
            Steering::StepBoundary,
            vec![FullAccess],
        ),
        (
            AcpExecutor::devin(),
            "acp.devin",
            Steering::StepBoundary,
            vec![FullAccess],
        ),
        (
            AcpExecutor::hermes(),
            "acp.hermes",
            Steering::TurnBoundary,
            vec![FullAccess],
        ),
        (
            AcpExecutor::fx(),
            "acp.fx",
            Steering::TurnBoundary,
            vec![Supervised, FullAccess],
        ),
        (
            AcpExecutor::antigravity(),
            "acp.antigravity",
            Steering::TurnBoundary,
            vec![Supervised, AutoAcceptEdits, FullAccess],
        ),
        (
            registry_agent,
            "acp.gemini",
            Steering::TurnBoundary,
            vec![Supervised, FullAccess],
        ),
    ] {
        let descriptor = executor.descriptor();
        assert_eq!(descriptor.id, id.parse::<ExecutorId>().unwrap());
        assert_eq!(descriptor.contract_version, 1);
        let caps = descriptor.caps;
        assert_eq!(caps.steering, steering, "{id}");
        assert_eq!(caps.access_modes, access, "{id}");
        assert!(caps.resume, "{id}");
        assert!(caps.mcp_injection, "{id}");
        assert!(caps.deterministic_turn_end, "{id}");
        // What a caller cannot request is not declared (ARCH-15): no schema, no plan mode,
        // and no sandbox Reno could set on an agent's own: only "no sandbox" is honoured.
        assert!(!caps.structured_output, "{id}");
        assert!(!caps.plan_mode, "{id}");
        assert_eq!(
            caps.sandbox_levels,
            vec![SandboxLevel::Unrestricted],
            "{id}"
        );
    }
}

#[test]
fn cli_verified_is_what_the_driver_pins_and_says_so_where_it_pins_nothing() {
    let verified = |executor: AcpExecutor| {
        let v = executor.descriptor().cli_verified;
        (v.min, v.verified)
    };
    assert_eq!(
        verified(AcpExecutor::grok()),
        ("1.0.4".into(), "1.0.46".into())
    );
    assert_eq!(
        verified(AcpExecutor::antigravity()),
        ("1.1.1".into(), "1.2.1".into())
    );
    for executor in [
        AcpExecutor::devin(),
        AcpExecutor::hermes(),
        AcpExecutor::fx(),
    ] {
        assert_eq!(verified(executor), ("0".into(), "unverified".into()));
    }
}

#[test]
fn the_models_an_agent_is_known_to_offer_are_requestable_ids() {
    let models = |executor: AcpExecutor| -> Vec<String> {
        executor
            .descriptor()
            .caps
            .models
            .into_iter()
            .map(|m| m.id)
            .collect()
    };
    assert_eq!(models(AcpExecutor::grok()), ["grok-4.5"]);
    assert_eq!(
        models(AcpExecutor::antigravity()),
        ["gemini-3.7-flash", "gemini-3.1-pro"]
    );
    // Nothing pins what these sessions advertise, so nothing is declared.
    for executor in [
        AcpExecutor::devin(),
        AcpExecutor::hermes(),
        AcpExecutor::fx(),
    ] {
        assert!(models(executor).is_empty());
    }
}

#[tokio::test]
async fn probe_reports_the_installed_agent_and_nothing_it_cannot_know() {
    let health = grok().probe().await;
    assert!(matches!(health.installed, InstallStatus::Installed { .. }));
    // Grok offers no login status the driver can read without a run.
    assert_eq!(health.signed_in, SignedIn::Unknown);
    // No conformance run covers it, so it is never "ok".
    assert!(matches!(
        health.version_gate,
        VersionGate::Untested { .. } | VersionGate::Unchecked | VersionGate::Ok
    ));
    assert_eq!(health.models.source, ModelSource::Curated);
    assert_eq!(health.ineligible(), None);
    let missing = AcpExecutor::grok()
        .with_executable("/nonexistent/grok-nowhere")
        .probe()
        .await;
    assert!(matches!(
        missing.installed,
        InstallStatus::NotInstalled {
            why: NotInstalledReason::Unusable { .. }
        }
    ));
    assert!(matches!(
        missing.ineligible(),
        Some(Ineligible::NotInstalled(_))
    ));
}

// ---------------------------------------------------------------------------
// Detection: a missing server is never installed by a run
// ---------------------------------------------------------------------------

/// Whether Antigravity's server is found, in the six states a machine can be in, and that a
/// missing or partly installed one is `NotInstalled` for a run and installs nothing. Each state
/// runs in a child process of this test binary, because it sets HOME, PATH and the adapters
/// directory.
#[test]
fn antigravity_detection_and_missing_server_never_install() {
    use std::os::unix::fs::PermissionsExt;
    for scenario in [
        "cli-only",
        "partial",
        "server",
        "par",
        "override",
        "invalid-override",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let name = match scenario {
            "server" => "agy_acp_server",
            "par" => "agy_acp_server.par",
            "override" => "custom-server",
            _ => "agy",
        };
        let exe = bin.join(name);
        std::fs::write(&exe, "#!/bin/sh\nexit 0\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let adapters = dir.path().join("adapters");
        if scenario == "partial" {
            let partial = adapters.join("antigravity-acp/1.2.1");
            std::fs::create_dir_all(&partial).unwrap();
            std::fs::write(partial.join("agy_acp_server.par"), "incomplete").unwrap();
        }
        let mut child = std::process::Command::new(std::env::current_exe().unwrap());
        child
            .args(["--exact", "antigravity_detection_subprocess", "--nocapture"])
            .env("HOME", dir.path())
            .env("PATH", &bin)
            .env("SHELL", "/nonexistent-shell")
            .env("RENO_ADAPTERS_DIR", &adapters)
            .env("RENO_TEST_DETECTION", scenario)
            .env_remove("ANTIGRAVITY_ACP_EXECUTABLE");
        if scenario == "override" {
            child.env("ANTIGRAVITY_ACP_EXECUTABLE", &exe);
        }
        if scenario == "invalid-override" {
            child.env("ANTIGRAVITY_ACP_EXECUTABLE", bin.join("missing"));
        }
        let output = child.output().unwrap();
        assert!(
            output.status.success(),
            "{scenario}: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!adapters.join(".tmp-antigravity-acp-1.2.1").exists());
    }
}

#[tokio::test]
async fn antigravity_detection_subprocess() {
    let Ok(scenario) = std::env::var("RENO_TEST_DETECTION") else {
        return;
    };
    let executor = AcpExecutor::antigravity();
    let installed = matches!(scenario.as_str(), "server" | "par" | "override");
    assert_eq!(executor.installed(), installed);
    let probed = executor.probe().await;
    assert_eq!(
        matches!(probed.installed, InstallStatus::Installed { .. }),
        installed,
        "{probed:?}"
    );
    if !installed {
        // A pinned download that was never fetched is reported as needing the explicit install,
        // never fetched by the probe; a bad override is just unusable.
        let expected = match scenario.as_str() {
            "invalid-override" => matches!(
                probed.installed,
                InstallStatus::NotInstalled {
                    why: NotInstalledReason::Unusable { .. }
                }
            ),
            _ => matches!(
                probed.installed,
                InstallStatus::NotInstalled {
                    why: NotInstalledReason::NeedsInstall { .. }
                        | NotInstalledReason::NotFound { .. }
                }
            ),
        };
        assert!(expected, "{probed:?}");
    }
    if installed {
        assert!(executor.launch_program().is_ok());
        return;
    }
    assert!(executor.launch_program().is_err());
    let (controls, _handle) = ExecControls::new(8);
    let error = executor
        .start(spec("hello"), controls)
        .await
        .err()
        .expect("nothing to start");
    assert!(matches!(error, ExecError::NotInstalled(_)), "{error:?}");
    let adapters = PathBuf::from(std::env::var_os("RENO_ADAPTERS_DIR").unwrap());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        std::fs::read_dir(adapters)
            .map(|entries| entries
                .map(Result::unwrap)
                .all(|entry| entry.file_name() == "antigravity-acp"))
            .unwrap_or(true)
    );
}
