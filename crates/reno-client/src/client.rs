//! [`Client`]: a typed facade over [`reno_rpc::Client`].
//!
//! It adds three things to the raw RPC client: a checked `daemon.hello` handshake, one error
//! type, and subscriptions that hide `sub.gap` (ARCH-12): [`Client::run_events`] resumes by
//! sequence number until the run is done, so callers see an ordered, gap-free stream or an error.
//!
//! The client spawns no tasks of its own; the connection's tasks belong to the underlying
//! [`reno_rpc::Client`], which has a tracker and a cancellation token (ARCH-11). Dropping the last
//! clone of a [`Client`], and of every stream taken from it, closes the connection.

use std::any::Any;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;

use futures::stream::{self, BoxStream};
use futures::{Stream, StreamExt};
use reno_proto::defs::{
    DefGetParams, DefKind, DefListParams, DefRecord, DefValidateParams, DefValidateResult,
    DefsSnapshot, DefsWatchParams,
};
use reno_proto::{
    Approval, ApprovalDecide, ApprovalDecideParams, ApprovalDecision, ApprovalList,
    ApprovalsSnapshot, ApprovalsWatch, ApprovalsWatchParams, DaemonHello, DaemonShutdown,
    DaemonStatus, DaemonStatusInfo, DefGet, DefList, DefValidate, DefsWatch, FEATURES, HelloParams,
    HelloResult, NoParams, PROTOCOL_VERSION, RoutePreview, RoutePreviewParams, RoutingDecision,
    RunCancel, RunCancelParams, RunEventItem, RunEvents, RunEventsParams, RunGet, RunGetParams,
    RunId, RunList, RunListParams, RunListResult, RunRouting, RunRoutingParams, RunStatus,
    RunSteer, RunSteerParams, RunSummary, RunsSnapshot, RunsWatch, RunsWatchParams, Seq, TaskRun,
    TaskRunParams, TaskRunResult,
};
use reno_rpc::{RpcError, SubEvent, SubStream};
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use crate::error::ClientError;

/// How many times in a row a `run.events` subscription may end in a gap without delivering a
/// single new event before the stream gives up with [`ClientError::Protocol`].
const MAX_STALLED_GAPS: u32 = 8;

/// Client name sent in `daemon.hello`.
const CLIENT_NAME: &str = "reno-client";

/// A connection to the daemon. Cheap to clone; clones share one connection.
#[derive(Clone)]
pub struct Client {
    rpc: reno_rpc::Client,
    hello: Arc<HelloResult>,
    /// Keeps whatever serves this connection alive (the fake daemon), until the last clone drops.
    _keepalive: Option<Arc<dyn Any + Send + Sync>>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("daemon", &self.hello)
            .finish_non_exhaustive()
    }
}

/// Sends `daemon.hello` and checks the answer.
async fn handshake(rpc: &reno_rpc::Client) -> Result<HelloResult, ClientError> {
    let hello = rpc
        .call::<DaemonHello>(HelloParams {
            client_name: CLIENT_NAME.to_owned(),
            protocol_version: PROTOCOL_VERSION,
            features: FEATURES.iter().map(|f| (*f).to_owned()).collect(),
        })
        .await?;
    if hello.protocol_version != PROTOCOL_VERSION {
        return Err(ClientError::Incompatible {
            client: PROTOCOL_VERSION,
            server: Some(hello.protocol_version),
        });
    }
    Ok(hello)
}

impl Client {
    /// Wraps a connected RPC client and performs the `daemon.hello` handshake.
    ///
    /// Fails with [`ClientError::Incompatible`] when the daemon does not speak this client's
    /// protocol version.
    pub async fn connect(rpc: reno_rpc::Client) -> Result<Self, ClientError> {
        let hello = handshake(&rpc).await?;
        Ok(Self {
            rpc,
            hello: Arc::new(hello),
            _keepalive: None,
        })
    }

    /// Connects to the daemon's Unix socket at `path` and performs the `daemon.hello` handshake.
    ///
    /// Fails with [`ClientError::NotRunning`] when the socket file is missing or refuses the
    /// connection. The connection closes when the last clone of the client is dropped.
    pub async fn connect_unix(path: impl AsRef<Path>) -> Result<Self, ClientError> {
        let path = path.as_ref();
        let rpc = reno_rpc::connect_unix(path, CancellationToken::new())
            .await
            .map_err(|e| match e {
                RpcError::Io(io)
                    if matches!(
                        io.kind(),
                        ErrorKind::NotFound | ErrorKind::ConnectionRefused
                    ) =>
                {
                    ClientError::NotRunning(path.to_path_buf())
                }
                other => ClientError::from(other),
            })?;
        Self::connect(rpc).await
    }

    /// Attaches a value that is dropped together with the last clone of this client.
    #[must_use]
    pub fn with_keepalive(mut self, keepalive: Arc<dyn Any + Send + Sync>) -> Self {
        self._keepalive = Some(keepalive);
        self
    }

    /// What the daemon said in the handshake done by [`Client::connect`].
    pub fn daemon(&self) -> &HelloResult {
        &self.hello
    }

    /// Whether the daemon negotiated `feature` in the handshake.
    pub fn supports(&self, feature: &str) -> bool {
        self.hello.features.iter().any(|f| f == feature)
    }

