//! State the kernel's tasks share, and the one place that spawns them.
//!
//! The mutex around `live` guards a small table and is never held across an `.await`.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use reno_harness::exec::Decision;
use reno_proto::{ApprovalId, ExecEvent, RunId, RunStatus, RunSummary, Seq, StopReason};
use reno_store::RunRecord;
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::approvals::Approvals;
use crate::config::KernelConfig;
use crate::db::Db;
use crate::error::{KernelError, Result};
use crate::ids::IdMint;
use crate::registry::ExecutorRegistry;

/// Commands a run supervisor accepts. Bounded (`COMMAND_QUEUE`); the sender waits for room, and
/// the supervisor never sends to its callers, so the queue cannot form a cycle (ARCH-11).
pub(crate) const COMMAND_QUEUE: usize = 16;
/// Steering, answer and process-record queues of one run's `ExecControls`.
pub(crate) const CONTROL_QUEUE: usize = 16;

#[derive(Debug)]
pub(crate) enum SupCmd {
    /// Register a `run_events` subscriber and say which journal seq it starts at.
    Subscribe {
        reply: oneshot::Sender<Subscription>,
    },
    /// Answer an approval.
    Decide {
        approval: ApprovalId,
        decision: Decision,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Queue a steering prompt for the executor.
    Steer {
        prompt: String,
        reply: oneshot::Sender<Result<()>>,
    },
}

/// What a supervisor hands a new subscriber.
#[derive(Debug)]
pub(crate) struct Subscription {
    /// The journal already holds every event below this seq (flushed); everything from it on
    /// arrives on `rx`.
    pub(crate) next_seq: Seq,
    pub(crate) rx: mpsc::Receiver<(Seq, ExecEvent)>,
    /// Zero while `rx` is complete; otherwise the seq of the first event it dropped.
    pub(crate) overflow: Arc<AtomicU64>,
}

/// A run that holds a slot: claimed, and not yet ended.
#[derive(Debug)]
pub(crate) struct LiveRun {
    /// Child of the root token. The executor's `ExecControls` carry the same token, so
    /// cancelling it is `ExecHandle::cancel`, and it works from the claim on, before the
    /// executor has started.
    pub(crate) cancel: CancellationToken,
    pub(crate) cmd: mpsc::Sender<SupCmd>,
}

/// A run's place in the `live` table. Dropping it frees the slot, drops the run's pending
/// approvals and wakes the dispatcher, on every path: normal end, panic, or a task dropped by
/// the shutdown deadline.
#[derive(Debug)]
pub(crate) struct Slot {
    shared: Arc<Shared>,
    run: RunId,
}

impl Slot {
    /// Take a slot for `run`.
    pub(crate) fn take(shared: &Arc<Shared>, run: RunId, live: LiveRun) -> Self {
        shared.live_insert(run.clone(), live);
        Self {
            shared: Arc::clone(shared),
            run,
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.shared.approvals.remove_run(&self.run);
        self.shared.live_remove(&self.run);
    }
}

pub(crate) struct Shared {
    pub(crate) db: Db,
    pub(crate) executors: ExecutorRegistry,
    pub(crate) config: KernelConfig,
    /// Cancelled by `shutdown`. Every run's token is its child.
    pub(crate) root: CancellationToken,
    /// Cancelled when the shutdown deadline passes: tasks drop their work where they stand.
    pub(crate) abort: CancellationToken,
    pub(crate) tracker: TaskTracker,
    /// Wakes the dispatcher: a run was submitted or a slot freed.
    pub(crate) wake: Notify,
    /// Wakes the runs hub: some run changed.
    pub(crate) runs_dirty: Notify,
    live: Mutex<HashMap<RunId, LiveRun>>,
    /// Bumped whenever a run enters or leaves `live`, for subscribers waiting for a queued run.
    pub(crate) live_changed: watch::Sender<u64>,
    pub(crate) approvals: Approvals,
    pub(crate) runs_tx: watch::Sender<Vec<RunSummary>>,
    pub(crate) mint: IdMint,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared")
            .field("config", &self.config)
            .field("live", &self.live_len())
            .finish_non_exhaustive()
    }
}

impl Shared {
    pub(crate) fn new(db: Db, executors: ExecutorRegistry, mut config: KernelConfig) -> Self {
        config.max_concurrent = config.max_concurrent.max(1);
        config.subscriber_queue = config.subscriber_queue.max(1);
        Self {
            db,
            executors,
            config,
            root: CancellationToken::new(),
            abort: CancellationToken::new(),
            tracker: TaskTracker::new(),
            wake: Notify::new(),
            runs_dirty: Notify::new(),
            live: Mutex::new(HashMap::new()),
            live_changed: watch::channel(0).0,
            approvals: Approvals::new(),
            runs_tx: watch::channel(Vec::new()).0,
            mint: IdMint::default(),
        }
    }

