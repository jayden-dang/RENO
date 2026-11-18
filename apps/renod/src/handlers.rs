//! The only place protocol methods meet kernel calls (ARCH-4).
//!
//! | Wire | Kernel |
//! |---|---|
//! | `daemon.hello` | none: checks the protocol version, answers version, features, pid |
//! | `daemon.status` | `list_runs` (counts) |
//! | `daemon.shutdown` | none: asks the daemon to begin its shutdown sequence |
//! | `task.run` | `submit_task` (the router picks the executor unless the task names one) |
//! | `run.get`, `run.list` | `get_run`, `list_runs` |
//! | `run.routing` | `run_routing` |
//! | `route.preview` | `preview_route` |
//! | `run.cancel` | `cancel` |
//! | `run.resume` | `resume_run` |
//! | `budget.status` | `budget_status` |
//! | `budget.pause`, `budget.resume` | `pause`, `resume` |
//! | `run.steer` | `get_run`, `steer` |
//! | `run.set_model` | `get_run`, `set_model` |
//! | `run.configure` | `configure_run` |
//! | `run.events` | `run_events` |
//! | `runs.watch` | `watch_runs` |
//! | `approval.list`, `approval.decide` | `pending_approvals`, `decide` |
//! | `approval.history` | `approval_history` |
//! | `rule.list`, `rule.add`, `rule.revoke` | `list_rules` / `list_project_rules`, `add_project_rule`, `revoke_rule` |
//! | `approvals.watch` | `watch_approvals` |
//! | `def.list` | `current_defs` |
//! | `def.get` | `get_definition` |
//! | `def.validate` | `validate_definition` |
//! | `defs.watch` | `watch_defs` |
//! | `trigger.list` | `list_triggers` |
//! | `trigger.fire` | `fire_trigger` |
//! | `triggers.watch` | `watch_triggers` |
//! | `workspace.list`, `workspace.get` | `workspaces`, `workspace` |
//! | `workspace.diff` | `workspace_diff` |
//! | `workspace.clean` | `clean_workspaces` |
//! | `secret.list`, `secret.set`, `secret.delete` | none: the `Secrets` port directly (`current_defs` for who uses a name) |
//! | `executor.list`, `executor.refresh` | `executors`, `refresh_executors` |
//! | `executor.frames` | `executor_frames` |
//! | `executor.install` | `install_executor` |
//! | `executor.job.start`, `.cancel`, `.dismiss`, `.log` | `start_job`, `cancel_job`, `dismiss_job`, `job_log` |
//! | `executor.auto_update.set` | `set_auto_update` |
//! | `executors.watch` | `watch_executors` |
//! | `agent.auth` | `agent_authenticate` |
//! | `agent.report`, `agent.request_approval` | `agent_report`, `agent_request_approval` |
//! | `agent.board_read`, `agent.board_write` | `agent_board_read`, `agent_board_write` |
//!
//! `sub.gap` is sent by `reno-rpc` when a socket reader falls behind; the daemon registers no
//! handler for it. Every kernel failure goes through [`crate::errors`].
//!
//! A connection is the owner's, whose peer uid was checked when it connected, until `agent.auth`
//! binds it to one run; then the router's guard lets it call `agent.*` and nothing else (T1.9).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use futures::stream::{self, BoxStream};
use futures::{Stream, StreamExt};
use reno_kernel::{
    AgentSession, CurrentDefs, Kernel, KernelError, SecretValue, Secrets, TaskRequest,
};
use reno_proto::defs::{
    DefGetParams, DefListParams, DefRecord, DefValidateParams, DefValidateResult, DefsSnapshot,
    DefsWatchParams,
};
use reno_proto::{
    ATTACHMENT_MAX_BYTES, ATTACHMENTS_MAX_COUNT, Attachment, RoutePreview, RoutePreviewParams,
    RoutePreviewResult, RunRouting, RunRoutingParams, RunRoutingResult,
};
use reno_proto::{
    AgentApprovalParams, AgentApprovalResult, AgentAuth, AgentAuthParams, AgentAuthResult,
    AgentBoardRead, AgentBoardReadParams, AgentBoardReadResult, AgentBoardWrite,
    AgentBoardWriteParams, AgentBoardWriteResult, AgentReport, AgentReportParams,
    AgentReportResult, AgentRequestApproval, token_digest,
};
use reno_proto::{
    ApprovalDecide, ApprovalDecideParams, ApprovalDecideResult, ApprovalHistory,
    ApprovalHistoryParams, ApprovalHistoryResult, ApprovalList, ApprovalListResult,
    ApprovalsSnapshot, ApprovalsWatch, ApprovalsWatchParams, DaemonHello, DaemonShutdown,
    DaemonShutdownResult, DaemonStatus, DaemonStatusInfo, ErrorCode, ErrorObject, ExecutorId,
    FEATURES, HelloParams, HelloResult, NoParams, PROTOCOL_VERSION, RunCancel, RunCancelParams,
    RunCancelResult, RunConfigure, RunConfigureParams, RunConfigureResult, RunEventItem, RunEvents,
    RunEventsParams, RunGet, RunGetParams, RunList, RunListParams, RunListResult, RunResume,
    RunResumeParams, RunResumeResult, RunSetModel, RunSetModelParams, RunSetModelResult, RunStatus,
    RunSteer, RunSteerParams, RunSteerResult, RunSummary, RunsSnapshot, RunsWatch, RunsWatchParams,
    TaskRun, TaskRunParams, TaskRunResult,
};
use reno_proto::{
    BudgetPause, BudgetPauseParams, BudgetPauseResult, BudgetResume, BudgetResumeParams,
    BudgetResumeResult, BudgetStatus, BudgetStatusParams, BudgetStatusResult,
};
use reno_proto::{
    DefGet, DefList, DefValidate, DefsWatch, TriggerFire, TriggerFireParams, TriggerFireResult,
    TriggerInfo, TriggerList, TriggerListResult, TriggersSnapshot, TriggersWatch,
    TriggersWatchParams,
};
use reno_proto::{
    ExecutorAutoUpdateSet, ExecutorAutoUpdateSetParams, ExecutorJobCancel, ExecutorJobDismiss,
    ExecutorJobLog, ExecutorJobLogResult, ExecutorJobParams, ExecutorJobResult, ExecutorJobStart,
    ExecutorJobStartParams, FEATURE_EXECUTOR_MAINTAIN,
};
use reno_proto::{
    ExecutorFrames, ExecutorFramesParams, ExecutorFramesResult, ExecutorInfo, ExecutorInstall,
    ExecutorInstallParams, ExecutorInstallResult, ExecutorList, ExecutorListResult,
    ExecutorRefresh, ExecutorRefreshParams, ExecutorsSnapshot, ExecutorsWatch,
    ExecutorsWatchParams,
};
use reno_proto::{
    RuleAdd, RuleAddParams, RuleList, RuleListParams, RuleListResult, RuleRevoke, RuleRevokeParams,
    RuleRevokeResult, SECRET_VALUE_MAX, SecretDelete, SecretDeleteParams, SecretDeleteResult,
    SecretInfo, SecretList, SecretListResult, SecretSet, SecretSetParams, SecretSetResult,
};
use reno_proto::{
    WorkspaceClean, WorkspaceCleanParams, WorkspaceCleanResult, WorkspaceDiff, WorkspaceDiffParams,
    WorkspaceDiffResult, WorkspaceGet, WorkspaceGetParams, WorkspaceInfo, WorkspaceList,
    WorkspaceListParams, WorkspaceListResult,
};
use reno_rpc::{Context, Router, SubItem};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::errors::{install_error, invalid, kernel_error, secrets_error};
use crate::install::Installer;

