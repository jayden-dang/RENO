#![cfg(test)]

//! Resume (T1.10): a run the daemon did not mean to stop continues its harness session, with
//! the same run id, a higher attempt and its worktree. Paused time and the mock executor; a
//! crash is a store left as a dead daemon leaves it, a shutdown is the real thing.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use reno_harness::exec::{MockExecutor, MockStep};
use reno_proto::defs::DefKind;
use reno_proto::{
    CancelledBy, ExecEvent, NotResumed, RunId, RunStatus, SessionRef, StartRefusal, StopDetail,
    StopReason, TaskId, WorkspaceChanges,
};
use reno_store::{NewTask, Store};
use tempfile::TempDir;

use super::defs::{FakePort, file, setup};
use super::*;
use crate::dispatcher::RESUME_PROMPT;
use crate::{FakeWorkspace, KernelConfig, KernelError};

const HOUR_MS: i64 = 3_600_000;

fn hour() -> Duration {
    Duration::from_secs(3600)
}

/// A run that gets its session and then works for an hour.
fn long_run() -> MockExecutor {
    mock([
        MockStep::started("sess-1", "mock-1"),
        MockStep::text("working"),
        MockStep::Sleep(hour()),
    ])
}

/// What a restarted daemon runs the resumed attempt with: it reports the session and finishes.
fn finishing() -> MockExecutor {
    mock([
        MockStep::started("sess-1", "mock-1"),
        MockStep::text("finished"),
    ])
}

fn no_resume(m: MockExecutor) -> MockExecutor {
    let mut descriptor = reno_harness::exec::Executor::descriptor(&m);
    descriptor.caps.resume = false;
    m.with_descriptor(descriptor)
}

async fn session_of(fx: &Fx, run: &RunId) -> Option<String> {
    fx.store
        .get_run(run)
        .await
        .unwrap()
        .and_then(|row| row.session_ref)
}

async fn row(fx: &Fx, run: &RunId) -> reno_store::RunRecord {
    fx.store.get_run(run).await.unwrap().unwrap()
}

/// Stops the kernel the way SIGTERM does, driving paused time until it is done.
async fn drain(fx: &Fx) {
    let kernel = fx.kernel.clone();
    let stopping = tokio::spawn(async move { kernel.shutdown().await });
    until(async || stopping.is_finished().then_some(())).await;
    assert!(!stopping.await.unwrap().aborted);
}

/// A run that works until the daemon stops, then the daemon starts again over the same store
/// with `next` as its only executor.
async fn stopped_and_restarted(first: MockExecutor, next: MockExecutor) -> (Fx, RunId) {
    let fx = Fx::new(config(1), registry(&[first])).await;
    let run = fx.submit("work").await;
    until(async || session_of(&fx, &run).await).await;
    drain(&fx).await;
    let (dir, store) = fx.into_store();
    let fx = Fx::over(dir, store, config(1), registry(&[next])).await;
    (fx, run)
}

fn ids(n: &str) -> (TaskId, RunId) {
    (
        TaskId::from_suffix(n).unwrap(),
        RunId::from_suffix(n).unwrap(),
    )
}

/// A store as a daemon that died leaves it: `run` has been claimed `attempts` times, the last
/// attempt is `running` with its session recorded at `last_event` (when `session`), and the
/// run started at `started`.
async fn crashed_store(
    session: Option<&str>,
    attempts: u64,
    started: i64,
    last_event: i64,
) -> (TempDir, Store, RunId) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("reno.db")).unwrap();
    let (task, run) = ids("crashed");
    store
        .create_task(
            NewTask {
                id: task.clone(),
                goal: "goal".into(),
                class: None,
                spec: serde_json::json!({ "cwd": "/work" }),
            },
            0,
        )
        .await
        .unwrap();
    store
        .enqueue_run(&task, &run, &id("mock"), 0)
        .await
        .unwrap();
    for attempt in 1..=attempts {
        let claim = store.claim_next("old", 1, 60_000).await.unwrap().unwrap();
        assert_eq!((claim.run_id.clone(), claim.fence), (run.clone(), attempt));
        store
            .transition(&run, claim.fence, RunStatus::Running, started)
            .await
            .unwrap();
        if let Some(session) = session {
            store
                .record_session(&run, claim.fence, &SessionRef(session.into()), last_event)
                .await
                .unwrap();
        }
        if attempt < attempts {
            store
                .transition(&run, claim.fence, RunStatus::Interrupted, last_event)
                .await
                .unwrap();
            store
                .resume_run(&run, claim.fence, None, last_event)
                .await
                .unwrap();
        }
    }
    (dir, store, run)
}

