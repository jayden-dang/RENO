#![cfg(test)]

//! Run tokens and Reno's own MCP tools through the kernel (T1.9): the token is minted when a run
//! starts and dies with it, a run reaches only its own scope, and `request_approval` is an
//! ordinary approval. The `RunLink` is the in-memory fake; the daemon's real one is tested in
//! `renod`.

use std::sync::Arc;

use super::defs::{FakePort, file, setup};
use super::{Fx, START_MS, config, id, mock, registry, until};
use crate::{AgentSession, FakeRunLink, KernelConfig, KernelError};
use reno_harness::exec::{MockExecutor, MockStep};
use reno_proto::defs::{DefFile, DefKind};
use reno_proto::{
    ActionClass, AgentApprovalParams, AgentReportParams, ApprovalDecision, ApprovalOutcome,
    ApprovalStatus, ApprovalSubject, BOARD_MAX_KEYS, BOARD_VALUE_MAX_BYTES, ExecEvent,
    ExecutorCaps, MCP_SERVER_NAME, ReportStatus, ResolvedBy, RunId, RunStatus, token_digest,
};

fn bot(name: &str) -> DefFile {
    file(
        DefKind::Bot,
        name,
        &format!("name = \"{name}\"\ninstructions = \"work\"\nexecutors = [\"mock\"]\n"),
    )
}

/// Started, then waits to be steered, then one more text: a run that stays live until told.
fn hold() -> Vec<MockStep> {
    vec![
        MockStep::started("s", "mock-1"),
        MockStep::AwaitSteer,
        MockStep::text("after"),
    ]
}

struct Rig {
    fx: Fx,
    link: Arc<FakeRunLink>,
    mock: MockExecutor,
}

impl Rig {
    async fn new(steps: Vec<MockStep>, cfg: KernelConfig) -> Self {
        Self::with_policy(steps, cfg, None).await
    }

    async fn with_policy(steps: Vec<MockStep>, cfg: KernelConfig, policy: Option<&str>) -> Self {
        let mock = mock(steps);
        let link = Arc::new(FakeRunLink::new());
        let mut files = vec![bot("keeper"), bot("scout")];
        if let Some(text) = policy {
            files.push(file(DefKind::Policy, "policy", text));
        }
        let port = FakePort::with(files);
        let fx = Fx::with_link(
            cfg,
            registry(std::slice::from_ref(&mock)),
            Some(setup(&port)),
            link.clone(),
        )
        .await;
        Self { fx, link, mock }
    }

    /// Waits until the mock has been started `n` times.
    async fn started(&self, n: usize) {
        until(async || (self.mock.log().specs.len() >= n).then_some(())).await;
    }

    /// A run of `bot` that has started, and a session for it.
    async fn live(&self, bot: Option<&str>) -> (RunId, AgentSession) {
        let before = self.mock.log().specs.len();
        let run = match bot {
            Some(bot) => self
                .fx
                .kernel
                .submit_for_bot(bot, "go", None, "/work".into(), None)
                .await
                .unwrap(),
            None => self.fx.submit("go").await,
        };
        self.started(before + 1).await;
        let session = self.session(&run).await.unwrap();
        (run, session)
    }

    fn digest(&self, run: &RunId) -> String {
        token_digest(&self.link.token_for(run).expect("a token was issued"))
    }

    async fn session(&self, run: &RunId) -> Result<AgentSession, KernelError> {
        self.fx
            .kernel
            .agent_authenticate(run, &self.digest(run))
            .await
    }

    async fn finish(&self, run: &RunId) {
        self.fx.kernel.steer(run, "go on").await.unwrap();
        self.fx.wait_status(run, RunStatus::Succeeded).await;
    }
}

fn approval(command: &str) -> AgentApprovalParams {
    AgentApprovalParams {
        action: ActionClass::Command,
        reason: "to publish".into(),
        command: Some(command.into()),
        paths: vec!["dist/".into()],
    }
}

