//! State the kernel's tasks share, and the one place that spawns them.
//!
//! The mutex around `live` guards a small table and is never held across an `.await`.

use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use reno_harness::exec::Decision;
use reno_proto::defs::ValidationContext;
use reno_proto::{
    ApprovalId, ApprovalOutcome, ApprovalSubject, ExecEvent, PROMPT_SUMMARY_CHARS,
    RUN_LIST_DEFAULT_LIMIT, RUN_LIST_MAX_LIMIT, ResolvedBy, RunId, RunStatus, RunSummary, Seq,
    SessionRef, StopDetail, StopReason, TriggerInfo,
};
use reno_store::{RunRecord, StoreError};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::approvals::Approvals;
use crate::budget::Pauses;
use crate::config::KernelConfig;
use crate::db::Db;
use crate::defs::CurrentDefs;
use crate::error::{KernelError, Result};
use crate::health::HealthBoard;
use crate::ids::IdMint;
use crate::link::RunLink;
use crate::registry::ExecutorRegistry;
use crate::scheduler::{COMMAND_QUEUE as SCHED_QUEUE, SchedCmd};
use crate::secrets::Secrets;
use crate::wake::WakeSignal;
use crate::workspace::Workspace;

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
        /// The reason, for a denial.
        message: Option<String>,
        /// Keep the answer as a standing rule.
        remember: Option<reno_proto::Remember>,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Queue a steering prompt for the executor.
    Steer {
        prompt: String,
        reply: oneshot::Sender<Result<()>>,
    },
    /// Ask the executor to switch the run to a model. The reply carries the model the harness
    /// resolved it to, once the change is journaled and stored.
    SetModel {
        model: String,
        reply: oneshot::Sender<Result<String>>,
    },
    /// Journal a structured report the run made through Reno's MCP server.
    Report {
        event: ExecEvent,
        reply: oneshot::Sender<Result<()>>,
    },
    /// An approval the run asked for through Reno's MCP server. The answer comes on `reply`
    /// when it is decided, expires or the run ends; the supervisor holds the sender until then.
    AgentApproval {
        subject: ApprovalSubject,
        reply: oneshot::Sender<AgentDecision>,
    },
}

/// How an approval the run asked for itself ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AgentDecision {
    pub(crate) outcome: ApprovalOutcome,
    pub(crate) by: ResolvedBy,
    pub(crate) message: Option<String>,
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

/// Why a live run was told to stop, written before its token is cancelled so the run's end can
/// say who stopped it. The first reason wins.
#[derive(Debug, Clone, Default)]
pub(crate) struct StopCause(Arc<OnceLock<StopDetail>>);

impl Stopper {
    fn of(live: &LiveRun) -> Self {
        Self {
            cancel: live.cancel.clone(),
            cause: live.cause.clone(),
        }
    }
}

impl StopCause {
    /// The reason, if one was given.
    pub(crate) fn get(&self) -> Option<StopDetail> {
        self.0.get().cloned()
    }
}

/// What stops a live run, with the reason it is stopped for.
#[derive(Debug, Clone)]
pub(crate) struct Stopper {
    cancel: CancellationToken,
    cause: StopCause,
}

impl Stopper {
    /// Records `why` (unless an earlier reason stands) and cancels the run.
    pub(crate) fn stop(&self, why: StopDetail) {
        let _ = self.cause.0.set(why);
        self.cancel.cancel();
    }
}

/// A run that holds a slot: claimed, and not yet ended.
#[derive(Debug)]
pub(crate) struct LiveRun {
    /// Child of the root token. The executor's `ExecControls` carry the same token, so
    /// cancelling it is `ExecHandle::cancel`, and it works from the claim on, before the
    /// executor has started.
    pub(crate) cancel: CancellationToken,
    /// Why the token was cancelled, when something other than the shutdown did it.
    pub(crate) cause: StopCause,
    pub(crate) cmd: mpsc::Sender<SupCmd>,
    /// The bot the run is pinned to, which a bot's pause (`stop_live`) is matched against.
    pub(crate) bot: Option<String>,
    /// The lease fence the run was claimed under, which its lease is renewed against.
    pub(crate) fence: u64,
}