/// How many times in a row a `run.events` stream may be cut off by the kernel without delivering
/// an event before the daemon gives up on it.
const MAX_STALLED_GAPS: u32 = 8;

/// Statuses that count as "active" in `daemon.status`: claimed and not yet finished.
const ACTIVE: [RunStatus; 5] = [
    RunStatus::Leased,
    RunStatus::Running,
    RunStatus::WaitingInput,
    RunStatus::WaitingApproval,
    RunStatus::Evaluating,
];

/// What every handler shares.
#[derive(Debug)]
pub(crate) struct Ctx {
    pub(crate) kernel: Kernel,
    /// Installs the executors that need one.
    pub(crate) installer: Installer,
    pub(crate) started: Instant,
    /// The executors a `task.run` may name, sorted.
    pub(crate) executors: Vec<ExecutorId>,
    /// Where a task with no `cwd` runs.
    pub(crate) default_cwd: PathBuf,
    /// Cancelled to ask the daemon to shut down.
    pub(crate) stop: CancellationToken,
    /// The secret store `secret.*` reads and writes; `None` when the daemon was started without
    /// one.
    pub(crate) secrets: Option<Arc<dyn Secrets>>,
    /// The definitions port, for whether its file watch is running.
    pub(crate) fs_defs: Arc<crate::defs::FsDefinitions>,
    /// The kernel was given the means to install and update executors, so the daemon offers
    /// feature `executor.maintain`.
    pub(crate) maintenance: bool,
}

/// Adapts `handler(ctx, params)` to the closure shape `Router` takes.
fn bind<P, Fut, F>(ctx: &Arc<Ctx>, handler: F) -> impl Fn(P, Context) -> Fut + Send + Sync + 'static
where
    F: Fn(Arc<Ctx>, P) -> Fut + Send + Sync + 'static,
{
    let ctx = Arc::clone(ctx);
    move |params, _conn| handler(Arc::clone(&ctx), params)
}

/// Adapts `handler(ctx, connection, params)`, for the handlers that act for the connection's own
/// run.
fn bind_conn<P, Fut, F>(
    ctx: &Arc<Ctx>,
    handler: F,
) -> impl Fn(P, Context) -> Fut + Send + Sync + 'static
where
    F: Fn(Arc<Ctx>, Context, P) -> Fut + Send + Sync + 'static,
{
    let ctx = Arc::clone(ctx);
    move |params, conn| handler(Arc::clone(&ctx), conn, params)
}