    /// Repeats `daemon.hello` and checks the answer again.
    pub async fn hello(&self) -> Result<HelloResult, ClientError> {
        handshake(&self.rpc).await
    }

    /// Closes the connection and waits for its tasks to stop.
    pub async fn close(&self) {
        self.rpc.close().await;
    }

    /// `daemon.status`: daemon health and load.
    pub async fn status(&self) -> Result<DaemonStatusInfo, ClientError> {
        Ok(self.rpc.call::<DaemonStatus>(NoParams {}).await?)
    }

    /// `daemon.shutdown`: asks the daemon to stop. Needs feature `daemon.shutdown`.
    ///
    /// The daemon answers once shutdown has begun, then closes its connections. To know it has
    /// stopped, wait for its socket to disappear; do not wait for this connection to stay open.
    pub async fn shutdown_daemon(&self) -> Result<(), ClientError> {
        self.require(reno_proto::FEATURE_DAEMON_SHUTDOWN)?;
        self.rpc.call::<DaemonShutdown>(NoParams {}).await?;
        Ok(())
    }

    /// `task.run`: creates a task and its first run. Running as a `bot` needs feature `defs`;
    /// leaving the executor to the router, or `in_place`, needs feature `routing`.
    pub async fn run_task(&self, params: TaskRunParams) -> Result<RunId, ClientError> {
        Ok(self.submit_task(params).await?.run_id)
    }

    /// Like [`run_task`](Self::run_task), and keeps the daemon's answer: the run and the routing
    /// decision that chose its executor (absent from a daemon that predates routing).
    pub async fn submit_task(&self, params: TaskRunParams) -> Result<TaskRunResult, ClientError> {
        if params.bot.is_some() {
            self.require(reno_proto::FEATURE_DEFS)?;
        }
        if !params.attachments.is_empty() {
            self.require(reno_proto::FEATURE_ATTACHMENTS)?;
        }
        if params.in_place || (params.executor.is_none() && params.bot.is_none()) {
            self.require(reno_proto::FEATURE_ROUTING)?;
        }
        Ok(self.rpc.call::<TaskRun>(params).await?)
    }

    /// `route.preview`: the decision `task` would get now. Nothing is created. Needs feature
    /// `routing`.
    pub async fn route_preview(&self, task: TaskRunParams) -> Result<RoutingDecision, ClientError> {
        self.require(reno_proto::FEATURE_ROUTING)?;
        let params = RoutePreviewParams { task };
        Ok(self.rpc.call::<RoutePreview>(params).await?.decision)
    }

    /// `run.routing`: the routing decision stored for a run, with its fallback steps. `None` for
    /// a run made before routing existed. Needs feature `routing`.
    pub async fn run_routing(
        &self,
        run_id: &RunId,
    ) -> Result<Option<RoutingDecision>, ClientError> {
        self.require(reno_proto::FEATURE_ROUTING)?;
        let params = RunRoutingParams {
            run_id: run_id.clone(),
        };
        Ok(self.rpc.call::<RunRouting>(params).await?.decision)
    }

    /// `run.get`: one run's summary.
    pub async fn get_run(&self, run_id: &RunId) -> Result<RunSummary, ClientError> {
        let params = RunGetParams {
            run_id: run_id.clone(),
        };
        Ok(self.rpc.call::<RunGet>(params).await?)
    }

    /// `run.list`: one page of runs, newest first (the daemon's default page size when `limit`
    /// is absent). Use [`Client::list_runs_page`] to page on.
    pub async fn list_runs(&self, params: RunListParams) -> Result<Vec<RunSummary>, ClientError> {
        Ok(self.list_runs_page(params).await?.runs)
    }

    /// `run.list` with its cursor: pass `next` as the following call's `before` until it is
    /// `None`.
    pub async fn list_runs_page(
        &self,
        params: RunListParams,
    ) -> Result<RunListResult, ClientError> {
        Ok(self.rpc.call::<RunList>(params).await?)
    }

    /// `run.cancel`: asks a run to stop. Returns the status once the cancel was accepted.
    pub async fn cancel_run(
        &self,
        run_id: &RunId,
        reason: Option<String>,
    ) -> Result<RunStatus, ClientError> {
        let params = RunCancelParams {
            run_id: run_id.clone(),
            reason,
        };
        Ok(self.rpc.call::<RunCancel>(params).await?.status)
    }

    /// `run.resume`: queues an `interrupted` run (or a `failed` one that has a session) to
    /// continue its harness session, with `prompt` when given. Returns the attempt it will be.
    /// A run that cannot be resumed is a `Conflict` error whose `data` is a
    /// [`reno_proto::NotResumable`]. Needs feature `run.resume`.
    pub async fn resume_run(
        &self,
        run_id: &RunId,
        prompt: Option<String>,
    ) -> Result<u32, ClientError> {
        self.require(reno_proto::FEATURE_RUN_RESUME)?;
        let params = reno_proto::RunResumeParams {
            run_id: run_id.clone(),
            prompt,
        };
        Ok(self
            .rpc
            .call::<reno_proto::RunResume>(params)
            .await?
            .attempt)
    }