/// A run's place in the `live` table. Dropping it frees the slot and wakes the dispatcher, on
/// every path: normal end, panic, or a task dropped by the shutdown deadline. A run's pending
/// approvals are closed in the store by its supervisor when it ends, and by shutdown and
/// recovery for the ones a dropped or crashed supervisor left behind.
#[derive(Debug)]
pub(crate) struct Slot {
    shared: Arc<Shared>,
    run: RunId,
    /// The executor the run stands on, which an install or update of it waits for (EX-5) and is
    /// refused by (EX-4). Released with the slot.
    claim: Option<crate::maintain::RunClaim>,
}

impl Slot {
    /// Take a slot for `run`.
    pub(crate) fn take(shared: &Arc<Shared>, run: RunId, live: LiveRun) -> Self {
        shared.live_insert(run.clone(), live);
        Self {
            shared: Arc::clone(shared),
            run,
            claim: None,
        }
    }

    /// Makes the run stand on the executor `claim` is for, releasing the one it stood on.
    pub(crate) fn hold(&mut self, claim: Option<crate::maintain::RunClaim>) {
        self.claim = claim;
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.shared.live_remove(&self.run);
    }
}

pub(crate) struct Shared {
    pub(crate) db: Db,
    pub(crate) executors: ExecutorRegistry,
    /// What the last probe found about each executor.
    pub(crate) health: HealthBoard,
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
    /// The definitions in force and the problems with the files. Written by the definitions
    /// loader only.
    pub(crate) defs_tx: watch::Sender<Arc<CurrentDefs>>,
    /// What validation may assume about the world: the executors a bot may name.
    pub(crate) defs_ctx: ValidationContext,
    pub(crate) mint: IdMint,
    /// Where write runs work; `None` runs everything in place.
    pub(crate) workspace: Option<Arc<dyn Workspace>>,
    /// Where a bot's secrets come from; `None` when the daemon has no secret store.
    pub(crate) secrets: Option<Arc<dyn Secrets>>,
    /// Raised when the system woke from sleep: the wall clock may have jumped while every
    /// monotonic timer stood still. The scheduler, the dispatcher and each supervisor listen
    /// (ARCH-10); `None` when the daemon does not report it.
    pub(crate) system_wake: Option<WakeSignal>,
    /// How a run's own `reno mcp` reaches the daemon; `None` injects nothing.
    pub(crate) link: Option<Arc<dyn RunLink>>,
    /// The scheduler's inbox. Senders wait when it is full; the scheduler never waits on them.
    pub(crate) sched_tx: mpsc::Sender<SchedCmd>,
    sched_rx: Mutex<Option<mpsc::Receiver<SchedCmd>>>,
    /// Every trigger and its state, as the scheduler last published it.
    pub(crate) triggers_tx: watch::Sender<Arc<Vec<TriggerInfo>>>,
    /// What is paused. Written by `pause` and `resume` only; the dispatcher and the scheduler
    /// read it.
    pub(crate) pauses_tx: watch::Sender<Arc<Pauses>>,
    /// Installs and updates of executors; set once by `Kernel::attach_maintenance`.
    pub(crate) maint: OnceLock<Arc<crate::maintain::Maintainer>>,
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
    /// The project of `dir`: what the `Workspace` port says (ARCH-7: the kernel touches no
    /// disk), except that Reno's own scratch directory is in none, even when something above it
    /// is a repository, because what runs there is not work on that repository. A kernel with no
    /// port has no projects.
    pub(crate) async fn project_of(&self, dir: &std::path::Path) -> Option<std::path::PathBuf> {
        if self
            .config
            .scratch_root
            .as_ref()
            .is_some_and(|root| dir.starts_with(root))
        {
            return None;
        }
        self.workspace.as_ref()?.project_of(dir).await
    }