/// The router for every method and subscription in `reno-proto`.
pub(crate) fn router(ctx: &Arc<Ctx>) -> Router {
    Router::new()
        .method::<DaemonHello, _, _>(bind(ctx, hello))
        .method::<DaemonStatus, _, _>(bind(ctx, status))
        .method::<DaemonShutdown, _, _>(bind(ctx, shutdown))
        .method::<TaskRun, _, _>(bind(ctx, task_run))
        .method::<RunGet, _, _>(bind(ctx, run_get))
        .method::<RunRouting, _, _>(bind(ctx, run_routing))
        .method::<RoutePreview, _, _>(bind(ctx, route_preview))
        .method::<BudgetStatus, _, _>(bind(ctx, budget_status))
        .method::<BudgetPause, _, _>(bind(ctx, budget_pause))
        .method::<BudgetResume, _, _>(bind(ctx, budget_resume))
        .method::<RunList, _, _>(bind(ctx, run_list))
        .method::<RunCancel, _, _>(bind(ctx, run_cancel))
        .method::<RunResume, _, _>(bind(ctx, run_resume))
        .method::<RunSteer, _, _>(bind(ctx, run_steer))
        .method::<RunSetModel, _, _>(bind(ctx, run_set_model))
        .method::<RunConfigure, _, _>(bind(ctx, run_configure))
        .method::<ApprovalList, _, _>(bind(ctx, approval_list))
        .method::<ApprovalDecide, _, _>(bind(ctx, approval_decide))
        .method::<ApprovalHistory, _, _>(bind(ctx, approval_history))
        .method::<RuleList, _, _>(bind(ctx, rule_list))
        .method::<RuleAdd, _, _>(bind(ctx, rule_add))
        .method::<RuleRevoke, _, _>(bind(ctx, rule_revoke))
        .method::<DefList, _, _>(bind(ctx, def_list))
        .method::<DefGet, _, _>(bind(ctx, def_get))
        .method::<DefValidate, _, _>(bind(ctx, def_validate))
        .method::<TriggerList, _, _>(bind(ctx, trigger_list))
        .method::<TriggerFire, _, _>(bind(ctx, trigger_fire))
        .method::<AgentAuth, _, _>(bind_conn(ctx, agent_auth))
        .method::<AgentReport, _, _>(bind_conn(ctx, agent_report))
        .method::<AgentRequestApproval, _, _>(bind_conn(ctx, agent_request_approval))
        .method::<AgentBoardRead, _, _>(bind_conn(ctx, agent_board_read))
        .method::<AgentBoardWrite, _, _>(bind_conn(ctx, agent_board_write))
        .method::<ExecutorList, _, _>(bind(ctx, executor_list))
        .method::<ExecutorRefresh, _, _>(bind(ctx, executor_refresh))
        .method::<ExecutorInstall, _, _>(bind(ctx, executor_install))
        .method::<ExecutorFrames, _, _>(bind(ctx, executor_frames))
        .method::<ExecutorJobStart, _, _>(bind(ctx, executor_job_start))
        .method::<ExecutorJobCancel, _, _>(bind(ctx, executor_job_cancel))
        .method::<ExecutorJobDismiss, _, _>(bind(ctx, executor_job_dismiss))
        .method::<ExecutorJobLog, _, _>(bind(ctx, executor_job_log))
        .method::<ExecutorAutoUpdateSet, _, _>(bind(ctx, executor_auto_update_set))
        .method::<SecretList, _, _>(bind(ctx, secret_list))
        .method::<SecretSet, _, _>(bind(ctx, secret_set))
        .method::<SecretDelete, _, _>(bind(ctx, secret_delete))
        .method::<WorkspaceList, _, _>(bind(ctx, workspace_list))
        .method::<WorkspaceGet, _, _>(bind(ctx, workspace_get))
        .method::<WorkspaceDiff, _, _>(bind(ctx, workspace_diff))
        .method::<WorkspaceClean, _, _>(bind(ctx, workspace_clean))
        .subscription::<RunEvents, _, _, _>(bind(ctx, run_events))
        .subscription::<RunsWatch, _, _, _>(bind(ctx, runs_watch))
        .subscription::<ApprovalsWatch, _, _, _>(bind(ctx, approvals_watch))
        .subscription::<DefsWatch, _, _, _>(bind(ctx, defs_watch))
        .subscription::<TriggersWatch, _, _, _>(bind(ctx, triggers_watch))
        .subscription::<ExecutorsWatch, _, _, _>(bind(ctx, executors_watch))
        .guard(connection_guard)
}

type Reply<T> = Result<T, ErrorObject>;

// ----- Connections that speak for a run (T1.9).
//
// A connection is the owner's until `agent.auth` binds it to a run with the run's token; from then
// on it may call `agent.*` and nothing else (the guard below), so a run token cannot reach
// `approval.decide`, `budget.pause`, `task.run` or any other method of the owner. The other
// direction too: an `agent.*` method on a connection that has not authenticated is refused. The
// peer's uid was checked when it connected, and that is all the owner's connection needs: a
// process of the same user that never presents a token is the owner (ARCHITECTURE 8 point 2).

/// Whether a connection that is bound to a run may call `method`: the handshake, and `agent.*`
/// once authenticated.
fn run_connection_may_call(method: &str) -> bool {
    method == "daemon.hello" || (method.starts_with("agent.") && method != "agent.auth")
}

/// The router's guard: keeps each kind of connection to its own methods.
fn connection_guard(conn: &Context, method: &str) -> Reply<()> {
    let bound = conn.state::<AgentSession>().is_some();
    if bound && !run_connection_may_call(method) {
        return Err(ErrorObject::new(
            ErrorCode::Unauthorized,
            format!("a run connection may call only agent.* methods, not {method}"),
        ));
    }
    if !bound && method.starts_with("agent.") && method != "agent.auth" {
        return Err(ErrorObject::new(
            ErrorCode::Unauthorized,
            "authenticate with agent.auth first",
        ));
    }
    Ok(())
}

/// The session the connection was bound to by `agent.auth`.
fn bound(conn: &Context) -> Reply<Arc<AgentSession>> {
    conn.state::<AgentSession>().ok_or_else(|| {
        ErrorObject::new(
            ErrorCode::Unauthorized,
            "authenticate with agent.auth first",
        )
    })
}

async fn agent_auth(ctx: Arc<Ctx>, conn: Context, p: AgentAuthParams) -> Reply<AgentAuthResult> {
    if conn.state::<AgentSession>().is_some() {
        return Err(ErrorObject::new(
            ErrorCode::Conflict,
            "this connection is already bound to a run",
        ));
    }
    // The kernel sees the digest; the token goes no further than this call.
    let digest = token_digest(&p.token);
    let session = ctx
        .kernel
        .agent_authenticate(&p.run_id, &digest)
        .await
        .map_err(|e| {
            tracing::warn!(run = %p.run_id, "agent.auth refused");
            kernel_error(&e)
        })?;
    let result = AgentAuthResult {
        run_id: session.run().clone(),
        bot: session.bot().map(str::to_owned),
        scope: session.scope().to_owned(),
    };
    if !conn.set_state(session) {
        return Err(ErrorObject::new(
            ErrorCode::Conflict,
            "this connection is already bound to a run",
        ));
    }
    Ok(result)
}

async fn agent_report(
    ctx: Arc<Ctx>,
    conn: Context,
    p: AgentReportParams,
) -> Reply<AgentReportResult> {
    let session = bound(&conn)?;
    ctx.kernel
        .agent_report(&session, p)
        .await
        .map_err(|e| kernel_error(&e))?;
    Ok(AgentReportResult {})
}