    /// Spawn `task` on the tracker. When the shutdown deadline passes the task is dropped
    /// where it stands, so the tracker always empties.
    pub(crate) fn spawn(&self, task: impl Future<Output = ()> + Send + 'static) {
        let abort = self.abort.clone();
        self.tracker.spawn(async move {
            tokio::select! {
                biased;
                () = abort.cancelled() => {}
                () = task => {}
            }
        });
    }

    fn live_lock(&self) -> MutexGuard<'_, HashMap<RunId, LiveRun>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn live_len(&self) -> usize {
        self.live_lock().len()
    }

    pub(crate) fn is_live(&self, run: &RunId) -> bool {
        self.live_lock().contains_key(run)
    }

    pub(crate) fn live_insert(&self, run: RunId, live: LiveRun) {
        self.live_lock().insert(run, live);
        self.live_changed.send_modify(|n| *n += 1);
    }

    /// Free a run's slot and tell the dispatcher and the runs hub.
    pub(crate) fn live_remove(&self, run: &RunId) {
        self.live_lock().remove(run);
        self.live_changed.send_modify(|n| *n += 1);
        self.wake.notify_one();
        self.runs_dirty.notify_one();
    }

    /// Wake `run_events` subscribers that wait for a queued run: its status changed without it
    /// ever entering the live table (it was cancelled).
    pub(crate) fn notify_status_changed(&self) {
        self.live_changed.send_modify(|n| *n += 1);
    }

    pub(crate) fn live_clear(&self) {
        self.live_lock().clear();
        self.live_changed.send_modify(|n| *n += 1);
    }

    pub(crate) fn live_cancel_token(&self, run: &RunId) -> Option<CancellationToken> {
        self.live_lock().get(run).map(|live| live.cancel.clone())
    }

    pub(crate) fn live_commands(&self, run: &RunId) -> Option<mpsc::Sender<SupCmd>> {
        self.live_lock().get(run).map(|live| live.cmd.clone())
    }

    /// Something about a run changed: refresh the `watch_runs` list.
    pub(crate) fn mark_runs_dirty(&self) {
        self.runs_dirty.notify_one();
    }

    /// The newest `limit` runs of the given statuses, newest first.
    pub(crate) async fn summaries(
        &self,
        status: Option<&[RunStatus]>,
        limit: Option<u32>,
    ) -> Result<Vec<RunSummary>> {
        let mut rows = self.db.list_runs(None).await?;
        rows.reverse();
        let limit = limit.map_or(usize::MAX, |n| usize::try_from(n).unwrap_or(usize::MAX));
        Ok(rows
            .iter()
            .filter(|row| status.is_none_or(|wanted| wanted.contains(&row.status)))
            .take(limit)
            .map(summarize)
            .collect())
    }

    /// Reload the run list into the `watch`.
    pub(crate) async fn refresh_runs(&self) -> Result<()> {
        let list = self.summaries(None, Some(self.config.watch_limit)).await?;
        self.runs_tx.send_replace(list);
        Ok(())
    }

    pub(crate) async fn run_record(&self, run: &RunId) -> Result<RunRecord> {
        self.db
            .get_run(run)
            .await?
            .ok_or_else(|| KernelError::RunNotFound(run.clone()))
    }
}

/// The first line of a prompt, cut to a length a list can show.
fn title(goal: &str) -> String {
    goal.lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(200)
        .collect()
}

/// The model a task asked for, from its spec (`{"cwd": .., "model": ..}`, written by `submit`).
fn requested_model(spec: &serde_json::Value) -> Option<String> {
    spec.get("model")?.as_str().map(str::to_owned)
}

/// A run row as a client sees it.
///
/// `model` is the row's model when the store has one, otherwise the model the task asked for;
/// `prompt` is the task's goal. `stop_reason` is derived from the status, because the store keeps no stop reason in Phase 0:
/// `failed` reads as `error` even when the executor said `refusal` or `max_tokens`, and
/// `cancelled` as `interrupted`. The journal's `Done` carries the exact reason.
pub(crate) fn summarize(row: &RunRecord) -> RunSummary {
    let stop_reason = match row.status {
        RunStatus::Succeeded => Some(StopReason::Completed),
        RunStatus::Failed => Some(StopReason::Error),
        RunStatus::Interrupted | RunStatus::Cancelled => Some(StopReason::Interrupted),
        _ => None,
    };
    RunSummary {
        run_id: row.id.clone(),
        task_id: row.task_id.clone(),
        executor: row.executor.clone(),
        status: row.status,
        attempt: u32::try_from(row.lease_fence.max(1)).unwrap_or(u32::MAX),
        title: title(&row.goal),
        prompt: Some(row.goal.clone()),
        model: row.model.clone().or_else(|| requested_model(&row.spec)),
        stop_reason,
        created_at_ms: u64::try_from(row.created_at).unwrap_or(0),
        updated_at_ms: u64::try_from(row.updated_at).unwrap_or(0),
    }
}
