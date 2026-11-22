#![cfg(test)]

//! Approvals through the kernel: policy decides, humans decide, deadlines expire, records last.
//!
//! Time is paused and the store is a tempdir. Wall time (`Clock`) and `tokio` time are separate
//! here as in the daemon: a deadline fires when paused time reaches it, and a record's
//! timestamps follow the `ManualClock`, so a test that checks one moves the other too.

use std::sync::Arc;
use std::time::Duration;

use reno_harness::exec::{Answer, Decision, MockExecutor, MockStep};
use reno_proto::defs::{DefFile, DefKind};
use reno_proto::{
    AccessMode, ActionClass, Approval, ApprovalDecision, ApprovalOutcome, ApprovalRecord,
    ApprovalStatus, ApprovalSubject, ExecEvent, ResolvedBy, RunId, RunStatus, SandboxLevel, Seq,
    StopReason,
};
use reno_store::Store;
use tokio::time::advance;

use super::defs::{FakePort, file, setup};
use super::{Fx, config, id, mock, registry, until};
use crate::{ExecutorRegistry, KernelConfig, KernelError, WakeSignal};

const WORKTREE: &str = "/work/tree";

pub(super) fn command(line: &str) -> ApprovalSubject {
    ApprovalSubject {
        action: ActionClass::Command,
        tool: "bash".into(),
        command: Some(line.into()),
        paths: Vec::new(),
        raw: serde_json::Value::Null,
    }
}

pub(super) fn edit(path: &str) -> ApprovalSubject {
    ApprovalSubject {
        action: ActionClass::Edit,
        tool: "write".into(),
        command: None,
        paths: vec![path.into()],
        raw: serde_json::Value::Null,
    }
}

pub(super) fn asks(request: &str, subject: ApprovalSubject) -> MockStep {
    MockStep::Emit(ExecEvent::NeedsApproval {
        request_id: request.into(),
        subject,
    })
}

pub(super) fn awaits(request: &str) -> MockStep {
    MockStep::AwaitAnswer {
        request_id: request.into(),
    }
}

/// `asks` then `awaits`, for each `(request, subject)`, then one last text.
pub(super) fn script(requests: Vec<(&str, ApprovalSubject)>) -> Vec<MockStep> {
    let mut steps = vec![MockStep::started("s", "mock-1")];
    for (request, subject) in requests {
        steps.push(asks(request, subject));
        steps.push(awaits(request));
    }
    steps.push(MockStep::text("after"));
    steps
}

pub(super) fn bot(name: &str, extra: &str) -> DefFile {
    file(
        DefKind::Bot,
        name,
        &format!("name = \"{name}\"\ninstructions = \"work\"\nexecutors = [\"mock\"]\n{extra}\n"),
    )
}

pub(super) fn policy(text: &str) -> DefFile {
    file(DefKind::Policy, "policy", text)
}

pub(super) struct Rig {
    pub(super) fx: Fx,
    pub(super) port: Arc<FakePort>,
    pub(super) mock: MockExecutor,
}

impl Rig {
    /// A kernel with `policy` and the bots `keeper` and `scout` loaded, and a mock that plays
    /// `steps`.
    pub(super) async fn new(policy_text: &str, steps: Vec<MockStep>) -> Self {
        Self::with(policy_text, steps, config(1)).await
    }

    pub(super) async fn with(
        policy_text: &str,
        steps: Vec<MockStep>,
        config: KernelConfig,
    ) -> Self {
        Self::with_setup(policy_text, steps, config, |setup| setup).await
    }

    /// Like `new`, with the system's wake-up wired in.
    pub(super) async fn waking(policy_text: &str, steps: Vec<MockStep>, wake: &WakeSignal) -> Self {
        Self::with_setup(policy_text, steps, config(1), |setup| {
            setup.with_wake(wake.clone())
        })
        .await
    }

    /// Like `with`, with `workspace` as the kernel's `Workspace` port.
    pub(super) async fn with_workspace(
        policy_text: &str,
        steps: Vec<MockStep>,
        config: KernelConfig,
        workspace: Arc<dyn crate::Workspace>,
    ) -> Self {
        let mock = mock(steps);
        let port = FakePort::with(vec![
            bot("keeper", ""),
            bot("scout", ""),
            policy(policy_text),
        ]);
        let fx = Fx::with_workspace(
            config,
            registry(std::slice::from_ref(&mock)),
            Some(setup(&port)),
            workspace,
        )
        .await;
        Self { fx, port, mock }
    }

    async fn with_setup(
        policy_text: &str,
        steps: Vec<MockStep>,
        config: KernelConfig,
        adjust: impl FnOnce(crate::DefsSetup) -> crate::DefsSetup,
    ) -> Self {
        let mock = mock(steps);
        let port = FakePort::with(vec![
            bot("keeper", ""),
            bot("scout", ""),
            policy(policy_text),
        ]);
        let fx = Fx::with_definitions(
            config,
            registry(std::slice::from_ref(&mock)),
            adjust(setup(&port)),
        )
        .await;
        Self { fx, port, mock }
    }

    /// A run of `bot` in the worktree.
    pub(super) async fn run_as(&self, bot: &str) -> RunId {
        self.fx
            .kernel
            .submit_for_bot(bot, "go", None, WORKTREE.into(), None)
            .await
            .unwrap()
    }

    /// Replace the policy file and wait until the kernel holds the new version.
    pub(super) async fn reload(&self, policy_text: &str) {
        let version = || {
            self.fx
                .kernel
                .current_defs()
                .version_of(DefKind::Policy, "policy")
                .map(|v| v.version)
        };
        let before = version();
        self.port.set(vec![
            bot("keeper", ""),
            bot("scout", ""),
            policy(policy_text),
        ]);
        self.port.touch_and_wait().await;
        until(async || (version() > before).then_some(())).await;
    }

    pub(super) async fn pending(&self) -> Approval {
        until(async || {
            self.fx
                .kernel
                .pending_approvals()
                .await
                .unwrap()
                .into_iter()
                .next()
        })
        .await
    }

    pub(super) async fn history(&self, run: &RunId) -> Vec<ApprovalRecord> {
        self.fx
            .kernel
            .approval_history(Some(run), None)
            .await
            .unwrap()
    }

    pub(super) fn decisions(&self) -> Vec<Decision> {
        self.mock
            .log()
            .answers
            .iter()
            .map(|a| match a.answer {
                Answer::Decision(decision, _) => decision,
                ref other => panic!("not a decision: {other:?}"),
            })
            .collect()
    }
}

/// Waits until the kernel holds version `version` of the bot `name`.
pub(super) async fn bot_at(fx: &Fx, name: &str, version: u32) {
    until(async || {
        let current = fx.kernel.current_defs();
        (current.version_of(DefKind::Bot, name).map(|v| v.version) == Some(version)).then_some(())
    })
    .await;
}

/// The `ApprovalResolved` events of a journal, in order.
pub(super) fn resolved(
    journal: &[(Seq, ExecEvent)],
) -> Vec<(String, ApprovalOutcome, ResolvedBy, Option<String>)> {
    journal
        .iter()
        .filter_map(|(_, event)| match event {
            ExecEvent::ApprovalResolved {
                request_id,
                decision,
                by,
                message,
            } => Some((request_id.clone(), *decision, by.clone(), message.clone())),
            _ => None,
        })
        .collect()
}

pub(super) fn rule(name: &str) -> ResolvedBy {
    ResolvedBy::Rule { name: name.into() }
}

const CARGO_ALLOWED: &str = r#"
default = "ask"
[[rules]]
note = "cargo is fine"
effect = "allow"
match = { action = "command", command = "cargo *" }
"#;