    pub(crate) fn new(
        db: Db,
        executors: ExecutorRegistry,
        mut config: KernelConfig,
        defs_ctx: ValidationContext,
        workspace: Option<Arc<dyn Workspace>>,
        link: Option<Arc<dyn RunLink>>,
    ) -> Self {
        config.max_concurrent = config.max_concurrent.max(1);
        config.subscriber_queue = config.subscriber_queue.max(1);
        let (sched_tx, sched_rx) = mpsc::channel(SCHED_QUEUE);
        Self {
            db,
            executors,
            health: HealthBoard::new(),
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
            defs_tx: watch::channel(Arc::default()).0,
            defs_ctx,
            mint: IdMint::default(),
            workspace,
            secrets: None,
            system_wake: None,
            link,
            sched_tx,
            sched_rx: Mutex::new(Some(sched_rx)),
            triggers_tx: watch::channel(Arc::default()).0,
            pauses_tx: watch::channel(Arc::default()).0,
            maint: OnceLock::new(),
        }
    }

    /// The scheduler's inbox, once; `None` if a scheduler already has it.
    pub(crate) fn take_sched_commands(&self) -> Option<mpsc::Receiver<SchedCmd>> {
        self.sched_rx
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Tells the scheduler about a run. A full inbox makes the caller wait (backpressure), so a
    /// start or a failure is never lost; the scheduler never waits on the dispatcher (it only
    /// wakes it through `Notify`), so the wait cannot form a cycle (ARCH-11). A closed inbox
    /// means the scheduler has stopped (shutdown) and the signal no longer matters.
    pub(crate) async fn signal_scheduler(&self, command: SchedCmd) {
        if self.sched_tx.send(command).await.is_err() {
            tracing::debug!("the scheduler has stopped; dropping a run signal");
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

    /// Every live run with the fence it holds its lease under.
    pub(crate) fn live_fences(&self) -> Vec<(RunId, u64)> {
        self.live_lock()
            .iter()
            .map(|(run, live)| (run.clone(), live.fence))
            .collect()
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

    pub(crate) fn live_stopper(&self, run: &RunId) -> Option<Stopper> {
        self.live_lock().get(run).map(Stopper::of)
    }

    /// The stoppers of the live runs of `bot` (every live run when `None`), with their ids.
    pub(crate) fn live_of(&self, bot: Option<&str>) -> Vec<(RunId, Stopper)> {
        self.live_lock()
            .iter()
            .filter(|(_, live)| bot.is_none_or(|name| live.bot.as_deref() == Some(name)))
            .map(|(run, live)| (run.clone(), Stopper::of(live)))
            .collect()
    }

    pub(crate) fn live_commands(&self, run: &RunId) -> Option<mpsc::Sender<SupCmd>> {
        self.live_lock().get(run).map(|live| live.cmd.clone())
    }

    /// Something about a run changed: refresh the `watch_runs` list.
    pub(crate) fn mark_runs_dirty(&self) {
        self.runs_dirty.notify_one();
    }

    /// One page of runs, newest first: at most `limit` (default and cap from the protocol),
    /// older than `before`. The second value is the cursor for the next page, when there is one.
    pub(crate) async fn summaries(
        &self,
        status: Option<&[RunStatus]>,
        limit: Option<u32>,
        before: Option<&RunId>,
    ) -> Result<(Vec<RunSummary>, Option<RunId>)> {
        let limit = limit
            .unwrap_or(RUN_LIST_DEFAULT_LIMIT)
            .clamp(1, RUN_LIST_MAX_LIMIT);
        // One extra row says whether another page follows.
        let mut rows = self
            .db
            .list_runs_page(status.unwrap_or_default(), before, limit.saturating_add(1))
            .await
            .map_err(|error| match (error, before) {
                (StoreError::NotFound { .. }, Some(cursor)) => {
                    KernelError::RunNotFound(cursor.clone())
                }
                (error, _) => KernelError::from(error),
            })?;
        let more = rows.len() > usize::try_from(limit).unwrap_or(usize::MAX);
        rows.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
        let next = if more {
            rows.last().map(|row| row.id.clone())
        } else {
            None
        };
        Ok((rows.iter().map(summarize).collect(), next))
    }

    /// Reload the live view into the `watch`: every non-terminal run (at most `watch_limit`) and
    /// the newest `recent_runs` terminal ones, newest first. Two indexed, bounded queries; the
    /// history is never read whole.
    pub(crate) async fn refresh_runs(&self) -> Result<()> {
        const TERMINAL: [RunStatus; 3] = [
            RunStatus::Succeeded,
            RunStatus::Failed,
            RunStatus::Cancelled,
        ];
        let open: Vec<RunStatus> = RunStatus::ALL
            .into_iter()
            .filter(|s| !s.is_terminal())
            .collect();
        let mut rows = self
            .db
            .list_runs_page(&open, None, self.config.watch_limit)
            .await?;
        rows.extend(
            self.db
                .list_runs_page(&TERMINAL, None, self.config.recent_runs)
                .await?,
        );
        // Stable: runs created in the same millisecond keep each query's own newest-first order.
        rows.sort_by_key(|row| std::cmp::Reverse(row.created_at));
        self.runs_tx
            .send_replace(rows.iter().map(summarize).collect());
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

/// The working directory a task was submitted with, from its spec.
fn requested_cwd(spec: &serde_json::Value) -> Option<PathBuf> {
    spec.get("cwd")?.as_str().map(PathBuf::from)
}

/// The model a task asked for, from its spec (`{"cwd": .., "model": ..}`, written by `submit`).
fn requested_model(spec: &serde_json::Value) -> Option<String> {
    spec.get("model")?.as_str().map(str::to_owned)
}

/// The effort a task asked for, from its spec (a word, written by `submit`).
fn requested_effort(spec: &serde_json::Value) -> Option<reno_proto::Effort> {
    reno_proto::Effort::from_word(spec.get("effort")?.as_str()?)
}

/// `prompt` cut to `PROMPT_SUMMARY_CHARS` characters (on a char boundary), with `…` when cut.
fn truncate_prompt(prompt: &str) -> String {
    match prompt.char_indices().nth(PROMPT_SUMMARY_CHARS) {
        None => prompt.to_owned(),
        Some((end, _)) => format!("{}…", &prompt[..end]),
    }
}

/// A run row as a list or the watch shows it: `prompt` cut to `PROMPT_SUMMARY_CHARS`
/// characters. [`summarize_full`] is the whole row, for `run.get`.
pub(crate) fn summarize(row: &RunRecord) -> RunSummary {
    RunSummary {
        prompt: Some(truncate_prompt(&row.goal)),
        ..summarize_full(row)
    }
}

/// A run row as a client sees it.
///
/// `model` is the row's model when the store has one, otherwise the model the task asked for;
/// `prompt` is the task's whole goal. `stop_reason` is derived from the status, because the store keeps no stop reason in Phase 0:
/// `failed` reads as `error` even when the executor said `refusal` or `max_tokens`, and
/// `cancelled` as `interrupted`. The journal's `Done` carries the exact reason.
pub(crate) fn summarize_full(row: &RunRecord) -> RunSummary {
    let stop_reason = match row.status {
        RunStatus::Succeeded => Some(StopReason::Completed),
        RunStatus::Failed => Some(StopReason::Error),
        RunStatus::Interrupted | RunStatus::Cancelled => Some(StopReason::Interrupted),
        _ => None,
    };
    RunSummary {
        model_options: Default::default(),
        effort: requested_effort(&row.spec),
        run_id: row.id.clone(),
        task_id: row.task_id.clone(),
        executor: row.executor.clone(),
        status: row.status,
        attempt: u32::try_from(row.lease_fence.max(1)).unwrap_or(u32::MAX),
        title: title(&row.goal),
        prompt: Some(row.goal.clone()),
        model: row.model.clone().or_else(|| requested_model(&row.spec)),
        stop_reason,
        stop: row.stop.clone(),
        not_resumed: row
            .not_resumed
            .clone()
            .filter(|_| row.status == RunStatus::Interrupted),
        session: row.session_ref.clone().map(SessionRef),
        bot: row.bot.clone(),
        cwd: requested_cwd(&row.spec),
        report: row.report.clone(),
        created_at_ms: u64::try_from(row.created_at).unwrap_or(0),
        updated_at_ms: u64::try_from(row.updated_at).unwrap_or(0),
    }
}
