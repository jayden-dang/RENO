//! The kernel façade: a cheap, clonable handle over the dispatcher, the run supervisors and the
//! runs hub.
//!
//! # Tasks and channels (ARCHITECTURE 17.2)
//!
//! | Task | Count | Owns | Inbox |
//! |---|---|---|---|
//! | dispatcher | 1 | the claim loop | `Notify` (no queue) and one deadline |
//! | runs hub | 1 | the `watch_runs` list | `Notify`, coalescing |
//! | definitions loader | 0 or 1 | the definitions in force | the port's watch, capacity 1 |
//! | run supervisor | one per live run | stream, journal, subscribers | `mpsc` of 16 commands |
//!
//! Per live run there are also the `ExecControls` queues (steering, answers and process records,
//! 16 each) and one queue of `subscriber_queue` (256) events per `run_events` subscriber. Replies
//! travel by `oneshot`, and no task sends to a task that sends back, so the bounded queues
//! cannot deadlock in a cycle (ARCH-11). Every task is spawned on one `TaskTracker`; every
//! run's cancel token is a child of the one root token.

use std::path::PathBuf;
use std::sync::Arc;

use futures::stream::BoxStream;
use reno_harness::exec::Decision;
use reno_proto::defs::{
    BotPin, DefKind, DefRecord, DefValidateResult, ValidationContext, validate_body,
};
use reno_proto::{
    APPROVAL_HISTORY_DEFAULT_LIMIT, APPROVAL_HISTORY_MAX_LIMIT, Approval, ApprovalDecision,
    ApprovalId, ApprovalRecord, ApprovalStatus, ExecutorId, RunEventItem, RunId, RunStatus,
    RunSummary, Seq, TaskId, TriggerFireResult, TriggerInfo,
};
use reno_store::{NewTask, StoreError};
use tokio::sync::{oneshot, watch};
use tokio::time::timeout;

use crate::approvals;
use crate::clock::Clock;
use crate::config::KernelConfig;
use crate::db::Db;
use crate::defs::{self, CurrentDefs, DefsSetup};
use crate::dispatcher;
use crate::error::{KernelError, Result};
use crate::events;
use crate::recovery;
use crate::registry::ExecutorRegistry;
use crate::scheduler::{self, SchedCmd};
use crate::shared::{Shared, SupCmd};

/// How often `submit` mints a new id after the store says the last one exists.
const MINT_ATTEMPTS: usize = 8;

/// What `shutdown` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShutdownReport {
    /// The deadline passed with runs still live, and they were dropped where they stood. Their
    /// rows stay non-terminal; the next start's recovery interrupts them.
    pub aborted: bool,
}

/// The daemon's brain. Clone it freely; every clone is the same kernel.
#[derive(Debug, Clone)]
pub struct Kernel {
    shared: Arc<Shared>,
}

impl Kernel {
    /// Recover from the previous run, then start the dispatcher.
    ///
    /// Unlike a plain constructor this is `async` and fallible: recovery reads and repairs the
    /// store before anything is claimed. Call [`shutdown`](Self::shutdown) to stop; dropping
    /// every handle does not stop the tasks.
    pub async fn start(
        config: KernelConfig,
        store: reno_store::Store,
        executors: ExecutorRegistry,
        clock: Arc<dyn Clock>,
    ) -> Result<Self> {
        Self::start_inner(config, store, executors, clock, None).await
    }

    /// Like [`start`](Self::start), and loads definitions through `definitions` (the
    /// `Definitions` port): the files are read once before this returns, and again whenever the
    /// port says they changed. Files that do not validate never stop the kernel: it runs on the
    /// last good set and publishes the errors ([`watch_defs`](Self::watch_defs)).
    pub async fn start_with_definitions(
        config: KernelConfig,
        store: reno_store::Store,
        executors: ExecutorRegistry,
        clock: Arc<dyn Clock>,
        definitions: DefsSetup,
    ) -> Result<Self> {
        Self::start_inner(config, store, executors, clock, Some(definitions)).await
    }

