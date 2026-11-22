#![cfg(test)]

//! Recovery at start, and the process records that feed its sweep. Unix only: the sweep ends a
//! real process group.

#![cfg(unix)]

use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Command, Stdio};

use reno_harness::exec::MockStep;
use reno_harness::proc::ProcessRecord;
use reno_proto::{ExecEvent, RunId, RunStatus, Seq, StopReason, TaskId};
use reno_store::{NewTask, Store};

use super::*;

/// A real `sleep` in its own process group, reaped by a thread the moment it dies.
struct Orphan {
    record: ProcessRecord,
    reaper: Option<std::thread::JoinHandle<std::process::ExitStatus>>,
}

impl Orphan {
    fn spawn() -> Self {
        let mut child = Command::new("sleep")
            .arg("300")
            .stdin(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let record = ProcessRecord::of_pid(child.id()).unwrap();
        assert_eq!(record.pid, record.pgid, "leads its own group");
        let reaper = std::thread::spawn(move || child.wait().unwrap());
        Self {
            record,
            reaper: Some(reaper),
        }
    }

    /// How the process died, once it has.
    async fn exit(mut self) -> std::process::ExitStatus {
        let reaper = self.reaper.take().unwrap();
        tokio::task::spawn_blocking(move || reaper.join().unwrap())
            .await
            .unwrap()
    }
}

impl Drop for Orphan {
    fn drop(&mut self) {
        if self.reaper.is_some() {
            // A failed test must not leave the process behind.
            let _ = Command::new("kill")
                .args(["-KILL", &format!("-{}", self.record.pgid)])
                .status();
        }
    }
}

fn ids(n: &str) -> (TaskId, RunId) {
    (
        TaskId::from_suffix(n).unwrap(),
        RunId::from_suffix(n).unwrap(),
    )
}

async fn queue(store: &Store, n: &str, at: i64) -> (TaskId, RunId) {
    let (task, run) = ids(n);
    store
        .create_task(
            NewTask {
                id: task.clone(),
                goal: format!("goal {n}"),
                class: None,
                spec: serde_json::json!({ "cwd": "/work" }),
            },
            at,
        )
        .await
        .unwrap();
    store
        .enqueue_run(&task, &run, &id("mock"), at)
        .await
        .unwrap();
    (task, run)
}

#[tokio::test(start_paused = true)]
async fn start_repairs_rows_sweeps_orphans_and_repairs_journals() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("reno.db")).unwrap();
    let orphan = Orphan::spawn();

    // The previous daemon's last picture, oldest run first (claims take the oldest).
    let (_, running) = queue(&store, "running", 1).await;
    let (_, leased) = queue(&store, "leased", 2).await;
    let (_, ended) = queue(&store, "ended", 3).await;
    let c = store.claim_next("old", 10, 60_000).await.unwrap().unwrap();
    assert_eq!(c.run_id, running);
    store
        .transition(&running, c.fence, RunStatus::Running, 11)
        .await
        .unwrap();
    store
        .record_process(
            &running,
            c.fence,
            u32::try_from(orphan.record.pid).unwrap(),
            u32::try_from(orphan.record.pgid).unwrap(),
            orphan.record.start_time,
            12,
        )
        .await
        .unwrap();
    let c = store.claim_next("old", 20, 60_000).await.unwrap().unwrap();
    assert_eq!(c.run_id, leased);
    let c = store.claim_next("old", 30, 60_000).await.unwrap().unwrap();
    assert_eq!(c.run_id, ended);
    for to in [
        RunStatus::Running,
        RunStatus::Evaluating,
        RunStatus::Succeeded,
    ] {
        store.transition(&ended, c.fence, to, 31).await.unwrap();
    }
    // Its journal has events but the crash came before the `Done`.
    let mut writer = store.journals().writer(&ended).unwrap();
    writer
        .append(&ExecEvent::Text { text: "hi".into() })
        .unwrap();
    writer.flush().unwrap();
    drop(writer);

    let fx = Fx::over(dir, store, config(1), registry(&[mock([])])).await;

    // The running run was interrupted; its process group was swept.
    assert_eq!(fx.status(&running).await, RunStatus::Interrupted);
    let status = orphan.exit().await;
    assert_eq!(status.signal(), Some(15), "ended by SIGTERM: {status:?}");
    // Its journal, which never existed, got the Done recovery owes it.
    let journal = fx.journal(&running);
    assert!(matches!(
        last_done(&journal),
        ExecEvent::Done {
            stop: StopReason::Interrupted,
            ..
        }
    ));

    // The leased run went back to the queue and, with a free slot, ran again at fence 2.
    fx.wait_status(&leased, RunStatus::Succeeded).await;
    assert_eq!(
        fx.store
            .get_run(&leased)
            .await
            .unwrap()
            .unwrap()
            .lease_fence,
        2
    );
    let requeued = fx
        .transitions(&leased)
        .await
        .iter()
        .any(|(from, to)| from == "leased" && to == "queued");
    assert!(requeued, "{:?}", fx.transitions(&leased).await);

    // The terminal run's journal was repaired from its row: Done after the lone Text.
    let journal = fx.journal(&ended);
    assert_eq!(journal.len(), 2);
    assert_eq!(journal[1].0, Seq(2));
    assert!(matches!(
        last_done(&journal),
        ExecEvent::Done {
            stop: StopReason::Completed,
            ..
        }
    ));
    assert_eq!(
        fx.status(&ended).await,
        RunStatus::Succeeded,
        "state untouched"
    );
}

