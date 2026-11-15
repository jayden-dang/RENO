//! The kernel façade: a cheap, clonable handle over the dispatcher, the run supervisors and the
//! runs hub.
//!
//! # Tasks and channels (ARCHITECTURE 17.2)
//!
//! | Task | Count | Owns | Inbox |
//! |---|---|---|---|
//! | dispatcher | 1 | the claim loop | `Notify` (no queue) and one deadline |
//! | runs hub | 1 | the `watch_runs` list | `Notify`, coalescing |
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
use reno_proto::{
    Approval, ApprovalDecision, ApprovalId, ExecutorId, RunEventItem, RunId, RunStatus, RunSummary,
    Seq, TaskId,
};
use reno_store::{NewTask, StoreError};
use tokio::sync::{oneshot, watch};
use tokio::time::timeout;

use crate::clock::Clock;
use crate::config::KernelConfig;
use crate::db::Db;
use crate::dispatcher;
use crate::error::{KernelError, Result};
use crate::events;
use crate::recovery;
use crate::registry::ExecutorRegistry;
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
        let shared = Arc::new(Shared::new(Db::new(store, clock), executors, config));
        recovery::recover(&shared).await?;
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
        if self.shared.root.is_cancelled() {
            return Err(KernelError::ShuttingDown);
        }
        let cwd = cwd
            .to_str()
            .ok_or_else(|| KernelError::Invalid("the working directory is not UTF-8".into()))?
            .to_owned();
        let prompt = prompt.into();
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
            match db.enqueue_run(&task, &run, &executor).await {
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
        Ok(crate::shared::summarize(
            &self.shared.run_record(run).await?,
        ))
    }

    /// Runs, newest first, optionally only those in `status`, at most `limit`.
    pub async fn list_runs(
        &self,
        status: Option<&[RunStatus]>,
        limit: Option<u32>,
    ) -> Result<Vec<RunSummary>> {
        self.shared.summaries(status, limit).await
    }

    /// The newest runs (`KernelConfig::watch_limit`), refreshed after every change. Latest value
    /// wins: a slow reader skips versions, never queues them.
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

    /// Queue a steering prompt for a live run's executor. Errors if the run is not live.
    ///
    /// The prompt is not journaled as an event yet: `ExecEvent` has no variant for a user
    /// message in this tree. The supervisor's `Steer` command is where it goes once it does.
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

    /// Approvals waiting for a human, oldest first. In memory in Phase 0.
    pub fn pending_approvals(&self) -> Vec<Approval> {
        self.shared.approvals.list()
    }

    /// The pending approvals, refreshed on every change. Latest value wins.
    pub fn watch_approvals(&self) -> watch::Receiver<Vec<Approval>> {
        self.shared.approvals.subscribe()
    }

    /// Answer a pending approval. The executor gets the decision, then the run goes back to
    /// `running`.
    pub async fn decide(&self, approval: &ApprovalId, decision: ApprovalDecision) -> Result<()> {
        let run = self
            .shared
            .approvals
            .run_of(approval)
            .ok_or_else(|| KernelError::ApprovalNotFound(approval.clone()))?;
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
            shared.approvals.clear();
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

/// Mint an id until it parses (it always does: the suffix is hex).
fn mint<T>(
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