async fn restart(dir: TempDir, store: Store, next: MockExecutor) -> Fx {
    Fx::over(dir, store, config(1), registry(&[next])).await
}

fn done_reason(journal: &[(reno_proto::Seq, ExecEvent)]) -> Option<StopDetail> {
    match journal.last() {
        Some((_, ExecEvent::Done { reason, .. })) => reason.clone(),
        other => panic!("not a Done: {other:?}"),
    }
}

// ---- what resumes ------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_run_stopped_by_a_shutdown_is_resumed_with_its_session_at_the_next_start() {
    let after = finishing();
    let (fx, run) = stopped_and_restarted(long_run(), after.clone()).await;

    fx.wait_status(&run, RunStatus::Succeeded).await;
    let specs = after.log().specs;
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].resume, Some(SessionRef("sess-1".into())));
    assert_eq!(specs[0].prompt, RESUME_PROMPT);
    assert_eq!(specs[0].run_id, run);

    let row = row(&fx, &run).await;
    assert_eq!(row.lease_fence, 2, "the second claim: attempt 2");
    assert_eq!(fx.kernel.get_run(&run).await.unwrap().attempt, 2);
    assert_eq!((row.stop, row.not_resumed), (None, None));

    // One journal, one run: the attempt that was stopped, the resume, the attempt that finished.
    let journal = fx.journal_with_done(&run).await;
    let events: Vec<&ExecEvent> = journal.iter().map(|(_, e)| e).collect();
    let stopped = events
        .iter()
        .position(|e| matches!(e, ExecEvent::Done { .. }))
        .expect("the first attempt's Done");
    assert!(matches!(
        events[stopped],
        ExecEvent::Done {
            stop: StopReason::Interrupted,
            reason: Some(StopDetail::Shutdown),
            ..
        }
    ));
    assert_eq!(
        events[stopped + 1],
        &ExecEvent::Resumed {
            from_attempt: 1,
            session: SessionRef("sess-1".into())
        }
    );
    assert!(matches!(events[stopped + 2], ExecEvent::Started { .. }));
    assert!(matches!(
        events.last(),
        Some(ExecEvent::Done {
            stop: StopReason::Completed,
            ..
        })
    ));
}