#[tokio::test(start_paused = true)]
async fn recovery_never_signals_a_pid_that_now_belongs_to_someone_else() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("reno.db")).unwrap();
    let bystander = Orphan::spawn();
    let (_, run) = queue(&store, "stale", 1).await;
    let c = store.claim_next("old", 10, 60_000).await.unwrap().unwrap();
    store
        .transition(&run, c.fence, RunStatus::Running, 11)
        .await
        .unwrap();
    // The recorded start time is not the live process's: the pid was reused.
    store
        .record_process(
            &run,
            c.fence,
            u32::try_from(bystander.record.pid).unwrap(),
            u32::try_from(bystander.record.pgid).unwrap(),
            bystander.record.start_time + 1,
            12,
        )
        .await
        .unwrap();
    let fx = Fx::over(dir, store, config(1), registry(&[mock([])])).await;
    assert_eq!(fx.status(&run).await, RunStatus::Interrupted);
    assert!(
        ProcessRecord::of_pid(u32::try_from(bystander.record.pid).unwrap()).is_some(),
        "the bystander is still alive"
    );
}

#[tokio::test(start_paused = true)]
async fn a_reported_process_is_recorded_durably_with_its_start_time() {
    let orphan = Orphan::spawn();
    let m = mock([
        MockStep::Spawned(orphan.record),
        MockStep::Sleep(std::time::Duration::from_secs(60)),
    ]);
    let fx = Fx::new(config(1), registry(&[m])).await;
    let run = fx.submit("spawns").await;
    let row = until(async || {
        let row = fx.store.get_run(&run).await.unwrap().unwrap();
        row.process_start.is_some().then_some(row)
    })
    .await;
    assert_eq!(
        (row.pid, row.pgid, row.process_start),
        (
            u32::try_from(orphan.record.pid).ok(),
            u32::try_from(orphan.record.pgid).ok(),
            Some(orphan.record.start_time)
        )
    );
    assert!(
        fx.store
            .events_since(Seq(0), 1000)
            .await
            .unwrap()
            .iter()
            .any(|e| e.kind == "run.process_recorded")
    );
    // A daemon that died here would leave exactly this row, and the next start would sweep it:
    // end the process the way recovery would, so the test leaves nothing behind.
    let record = orphan.record;
    let swept = tokio::spawn(async move {
        reno_harness::proc::sweep_orphans_with_grace(
            &[record],
            std::time::Duration::from_millis(50),
        )
        .await
    });
    until(async || swept.is_finished().then_some(())).await;
    assert_eq!(swept.await.unwrap().killed, vec![orphan.record]);
    assert_eq!(orphan.exit().await.signal(), Some(15));
}

