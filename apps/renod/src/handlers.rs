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
//! | `run.steer` | `get_run`, `steer` |
//! | `run.events` | `run_events` |
//! | `runs.watch` | `watch_runs` |
//! | `approval.list`, `approval.decide` | `pending_approvals`, `decide` |
//! | `approval.history` | `approval_history` |
//! | `approvals.watch` | `watch_approvals` |
//! | `def.list` | `current_defs` |
//! | `def.get` | `get_definition` |
//! | `def.validate` | `validate_definition` |
//! | `defs.watch` | `watch_defs` |
//! | `trigger.list` | `list_triggers` |
//! | `trigger.fire` | `fire_trigger` |
//! | `triggers.watch` | `watch_triggers` |
//! | `workspace.list`, `workspace.get` | `workspaces`, `workspace` |
//! | `workspace.clean` | `clean_workspaces` |
//! | `executor.list`, `executor.refresh` | `executors`, `refresh_executors` |
//! | `executors.watch` | `watch_executors` |
//!
//! `sub.gap` is sent by `reno-rpc` when a socket reader falls behind; the daemon registers no
//! handler for it. Every kernel failure goes through [`crate::errors`].
//!
//! Phase 0 has one kind of connection, the owner (ARCHITECTURE 8), so there are no per-method
//! capability checks yet; the peer's uid was checked when it connected.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use futures::stream::{self, BoxStream};
use futures::{Stream, StreamExt};
use reno_kernel::{CurrentDefs, Kernel, KernelError, TaskRequest};
use reno_proto::defs::{
    DefGetParams, DefListParams, DefRecord, DefValidateParams, DefValidateResult, DefsSnapshot,
    DefsWatchParams,
};
use reno_proto::{
    ApprovalDecide, ApprovalDecideParams, ApprovalDecideResult, ApprovalHistory,
    ApprovalHistoryParams, ApprovalHistoryResult, ApprovalList, ApprovalListResult,
    ApprovalsSnapshot, ApprovalsWatch, ApprovalsWatchParams, DaemonHello, DaemonShutdown,
    DaemonShutdownResult, DaemonStatus, DaemonStatusInfo, ErrorCode, ErrorObject, ExecutorId,
    FEATURES, HelloParams, HelloResult, NoParams, PROTOCOL_VERSION, RunCancel, RunCancelParams,
    RunCancelResult, RunEventItem, RunEvents, RunEventsParams, RunGet, RunGetParams, RunList,
    RunListParams, RunListResult, RunStatus, RunSteer, RunSteerParams, RunSteerResult, RunSummary,
    RunsSnapshot, RunsWatch, RunsWatchParams, TaskRun, TaskRunParams, TaskRunResult,
};
use reno_proto::{
    DefGet, DefList, DefValidate, DefsWatch, TriggerFire, TriggerFireParams, TriggerFireResult,
    TriggerInfo, TriggerList, TriggerListResult, TriggersSnapshot, TriggersWatch,
    TriggersWatchParams,
};
use reno_proto::{
    ExecutorInfo, ExecutorList, ExecutorListResult, ExecutorRefresh, ExecutorRefreshParams,
    ExecutorsSnapshot, ExecutorsWatch, ExecutorsWatchParams,
};
use reno_proto::{
    RoutePreview, RoutePreviewParams, RoutePreviewResult, RunRouting, RunRoutingParams,
    RunRoutingResult,
};
use reno_proto::{
    WorkspaceClean, WorkspaceCleanParams, WorkspaceCleanResult, WorkspaceGet, WorkspaceGetParams,
    WorkspaceInfo, WorkspaceList, WorkspaceListParams, WorkspaceListResult,
};
use reno_rpc::{Context, Router, SubItem};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::errors::{invalid, kernel_error};

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
    pub(crate) started: Instant,
    /// The executors a `task.run` may name, sorted.
    pub(crate) executors: Vec<ExecutorId>,
    /// Where a task with no `cwd` runs.
    pub(crate) default_cwd: PathBuf,
    /// Cancelled to ask the daemon to shut down.
    pub(crate) stop: CancellationToken,
}

/// Adapts `handler(ctx, params)` to the closure shape `Router` takes.
fn bind<P, Fut, F>(ctx: &Arc<Ctx>, handler: F) -> impl Fn(P, Context) -> Fut + Send + Sync + 'static
where
    F: Fn(Arc<Ctx>, P) -> Fut + Send + Sync + 'static,
{
    let ctx = Arc::clone(ctx);
    move |params, _conn| handler(Arc::clone(&ctx), params)
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
        .method::<RunList, _, _>(bind(ctx, run_list))
        .method::<RunCancel, _, _>(bind(ctx, run_cancel))
        .method::<RunSteer, _, _>(bind(ctx, run_steer))
        .method::<ApprovalList, _, _>(bind(ctx, approval_list))
        .method::<ApprovalDecide, _, _>(bind(ctx, approval_decide))
        .method::<ApprovalHistory, _, _>(bind(ctx, approval_history))
        .method::<DefList, _, _>(bind(ctx, def_list))
        .method::<DefGet, _, _>(bind(ctx, def_get))
        .method::<DefValidate, _, _>(bind(ctx, def_validate))
        .method::<TriggerList, _, _>(bind(ctx, trigger_list))
        .method::<TriggerFire, _, _>(bind(ctx, trigger_fire))
        .method::<ExecutorList, _, _>(bind(ctx, executor_list))
        .method::<ExecutorRefresh, _, _>(bind(ctx, executor_refresh))
        .method::<WorkspaceList, _, _>(bind(ctx, workspace_list))
        .method::<WorkspaceGet, _, _>(bind(ctx, workspace_get))
        .method::<WorkspaceClean, _, _>(bind(ctx, workspace_clean))
        .subscription::<RunEvents, _, _, _>(bind(ctx, run_events))
        .subscription::<RunsWatch, _, _, _>(bind(ctx, runs_watch))
        .subscription::<ApprovalsWatch, _, _, _>(bind(ctx, approvals_watch))
        .subscription::<DefsWatch, _, _, _>(bind(ctx, defs_watch))
        .subscription::<TriggersWatch, _, _, _>(bind(ctx, triggers_watch))
        .subscription::<ExecutorsWatch, _, _, _>(bind(ctx, executors_watch))
}

type Reply<T> = Result<T, ErrorObject>;

async fn hello(_ctx: Arc<Ctx>, p: HelloParams) -> Reply<HelloResult> {
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

/// The kernel's view of a `task.run`.
fn task_request(p: TaskRunParams, cwd: PathBuf) -> TaskRequest {
    let mut request = TaskRequest::new(p.prompt, cwd)
        .model(p.model.filter(|model| !model.is_empty()))
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
        .decide(&p.approval_id, p.decision)
        .await
        .map_err(|e| kernel_error(&e))?;
    Ok(ApprovalDecideResult {})
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

async fn executor_list(ctx: Arc<Ctx>, _: NoParams) -> Reply<ExecutorListResult> {
    Ok(ExecutorListResult {
        executors: ctx.kernel.executors(),
    })
}

/// Probes again and waits for the answer: the one call a person makes to say "I fixed it".
async fn executor_refresh(ctx: Arc<Ctx>, p: ExecutorRefreshParams) -> Reply<ExecutorListResult> {
    tracing::info!(executor = ?p.id, "executor refresh requested");
    Ok(ExecutorListResult {
        executors: ctx
            .kernel
            .refresh_executors(p.id.as_ref())
            .await
            .map_err(|e| kernel_error(&e))?,
    })
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