#[tokio::test(start_paused = true)]
async fn a_run_the_daemon_died_with_is_resumed_with_its_session_and_the_next_attempt() {
    let (dir, store, run) = crashed_store(Some("sess-9"), 1, 10, 20).await;
    let after = finishing();
    let fx = restart(dir, store, after.clone()).await;

    fx.wait_status(&run, RunStatus::Succeeded).await;
    let specs = after.log().specs;
    assert_eq!(specs[0].resume, Some(SessionRef("sess-9".into())));
    assert_eq!(fx.kernel.get_run(&run).await.unwrap().attempt, 2);

    // Recovery's own Done for the dead attempt says it crashed, and the resume follows it.
    let journal = fx.journal_with_done(&run).await;
    assert!(matches!(
        &journal[0].1,
        ExecEvent::Done {
            stop: StopReason::Interrupted,
            reason: Some(StopDetail::Crashed),
            ..
        }
    ));
    assert_eq!(
        journal[1].1,
        ExecEvent::Resumed {
            from_attempt: 1,
            session: SessionRef("sess-9".into())
        }
    );
    let kinds = fx.transitions(&run).await;
    assert!(
        kinds
            .iter()
            .any(|(from, to)| from == "interrupted" && to == "queued"),
        "{kinds:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_resumed_run_that_works_in_a_worktree_gets_the_same_worktree_back() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let first = long_run();
    let fx = Fx::with_workspace(config(1), registry(&[first]), None, ws.clone()).await;
    let run = fx
        .kernel
        .submit("work", id("mock"), PathBuf::from("/repo/crates/x"), None)
        .await
        .unwrap();
    until(async || session_of(&fx, &run).await).await;
    ws.set_changes(
        &run,
        WorkspaceChanges {
            commits: 1,
            dirty: true,
        },
    );
    drain(&fx).await;
    let before = fx.kernel.workspace(&run).await.unwrap();
    let (dir, store) = fx.into_store();

    let after = finishing();
    let fx = Fx::over_workspace(
        dir,
        store,
        config(1),
        registry(std::slice::from_ref(&after)),
        None,
        ws.clone(),
    )
    .await;
    fx.wait_status(&run, RunStatus::Succeeded).await;

    let specs = after.log().specs;
    assert_eq!(specs[0].cwd, PathBuf::from(&before.path).join("crates/x"));
    assert_eq!(specs[0].resume, Some(SessionRef("sess-1".into())));
    // Made once: the second attempt found it and used it.
    let creates = ws
        .calls()
        .into_iter()
        .filter(|call| call.starts_with("create"))
        .count();
    assert_eq!(creates, 1, "{:?}", ws.calls());
    assert_eq!(fx.kernel.workspace(&run).await.unwrap().path, before.path);
}

#[tokio::test(start_paused = true)]
async fn a_bot_that_says_auto_or_nothing_has_its_runs_resumed() {
    let port = FakePort::with(vec![file(
        DefKind::Bot,
        "keeper",
        "name = \"keeper\"\ninstructions = \"x\"\nexecutors = [\"mock\"]\n",
    )]);
    let fx = Fx::with_definitions(config(1), registry(&[long_run()]), setup(&port)).await;
    let run = fx
        .kernel
        .submit_for_bot("keeper", "go", None, PathBuf::from("/work"), None)
        .await
        .unwrap();
    until(async || session_of(&fx, &run).await).await;
    drain(&fx).await;
    let (dir, store) = fx.into_store();
    let after = finishing();
    let fx = Fx::over_definitions(
        dir,
        store,
        config(1),
        registry(std::slice::from_ref(&after)),
        setup(&port),
    )
    .await;
    fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(after.log().specs.len(), 1);
    assert_eq!(fx.kernel.get_run(&run).await.unwrap().attempt, 2);
}

// ---- what does not -----------------------------------------------------------------------

/// The run is interrupted, was not queued, and says why.
async fn stays_interrupted(fx: &Fx, run: &RunId, why: NotResumed) {
    for _ in 0..500 {
        tokio::task::yield_now().await;
    }
    assert_eq!(fx.status(run).await, RunStatus::Interrupted);
    let summary = fx.kernel.get_run(run).await.unwrap();
    assert_eq!(summary.not_resumed, Some(why));
}

#[tokio::test(start_paused = true)]
async fn a_run_a_person_cancelled_is_not_resumed() {
    let fx = Fx::new(config(1), registry(&[long_run()])).await;
    let run = fx.submit("work").await;
    until(async || session_of(&fx, &run).await).await;
    fx.kernel.cancel(&run).await.unwrap();
    fx.wait_status(&run, RunStatus::Interrupted).await;
    let journal = fx.journal_with_done(&run).await;
    assert_eq!(
        done_reason(&journal),
        Some(StopDetail::Cancelled {
            by: CancelledBy::User
        })
    );
    // The reason is on the run the moment it stops; no restart is needed to learn it.
    let summary = fx.kernel.get_run(&run).await.unwrap();
    assert_eq!(
        summary.stop,
        Some(StopDetail::Cancelled {
            by: CancelledBy::User
        })
    );
    assert_eq!(summary.not_resumed, Some(NotResumed::Cancelled));

    drain(&fx).await;
    let (dir, store) = fx.into_store();
    let after = finishing();
    let fx = restart(dir, store, after.clone()).await;
    stays_interrupted(&fx, &run, NotResumed::Cancelled).await;
    assert!(after.log().specs.is_empty(), "nothing was started");
}

#[tokio::test(start_paused = true)]
async fn a_run_a_budget_stopped_is_not_resumed() {
    let port = FakePort::with(vec![file(
        DefKind::Bot,
        "keeper",
        "name = \"keeper\"\ninstructions = \"x\"\nexecutors = [\"mock\"]\n[budget]\nmax_wall_time = \"10s\"\n",
    )]);
    let fx = Fx::with_definitions(config(1), registry(&[long_run()]), setup(&port)).await;
    let run = fx
        .kernel
        .submit_for_bot("keeper", "go", None, PathBuf::from("/work"), None)
        .await
        .unwrap();
    fx.wait_status(&run, RunStatus::Interrupted).await;
    let journal = fx.journal_with_done(&run).await;
    let stop = StopDetail::BudgetExceeded {
        kind: reno_proto::BudgetKind::WallTime,
        limit: 10_000,
        used: 10_000,
    };
    assert_eq!(done_reason(&journal), Some(stop.clone()));
    assert_eq!(fx.kernel.get_run(&run).await.unwrap().stop, Some(stop));

    drain(&fx).await;
    let (dir, store) = fx.into_store();
    let after = finishing();
    let fx = Fx::over_definitions(
        dir,
        store,
        config(1),
        registry(std::slice::from_ref(&after)),
        setup(&port),
    )
    .await;
    stays_interrupted(&fx, &run, NotResumed::BudgetStopped).await;
    assert!(after.log().specs.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_run_whose_attempts_are_used_stays_interrupted_with_the_reason() {
    let (dir, store, run) = crashed_store(Some("sess-1"), 3, 10, 20).await;
    let after = finishing();
    let fx = restart(dir, store, after.clone()).await;
    stays_interrupted(
        &fx,
        &run,
        NotResumed::AttemptsExhausted {
            attempts: 3,
            max: 3,
        },
    )
    .await;
    assert!(after.log().specs.is_empty());
    // The cap is the kernel's setting, not a constant: a higher one resumes it.
    let (dir, store, run) = crashed_store(Some("sess-1"), 3, 10, 20).await;
    let after = finishing();
    let config = KernelConfig {
        max_attempts: 4,
        ..config(1)
    };
    let fx = Fx::over(dir, store, config, registry(std::slice::from_ref(&after))).await;
    fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(fx.kernel.get_run(&run).await.unwrap().attempt, 4);
}

#[tokio::test(start_paused = true)]
async fn a_run_with_no_session_has_nothing_to_resume() {
    let (dir, store, run) = crashed_store(None, 1, 10, 20).await;
    let fx = restart(dir, store, finishing()).await;
    stays_interrupted(&fx, &run, NotResumed::NoSession).await;
}

#[tokio::test(start_paused = true)]
async fn a_run_past_its_wall_time_budget_is_not_resumed() {
    // Started at 10 ms, last heard from after the default four hours.
    let (dir, store, run) = crashed_store(Some("sess-1"), 1, 10, 4 * HOUR_MS + 11).await;
    let fx = restart(dir, store, finishing()).await;
    stays_interrupted(
        &fx,
        &run,
        NotResumed::WallTimeSpent {
            limit_ms: u64::try_from(4 * HOUR_MS).unwrap(),
        },
    )
    .await;
    // The time the daemon was down is not the run's: a run that worked for a minute and was
    // found a day later is within its budget.
    let (dir, store, run) = crashed_store(Some("sess-1"), 1, 10, 60_010).await;
    let after = finishing();
    let fx = restart(dir, store, after).await;
    fx.wait_status(&run, RunStatus::Succeeded).await;
}

#[tokio::test(start_paused = true)]
async fn an_executor_that_cannot_resume_leaves_the_run_interrupted() {
    let (fx, run) = stopped_and_restarted(long_run(), no_resume(finishing())).await;
    stays_interrupted(
        &fx,
        &run,
        NotResumed::ExecutorCannotResume {
            executor: id("mock"),
        },
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn an_executor_the_daemon_no_longer_has_leaves_the_run_interrupted() {
    let (dir, store, run) = crashed_store(Some("sess-1"), 1, 10, 20).await;
    let fx = Fx::over(dir, store, config(1), ExecutorRegistry::new()).await;
    stays_interrupted(
        &fx,
        &run,
        NotResumed::ExecutorMissing {
            executor: id("mock"),
        },
    )
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_bot_that_says_never_keeps_its_runs_interrupted() {
    let port = FakePort::with(vec![file(
        DefKind::Bot,
        "keeper",
        "name = \"keeper\"\ninstructions = \"x\"\nexecutors = [\"mock\"]\nresume = \"never\"\n",
    )]);
    let fx = Fx::with_definitions(config(1), registry(&[long_run()]), setup(&port)).await;
    let run = fx
        .kernel
        .submit_for_bot("keeper", "go", None, PathBuf::from("/work"), None)
        .await
        .unwrap();
    until(async || session_of(&fx, &run).await).await;
    drain(&fx).await;
    let (dir, store) = fx.into_store();
    let after = finishing();
    let fx = Fx::over_definitions(
        dir,
        store,
        config(1),
        registry(std::slice::from_ref(&after)),
        setup(&port),
    )
    .await;
    stays_interrupted(
        &fx,
        &run,
        NotResumed::BotForbids {
            bot: "keeper".into(),
        },
    )
    .await;
    assert!(after.log().specs.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_run_judged_once_is_not_judged_again_at_every_start() {
    let (dir, store, run) = crashed_store(Some("sess-1"), 3, 10, 20).await;
    let fx = restart(dir, store, finishing()).await;
    stays_interrupted(
        &fx,
        &run,
        NotResumed::AttemptsExhausted {
            attempts: 3,
            max: 3,
        },
    )
    .await;
    let kinds_before = fx
        .store
        .events_since(reno_proto::Seq(0), 100_000)
        .await
        .unwrap()
        .len();
    drain(&fx).await;
    let (dir, store) = fx.into_store();
    let fx = restart(dir, store, finishing()).await;
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
    let kinds_after = fx
        .store
        .events_since(reno_proto::Seq(0), 100_000)
        .await
        .unwrap()
        .len();
    assert_eq!(
        kinds_before, kinds_after,
        "a second start wrote nothing for the run"
    );
}

// ---- a person's resume ---------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_person_resumes_a_cancelled_run_with_a_prompt_of_their_own() {
    let first = long_run();
    let fx = Fx::new(config(1), registry(std::slice::from_ref(&first))).await;
    // The same executor serves the second attempt: swap its script by using a second mock id.
    let run = fx.submit("work").await;
    until(async || session_of(&fx, &run).await).await;
    fx.kernel.cancel(&run).await.unwrap();
    fx.wait_status(&run, RunStatus::Interrupted).await;
    fx.journal_with_done(&run).await;

    let attempt = fx
        .kernel
        .resume_run(&run, Some("carry on with the tests".into()))
        .await
        .unwrap();
    assert_eq!(attempt, 2);
    until(async || (first.log().specs.len() == 2).then_some(())).await;
    let specs = first.log().specs;
    assert_eq!(specs[1].resume, Some(SessionRef("sess-1".into())));
    assert_eq!(specs[1].prompt, "carry on with the tests");
    assert_eq!(specs[0].prompt, "work");
    let summary = fx.kernel.get_run(&run).await.unwrap();
    assert_eq!((summary.stop, summary.not_resumed), (None, None));
    until(async || {
        fx.journal(&run)
            .iter()
            .any(|(_, e)| {
                matches!(
                    e,
                    ExecEvent::Resumed {
                        from_attempt: 1,
                        ..
                    }
                )
            })
            .then_some(())
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_person_resumes_a_failed_run_that_has_a_session() {
    let failing = mock([
        MockStep::started("sess-1", "mock-1"),
        MockStep::Emit(ExecEvent::Done {
            stop: StopReason::Error,
            result: None,
            diagnostic: Some("the harness fell over".into()),
            reason: None,
        }),
    ]);
    let fx = Fx::new(config(1), registry(std::slice::from_ref(&failing))).await;
    let run = fx.submit("work").await;
    fx.wait_status(&run, RunStatus::Failed).await;
    fx.journal_with_done(&run).await;
    assert_eq!(fx.kernel.resume_run(&run, None).await.unwrap(), 2);
    until(async || (failing.log().specs.len() == 2).then_some(())).await;
    assert_eq!(
        failing.log().specs[1].resume,
        Some(SessionRef("sess-1".into()))
    );
}

fn not_resumable(error: KernelError) -> NotResumed {
    match error {
        KernelError::NotResumable { why, .. } => why,
        other => panic!("not a NotResumable: {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn a_run_that_cannot_be_resumed_is_refused_with_the_reason() {
    let m = mock([MockStep::started("sess-1", "mock-1"), MockStep::text("hi")]);
    let fx = Fx::new(config(1), registry(&[m])).await;
    let done = fx.submit("work").await;
    fx.wait_status(&done, RunStatus::Succeeded).await;
    fx.journal_with_done(&done).await;
    let error = fx.kernel.resume_run(&done, None).await.unwrap_err();
    assert_eq!(
        not_resumable(error),
        NotResumed::WrongStatus {
            status: RunStatus::Succeeded
        }
    );

    // Interrupted before its harness said anything: no session.
    let quiet = Fx::new(config(1), registry(&[mock([MockStep::Sleep(hour())])])).await;
    let run = quiet.submit("work").await;
    quiet.wait_status(&run, RunStatus::Running).await;
    quiet.kernel.cancel(&run).await.unwrap();
    quiet.wait_status(&run, RunStatus::Interrupted).await;
    quiet.journal_with_done(&run).await;
    assert_eq!(
        not_resumable(quiet.kernel.resume_run(&run, None).await.unwrap_err()),
        NotResumed::NoSession
    );

    // A session, but an executor that cannot continue it.
    let (dir, store, run) = crashed_store(Some("sess-1"), 1, 10, 20).await;
    let fx = restart(dir, store, no_resume(finishing())).await;
    stays_interrupted(
        &fx,
        &run,
        NotResumed::ExecutorCannotResume {
            executor: id("mock"),
        },
    )
    .await;
    assert_eq!(
        not_resumable(fx.kernel.resume_run(&run, None).await.unwrap_err()),
        NotResumed::ExecutorCannotResume {
            executor: id("mock")
        }
    );

    // A run that is not there.
    let ghost = RunId::from_suffix("ghost").unwrap();
    assert!(matches!(
        fx.kernel.resume_run(&ghost, None).await,
        Err(KernelError::RunNotFound(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn a_person_may_resume_a_run_past_the_attempt_cap() {
    let (dir, store, run) = crashed_store(Some("sess-1"), 3, 10, 20).await;
    let after = finishing();
    let fx = restart(dir, store, after.clone()).await;
    stays_interrupted(
        &fx,
        &run,
        NotResumed::AttemptsExhausted {
            attempts: 3,
            max: 3,
        },
    )
    .await;
    assert_eq!(fx.kernel.resume_run(&run, None).await.unwrap(), 4);
    fx.wait_status(&run, RunStatus::Succeeded).await;
}

// ---- the typed reason a run stops for ----------------------------------------------------------

async fn failed_start(fx: &Fx, run: &RunId) -> StopDetail {
    fx.wait_status(run, RunStatus::Failed).await;
    let journal = fx.journal_with_done(run).await;
    let reason = done_reason(&journal).expect("a typed reason");
    assert_eq!(
        fx.kernel.get_run(run).await.unwrap().stop,
        Some(reason.clone())
    );
    reason
}

#[tokio::test(start_paused = true)]
async fn a_run_no_executor_will_take_says_why_in_a_typed_reason() {
    let fx = Fx::new(config(1), registry(&[mock([])])).await;
    let ghost = fx
        .kernel
        .submit("go", id("ghost"), PathBuf::from("/work"), None)
        .await
        .unwrap();
    assert!(matches!(
        failed_start(&fx, &ghost).await,
        StopDetail::StartRefused {
            failure: StartRefusal::Invalid,
            ..
        }
    ));

    // An executor that offers only full access, asked for the strictest mode.
    let mut descriptor = MockExecutor::default_descriptor(id("picky"));
    descriptor.caps.access_modes = vec![reno_proto::AccessMode::FullAccess];
    let picky = MockExecutor::new(id("picky")).with_descriptor(descriptor);
    let fx = Fx::new(config(1), registry(&[picky])).await;
    let run = fx
        .kernel
        .submit("go", id("picky"), PathBuf::from("/work"), None)
        .await
        .unwrap();
    match failed_start(&fx, &run).await {
        StopDetail::StartRefused { failure, detail } => {
            assert_eq!(failure, StartRefusal::NoEligibleExecutor);
            assert!(detail.starts_with("NoEligibleExecutor"), "{detail}");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn a_write_run_with_no_repository_is_refused_as_a_workspace_refusal() {
    let ws = Arc::new(FakeWorkspace::new());
    let fx = Fx::with_workspace(config(1), registry(&[mock([])]), None, ws).await;
    let run = fx
        .kernel
        .submit("go", id("mock"), PathBuf::from("/nowhere"), None)
        .await
        .unwrap();
    assert!(matches!(
        failed_start(&fx, &run).await,
        StopDetail::StartRefused {
            failure: StartRefusal::WorkspaceRefused,
            ..
        }
    ));
}

#[tokio::test(start_paused = true)]
async fn an_executor_that_ends_a_run_by_itself_has_its_own_reason_and_is_not_resumed_unasked() {
    let m = mock([
        MockStep::started("sess-1", "mock-1"),
        MockStep::done(StopReason::Interrupted),
    ]);
    let fx = Fx::new(config(1), registry(&[m])).await;
    let run = fx.submit("go").await;
    fx.wait_status(&run, RunStatus::Interrupted).await;
    let journal = fx.journal_with_done(&run).await;
    assert_eq!(done_reason(&journal), Some(StopDetail::ExecutorStopped));
    assert_eq!(
        fx.kernel.get_run(&run).await.unwrap().not_resumed,
        Some(NotResumed::ExecutorStopped)
    );
}