#[tokio::test(start_paused = true)]
async fn a_run_starts_with_reno_first_and_a_token_that_works_until_the_run_ends() {
    let rig = Rig::new(hold(), config(1)).await;
    let (run, session) = rig.live(None).await;

    let spec = rig.mock.log().specs.remove(0);
    assert_eq!(spec.mcp[0].name, MCP_SERVER_NAME);
    assert_eq!(spec.mcp[0].env["RENO_RUN_ID"], run.to_string());
    // The environment names the file, never the token.
    let token = rig.link.token_for(&run).unwrap();
    assert!(!format!("{spec:?}").contains(&token));
    assert_eq!(session.run(), &run);
    assert_eq!(session.scope(), format!("run:{run}"));

    rig.finish(&run).await;
    assert!(matches!(
        rig.fx
            .kernel
            .agent_authenticate(&run, &token_digest(&token))
            .await,
        Err(KernelError::AgentDenied)
    ));
    // The link's file is gone too.
    assert!(rig.link.token_for(&run).is_none());
    // And a call made with the session from before the end is refused.
    assert!(matches!(
        rig.fx.kernel.agent_board_read(&session, None).await,
        Err(KernelError::AgentDenied)
    ));
}

#[tokio::test(start_paused = true)]
async fn a_token_is_refused_for_another_run_and_when_it_is_wrong() {
    let rig = Rig::new(hold(), config(2)).await;
    let (first, _) = rig.live(None).await;
    let (second, _) = rig.live(None).await;

    let kernel = &rig.fx.kernel;
    let denied =
        |result: Result<AgentSession, KernelError>| matches!(result, Err(KernelError::AgentDenied));
    // The first run's token under the second run's id.
    assert!(denied(
        kernel
            .agent_authenticate(&second, &rig.digest(&first))
            .await
    ));
    assert!(denied(
        kernel
            .agent_authenticate(&first, &token_digest("not-a-token"))
            .await
    ));
    // A run that does not exist.
    let missing = RunId::from_suffix("nope").unwrap();
    assert!(denied(
        kernel
            .agent_authenticate(&missing, &rig.digest(&first))
            .await
    ));
    assert!(rig.session(&first).await.is_ok());
}

const HOUR_MS: i64 = 60 * 60 * 1000;

#[tokio::test(start_paused = true)]
async fn a_token_outlives_a_wall_clock_jump_while_its_run_is_live_and_dies_with_the_run() {
    let rig = Rig::new(hold(), config(1)).await;
    let (run, session) = rig.live(None).await;
    let token = rig.link.token_for(&run).unwrap();
    assert!(rig.fx.kernel.agent_board_read(&session, None).await.is_ok());

    // The machine sleeps: the wall clock is far past the run's wall time, its cancel grace and
    // the old minute of slack, and the run is still live (its budget counts awake time).
    rig.fx.clock.advance(5 * HOUR_MS);
    assert!(rig.fx.kernel.agent_board_read(&session, None).await.is_ok());
    assert!(
        rig.fx
            .kernel
            .agent_authenticate(&run, &token_digest(&token))
            .await
            .is_ok()
    );

    // Revocation at the run's end is the bound.
    rig.finish(&run).await;
    assert!(matches!(
        rig.fx.kernel.agent_board_read(&session, None).await,
        Err(KernelError::AgentDenied)
    ));
}

#[tokio::test(start_paused = true)]
async fn a_tokens_wall_clock_expiry_is_a_generous_safety_net_not_the_runs_budget() {
    let rig = Rig::new(hold(), config(1)).await;
    let (run, session) = rig.live(None).await;
    let stored = rig
        .fx
        .store
        .run_token(&run, &rig.digest(&run))
        .await
        .unwrap()
        .unwrap();
    // The default wall time is 4 h: four times that is under a day, so the floor of 24 h holds.
    assert_eq!(stored.expires_at, START_MS + 24 * HOUR_MS);

    rig.fx.clock.advance(24 * HOUR_MS - 1);
    assert!(rig.fx.kernel.agent_board_read(&session, None).await.is_ok());
    rig.fx.clock.advance(1);
    assert!(matches!(
        rig.fx.kernel.agent_board_read(&session, None).await,
        Err(KernelError::AgentDenied)
    ));
}