    /// `run.steer`: sends a steering message to a live run. Needs feature `run.steer`.
    pub async fn steer(&self, run_id: &RunId, prompt: String) -> Result<(), ClientError> {
        self.require(reno_proto::FEATURE_RUN_STEER)?;
        let params = RunSteerParams {
            run_id: run_id.clone(),
            prompt,
        };
        self.rpc.call::<RunSteer>(params).await?;
        Ok(())
    }

    /// `run.set_model`: changes a live run's model from the harness's next model call; the
    /// answer is the model now in use. Needs feature `run.model`.
    pub async fn set_run_model(
        &self,
        run_id: &RunId,
        model: String,
    ) -> Result<reno_proto::RunSetModelResult, ClientError> {
        self.require(reno_proto::FEATURE_RUN_MODEL)?;
        let params = reno_proto::RunSetModelParams {
            run_id: run_id.clone(),
            model,
        };
        Ok(self.rpc.call::<reno_proto::RunSetModel>(params).await?)
    }

    /// `trigger.list`: every trigger and its state. Needs feature `triggers`.
    pub async fn list_triggers(&self) -> Result<Vec<reno_proto::TriggerInfo>, ClientError> {
        self.require(reno_proto::FEATURE_TRIGGERS)?;
        Ok(self
            .rpc
            .call::<reno_proto::TriggerList>(NoParams {})
            .await?
            .triggers)
    }

    /// `secret.list`: the names of the secrets in the Keychain and of those bots ask for, and
    /// whether each is set. Never a value. Needs feature `secrets`.
    pub async fn list_secrets(&self) -> Result<reno_proto::SecretListResult, ClientError> {
        self.require(reno_proto::FEATURE_SECRETS)?;
        Ok(self.rpc.call::<reno_proto::SecretList>(NoParams {}).await?)
    }

    /// `secret.set`: stores `value` under `name` in the daemon's Keychain. Whether it replaced
    /// one. Needs feature `secrets`.
    pub async fn set_secret(&self, name: &str, value: &str) -> Result<bool, ClientError> {
        self.require(reno_proto::FEATURE_SECRETS)?;
        let params = reno_proto::SecretSetParams {
            name: name.to_owned(),
            value: value.to_owned(),
        };
        Ok(self
            .rpc
            .call::<reno_proto::SecretSet>(params)
            .await?
            .replaced)
    }

    /// `secret.delete`: removes `name`. Whether it was set. Needs feature `secrets`.
    pub async fn delete_secret(&self, name: &str) -> Result<bool, ClientError> {
        self.require(reno_proto::FEATURE_SECRETS)?;
        let params = reno_proto::SecretDeleteParams {
            name: name.to_owned(),
        };
        Ok(self
            .rpc
            .call::<reno_proto::SecretDelete>(params)
            .await?
            .deleted)
    }

    /// `budget.status`: every bot's limits and use today, and what is paused. Needs feature
    /// `budget`.
    pub async fn budget_status(&self) -> Result<reno_proto::BudgetStatusResult, ClientError> {
        self.require(reno_proto::FEATURE_BUDGET)?;
        Ok(self
            .rpc
            .call::<reno_proto::BudgetStatus>(reno_proto::BudgetStatusParams::default())
            .await?)
    }

    /// `budget.pause`: the kill switch. Nothing new is claimed for `bot` (everything when
    /// `None`), and its triggers are held; with `stop_live` its live runs are stopped too. Needs
    /// feature `budget`.
    pub async fn pause(
        &self,
        bot: Option<&str>,
        stop_live: bool,
    ) -> Result<reno_proto::BudgetPauseResult, ClientError> {
        self.require(reno_proto::FEATURE_BUDGET)?;
        let params = reno_proto::BudgetPauseParams {
            bot: bot.map(str::to_owned),
            stop_live,
        };
        Ok(self.rpc.call::<reno_proto::BudgetPause>(params).await?)
    }

    /// `budget.resume`: lifts the pause of `bot` (the global one when `None`). Needs feature
    /// `budget`.
    pub async fn resume(
        &self,
        bot: Option<&str>,
    ) -> Result<reno_proto::BudgetResumeResult, ClientError> {
        self.require(reno_proto::FEATURE_BUDGET)?;
        let params = reno_proto::BudgetResumeParams {
            bot: bot.map(str::to_owned),
        };
        Ok(self.rpc.call::<reno_proto::BudgetResume>(params).await?)
    }

    /// `trigger.fire`: fires a manual trigger, or any enabled one, now. Needs feature
    /// `triggers`.
    pub async fn fire_trigger(
        &self,
        name: &str,
    ) -> Result<reno_proto::TriggerFireResult, ClientError> {
        self.require(reno_proto::FEATURE_TRIGGERS)?;
        let params = reno_proto::TriggerFireParams {
            name: name.to_owned(),
        };
        Ok(self.rpc.call::<reno_proto::TriggerFire>(params).await?)
    }

    /// `executor.list`: every executor with its descriptor and the health of its last probe.
    /// Needs feature `executors`.
    pub async fn list_executors(&self) -> Result<Vec<reno_proto::ExecutorInfo>, ClientError> {
        self.require(reno_proto::FEATURE_EXECUTORS)?;
        Ok(self
            .rpc
            .call::<reno_proto::ExecutorList>(NoParams {})
            .await?
            .executors)
    }

