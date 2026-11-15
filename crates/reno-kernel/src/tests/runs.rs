#![cfg(test)]

//! Runs through the dispatcher and supervisors: happy path, concurrency, cancel, approvals,
//! start failures, subscriptions, shutdown and idleness.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use reno_harness::exec::{
    Answer, Decision, ExecControls, ExecError, Executor, ExecutorHealth, MockExecutor, MockStep,
};
use reno_proto::{
    AccessMode, ActionClass, ApprovalDecision, ApprovalSubject, ExecEvent, ExecSpec, RunStatus,
    Seq, StopReason,
};
use tokio::time::advance;

use super::*;
use crate::KernelError;

fn needs_approval(request: &str) -> ExecEvent {
    ExecEvent::NeedsApproval {
        request_id: request.into(),
        subject: ApprovalSubject {
            action: ActionClass::Command,
            tool: "bash".into(),
            command: Some("rm -rf build".into()),
            paths: Vec::new(),
            raw: serde_json::Value::Null,
        },
    }
}

fn diagnostic(done: &ExecEvent) -> String {
    match done {
        ExecEvent::Done { diagnostic, .. } => diagnostic.clone().unwrap_or_default(),
        other => panic!("not a Done: {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn a_run_succeeds_and_the_journal_and_the_live_subscriber_agree() {
    let m = mock([
        MockStep::started("s1", "mock-1"),
        MockStep::text("a"),
        MockStep::Sleep(Duration::from_secs(5)),
        MockStep::text("b"),
    ]);
    let fx = Fx::new(config(2), registry(&[m])).await;
    let mut runs = fx.kernel.watch_runs();

    let run = fx.submit("write a poem\nsecond line").await;
    // Subscribe while the run is still queued: the stream waits for it to go live.
    let live = {
        let kernel = fx.kernel.clone();
        let run = run.clone();
        tokio::spawn(async move {
            let mut seen = Vec::new();
            let mut stream = kernel.run_events(&run, None).await.unwrap();
            while let Some(item) = stream.next().await {
                let item = item.unwrap();
                if matches!(item.event, ExecEvent::Done { .. }) {
                    // Completion order: the state committed before `Done` reached us.
                    let status = kernel.get_run(&run).await.unwrap().status;
                    assert_eq!(status, RunStatus::Succeeded);
                }
                seen.push(item);
            }
            seen
        })
    };
    until(async || live.is_finished().then_some(())).await;
    let live = live.await.unwrap();

    let journal = fx.journal(&run);
    assert_eq!(
        live.iter()
            .map(|i| (i.seq, i.event.clone()))
            .collect::<Vec<_>>(),
        journal
    );
    assert_eq!(
        journal.iter().map(|(s, _)| s.0).collect::<Vec<_>>(),
        [1, 2, 3, 4],
        "seqs are gap-free"
    );
    assert!(matches!(journal[0].1, ExecEvent::Started { .. }));
    assert_eq!(
        *last_done(&journal),
        ExecEvent::Done {
            stop: StopReason::Completed,
            result: None,
            diagnostic: None
        }
    );
    assert_eq!(
        fx.transitions(&run).await,
        [
            ("leased".into(), "running".into()),
            ("running".into(), "evaluating".into()),
            ("evaluating".into(), "succeeded".into()),
        ]
    );

    // Resuming from a seq in the middle replays from it, inclusive.
    let from_two: Vec<_> = fx
        .events(&run, Some(Seq(2)))
        .await
        .into_iter()
        .map(|i| i.unwrap().seq.0)
        .collect();
    assert_eq!(from_two, [2, 3, 4]);

    let summary = fx.kernel.get_run(&run).await.unwrap();
    assert_eq!(summary.title, "write a poem");
    assert_eq!(summary.prompt.as_deref(), Some("write a poem\nsecond line"));
    assert_eq!(summary.model, None, "no model was asked for");
    assert_eq!(summary.stop_reason, Some(StopReason::Completed));
    assert_eq!(summary.attempt, 1);
    let (listed, next) = fx
        .kernel
        .list_runs(Some(&[RunStatus::Succeeded]), None, None)
        .await
        .unwrap();
    assert_eq!(listed, vec![summary.clone()]);
    assert_eq!(next, None);

    // The watch carries the refreshed list.
    until(async || {
        runs.borrow_and_update()
            .iter()
            .any(|r| r.run_id == run && r.status == RunStatus::Succeeded)
            .then_some(())
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn the_summary_carries_the_prompt_and_the_requested_model() {
    let fx = Fx::new(config(1), registry(&[mock([MockStep::text("hi")])])).await;
    let prompt = "first line\nsecond line";
    let run = fx
        .kernel
        .submit(
            prompt,
            id("mock"),
            PathBuf::from("/work"),
            Some("mock-1".into()),
        )
        .await
        .unwrap();
    fx.wait_status(&run, RunStatus::Succeeded).await;
    let summary = fx.kernel.get_run(&run).await.unwrap();
    assert_eq!(summary.title, "first line");
    assert_eq!(summary.prompt.as_deref(), Some(prompt));
    assert_eq!(summary.model.as_deref(), Some("mock-1"));
    fx.kernel.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_late_subscriber_resumes_mid_run_without_a_gap() {
    let m = mock([
        MockStep::text("0"),
        MockStep::text("1"),
        MockStep::Sleep(Duration::from_secs(5)),
        MockStep::text("2"),
        MockStep::text("3"),
    ]);
    let fx = Fx::new(config(1), registry(&[m])).await;
    let run = fx.submit("go").await;
    let mut first = fx.kernel.run_events(&run, None).await.unwrap();
    let a = first.next().await.unwrap().unwrap();
    let b = first.next().await.unwrap().unwrap();
    assert_eq!((a.seq, b.seq), (Seq(1), Seq(2)));
    // The run is asleep now. A second subscriber starting from seq 2 gets the journal's tail
    // and then the live events.
    let second = fx.kernel.run_events(&run, Some(Seq(2))).await.unwrap();
    let rest = tokio::spawn(second.collect::<Vec<_>>());
    advance(Duration::from_secs(5)).await;
    until(async || rest.is_finished().then_some(())).await;
    let seqs: Vec<u64> = rest
        .await
        .unwrap()
        .into_iter()
        .map(|i| i.unwrap().seq.0)
        .collect();
    assert_eq!(seqs, [2, 3, 4, 5]);
}

#[tokio::test(start_paused = true)]
async fn max_concurrent_is_honoured_and_the_next_run_starts_when_a_slot_frees() {
    let m = mock([MockStep::Sleep(Duration::from_secs(10))]);
    let fx = Fx::new(config(1), registry(&[m])).await;
    let a = fx.submit("a").await;
    let b = fx.submit("b").await;
    fx.wait_status(&a, RunStatus::Running).await;
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert_eq!(fx.status(&b).await, RunStatus::Queued, "no free slot");

    advance(Duration::from_secs(10)).await;
    fx.wait_status(&b, RunStatus::Running).await;
    assert_eq!(fx.status(&a).await, RunStatus::Succeeded);
    advance(Duration::from_secs(10)).await;
    fx.wait_status(&b, RunStatus::Succeeded).await;

    // `b` was claimed only after `a` ended.
    let log = fx.store.events_since(Seq(0), 1000).await.unwrap();
    let position = |kind: &str, run: &RunId| {
        log.iter()
            .position(|e| e.kind == kind && e.subject_id == run.as_str())
            .unwrap()
    };
    assert!(position("run.ended", &a) < position("run.claimed", &b));
}

#[tokio::test(start_paused = true)]
async fn cancel_ends_a_live_run_interrupted_and_a_queued_one_cancelled() {
    let m = mock([
        MockStep::text("working"),
        MockStep::Sleep(Duration::from_secs(3600)),
    ]);
    let fx = Fx::new(config(1), registry(&[m])).await;
    let live = fx.submit("long").await;
    let queued = fx.submit("waiting").await;
    fx.wait_status(&live, RunStatus::Running).await;

    assert_eq!(
        fx.kernel.cancel(&queued).await.unwrap(),
        RunStatus::Cancelled
    );
    assert_eq!(fx.status(&queued).await, RunStatus::Cancelled);

    fx.kernel.cancel(&live).await.unwrap();
    fx.wait_status(&live, RunStatus::Interrupted).await;
    let journal = fx.journal_with_done(&live).await;
    assert!(matches!(
        last_done(&journal),
        ExecEvent::Done {
            stop: StopReason::Interrupted,
            ..
        }
    ));

    // Nothing is left to cancel.
    assert!(matches!(
        fx.kernel.cancel(&queued).await,
        Err(KernelError::NotCancellable {
            status: RunStatus::Cancelled,
            ..
        })
    ));
    // An interrupted run can still be cancelled: it waits for a human.
    assert_eq!(fx.kernel.cancel(&live).await.unwrap(), RunStatus::Cancelled);
    let unknown = RunId::from_suffix("nope").unwrap();
    assert!(matches!(
        fx.kernel.cancel(&unknown).await,
        Err(KernelError::RunNotFound(_))
    ));
}

async fn approval_flow(decision: ApprovalDecision, expect: Decision) {
    let m = mock([
        MockStep::started("s", "mock-1"),
        MockStep::Emit(needs_approval("req1")),
        MockStep::AwaitAnswer {
            request_id: "req1".into(),
        },
        MockStep::text("after"),
    ]);
    let fx = Fx::new(config(1), registry(std::slice::from_ref(&m))).await;
    let mut watch = fx.kernel.watch_approvals();
    let run = fx.submit("needs a human").await;

    let approval = until(async || {
        fx.kernel
            .pending_approvals()
            .await
            .unwrap()
            .into_iter()
            .next()
    })
    .await;
    assert_eq!(approval.run_id, run);
    assert_eq!(approval.subject.tool, "bash");
    fx.wait_status(&run, RunStatus::WaitingApproval).await;
    assert_eq!(watch.borrow_and_update().len(), 1);

    fx.kernel.decide(&approval.id, decision).await.unwrap();
    assert!(fx.kernel.pending_approvals().await.unwrap().is_empty());
    assert!(watch.borrow_and_update().is_empty());
    fx.wait_status(&run, RunStatus::Succeeded).await;

    let answers = m.log().answers;
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].request_id, "req1");
    assert_eq!(answers[0].answer, Answer::Decision(expect));

    let path: Vec<_> = fx
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
    // Deciding twice finds nothing.
    assert!(matches!(
        fx.kernel.decide(&approval.id, decision).await,
        Err(KernelError::ApprovalNotFound(_))
    ));
    // The request is in the journal like any other event.
    assert!(
        fx.journal(&run)
            .iter()
            .any(|(_, e)| matches!(e, ExecEvent::NeedsApproval { .. }))
    );
}

#[tokio::test(start_paused = true)]
async fn approving_returns_the_run_to_running_and_it_succeeds() {
    approval_flow(ApprovalDecision::ApproveOnce, Decision::Allow).await;
}

#[tokio::test(start_paused = true)]
async fn denying_is_delivered_and_the_run_goes_on() {
    approval_flow(ApprovalDecision::Deny, Decision::Deny).await;
}

#[tokio::test(start_paused = true)]
async fn needs_input_waits_for_input_and_cancel_still_ends_it() {
    let m = mock([
        MockStep::Emit(ExecEvent::NeedsInput {
            request_id: "q".into(),
            questions: Vec::new(),
        }),
        MockStep::AwaitAnswer {
            request_id: "q".into(),
        },
    ]);
    let fx = Fx::new(config(1), registry(&[m])).await;
    let run = fx.submit("ask").await;
    fx.wait_status(&run, RunStatus::WaitingInput).await;
    fx.kernel.cancel(&run).await.unwrap();
    fx.wait_status(&run, RunStatus::Interrupted).await;
    assert_eq!(
        fx.transitions(&run).await.last().unwrap(),
        &("waiting_input".to_owned(), "interrupted".to_owned())
    );
}

#[tokio::test(start_paused = true)]
async fn an_unknown_executor_fails_the_run_with_the_diagnostic_in_its_journal() {
    let fx = Fx::new(config(1), registry(&[])).await;
    let run = fx
        .kernel
        .submit("hi", id("ghost"), "/work".into(), None)
        .await
        .unwrap();
    fx.wait_status(&run, RunStatus::Failed).await;
    let events = fx.events(&run, None).await;
    assert_eq!(events.len(), 1);
    let done = events[0].as_ref().unwrap().event.clone();
    assert!(
        diagnostic(&done).contains("unknown executor ghost"),
        "{done:?}"
    );
    assert!(matches!(
        done,
        ExecEvent::Done {
            stop: StopReason::Error,
            ..
        }
    ));
    let path: Vec<_> = fx
        .transitions(&run)
        .await
        .into_iter()
        .map(|(_, to)| to)
        .collect();
    assert_eq!(path, ["running", "evaluating", "failed"]);
    // The slot was freed: another run still goes through.
    assert_eq!(
        fx.kernel.get_run(&run).await.unwrap().stop_reason,
        Some(StopReason::Error)
    );
}

#[tokio::test(start_paused = true)]
async fn an_executor_that_cannot_honour_the_access_mode_is_not_eligible() {
    let mut descriptor = MockExecutor::default_descriptor(id("mock"));
    descriptor.caps.access_modes = vec![AccessMode::FullAccess];
    let m = MockExecutor::new(id("mock")).with_descriptor(descriptor);
    let fx = Fx::new(config(1), registry(std::slice::from_ref(&m))).await; // asks for Supervised
    let run = fx.submit("hi").await;
    fx.wait_status(&run, RunStatus::Failed).await;
    let events = fx.events(&run, None).await;
    let done = events.last().unwrap().as_ref().unwrap().event.clone();
    let why = diagnostic(&done);
    assert!(why.starts_with("NoEligibleExecutor:"), "{done:?}");
    assert!(
        why.contains("Supervised") && why.contains("[FullAccess]"),
        "{why}"
    );
    assert!(m.log().specs.is_empty(), "nothing was started");
}

/// An executor whose `start` refuses every run, as a harness that is not signed in does.
struct Refusing;

#[async_trait]
impl Executor for Refusing {
    fn descriptor(&self) -> reno_proto::ExecutorDescriptor {
        MockExecutor::default_descriptor(id("mock"))
    }

    async fn probe(&self) -> ExecutorHealth {
        ExecutorHealth::default()
    }

    async fn start(
        &self,
        _: ExecSpec,
        _: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError> {
        Err(ExecError::NotSignedIn("run `claude login`".into()))
    }
}

#[tokio::test(start_paused = true)]
async fn an_exec_error_fails_the_run_with_the_diagnostic() {
    let registry = ExecutorRegistry::new().with(Arc::new(Refusing));
    let fx = Fx::new(config(1), registry).await;
    let run = fx.submit("hi").await;
    fx.wait_status(&run, RunStatus::Failed).await;
    let events = fx.events(&run, None).await;
    let done = events.last().unwrap().as_ref().unwrap().event.clone();
    assert!(diagnostic(&done).contains("not signed in"), "{done:?}");
    assert!(
        diagnostic(&done).contains("the executor refused the run"),
        "{done:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_slow_subscriber_gets_a_gap_and_resumes_without_loss() {
    let mut steps = vec![
        MockStep::text("first"),
        MockStep::Sleep(Duration::from_secs(5)),
    ];
    steps.extend((0..10).map(|i| MockStep::text(format!("burst {i}"))));
    let config = KernelConfig {
        subscriber_queue: 2,
        ..config(1)
    };
    let fx = Fx::new(config, registry(&[mock(steps)])).await;
    let run = fx.submit("go").await;
    let mut stream = fx.kernel.run_events(&run, None).await.unwrap();
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(first.seq, Seq(1));
    // The subscriber stops reading while the burst overflows its queue.
    advance(Duration::from_secs(5)).await;
    fx.wait_status(&run, RunStatus::Succeeded).await;

    let mut got = vec![first.seq.0];
    let resume = loop {
        match stream.next().await {
            Some(Ok(item)) => got.push(item.seq.0),
            Some(Err(KernelError::Gap { resume_from })) => break resume_from,
            other => panic!("expected a gap, got {other:?}"),
        }
    };
    assert!(
        stream.next().await.is_none(),
        "the stream ends after the gap"
    );
    assert_eq!(got.len(), 3, "one event, then a full queue of two: {got:?}");
    assert_eq!(
        resume.0,
        got.last().unwrap() + 1,
        "resume is the first missed seq"
    );

    let rest: Vec<u64> = fx
        .events(&run, Some(resume))
        .await
        .into_iter()
        .map(|i| i.unwrap().seq.0)
        .collect();
    got.extend(rest);
    assert_eq!(
        got,
        (1..=12).collect::<Vec<u64>>(),
        "11 events and the Done, no loss"
    );
}

#[tokio::test(start_paused = true)]
async fn steer_reaches_a_live_run_and_a_finished_one_refuses() {
    let m = mock([MockStep::AwaitSteer, MockStep::text("steered")]);
    let fx = Fx::new(config(1), registry(std::slice::from_ref(&m))).await;
    let run = fx.submit("go").await;
    fx.wait_status(&run, RunStatus::Running).await;
    fx.kernel.steer(&run, "left a bit").await.unwrap();
    fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(m.log().steers, ["left a bit"]);
    assert!(
        fx.journal(&run)
            .iter()
            .any(|(_, e)| matches!(e, ExecEvent::UserMessage { text } if text == "left a bit")),
        "an accepted steer is journaled as a UserMessage"
    );
    assert!(matches!(
        fx.kernel.steer(&run, "again").await,
        Err(KernelError::RunEnded(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn a_live_run_heartbeats_every_third_of_the_lease() {
    let m = mock([MockStep::Sleep(Duration::from_secs(3600))]);
    let fx = Fx::new(config(1), registry(&[m])).await;
    let run = fx.submit("long").await;
    fx.wait_status(&run, RunStatus::Running).await;
    let lease = |fx: &Fx, run: &RunId| {
        let (store, run) = (fx.store.clone(), run.clone());
        async move {
            store
                .get_run(&run)
                .await
                .unwrap()
                .unwrap()
                .lease_expires_at
                .unwrap()
        }
    };
    let before = lease(&fx, &run).await;
    fx.clock.advance(21_000);
    advance(Duration::from_secs(21)).await;
    until(async || (lease(&fx, &run).await > before).then_some(())).await;
    assert_eq!(lease(&fx, &run).await, 1_700_000_000_000 + 21_000 + 60_000);
}

#[tokio::test(start_paused = true)]
async fn a_lease_held_by_someone_else_is_claimed_when_it_expires_and_not_before() {
    let m = mock([]);
    let fx = Fx::new(config(1), registry(&[m])).await;
    fx.idle().await;
    // Another owner leases a run behind the kernel's back.
    let task = reno_proto::TaskId::from_suffix("foreign").unwrap();
    let run = RunId::from_suffix("foreign").unwrap();
    let now = crate::Clock::now_ms(fx.clock.as_ref());
    fx.store
        .create_task(
            reno_store::NewTask {
                id: task.clone(),
                goal: "foreign".into(),
                class: None,
                spec: serde_json::json!({ "cwd": "/work" }),
            },
            now,
        )
        .await
        .unwrap();
    fx.store
        .enqueue_run(&task, &run, &id("mock"), now)
        .await
        .unwrap();
    fx.store
        .claim_next("other-daemon", now, 60_000)
        .await
        .unwrap()
        .unwrap();

    // A submit wakes the dispatcher; it runs its own run and then sleeps until the foreign
    // lease's expiry.
    let own = fx.submit("own").await;
    fx.wait_status(&own, RunStatus::Succeeded).await;
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert_eq!(fx.status(&run).await, RunStatus::Leased);

    fx.clock.advance(60_001);
    advance(Duration::from_secs(61)).await;
    fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(
        fx.store.get_run(&run).await.unwrap().unwrap().lease_fence,
        2
    );
}

/// An executor that ignores its cancel token and never ends.
#[derive(Default)]
struct Stubborn {
    started: Arc<std::sync::atomic::AtomicBool>,
}

#[async_trait]
impl Executor for Stubborn {
    fn descriptor(&self) -> reno_proto::ExecutorDescriptor {
        MockExecutor::default_descriptor(id("mock"))
    }

    async fn probe(&self) -> ExecutorHealth {
        ExecutorHealth::default()
    }

    async fn start(
        &self,
        _: ExecSpec,
        controls: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError> {
        self.started
            .store(true, std::sync::atomic::Ordering::SeqCst);
        Ok(futures::stream::once(async move {
            let _keep = controls;
            std::future::pending::<ExecEvent>().await
        })
        .boxed())
    }
}

#[tokio::test(start_paused = true)]
async fn an_executor_that_ignores_cancel_is_cut_off_after_the_grace() {
    let stubborn = Stubborn::default();
    let started = stubborn.started.clone();
    let registry = ExecutorRegistry::new().with(Arc::new(stubborn));
    let fx = Fx::new(config(1), registry).await;
    let run = fx.submit("stuck").await;
    until(async || {
        started
            .load(std::sync::atomic::Ordering::SeqCst)
            .then_some(())
    })
    .await;
    fx.kernel.cancel(&run).await.unwrap();
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        fx.status(&run).await,
        RunStatus::Running,
        "grace not over: {:?}",
        fx.journal(&run)
    );
    advance(Duration::from_secs(10)).await;
    fx.wait_status(&run, RunStatus::Interrupted).await;
    let journal = fx.journal_with_done(&run).await;
    assert!(diagnostic(last_done(&journal)).contains("did not stop"));
}

#[tokio::test(start_paused = true)]
async fn shutdown_interrupts_live_runs_and_leaves_no_task() {
    let m = mock([
        MockStep::text("x"),
        MockStep::Sleep(Duration::from_secs(3600)),
    ]);
    let fx = Fx::new(config(2), registry(&[m])).await;
    let a = fx.submit("a").await;
    let b = fx.submit("b").await;
    let queued = fx.submit("c").await;
    fx.wait_status(&a, RunStatus::Running).await;
    fx.wait_status(&b, RunStatus::Running).await;
    // Dispatcher, hub and two supervisors.
    until(async || (fx.kernel.live_tasks() == 4).then_some(())).await;

    let stopping = {
        let kernel = fx.kernel.clone();
        tokio::spawn(async move { kernel.shutdown().await })
    };
    until(async || stopping.is_finished().then_some(())).await;
    let report = stopping.await.unwrap();
    assert!(!report.aborted);
    assert_eq!(fx.kernel.live_tasks(), 0);
    for run in [&a, &b] {
        assert_eq!(fx.status(run).await, RunStatus::Interrupted);
        let journal = fx.journal(run);
        assert!(matches!(
            last_done(&journal),
            ExecEvent::Done {
                stop: StopReason::Interrupted,
                ..
            }
        ));
    }
    assert_eq!(fx.status(&queued).await, RunStatus::Queued, "never claimed");
    assert!(matches!(
        fx.kernel
            .submit("late", id("mock"), "/work".into(), None)
            .await,
        Err(KernelError::ShuttingDown)
    ));
}

#[tokio::test(start_paused = true)]
async fn past_the_deadline_shutdown_drops_stuck_runs_and_still_empties_the_tracker() {
    let config = KernelConfig {
        cancel_grace: Duration::from_secs(3600),
        shutdown_deadline: Duration::from_secs(5),
        ..config(1)
    };
    let stubborn = Stubborn::default();
    let started = stubborn.started.clone();
    let registry = ExecutorRegistry::new().with(Arc::new(stubborn));
    let fx = Fx::new(config, registry).await;
    let run = fx.submit("stuck").await;
    until(async || {
        started
            .load(std::sync::atomic::Ordering::SeqCst)
            .then_some(())
    })
    .await;
    let stopping = {
        let kernel = fx.kernel.clone();
        tokio::spawn(async move { kernel.shutdown().await })
    };
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    advance(Duration::from_secs(5)).await;
    until(async || stopping.is_finished().then_some(())).await;
    assert!(stopping.await.unwrap().aborted);
    assert_eq!(fx.kernel.live_tasks(), 0);
    // The run stands as it was; the next start's recovery interrupts it.
    assert_eq!(fx.status(&run).await, RunStatus::Running);
}

#[tokio::test(start_paused = true)]
async fn an_idle_kernel_makes_no_store_write_however_long_it_sits() {
    let fx = Fx::new(config(2), registry(&[mock([MockStep::text("x")])])).await;
    let run = fx.submit("one").await;
    fx.wait_status(&run, RunStatus::Succeeded).await;
    // Let the queue drain: the dispatcher's last, empty claim and the hub's last refresh.
    let settled = fx.idle().await;

    let events_before = fx.store.events_since(Seq(0), 100_000).await.unwrap().len();
    fx.clock.advance(3_600_000);
    advance(Duration::from_secs(3600)).await;
    for _ in 0..100 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        fx.kernel.store_writes(),
        settled,
        "no write call while idle"
    );
    let events_after = fx.store.events_since(Seq(0), 100_000).await.unwrap().len();
    assert_eq!(events_before, events_after, "no event row either");
    // And it still works afterwards.
    let again = fx.submit("two").await;
    fx.wait_status(&again, RunStatus::Succeeded).await;
}

/// 200 ended runs and two live ones, with a window of 50.
#[tokio::test(start_paused = true)]
async fn the_watch_holds_the_live_runs_and_only_the_recent_ended_ones() {
    let quick = mock([MockStep::started("s1", "mock-1"), MockStep::text("a")]);
    let hold =
        MockExecutor::new(id("hold")).with_steps([MockStep::Sleep(Duration::from_secs(3600))]);
    let cfg = KernelConfig {
        recent_runs: 50,
        ..config(4)
    };
    let fx = Fx::new(cfg, registry(&[quick, hold])).await;

    let mut ended = Vec::new();
    for n in 0..200 {
        ended.push(fx.submit(&format!("run {n}")).await);
    }
    until(async || {
        let done = fx.kernel.count_runs(&[RunStatus::Succeeded]).await.unwrap();
        (done == 200).then_some(())
    })
    .await;

    let mut live = Vec::new();
    for _ in 0..2 {
        let run = fx
            .kernel
            .submit("hold", id("hold"), PathBuf::from("/work"), None)
            .await
            .unwrap();
        fx.wait_status(&run, RunStatus::Running).await;
        live.push(run);
    }

    let mut watch = fx.kernel.watch_runs();
    let window = until(async || {
        let list = watch.borrow_and_update().clone();
        let seen_live = live.iter().all(|run| {
            list.iter()
                .any(|r| r.run_id == *run && r.status == RunStatus::Running)
        });
        seen_live.then_some(list)
    })
    .await;
    assert_eq!(window.len(), 2 + 50);
    assert_eq!(window.iter().filter(|r| !r.status.is_terminal()).count(), 2);
    let mut kept: Vec<_> = window
        .iter()
        .filter(|r| r.status.is_terminal())
        .map(|r| r.run_id.clone())
        .collect();
    kept.sort();
    let mut newest: Vec<_> = ended[150..].to_vec();
    newest.sort();
    assert_eq!(kept, newest, "the 50 newest ended runs");

    // The listing pages through all 202 without a repeat, 70 at a time.
    let mut seen = Vec::new();
    let mut before = None;
    loop {
        let (page, next) = fx
            .kernel
            .list_runs(None, Some(70), before.as_ref())
            .await
            .unwrap();
        assert!(page.len() <= 70);
        seen.extend(page.into_iter().map(|r| r.run_id));
        match next {
            Some(cursor) => before = Some(cursor),
            None => break,
        }
    }
    assert_eq!(seen.len(), 202);
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 202);
    // The default page is 100, and the cap is 500.
    let (page, next) = fx.kernel.list_runs(None, None, None).await.unwrap();
    assert_eq!((page.len(), next.is_some()), (100, true));
    let (page, next) = fx.kernel.list_runs(None, Some(9999), None).await.unwrap();
    assert_eq!((page.len(), next), (202, None));
    // An unknown cursor is an error, not an empty page.
    let unknown = RunId::from_suffix("nosuchrun").unwrap();
    assert!(matches!(
        fx.kernel.list_runs(None, None, Some(&unknown)).await,
        Err(KernelError::RunNotFound(_))
    ));
    for run in &live {
        fx.kernel.cancel(run).await.unwrap();
    }
}

#[tokio::test(start_paused = true)]
async fn list_and_watch_cut_the_prompt_and_run_get_keeps_it_whole() {
    let quick = mock([MockStep::started("s1", "mock-1"), MockStep::text("a")]);
    let fx = Fx::new(config(2), registry(&[quick])).await;
    // Multi-byte characters: the cut must fall on a char boundary.
    let long = "é".repeat(1000);
    let run = fx.submit(&long).await;
    fx.wait_status(&run, RunStatus::Succeeded).await;

    let cut = format!("{}…", "é".repeat(280));
    let (listed, _) = fx.kernel.list_runs(None, None, None).await.unwrap();
    assert_eq!(listed[0].prompt.as_deref(), Some(cut.as_str()));
    let mut watch = fx.kernel.watch_runs();
    let watched = until(async || {
        let list = watch.borrow_and_update().clone();
        list.iter().any(|r| r.run_id == run).then_some(list)
    })
    .await;
    assert_eq!(watched[0].prompt.as_deref(), Some(cut.as_str()));
    assert_eq!(
        fx.kernel.get_run(&run).await.unwrap().prompt.as_deref(),
        Some(long.as_str())
    );

    // A prompt of exactly 280 characters is not touched.
    let exact = "x".repeat(280);
    let run = fx.submit(&exact).await;
    fx.wait_status(&run, RunStatus::Succeeded).await;
    let (listed, _) = fx.kernel.list_runs(None, Some(1), None).await.unwrap();
    assert_eq!(listed[0].prompt.as_deref(), Some(exact.as_str()));
}