#[tokio::test(start_paused = true)]
async fn a_longer_wall_time_budget_stretches_the_safety_net_to_four_times_it() {
    let cfg = KernelConfig {
        default_wall_time: std::time::Duration::from_secs(10 * 60 * 60),
        ..config(1)
    };
    let rig = Rig::new(hold(), cfg).await;
    let (run, _) = rig.live(None).await;
    let stored = rig
        .fx
        .store
        .run_token(&run, &rig.digest(&run))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.expires_at, START_MS + 40 * HOUR_MS);
}

#[tokio::test(start_paused = true)]
async fn a_token_for_a_run_the_kernel_does_not_hold_is_refused_and_start_up_revokes_it() {
    let rig = Rig::new(hold(), config(1)).await;
    let (run, _) = rig.live(None).await;
    let digest = rig.digest(&run);

    // The daemon dies with the run live: the store keeps the open token and a `running` row.
    let (dir, store) = rig.fx.into_store();
    let open = store.run_token(&run, &digest).await.unwrap().unwrap();
    assert_eq!(open.revoked_at, None);

    // Back up a day and a half later: no run is live in the new kernel, so the token is refused,
    // and start-up revokes it whatever its expiry says.
    let second = Fx::over_link(
        dir,
        store,
        config(1),
        registry(&[mock(hold())]),
        None,
        Arc::new(FakeRunLink::new()),
        START_MS + 36 * HOUR_MS,
    )
    .await;
    assert!(matches!(
        second.kernel.agent_authenticate(&run, &digest).await,
        Err(KernelError::AgentDenied)
    ));
    let revoked = second
        .store
        .run_token(&run, &digest)
        .await
        .unwrap()
        .unwrap();
    assert!(revoked.revoked_at.is_some(), "start-up revoked it");
}

#[tokio::test(start_paused = true)]
async fn a_run_that_fails_to_start_leaves_no_token() {
    let rig = Rig::new(hold(), config(1)).await;
    let run = rig
        .fx
        .kernel
        .submit("go", id("missing"), "/work".into(), None)
        .await
        .unwrap();
    rig.fx.wait_status(&run, RunStatus::Failed).await;
    assert!(rig.link.token_for(&run).is_none());
}

#[tokio::test(start_paused = true)]
async fn a_link_that_cannot_be_issued_fails_the_run_instead_of_starting_it_without() {
    let rig = Rig::new(hold(), config(1)).await;
    rig.link.fail_issuing(true);
    let run = rig.fx.submit("go").await;
    rig.fx.wait_status(&run, RunStatus::Failed).await;
    assert!(rig.mock.log().specs.is_empty());
    let journal = rig.fx.journal_with_done(&run).await;
    let Some((_, ExecEvent::Done { diagnostic, .. })) = journal.last() else {
        panic!("no Done");
    };
    assert!(diagnostic.as_deref().unwrap().starts_with("McpLinkFailed"));
}