async fn agent_request_approval(
    ctx: Arc<Ctx>,
    conn: Context,
    p: AgentApprovalParams,
) -> Reply<AgentApprovalResult> {
    let session = bound(&conn)?;
    ctx.kernel
        .agent_request_approval(&session, p)
        .await
        .map_err(|e| kernel_error(&e))
}

async fn agent_board_read(
    ctx: Arc<Ctx>,
    conn: Context,
    p: AgentBoardReadParams,
) -> Reply<AgentBoardReadResult> {
    let session = bound(&conn)?;
    let (scope, entries) = ctx
        .kernel
        .agent_board_read(&session, p.key.as_deref())
        .await
        .map_err(|e| kernel_error(&e))?;
    Ok(AgentBoardReadResult { scope, entries })
}

async fn agent_board_write(
    ctx: Arc<Ctx>,
    conn: Context,
    p: AgentBoardWriteParams,
) -> Reply<AgentBoardWriteResult> {
    let session = bound(&conn)?;
    let (scope, version) = ctx
        .kernel
        .agent_board_write(&session, &p.key, &p.value, p.expected_version)
        .await
        .map_err(|e| kernel_error(&e))?;
    Ok(AgentBoardWriteResult { scope, version })
}

async fn hello(ctx: Arc<Ctx>, p: HelloParams) -> Reply<HelloResult> {
    if p.protocol_version < PROTOCOL_VERSION {
        return Err(ErrorObject::new(
            ErrorCode::ProtocolVersionMismatch,
            format!(
                "this daemon speaks protocol {PROTOCOL_VERSION}; {} asked for {}",
                p.client_name, p.protocol_version
            ),
        ));
    }
    tracing::info!(client = %p.client_name, protocol = p.protocol_version, "client said hello");
    Ok(HelloResult {
        server_version: env!("CARGO_PKG_VERSION").to_owned(),
        protocol_version: PROTOCOL_VERSION,
        features: FEATURES
            .iter()
            .filter(|feature| p.features.iter().any(|offered| offered == *feature))
            // Offered only when the daemon can do it: a client that sees the feature shows
            // Update and Install buttons.
            .filter(|feature| ctx.maintenance || **feature != FEATURE_EXECUTOR_MAINTAIN)
            .map(|feature| (*feature).to_owned())
            .collect(),
        pid: std::process::id(),
    })
}

async fn count(ctx: &Ctx, statuses: &[RunStatus]) -> Reply<u32> {
    let n = ctx
        .kernel
        .count_runs(statuses)
        .await
        .map_err(|e| kernel_error(&e))?;
    Ok(u32::try_from(n).unwrap_or(u32::MAX))
}

async fn status(ctx: Arc<Ctx>, _: NoParams) -> Reply<DaemonStatusInfo> {
    Ok(DaemonStatusInfo {
        version: env!("CARGO_PKG_VERSION").to_owned(),
        pid: std::process::id(),
        uptime_secs: ctx.started.elapsed().as_secs(),
        active_runs: count(&ctx, &ACTIVE).await?,
        queued_runs: count(&ctx, &[RunStatus::Queued]).await?,
        definitions_watch: ctx.fs_defs.watch_problem(),
        // The executors found installed: one whose last probe says otherwise is left out.
        executors: ctx
            .kernel
            .executors()
            .into_iter()
            .filter(|info| {
                !info.health.as_ref().is_some_and(|health| {
                    matches!(
                        health.installed,
                        reno_proto::InstallStatus::NotInstalled { .. }
                    )
                })
            })
            .map(|info| info.descriptor.id)
            .collect(),
    })
}

/// Answers, then lets the daemon's main task run the shutdown sequence. The reply is not held
/// back: the sequence keeps connections open until it has finished its own work.
async fn shutdown(ctx: Arc<Ctx>, _: NoParams) -> Reply<DaemonShutdownResult> {
    tracing::info!("shutdown requested over the socket");
    ctx.stop.cancel();
    Ok(DaemonShutdownResult {})
}

async fn task_run(ctx: Arc<Ctx>, p: TaskRunParams) -> Reply<TaskRunResult> {
    if p.prompt.trim().is_empty() {
        return Err(invalid("the prompt is empty"));
    }
    if let Some(executor) = &p.executor
        && !ctx.executors.contains(executor)
    {
        let known: Vec<_> = ctx.executors.iter().map(ExecutorId::as_str).collect();
        return Err(ErrorObject::new(
            ErrorCode::NotFound,
            format!(
                "no executor `{executor}` on this daemon (available: {})",
                known.join(", ")
            ),
        ));
    }
    let cwd = match &p.cwd {
        Some(cwd) => {
            if !cwd.is_absolute() {
                return Err(invalid("cwd must be an absolute path"));
            }
            // Resolved, so that where a run works does not depend on how the path was spelled
            // (symlinks, `/var` against `/private/var`).
            match tokio::fs::canonicalize(cwd).await {
                Ok(resolved) if resolved.is_dir() => resolved,
                _ => return Err(invalid(format!("cwd {} is not a directory", cwd.display()))),
            }
        }
        None => ctx.default_cwd.clone(),
    };
    let mut p = p;
    let suffix = attachment_suffix(&p.attachments).await?;
    p.prompt.push_str(&suffix);
    let request = task_request(p, cwd);
    let (run_id, decision) = ctx
        .kernel
        .submit_task(request)
        .await
        .map_err(|e| kernel_error(&e))?;
    Ok(TaskRunResult {
        run_id,
        routing: Some(decision),
    })
}