    async fn start_inner(
        config: KernelConfig,
        store: reno_store::Store,
        executors: ExecutorRegistry,
        clock: Arc<dyn Clock>,
        definitions: Option<DefsSetup>,
    ) -> Result<Self> {
        // Without a port the kernel still validates bodies, against the executors it has.
        let watch_port = definitions.as_ref().and_then(|setup| setup.watch.clone());
        let has_definitions = definitions.is_some();
        let context = match &definitions {
            Some(setup) => setup.context.clone(),
            None => ValidationContext::with_builtin(executors.ids().cloned()),
        };
        let shared = Arc::new(Shared::new(
            Db::new(store, clock),
            executors,
            config,
            context,
        ));
        recovery::recover(&shared).await?;
        shared.approvals.refresh(&shared.db).await?;
        if let Some(setup) = definitions {
            defs::start(&shared, setup).await?;
        }
        if has_definitions {
            scheduler::start(&shared, watch_port).await?;
        }
        shared.refresh_runs().await?;
        shared.spawn(dispatcher::run(Arc::clone(&shared)));
        shared.spawn(runs_hub(Arc::clone(&shared)));
        shared.wake.notify_one();
        Ok(Self { shared })
    }

    /// Create a task for `prompt` and queue a run of it on `executor`. The kernel mints both
    /// ids. An executor id the kernel does not know is accepted: the run fails when it is
    /// claimed, with the reason in its journal.
    pub async fn submit(
        &self,
        prompt: impl Into<String>,
        executor: ExecutorId,
        cwd: PathBuf,
        model: Option<String>,
    ) -> Result<RunId> {
        self.submit_pinned(prompt.into(), executor, cwd, model, None)
            .await
    }

    /// Create a task for `prompt` and queue a run of it as bot `bot`, pinned to the version of
    /// the bot's definition that is in force now; the pin never changes, whatever is edited
    /// later. With no `executor` the run goes to the first executor in the bot's list that the
    /// kernel has. An unknown bot is [`KernelError::DefinitionNotFound`].
    ///
    /// The run launches with the `access` and `sandbox` of the pinned bot version. With no
    /// `executor` given, the first executor in the bot's list that this kernel has **and that
    /// declares the bot's access mode** is chosen; when none does, the first one it has is
    /// recorded and the run fails at launch with a `NoEligibleExecutor` diagnostic rather than
    /// being started with a different access mode (ARCH-15). Choosing among the bot's executors
    /// by health and routing statistics is T1.5, and applying its instructions and budget is
    /// T1.5 and T1.7.
    pub async fn submit_for_bot(
        &self,
        bot: &str,
        prompt: impl Into<String>,
        executor: Option<ExecutorId>,
        cwd: PathBuf,
        model: Option<String>,
    ) -> Result<RunId> {
        let (executor, pin) = bot_launch(&self.shared, bot, executor).map_err(|e| match e {
            KernelError::NoExecutor(_) => KernelError::Invalid(e.to_string()),
            other => other,
        })?;
        self.submit_pinned(prompt.into(), executor, cwd, model, Some(pin))
            .await
    }