    /// `executor.refresh`: probes `id` again (every executor when `None`), waits for the answer
    /// and returns the whole list. Needs feature `executors`.
    pub async fn refresh_executors(
        &self,
        id: Option<reno_proto::ExecutorId>,
    ) -> Result<Vec<reno_proto::ExecutorInfo>, ClientError> {
        self.require(reno_proto::FEATURE_EXECUTORS)?;
        Ok(self
            .rpc
            .call::<reno_proto::ExecutorRefresh>(reno_proto::ExecutorRefreshParams {
                id,
                usage_only: false,
            })
            .await?
            .executors)
    }

    /// `executor.refresh` with `usageOnly`: reads the plan usage of `id` (every executor when
    /// `None`) again without probing, and returns the whole list. The daemon reads each
    /// executor's usage at most once per 30 s, so calling this often is cheap. Needs feature
    /// `executor.usage`.
    pub async fn refresh_executor_usage(
        &self,
        id: Option<reno_proto::ExecutorId>,
    ) -> Result<Vec<reno_proto::ExecutorInfo>, ClientError> {
        self.require(reno_proto::FEATURE_EXECUTOR_USAGE)?;
        Ok(self
            .rpc
            .call::<reno_proto::ExecutorRefresh>(reno_proto::ExecutorRefreshParams {
                id,
                usage_only: true,
            })
            .await?
            .executors)
    }

    /// `executor.frames`: the frames each executor's wire layer did not recognise (every executor
    /// when `id` is `None`), by kind. Needs feature `executor.frames`.
    pub async fn executor_frames(
        &self,
        id: Option<reno_proto::ExecutorId>,
    ) -> Result<Vec<reno_proto::ExecutorFrameCounts>, ClientError> {
        self.require(reno_proto::FEATURE_EXECUTOR_FRAMES)?;
        Ok(self
            .rpc
            .call::<reno_proto::ExecutorFrames>(reno_proto::ExecutorFramesParams { id })
            .await?
            .executors)
    }

    /// `executor.install`: fetches and verifies the managed adapter of an executor whose last
    /// probe said it needs one, waits for the probe that follows and returns the executor. Can
    /// take minutes. Needs feature `executor.install`.
    pub async fn install_executor(
        &self,
        id: &reno_proto::ExecutorId,
    ) -> Result<reno_proto::ExecutorInfo, ClientError> {
        self.require(reno_proto::FEATURE_EXECUTOR_INSTALL)?;
        Ok(self
            .rpc
            .call::<reno_proto::ExecutorInstall>(reno_proto::ExecutorInstallParams {
                id: id.clone(),
            })
            .await?
            .executor)
    }

    /// `executor.job.start`: installs (when `id` is not found) or updates it as a job and
    /// returns the executor once the job is queued or started, not when it ends. An update of an
    /// executor with runs in flight is queued unless `force`. Progress arrives through
    /// [`Client::watch_executors`]. Needs feature `executor.maintain`.
    pub async fn start_executor_job(
        &self,
        id: &reno_proto::ExecutorId,
        kind: reno_proto::JobKind,
        force: bool,
    ) -> Result<reno_proto::ExecutorInfo, ClientError> {
        self.require(reno_proto::FEATURE_EXECUTOR_MAINTAIN)?;
        Ok(self
            .rpc
            .call::<reno_proto::ExecutorJobStart>(reno_proto::ExecutorJobStartParams {
                id: id.clone(),
                kind,
                force,
            })
            .await?
            .executor)
    }

    /// `executor.job.cancel`: stops the executor's job, which ends `cancelled`. Needs feature
    /// `executor.maintain`.
    pub async fn cancel_executor_job(
        &self,
        id: &reno_proto::ExecutorId,
    ) -> Result<reno_proto::ExecutorInfo, ClientError> {
        self.require(reno_proto::FEATURE_EXECUTOR_MAINTAIN)?;
        Ok(self
            .rpc
            .call::<reno_proto::ExecutorJobCancel>(reno_proto::ExecutorJobParams { id: id.clone() })
            .await?
            .executor)
    }

    /// `executor.job.dismiss`: clears a failed job. Needs feature `executor.maintain`.
    pub async fn dismiss_executor_job(
        &self,
        id: &reno_proto::ExecutorId,
    ) -> Result<reno_proto::ExecutorInfo, ClientError> {
        self.require(reno_proto::FEATURE_EXECUTOR_MAINTAIN)?;
        Ok(self
            .rpc
            .call::<reno_proto::ExecutorJobDismiss>(reno_proto::ExecutorJobParams {
                id: id.clone(),
            })
            .await?
            .executor)
    }

    /// `executor.job.log`: the last lines of output of the executor's current or last job, and
    /// whether older ones were dropped. Needs feature `executor.maintain`.
    pub async fn executor_job_log(
        &self,
        id: &reno_proto::ExecutorId,
    ) -> Result<reno_proto::ExecutorJobLogResult, ClientError> {
        self.require(reno_proto::FEATURE_EXECUTOR_MAINTAIN)?;
        Ok(self
            .rpc
            .call::<reno_proto::ExecutorJobLog>(reno_proto::ExecutorJobParams { id: id.clone() })
            .await?)
    }