/// Checks the attachments of a `task.run` and returns the text to append to its prompt.
///
/// The executors are given no new input channel: each attachment is a path the executor reads
/// itself, listed under "Attached files:" at the end of the prompt. So the daemon only has to
/// see that each is an absolute path to a readable regular file within the size cap, and that
/// there are not too many. The paths are resolved first, so the list names the files, not
/// symlinks to them.
async fn attachment_suffix(attachments: &[Attachment]) -> Result<String, ErrorObject> {
    if attachments.len() > ATTACHMENTS_MAX_COUNT {
        return Err(invalid(format!(
            "too many attachments: {} (at most {ATTACHMENTS_MAX_COUNT})",
            attachments.len()
        )));
    }
    let mut checked = Vec::with_capacity(attachments.len());
    for a in attachments {
        let shown = a.path.display();
        if !a.path.is_absolute() {
            return Err(invalid(format!(
                "attachment {shown} is not an absolute path"
            )));
        }
        let path = tokio::fs::canonicalize(&a.path)
            .await
            .map_err(|e| invalid(format!("attachment {shown} cannot be read: {e}")))?;
        let meta = tokio::fs::metadata(&path)
            .await
            .map_err(|e| invalid(format!("attachment {shown} cannot be read: {e}")))?;
        if !meta.is_file() {
            return Err(invalid(format!("attachment {shown} is not a file")));
        }
        if meta.len() > ATTACHMENT_MAX_BYTES {
            return Err(invalid(format!(
                "attachment {shown} is {} MiB (at most {} MiB)",
                meta.len().div_ceil(1024 * 1024),
                ATTACHMENT_MAX_BYTES / (1024 * 1024)
            )));
        }
        // Opening is the readable check: a mode the daemon's user cannot read fails here.
        tokio::fs::File::open(&path)
            .await
            .map_err(|e| invalid(format!("attachment {shown} cannot be read: {e}")))?;
        let name = if a.name.trim().is_empty() {
            path.file_name()
                .map_or_else(String::new, |n| n.to_string_lossy().into_owned())
        } else {
            a.name.clone()
        };
        checked.push(Attachment {
            path,
            name,
            mime: a.mime.clone(),
        });
    }
    Ok(Attachment::prompt_suffix(&checked))
}

/// The kernel's view of a `task.run`.
fn task_request(p: TaskRunParams, cwd: PathBuf) -> TaskRequest {
    let mut request = TaskRequest::new(p.prompt, cwd)
        .model(p.model.filter(|model| !model.is_empty()))
        .effort(p.effort)
        .model_options(p.model_options)
        .in_place(p.in_place);
    request.executor = p.executor;
    request.bot = p.bot;
    request
}

/// `route.preview`: the decision the task would get. Nothing is created, so an empty prompt, an
/// executor this daemon lacks and a directory that is not there are all answered as they would
/// route (the unknown executor shows as rejected), not refused.
async fn route_preview(ctx: Arc<Ctx>, p: RoutePreviewParams) -> Reply<RoutePreviewResult> {
    let cwd = p
        .task
        .cwd
        .clone()
        .unwrap_or_else(|| ctx.default_cwd.clone());
    let decision = ctx
        .kernel
        .preview_route(&task_request(p.task, cwd))
        .map_err(|e| kernel_error(&e))?;
    Ok(RoutePreviewResult { decision })
}

async fn run_routing(ctx: Arc<Ctx>, p: RunRoutingParams) -> Reply<RunRoutingResult> {
    let decision = ctx
        .kernel
        .run_routing(&p.run_id)
        .await
        .map_err(|e| kernel_error(&e))?;
    Ok(RunRoutingResult {
        run_id: p.run_id,
        decision,
    })
}

async fn run_get(ctx: Arc<Ctx>, p: RunGetParams) -> Reply<RunSummary> {
    ctx.kernel
        .get_run(&p.run_id)
        .await
        .map_err(|e| kernel_error(&e))
}

async fn run_list(ctx: Arc<Ctx>, p: RunListParams) -> Reply<RunListResult> {
    let (runs, next) = ctx
        .kernel
        .list_runs(p.status.as_deref(), p.limit, p.before.as_ref())
        .await
        .map_err(|e| kernel_error(&e))?;
    Ok(RunListResult { runs, next })
}

async fn run_cancel(ctx: Arc<Ctx>, p: RunCancelParams) -> Reply<RunCancelResult> {
    tracing::info!(run = %p.run_id, reason = ?p.reason, "cancel requested");
    let status = ctx
        .kernel
        .cancel(&p.run_id)
        .await
        .map_err(|e| kernel_error(&e))?;
    Ok(RunCancelResult { status })
}

async fn run_resume(ctx: Arc<Ctx>, p: RunResumeParams) -> Reply<RunResumeResult> {
    tracing::info!(run = %p.run_id, "resume requested");
    let prompt = p.prompt.filter(|prompt| !prompt.trim().is_empty());
    let attempt = ctx
        .kernel
        .resume_run(&p.run_id, prompt)
        .await
        .map_err(|e| kernel_error(&e))?;
    Ok(RunResumeResult {
        status: RunStatus::Queued,
        attempt,
    })
}

async fn run_steer(ctx: Arc<Ctx>, p: RunSteerParams) -> Reply<RunSteerResult> {
    if p.prompt.trim().is_empty() {
        return Err(invalid("the steering prompt is empty"));
    }
    // `steer` says "ended" for a run that does not exist; a caller wants to hear "not found".
    ctx.kernel
        .get_run(&p.run_id)
        .await
        .map_err(|e| kernel_error(&e))?;
    ctx.kernel
        .steer(&p.run_id, p.prompt)
        .await
        .map_err(|e| kernel_error(&e))?;
    Ok(RunSteerResult {})
}

async fn run_set_model(ctx: Arc<Ctx>, p: RunSetModelParams) -> Reply<RunSetModelResult> {
    if p.model.trim().is_empty() {
        return Err(invalid("the model is empty"));
    }
    let model = ctx
        .kernel
        .set_model(&p.run_id, p.model.trim())
        .await
        .map_err(|e| kernel_error(&e))?;
    Ok(RunSetModelResult { model })
}