#[tokio::test(start_paused = true)]
async fn an_executor_that_cannot_inject_mcp_servers_gets_none() {
    let link = Arc::new(FakeRunLink::new());
    let mut descriptor = MockExecutor::default_descriptor(id("mock"));
    descriptor.caps = ExecutorCaps {
        mcp_injection: false,
        ..descriptor.caps
    };
    let mock = mock(vec![MockStep::started("s", "mock-1")]).with_descriptor(descriptor);
    let fx = Fx::with_link(config(1), registry(std::slice::from_ref(&mock)), None, link).await;
    let run = fx.submit("go").await;
    fx.wait_status(&run, RunStatus::Succeeded).await;
    assert!(mock.log().specs[0].mcp.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_kernel_with_no_link_injects_nothing() {
    let mock = mock(vec![MockStep::started("s", "mock-1")]);
    let fx = Fx::new(config(1), registry(std::slice::from_ref(&mock))).await;
    let run = fx.submit("go").await;
    fx.wait_status(&run, RunStatus::Succeeded).await;
    assert!(mock.log().specs[0].mcp.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_restart_revokes_the_tokens_the_last_daemon_left_and_clears_the_link() {
    let rig = Rig::new(hold(), config(1)).await;
    let (run, _) = rig.live(None).await;
    let digest = rig.digest(&run);

    // The daemon dies with the run live: the store keeps the open token.
    let (dir, store) = rig.fx.into_store();
    let link = Arc::new(FakeRunLink::new());
    let second = Fx::over_link(
        dir,
        store,
        config(1),
        registry(&[mock(hold())]),
        None,
        link.clone(),
        START_MS + 1_000,
    )
    .await;
    assert!(matches!(
        second.kernel.agent_authenticate(&run, &digest).await,
        Err(KernelError::AgentDenied)
    ));
    assert_eq!(link.clears(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_report_is_journaled_as_the_runs_own_event() {
    let rig = Rig::new(hold(), config(1)).await;
    let (run, session) = rig.live(None).await;
    rig.fx
        .kernel
        .agent_report(
            &session,
            AgentReportParams {
                status: ReportStatus::Blocked,
                summary: "need the staging key".into(),
                details: Some("tried the vault".into()),
            },
        )
        .await
        .unwrap();
    rig.finish(&run).await;
    let journal = rig.fx.journal_with_done(&run).await;
    assert!(journal.iter().any(|(_, event)| matches!(
        event,
        ExecEvent::AgentReport { status: ReportStatus::Blocked, summary, details }
            if summary == "need the staging key" && details.as_deref() == Some("tried the vault")
    )));

    // Over the limit is refused, not cut.
    let (_, session) = rig.live(None).await;
    let long = "x".repeat(reno_proto::REPORT_SUMMARY_MAX_CHARS + 1);
    let error = rig
        .fx
        .kernel
        .agent_report(
            &session,
            AgentReportParams {
                status: ReportStatus::Done,
                summary: long,
                details: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(error, KernelError::Invalid(_)));
}

#[tokio::test(start_paused = true)]
async fn a_runs_summary_carries_the_agents_last_report_beside_its_status() {
    let rig = Rig::new(hold(), config(1)).await;
    let (run, session) = rig.live(None).await;
    assert_eq!(rig.fx.kernel.get_run(&run).await.unwrap().report, None);
    for (status, summary) in [
        (ReportStatus::Progress, "halfway"),
        (ReportStatus::Failed, "tests failed, Cargo.lock reverted"),
    ] {
        rig.fx
            .kernel
            .agent_report(
                &session,
                AgentReportParams {
                    status,
                    summary: summary.into(),
                    details: Some("long details stay in the journal".into()),
                },
            )
            .await
            .unwrap();
    }
    rig.finish(&run).await;
    // The run's own status is untouched; the report is beside it.
    let summary = rig.fx.kernel.get_run(&run).await.unwrap();
    assert_eq!(summary.status, RunStatus::Succeeded);
    assert_eq!(
        summary.report,
        Some(reno_proto::RunReport {
            status: ReportStatus::Failed,
            summary: "tests failed, Cargo.lock reverted".into(),
        })
    );
    // The list carries it too.
    let (page, _) = rig.fx.kernel.list_runs(None, None, None).await.unwrap();
    assert_eq!(page[0].report, summary.report);
}

#[tokio::test(start_paused = true)]
async fn a_bot_less_run_has_its_own_scope_and_a_bots_runs_share_theirs() {
    let rig = Rig::new(hold(), config(3)).await;
    let (first_run, first) = rig.live(Some("keeper")).await;
    let (_, scout) = rig.live(Some("scout")).await;
    let (_, nobody) = rig.live(None).await;
    let kernel = &rig.fx.kernel;

    assert_eq!(first.scope(), "bot:keeper");
    assert_eq!(scout.scope(), "bot:scout");
    assert!(nobody.scope().starts_with("run:"));

    let (scope, version) = kernel
        .agent_board_write(&first, "plan", "ship it", None)
        .await
        .unwrap();
    assert_eq!((scope.as_str(), version), ("bot:keeper", 1));

    // A later run of the same bot reads it (a bot has one run at a time); another bot and a
    // bot-less run do not.
    rig.finish(&first_run).await;
    let (_, second) = rig.live(Some("keeper")).await;
    let (_, read) = kernel
        .agent_board_read(&second, Some("plan"))
        .await
        .unwrap();
    assert_eq!(read[0].value.as_deref(), Some("ship it"));
    for outsider in [&scout, &nobody] {
        let (_, read) = kernel
            .agent_board_read(outsider, Some("plan"))
            .await
            .unwrap();
        assert!(read.is_empty(), "{:?} saw bot keeper's board", outsider);
        let (_, listing) = kernel.agent_board_read(outsider, None).await.unwrap();
        assert!(listing.is_empty());
    }
}

#[tokio::test(start_paused = true)]
async fn a_listing_has_no_values_and_a_stale_write_answers_with_the_current_entry() {
    let rig = Rig::new(hold(), config(1)).await;
    let (_, session) = rig.live(Some("keeper")).await;
    let kernel = &rig.fx.kernel;

    kernel
        .agent_board_write(&session, "a", "one", Some(0))
        .await
        .unwrap();
    let (_, listing) = kernel.agent_board_read(&session, None).await.unwrap();
    assert_eq!(listing.len(), 1);
    assert_eq!(
        (
            listing[0].key.as_str(),
            listing[0].value.as_deref(),
            listing[0].version
        ),
        ("a", None, 1)
    );

    let error = kernel
        .agent_board_write(&session, "a", "two", Some(0))
        .await
        .unwrap_err();
    let KernelError::BoardStale { current } = error else {
        panic!("not stale: {error:?}");
    };
    let current = current.expect("the entry exists");
    assert_eq!(
        (current.value.as_deref(), current.version),
        (Some("one"), 1)
    );
    let (_, version) = kernel
        .agent_board_write(&session, "a", "two", Some(1))
        .await
        .unwrap();
    assert_eq!(version, 2);
}

#[tokio::test(start_paused = true)]
async fn board_limits_are_typed_errors() {
    let rig = Rig::new(hold(), config(1)).await;
    let (_, session) = rig.live(Some("keeper")).await;
    let kernel = &rig.fx.kernel;

    let big = "x".repeat(BOARD_VALUE_MAX_BYTES + 1);
    assert!(matches!(
        kernel.agent_board_write(&session, "k", &big, None).await,
        Err(KernelError::Invalid(_))
    ));
    assert!(matches!(
        kernel.agent_board_write(&session, "", "v", None).await,
        Err(KernelError::Invalid(_))
    ));
    for n in 0..BOARD_MAX_KEYS {
        kernel
            .agent_board_write(&session, &format!("k{n}"), "v", None)
            .await
            .unwrap();
    }
    assert!(matches!(
        kernel.agent_board_write(&session, "more", "v", None).await,
        Err(KernelError::BoardFull { .. })
    ));
}

/// Starts `request_approval` in the background and returns its handle.
fn ask(
    rig: &Rig,
    session: &AgentSession,
    params: AgentApprovalParams,
) -> tokio::task::JoinHandle<Result<reno_proto::AgentApprovalResult, KernelError>> {
    let kernel = rig.fx.kernel.clone();
    let session = session.clone();
    tokio::spawn(async move { kernel.agent_request_approval(&session, params).await })
}

async fn pending(rig: &Rig) -> reno_proto::Approval {
    until(async || {
        rig.fx
            .kernel
            .pending_approvals()
            .await
            .unwrap()
            .into_iter()
            .next()
    })
    .await
}

#[tokio::test(start_paused = true)]
async fn an_approved_request_unblocks_the_call_and_is_on_record_with_its_subject() {
    let rig = Rig::new(hold(), config(1)).await;
    let (run, session) = rig.live(None).await;
    let waiting = ask(&rig, &session, approval("npm publish"));

    let held = pending(&rig).await;
    assert_eq!(held.run_id, run);
    assert_eq!(held.subject.action, ActionClass::Command);
    assert_eq!(held.subject.command.as_deref(), Some("npm publish"));
    assert_eq!(held.subject.paths, [std::path::PathBuf::from("dist/")]);
    assert_eq!(held.subject.raw["reason"], "to publish");
    assert_eq!(rig.fx.status(&run).await, RunStatus::WaitingApproval);
    assert!(!waiting.is_finished());

    rig.fx
        .kernel
        .decide(&held.id, ApprovalDecision::ApproveOnce)
        .await
        .unwrap();
    let answer = waiting.await.unwrap().unwrap();
    assert_eq!(answer.decision, ApprovalOutcome::Approved);
    assert_eq!(answer.by, ResolvedBy::Human);
    rig.fx.wait_status(&run, RunStatus::Running).await;

    let history = rig
        .fx
        .kernel
        .approval_history(Some(&run), None)
        .await
        .unwrap();
    assert_eq!(history[0].status, ApprovalStatus::Approved);
    // The journal shows the request and its answer, like any approval.
    rig.finish(&run).await;
    let journal = rig.fx.journal_with_done(&run).await;
    let seen: Vec<&str> = journal
        .iter()
        .filter_map(|(_, event)| match event {
            ExecEvent::NeedsApproval { .. } => Some("asked"),
            ExecEvent::ApprovalResolved { .. } => Some("resolved"),
            _ => None,
        })
        .collect();
    assert_eq!(seen, ["asked", "resolved"]);
}

#[tokio::test(start_paused = true)]
async fn a_denied_request_carries_the_reason_and_the_same_request_is_then_refused_unasked() {
    let rig = Rig::new(hold(), config(1)).await;
    let (_, session) = rig.live(None).await;
    let waiting = ask(&rig, &session, approval("npm publish"));
    let held = pending(&rig).await;
    rig.fx
        .kernel
        .decide_with_message(&held.id, ApprovalDecision::Deny, Some("not today".into()))
        .await
        .unwrap();
    let answer = waiting.await.unwrap().unwrap();
    assert_eq!(answer.decision, ApprovalOutcome::Denied);
    assert_eq!(answer.message.as_deref(), Some("not today"));

    // Asked again: refused at once, with nobody asked.
    let again = rig
        .fx
        .kernel
        .agent_request_approval(&session, approval("npm publish"))
        .await
        .unwrap();
    assert_eq!(again.decision, ApprovalOutcome::Denied);
    assert_eq!(again.by, ResolvedBy::RepeatDenial);
    assert!(rig.fx.kernel.pending_approvals().await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_unanswered_request_expires_as_a_denial() {
    let rig = Rig::new(hold(), config(1)).await;
    let (_, session) = rig.live(None).await;
    let waiting = ask(&rig, &session, approval("npm publish"));
    pending(&rig).await;

    tokio::time::advance(KernelConfig::default().approval_ttl + std::time::Duration::from_secs(1))
        .await;
    let answer = waiting.await.unwrap().unwrap();
    assert_eq!(answer.decision, ApprovalOutcome::Denied);
    assert_eq!(answer.by, ResolvedBy::Expired);
    assert!(rig.fx.kernel.pending_approvals().await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_request_still_waiting_when_the_run_ends_is_denied_as_expired() {
    let rig = Rig::new(hold(), config(1)).await;
    let (run, session) = rig.live(None).await;
    let waiting = ask(&rig, &session, approval("npm publish"));
    pending(&rig).await;

    rig.fx.kernel.cancel(&run).await.unwrap();
    // Either the denial the run's end writes, or the call learning the run is over: never a
    // approval.
    match waiting.await.unwrap() {
        Ok(answer) => assert_eq!(answer.decision, ApprovalOutcome::Denied),
        Err(error) => assert!(matches!(
            error,
            KernelError::RunEnded(_) | KernelError::AgentDenied
        )),
    }
    assert!(rig.fx.kernel.pending_approvals().await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_approval_the_run_asked_for_itself_cannot_be_kept_as_a_standing_rule() {
    use reno_proto::{Remember, RememberScope};
    let rig = Rig::new(hold(), config(1)).await;
    let (_, session) = rig.live(None).await;
    let asked = ask(&rig, &session, approval("git push"));
    let held = pending(&rig).await;
    let refused = rig
        .fx
        .kernel
        .decide_remembering(
            &held.id,
            ApprovalDecision::ApproveOnce,
            None,
            Some(Remember {
                scope: RememberScope::Always,
                expires_in_secs: None,
            }),
        )
        .await;
    assert!(
        matches!(refused, Err(KernelError::Invalid(_))),
        "{refused:?}"
    );
    assert!(rig.fx.kernel.list_rules(true).await.unwrap().is_empty());
    // It is still held, and a plain answer goes through.
    assert_eq!(pending(&rig).await.id, held.id);
    rig.fx
        .kernel
        .decide(&held.id, ApprovalDecision::ApproveOnce)
        .await
        .unwrap();
    assert_eq!(
        asked.await.unwrap().unwrap().decision,
        ApprovalOutcome::Approved
    );
}

#[tokio::test(start_paused = true)]
async fn a_policy_rule_answers_a_request_without_a_human() {
    let policy = r#"
default = "ask"
[[rules]]
note = "tests are fine"
effect = "allow"
match = { action = "command", command = "cargo test*" }
[[rules]]
note = "no force pushes"
effect = "deny"
match = { action = "command", command = "git push --force*" }
"#;
    let rig = Rig::with_policy(hold(), config(1), Some(policy)).await;
    let (_, session) = rig.live(None).await;
    let kernel = &rig.fx.kernel;

    let allowed = kernel
        .agent_request_approval(&session, approval("cargo test"))
        .await
        .unwrap();
    assert_eq!(allowed.decision, ApprovalOutcome::Approved);
    assert_eq!(
        allowed.by,
        ResolvedBy::Rule {
            name: "tests are fine".into()
        }
    );
    let denied = kernel
        .agent_request_approval(&session, approval("git push --force origin main"))
        .await
        .unwrap();
    assert_eq!(denied.decision, ApprovalOutcome::Denied);
    assert_eq!(
        denied.by,
        ResolvedBy::Rule {
            name: "no force pushes".into()
        }
    );
    assert!(denied.message.is_some());
    assert!(kernel.pending_approvals().await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn too_many_requests_at_once_are_refused_not_queued_without_bound() {
    let rig = Rig::new(hold(), config(1)).await;
    let (_, session) = rig.live(None).await;
    let mut waiting = Vec::new();
    for n in 0..8 {
        waiting.push(ask(&rig, &session, approval(&format!("job {n}"))));
    }
    until(async || (rig.fx.kernel.pending_approvals().await.unwrap().len() == 8).then_some(()))
        .await;
    let ninth = rig
        .fx
        .kernel
        .agent_request_approval(&session, approval("job 9"))
        .await
        .unwrap();
    assert_eq!(ninth.decision, ApprovalOutcome::Denied);
    for handle in waiting {
        handle.abort();
    }
}

// ----- Reno's own tools are not asked about twice.

fn mcp_call(tool: &str) -> ApprovalSubject {
    ApprovalSubject {
        action: ActionClass::McpTool,
        tool: tool.into(),
        command: None,
        paths: Vec::new(),
        raw: serde_json::Value::Null,
    }
}

/// One `NeedsApproval` for `subject`, answered by the kernel, then done.
async fn judged(subject: ApprovalSubject, policy: Option<&str>) -> (ApprovalOutcome, ResolvedBy) {
    let steps = vec![
        MockStep::started("s", "mock-1"),
        MockStep::Emit(ExecEvent::NeedsApproval {
            request_id: "r1".into(),
            subject,
        }),
        MockStep::AwaitAnswer {
            request_id: "r1".into(),
        },
        MockStep::text("after"),
    ];
    let rig = Rig::with_policy(steps, config(1), policy).await;
    let run = rig
        .fx
        .kernel
        .submit_for_bot("keeper", "go", None, "/work".into(), None)
        .await
        .unwrap();
    // Either answered at once by a rule, or waiting for a human who is not coming.
    until(async || {
        let journal = rig.fx.journal(&run);
        journal.iter().find_map(|(_, event)| match event {
            ExecEvent::ApprovalResolved { decision, by, .. } => Some((*decision, by.clone())),
            _ => None,
        })
    })
    .await
}

#[tokio::test(start_paused = true)]
async fn each_tool_of_renos_own_server_is_allowed_without_asking_a_human_and_without_a_journal_pair()
 {
    for tool in ["report", "request_approval", "board_read", "board_write"] {
        let name = format!("mcp__reno__{tool}");
        let steps = vec![
            MockStep::started("s", "mock-1"),
            MockStep::Emit(ExecEvent::NeedsApproval {
                request_id: "r1".into(),
                subject: mcp_call(&name),
            }),
            MockStep::AwaitAnswer {
                request_id: "r1".into(),
            },
            MockStep::text("after"),
        ];
        let rig = Rig::with_policy(steps, config(1), None).await;
        let run = rig
            .fx
            .kernel
            .submit_for_bot("keeper", "go", None, "/work".into(), None)
            .await
            .unwrap();
        // The harness got its answer: the text after the wait is journaled.
        until(async || {
            rig.fx
                .journal(&run)
                .iter()
                .any(|(_, event)| matches!(event, ExecEvent::Text { .. }))
                .then_some(())
        })
        .await;
        let journal = rig.fx.journal(&run);
        assert!(
            !journal.iter().any(|(_, event)| matches!(
                event,
                ExecEvent::NeedsApproval { .. } | ExecEvent::ApprovalResolved { .. }
            )),
            "{tool}: the call is in the approval history only, not the journal: {journal:?}"
        );
        let history = rig
            .fx
            .kernel
            .approval_history(Some(&run), None)
            .await
            .unwrap();
        assert_eq!(history.len(), 1, "{tool}");
        assert_eq!(history[0].subject, mcp_call(&name));
        assert_eq!(
            history[0].subject_digest.as_deref(),
            Some(mcp_call(&name).digest().as_str())
        );
        assert_eq!(history[0].status, ApprovalStatus::Approved);
        assert_eq!(
            history[0].decided_by,
            Some(ResolvedBy::Rule {
                name: "builtin:reno-mcp".into()
            })
        );
    }
}

#[tokio::test(start_paused = true)]
async fn another_servers_tool_and_a_look_alike_still_ask() {
    let policy = "default = \"deny\"\n";
    for tool in [
        "mcp__github__create_issue",
        "mcp__reno__rm_everything",
        "mcp__reno2__report",
    ] {
        let (decision, by) = judged(mcp_call(tool), Some(policy)).await;
        assert_eq!(decision, ApprovalOutcome::Denied, "{tool}");
        assert_ne!(
            by,
            ResolvedBy::Rule {
                name: "builtin:reno-mcp".into()
            },
            "{tool}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_rule_that_denies_renos_tools_still_wins() {
    let policy = r#"
default = "ask"
[[rules]]
note = "no reno board"
effect = "deny"
match = { action = "mcp_tool", tool = "mcp__reno__board_write" }
"#;
    let (decision, by) = judged(mcp_call("mcp__reno__board_write"), Some(policy)).await;
    assert_eq!(decision, ApprovalOutcome::Denied);
    assert_eq!(
        by,
        ResolvedBy::Rule {
            name: "no reno board".into()
        }
    );
}