    /// `executor.auto_update.set`: stores the auto-update setting of `id`. Needs feature
    /// `executor.maintain`.
    pub async fn set_executor_auto_update(
        &self,
        id: &reno_proto::ExecutorId,
        on: bool,
        verified_only: bool,
    ) -> Result<reno_proto::ExecutorInfo, ClientError> {
        self.require(reno_proto::FEATURE_EXECUTOR_MAINTAIN)?;
        Ok(self
            .rpc
            .call::<reno_proto::ExecutorAutoUpdateSet>(reno_proto::ExecutorAutoUpdateSetParams {
                id: id.clone(),
                on,
                verified_only,
            })
            .await?
            .executor)
    }

    /// `executors.watch`: every executor and its health, then a new full list on every change.
    /// Latest-value, like [`Client::watch_runs`]. Needs feature `executors`.
    pub async fn watch_executors(
        &self,
    ) -> Result<BoxStream<'static, Result<reno_proto::ExecutorsSnapshot, ClientError>>, ClientError>
    {
        self.require(reno_proto::FEATURE_EXECUTORS)?;
        watch_stream::<reno_proto::ExecutorsWatch>(
            &self.rpc,
            reno_proto::ExecutorsWatchParams::default(),
        )
        .await
    }

    /// `workspace.list`: the worktrees of write runs, newest first, optionally only those in
    /// one of `status`. Needs feature `workspaces`.
    pub async fn list_workspaces(
        &self,
        status: Option<Vec<reno_proto::WorkspaceStatus>>,
    ) -> Result<Vec<reno_proto::WorkspaceInfo>, ClientError> {
        self.require(reno_proto::FEATURE_WORKSPACES)?;
        Ok(self
            .rpc
            .call::<reno_proto::WorkspaceList>(reno_proto::WorkspaceListParams { status })
            .await?
            .workspaces)
    }

    /// `workspace.get`: one run's workspace. Needs feature `workspaces`.
    pub async fn get_workspace(
        &self,
        run_id: reno_proto::RunId,
    ) -> Result<reno_proto::WorkspaceInfo, ClientError> {
        self.require(reno_proto::FEATURE_WORKSPACES)?;
        Ok(self
            .rpc
            .call::<reno_proto::WorkspaceGet>(reno_proto::WorkspaceGetParams { run_id })
            .await?)
    }

    /// `workspace.diff`: what a run changed against its base commit, and with `path` the patch
    /// of that one file. Needs feature `run.changes`.
    pub async fn workspace_diff(
        &self,
        run_id: reno_proto::RunId,
        path: Option<String>,
    ) -> Result<reno_proto::WorkspaceDiffResult, ClientError> {
        self.require(reno_proto::FEATURE_RUN_CHANGES)?;
        Ok(self
            .rpc
            .call::<reno_proto::WorkspaceDiff>(reno_proto::WorkspaceDiffParams { run_id, path })
            .await?)
    }

    /// `workspace.clean`: removes workspaces that hold nothing to lose (or merged ones, for
    /// `Merged` and a single run); with `force`, any. Needs feature `workspaces`.
    pub async fn clean_workspaces(
        &self,
        target: reno_proto::CleanTarget,
        force: bool,
    ) -> Result<Vec<reno_proto::CleanItem>, ClientError> {
        self.require(reno_proto::FEATURE_WORKSPACES)?;
        Ok(self
            .rpc
            .call::<reno_proto::WorkspaceClean>(reno_proto::WorkspaceCleanParams { target, force })
            .await?
            .items)
    }

    /// `triggers.watch`: every trigger and its state, then a new full list on every change.
    /// Latest-value, like [`Client::watch_runs`]. Needs feature `triggers`.
    pub async fn watch_triggers(
        &self,
    ) -> Result<BoxStream<'static, Result<reno_proto::TriggersSnapshot, ClientError>>, ClientError>
    {
        self.require(reno_proto::FEATURE_TRIGGERS)?;
        watch_stream::<reno_proto::TriggersWatch>(
            &self.rpc,
            reno_proto::TriggersWatchParams::default(),
        )
        .await
    }

    /// `approval.list`: the pending approvals, oldest first.
    pub async fn list_approvals(&self) -> Result<Vec<Approval>, ClientError> {
        self.require(reno_proto::FEATURE_APPROVALS)?;
        Ok(self.rpc.call::<ApprovalList>(NoParams {}).await?.approvals)
    }

    /// `approval.history`: decided approvals, newest decision first, with who decided each and
    /// by which rule. Only `run`'s when given. Needs feature `approval.history`.
    pub async fn approval_history(
        &self,
        run: Option<RunId>,
        limit: Option<u32>,
    ) -> Result<Vec<reno_proto::ApprovalRecord>, ClientError> {
        self.require(reno_proto::FEATURE_APPROVAL_HISTORY)?;
        let params = reno_proto::ApprovalHistoryParams { run, limit };
        Ok(self
            .rpc
            .call::<reno_proto::ApprovalHistory>(params)
            .await?
            .approvals)
    }