async fn run_configure(ctx: Arc<Ctx>, p: RunConfigureParams) -> Reply<RunConfigureResult> {
    ctx.kernel
        .configure_run(&p.run_id, p.effort, p.model_options)
        .await
        .map_err(|e| kernel_error(&e))
}

async fn approval_list(ctx: Arc<Ctx>, _: NoParams) -> Reply<ApprovalListResult> {
    Ok(ApprovalListResult {
        approvals: ctx
            .kernel
            .pending_approvals()
            .await
            .map_err(|e| kernel_error(&e))?,
    })
}

async fn approval_history(ctx: Arc<Ctx>, p: ApprovalHistoryParams) -> Reply<ApprovalHistoryResult> {
    Ok(ApprovalHistoryResult {
        approvals: ctx
            .kernel
            .approval_history(p.run.as_ref(), p.limit)
            .await
            .map_err(|e| kernel_error(&e))?,
    })
}

async fn approval_decide(ctx: Arc<Ctx>, p: ApprovalDecideParams) -> Reply<ApprovalDecideResult> {
    ctx.kernel
        .decide_remembering(&p.approval_id, p.decision, p.message, p.remember)
        .await
        .map_err(|e| kernel_error(&e))?;
    Ok(ApprovalDecideResult {})
}

async fn rule_list(ctx: Arc<Ctx>, p: RuleListParams) -> Reply<RuleListResult> {
    let rules = match &p.project {
        Some(project) => {
            ctx.kernel
                .list_project_rules(project, p.include_inactive)
                .await
        }
        None => ctx.kernel.list_rules(p.include_inactive).await,
    };
    Ok(RuleListResult {
        rules: rules.map_err(|e| kernel_error(&e))?,
    })
}

async fn rule_add(ctx: Arc<Ctx>, p: RuleAddParams) -> Reply<reno_proto::StandingRule> {
    ctx.kernel
        .add_project_rule(
            &p.project,
            p.effect,
            &p.command,
            p.tool.as_deref(),
            p.expires_in_secs,
        )
        .await
        .map_err(|e| kernel_error(&e))
}

async fn rule_revoke(ctx: Arc<Ctx>, p: RuleRevokeParams) -> Reply<RuleRevokeResult> {
    ctx.kernel
        .revoke_rule(&p.id)
        .await
        .map_err(|e| kernel_error(&e))?;
    Ok(RuleRevokeResult {})
}

async fn def_list(ctx: Arc<Ctx>, p: DefListParams) -> Reply<DefsSnapshot> {
    Ok(ctx.kernel.current_defs().snapshot(p.kind))
}

async fn def_get(ctx: Arc<Ctx>, p: DefGetParams) -> Reply<DefRecord> {
    ctx.kernel
        .get_definition(p.kind, &p.name, p.version)
        .await
        .map_err(|e| kernel_error(&e))
}

async fn def_validate(ctx: Arc<Ctx>, p: DefValidateParams) -> Reply<DefValidateResult> {
    Ok(ctx.kernel.validate_definition(p.kind, &p.name, &p.body))
}

async fn trigger_list(ctx: Arc<Ctx>, _: NoParams) -> Reply<TriggerListResult> {
    Ok(TriggerListResult {
        triggers: ctx.kernel.list_triggers(),
    })
}

async fn trigger_fire(ctx: Arc<Ctx>, p: TriggerFireParams) -> Reply<TriggerFireResult> {
    tracing::info!(trigger = %p.name, "trigger fire requested");
    ctx.kernel
        .fire_trigger(&p.name)
        .await
        .map_err(|e| kernel_error(&e))
}

/// `budget.status`: each bot's limits and use today, and what is paused.
async fn budget_status(ctx: Arc<Ctx>, _: BudgetStatusParams) -> Reply<BudgetStatusResult> {
    ctx.kernel
        .budget_status()
        .await
        .map_err(|e| kernel_error(&e))
}

/// `budget.pause`: the kill switch, for everything or one bot.
async fn budget_pause(ctx: Arc<Ctx>, p: BudgetPauseParams) -> Reply<BudgetPauseResult> {
    tracing::warn!(bot = ?p.bot, stop_live = p.stop_live, "pause requested");
    ctx.kernel
        .pause(p.bot.as_deref(), p.stop_live)
        .await
        .map_err(|e| kernel_error(&e))
}

/// `budget.resume`: lifts a pause.
async fn budget_resume(ctx: Arc<Ctx>, p: BudgetResumeParams) -> Reply<BudgetResumeResult> {
    tracing::warn!(bot = ?p.bot, "resume requested");
    ctx.kernel
        .resume(p.bot.as_deref())
        .await
        .map_err(|e| kernel_error(&e))
}

async fn executor_list(ctx: Arc<Ctx>, _: NoParams) -> Reply<ExecutorListResult> {
    Ok(ExecutorListResult {
        executors: ctx.kernel.executors(),
    })
}

/// The frames each executor's wire layer did not recognise, by kind (T1.14).
async fn executor_frames(ctx: Arc<Ctx>, p: ExecutorFramesParams) -> Reply<ExecutorFramesResult> {
    Ok(ExecutorFramesResult {
        executors: ctx
            .kernel
            .executor_frames(p.id.as_ref())
            .map_err(|e| kernel_error(&e))?,
    })
}

/// Probes again and waits for the answer: the one call a person makes to say "I fixed it".
async fn executor_refresh(ctx: Arc<Ctx>, p: ExecutorRefreshParams) -> Reply<ExecutorListResult> {
    tracing::info!(executor = ?p.id, usage_only = p.usage_only, "executor refresh requested");
    let executors = if p.usage_only {
        ctx.kernel.refresh_executor_usage(p.id.as_ref()).await
    } else {
        ctx.kernel.refresh_executors(p.id.as_ref()).await
    };
    Ok(ExecutorListResult {
        executors: executors.map_err(|e| kernel_error(&e))?,
    })
}