#[tokio::test(start_paused = true)]
async fn an_allow_rule_approves_at_once_and_the_human_never_sees_it() {
    let rig = Rig::new(CARGO_ALLOWED, script(vec![("r1", command("cargo test"))])).await;
    let mut view = rig.fx.kernel.watch_approvals();
    let run = rig.run_as("keeper").await;
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;

    assert_eq!(rig.decisions(), [Decision::Allow]);
    // Never pending, never visible, and the run never waited.
    assert!(rig.fx.kernel.pending_approvals().await.unwrap().is_empty());
    assert!(!view.has_changed().unwrap(), "the watch never carried it");
    view.borrow_and_update();
    let path: Vec<_> = rig
        .fx
        .transitions(&run)
        .await
        .into_iter()
        .map(|(_, to)| to)
        .collect();
    assert_eq!(path, ["running", "evaluating", "succeeded"]);

    // But it is on record, with the rule that decided it.
    let history = rig.history(&run).await;
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, ApprovalStatus::Approved);
    assert_eq!(history[0].decided_by, Some(rule("cargo is fine")));
    assert_eq!(history[0].request_id, "r1");
    assert_eq!(history[0].subject, command("cargo test"));
    assert!(history[0].decided_at_ms.is_some());

    // And the transcript says why: the request, then its resolution, then the end.
    let journal = rig.fx.journal_with_done(&run).await;
    let kinds: Vec<&str> = journal
        .iter()
        .map(|(_, e)| match e {
            ExecEvent::Started { .. } => "started",
            ExecEvent::NeedsApproval { .. } => "needs_approval",
            ExecEvent::ApprovalResolved { .. } => "resolved",
            ExecEvent::Text { .. } => "text",
            ExecEvent::Done { .. } => "done",
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(
        kinds,
        ["started", "needs_approval", "resolved", "text", "done"]
    );
    assert_eq!(
        resolved(&journal),
        [(
            "r1".into(),
            ApprovalOutcome::Approved,
            rule("cargo is fine"),
            None
        )]
    );
}

#[tokio::test(start_paused = true)]
async fn a_deny_rule_refuses_at_once_with_its_message() {
    let rig = Rig::new(
        "default = \"allow\"\n[[rules]]\nnote = \"never reset hard\"\neffect = \"deny\"\nmatch = { action = \"command\", command = \"git reset --hard*\" }\n",
        script(vec![("r1", command("git reset --hard HEAD~3"))]),
    )
    .await;
    let run = rig.run_as("keeper").await;
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;

    assert_eq!(rig.decisions(), [Decision::Deny]);
    // The harness is told the rule's words as well as no: the journal's message and the
    // executor's are the same text.
    assert_eq!(
        rig.mock.log().answers[0].answer,
        Answer::Decision(
            Decision::Deny,
            Some("denied by policy: never reset hard".into())
        )
    );
    let journal = rig.fx.journal_with_done(&run).await;
    assert_eq!(
        resolved(&journal),
        [(
            "r1".into(),
            ApprovalOutcome::Denied,
            rule("never reset hard"),
            Some("denied by policy: never reset hard".into())
        )]
    );
    let history = rig.history(&run).await;
    assert_eq!(history[0].status, ApprovalStatus::Denied);
    assert_eq!(history[0].decided_by, Some(rule("never reset hard")));
    assert!(rig.fx.kernel.pending_approvals().await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn no_matching_rule_applies_the_default_and_names_it() {
    for (default, decision, outcome) in [
        ("allow", Decision::Allow, ApprovalOutcome::Approved),
        ("deny", Decision::Deny, ApprovalOutcome::Denied),
    ] {
        let rig = Rig::new(
            &format!("default = \"{default}\"\n"),
            script(vec![("r1", command("anything"))]),
        )
        .await;
        let run = rig.run_as("keeper").await;
        rig.fx.wait_status(&run, RunStatus::Succeeded).await;
        assert_eq!(rig.decisions(), [decision]);
        let journal = rig.fx.journal_with_done(&run).await;
        let (_, got, by, _) = resolved(&journal).remove(0);
        assert_eq!((got, by), (outcome, rule("default")));
    }
}

#[tokio::test(start_paused = true)]
async fn ask_creates_a_pending_approval_and_a_human_approves_it() {
    let rig = Rig::new(
        "default = \"ask\"\n",
        script(vec![("r1", command("git push"))]),
    )
    .await;
    let mut view = rig.fx.kernel.watch_approvals();
    let run = rig.run_as("keeper").await;

    let pending = rig.pending().await;
    assert_eq!(
        (pending.run_id.clone(), pending.subject.clone()),
        (run.clone(), command("git push"))
    );
    rig.fx.wait_status(&run, RunStatus::WaitingApproval).await;
    // The watch follows the store: it is refreshed just after the row is written.
    until(async || (view.borrow_and_update().len() == 1).then_some(())).await;
    assert_eq!(view.borrow().clone(), vec![pending.clone()]);
    assert!(rig.decisions().is_empty(), "the harness is still held");

    // The stored record is pending, with a deadline 30 minutes out.
    let stored = rig
        .fx
        .store
        .get_approval(&pending.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.status, ApprovalStatus::Pending);
    assert_eq!(stored.expires_at - stored.created_at, 30 * 60 * 1000);

    rig.fx
        .kernel
        .decide(&pending.id, ApprovalDecision::ApproveOnce)
        .await
        .unwrap();
    assert!(view.borrow_and_update().is_empty());
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(rig.decisions(), [Decision::Allow]);

    let history = rig.history(&run).await;
    assert_eq!(history[0].id, pending.id);
    assert_eq!(history[0].status, ApprovalStatus::Approved);
    assert_eq!(history[0].decided_by, Some(ResolvedBy::Human));
    let journal = rig.fx.journal_with_done(&run).await;
    assert_eq!(
        resolved(&journal),
        [(
            "r1".into(),
            ApprovalOutcome::Approved,
            ResolvedBy::Human,
            None
        )]
    );
    let path: Vec<_> = rig
        .fx
        .transitions(&run)
        .await
        .into_iter()
        .map(|(_, to)| to)
        .collect();
    assert_eq!(
        path,
        [
            "running",
            "waiting_approval",
            "running",
            "evaluating",
            "succeeded"
        ]
    );

    // The event log has the request and the decision, each attributed.
    let events = rig.fx.store.events_since(Seq(0), 10_000).await.unwrap();
    let decided: Vec<_> = events
        .iter()
        .filter(|e| e.kind == "approval.decided")
        .collect();
    assert_eq!(decided.len(), 1);
    assert_eq!(decided[0].principal.as_deref(), Some("human"));
    assert!(events.iter().any(|e| e.kind == "approval.requested"));
}

#[tokio::test(start_paused = true)]
async fn a_human_can_deny() {
    let rig = Rig::new(
        "default = \"ask\"\n",
        script(vec![("r1", command("curl evil.example"))]),
    )
    .await;
    let run = rig.run_as("keeper").await;
    let pending = rig.pending().await;
    rig.fx
        .kernel
        .decide(&pending.id, ApprovalDecision::Deny)
        .await
        .unwrap();
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(rig.decisions(), [Decision::Deny]);
    // A person's no has no rule message to pass on.
    assert_eq!(
        rig.mock.log().answers[0].answer,
        Answer::Decision(Decision::Deny, None)
    );
    let history = rig.history(&run).await;
    assert_eq!(history[0].status, ApprovalStatus::Denied);
    assert_eq!(history[0].decided_by, Some(ResolvedBy::Human));
    assert_eq!(
        resolved(&rig.fx.journal_with_done(&run).await),
        [(
            "r1".into(),
            ApprovalOutcome::Denied,
            ResolvedBy::Human,
            None
        )]
    );
    // Deciding again finds nothing.
    assert!(matches!(
        rig.fx
            .kernel
            .decide(&pending.id, ApprovalDecision::ApproveOnce)
            .await,
        Err(KernelError::ApprovalNotFound(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn a_humans_deny_message_reaches_the_executor_the_journal_and_the_history() {
    let rig = Rig::new(
        "default = \"ask\"\n",
        script(vec![("r1", command("curl evil.example"))]),
    )
    .await;
    let run = rig.run_as("keeper").await;
    let pending = rig.pending().await;
    // Blank space around it is trimmed.
    rig.fx
        .kernel
        .decide_with_message(
            &pending.id,
            ApprovalDecision::Deny,
            Some("  use the mirror instead \n".into()),
        )
        .await
        .unwrap();
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(
        rig.mock.log().answers[0].answer,
        Answer::Decision(Decision::Deny, Some("use the mirror instead".into()))
    );
    assert_eq!(
        resolved(&rig.fx.journal_with_done(&run).await),
        [(
            "r1".into(),
            ApprovalOutcome::Denied,
            ResolvedBy::Human,
            Some("use the mirror instead".into())
        )]
    );
    let history = rig.history(&run).await;
    assert_eq!(history[0].status, ApprovalStatus::Denied);
    assert_eq!(
        history[0].message.as_deref(),
        Some("use the mirror instead")
    );
}

#[tokio::test(start_paused = true)]
async fn a_blank_deny_message_is_none_and_an_approval_takes_no_message() {
    let rig = Rig::new(
        "default = \"ask\"\n",
        script(vec![
            ("r1", command("curl evil.example")),
            ("r2", command("curl other.example")),
        ]),
    )
    .await;
    let run = rig.run_as("keeper").await;
    let first = rig.pending().await;
    // A reason with an approval, or an over-long one, is refused and the approval stays open.
    for (decision, text) in [
        (ApprovalDecision::ApproveOnce, "because".to_owned()),
        (ApprovalDecision::Deny, "x".repeat(1001)),
    ] {
        assert!(matches!(
            rig.fx
                .kernel
                .decide_with_message(&first.id, decision, Some(text))
                .await,
            Err(KernelError::Invalid(_))
        ));
    }
    rig.fx
        .kernel
        .decide_with_message(&first.id, ApprovalDecision::Deny, Some("   ".into()))
        .await
        .unwrap();
    assert_eq!(
        rig.mock.log().answers[0].answer,
        Answer::Decision(Decision::Deny, None)
    );
    let second = rig.pending().await;
    rig.fx
        .kernel
        .decide_with_message(&second.id, ApprovalDecision::ApproveOnce, Some(" ".into()))
        .await
        .unwrap();
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;
    let history = rig.history(&run).await;
    assert!(history.iter().all(|record| record.message.is_none()));
}

#[tokio::test(start_paused = true)]
async fn an_approval_nobody_answers_is_denied_and_expired_at_its_deadline() {
    let rig = Rig::new(
        "default = \"ask\"\n",
        script(vec![("r1", command("git push"))]),
    )
    .await;
    let run = rig.run_as("keeper").await;
    let pending = rig.pending().await;
    rig.fx.wait_status(&run, RunStatus::WaitingApproval).await;

    // Ten seconds short of 30 minutes (paused time has moved a few milliseconds since the
    // approval was raised, so the margin is generous): still held.
    advance(Duration::from_secs(30 * 60 - 10)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert!(rig.decisions().is_empty());
    assert_eq!(rig.fx.status(&run).await, RunStatus::WaitingApproval);
    assert_eq!(rig.fx.kernel.pending_approvals().await.unwrap().len(), 1);

    // Past the deadline: denied. The record's clock moves with it.
    rig.fx.clock.advance(30 * 60 * 1000);
    advance(Duration::from_secs(10)).await;
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(rig.decisions(), [Decision::Deny]);
    assert!(rig.fx.kernel.pending_approvals().await.unwrap().is_empty());

    let history = rig.history(&run).await;
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id, pending.id);
    assert_eq!(history[0].status, ApprovalStatus::Expired);
    assert_eq!(history[0].decided_by, Some(ResolvedBy::Expired));
    assert_eq!(
        history[0].decided_at_ms.unwrap() - history[0].created_at_ms,
        30 * 60 * 1000,
        "decided at the deadline of the stored record"
    );
    assert_eq!(history[0].decided_at_ms.unwrap(), history[0].expires_at_ms);
    assert_eq!(
        resolved(&rig.fx.journal_with_done(&run).await),
        [(
            "r1".into(),
            ApprovalOutcome::Denied,
            ResolvedBy::Expired,
            None
        )]
    );
    // A human who answers late finds it gone.
    assert!(matches!(
        rig.fx
            .kernel
            .decide(&pending.id, ApprovalDecision::ApproveOnce)
            .await,
        Err(KernelError::ApprovalNotFound(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn the_policy_may_set_its_own_time_limit_and_the_config_sets_the_default() {
    // The policy's `approval_ttl` wins over the configured default.
    let rig = Rig::new(
        "default = \"ask\"\napproval_ttl = \"2m\"\n",
        script(vec![("r1", command("x"))]),
    )
    .await;
    let run = rig.run_as("keeper").await;
    let pending = rig.pending().await;
    let stored = rig
        .fx
        .store
        .get_approval(&pending.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.expires_at - stored.created_at, 2 * 60 * 1000);
    advance(Duration::from_secs(2 * 60)).await;
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(rig.decisions(), [Decision::Deny]);
    assert_eq!(rig.history(&run).await[0].status, ApprovalStatus::Expired);

    // With none in the policy, the kernel's configuration applies.
    let config = KernelConfig {
        approval_ttl: Duration::from_secs(10 * 60),
        ..config(1)
    };
    let rig = Rig::with(
        "default = \"ask\"\n",
        script(vec![("r1", command("x"))]),
        config,
    )
    .await;
    let run = rig.run_as("keeper").await;
    let pending = rig.pending().await;
    let stored = rig
        .fx
        .store
        .get_approval(&pending.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.expires_at - stored.created_at, 10 * 60 * 1000);
    advance(Duration::from_secs(10 * 60)).await;
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;
}

#[tokio::test(start_paused = true)]
async fn several_waiting_approvals_each_expire_at_their_own_deadline() {
    // Two approvals open at once, 5 minutes apart: the one timer is re-armed for the second.
    // The mock asks for the second only once it is steered, which the test does 5 minutes later.
    let steps = vec![
        MockStep::started("s", "mock-1"),
        asks("r1", command("a")),
        MockStep::AwaitSteer,
        asks("r2", command("b")),
        awaits("r1"),
        awaits("r2"),
        MockStep::text("after"),
    ];
    let rig = Rig::new("default = \"ask\"\napproval_ttl = \"10m\"\n", steps).await;
    let run = rig.run_as("keeper").await;
    until(async || (rig.fx.kernel.pending_approvals().await.unwrap().len() == 1).then_some(()))
        .await;
    advance(Duration::from_secs(5 * 60)).await;
    rig.fx.kernel.steer(&run, "ask the second").await.unwrap();
    until(async || (rig.fx.kernel.pending_approvals().await.unwrap().len() == 2).then_some(()))
        .await;

    // r1 is due in about 5 minutes and r2 in about 10; each step has ten seconds of margin for
    // the few milliseconds paused time moved while the test waited.
    advance(Duration::from_secs(5 * 60 + 10)).await;
    until(async || (rig.fx.kernel.pending_approvals().await.unwrap().len() == 1).then_some(()))
        .await;
    until(async || (rig.decisions().len() == 1).then_some(())).await;
    assert_eq!(rig.decisions(), [Decision::Deny]);
    assert_eq!(
        rig.fx.status(&run).await,
        RunStatus::WaitingApproval,
        "r2 still holds the run"
    );
    advance(Duration::from_secs(5 * 60 + 10)).await;
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(rig.decisions(), [Decision::Deny, Decision::Deny]);
}

#[tokio::test(start_paused = true)]
async fn a_subject_denied_earlier_in_the_run_is_denied_again_unasked() {
    let rig = Rig::new(
        "default = \"ask\"\n",
        script(vec![
            ("r1", command("rm important")),
            ("r2", command("rm important")),
            ("r3", command("ls")),
        ]),
    )
    .await;
    let run = rig.run_as("keeper").await;
    let first = rig.pending().await;
    rig.fx
        .kernel
        .decide(&first.id, ApprovalDecision::Deny)
        .await
        .unwrap();

    // r2 is the same subject: denied at once, and no second pending approval ever appears.
    until(async || (rig.decisions().len() == 2).then_some(())).await;
    assert_eq!(rig.decisions(), [Decision::Deny, Decision::Deny]);
    assert!(
        rig.fx
            .kernel
            .pending_approvals()
            .await
            .unwrap()
            .iter()
            .all(|a| a.subject != command("rm important"))
    );

    // r3 is a different subject, so it asks.
    let third = until(async || {
        rig.fx
            .kernel
            .pending_approvals()
            .await
            .unwrap()
            .into_iter()
            .next()
    })
    .await;
    assert_eq!(third.subject, command("ls"));
    rig.fx
        .kernel
        .decide(&third.id, ApprovalDecision::ApproveOnce)
        .await
        .unwrap();
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;

    let journal = rig.fx.journal_with_done(&run).await;
    assert_eq!(
        resolved(&journal),
        [
            (
                "r1".into(),
                ApprovalOutcome::Denied,
                ResolvedBy::Human,
                None
            ),
            (
                "r2".into(),
                ApprovalOutcome::Denied,
                ResolvedBy::RepeatDenial,
                None
            ),
            (
                "r3".into(),
                ApprovalOutcome::Approved,
                ResolvedBy::Human,
                None
            ),
        ]
    );
    let history = rig.history(&run).await;
    assert_eq!(history.len(), 3);
    assert_eq!(
        history
            .iter()
            .filter(|r| r.decided_by == Some(ResolvedBy::RepeatDenial))
            .count(),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn a_denial_is_remembered_for_its_own_run_only() {
    let rig = Rig::new(
        "default = \"ask\"\n",
        script(vec![("r1", command("rm important"))]),
    )
    .await;
    let first = rig.run_as("keeper").await;
    let a = rig.pending().await;
    rig.fx
        .kernel
        .decide(&a.id, ApprovalDecision::Deny)
        .await
        .unwrap();
    rig.fx.wait_status(&first, RunStatus::Succeeded).await;

    // A later run asks the same thing and is asked again.
    let second = rig.run_as("keeper").await;
    let b = rig.pending().await;
    assert_ne!(b.id, a.id);
    assert_eq!(b.run_id, second);
    rig.fx
        .kernel
        .decide(&b.id, ApprovalDecision::ApproveOnce)
        .await
        .unwrap();
    rig.fx.wait_status(&second, RunStatus::Succeeded).await;
}

#[tokio::test(start_paused = true)]
async fn an_unanswered_request_counts_as_denied_so_a_retry_is_denied_unasked() {
    let rig = Rig::new(
        "default = \"ask\"\napproval_ttl = \"1m\"\n",
        script(vec![("r1", command("x")), ("r2", command("x"))]),
    )
    .await;
    let run = rig.run_as("keeper").await;
    rig.pending().await;
    advance(Duration::from_secs(70)).await;
    // No answer means deny, so the harness that asks again gets the same answer at once: it is
    // not held for a second time limit.
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(rig.decisions(), [Decision::Deny, Decision::Deny]);
    assert!(rig.fx.kernel.pending_approvals().await.unwrap().is_empty());
    let history = rig.history(&run).await;
    assert_eq!(
        history
            .iter()
            .map(|r| (r.request_id.as_str(), r.status, r.decided_by.clone()))
            .collect::<Vec<_>>(),
        [
            ("r2", ApprovalStatus::Denied, Some(ResolvedBy::RepeatDenial)),
            ("r1", ApprovalStatus::Expired, Some(ResolvedBy::Expired)),
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_remembered_denial_beats_an_allow_rule_added_later() {
    let rig = Rig::new(
        "default = \"ask\"\n",
        script(vec![
            ("r1", command("cargo publish")),
            ("r2", command("cargo publish")),
        ]),
    )
    .await;
    let run = rig.run_as("keeper").await;
    let first = rig.pending().await;
    // The policy now allows cargo, before the first answer and so before the repeat.
    rig.reload(CARGO_ALLOWED).await;
    rig.fx
        .kernel
        .decide(&first.id, ApprovalDecision::Deny)
        .await
        .unwrap();
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(rig.decisions(), [Decision::Deny, Decision::Deny]);
    let journal = rig.fx.journal_with_done(&run).await;
    assert_eq!(resolved(&journal)[1].2, ResolvedBy::RepeatDenial);
}

const OUTSIDE_DENIED: &str = r#"
default = "ask"
[[rules]]
note = "outside the worktree"
effect = "deny"
match = { action = "edit", path = "/**" }
[[rules]]
note = "inside the worktree"
effect = "allow"
match = { action = "edit", path = "**" }
"#;

#[tokio::test(start_paused = true)]
async fn slash_star_star_is_outside_the_worktree_including_a_dotdot_escape() {
    let cases = [
        ("inside, relative", "src/a.rs", Decision::Allow),
        ("inside, absolute", "/work/tree/src/b.rs", Decision::Allow),
        (
            "inside, dot-dot that stays",
            "src/../lib.rs",
            Decision::Allow,
        ),
        ("outside, absolute", "/etc/passwd", Decision::Deny),
        (
            "outside, sibling with the same prefix",
            "/work/tree-other/x",
            Decision::Deny,
        ),
        ("outside, dot-dot escape", "../outside.txt", Decision::Deny),
        (
            "outside, deeper dot-dot escape",
            "src/../../outside.txt",
            Decision::Deny,
        ),
        (
            "outside, absolute dot-dot escape",
            "/work/tree/../other/x",
            Decision::Deny,
        ),
    ];
    let requests: Vec<(String, ApprovalSubject)> = cases
        .iter()
        .enumerate()
        .map(|(n, (_, path, _))| (format!("r{n}"), edit(path)))
        .collect();
    let steps = script(
        requests
            .iter()
            .map(|(r, s)| (r.as_str(), s.clone()))
            .collect(),
    );
    let rig = Rig::new(OUTSIDE_DENIED, steps).await;
    let run = rig.run_as("keeper").await;
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;
    for (n, (what, _, want)) in cases.iter().enumerate() {
        assert_eq!(rig.decisions()[n], *want, "{what}");
    }
    assert!(
        rig.fx.kernel.pending_approvals().await.unwrap().is_empty(),
        "no human was asked"
    );
}

#[tokio::test(start_paused = true)]
async fn a_rule_scoped_to_a_bot_applies_to_that_bot_only() {
    let rig = Rig::new(
        r#"
        default = "ask"
        [[rules]]
        note = "cargo for keeper"
        effect = "allow"
        match = { action = "command", command = "cargo *", scope = ["keeper"] }
        "#,
        script(vec![("r1", command("cargo build"))]),
    )
    .await;
    let keeper = rig.run_as("keeper").await;
    rig.fx.wait_status(&keeper, RunStatus::Succeeded).await;
    assert_eq!(
        rig.history(&keeper).await[0].decided_by,
        Some(rule("cargo for keeper"))
    );

    // The scout is asked.
    let scout = rig.run_as("scout").await;
    let pending = rig.pending().await;
    assert_eq!(pending.run_id, scout);
    rig.fx
        .kernel
        .decide(&pending.id, ApprovalDecision::Deny)
        .await
        .unwrap();
    rig.fx.wait_status(&scout, RunStatus::Succeeded).await;

    // So is a run that is no bot's.
    let plain = rig
        .fx
        .kernel
        .submit("go", id("mock"), WORKTREE.into(), None)
        .await
        .unwrap();
    let pending = rig.pending().await;
    assert_eq!(pending.run_id, plain);
    rig.fx
        .kernel
        .decide(&pending.id, ApprovalDecision::Deny)
        .await
        .unwrap();
    rig.fx.wait_status(&plain, RunStatus::Succeeded).await;
}

#[tokio::test(start_paused = true)]
async fn a_policy_reload_changes_decisions_for_new_approvals_only() {
    let rig = Rig::new(
        "default = \"ask\"\napproval_ttl = \"5m\"\n",
        script(vec![
            ("r1", command("make")),
            ("r2", command("make install")),
        ]),
    )
    .await;
    let run = rig.run_as("keeper").await;
    let first = rig.pending().await;

    // The policy now denies everything, and has a longer time limit.
    rig.reload("default = \"deny\"\napproval_ttl = \"1h\"\n")
        .await;
    // The waiting approval is untouched: still pending, still decidable, same deadline.
    let stored = rig.fx.store.get_approval(&first.id).await.unwrap().unwrap();
    assert_eq!(stored.status, ApprovalStatus::Pending);
    assert_eq!(stored.expires_at - stored.created_at, 5 * 60 * 1000);
    assert_eq!(rig.fx.kernel.pending_approvals().await.unwrap().len(), 1);

    rig.fx
        .kernel
        .decide(&first.id, ApprovalDecision::ApproveOnce)
        .await
        .unwrap();
    // The next one meets the new policy.
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(rig.decisions(), [Decision::Allow, Decision::Deny]);
    let journal = rig.fx.journal_with_done(&run).await;
    assert_eq!(
        resolved(&journal)
            .iter()
            .map(|r| r.2.clone())
            .collect::<Vec<_>>(),
        [ResolvedBy::Human, rule("default")]
    );
}

#[tokio::test(start_paused = true)]
async fn history_lists_decided_records_newest_first_and_by_run() {
    let rig = Rig::new(
        "default = \"ask\"\n[[rules]]\nnote = \"ls ok\"\neffect = \"allow\"\nmatch = { command = \"ls*\" }\n",
        script(vec![("r1", command("ls")), ("r2", command("git push"))]),
    )
    .await;
    let run = rig.run_as("keeper").await;
    let pending = rig.pending().await;
    // While r2 is pending, history holds only the decided r1.
    let history = rig.history(&run).await;
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].request_id, "r1");
    rig.fx.clock.advance(1000);
    rig.fx
        .kernel
        .decide(&pending.id, ApprovalDecision::ApproveOnce)
        .await
        .unwrap();
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;

    let all = rig.fx.kernel.approval_history(None, None).await.unwrap();
    assert_eq!(
        all.iter()
            .map(|r| r.request_id.as_str())
            .collect::<Vec<_>>(),
        ["r2", "r1"]
    );
    let limited = rig.fx.kernel.approval_history(None, Some(1)).await.unwrap();
    assert_eq!(limited.len(), 1);
    let other = RunId::from_suffix("nothere").unwrap();
    assert!(
        rig.fx
            .kernel
            .approval_history(Some(&other), None)
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test(start_paused = true)]
async fn a_run_that_ends_while_waiting_closes_its_approval_before_its_done() {
    let rig = Rig::new("default = \"ask\"\n", script(vec![("r1", command("x"))])).await;
    let run = rig.run_as("keeper").await;
    let pending = rig.pending().await;
    rig.fx.kernel.cancel(&run).await.unwrap();
    rig.fx.wait_status(&run, RunStatus::Cancelled).await;

    let journal = rig.fx.journal_with_done(&run).await;
    let kinds: Vec<_> = journal
        .iter()
        .map(|(_, e)| match e {
            ExecEvent::NeedsApproval { .. } => "needs",
            ExecEvent::ApprovalResolved { .. } => "resolved",
            ExecEvent::Done { .. } => "done",
            _ => "other",
        })
        .collect();
    assert_eq!(
        &kinds[kinds.len() - 3..],
        ["needs", "resolved", "done"],
        "{journal:?}"
    );
    assert!(matches!(
        journal.last().unwrap().1,
        ExecEvent::Done {
            stop: StopReason::Interrupted,
            ..
        }
    ));
    let stored = rig
        .fx
        .store
        .get_approval(&pending.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.status, ApprovalStatus::Expired);
    assert!(rig.fx.kernel.pending_approvals().await.unwrap().is_empty());
    assert!(matches!(
        rig.fx
            .kernel
            .decide(&pending.id, ApprovalDecision::ApproveOnce)
            .await,
        Err(KernelError::ApprovalNotFound(_))
    ));
}

/// Cancelling a held run does not say "nobody answered in time": the approval ended because the
/// run did, and the record, the journal and the harness-facing answer all say so. A request that
/// really timed out carries no such reason.
#[tokio::test(start_paused = true)]
async fn cancelling_a_held_run_withdraws_its_approval_instead_of_expiring_it() {
    let rig = Rig::new("default = \"ask\"\n", script(vec![("r1", command("x"))])).await;
    let run = rig.run_as("keeper").await;
    let pending = rig.pending().await;
    rig.fx.kernel.cancel(&run).await.unwrap();
    rig.fx.wait_status(&run, RunStatus::Cancelled).await;

    let journal = rig.fx.journal_with_done(&run).await;
    let resolved = journal.iter().find_map(|(_, e)| match e {
        ExecEvent::ApprovalResolved { by, message, .. } => Some((by.clone(), message.clone())),
        _ => None,
    });
    assert_eq!(
        resolved,
        Some((
            ResolvedBy::Expired,
            Some("withdrawn: the run was cancelled".to_owned())
        ))
    );
    let history = rig
        .fx
        .kernel
        .approval_history(Some(&run), None)
        .await
        .unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id, pending.id);
    assert_eq!(history[0].status, ApprovalStatus::Expired);
    assert_eq!(
        history[0].message.as_deref(),
        Some("withdrawn: the run was cancelled")
    );
}

#[tokio::test(start_paused = true)]
async fn shutdown_closes_what_a_dropped_supervisor_left_pending() {
    // A zero deadline drops the supervisors where they stand, before they can close anything.
    let config = KernelConfig {
        shutdown_deadline: Duration::ZERO,
        ..config(1)
    };
    let rig = Rig::with(
        "default = \"ask\"\n",
        script(vec![("r1", command("x"))]),
        config,
    )
    .await;
    let run = rig.run_as("keeper").await;
    let pending = rig.pending().await;
    let kernel = rig.fx.kernel.clone();
    let stopping = tokio::spawn(async move { kernel.shutdown().await });
    until(async || stopping.is_finished().then_some(())).await;
    assert!(stopping.await.unwrap().aborted);

    let stored = rig
        .fx
        .store
        .get_approval(&pending.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.status, ApprovalStatus::Expired);
    assert!(rig.fx.kernel.pending_approvals().await.unwrap().is_empty());
    assert_eq!(
        resolved(&rig.fx.journal(&run)),
        [(
            "r1".into(),
            ApprovalOutcome::Denied,
            ResolvedBy::Expired,
            None
        )]
    );
}

/// A store as a crashed daemon left it: a run waiting on a held approval, its journal showing the
/// request and not the end.
async fn crashed_with_a_pending_approval()
-> (tempfile::TempDir, Store, RunId, reno_proto::ApprovalId) {
    use reno_store::{NewApproval, NewTask};
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("reno.db")).unwrap();
    let task = reno_proto::TaskId::from_suffix("crash").unwrap();
    let run = RunId::from_suffix("crash").unwrap();
    store
        .create_task(
            NewTask {
                id: task.clone(),
                goal: "g".into(),
                class: None,
                spec: serde_json::json!({ "cwd": WORKTREE }),
            },
            1,
        )
        .await
        .unwrap();
    store
        .enqueue_run(&task, &run, &id("mock"), 2)
        .await
        .unwrap();
    let claim = store.claim_next("old", 3, 60_000).await.unwrap().unwrap();
    for to in [RunStatus::Running, RunStatus::WaitingApproval] {
        store.transition(&run, claim.fence, to, 4).await.unwrap();
    }
    let approval = reno_proto::ApprovalId::from_suffix("crash").unwrap();
    store
        .create_approval(
            NewApproval {
                id: approval.clone(),
                run_id: run.clone(),
                request_id: "r1".into(),
                subject: command("git push"),
                expires_at: 1_000_000,
            },
            claim.fence,
            5,
        )
        .await
        .unwrap();
    let mut writer = store.journals().writer(&run).unwrap();
    writer
        .append(&ExecEvent::NeedsApproval {
            request_id: "r1".into(),
            subject: command("git push"),
        })
        .unwrap();
    writer.flush().unwrap();
    drop(writer);
    (dir, store, run, approval)
}

#[tokio::test(start_paused = true)]
async fn a_restart_expires_the_approvals_an_interrupted_run_was_waiting_on() {
    let (dir, store, run, approval) = crashed_with_a_pending_approval().await;
    assert_eq!(store.pending_approvals().await.unwrap().len(), 1);

    let fx = Fx::over(dir, store, config(1), registry(&[mock([])])).await;

    assert_eq!(fx.status(&run).await, RunStatus::Interrupted);
    let stored = fx.store.get_approval(&approval).await.unwrap().unwrap();
    assert_eq!(stored.status, ApprovalStatus::Expired);
    assert_eq!(stored.decided_by, Some(ResolvedBy::Expired));
    assert!(fx.kernel.pending_approvals().await.unwrap().is_empty());
    assert!(fx.kernel.watch_approvals().borrow().is_empty());
    let history = fx.kernel.approval_history(Some(&run), None).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, ApprovalStatus::Expired);

    // The journal says so before it says the run was interrupted.
    let journal = fx.journal(&run);
    assert_eq!(journal.len(), 3, "{journal:?}");
    assert!(matches!(journal[0].1, ExecEvent::NeedsApproval { .. }));
    assert_eq!(
        resolved(&journal),
        [(
            "r1".into(),
            ApprovalOutcome::Denied,
            ResolvedBy::Expired,
            None
        )]
    );
    assert!(matches!(
        journal[2].1,
        ExecEvent::Done {
            stop: StopReason::Interrupted,
            ..
        }
    ));
    // And it can no longer be decided.
    assert!(matches!(
        fx.kernel
            .decide(&approval, ApprovalDecision::ApproveOnce)
            .await,
        Err(KernelError::ApprovalNotFound(_))
    ));
    // A second restart finds nothing more to do.
    let (dir, store) = fx.into_store();
    let fx = Fx::over(dir, store, config(1), registry(&[mock([])])).await;
    assert_eq!(resolved(&fx.journal(&run)).len(), 1);
}

// ---- bot launch settings ------------------------------------------------------------------

fn descriptor(name: &str, modes: &[AccessMode]) -> reno_proto::ExecutorDescriptor {
    let mut d = MockExecutor::default_descriptor(id(name));
    d.caps.access_modes = modes.to_vec();
    d
}

#[tokio::test(start_paused = true)]
async fn a_bot_run_launches_with_the_bots_access_and_sandbox() {
    let m = mock([MockStep::text("hi")]);
    let port = FakePort::with(vec![bot(
        "keeper",
        "access = \"auto_accept_edits\"\nsandbox = \"read_only\"",
    )]);
    let fx =
        Fx::with_definitions(config(1), registry(std::slice::from_ref(&m)), setup(&port)).await;
    let run = fx
        .kernel
        .submit_for_bot("keeper", "go", None, WORKTREE.into(), None)
        .await
        .unwrap();
    fx.wait_status(&run, RunStatus::Succeeded).await;
    let specs = m.log().specs;
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].access, AccessMode::AutoAcceptEdits);
    assert_eq!(specs[0].sandbox, SandboxLevel::ReadOnly);
    assert_eq!(specs[0].cwd, std::path::PathBuf::from(WORKTREE));

    // A bot that says nothing gets the strictest access and no sandbox ceiling beyond it.
    port.set(vec![bot("keeper", ""), bot("plain", "")]);
    port.touch_and_wait().await;
    bot_at(&fx, "plain", 1).await;
    let run = fx
        .kernel
        .submit_for_bot("plain", "go", None, WORKTREE.into(), None)
        .await
        .unwrap();
    fx.wait_status(&run, RunStatus::Succeeded).await;
    let specs = m.log().specs;
    assert_eq!(
        (specs[1].access, specs[1].sandbox),
        (AccessMode::Supervised, SandboxLevel::Unrestricted)
    );
}

#[tokio::test(start_paused = true)]
async fn a_bot_run_is_isolated_from_user_config_unless_the_bot_opts_in() {
    let m = mock([MockStep::text("hi")]);
    let port = FakePort::with(vec![
        bot("plain", ""),
        bot("open", "inherit_user_config = true"),
    ]);
    let fx =
        Fx::with_definitions(config(1), registry(std::slice::from_ref(&m)), setup(&port)).await;
    for name in ["plain", "open"] {
        let run = fx
            .kernel
            .submit_for_bot(name, "go", None, WORKTREE.into(), None)
            .await
            .unwrap();
        fx.wait_status(&run, RunStatus::Succeeded).await;
    }
    let specs = m.log().specs;
    assert!(!specs[0].inherit_user_config);
    assert!(specs[1].inherit_user_config);
    // A task with no bot is isolated too.
    let run = fx.submit("go").await;
    fx.wait_status(&run, RunStatus::Succeeded).await;
    assert!(!m.log().specs[2].inherit_user_config);
}

#[tokio::test(start_paused = true)]
async fn a_queued_run_keeps_the_settings_of_the_bot_version_it_is_pinned_to() {
    let m = mock([MockStep::text("hi")]);
    let port = FakePort::with(vec![bot("keeper", "access = \"auto_accept_edits\"")]);
    // One slot, and a run holds it, so the pinned run waits in the queue.
    let registry = ExecutorRegistry::new()
        .with(Arc::new(m.clone()))
        .with(Arc::new(
            MockExecutor::new(id("blocker"))
                .with_steps([MockStep::Sleep(Duration::from_secs(3600))]),
        ));
    let fx = Fx::with_definitions(config(1), registry, setup(&port)).await;
    let held = fx
        .kernel
        .submit("hold", id("blocker"), WORKTREE.into(), None)
        .await
        .unwrap();
    fx.wait_status(&held, RunStatus::Running).await;
    let queued = fx
        .kernel
        .submit_for_bot("keeper", "go", Some(id("mock")), WORKTREE.into(), None)
        .await
        .unwrap();

    // The bot is edited to ask for full access while the run waits.
    port.set(vec![bot("keeper", "access = \"full_access\"")]);
    port.touch_and_wait().await;
    bot_at(&fx, "keeper", 2).await;

    fx.kernel.cancel(&held).await.unwrap();
    fx.wait_status(&queued, RunStatus::Succeeded).await;
    assert_eq!(
        m.log().specs[0].access,
        AccessMode::AutoAcceptEdits,
        "v1, not v2"
    );
}

#[tokio::test(start_paused = true)]
async fn no_eligible_executor_fails_the_run_instead_of_downgrading_it() {
    // The bot wants full access; the only executor it names supports less.
    let m = MockExecutor::new(id("mock"))
        .with_descriptor(descriptor("mock", &[AccessMode::Supervised]));
    let port = FakePort::with(vec![bot("keeper", "access = \"full_access\"")]);
    let fx =
        Fx::with_definitions(config(1), registry(std::slice::from_ref(&m)), setup(&port)).await;
    let run = fx
        .kernel
        .submit_for_bot("keeper", "go", None, WORKTREE.into(), None)
        .await
        .unwrap();
    fx.wait_status(&run, RunStatus::Failed).await;
    let journal = fx.journal_with_done(&run).await;
    let ExecEvent::Done {
        stop, diagnostic, ..
    } = &journal.last().unwrap().1
    else {
        panic!("no Done");
    };
    assert_eq!(*stop, StopReason::Error);
    let why = diagnostic.clone().unwrap();
    assert!(!why.contains("NoEligibleExecutor"), "{why}");
    assert!(
        why.contains("Full access")
            && why.contains("supports only Supervised")
            && !why.contains("["),
        "{why}"
    );
    assert!(
        m.log().specs.is_empty(),
        "never started, with less or with more access"
    );
}

#[tokio::test(start_paused = true)]
async fn a_bot_run_goes_to_the_first_listed_executor_that_supports_its_access() {
    let weak = MockExecutor::new(id("codex"))
        .with_descriptor(descriptor("codex", &[AccessMode::Supervised]));
    let strong = mock([MockStep::text("hi")]);
    let registry = ExecutorRegistry::new()
        .with(Arc::new(weak.clone()))
        .with(Arc::new(strong.clone()));
    let port = FakePort::with(vec![file(
        DefKind::Bot,
        "keeper",
        "name = \"keeper\"\ninstructions = \"work\"\nexecutors = [\"codex\", \"mock\"]\naccess = \"full_access\"\n",
    )]);
    let fx = Fx::with_definitions(config(1), registry, setup(&port)).await;
    let run = fx
        .kernel
        .submit_for_bot("keeper", "go", None, WORKTREE.into(), None)
        .await
        .unwrap();
    assert_eq!(fx.kernel.get_run(&run).await.unwrap().executor, id("mock"));
    fx.wait_status(&run, RunStatus::Succeeded).await;
    assert!(weak.log().specs.is_empty());
    assert_eq!(strong.log().specs[0].access, AccessMode::FullAccess);

    // A bot that asks for less goes to its first executor, as before.
    port.set(vec![file(
        DefKind::Bot,
        "keeper",
        "name = \"keeper\"\ninstructions = \"work\"\nexecutors = [\"codex\", \"mock\"]\n",
    )]);
    port.touch_and_wait().await;
    bot_at(&fx, "keeper", 2).await;
    let run = fx
        .kernel
        .submit_for_bot("keeper", "go", None, WORKTREE.into(), None)
        .await
        .unwrap();
    assert_eq!(fx.kernel.get_run(&run).await.unwrap().executor, id("codex"));
}

/// Lets spawned tasks run without moving paused time.
async fn settle() {
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
}

#[tokio::test(start_paused = true)]
async fn an_approval_that_came_due_while_the_machine_slept_expires_on_the_wake_signal() {
    let wake = WakeSignal::new();
    let same = || command("git push");
    let rig = Rig::waking(
        "default = \"ask\"\n",
        script(vec![("r1", same()), ("r2", same())]),
        &wake,
    )
    .await;
    let run = rig.run_as("keeper").await;
    let pending = rig.pending().await;
    rig.fx.wait_status(&run, RunStatus::WaitingApproval).await;

    // The machine sleeps for an hour: the wall clock moves, the timer only 10 s.
    let begun = tokio::time::Instant::now();
    rig.fx.clock.advance(60 * 60 * 1000);
    advance(Duration::from_secs(10)).await;
    settle().await;
    assert!(rig.decisions().is_empty(), "nothing reads the clock yet");
    assert_eq!(rig.fx.kernel.pending_approvals().await.unwrap().len(), 1);

    // It wakes up. The approval is expired at once, not 5 more minutes of timer time later.
    wake.notify();
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;
    assert!(begun.elapsed() < Duration::from_secs(60));
    assert!(rig.fx.kernel.pending_approvals().await.unwrap().is_empty());

    let history = rig.history(&run).await;
    let first = history.iter().find(|r| r.id == pending.id).unwrap();
    assert_eq!(first.status, ApprovalStatus::Expired);
    assert_eq!(first.decided_by, Some(ResolvedBy::Expired));
    // Exactly as the timer would have: the journal says so, and the denial is remembered, so
    // the same request, asked again, is refused without a human.
    assert_eq!(
        resolved(&rig.fx.journal_with_done(&run).await),
        [
            (
                "r1".into(),
                ApprovalOutcome::Denied,
                ResolvedBy::Expired,
                None
            ),
            (
                "r2".into(),
                ApprovalOutcome::Denied,
                ResolvedBy::RepeatDenial,
                None
            ),
        ]
    );
    assert_eq!(rig.decisions(), [Decision::Deny, Decision::Deny]);
}

#[tokio::test(start_paused = true)]
async fn a_wake_leaves_an_approval_that_is_not_due_and_the_timer_still_expires_it() {
    let wake = WakeSignal::new();
    let rig = Rig::waking(
        "default = \"ask\"\n",
        script(vec![("r1", command("git push"))]),
        &wake,
    )
    .await;
    let run = rig.run_as("keeper").await;
    rig.pending().await;
    rig.fx.wait_status(&run, RunStatus::WaitingApproval).await;

    rig.fx.clock.advance(60 * 1000);
    wake.notify();
    wake.notify();
    settle().await;
    assert!(rig.decisions().is_empty());
    assert_eq!(rig.fx.kernel.pending_approvals().await.unwrap().len(), 1);
    assert_eq!(rig.fx.status(&run).await, RunStatus::WaitingApproval);

    // The monotonic timer is still armed for the deadline.
    rig.fx.clock.advance(30 * 60 * 1000);
    advance(Duration::from_secs(30 * 60 + 1)).await;
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(rig.decisions(), [Decision::Deny]);
}

#[tokio::test(start_paused = true)]
async fn a_wake_expires_only_the_approvals_that_are_due() {
    let wake = WakeSignal::new();
    let rig = Rig::waking(
        "default = \"ask\"\napproval_ttl = \"5m\"\n",
        script(vec![("r1", command("git push"))]),
        &wake,
    )
    .await;
    let run = rig.run_as("keeper").await;
    rig.pending().await;
    rig.fx.wait_status(&run, RunStatus::WaitingApproval).await;
    // Three minutes of wall clock: not yet due at a 5 minute limit.
    rig.fx.clock.advance(3 * 60 * 1000);
    wake.notify();
    settle().await;
    assert_eq!(rig.fx.kernel.pending_approvals().await.unwrap().len(), 1);
    // Two minutes more: due exactly at the stored deadline.
    rig.fx.clock.advance(2 * 60 * 1000);
    wake.notify();
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(rig.decisions(), [Decision::Deny]);
}

fn asks_input(request: &str) -> MockStep {
    MockStep::Emit(ExecEvent::NeedsInput {
        request_id: request.into(),
        questions: vec![reno_proto::Question {
            id: "q1".into(),
            prompt: "which?".into(),
            options: vec!["a".into(), "b".into()],
            multi_select: false,
        }],
    })
}

fn input_script() -> Vec<MockStep> {
    vec![
        MockStep::started("s", "mock-1"),
        asks_input("q1"),
        awaits("q1"),
        MockStep::text("after"),
    ]
}

#[tokio::test(start_paused = true)]
async fn a_question_nobody_answers_is_declined_at_the_approval_deadline() {
    let rig = Rig::new("default = \"ask\"\n", input_script()).await;
    let run = rig.run_as("keeper").await;
    rig.fx.wait_status(&run, RunStatus::WaitingInput).await;

    // Short of the 30 minute default: still waiting, nothing sent.
    advance(Duration::from_secs(30 * 60 - 10)).await;
    settle().await;
    assert_eq!(rig.fx.status(&run).await, RunStatus::WaitingInput);
    assert!(rig.mock.log().answers.is_empty());

    // Past it: the harness is told "no answers", and the run goes on.
    rig.fx.clock.advance(30 * 60 * 1000);
    advance(Duration::from_secs(10)).await;
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;
    let answers = rig.mock.log().answers;
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].request_id, "q1");
    assert!(matches!(&answers[0].answer, Answer::Input(a) if a.is_empty()));

    // The journal says why, between the question and the end.
    let journal = rig.fx.journal_with_done(&run).await;
    let kinds: Vec<_> = journal
        .iter()
        .filter_map(|(_, event)| match event {
            ExecEvent::NeedsInput { request_id, .. } => Some(format!("asked {request_id}")),
            ExecEvent::InputExpired { request_id } => Some(format!("expired {request_id}")),
            _ => None,
        })
        .collect();
    assert_eq!(kinds, ["asked q1", "expired q1"]);
}

#[tokio::test(start_paused = true)]
async fn a_question_that_came_due_while_the_machine_slept_is_declined_on_the_wake_signal() {
    let wake = WakeSignal::new();
    let rig = Rig::waking(
        "default = \"ask\"\napproval_ttl = \"5m\"\n",
        input_script(),
        &wake,
    )
    .await;
    let run = rig.run_as("keeper").await;
    rig.fx.wait_status(&run, RunStatus::WaitingInput).await;

    // Three minutes of wall clock: not yet due.
    rig.fx.clock.advance(3 * 60 * 1000);
    wake.notify();
    settle().await;
    assert_eq!(rig.fx.status(&run).await, RunStatus::WaitingInput);

    // The machine slept past the limit: the wake declines it at once.
    rig.fx.clock.advance(2 * 60 * 1000);
    wake.notify();
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(rig.mock.log().answers.len(), 1);
}

/// A rig whose bot `keeper` runs with full access.
async fn full_access_rig(policy_text: &str, steps: Vec<MockStep>) -> Rig {
    let mock = mock(steps);
    let port = FakePort::with(vec![
        bot("keeper", "access = \"full_access\""),
        policy(policy_text),
    ]);
    let fx = Fx::with_definitions(
        config(1),
        registry(std::slice::from_ref(&mock)),
        setup(&port),
    )
    .await;
    Rig { fx, port, mock }
}

const DENY_RESET: &str = "default = \"ask\"\n[[rules]]\nnote = \"never reset hard\"\neffect = \"deny\"\nmatch = { action = \"command\", command = \"git reset --hard*\" }\n";

#[tokio::test(start_paused = true)]
async fn full_access_still_obeys_a_policy_deny_and_journals_it() {
    let rig = full_access_rig(
        DENY_RESET,
        script(vec![("r1", command("git reset --hard HEAD~3"))]),
    )
    .await;
    let run = rig.run_as("keeper").await;
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(rig.decisions(), [Decision::Deny]);
    let journal = rig.fx.journal_with_done(&run).await;
    assert_eq!(
        resolved(&journal),
        [(
            "r1".into(),
            ApprovalOutcome::Denied,
            rule("never reset hard"),
            Some("denied by policy: never reset hard".into())
        )]
    );
    assert_eq!(rig.history(&run).await.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn full_access_allows_the_rest_without_a_human_and_records_the_mode() {
    let rig = full_access_rig(DENY_RESET, script(vec![("r1", command("cargo build"))])).await;
    let run = rig.run_as("keeper").await;
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(rig.decisions(), [Decision::Allow]);
    assert!(rig.fx.kernel.pending_approvals().await.unwrap().is_empty());
    let path: Vec<_> = rig
        .fx
        .transitions(&run)
        .await
        .into_iter()
        .map(|(_, to)| to)
        .collect();
    assert_eq!(path, ["running", "evaluating", "succeeded"]);
    let journal = rig.fx.journal_with_done(&run).await;
    assert_eq!(
        resolved(&journal),
        [(
            "r1".into(),
            ApprovalOutcome::Approved,
            rule("builtin:full-access"),
            None
        )]
    );
    let history = rig.history(&run).await;
    assert_eq!(history[0].decided_by, Some(rule("builtin:full-access")));
}

const ASK_PUSH: &str = "default = \"allow\"\n[[rules]]\nnote = \"push needs a human\"\neffect = \"ask\"\nmatch = { action = \"command\", command = \"git push*\" }\n";

#[tokio::test(start_paused = true)]
async fn full_access_still_asks_a_person_where_a_policy_rule_says_ask() {
    // An explicit `ask` rule is the owner saying "a human decides this"; the access mode must not
    // answer it. Everything the rule does not name is still allowed without a question.
    let rig = full_access_rig(
        ASK_PUSH,
        script(vec![
            ("r1", command("cargo build")),
            ("r2", command("git push origin main")),
        ]),
    )
    .await;
    let run = rig.run_as("keeper").await;
    rig.fx.wait_status(&run, RunStatus::WaitingApproval).await;
    // The status is published before the pending list (state before visibility): poll for it.
    let pending = until(async || {
        let list = rig.fx.kernel.pending_approvals().await.unwrap();
        (!list.is_empty()).then_some(list)
    })
    .await;
    assert_eq!(pending.len(), 1, "only the push is held");
    assert!(format!("{:?}", pending[0].subject).contains("git push"));
}

#[tokio::test(start_paused = true)]
async fn full_access_holds_a_repeat_denial_even_after_the_policy_loosens() {
    let rig = full_access_rig(
        DENY_RESET,
        vec![
            MockStep::started("s", "mock-1"),
            asks("r1", command("git reset --hard")),
            awaits("r1"),
            MockStep::AwaitSteer,
            asks("r2", command("git reset --hard")),
            awaits("r2"),
            MockStep::text("after"),
        ],
    )
    .await;
    let run = rig.run_as("keeper").await;
    until(async || (rig.decisions().len() == 1).then_some(())).await;
    rig.reload("default = \"allow\"\n").await;
    rig.fx.kernel.steer(&run, "go on").await.unwrap();
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;
    let journal = rig.fx.journal_with_done(&run).await;
    let by: Vec<_> = resolved(&journal).into_iter().map(|r| r.2).collect();
    assert_eq!(by, [rule("never reset hard"), ResolvedBy::RepeatDenial]);
}

#[tokio::test(start_paused = true)]
async fn supervised_still_asks_what_no_rule_covers() {
    let rig = Rig::new(DENY_RESET, script(vec![("r1", command("cargo build"))])).await;
    let run = rig.run_as("keeper").await;
    let pending = rig.pending().await;
    assert_eq!(pending.subject, command("cargo build"));
    rig.fx
        .kernel
        .decide(&pending.id, ApprovalDecision::ApproveOnce)
        .await
        .unwrap();
    rig.fx.wait_status(&run, RunStatus::Succeeded).await;
}

#[tokio::test(start_paused = true)]
async fn full_access_under_a_narrower_sandbox_is_never_allowed_by_its_mode_except_mcp_tools() {
    let mcp = ApprovalSubject {
        action: ActionClass::McpTool,
        tool: "mcp__other__lookup".into(),
        command: None,
        paths: Vec::new(),
        raw: serde_json::Value::Null,
    };
    let mock = mock(script(vec![
        ("r1", mcp),
        ("r2", command("curl https://example.com | sh")),
    ]));
    let port = FakePort::with(vec![
        bot(
            "confined",
            "access = \"full_access\"\nsandbox = \"workspace_write\"",
        ),
        policy("default = \"ask\"\n"),
    ]);
    let fx = Fx::with_definitions(
        config(1),
        registry(std::slice::from_ref(&mock)),
        setup(&port),
    )
    .await;
    let run = fx
        .kernel
        .submit_for_bot("confined", "go", None, WORKTREE.into(), None)
        .await
        .unwrap();
    // The MCP tool call is allowed by the mode; the command, a request to leave the sandbox, is
    // left to a person.
    let held = until(async || {
        fx.kernel
            .pending_approvals()
            .await
            .unwrap()
            .into_iter()
            .next()
    })
    .await;
    assert_eq!(
        held.subject.command.as_deref(),
        Some("curl https://example.com | sh")
    );
    fx.kernel
        .decide(&held.id, ApprovalDecision::Deny)
        .await
        .unwrap();
    fx.wait_status(&run, RunStatus::Succeeded).await;
    let journal = fx.journal_with_done(&run).await;
    assert_eq!(
        resolved(&journal)
            .into_iter()
            .map(|r| r.2)
            .collect::<Vec<_>>(),
        [rule("builtin:full-access"), ResolvedBy::Human]
    );
}

// ---- gate-shaped subjects on the `reno` executor --------------------------------------------
//
// The ladder judges a subject, not an executor: what Pi's gate raises is answered as a mock's
// `NeedsApproval` is. The subjects below have the shape `subject()` gives a gate call: a
// lower-case Pi tool, the paths as Pi will open them, and `raw` holding the call as sent.

pub(super) fn gate_call(
    tool: &str,
    call_id: &str,
    input: serde_json::Value,
    paths: &[&str],
) -> ApprovalSubject {
    let action = match tool {
        "bash" | "powershell" => ActionClass::Command,
        "read" | "grep" | "find" | "ls" => ActionClass::Read,
        "edit" | "write" => ActionClass::Edit,
        t if t.starts_with("mcp__") => ActionClass::McpTool,
        _ => ActionClass::Other,
    };
    let is_path_tool = matches!(action, ActionClass::Edit | ActionClass::Read);
    ApprovalSubject {
        action,
        tool: tool.into(),
        command: (action == ActionClass::Command)
            .then(|| input["command"].as_str().map(str::to_owned))
            .flatten(),
        paths: if is_path_tool {
            paths.iter().map(Into::into).collect()
        } else {
            Vec::new()
        },
        raw: serde_json::json!({
            "toolName": tool,
            "toolCallId": call_id,
            "parentToolCallId": null,
            "input": input,
        }),
    }
}

pub(super) fn gate_bash(call_id: &str, line: &str) -> ApprovalSubject {
    gate_call("bash", call_id, serde_json::json!({ "command": line }), &[])
}

pub(super) fn gate_edit(call_id: &str, path: &str) -> ApprovalSubject {
    gate_call(
        "edit",
        call_id,
        serde_json::json!({ "path": path, "edits": [] }),
        &[path],
    )
}

/// The two access modes a `reno` run can have, as a bot's definition says them.
pub(super) const MODES: [(&str, &str); 2] = [
    ("supervised", ""),
    ("full_access", "access = \"full_access\""),
];

fn reno_defs(policy_text: &str, bot_extra: &str) -> Vec<DefFile> {
    vec![
        file(
            DefKind::Bot,
            "keeper",
            &format!(
                "name = \"keeper\"\ninstructions = \"work\"\nexecutors = [\"reno\"]\n{bot_extra}\n"
            ),
        ),
        policy(policy_text),
    ]
}

/// A kernel whose bot `keeper` runs on `reno` (a mock under that name), with `bot_extra` added
/// to its definition.
pub(super) async fn reno_rig(policy_text: &str, bot_extra: &str, steps: Vec<MockStep>) -> Rig {
    let mock = MockExecutor::new(id("reno")).with_steps(steps);
    let port = FakePort::with(reno_defs(policy_text, bot_extra));
    let fx = Fx::with_definitions(
        config(1),
        registry(std::slice::from_ref(&mock)),
        setup(&port),
    )
    .await;
    Rig { fx, port, mock }
}

const GATE_POLICY: &str = r#"
default = "ask"
[[rules]]
note = "no force push"
effect = "deny"
match = { action = "command", command = "git push --force*" }
[[rules]]
note = "cargo is fine"
effect = "allow"
match = { action = "command", command = "cargo *" }
[[rules]]
note = "pushing needs the owner"
effect = "ask"
match = { action = "command", command = "git push*" }
[[rules]]
note = "the report waits for the owner"
effect = "ask"
match = { action = "mcp_tool", tool = "mcp__reno__report" }
"#;

#[derive(Clone, Copy, Debug)]
enum Want {
    /// Denied at once by the rule of this name, with this message.
    Refused(&'static str, &'static str),
    /// Allowed at once, by the rule of this name, with a journal pair.
    Allowed(&'static str),
    /// Allowed at once, on record in the approval history only.
    Recorded(&'static str),
    /// Held for the owner, who then approves it.
    Asked,
}

fn gate_rows() -> Vec<(&'static str, ApprovalSubject, Want, Want)> {
    let full_access = Want::Allowed("builtin:full-access");
    let rest = |name, subject| (name, subject, Want::Asked, full_access);
    vec![
        (
            "a policy deny",
            gate_bash("c1", "git push --force origin main"),
            Want::Refused("no force push", "denied by policy: no force push"),
            Want::Refused("no force push", "denied by policy: no force push"),
        ),
        (
            "a policy allow",
            gate_bash("c1", "cargo test"),
            Want::Allowed("cargo is fine"),
            Want::Allowed("cargo is fine"),
        ),
        (
            "an ask rule",
            gate_bash("c1", "git push origin main"),
            Want::Asked,
            Want::Asked,
        ),
        (
            "Reno's own report, even where an ask rule names it",
            gate_call(
                "mcp__reno__report",
                "c1",
                serde_json::json!({ "summary": "done" }),
                &[],
            ),
            Want::Recorded("builtin:reno-mcp"),
            Want::Recorded("builtin:reno-mcp"),
        ),
        rest("a command no rule names", gate_bash("c1", "ls -la")),
        rest(
            "a read",
            gate_call(
                "read",
                "c1",
                serde_json::json!({ "path": "README.md" }),
                &["README.md"],
            ),
        ),
        rest("an edit", gate_edit("c1", "src/lib.rs")),
        rest(
            "a tool of no known kind",
            gate_call(
                "web_search",
                "c1",
                serde_json::json!({ "query": "rust" }),
                &[],
            ),
        ),
        rest(
            "another server's tool",
            gate_call(
                "mcp__github__create_issue",
                "c1",
                serde_json::json!({}),
                &[],
            ),
        ),
        rest(
            "a look-alike of Reno's server",
            gate_call("mcp__reno__rm_everything", "c1", serde_json::json!({}), &[]),
        ),
    ]
}

#[tokio::test(start_paused = true)]
async fn the_ladder_answers_each_kind_of_gate_call_in_both_access_modes() {
    for (mode, extra) in MODES {
        for (name, subject, supervised, full_access) in gate_rows() {
            let want = if mode == "supervised" {
                supervised
            } else {
                full_access
            };
            let label = format!("{name} at {mode}");
            let rig = reno_rig(GATE_POLICY, extra, script(vec![("r1", subject.clone())])).await;
            let run = rig.run_as("keeper").await;
            let by_human = matches!(want, Want::Asked);
            if by_human {
                let pending = rig.pending().await;
                assert_eq!(pending.subject, subject, "{label}");
                rig.fx.wait_status(&run, RunStatus::WaitingApproval).await;
                rig.fx
                    .kernel
                    .decide(&pending.id, ApprovalDecision::ApproveOnce)
                    .await
                    .unwrap();
            }
            rig.fx.wait_status(&run, RunStatus::Succeeded).await;
            assert!(
                rig.fx.kernel.pending_approvals().await.unwrap().is_empty(),
                "{label}"
            );
            let journal = rig.fx.journal_with_done(&run).await;
            let history = rig.history(&run).await;
            assert_eq!(history.len(), 1, "{label}");
            assert_eq!(history[0].subject, subject, "{label}: kept verbatim");
            assert_eq!(history[0].request_id, "r1", "{label}");
            let kept = &history[0].subject.raw;
            assert_eq!(kept["toolCallId"], "c1", "{label}");
            match want {
                Want::Refused(by, message) => {
                    assert_eq!(rig.decisions(), [Decision::Deny], "{label}");
                    assert_eq!(
                        rig.mock.log().answers[0].answer,
                        Answer::Decision(Decision::Deny, Some(message.into())),
                        "{label}"
                    );
                    assert_eq!(
                        resolved(&journal),
                        [(
                            "r1".into(),
                            ApprovalOutcome::Denied,
                            rule(by),
                            Some(message.into())
                        )],
                        "{label}"
                    );
                    assert_eq!(history[0].status, ApprovalStatus::Denied, "{label}");
                    assert_eq!(history[0].decided_by, Some(rule(by)), "{label}");
                }
                Want::Allowed(by) => {
                    assert_eq!(rig.decisions(), [Decision::Allow], "{label}");
                    assert_eq!(
                        resolved(&journal),
                        [("r1".into(), ApprovalOutcome::Approved, rule(by), None)],
                        "{label}"
                    );
                    assert_eq!(history[0].decided_by, Some(rule(by)), "{label}");
                }
                Want::Recorded(by) => {
                    assert_eq!(rig.decisions(), [Decision::Allow], "{label}");
                    assert!(
                        !journal.iter().any(|(_, event)| matches!(
                            event,
                            ExecEvent::NeedsApproval { .. } | ExecEvent::ApprovalResolved { .. }
                        )),
                        "{label}: no journal pair: {journal:?}"
                    );
                    assert_eq!(history[0].status, ApprovalStatus::Approved, "{label}");
                    assert_eq!(history[0].decided_by, Some(rule(by)), "{label}");
                    assert_eq!(
                        history[0].subject_digest.as_deref(),
                        Some(subject.digest().as_str()),
                        "{label}"
                    );
                }
                Want::Asked => {
                    assert_eq!(rig.decisions(), [Decision::Allow], "{label}");
                    assert_eq!(
                        resolved(&journal),
                        [(
                            "r1".into(),
                            ApprovalOutcome::Approved,
                            ResolvedBy::Human,
                            None
                        )],
                        "{label}"
                    );
                }
            }
        }
    }
}

const ASK_EVERY_COMMAND: &str = r#"
default = "ask"
[[rules]]
note = "the owner decides commands"
effect = "ask"
match = { action = "command", command = "*" }
"#;

/// Every access mode keeps what an owner's refusal and a remembered answer decided.
mod remembered {
    use reno_proto::{Remember, RememberScope};

    use super::*;
    use crate::tests::until;

    fn remember_for_bot() -> Option<Remember> {
        Some(Remember {
            scope: RememberScope::Bot,
            expires_in_secs: None,
        })
    }

    #[tokio::test(start_paused = true)]
    async fn a_repeat_denial_refuses_a_retry_with_a_new_call_id_unasked() {
        for (mode, extra) in MODES {
            let rig = reno_rig(
                GATE_POLICY,
                extra,
                script(vec![
                    ("r1", gate_bash("c1", "git push origin main")),
                    ("r2", gate_bash("c2", "git push origin main")),
                ]),
            )
            .await;
            let run = rig.run_as("keeper").await;
            let pending = rig.pending().await;
            rig.fx
                .kernel
                .decide(&pending.id, ApprovalDecision::Deny)
                .await
                .unwrap();
            rig.fx.wait_status(&run, RunStatus::Succeeded).await;

            assert_eq!(rig.decisions(), [Decision::Deny, Decision::Deny], "{mode}");
            let journal = rig.fx.journal_with_done(&run).await;
            let by: Vec<_> = resolved(&journal).into_iter().map(|r| r.2).collect();
            assert_eq!(by, [ResolvedBy::Human, ResolvedBy::RepeatDenial], "{mode}");
            assert!(rig.fx.kernel.pending_approvals().await.unwrap().is_empty());
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_standing_deny_refuses_and_a_standing_allow_allows_in_a_later_run() {
        for (mode, extra) in MODES {
            for (decision, outcome, answered) in [
                (
                    ApprovalDecision::Deny,
                    ApprovalOutcome::Denied,
                    Decision::Deny,
                ),
                (
                    ApprovalDecision::ApproveOnce,
                    ApprovalOutcome::Approved,
                    Decision::Allow,
                ),
            ] {
                let rig = reno_rig(
                    GATE_POLICY,
                    extra,
                    script(vec![("r1", gate_bash("c1", "git push origin main"))]),
                )
                .await;
                let first = rig.run_as("keeper").await;
                let pending = rig.pending().await;
                rig.fx
                    .kernel
                    .decide_remembering(&pending.id, decision, None, remember_for_bot())
                    .await
                    .unwrap();
                rig.fx.wait_status(&first, RunStatus::Succeeded).await;
                let rules = rig.fx.kernel.list_rules(true).await.unwrap();
                assert_eq!(rules.len(), 1, "{mode}");
                let name = format!("standing:{}", rules[0].id);

                let later = rig.run_as("keeper").await;
                rig.fx.wait_status(&later, RunStatus::Succeeded).await;
                assert!(rig.fx.kernel.pending_approvals().await.unwrap().is_empty());
                assert_eq!(rig.decisions(), [answered, answered], "{mode}");
                let journal = rig.fx.journal_with_done(&later).await;
                let [(_, got, by, _)] = &resolved(&journal)[..] else {
                    panic!("{mode}: one resolution expected");
                };
                assert_eq!((*got, by), (outcome, &rule(&name)), "{mode}");
            }
        }
    }

    /// Remember the owner's answer to the one call, then load `later` as the policy and run again.
    async fn later_run(
        extra: &str,
        later: &str,
        subject: ApprovalSubject,
        decision: ApprovalDecision,
    ) -> (Rig, RunId) {
        let rig = reno_rig(ASK_EVERY_COMMAND, extra, script(vec![("r1", subject)])).await;
        let first = rig.run_as("keeper").await;
        let pending = rig.pending().await;
        rig.fx
            .kernel
            .decide_remembering(&pending.id, decision, None, remember_for_bot())
            .await
            .unwrap();
        rig.fx.wait_status(&first, RunStatus::Succeeded).await;
        let version = || {
            rig.fx
                .kernel
                .current_defs()
                .version_of(DefKind::Policy, "policy")
                .map(|v| v.version)
        };
        let before = version();
        rig.port.set(reno_defs(later, extra));
        rig.port.touch_and_wait().await;
        until(async || (version() > before).then_some(())).await;
        let again = rig.run_as("keeper").await;
        rig.fx.wait_status(&again, RunStatus::Succeeded).await;
        (rig, again)
    }

    #[tokio::test(start_paused = true)]
    async fn a_policy_deny_added_later_beats_a_standing_allow() {
        for (mode, extra) in MODES {
            let (rig, run) = later_run(
                extra,
                GATE_POLICY,
                gate_bash("c1", "git push --force origin main"),
                ApprovalDecision::ApproveOnce,
            )
            .await;
            let journal = rig.fx.journal_with_done(&run).await;
            let [(_, got, by, _)] = &resolved(&journal)[..] else {
                panic!("{mode}: one resolution expected");
            };
            assert_eq!(
                (*got, by),
                (ApprovalOutcome::Denied, &rule("no force push")),
                "{mode}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_standing_deny_beats_a_policy_allow_added_later() {
        for (mode, extra) in MODES {
            let (rig, run) = later_run(
                extra,
                GATE_POLICY,
                gate_bash("c1", "cargo test"),
                ApprovalDecision::Deny,
            )
            .await;
            let rules = rig.fx.kernel.list_rules(true).await.unwrap();
            let name = format!("standing:{}", rules[0].id);
            let journal = rig.fx.journal_with_done(&run).await;
            let [(_, got, by, _)] = &resolved(&journal)[..] else {
                panic!("{mode}: one resolution expected");
            };
            assert_eq!(
                (*got, by),
                (ApprovalOutcome::Denied, &rule(&name)),
                "{mode}"
            );
        }
    }
}

#[tokio::test(start_paused = true)]
async fn an_unanswered_gate_call_expires_as_denied_in_both_modes_and_stays_denied() {
    for (mode, extra) in MODES {
        let rig = reno_rig(
            GATE_POLICY,
            extra,
            script(vec![
                ("r1", gate_bash("c1", "git push origin main")),
                ("r2", gate_bash("c2", "git push origin main")),
            ]),
        )
        .await;
        let run = rig.run_as("keeper").await;
        let pending = rig.pending().await;
        rig.fx.wait_status(&run, RunStatus::WaitingApproval).await;
        assert!(rig.decisions().is_empty(), "{mode}: held");

        rig.fx.clock.advance(30 * 60 * 1000);
        advance(Duration::from_secs(30 * 60)).await;
        rig.fx.wait_status(&run, RunStatus::Succeeded).await;

        assert_eq!(rig.decisions(), [Decision::Deny, Decision::Deny], "{mode}");
        let history = rig.history(&run).await;
        let expired = history
            .iter()
            .find(|record| record.id == pending.id)
            .expect("the held request is on record");
        assert_eq!(expired.status, ApprovalStatus::Expired, "{mode}");
        assert_eq!(expired.decided_by, Some(ResolvedBy::Expired), "{mode}");
        let journal = rig.fx.journal_with_done(&run).await;
        let by: Vec<_> = resolved(&journal).into_iter().map(|r| r.2).collect();
        assert_eq!(
            by,
            [ResolvedBy::Expired, ResolvedBy::RepeatDenial],
            "{mode}"
        );
        assert!(rig.fx.kernel.pending_approvals().await.unwrap().is_empty());
    }
}

/// The answer the harness got, and the decided approvals on record at that moment.
type Told = (Answer, Vec<reno_store::StoredApproval>);

/// An executor that asks once and, when it is told the answer, looks at the store: what the
/// approval history held at the moment the harness was told.
///
/// The store answers a read after the write queued before it, so this shows the decision whole on
/// record when Pi is told; it cannot tell a write that lands a moment after the answer.
struct Observing {
    base: MockExecutor,
    store: Arc<std::sync::OnceLock<Store>>,
    subject: ApprovalSubject,
    seen: Arc<std::sync::Mutex<Vec<Told>>>,
}

#[async_trait::async_trait]
impl reno_harness::exec::Executor for Observing {
    fn descriptor(&self) -> reno_proto::ExecutorDescriptor {
        self.base.descriptor()
    }

    async fn probe(&self) -> reno_proto::HealthProbe {
        self.base.probe().await
    }

    async fn start(
        &self,
        _spec: reno_proto::ExecSpec,
        controls: reno_harness::exec::ExecControls,
    ) -> Result<futures::stream::BoxStream<'static, ExecEvent>, reno_harness::exec::ExecError> {
        use futures::StreamExt as _;
        let (store, seen, subject) = (
            Arc::clone(&self.store),
            Arc::clone(&self.seen),
            self.subject.clone(),
        );
        Ok(
            futures::stream::unfold((0_u8, controls), move |(step, mut controls)| {
                let (store, seen, subject) = (store.clone(), seen.clone(), subject.clone());
                async move {
                    let event = match step {
                        0 => ExecEvent::Started {
                            session: reno_proto::SessionRef("s".into()),
                            model: "mock-1".into(),
                        },
                        1 => ExecEvent::NeedsApproval {
                            request_id: "r1".into(),
                            subject,
                        },
                        2 => {
                            let answer = controls.answers.recv().await?;
                            let held = store
                                .get()
                                .expect("the store is set before a run starts")
                                .approval_history(None, 10)
                                .await
                                .unwrap();
                            seen.lock().unwrap().push((answer.answer, held));
                            ExecEvent::Text {
                                text: "after".into(),
                            }
                        }
                        3 => ExecEvent::Done {
                            stop: reno_proto::StopReason::Completed,
                            result: None,
                            diagnostic: None,
                            reason: None,
                        },
                        _ => return None,
                    };
                    Some((event, (step + 1, controls)))
                }
            })
            .boxed(),
        )
    }
}

#[tokio::test(start_paused = true)]
async fn the_decision_is_on_record_before_the_harness_is_told_and_then_journaled() {
    // How each decision comes about: a rule at once, the owner, the deadline.
    for how in ["policy deny", "policy allow", "owner", "deadline"] {
        let (subject, policy_text) = match how {
            "policy deny" => (gate_bash("c1", "git push --force origin main"), GATE_POLICY),
            "policy allow" => (gate_bash("c1", "cargo test"), GATE_POLICY),
            _ => (gate_bash("c1", "git push origin main"), GATE_POLICY),
        };
        let store = Arc::new(std::sync::OnceLock::new());
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let observing = Observing {
            base: MockExecutor::new(id("reno")),
            store: Arc::clone(&store),
            subject: subject.clone(),
            seen: Arc::clone(&seen),
        };
        let port = FakePort::with(reno_defs(policy_text, ""));
        let fx = Fx::with_definitions(
            config(1),
            ExecutorRegistry::new().with(Arc::new(observing)),
            setup(&port),
        )
        .await;
        store.set(fx.store.clone()).unwrap();
        let run = fx
            .kernel
            .submit_for_bot("keeper", "go", None, WORKTREE.into(), None)
            .await
            .unwrap();
        match how {
            "owner" => {
                let held = until(async || {
                    fx.kernel
                        .pending_approvals()
                        .await
                        .unwrap()
                        .into_iter()
                        .next()
                })
                .await;
                fx.kernel
                    .decide(&held.id, ApprovalDecision::ApproveOnce)
                    .await
                    .unwrap();
            }
            "deadline" => {
                until(async || {
                    (!fx.kernel.pending_approvals().await.unwrap().is_empty()).then_some(())
                })
                .await;
                fx.clock.advance(30 * 60 * 1000);
                advance(Duration::from_secs(30 * 60)).await;
            }
            _ => {}
        }
        fx.wait_status(&run, RunStatus::Succeeded).await;

        let seen = seen.lock().unwrap().clone();
        let [(answer, on_record)] = &seen[..] else {
            panic!("{how}: the harness was told once: {seen:?}");
        };
        let [row] = &on_record[..] else {
            panic!("{how}: the decision was not on record when the harness was told");
        };
        assert_eq!(row.request_id, "r1", "{how}");
        assert_eq!(row.subject, subject, "{how}");
        let (status, by, told, message) = match how {
            "policy deny" => (
                ApprovalStatus::Denied,
                rule("no force push"),
                Decision::Deny,
                Some("denied by policy: no force push"),
            ),
            "policy allow" => (
                ApprovalStatus::Approved,
                rule("cargo is fine"),
                Decision::Allow,
                None,
            ),
            "owner" => (
                ApprovalStatus::Approved,
                ResolvedBy::Human,
                Decision::Allow,
                None,
            ),
            _ => (
                ApprovalStatus::Expired,
                ResolvedBy::Expired,
                Decision::Deny,
                None,
            ),
        };
        assert_eq!(row.status, status, "{how}");
        assert_eq!(row.decided_by, Some(by.clone()), "{how}");
        assert_eq!(
            *answer,
            Answer::Decision(told, message.map(str::to_owned)),
            "{how}"
        );

        // The journal has the request, then its resolution, then the harness's next event.
        let journal = fx.journal_with_done(&run).await;
        let kinds: Vec<&str> = journal
            .iter()
            .map(|(_, event)| match event {
                ExecEvent::Started { .. } => "started",
                ExecEvent::NeedsApproval { .. } => "needs_approval",
                ExecEvent::ApprovalResolved { .. } => "resolved",
                ExecEvent::Text { .. } => "text",
                ExecEvent::Done { .. } => "done",
                other => panic!("{how}: {other:?}"),
            })
            .collect();
        assert_eq!(
            kinds,
            ["started", "needs_approval", "resolved", "text", "done"],
            "{how}"
        );
        assert_eq!(resolved(&journal)[0].2, by, "{how}");
    }
}

#[tokio::test(start_paused = true)]
async fn a_bots_max_wall_time_still_stops_a_reno_run_held_on_a_question() {
    use crate::db::Armed;

    for (mode, extra) in MODES {
        let extra = format!("{extra}\n[budget]\nmax_wall_time = \"1m\"");
        let rig = reno_rig(
            GATE_POLICY,
            &extra,
            script(vec![("r1", gate_bash("c1", "git push origin main"))]),
        )
        .await;
        let run = rig.run_as("keeper").await;
        rig.pending().await;
        rig.fx.wait_status(&run, RunStatus::WaitingApproval).await;
        rig.fx.wait_armed(Armed::WallTime, 0).await;

        advance(Duration::from_secs(59)).await;
        for _ in 0..100 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            rig.fx.status(&run).await,
            RunStatus::WaitingApproval,
            "{mode}: not yet"
        );
        advance(Duration::from_secs(2)).await;
        rig.fx.wait_status(&run, RunStatus::Interrupted).await;

        let journal = rig.fx.journal_with_done(&run).await;
        let Some((_, ExecEvent::Done { stop, reason, .. })) = journal.last() else {
            panic!("{mode}: no Done: {journal:?}");
        };
        assert_eq!(*stop, StopReason::Interrupted, "{mode}");
        let budget = reason.as_ref().and_then(reno_proto::StopDetail::budget);
        let budget = budget.expect("the reason is a budget stop");
        assert_eq!(
            (budget.kind, budget.limit),
            (reno_proto::BudgetKind::WallTime, 60_000),
            "{mode}"
        );
        assert!(
            rig.fx.kernel.pending_approvals().await.unwrap().is_empty(),
            "{mode}: the held question is closed with the run"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_reno_run_with_no_bot_has_the_kernels_default_wall_time() {
    use crate::db::Armed;

    let cfg = KernelConfig {
        default_wall_time: Duration::from_secs(30),
        ..config(1)
    };
    let mock =
        MockExecutor::new(id("reno")).with_steps([MockStep::Sleep(Duration::from_secs(3600))]);
    let fx = Fx::new(cfg, registry(std::slice::from_ref(&mock))).await;
    let run = fx
        .kernel
        .submit("long", id("reno"), WORKTREE.into(), None)
        .await
        .unwrap();
    fx.wait_status(&run, RunStatus::Running).await;
    fx.wait_armed(Armed::WallTime, 0).await;
    advance(Duration::from_secs(31)).await;
    fx.wait_status(&run, RunStatus::Interrupted).await;
    let journal = fx.journal_with_done(&run).await;
    let Some((_, ExecEvent::Done { reason, .. })) = journal.last() else {
        panic!("no Done: {journal:?}");
    };
    let budget = reason
        .as_ref()
        .and_then(reno_proto::StopDetail::budget)
        .unwrap();
    assert_eq!(budget.limit, 30_000);
}