    /// `approval.decide`: answers one pending approval.
    pub async fn decide(
        &self,
        approval_id: &reno_proto::ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<(), ClientError> {
        self.decide_with_message(approval_id, decision, None).await
    }

    /// `approval.decide` with the reason for a denial, which reaches the harness and shows in
    /// the approval history. A `message` needs feature `approval.message`.
    pub async fn decide_with_message(
        &self,
        approval_id: &reno_proto::ApprovalId,
        decision: ApprovalDecision,
        message: Option<String>,
    ) -> Result<(), ClientError> {
        self.decide_remembering(approval_id, decision, message, None)
            .await
    }

    /// `approval.decide` that also keeps the answer as a standing rule (`remember`), whose
    /// matcher the daemon derives from the approval's own subject. A `remember` needs feature
    /// `rules`.
    pub async fn decide_remembering(
        &self,
        approval_id: &reno_proto::ApprovalId,
        decision: ApprovalDecision,
        message: Option<String>,
        remember: Option<reno_proto::Remember>,
    ) -> Result<(), ClientError> {
        self.require(reno_proto::FEATURE_APPROVALS)?;
        if message.is_some() {
            self.require(reno_proto::FEATURE_APPROVAL_MESSAGE)?;
        }
        if remember.is_some() {
            self.require(reno_proto::FEATURE_RULES)?;
        }
        if remember
            .as_ref()
            .is_some_and(|r| r.scope == reno_proto::RememberScope::Project)
        {
            self.require(reno_proto::FEATURE_PROJECT_RULES)?;
        }
        let params = ApprovalDecideParams {
            approval_id: approval_id.clone(),
            decision,
            message,
            remember,
        };
        self.rpc.call::<ApprovalDecide>(params).await?;
        Ok(())
    }

    /// `rule.list`: the standing rules, newest first; with `include_inactive` also the revoked
    /// and expired ones. Needs feature `rules`.
    pub async fn list_rules(
        &self,
        include_inactive: bool,
    ) -> Result<Vec<reno_proto::StandingRule>, ClientError> {
        self.require(reno_proto::FEATURE_RULES)?;
        let params = reno_proto::RuleListParams {
            include_inactive,
            project: None,
        };
        Ok(self.rpc.call::<reno_proto::RuleList>(params).await?.rules)
    }

    /// `rule.list` with the `project` filter: only the rules whose scope is the project of
    /// `project` (any directory of it; the daemon resolves the repository root). Needs features
    /// `rules` and `rules.project`.
    pub async fn list_project_rules(
        &self,
        project: &std::path::Path,
        include_inactive: bool,
    ) -> Result<Vec<reno_proto::StandingRule>, ClientError> {
        self.require(reno_proto::FEATURE_RULES)?;
        self.require(reno_proto::FEATURE_PROJECT_RULES)?;
        let params = reno_proto::RuleListParams {
            include_inactive,
            project: Some(project.to_path_buf()),
        };
        Ok(self.rpc.call::<reno_proto::RuleList>(params).await?.rules)
    }

    /// `rule.add`: a standing rule for one project from an exact command line; returns the rule.
    /// Needs features `rules` and `rules.project`.
    pub async fn add_rule(
        &self,
        params: reno_proto::RuleAddParams,
    ) -> Result<reno_proto::StandingRule, ClientError> {
        self.require(reno_proto::FEATURE_RULES)?;
        self.require(reno_proto::FEATURE_PROJECT_RULES)?;
        Ok(self.rpc.call::<reno_proto::RuleAdd>(params).await?)
    }

    /// `rule.revoke`: stops a standing rule applying, for good. Needs feature `rules`.
    pub async fn revoke_rule(&self, id: &reno_proto::RuleId) -> Result<(), ClientError> {
        self.require(reno_proto::FEATURE_RULES)?;
        let params = reno_proto::RuleRevokeParams { id: id.clone() };
        self.rpc.call::<reno_proto::RuleRevoke>(params).await?;
        Ok(())
    }

    /// `agent.auth`: binds this connection to the run `run` with its token. From then on the
    /// connection may call only the `agent.*` methods (the daemon refuses every other), so use a
    /// connection of its own for it. Needs feature `agent`.
    ///
    /// The token is a secret: it is sent once, here, and never kept.
    pub async fn agent_auth(
        &self,
        run: &RunId,
        token: String,
    ) -> Result<reno_proto::AgentAuthResult, ClientError> {
        self.require(reno_proto::FEATURE_AGENT)?;
        let params = reno_proto::AgentAuthParams {
            run_id: run.clone(),
            token,
        };
        Ok(self.rpc.call::<reno_proto::AgentAuth>(params).await?)
    }

    /// `agent.report`: a structured status of the run, journaled in its transcript. After
    /// [`Client::agent_auth`].
    pub async fn agent_report(
        &self,
        params: reno_proto::AgentReportParams,
    ) -> Result<(), ClientError> {
        self.require(reno_proto::FEATURE_AGENT)?;
        self.rpc.call::<reno_proto::AgentReport>(params).await?;
        Ok(())
    }

    /// `agent.request_approval`: asks for an approval for the run and waits until it is decided,
    /// expires or the run ends. After [`Client::agent_auth`].
    pub async fn agent_request_approval(
        &self,
        params: reno_proto::AgentApprovalParams,
    ) -> Result<reno_proto::AgentApprovalResult, ClientError> {
        self.require(reno_proto::FEATURE_AGENT)?;
        Ok(self
            .rpc
            .call::<reno_proto::AgentRequestApproval>(params)
            .await?)
    }

    /// `agent.board_read`: one key of the run's own board scope, or every key without values.
    /// After [`Client::agent_auth`].
    pub async fn agent_board_read(
        &self,
        key: Option<String>,
    ) -> Result<reno_proto::AgentBoardReadResult, ClientError> {
        self.require(reno_proto::FEATURE_AGENT)?;
        let params = reno_proto::AgentBoardReadParams { key };
        Ok(self.rpc.call::<reno_proto::AgentBoardRead>(params).await?)
    }

    /// `agent.board_write`: writes a key of the run's own board scope. A stale
    /// `expected_version` is a `Conflict` error whose `data` is a [`reno_proto::BoardConflict`].
    /// After [`Client::agent_auth`].
    pub async fn agent_board_write(
        &self,
        params: reno_proto::AgentBoardWriteParams,
    ) -> Result<reno_proto::AgentBoardWriteResult, ClientError> {
        self.require(reno_proto::FEATURE_AGENT)?;
        Ok(self.rpc.call::<reno_proto::AgentBoardWrite>(params).await?)
    }

    /// `def.list`: the definitions in force and the problems with the files on disk, optionally
    /// of one kind. Needs feature `defs`.
    pub async fn list_defs(&self, kind: Option<DefKind>) -> Result<DefsSnapshot, ClientError> {
        self.require(reno_proto::FEATURE_DEFS)?;
        Ok(self.rpc.call::<DefList>(DefListParams { kind }).await?)
    }

    /// `def.get`: one version of a definition with its body; the newest when `version` is
    /// `None`. Needs feature `defs`.
    pub async fn get_def(
        &self,
        kind: DefKind,
        name: &str,
        version: Option<u32>,
    ) -> Result<DefRecord, ClientError> {
        self.require(reno_proto::FEATURE_DEFS)?;
        let params = DefGetParams {
            kind,
            name: name.to_owned(),
            version,
        };
        Ok(self.rpc.call::<DefGet>(params).await?)
    }

    /// `def.validate`: checks a body against every rule without saving it. Needs feature `defs`.
    pub async fn validate_def(
        &self,
        kind: DefKind,
        name: &str,
        body: serde_json::Value,
    ) -> Result<DefValidateResult, ClientError> {
        self.require(reno_proto::FEATURE_DEFS)?;
        let params = DefValidateParams {
            kind,
            name: name.to_owned(),
            body,
        };
        Ok(self.rpc.call::<DefValidate>(params).await?)
    }

    /// `defs.watch`: the definitions in force and the file problems, then a new full snapshot on
    /// every change. Latest-value, like [`Client::watch_runs`]. Needs feature `defs`.
    pub async fn watch_defs(
        &self,
    ) -> Result<BoxStream<'static, Result<DefsSnapshot, ClientError>>, ClientError> {
        self.require(reno_proto::FEATURE_DEFS)?;
        watch_stream::<DefsWatch>(&self.rpc, DefsWatchParams::default()).await
    }