#[tokio::test(start_paused = true)]
async fn start_backfills_the_report_of_a_run_that_ended_before_reports_were_stored() {
    use reno_proto::ReportStatus;
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("reno.db")).unwrap();
    let (_, old) = queue(&store, "old", 1).await;
    let c = store.claim_next("old", 10, 60_000).await.unwrap().unwrap();
    for to in [
        RunStatus::Running,
        RunStatus::Evaluating,
        RunStatus::Succeeded,
    ] {
        store.transition(&old, c.fence, to, 20).await.unwrap();
    }
    let mut writer = store.journals().writer(&old).unwrap();
    writer
        .append(&ExecEvent::AgentReport {
            status: ReportStatus::Failed,
            summary: "could not finish".into(),
            details: None,
        })
        .unwrap();
    writer.flush().unwrap();
    drop(writer);

    let fx = Fx::over(dir, store, config(1), registry(&[mock([])])).await;
    let report = until(async || {
        fx.store
            .get_run(&old)
            .await
            .unwrap()
            .and_then(|run| run.report)
    })
    .await;
    assert_eq!(report.status, ReportStatus::Failed);
    assert_eq!(report.summary, "could not finish");
}

// ---- which runs keep their harness session --------------------------------------------------

/// What a recording executor saw when the kernel asked it which runs to keep.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sweep {
    keep: std::collections::HashSet<RunId>,
    /// Each watched run's status when the call came in, and when it returned.
    before: Vec<(RunId, RunStatus)>,
    after: Vec<(RunId, RunStatus)>,
    started_before: usize,
    started_after: usize,
}

#[derive(Clone)]
struct Sweeping {
    inner: reno_harness::exec::MockExecutor,
    store: Store,
    watched: Vec<RunId>,
    seen: std::sync::Arc<std::sync::Mutex<Vec<Sweep>>>,
}

impl Sweeping {
    fn seen(&self) -> Vec<Sweep> {
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    async fn statuses(&self) -> Vec<(RunId, RunStatus)> {
        let mut found = Vec::new();
        for run in &self.watched {
            let status = self.store.get_run(run).await.unwrap().unwrap().status;
            found.push((run.clone(), status));
        }
        found
    }
}

#[async_trait::async_trait]
impl reno_harness::exec::Executor for Sweeping {
    fn descriptor(&self) -> reno_proto::ExecutorDescriptor {
        self.inner.descriptor()
    }
    async fn probe(&self) -> reno_proto::HealthProbe {
        self.inner.probe().await
    }
    async fn start(
        &self,
        spec: reno_proto::ExecSpec,
        controls: reno_harness::exec::ExecControls,
    ) -> Result<futures::stream::BoxStream<'static, ExecEvent>, reno_harness::exec::ExecError> {
        self.inner.start(spec, controls).await
    }
    async fn retain_runs(&self, keep: &std::collections::HashSet<RunId>) {
        let before = self.statuses().await;
        let started_before = self.inner.log().specs.len();
        // Recovery must wait for this call: give anything that does not wait the time to run.
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        tokio::task::spawn_blocking(|| std::thread::sleep(std::time::Duration::from_millis(50)))
            .await
            .unwrap();
        let after = self.statuses().await;
        let started_after = self.inner.log().specs.len();
        self.seen
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(Sweep {
                keep: keep.clone(),
                before,
                after,
                started_before,
                started_after,
            });
    }
}

/// A run in `status`, made through the store the way a daemon that died would have left it.
async fn left_as(store: &Store, n: &str, at: i64, status: &str, session: bool) -> RunId {
    let (_, run) = queue(store, n, at).await;
    if status == "queued" {
        return run;
    }
    if status == "cancelled" {
        store
            .transition(&run, 0, RunStatus::Cancelled, at)
            .await
            .unwrap();
        return run;
    }
    let claim = store.claim_next("old", at, 60_000).await.unwrap().unwrap();
    assert_eq!(claim.run_id, run);
    if status == "leased" {
        return run;
    }
    store
        .transition(&run, claim.fence, RunStatus::Running, at)
        .await
        .unwrap();
    if session {
        store
            .record_session(
                &run,
                claim.fence,
                &reno_proto::SessionRef("sess".into()),
                at,
            )
            .await
            .unwrap();
    }
    match status {
        "running" => {}
        "interrupted" => {
            store
                .transition(&run, claim.fence, RunStatus::Interrupted, at)
                .await
                .unwrap();
        }
        "requeued" => {
            store
                .transition(&run, claim.fence, RunStatus::Interrupted, at)
                .await
                .unwrap();
            store.resume_run(&run, claim.fence, None, at).await.unwrap();
        }
        "failed" | "succeeded" => {
            let end = if status == "failed" {
                RunStatus::Failed
            } else {
                RunStatus::Succeeded
            };
            for to in [RunStatus::Evaluating, end] {
                store.transition(&run, claim.fence, to, at).await.unwrap();
            }
        }
        other => panic!("no such state {other}"),
    }
    run
}