/// Fetches and verifies the executor's managed adapter, probes it again and answers with the
/// result. Waits for the install, which can take a while.
async fn executor_install(ctx: Arc<Ctx>, p: ExecutorInstallParams) -> Reply<ExecutorInstallResult> {
    tracing::info!(executor = %p.id, "executor install requested");
    let executor = ctx
        .installer
        .install(&ctx.kernel, &p.id)
        .await
        .map_err(|e| install_error(&e))?;
    Ok(ExecutorInstallResult { executor })
}

// Installs and updates of executors. These run OUTSIDE the policy engine, on purpose: they are
// not agent actions. A person pressed Install or Update (or turned auto-update on), and the
// commands are compiled into Reno per adapter; an RPC names an executor and a kind and never
// carries a command string. They run as the user with no stdin and no sudo (see the runner).

/// Starts or queues an install or update and answers once it is started or queued.
async fn executor_job_start(ctx: Arc<Ctx>, p: ExecutorJobStartParams) -> Reply<ExecutorJobResult> {
    tracing::info!(executor = %p.id, kind = ?p.kind, force = p.force, "executor job requested");
    let executor = ctx
        .kernel
        .start_job(&p.id, p.kind, p.force)
        .await
        .map_err(|e| kernel_error(&e))?;
    Ok(ExecutorJobResult { executor })
}

/// Cancels the job of an executor: kills what it runs and ends it cancelled.
async fn executor_job_cancel(ctx: Arc<Ctx>, p: ExecutorJobParams) -> Reply<ExecutorJobResult> {
    tracing::info!(executor = %p.id, "executor job cancel requested");
    let executor = ctx
        .kernel
        .cancel_job(&p.id)
        .await
        .map_err(|e| kernel_error(&e))?;
    Ok(ExecutorJobResult { executor })
}

/// Clears a failed job.
async fn executor_job_dismiss(ctx: Arc<Ctx>, p: ExecutorJobParams) -> Reply<ExecutorJobResult> {
    let executor = ctx
        .kernel
        .dismiss_job(&p.id)
        .await
        .map_err(|e| kernel_error(&e))?;
    Ok(ExecutorJobResult { executor })
}

/// The last 500 lines of the current or last job.
async fn executor_job_log(ctx: Arc<Ctx>, p: ExecutorJobParams) -> Reply<ExecutorJobLogResult> {
    let (lines, truncated) = ctx.kernel.job_log(&p.id).map_err(|e| kernel_error(&e))?;
    Ok(ExecutorJobLogResult { lines, truncated })
}

/// Stores an executor's auto-update choice (and moves a self-updating CLI's own updater).
async fn executor_auto_update_set(
    ctx: Arc<Ctx>,
    p: ExecutorAutoUpdateSetParams,
) -> Reply<ExecutorJobResult> {
    tracing::info!(executor = %p.id, on = p.on, verified_only = p.verified_only, "auto-update setting requested");
    let executor = ctx
        .kernel
        .set_auto_update(&p.id, p.on, p.verified_only)
        .await
        .map_err(|e| kernel_error(&e))?;
    Ok(ExecutorJobResult { executor })
}

async fn workspace_list(ctx: Arc<Ctx>, p: WorkspaceListParams) -> Reply<WorkspaceListResult> {
    Ok(WorkspaceListResult {
        workspaces: ctx
            .kernel
            .workspaces(p.status.as_deref())
            .await
            .map_err(|e| kernel_error(&e))?,
    })
}

async fn workspace_get(ctx: Arc<Ctx>, p: WorkspaceGetParams) -> Reply<WorkspaceInfo> {
    ctx.kernel
        .workspace(&p.run_id)
        .await
        .map_err(|e| kernel_error(&e))
}

async fn workspace_diff(ctx: Arc<Ctx>, p: WorkspaceDiffParams) -> Reply<WorkspaceDiffResult> {
    ctx.kernel
        .workspace_diff(&p.run_id, p.path.as_deref())
        .await
        .map_err(|e| kernel_error(&e))
}

/// The secret store, or the refusal of a daemon that has none.
fn secret_port(ctx: &Ctx) -> Reply<&Arc<dyn Secrets>> {
    ctx.secrets.as_ref().ok_or_else(|| {
        ErrorObject::new(
            ErrorCode::Unavailable,
            "this daemon was started without a secret store",
        )
    })
}

/// Names and whether each is set: those in the Keychain, and those a bot's definition asks for.
/// Never a value.
async fn secret_list(ctx: Arc<Ctx>, _: NoParams) -> Reply<SecretListResult> {
    let stored = secret_port(&ctx)?
        .names()
        .await
        .map_err(|e| secrets_error(&e))?;
    let mut used_by: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    for (bot, def) in ctx.kernel.current_defs().set().bots() {
        for name in &def.secrets {
            used_by.entry(name.clone()).or_default().push(bot.clone());
        }
    }
    let mut names: std::collections::BTreeSet<String> = stored.iter().cloned().collect();
    names.extend(used_by.keys().cloned());
    let secrets = names
        .into_iter()
        .map(|name| SecretInfo {
            set: stored.contains(&name),
            used_by: used_by.remove(&name).unwrap_or_default(),
            name,
        })
        .collect();
    Ok(SecretListResult { secrets })
}

/// Stores a value straight into the Keychain. The value is not logged, and it is moved into the
/// port, never copied into a message.
async fn secret_set(ctx: Arc<Ctx>, p: SecretSetParams) -> Reply<SecretSetResult> {
    let port = secret_port(&ctx)?;
    if p.value.len() > SECRET_VALUE_MAX {
        return Err(invalid(format!(
            "a secret value is at most {SECRET_VALUE_MAX} bytes"
        )));
    }
    let SecretSetParams { name, value } = p;
    tracing::info!(secret = %name, "storing a secret");
    let replaced = port
        .set(&name, SecretValue::new(value))
        .await
        .map_err(|e| secrets_error(&e))?;
    Ok(SecretSetResult { replaced })
}