    /// `runs.watch`: the current run list, then a new full list on every change.
    ///
    /// Latest-value: a slow consumer sees fewer lists, never stale ones out of order. The stream
    /// ends when the daemon closes the subscription or the connection drops.
    pub async fn watch_runs(
        &self,
    ) -> Result<BoxStream<'static, Result<RunsSnapshot, ClientError>>, ClientError> {
        watch_stream::<RunsWatch>(&self.rpc, RunsWatchParams::default()).await
    }

    /// `approvals.watch`: the pending approvals, then a new full list on every change.
    ///
    /// Latest-value, like [`Client::watch_runs`].
    pub async fn watch_approvals(
        &self,
    ) -> Result<BoxStream<'static, Result<ApprovalsSnapshot, ClientError>>, ClientError> {
        self.require(reno_proto::FEATURE_APPROVALS)?;
        watch_stream::<ApprovalsWatch>(&self.rpc, ApprovalsWatchParams::default()).await
    }

    /// `run.events`: the run's journal from `from` (inclusive; the start when `None`), then
    /// live, until the daemon reports the run done.
    ///
    /// When the daemon cuts the subscription off with `sub.gap` (ARCH-12), the stream
    /// re-subscribes from `resume_from` and carries on. Items arrive in strictly increasing,
    /// consecutive sequence order; a repeated seq is skipped, and a hole ends the stream with
    /// [`ClientError::Protocol`]. The stream is lazy: nothing is sent until it is first polled.
    /// After an `Err` it ends.
    pub fn run_events(
        &self,
        run_id: RunId,
        from: Option<Seq>,
    ) -> impl Stream<Item = Result<RunEventItem, ClientError>> + Send + use<> {
        let state = EventsState {
            rpc: self.rpc.clone(),
            run_id,
            // A run's journal counts from 1, so an absent `from` and `Seq(0)` both mean the start.
            next: from.unwrap_or(Seq(1)).max(Seq(1)),
            sub: None,
            stalled: 0,
            finished: false,
        };
        stream::unfold(state, EventsState::step)
    }

    fn require(&self, feature: &str) -> Result<(), ClientError> {
        if self.supports(feature) {
            Ok(())
        } else {
            Err(ClientError::Protocol(format!(
                "the daemon did not negotiate feature `{feature}`"
            )))
        }
    }
}