async fn sweeping_start(
    dir: tempfile::TempDir,
    store: Store,
    watched: Vec<RunId>,
) -> (Fx, Sweeping) {
    let sweeping = Sweeping {
        inner: mock([]),
        store: store.clone(),
        watched,
        seen: Default::default(),
    };
    let registry = crate::ExecutorRegistry::new().with(std::sync::Arc::new(sweeping.clone()));
    (Fx::over(dir, store, config(1), registry).await, sweeping)
}

#[tokio::test(start_paused = true)]
async fn start_asks_each_executor_to_keep_only_the_runs_that_can_still_resume() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("reno.db")).unwrap();
    let failed = left_as(&store, "failed", 1, "failed", true).await;
    let interrupted = left_as(&store, "interrupted", 2, "interrupted", true).await;
    let running = left_as(&store, "running", 3, "running", false).await;
    let succeeded = left_as(&store, "succeeded", 4, "succeeded", true).await;
    let cancelled = left_as(&store, "cancelled", 5, "cancelled", false).await;
    let leased = left_as(&store, "leased", 6, "leased", false).await;
    // Queued again, so it goes last: the next claim would take it before any younger run.
    let requeued = left_as(&store, "requeued", 7, "requeued", true).await;
    let plain = left_as(&store, "plain", 8, "queued", false).await;
    let watched = [
        &failed,
        &interrupted,
        &running,
        &requeued,
        &succeeded,
        &cancelled,
        &leased,
        &plain,
    ]
    .map(Clone::clone)
    .to_vec();

    let (_fx, sweeping) = sweeping_start(dir, store, watched).await;

    let seen = sweeping.seen();
    assert_eq!(seen.len(), 1, "asked once per executor: {seen:?}");
    let keep: std::collections::HashSet<RunId> = [&failed, &interrupted, &running, &requeued]
        .into_iter()
        .cloned()
        .collect();
    assert_eq!(seen[0].keep, keep);
    for gone in [&succeeded, &cancelled, &leased, &plain] {
        assert!(!seen[0].keep.contains(gone));
    }
}

#[tokio::test(start_paused = true)]
async fn the_sweep_is_awaited_after_the_state_repair_and_before_any_resume_or_claim() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("reno.db")).unwrap();
    let running = left_as(&store, "running", 1, "running", true).await;
    let leased = left_as(&store, "leased", 2, "leased", false).await;
    let waiting = left_as(&store, "waiting", 3, "queued", false).await;
    let watched = vec![running.clone(), leased.clone(), waiting.clone()];

    let (fx, sweeping) = sweeping_start(dir, store, watched).await;

    let seen = sweeping.seen();
    assert_eq!(seen.len(), 1);
    let sweep = &seen[0];
    // Step 2 had run: the live run is interrupted and the leased one is queued again.
    assert_eq!(
        sweep.before,
        [
            (running.clone(), RunStatus::Interrupted),
            (leased.clone(), RunStatus::Queued),
            (waiting.clone(), RunStatus::Queued),
        ]
    );
    // Nothing moved while the executor was still at it: no resume, no claim.
    assert_eq!(sweep.after, sweep.before);
    assert_eq!((sweep.started_before, sweep.started_after), (0, 0));
    // Afterwards the work went on as before: the interrupted run was resumed, all three ran.
    for run in [&running, &leased, &waiting] {
        fx.wait_status(run, RunStatus::Succeeded).await;
    }
}
