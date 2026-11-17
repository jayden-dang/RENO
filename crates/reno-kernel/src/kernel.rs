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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures::stream::BoxStream;
use reno_harness::exec::Decision;
use reno_proto::defs::{
    BotPin, DefKind, DefRecord, DefValidateResult, ValidationContext, validate_body,
};
use reno_proto::{
    APPROVAL_HISTORY_DEFAULT_LIMIT, APPROVAL_HISTORY_MAX_LIMIT, APPROVAL_MESSAGE_MAX_CHARS,
    Approval, ApprovalDecision, ApprovalId, ApprovalRecord, ApprovalStatus, CancelledBy, CleanItem,
    CleanTarget, Effort, ExecutorId, ExecutorInfo, RULE_MAX_EXPIRY_SECS, Remember, RoutingDecision,
    RuleId, RunEventItem, RunId, RunStatus, RunSummary, Seq, StopDetail, TaskId, TriggerFireResult,
    TriggerInfo, WorkspaceInfo, WorkspaceStatus,
};
use reno_proto::{
    ActionClass, CommandPattern, RULE_ADD_COMMAND_MAX_CHARS, RULE_ADD_DEFAULT_TOOL, RuleMatcher,
    StandingEffect, StandingRule, StandingScope,
};
use reno_store::{NewRule, NewTask, Note, StoreError};

use tokio::sync::{oneshot, watch};
use tokio::time::timeout;

use crate::approvals;
use crate::budget;
use crate::clock::Clock;
use crate::config::KernelConfig;
use crate::db::Db;
use crate::defs::{self, CurrentDefs, DefsSetup};
use crate::dispatcher;
use crate::effort;
use crate::error::{KernelError, Result};
use crate::events;
use crate::health;
use crate::link::RunLink;
use crate::recovery;
use crate::registry::ExecutorRegistry;
use crate::resume;
use crate::router::{self, BotPreference, Candidate, RouteRequest};
use crate::scheduler::{self, SchedCmd};
use crate::shared::{Shared, SupCmd};
use crate::workspace::Workspace;
use crate::workspaces;

/// How often `submit` mints a new id after the store says the last one exists.
const MINT_ATTEMPTS: usize = 8;