/// Starts a latest-value subscription and maps it to a plain stream of values.
///
/// A latest-value subscription has no sequence numbers, so a daemon should never send `sub.gap`
/// on one. If it does, the subscription is started again: its first item is the whole current
/// value, which is the refetch. Gaps in a row with no item between them end the stream with
/// [`ClientError::Protocol`].
async fn watch_stream<S>(
    rpc: &reno_rpc::Client,
    params: S::Params,
) -> Result<BoxStream<'static, Result<S::Item, ClientError>>, ClientError>
where
    S: reno_proto::Subscription + 'static,
    S::Params: Clone + Send + 'static,
    S::Item: DeserializeOwned + Send + 'static,
{
    let sub = rpc.subscribe::<S>(params.clone()).await?;
    let state = WatchState::<S> {
        rpc: rpc.clone(),
        params,
        sub: Some(sub),
        stalled: 0,
        failed: false,
    };
    Ok(stream::unfold(state, WatchState::step).boxed())
}

struct WatchState<S: reno_proto::Subscription> {
    rpc: reno_rpc::Client,
    params: S::Params,
    sub: Option<SubStream<S::Item>>,
    stalled: u32,
    failed: bool,
}

impl<S> WatchState<S>
where
    S: reno_proto::Subscription,
    S::Params: Clone,
    S::Item: DeserializeOwned + Send + 'static,
{
    async fn step(mut self) -> Option<(Result<S::Item, ClientError>, Self)> {
        if self.failed {
            return None;
        }
        match self.advance().await {
            Ok(Some(item)) => Some((Ok(item), self)),
            Ok(None) => None,
            Err(e) => {
                self.failed = true;
                Some((Err(e), self))
            }
        }
    }

    async fn advance(&mut self) -> Result<Option<S::Item>, ClientError> {
        loop {
            let sub = match self.sub.as_mut() {
                Some(sub) => sub,
                None => self
                    .sub
                    .insert(self.rpc.subscribe::<S>(self.params.clone()).await?),
            };
            match sub.next().await {
                Some(SubEvent::Item(item)) => {
                    self.stalled = 0;
                    return Ok(Some(item));
                }
                Some(SubEvent::Gap { .. }) => {
                    self.sub = None;
                    self.stalled += 1;
                    if self.stalled > MAX_STALLED_GAPS {
                        return Err(ClientError::Protocol(
                            "a watch keeps gapping without delivering a value".to_owned(),
                        ));
                    }
                }
                Some(SubEvent::Done) | None => return Ok(None),
            }
        }
    }
}

struct EventsState {
    rpc: reno_rpc::Client,
    run_id: RunId,
    /// The next seq the caller should see.
    next: Seq,
    sub: Option<SubStream<RunEventItem>>,
    /// Gaps in a row with no new event in between.
    stalled: u32,
    finished: bool,
}

impl EventsState {
    async fn step(mut self) -> Option<(Result<RunEventItem, ClientError>, Self)> {
        match self.advance().await {
            Ok(Some(item)) => Some((Ok(item), self)),
            Ok(None) => None,
            Err(e) => {
                self.finished = true;
                Some((Err(e), self))
            }
        }
    }

    async fn advance(&mut self) -> Result<Option<RunEventItem>, ClientError> {
        loop {
            if self.finished {
                return Ok(None);
            }
            let sub = match self.sub.as_mut() {
                Some(sub) => sub,
                None => {
                    let params = RunEventsParams {
                        run_id: self.run_id.clone(),
                        from_seq: Some(self.next),
                    };
                    self.sub
                        .insert(self.rpc.subscribe::<RunEvents>(params).await?)
                }
            };
            match sub.next().await {
                Some(SubEvent::Item(item)) => {
                    if item.seq < self.next {
                        // A repeat after a resume: already delivered.
                        continue;
                    }
                    if item.seq > self.next {
                        return Err(ClientError::Protocol(format!(
                            "run {}: expected seq {}, daemon sent {}",
                            self.run_id, self.next, item.seq
                        )));
                    }
                    self.next = item.seq.next().ok_or_else(|| {
                        ClientError::Protocol("run sequence numbers overflowed".to_owned())
                    })?;
                    self.stalled = 0;
                    return Ok(Some(item));
                }
                Some(SubEvent::Gap { resume_from }) => {
                    self.sub = None;
                    self.stalled += 1;
                    if self.stalled > MAX_STALLED_GAPS {
                        return Err(ClientError::Protocol(format!(
                            "run {}: subscription keeps gapping without progress at seq {}",
                            self.run_id, self.next
                        )));
                    }
                    if resume_from > self.next {
                        return Err(ClientError::Protocol(format!(
                            "run {}: daemon resumes at {resume_from} but only {} was delivered",
                            self.run_id, self.next
                        )));
                    }
                    self.next = self.next.min(resume_from);
                }
                Some(SubEvent::Done) => {
                    self.finished = true;
                    return Ok(None);
                }
                None => return Err(ClientError::Disconnected),
            }
        }
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