    async fn submit_pinned(
        &self,
        prompt: String,
        executor: ExecutorId,
        cwd: PathBuf,
        model: Option<String>,
        pin: Option<BotPin>,
    ) -> Result<RunId> {
        if self.shared.root.is_cancelled() {
            return Err(KernelError::ShuttingDown);
        }
        let cwd = cwd
            .to_str()
            .ok_or_else(|| KernelError::Invalid("the working directory is not UTF-8".into()))?
            .to_owned();
        let spec = serde_json::json!({ "cwd": cwd, "model": model });
        let db = &self.shared.db;

        let mut minted = None;
        for _ in 0..MINT_ATTEMPTS {
            let id = mint(&self.shared, TaskId::from_suffix)?;
            let task = NewTask {
                id: id.clone(),
                goal: prompt.clone(),
                class: None,
                spec: spec.clone(),
            };
            match db.create_task(task).await {
                Ok(()) => {
                    minted = Some(id);
                    break;
                }
                Err(StoreError::AlreadyExists { .. }) => {}
                Err(error) => return Err(error.into()),
            }
        }
        let task = minted.ok_or_else(|| KernelError::Invalid("no free task id".into()))?;

        for _ in 0..MINT_ATTEMPTS {
            let run = mint(&self.shared, RunId::from_suffix)?;
            let enqueued = match &pin {
                Some(pin) => db.enqueue_pinned_run(&task, &run, &executor, pin).await,
                None => db.enqueue_run(&task, &run, &executor).await,
            };
            match enqueued {
                Ok(()) => {
                    self.shared.wake.notify_one();
                    self.shared.mark_runs_dirty();
                    return Ok(run);
                }
                Err(StoreError::AlreadyExists { .. }) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(KernelError::Invalid("no free run id".into()))
    }

    /// The definitions in force, and the problems with the files on disk. Latest value wins: a
    /// slow reader skips versions, never queues them.
    pub fn watch_defs(&self) -> watch::Receiver<Arc<CurrentDefs>> {
        defs::subscribe(&self.shared)
    }

    /// Every trigger and its state. Latest value wins: a slow reader skips versions, never
    /// queues them.
    pub fn watch_triggers(&self) -> watch::Receiver<Arc<Vec<TriggerInfo>>> {
        self.shared.triggers_tx.subscribe()
    }

    /// Every trigger and its state now.
    pub fn list_triggers(&self) -> Vec<TriggerInfo> {
        self.shared.triggers_tx.borrow().as_ref().clone()
    }

    /// Fires a trigger now, whatever its schedule: a `manual` trigger or any enabled one. It
    /// does not move the schedule and is not held back by a backoff. The result says whether a
    /// run was started, merged into the trigger's running one, or was a duplicate.
    pub async fn fire_trigger(&self, name: &str) -> Result<TriggerFireResult> {
        if self.shared.root.is_cancelled() {
            return Err(KernelError::ShuttingDown);
        }
        let (reply, answer) = oneshot::channel();
        self.shared
            .sched_tx
            .send(SchedCmd::Fire {
                name: name.to_owned(),
                reply,
            })
            .await
            .map_err(|_| KernelError::ShuttingDown)?;
        answer.await.map_err(|_| KernelError::ShuttingDown)?
    }

    /// The definitions in force now.
    pub fn current_defs(&self) -> Arc<CurrentDefs> {
        self.shared.current_defs()
    }

    /// One stored version of a definition with its body: the newest when `version` is `None`.
    /// Versions that are no longer in force can still be read.
    pub async fn get_definition(
        &self,
        kind: DefKind,
        name: &str,
        version: Option<u32>,
    ) -> Result<DefRecord> {
        let db = &self.shared.db;
        let latest = db.latest_definition(kind, name).await?;
        let record = match (version, latest.as_ref()) {
            (None, found) => found.cloned(),
            (Some(want), Some(newest)) if newest.version == want => Some(newest.clone()),
            (Some(want), _) => db.get_definition(kind, name, want).await?,
        };
        let record = record.ok_or_else(|| defs::not_found(kind, name, version))?;
        Ok(DefRecord {
            kind: record.kind,
            name: record.name,
            version: record.version,
            hash: record.hash,
            body: record.body,
            created_at_ms: u64::try_from(record.created_at).unwrap_or(0),
            latest: latest.is_some_and(|newest| newest.version == record.version),
        })
    }

    /// Checks `body` against every rule as if it were saved as `name`, in the context of the
    /// definitions in force. Nothing is saved.
    pub fn validate_definition(
        &self,
        kind: DefKind,
        name: &str,
        body: &serde_json::Value,
    ) -> DefValidateResult {
        let current = self.shared.current_defs();
        let (errors, hash) = validate_body(current.set(), kind, name, body, &self.shared.defs_ctx);
        DefValidateResult {
            valid: errors.is_empty(),
            errors,
            hash,
        }
    }

    /// Cancel a run. Returns its status when the cancel was accepted.
    ///
    /// - A live run is told to stop (`ExecHandle::cancel`, through its token) and ends
    ///   `interrupted` once the executor's `Done` arrives, or after the cancel grace.
    /// - A `queued` or `interrupted` run becomes `cancelled`.
    /// - Anything else has nothing to cancel.
    pub async fn cancel(&self, run: &RunId) -> Result<RunStatus> {
        let mut last = RunStatus::Queued;
        for _ in 0..3 {
            if let Some(token) = self.shared.live_cancel_token(run) {
                token.cancel();
                return Ok(self.shared.run_record(run).await?.status);
            }
            let row = self.shared.run_record(run).await?;
            last = row.status;
            match row.status {
                RunStatus::Queued | RunStatus::Interrupted => {
                    match self
                        .shared
                        .db
                        .transition(run, row.lease_fence, RunStatus::Cancelled)
                        .await
                    {
                        Ok(()) => {
                            self.shared.notify_status_changed();
                            self.shared.mark_runs_dirty();
                            return Ok(RunStatus::Cancelled);
                        }
                        // The dispatcher claimed it first: look again, it is live now.
                        Err(
                            StoreError::StaleFence { .. } | StoreError::IllegalTransition { .. },
                        ) => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                // Claimed an instant ago, or leased by someone else: look again.
                RunStatus::Leased => {}
                _ => break,
            }
            tokio::task::yield_now().await;
        }
        Err(KernelError::NotCancellable {
            run: run.clone(),
            status: last,
        })
    }

    /// One run.
    pub async fn get_run(&self, run: &RunId) -> Result<RunSummary> {
        Ok(crate::shared::summarize_full(
            &self.shared.run_record(run).await?,
        ))
    }

    /// One page of runs, newest first, optionally only those in `status`. `limit` defaults to
    /// 100 and is capped at 500; `before` is the cursor (the previous page's `next`), and an
    /// unknown one is [`KernelError::RunNotFound`]. Prompts are cut as in the watch; [`get_run`]
    /// has the whole prompt. The second value is the cursor for the next page, if any.
    ///
    /// [`get_run`]: Self::get_run
    pub async fn list_runs(
        &self,
        status: Option<&[RunStatus]>,
        limit: Option<u32>,
        before: Option<&RunId>,
    ) -> Result<(Vec<RunSummary>, Option<RunId>)> {
        self.shared.summaries(status, limit, before).await
    }

    /// How many runs are in one of `status`.
    pub async fn count_runs(&self, status: &[RunStatus]) -> Result<u64> {
        Ok(self.shared.db.count_runs(status).await?)
    }

    /// The live view, refreshed after every change: every non-terminal run (at most
    /// `KernelConfig::watch_limit`) and the newest `KernelConfig::recent_runs` terminal ones,
    /// newest first, with prompts cut. Latest value wins: a slow reader skips versions, never
    /// queues them.
    pub fn watch_runs(&self) -> watch::Receiver<Vec<RunSummary>> {
        self.shared.runs_tx.subscribe()
    }

    /// A run's events from `from` (inclusive; the start when `None`): the journal first, then the
    /// live events, in order and without a gap. The stream ends after the run's `Done`, or with
    /// [`KernelError::Gap`] if the subscriber fell behind.
    pub async fn run_events(
        &self,
        run: &RunId,
        from: Option<Seq>,
    ) -> Result<BoxStream<'static, Result<RunEventItem>>> {
        events::run_events(Arc::clone(&self.shared), run.clone(), from).await
    }

    /// Queue a steering prompt for a live run's executor. Errors if the run is not live. Once
    /// the executor takes it, the supervisor journals it as `ExecEvent::UserMessage`.
    pub async fn steer(&self, run: &RunId, prompt: impl Into<String>) -> Result<()> {
        let commands = self
            .shared
            .live_commands(run)
            .ok_or_else(|| KernelError::RunEnded(run.clone()))?;
        let (reply, answer) = oneshot::channel();
        commands
            .send(SupCmd::Steer {
                prompt: prompt.into(),
                reply,
            })
            .await
            .map_err(|_| KernelError::RunEnded(run.clone()))?;
        answer
            .await
            .map_err(|_| KernelError::RunEnded(run.clone()))?
    }

    /// Approvals waiting for a human, oldest first, as the store holds them.
    pub async fn pending_approvals(&self) -> Result<Vec<Approval>> {
        Ok(self
            .shared
            .db
            .pending_approvals()
            .await?
            .iter()
            .map(approvals::approval)
            .collect())
    }

    /// The pending approvals, refreshed from the store on every change. Latest value wins.
    pub fn watch_approvals(&self) -> watch::Receiver<Vec<Approval>> {
        self.shared.approvals.subscribe()
    }

    /// Decided approvals, newest decision first: who decided each, and by which rule. Only
    /// `run`'s when given. `limit` defaults to 50 and is capped at 500.
    pub async fn approval_history(
        &self,
        run: Option<&RunId>,
        limit: Option<u32>,
    ) -> Result<Vec<ApprovalRecord>> {
        let limit = limit
            .unwrap_or(APPROVAL_HISTORY_DEFAULT_LIMIT)
            .clamp(1, APPROVAL_HISTORY_MAX_LIMIT);
        Ok(self
            .shared
            .db
            .approval_history(run, limit)
            .await?
            .iter()
            .map(approvals::record)
            .collect())
    }

    /// Answer a pending approval. The decision is stored (decided by a human), the executor gets
    /// it, then the run goes back to `running`.
    pub async fn decide(&self, approval: &ApprovalId, decision: ApprovalDecision) -> Result<()> {
        let row = self
            .shared
            .db
            .get_approval(approval)
            .await?
            .filter(|row| row.status == ApprovalStatus::Pending)
            .ok_or_else(|| KernelError::ApprovalNotFound(approval.clone()))?;
        let run = row.run_id;
        let commands = self
            .shared
            .live_commands(&run)
            .ok_or_else(|| KernelError::RunEnded(run.clone()))?;
        let (reply, answer) = oneshot::channel();
        commands
            .send(SupCmd::Decide {
                approval: approval.clone(),
                decision: match decision {
                    ApprovalDecision::ApproveOnce => Decision::Allow,
                    ApprovalDecision::Deny => Decision::Deny,
                },
                reply,
            })
            .await
            .map_err(|_| KernelError::RunEnded(run.clone()))?;
        answer.await.map_err(|_| KernelError::RunEnded(run))?
    }

    /// Stop: no more claims, every live run cancelled (each ends `interrupted`), then wait up to
    /// `KernelConfig::shutdown_deadline` for the tasks to finish. Past the deadline the rest are
    /// dropped where they stand. Either way no task of this kernel is left running.
    pub async fn shutdown(&self) -> ShutdownReport {
        let shared = &self.shared;
        shared.root.cancel();
        shared.tracker.close();
        let aborted = timeout(shared.config.shutdown_deadline, shared.tracker.wait())
            .await
            .is_err();
        if aborted {
            tracing::warn!("shutdown deadline passed; dropping live runs");
            shared.abort.cancel();
            shared.tracker.wait().await;
            shared.live_clear();
        }
        // Whatever the supervisors that were dropped left pending is closed here; recovery would
        // close it at the next start, but a view that shows an approval nobody can answer is
        // wrong now.
        if let Err(error) = recovery::expire_orphans(shared).await {
            tracing::warn!(%error, "could not close the pending approvals at shutdown");
        }
        if let Err(error) = shared.refresh_runs().await {
            tracing::warn!(%error, "final run list refresh failed");
        }
        ShutdownReport { aborted }
    }

    /// Tasks of this kernel still alive: the dispatcher, the runs hub and every supervisor.
    /// Zero after [`shutdown`](Self::shutdown).
    pub fn live_tasks(&self) -> usize {
        self.shared.tracker.len()
    }

    /// Store write calls the kernel has made (for the idle test).
    #[cfg(test)]
    pub(crate) fn store_writes(&self) -> u64 {
        self.shared.db.writes()
    }
}

/// The executor and bot pin a run for `bot` starts with: the executor given, or the first in the
/// bot's list that this kernel has and that declares the bot's access mode, else the first it
/// has (the run then fails at launch with `NoEligibleExecutor`, ARCH-15). The pin is the bot's
/// version in force now.
pub(crate) fn bot_launch(
    shared: &Shared,
    bot: &str,
    executor: Option<ExecutorId>,
) -> Result<(ExecutorId, BotPin)> {
    let current = shared.current_defs();
    let def = current
        .set()
        .bot(bot)
        .ok_or_else(|| defs::not_found(DefKind::Bot, bot, None))?;
    let stored = current
        .version_of(DefKind::Bot, bot)
        .ok_or_else(|| defs::not_found(DefKind::Bot, bot, None))?;
    let eligible = |id: &&ExecutorId| {
        shared.executors.get(id).is_some_and(|executor| {
            dispatcher::no_eligible_executor(&executor.descriptor(), def.access).is_none()
        })
    };
    let executor = match executor {
        Some(executor) => executor,
        None => def
            .executors
            .iter()
            .find(eligible)
            .or_else(|| {
                def.executors
                    .iter()
                    .find(|id| shared.executors.get(id).is_some())
            })
            .cloned()
            .ok_or_else(|| KernelError::NoExecutor(bot.to_owned()))?,
    };
    let pin = BotPin {
        name: bot.to_owned(),
        version: stored.version,
    };
    Ok((executor, pin))
}

/// Mint an id until it parses (it always does: the suffix is hex).
pub(crate) fn mint<T>(
    shared: &Shared,
    from_suffix: fn(&str) -> std::result::Result<T, reno_proto::IdError>,
) -> Result<T> {
    from_suffix(&shared.mint.suffix(shared.db.now_ms()))
        .map_err(|error| KernelError::Invalid(error.to_string()))
}

/// The runs hub: reloads the `watch_runs` list after changes. Bursts coalesce into one reload.
async fn runs_hub(shared: Arc<Shared>) {
    loop {
        tokio::select! {
            biased;
            () = shared.root.cancelled() => break,
            () = shared.runs_dirty.notified() => {}
        }
        if let Err(error) = shared.refresh_runs().await {
            tracing::warn!(%error, "could not refresh the run list");
        }
    }
}