/// The most ended runs the one-time report backfill looks at, newest first.
const BACKFILL_MAX_RUNS: usize = 20_000;

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
    /// The shared state, for the kernel's own modules.
    pub(crate) fn shared(&self) -> &Arc<Shared> {
        &self.shared
    }

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
        Self::start_inner(config, store, executors, clock, None, None, None).await
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
        Self::start_inner(
            config,
            store,
            executors,
            clock,
            Some(definitions),
            None,
            None,
        )
        .await
    }

    /// The general form of [`start`](Self::start): with or without definitions, and with or
    /// without a `Workspace` port. With the port, a write run works in a git worktree and is
    /// refused where it cannot be isolated (see [`Placement`](crate::Placement)); without it,
    /// every run works in the directory its task named.
    pub async fn start_with(
        config: KernelConfig,
        store: reno_store::Store,
        executors: ExecutorRegistry,
        clock: Arc<dyn Clock>,
        definitions: Option<DefsSetup>,
        workspace: Option<Arc<dyn Workspace>>,
    ) -> Result<Self> {
        Self::start_inner(
            config,
            store,
            executors,
            clock,
            definitions,
            workspace,
            None,
        )
        .await
    }

    /// The general form of [`start_with`](Self::start_with), with the [`RunLink`] port: every run
    /// is started with Reno's own MCP server first in `ExecSpec::mcp`, holding a token bound to
    /// that run and revoked when it ends (see [`RunLink`]). Without the port nothing is injected.
    pub async fn start_linked(
        config: KernelConfig,
        store: reno_store::Store,
        executors: ExecutorRegistry,
        clock: Arc<dyn Clock>,
        definitions: Option<DefsSetup>,
        workspace: Option<Arc<dyn Workspace>>,
        link: Option<Arc<dyn RunLink>>,
    ) -> Result<Self> {
        Self::start_inner(
            config,
            store,
            executors,
            clock,
            definitions,
            workspace,
            link,
        )
        .await
    }

    async fn start_inner(
        config: KernelConfig,
        store: reno_store::Store,
        executors: ExecutorRegistry,
        clock: Arc<dyn Clock>,
        definitions: Option<DefsSetup>,
        workspace: Option<Arc<dyn Workspace>>,
        link: Option<Arc<dyn RunLink>>,
    ) -> Result<Self> {
        // Without a port the kernel still validates bodies, against the executors it has.
        let watch_port = definitions.as_ref().and_then(|setup| setup.watch.clone());
        let wake = definitions.as_ref().and_then(|setup| setup.wake.clone());
        let secrets = definitions.as_ref().and_then(|setup| setup.secrets.clone());
        let has_definitions = definitions.is_some();
        let context = match &definitions {
            Some(setup) => setup.context.clone(),
            None => ValidationContext::with_builtin(executors.ids().cloned()),
        };
        let mut shared = Shared::new(
            Db::new(store, clock),
            executors,
            config,
            context,
            workspace,
            link,
        );
        shared.secrets = secrets;
        shared.system_wake.clone_from(&wake);
        let shared = Arc::new(shared);
        recovery::recover(&shared).await?;
        workspaces::sweep(&shared).await?;
        shared.load_executor_health().await?;
        shared.approvals.refresh(&shared.db).await?;
        budget::load(&shared).await?;
        if let Some(setup) = definitions {
            defs::start(&shared, setup).await?;
        }
        if has_definitions {
            scheduler::start(&shared, watch_port, wake).await?;
        }
        shared.refresh_runs().await?;
        shared.spawn(dispatcher::run(Arc::clone(&shared)));
        shared.spawn(runs_hub(Arc::clone(&shared)));
        shared.wake.notify_one();
        // Reports of runs that ended before the column existed: filled in the background, once,
        // after the daemon is up, so a large store does not slow the start.
        let backfill = Arc::clone(&shared);
        shared.spawn(async move {
            match backfill.db.backfill_reports(BACKFILL_MAX_RUNS).await {
                Ok(reno_store::Backfill::Ran { filled, .. }) if filled > 0 => {
                    if let Err(error) = backfill.refresh_runs().await {
                        tracing::warn!(%error, "could not refresh runs after the report backfill");
                    }
                }
                Ok(_) => {}
                Err(error) => tracing::warn!(%error, "could not backfill run reports"),
            }
        });
        Ok(Self { shared })
    }

    /// Create a task for `prompt` and queue a run of it on `executor`. The kernel mints both
    /// ids. An executor id the kernel does not know is accepted: the run fails when it is
    /// claimed, with the reason in its journal. The routing decision (the executor named, so
    /// only it is a candidate) is stored with the run.
    pub async fn submit(
        &self,
        prompt: impl Into<String>,
        executor: ExecutorId,
        cwd: PathBuf,
        model: Option<String>,
    ) -> Result<RunId> {
        let request = TaskRequest::new(prompt, cwd)
            .executor(executor)
            .model(model);
        Ok(self.submit_task(request).await?.0)
    }

    /// Create a task for `prompt` and queue a run of it as bot `bot`, pinned to the version of
    /// the bot's definition that is in force now; the pin never changes, whatever is edited
    /// later. With no `executor` the router chooses among the bot's executors (see
    /// [`route`](crate::route)). An unknown bot is [`KernelError::DefinitionNotFound`].
    ///
    /// The run launches with the `access`, `sandbox` and `instructions` of the pinned bot
    /// version. When no executor of the bot is eligible the run is still recorded, with a
    /// decision that has none chosen, and fails at launch with a `NoEligibleExecutor`
    /// diagnostic rather than being started with a different access mode (ARCH-15).
    pub async fn submit_for_bot(
        &self,
        bot: &str,
        prompt: impl Into<String>,
        executor: Option<ExecutorId>,
        cwd: PathBuf,
        model: Option<String>,
    ) -> Result<RunId> {
        let mut request = TaskRequest::new(prompt, cwd).bot(bot).model(model);
        request.executor = executor;
        match self.submit_task(request).await {
            Ok((run, _)) => Ok(run),
            // A bot none of whose executors is here: the caller named a bot that cannot run.
            Err(error @ KernelError::NoExecutor(_)) => Err(KernelError::Invalid(error.to_string())),
            Err(error) => Err(error),
        }
    }

    /// Route `request` and queue a run of it. The decision is stored with the run in one
    /// transaction and returned. A task with neither an executor nor a bot goes to the router's
    /// default order. [`KernelError::NoExecutor`] when there is nothing it could even be
    /// recorded against: no executor is registered, or none of its bot's is.
    pub async fn submit_task(&self, request: TaskRequest) -> Result<(RunId, RoutingDecision)> {
        if self.shared.root.is_cancelled() {
            return Err(KernelError::ShuttingDown);
        }
        if let Some(bot) = request.bot.as_deref()
            && let Some(stop) = budget::token_refusal(&self.shared, bot).await?
        {
            return Err(KernelError::BudgetExhausted {
                bot: bot.to_owned(),
                stop,
            });
        }
        let planned = plan_route(
            &self.shared,
            request.bot.as_deref(),
            request.executor.clone(),
            request.model.as_deref(),
            request.effort,
        )?;
        let cwd = request
            .cwd
            .to_str()
            .ok_or_else(|| KernelError::Invalid("the working directory is not UTF-8".into()))?
            .to_owned();
        // The model the run asks for, fixed now with the bot version it is pinned to: the
        // task's own, else the bot's, else none (the executor's default).
        let mut spec = serde_json::json!({
            "cwd": cwd,
            "model": planned.model,
            "effort": planned.effort.map(Effort::as_str),
        });
        if request.in_place {
            spec["inPlace"] = serde_json::Value::Bool(true);
        }
        let db = &self.shared.db;

        let mut minted = None;
        for _ in 0..MINT_ATTEMPTS {
            let id = mint(&self.shared, TaskId::from_suffix)?;
            let task = NewTask {
                id: id.clone(),
                goal: request.prompt.clone(),
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
            let enqueued = db
                .enqueue_routed_run(
                    &task,
                    &run,
                    &planned.executor,
                    planned.pin.as_ref(),
                    &planned.decision,
                )
                .await;
            match enqueued {
                Ok(()) => {
                    self.shared.wake.notify_one();
                    self.shared.mark_runs_dirty();
                    return Ok((run, planned.decision));
                }
                Err(StoreError::AlreadyExists { .. }) => {}
                Err(error) => return Err(error.into()),
            }
        }
        Err(KernelError::Invalid("no free run id".into()))
    }

    /// The kill switch: nothing new is claimed for `bot` (everything when `None`), and its
    /// triggers are held. Runs already live go on, unless `stop_live`, which cancels them (each
    /// ends `interrupted`). Persisted: a restart keeps it. See the module `budget`.
    pub async fn pause(
        &self,
        bot: Option<&str>,
        stop_live: bool,
    ) -> Result<reno_proto::BudgetPauseResult> {
        budget::pause(&self.shared, bot, stop_live).await
    }

    /// Lifts the pause of `bot` (the global pause when `None`). Held runs are claimed and held
    /// triggers catch up.
    pub async fn resume(&self, bot: Option<&str>) -> Result<reno_proto::BudgetResumeResult> {
        budget::resume(&self.shared, bot).await
    }

    /// Every bot's limits and use today, and what is paused.
    pub async fn budget_status(&self) -> Result<reno_proto::BudgetStatusResult> {
        budget::status(&self.shared).await
    }

    /// The decision `request` would get now: the router run over the executors and the health
    /// this kernel holds. Nothing is created, stored or probed.
    pub fn preview_route(&self, request: &TaskRequest) -> Result<RoutingDecision> {
        Ok(plan_route(
            &self.shared,
            request.bot.as_deref(),
            request.executor.clone(),
            request.model.as_deref(),
            request.effort,
        )?
        .decision)
    }

    /// The routing decision stored for `run`, with the fallback steps taken so far. `None` for a
    /// run made before routing existed.
    pub async fn run_routing(&self, run: &RunId) -> Result<Option<RoutingDecision>> {
        self.shared.run_record(run).await?;
        Ok(self.shared.db.run_routing(run).await?)
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

    /// Every executor with its descriptor and the health of its last probe. Latest value wins:
    /// a slow reader skips versions, never queues them. The health stored by the previous daemon
    /// is shown until this one has probed.
    pub fn watch_executors(&self) -> watch::Receiver<Arc<Vec<ExecutorInfo>>> {
        self.shared.health.subscribe()
    }

    /// Every executor and its last health now, by id.
    pub fn executors(&self) -> Vec<ExecutorInfo> {
        self.shared.health.list()
    }

    /// Probes `id` (every executor when `None`) again, waits for the answers and returns the
    /// list. Probes never overlap for one executor and at most a few run at once. Errors with
    /// [`KernelError::ExecutorNotFound`] for an id that is not registered.
    pub async fn refresh_executors(&self, id: Option<&ExecutorId>) -> Result<Vec<ExecutorInfo>> {
        if self.shared.root.is_cancelled() {
            return Err(KernelError::ShuttingDown);
        }
        self.shared
            .probe_executors(id, health::Probing::Fresh)
            .await?;
        Ok(self.executors())
    }

    /// The frames each executor's wire layer did not recognise since the daemon started, by kind,
    /// for `id` or for every executor. Errors with [`KernelError::ExecutorNotFound`] for an id
    /// that is not registered.
    pub fn executor_frames(
        &self,
        id: Option<&ExecutorId>,
    ) -> Result<Vec<reno_proto::ExecutorFrameCounts>> {
        self.shared.executor_frames(id)
    }

    /// Probes every executor in the background, on the kernel's own tasks. The daemon calls it
    /// once when it starts; nothing probes on a timer (ARCH-10).
    pub fn probe_executors_in_background(&self) {
        let shared = Arc::clone(&self.shared);
        self.shared.spawn(async move {
            if let Err(error) = shared.probe_executors(None, health::Probing::Shared).await {
                tracing::debug!(%error, "the start-up probe of the executors ended early");
            }
        });
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
            if let Some(stopper) = self.shared.live_stopper(run) {
                stopper.stop(StopDetail::Cancelled {
                    by: CancelledBy::User,
                });
                return Ok(self.shared.run_record(run).await?.status);
            }
            let row = self.shared.run_record(run).await?;
            last = row.status;
            match row.status {
                RunStatus::Queued | RunStatus::Interrupted => {
                    match self
                        .shared
                        .db
                        .transition_noted(
                            run,
                            row.lease_fence,
                            RunStatus::Cancelled,
                            Note {
                                stop: Some(StopDetail::Cancelled {
                                    by: CancelledBy::User,
                                }),
                                not_resumed: None,
                            },
                        )
                        .await
                    {
                        Ok(()) => {
                            workspaces::release_after_run(&self.shared, run).await;
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

    /// Resume `run`: an `interrupted` run, or a `failed` one that has a session, is queued again
    /// to continue its harness session, with `prompt` (or the standard continue prompt). The run
    /// keeps its id, and its next attempt is the number returned. Not resumable (the status, no
    /// session, an executor that cannot): [`KernelError::NotResumable`].
    pub async fn resume_run(&self, run: &RunId, prompt: Option<String>) -> Result<u32> {
        resume::manual(&self.shared, run, prompt).await
    }

    /// The workspace of `run`, when it has one: it ran, or runs, in a worktree.
    pub async fn workspace(&self, run: &RunId) -> Result<WorkspaceInfo> {
        self.shared
            .db
            .get_workspace(run)
            .await?
            .ok_or_else(|| KernelError::WorkspaceNotFound(run.clone()))
    }

    /// Workspaces in one of `status` (all when `None`), newest first.
    pub async fn workspaces(
        &self,
        status: Option<&[WorkspaceStatus]>,
    ) -> Result<Vec<WorkspaceInfo>> {
        Ok(self
            .shared
            .db
            .list_workspaces(status.unwrap_or_default())
            .await?)
    }

    /// Removes workspaces: see `CleanTarget`. Without `force`, only what holds nothing to lose
    /// (or, for `Merged` and a single run, what is merged) goes, and a branch with unmerged
    /// commits is never deleted. A single run whose run has not ended is
    /// [`WorkspaceError::InUse`](crate::WorkspaceError::InUse).
    pub async fn clean_workspaces(
        &self,
        target: CleanTarget,
        force: bool,
    ) -> Result<Vec<CleanItem>> {
        if self.shared.root.is_cancelled() {
            return Err(KernelError::ShuttingDown);
        }
        workspaces::clean(&self.shared, target, force).await
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

    /// Switch a live run to `model` from the harness's next model call. Returns the model the
    /// harness resolved it to (`sonnet` becomes `claude-sonnet-5-5`); the run's journal then
    /// holds `ExecEvent::ModelChanged`, its summary shows the model, and a resumed attempt
    /// starts on it.
    ///
    /// Refused with [`KernelError::RunEnded`] when the run is not live,
    /// [`KernelError::Invalid`] when its executor cannot switch a model or does not know
    /// `model` (an id of its model list or an alias of its own, as at submit; an executor that
    /// lists no models accepts any name and lets the harness decide), and with the harness's own
    /// reason when it refuses.
    pub async fn set_model(&self, run: &RunId, model: impl Into<String>) -> Result<String> {
        let model = model.into();
        if model.trim().is_empty() {
            return Err(KernelError::Invalid("the model is empty".into()));
        }
        let record = self.shared.run_record(run).await?;
        let commands = self
            .shared
            .live_commands(run)
            .ok_or_else(|| KernelError::RunEnded(run.clone()))?;
        let descriptor = self
            .shared
            .executors
            .get(&record.executor)
            .ok_or_else(|| KernelError::ExecutorNotFound(record.executor.clone()))?
            .descriptor();
        if !descriptor.caps.model_switch {
            return Err(KernelError::Invalid(format!(
                "{} cannot change a running run's model",
                record.executor
            )));
        }
        let candidate = Candidate {
            descriptor,
            health: self.shared.live_health(&record.executor),
        };
        if !knows_model(&candidate, &model) {
            return Err(KernelError::Invalid(format!(
                "{} has no model {model:?}",
                record.executor
            )));
        }
        let (reply, answer) = oneshot::channel();
        commands
            .send(SupCmd::SetModel { model, reply })
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
        self.decide_with_message(approval, decision, None).await
    }

    /// [`Self::decide`] with the reason for a denial. The reason is trimmed (blank is none),
    /// stored with the decision, journalled in `ApprovalResolved.message`, and sent to the
    /// harness the way a rule's message is. `Invalid` for a reason with an approval, or one
    /// longer than [`APPROVAL_MESSAGE_MAX_CHARS`] characters.
    pub async fn decide_with_message(
        &self,
        approval: &ApprovalId,
        decision: ApprovalDecision,
        message: Option<String>,
    ) -> Result<()> {
        self.decide_remembering(approval, decision, message, None)
            .await
    }

    /// [`Self::decide_with_message`], and with `remember` the answer is also kept as a standing
    /// rule (see `reno_proto::rules`), in the same transaction as the decision. The rule's
    /// matcher is derived from the approval's own subject; its scope is the run, the run's bot or
    /// everything. `Invalid` for scope `bot` on a run with no bot, and for an expiry of zero or
    /// beyond [`RULE_MAX_EXPIRY_SECS`]; nothing is decided in that case.
    pub async fn decide_remembering(
        &self,
        approval: &ApprovalId,
        decision: ApprovalDecision,
        message: Option<String>,
        remember: Option<Remember>,
    ) -> Result<()> {
        if let Some(secs) = remember.as_ref().and_then(|r| r.expires_in_secs)
            && !(1..=RULE_MAX_EXPIRY_SECS).contains(&secs)
        {
            return Err(KernelError::Invalid(format!(
                "a rule's expiry is between 1 and {RULE_MAX_EXPIRY_SECS} seconds"
            )));
        }
        let message = message
            .map(|text| text.trim().to_owned())
            .filter(|text| !text.is_empty());
        if let Some(text) = &message {
            if decision != ApprovalDecision::Deny {
                return Err(KernelError::Invalid(
                    "only a denial takes a message".to_owned(),
                ));
            }
            if text.chars().count() > APPROVAL_MESSAGE_MAX_CHARS {
                return Err(KernelError::Invalid(format!(
                    "a denial message is at most {APPROVAL_MESSAGE_MAX_CHARS} characters"
                )));
            }
        }
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
                message,
                remember,
                reply,
            })
            .await
            .map_err(|_| KernelError::RunEnded(run.clone()))?;
        answer.await.map_err(|_| KernelError::RunEnded(run))?
    }

    /// The standing rules, newest first: only those in force now unless `include_inactive`.
    pub async fn list_rules(&self, include_inactive: bool) -> Result<Vec<StandingRule>> {
        Ok(self.shared.db.list_rules(include_inactive, None).await?)
    }

    /// [`Self::list_rules`], only the rules whose scope is the project of `dir`: any directory of
    /// it will do (see `crate::project`). A directory in no project lists nothing.
    pub async fn list_project_rules(
        &self,
        dir: &Path,
        include_inactive: bool,
    ) -> Result<Vec<StandingRule>> {
        let Some(root) = self.shared.project_of(dir).await else {
            return Ok(Vec::new());
        };
        Ok(self
            .shared
            .db
            .list_rules(include_inactive, Some(&root))
            .await?)
    }

    /// Make a standing rule for the project of `dir` from a command line, as if a person had
    /// answered that command's approval there with "always in this project": action `command`,
    /// `tool` (default [`RULE_ADD_DEFAULT_TOOL`]), the line exactly (trimmed), no paths. The
    /// project is resolved here; `Invalid` for a directory in none, an empty or multi-line
    /// command, one over [`RULE_ADD_COMMAND_MAX_CHARS`], an empty `tool`, and an expiry outside
    /// 1 to [`RULE_MAX_EXPIRY_SECS`] seconds.
    pub async fn add_project_rule(
        &self,
        dir: &Path,
        effect: StandingEffect,
        command: &str,
        tool: Option<&str>,
        expires_in_secs: Option<u64>,
    ) -> Result<StandingRule> {
        let command = command.trim();
        if command.is_empty() || command.contains(['\n', '\r']) {
            return Err(KernelError::Invalid(
                "a rule's command is one non-empty line".to_owned(),
            ));
        }
        if command.chars().count() > RULE_ADD_COMMAND_MAX_CHARS {
            return Err(KernelError::Invalid(format!(
                "a rule's command is at most {RULE_ADD_COMMAND_MAX_CHARS} characters"
            )));
        }
        let tool = tool.map_or(RULE_ADD_DEFAULT_TOOL, str::trim);
        if tool.is_empty() {
            return Err(KernelError::Invalid(
                "a rule's tool is not empty".to_owned(),
            ));
        }
        if let Some(secs) = expires_in_secs
            && !(1..=RULE_MAX_EXPIRY_SECS).contains(&secs)
        {
            return Err(KernelError::Invalid(format!(
                "a rule's expiry is between 1 and {RULE_MAX_EXPIRY_SECS} seconds"
            )));
        }
        let root = self.shared.project_of(dir).await.ok_or_else(|| {
            KernelError::Invalid(format!(
                "{} is not in a git repository, so it is in no project",
                dir.display()
            ))
        })?;
        let now = self.shared.db.now_ms();
        let id = RuleId::from_suffix(&self.shared.mint.suffix(now))
            .map_err(|error| KernelError::Invalid(format!("cannot mint a rule id: {error}")))?;
        self.shared
            .db
            .create_rule(NewRule {
                id: id.clone(),
                scope: StandingScope::Project { root },
                effect,
                matcher: RuleMatcher {
                    action: ActionClass::Command,
                    tool: tool.to_owned(),
                    command: Some(CommandPattern::Exact {
                        command: command.to_owned(),
                    }),
                    paths: Vec::new(),
                },
                expires_at: expires_in_secs.map(|secs| {
                    now.saturating_add(i64::try_from(secs.saturating_mul(1000)).unwrap_or(i64::MAX))
                }),
                source_approval: None,
            })
            .await?;
        self.shared
            .db
            .get_rule(&id)
            .await?
            .ok_or(KernelError::RuleNotFound(id))
    }

    /// Revoke a standing rule for good. `RuleNotFound` for an unknown id; revoking a revoked
    /// rule is a no-op. A run that is judging a request at this moment may still see the rule.
    pub async fn revoke_rule(&self, id: &RuleId) -> Result<()> {
        match self.shared.db.revoke_rule(id).await {
            Err(StoreError::NotFound { .. }) => Err(KernelError::RuleNotFound(id.clone())),
            other => Ok(other?),
        }
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

    /// Deadline timers of `kind` armed so far (for tests that advance paused time).
    #[cfg(test)]
    pub(crate) fn armed_count(&self, kind: crate::db::Armed) -> u64 {
        self.shared.db.armed_count(kind)
    }
}

/// A task as a caller states it, before the router has chosen its executor.
#[derive(Debug, Clone)]
pub struct TaskRequest {
    /// The prompt.
    pub prompt: String,
    /// The executor the task names. With it the router has one candidate; without it the bot's
    /// list, or the default order, decides.
    pub executor: Option<ExecutorId>,
    /// The bot to run as; the run is pinned to the version in force now.
    pub bot: Option<String>,
    /// Where the task works.
    pub cwd: PathBuf,
    /// The model to ask for.
    pub model: Option<String>,
    /// The reasoning effort to ask for: one the run's model lists, else the task is refused.
    /// Absent: the bot's, else the model's own default.
    pub effort: Option<Effort>,
    /// Allow a write run in `cwd` when it is not a git repository, as a bot's `in_place_writes`
    /// does. A git repository is never written in place.
    pub in_place: bool,
}

impl TaskRequest {
    /// A task for `prompt` in `cwd`: no executor named, no bot, no model.
    pub fn new(prompt: impl Into<String>, cwd: PathBuf) -> Self {
        Self {
            prompt: prompt.into(),
            executor: None,
            bot: None,
            cwd,
            model: None,
            effort: None,
            in_place: false,
        }
    }

    /// Asks for `effort`.
    #[must_use]
    pub fn effort(mut self, effort: Option<Effort>) -> Self {
        self.effort = effort;
        self
    }

    /// Names the executor.
    #[must_use]
    pub fn executor(mut self, executor: ExecutorId) -> Self {
        self.executor = Some(executor);
        self
    }

    /// Runs as `bot`.
    #[must_use]
    pub fn bot(mut self, bot: impl Into<String>) -> Self {
        self.bot = Some(bot.into());
        self
    }

    /// Asks for `model`.
    #[must_use]
    pub fn model(mut self, model: Option<String>) -> Self {
        self.model = model;
        self
    }

    /// Opts in to writing in place in a directory that is not a git repository.
    #[must_use]
    pub fn in_place(mut self, in_place: bool) -> Self {
        self.in_place = in_place;
        self
    }
}

/// What routing a task came to: the decision, the executor the run is recorded against, and the
/// bot pin.
pub(crate) struct Planned {
    pub(crate) decision: RoutingDecision,
    /// The decision's `chosen`; when none is eligible, the executor the run is recorded against
    /// anyway so that it can fail at launch with the reason.
    pub(crate) executor: ExecutorId,
    pub(crate) pin: Option<BotPin>,
    /// The model the run asks for: the task's, else its bot's; `None` leaves the executor's
    /// default.
    pub(crate) model: Option<String>,
    /// The effort the run asks for: the task's, else its bot's; `None` leaves the model's own.
    pub(crate) effort: Option<Effort>,
}

/// Routes a task: the access, sandbox and preference of its bot (else the kernel's defaults),
/// every registered executor with the health this process found, and the router's rules.
pub(crate) fn plan_route(
    shared: &Shared,
    bot: Option<&str>,
    explicit: Option<ExecutorId>,
    task_model: Option<&str>,
    task_effort: Option<Effort>,
) -> Result<Planned> {
    let (access, sandbox, preference, pin, bot_model, bot_effort) = match bot {
        Some(name) => {
            let current = shared.current_defs();
            let def = current
                .set()
                .bot(name)
                .ok_or_else(|| defs::not_found(DefKind::Bot, name, None))?;
            let stored = current
                .version_of(DefKind::Bot, name)
                .ok_or_else(|| defs::not_found(DefKind::Bot, name, None))?;
            let preference = BotPreference {
                name: name.to_owned(),
                executors: def.executors.clone(),
            };
            let pin = BotPin {
                name: name.to_owned(),
                version: stored.version,
            };
            (
                def.access,
                def.sandbox,
                Some(preference),
                Some(pin),
                def.model.clone(),
                def.effort,
            )
        }
        None => (
            shared.config.access,
            shared.config.sandbox,
            None,
            None,
            None,
            None,
        ),
    };
    // Precedence: the task's own model, then the bot's; the executor's default is "none".
    let model = task_model.map(str::to_owned).or(bot_model);
    // Likewise the effort: the task's, then the bot's, else the model's own.
    let effort = task_effort.or(bot_effort);
    let candidates: Vec<Candidate> = shared
        .executors
        .ids()
        .filter_map(|id| {
            Some(Candidate {
                descriptor: shared.executors.get(id)?.descriptor(),
                health: shared.live_health(id),
            })
        })
        .collect();
    let at_ms = u64::try_from(shared.db.now_ms()).unwrap_or(0);
    // With no executor named, one whose models never take the effort is not a candidate; a named
    // one is refused below instead.
    let unfit_effort: Vec<(ExecutorId, String)> = match (effort, &explicit) {
        (Some(effort), None) => candidates
            .iter()
            .filter_map(|candidate| {
                effort::check(candidate, model.as_deref(), effort)
                    .err()
                    .map(|why| (candidate.descriptor.id.clone(), why))
            })
            .collect(),
        _ => Vec::new(),
    };
    let mut decision = router::route(
        &RouteRequest {
            access,
            sandbox,
            explicit: explicit.clone(),
            bot: preference.clone(),
            exclude: Vec::new(),
            unfit_effort,
            defaults: shared.config.default_executors.clone(),
        },
        &candidates,
        at_ms,
    );
    let registered = |id: &&ExecutorId| shared.executors.get(id).is_some();
    let executor = decision
        .chosen
        .clone()
        .or(explicit)
        .or_else(|| match &preference {
            // A bot's run is recorded against one of its own executors.
            Some(bot) => bot.executors.iter().find(registered).cloned(),
            // Recorded against a default executor when one is here, so the run fails with its
            // refusal; never against one the router left out as not a default.
            None => shared
                .config
                .default_executors
                .iter()
                .find(registered)
                .or_else(|| shared.executors.ids().min_by_key(|id| id.as_str()))
                .cloned(),
        })
        .ok_or_else(|| KernelError::NoExecutor(bot.unwrap_or("(any)").to_owned()))?;
    // Refused before anything is queued: the run would fail at start, or silently run at another
    // level, with the executor the run is recorded against.
    if let Some(effort) = effort
        && let Some(candidate) = candidates
            .iter()
            .find(|candidate| candidate.descriptor.id == executor)
    {
        effort::check(candidate, model.as_deref(), effort).map_err(KernelError::Invalid)?;
    }
    // A model the executor's list lacks is a warning, never a refusal: the list may be curated
    // and incomplete, and an executor that really does not know the model fails the run itself.
    if let Some(model) = &model
        && let Some(warning) = unlisted_model_warning(&candidates, &executor, model)
    {
        tracing::warn!(%executor, %model, "{warning}");
        decision.warnings.push(warning);
    }
    Ok(Planned {
        decision,
        executor,
        pin,
        model,
        effort,
    })
}

/// Whether the executor accepts `model`: one of the models it lists (the live list its last
/// probe found, else the adapter's curated one), one of its aliases, or anything at all when it
/// lists nothing to check against.
fn knows_model(candidate: &Candidate, model: &str) -> bool {
    let caps = &candidate.descriptor.caps;
    let live = candidate
        .health
        .as_ref()
        .map(|health| health.models.models.as_slice())
        .filter(|models| !models.is_empty());
    let listed = live.unwrap_or(caps.models.as_slice());
    let base = caps.base_model(model);
    caps.is_model_alias(model)
        || listed.is_empty()
        || listed
            .iter()
            .any(|known| known.id == model || known.id == base)
}

/// The warning for `model` when the models `executor` lists (the live list its last probe
/// found, else the adapter's curated one) do not include it. `None` when it is listed, is one
/// of the executor's aliases, or when nothing is listed to check against.
fn unlisted_model_warning(
    candidates: &[Candidate],
    executor: &ExecutorId,
    model: &str,
) -> Option<String> {
    let candidate = candidates
        .iter()
        .find(|candidate| &candidate.descriptor.id == executor)?;
    let live = candidate
        .health
        .as_ref()
        .map(|health| health.models.models.as_slice())
        .filter(|models| !models.is_empty());
    let caps = &candidate.descriptor.caps;
    let listed = live.unwrap_or(caps.models.as_slice());
    // An alias resolves on the CLI's side; so does a listed id with the `[1m]` suffix.
    let base = caps.base_model(model);
    if caps.is_model_alias(model)
        || listed.is_empty()
        || listed
            .iter()
            .any(|known| known.id == model || known.id == base)
    {
        return None;
    }
    let names: Vec<&str> = listed.iter().map(|known| known.id.as_str()).collect();
    Some(format!(
        "model {model:?} is not in {executor}'s model list ({}); the run asks for it anyway",
        names.join(", ")
    ))
}

/// What a trigger's run starts with: the executor, the bot pin, the routing decision and the
/// model and effort the bot asks for.
pub(crate) fn bot_launch(shared: &Shared, bot: &str) -> Result<(Planned, BotPin)> {
    let planned = plan_route(shared, Some(bot), None, None, None)?;
    let pin = planned
        .pin
        .clone()
        .ok_or_else(|| defs::not_found(DefKind::Bot, bot, None))?;
    Ok((planned, pin))
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