async fn secret_delete(ctx: Arc<Ctx>, p: SecretDeleteParams) -> Reply<SecretDeleteResult> {
    let port = secret_port(&ctx)?;
    tracing::info!(secret = %p.name, "deleting a secret");
    let deleted = port.delete(&p.name).await.map_err(|e| secrets_error(&e))?;
    Ok(SecretDeleteResult { deleted })
}

async fn workspace_clean(ctx: Arc<Ctx>, p: WorkspaceCleanParams) -> Reply<WorkspaceCleanResult> {
    tracing::info!(target = ?p.target, force = p.force, "workspace clean requested");
    Ok(WorkspaceCleanResult {
        items: ctx
            .kernel
            .clean_workspaces(p.target, p.force)
            .await
            .map_err(|e| kernel_error(&e))?,
    })
}

// ---- subscriptions -------------------------------------------------------------------------

async fn run_events(
    ctx: Arc<Ctx>,
    p: RunEventsParams,
) -> Reply<impl Stream<Item = SubItem<RunEventItem>> + Send + use<>> {
    let inner = ctx
        .kernel
        .run_events(&p.run_id, p.from_seq)
        .await
        .map_err(|e| kernel_error(&e))?;
    let state = Events {
        kernel: ctx.kernel.clone(),
        run: p.run_id,
        inner,
        stalled: 0,
    };
    Ok(stream::unfold(state, Events::step))
}

/// A `run.events` subscription in flight.
struct Events {
    kernel: Kernel,
    run: reno_proto::RunId,
    inner: BoxStream<'static, Result<RunEventItem, KernelError>>,
    /// Kernel gaps in a row with no event in between.
    stalled: u32,
}

impl Events {
    /// The next item, or `None` when the stream is over.
    ///
    /// When the kernel cuts this subscriber off (`Gap`) the daemon resumes it in place at
    /// `resume_from`, so the client sees no hole. Any other failure ends the stream with a
    /// log line: items cannot carry an error, so the subscription ends as if it were done. A
    /// consumer that must know the run finished has to have seen its `Done` (the CLI checks).
    async fn step(mut self) -> Option<(SubItem<RunEventItem>, Self)> {
        loop {
            match self.inner.next().await? {
                Ok(item) => {
                    self.stalled = 0;
                    return Some((SubItem::seq(item.seq, item), self));
                }
                Err(KernelError::Gap { resume_from }) => {
                    self.stalled += 1;
                    if self.stalled > MAX_STALLED_GAPS {
                        tracing::warn!(run = %self.run, "run.events keeps falling behind; ending it");
                        return None;
                    }
                    tracing::debug!(run = %self.run, %resume_from, "run.events resumes after a gap");
                    // Everything below `resume_from` was delivered before the cut-off.
                    match self.kernel.run_events(&self.run, Some(resume_from)).await {
                        Ok(inner) => self.inner = inner,
                        Err(error) => {
                            tracing::warn!(run = %self.run, %error, "run.events could not resume");
                            return None;
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(run = %self.run, %error, "run.events failed; ending it");
                    return None;
                }
            }
        }
    }
}

async fn runs_watch(
    ctx: Arc<Ctx>,
    p: RunsWatchParams,
) -> Reply<impl Stream<Item = SubItem<RunsSnapshot>> + Send + use<>> {
    let only = p.status;
    Ok(latest(
        ctx.kernel.watch_runs(),
        move |all: &Vec<RunSummary>| {
            let mut runs = all.clone();
            if let Some(only) = &only {
                runs.retain(|run| only.contains(&run.status));
            }
            RunsSnapshot { runs }
        },
    ))
}

async fn defs_watch(
    ctx: Arc<Ctx>,
    _: DefsWatchParams,
) -> Reply<impl Stream<Item = SubItem<DefsSnapshot>> + Send + use<>> {
    Ok(latest(
        ctx.kernel.watch_defs(),
        |current: &Arc<CurrentDefs>| current.snapshot(None),
    ))
}

async fn triggers_watch(
    ctx: Arc<Ctx>,
    _: TriggersWatchParams,
) -> Reply<impl Stream<Item = SubItem<TriggersSnapshot>> + Send + use<>> {
    Ok(latest(
        ctx.kernel.watch_triggers(),
        |all: &Arc<Vec<TriggerInfo>>| TriggersSnapshot {
            triggers: all.as_ref().clone(),
        },
    ))
}

async fn executors_watch(
    ctx: Arc<Ctx>,
    _: ExecutorsWatchParams,
) -> Reply<impl Stream<Item = SubItem<ExecutorsSnapshot>> + Send + use<>> {
    Ok(latest(
        ctx.kernel.watch_executors(),
        |all: &Arc<Vec<ExecutorInfo>>| ExecutorsSnapshot {
            executors: all.as_ref().clone(),
        },
    ))
}

async fn approvals_watch(
    ctx: Arc<Ctx>,
    _: ApprovalsWatchParams,
) -> Reply<impl Stream<Item = SubItem<ApprovalsSnapshot>> + Send + use<>> {
    Ok(latest(
        ctx.kernel.watch_approvals(),
        |all: &Vec<reno_proto::Approval>| ApprovalsSnapshot {
            approvals: all.clone(),
        },
    ))
}

/// A latest-value stream over a `watch`: the current value first, then each change. A reader that
/// is slow sees fewer values, never stale ones out of order. Ends if the sender is dropped.
fn latest<T, I, F>(rx: watch::Receiver<T>, map: F) -> impl Stream<Item = SubItem<I>> + Send
where
    T: Send + Sync + 'static,
    I: Send + 'static,
    F: Fn(&T) -> I + Send + 'static,
{
    stream::unfold((rx, map, true), |(mut rx, map, first)| async move {
        if !first {
            rx.changed().await.ok()?;
        }
        let item = map(&rx.borrow_and_update());
        Some((SubItem::latest(item), (rx, map, false)))
    })
}
