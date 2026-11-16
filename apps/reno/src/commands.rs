//! `reno task run`, `reno run ...`, `reno trigger list|fire`, `reno workspace list|show|clean`, `reno approval list`, `reno approval history`,
//! `reno approve` and `reno deny`.

use std::io;
use std::process::ExitCode;

use anyhow::{Context, bail};
use futures::StreamExt;
use reno_client::Client;
use reno_proto::{
    ApprovalDecision, ApprovalHistoryResult, ApprovalListResult, CleanTarget, ExecEvent,
    ExecutorListResult, RUN_LIST_MAX_LIMIT, RoutingDecision, RunCancelResult, RunId, RunListParams,
    RunListResult, RunSummary, Seq, StopReason, TaskRunParams, TriggerListResult,
    WorkspaceCleanResult, WorkspaceListResult,
};
use serde::Serialize;

use crate::cli::{
    ApprovalCommand, ApprovalHistory, Decide, ExecutorCommand, ExecutorInstall, ExecutorRefresh,
    Pause, Resume, RunCancel, RunCommand, RunList, RunShow, RunTail, TaskCommand, TaskRun,
    TriggerCommand, TriggerFire, WorkspaceClean, WorkspaceCommand, WorkspaceList, WorkspaceShow,
};
use crate::env::{Env, print_json, print_line, print_note};
use crate::render::{self, Finished, Transcript};

pub(crate) async fn task(env: &Env, command: TaskCommand) -> anyhow::Result<ExitCode> {
    match command {
        TaskCommand::Run(args) => task_run(env, args).await,
    }
}

pub(crate) async fn run(env: &Env, command: RunCommand) -> anyhow::Result<ExitCode> {
    match command {
        RunCommand::List(args) => run_list(env, args).await,
        RunCommand::Show(args) => run_show(env, args).await,
        RunCommand::Tail(args) => run_tail(env, args).await,
        RunCommand::Cancel(args) => run_cancel(env, args).await,
    }
}

pub(crate) async fn trigger(env: &Env, command: TriggerCommand) -> anyhow::Result<ExitCode> {
    match command {
        TriggerCommand::List => trigger_list(env).await,
        TriggerCommand::Fire(args) => trigger_fire(env, args).await,
    }
}

pub(crate) async fn executor(env: &Env, command: ExecutorCommand) -> anyhow::Result<ExitCode> {
    match command {
        ExecutorCommand::List => executor_list(env).await,
        ExecutorCommand::Refresh(args) => executor_refresh(env, args).await,
        ExecutorCommand::Install(args) => executor_install(env, args).await,
    }
}

pub(crate) async fn workspace(env: &Env, command: WorkspaceCommand) -> anyhow::Result<ExitCode> {
    match command {
        WorkspaceCommand::List(args) => workspace_list(env, args).await,
        WorkspaceCommand::Show(args) => workspace_show(env, args).await,
        WorkspaceCommand::Clean(args) => workspace_clean(env, args).await,
    }
}

pub(crate) async fn approval(env: &Env, command: ApprovalCommand) -> anyhow::Result<ExitCode> {
    match command {
        ApprovalCommand::List => approval_list(env).await,
        ApprovalCommand::History(args) => approval_history(env, args).await,
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Submitted<'a> {
    run_id: &'a RunId,
    /// Where the router sent the run and why; absent from a daemon that predates routing.
    #[serde(skip_serializing_if = "Option::is_none")]
    routing: Option<&'a RoutingDecision>,
}

async fn task_run(env: &Env, args: TaskRun) -> anyhow::Result<ExitCode> {
    // Without `--executor` (and without a bot) the daemon's router chooses; naming one pins it.
    let executor = args
        .executor
        .as_deref()
        .map(|name| {
            name.parse()
                .with_context(|| format!("`{name}` is not an executor name"))
        })
        .transpose()?;
    let cwd = match args.cwd {
        Some(cwd) => cwd,
        None => std::env::current_dir().context("cannot read the current directory")?,
    };
    // The daemon wants an absolute path, and it is not in this process's directory.
    let cwd = std::path::absolute(&cwd)
        .with_context(|| format!("cannot make {} absolute", cwd.display()))?;

    let client = env.connect().await?;
    let submitted = client
        .submit_task(TaskRunParams {
            prompt: args.prompt,
            executor,
            bot: args.bot,
            cwd: Some(cwd),
            model: args.model,
            in_place: args.in_place,
            attachments: Vec::new(),
        })
        .await
        .map_err(|e| env.explain(e))?;
    let run = submitted.run_id;
    if env.json {
        print_json(&Submitted {
            run_id: &run,
            routing: submitted.routing.as_ref(),
        })?;
    } else if let Some(decision) = &submitted.routing {
        // On stderr, so stdout stays the transcript that `run tail` prints too.
        print_note(format!("routed to {}", decision.explain()))?;
    }
    follow(env, &client, run, None).await
}

/// One run with its routing decision.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Shown {
    run: RunSummary,
    #[serde(skip_serializing_if = "Option::is_none")]
    routing: Option<RoutingDecision>,
}

async fn run_show(env: &Env, args: RunShow) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let run = client
        .get_run(&args.run)
        .await
        .map_err(|e| env.explain(e))?;
    let routing = client
        .run_routing(&args.run)
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&Shown { run, routing })?;
        return Ok(ExitCode::SUCCESS);
    }
    print_line(render::run_line(&run))?;
    match &routing {
        Some(decision) => {
            for line in render::routing_lines(decision) {
                print_line(line)?;
            }
        }
        None => print_line("routing: none recorded (the run predates routing)")?,
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_tail(env: &Env, args: RunTail) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    follow(env, &client, args.run, args.from).await
}

/// Streams a run's events to stdout until its `Done`. Exits 0 only for a completed run, and 1
/// for any other end, including a stream that stops without one.
async fn follow(
    env: &Env,
    client: &Client,
    run: RunId,
    from: Option<Seq>,
) -> anyhow::Result<ExitCode> {
    let mut transcript = Transcript::new(io::stdout(), env.json);
    let mut events = Box::pin(client.run_events(run.clone(), from));
    let mut finished: Option<Finished> = None;
    while let Some(item) = events.next().await {
        let item = item.map_err(|e| env.explain(e))?;
        finished = transcript.event(&item)?;
        if matches!(item.event, ExecEvent::NeedsApproval { .. }) && !env.json {
            // The daemon records the approval before the event is published.
            let pending: Vec<_> = client
                .list_approvals()
                .await
                .map_err(|e| env.explain(e))?
                .into_iter()
                .filter(|approval| approval.run_id == run)
                .collect();
            transcript.approval_hint(&pending)?;
        }
        if finished.is_some() {
            break;
        }
    }
    let Some(finished) = finished else {
        bail!(
            "the event stream ended before run {run} finished; follow it with `reno run tail {run}`"
        );
    };
    if finished.stop != StopReason::Completed
        && let Some(diagnostic) = &finished.diagnostic
    {
        bail!("the run did not complete: {diagnostic}");
    }
    Ok(if finished.stop == StopReason::Completed {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

async fn run_list(env: &Env, args: RunList) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let mut runs = Vec::new();
    let mut before = None;
    // `--all` pages with the largest page the daemon allows; otherwise one page of `--limit`.
    let page = if args.all {
        RUN_LIST_MAX_LIMIT
    } else {
        args.limit.min(RUN_LIST_MAX_LIMIT)
    };
    let mut want = if args.all {
        usize::MAX
    } else {
        usize::try_from(args.limit).unwrap_or(usize::MAX)
    };
    let next = loop {
        let result = client
            .list_runs_page(RunListParams {
                status: None,
                limit: Some(page.min(u32::try_from(want).unwrap_or(u32::MAX))),
                before: before.take(),
            })
            .await
            .map_err(|e| env.explain(e))?;
        want = want.saturating_sub(result.runs.len());
        runs.extend(result.runs);
        match result.next {
            Some(cursor) if want > 0 => before = Some(cursor),
            other => break other,
        }
    };
    if env.json {
        print_json(&RunListResult { runs, next })?;
    } else if runs.is_empty() {
        print_line("no runs")?;
    } else {
        for run in &runs {
            print_line(render::run_line(run))?;
        }
        if next.is_some() {
            print_line("more runs exist: raise --limit, or use --all")?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn run_cancel(env: &Env, args: RunCancel) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let status = client
        .cancel_run(&args.run, Some("cancelled from the CLI".to_owned()))
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&RunCancelResult { status })?;
    } else {
        print_line(format_args!(
            "cancelling {} (it was {})",
            args.run,
            render::status_name(status)
        ))?;
    }
    Ok(ExitCode::SUCCESS)
}

async fn executor_list(env: &Env) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let executors = client.list_executors().await.map_err(|e| env.explain(e))?;
    print_executors(env, executors)
}

async fn executor_refresh(env: &Env, args: ExecutorRefresh) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let mut executors = client
        .refresh_executors(args.id.clone())
        .await
        .map_err(|e| env.explain(e))?;
    // Naming one executor asked about that one: show it, not the whole list.
    if let Some(id) = &args.id {
        executors.retain(|info| &info.descriptor.id == id);
    }
    print_executors(env, executors)
}

async fn executor_install(env: &Env, args: ExecutorInstall) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let executor = client
        .install_executor(&args.id)
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&reno_proto::ExecutorInstallResult { executor })?;
    } else {
        print_line(render::executor_line(&executor))?;
    }
    Ok(ExitCode::SUCCESS)
}

fn print_executors(
    env: &Env,
    executors: Vec<reno_proto::ExecutorInfo>,
) -> anyhow::Result<ExitCode> {
    if env.json {
        print_json(&ExecutorListResult { executors })?;
    } else if executors.is_empty() {
        print_line("no executors")?;
    } else {
        for info in &executors {
            print_line(render::executor_line(info))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn trigger_list(env: &Env) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let triggers = client.list_triggers().await.map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&TriggerListResult { triggers })?;
    } else if triggers.is_empty() {
        print_line("no triggers are defined")?;
    } else {
        for trigger in &triggers {
            print_line(render::trigger_line(trigger))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub(crate) async fn pause(env: &Env, args: Pause) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let paused = client
        .pause(args.bot.as_deref(), args.stop_live)
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&paused)?;
    } else {
        print_line(render::pause_line(args.bot.as_deref(), &paused))?;
    }
    Ok(ExitCode::SUCCESS)
}

pub(crate) async fn resume(env: &Env, args: Resume) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let resumed = client
        .resume(args.bot.as_deref())
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&resumed)?;
    } else {
        print_line(render::resume_line(args.bot.as_deref(), &resumed))?;
    }
    Ok(ExitCode::SUCCESS)
}

pub(crate) async fn budget(env: &Env) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let status = client.budget_status().await.map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&status)?;
    } else {
        for line in render::budget_lines(&status) {
            print_line(line)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn trigger_fire(env: &Env, args: TriggerFire) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let fired = client
        .fire_trigger(&args.name)
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&fired)?;
    } else {
        print_line(render::fired_line(&args.name, &fired))?;
    }
    Ok(ExitCode::SUCCESS)
}

async fn workspace_list(env: &Env, args: WorkspaceList) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let status = (!args.status.is_empty()).then_some(args.status);
    let workspaces = client
        .list_workspaces(status)
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&WorkspaceListResult { workspaces })?;
    } else if workspaces.is_empty() {
        print_line("no workspaces")?;
    } else {
        for workspace in &workspaces {
            print_line(render::workspace_line(workspace))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn workspace_show(env: &Env, args: WorkspaceShow) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let workspace = client
        .get_workspace(args.run)
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&workspace)?;
    } else {
        for line in render::workspace_detail(&workspace) {
            print_line(line)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn workspace_clean(env: &Env, args: WorkspaceClean) -> anyhow::Result<ExitCode> {
    let target = match (args.run, args.merged, args.all_clean) {
        (Some(run_id), false, false) => CleanTarget::Run { run_id },
        (None, true, false) => CleanTarget::Merged,
        (None, false, true) => CleanTarget::AllClean,
        _ => bail!("give one of: a run, --merged or --all-clean"),
    };
    let client = env.connect().await?;
    let items = client
        .clean_workspaces(target, args.force)
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&WorkspaceCleanResult { items })?;
    } else if items.is_empty() {
        print_line("nothing to clean")?;
    } else {
        for item in &items {
            print_line(render::clean_line(item))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn approval_list(env: &Env) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let approvals = client.list_approvals().await.map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&ApprovalListResult { approvals })?;
    } else if approvals.is_empty() {
        print_line("no approvals are waiting")?;
    } else {
        for approval in &approvals {
            print_line(render::approval_line(approval))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

async fn approval_history(env: &Env, args: ApprovalHistory) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let approvals = client
        .approval_history(args.run, Some(args.limit))
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&ApprovalHistoryResult { approvals })?;
    } else if approvals.is_empty() {
        print_line("no approvals have been decided")?;
    } else {
        for record in &approvals {
            print_line(render::history_line(record))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Decided<'a> {
    approval_id: &'a reno_proto::ApprovalId,
    decision: ApprovalDecision,
}

pub(crate) async fn decide(
    env: &Env,
    args: Decide,
    decision: ApprovalDecision,
) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    client
        .decide(&args.approval, decision)
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&Decided {
            approval_id: &args.approval,
            decision,
        })?;
    } else {
        let verb = match decision {
            ApprovalDecision::ApproveOnce => "approved",
            ApprovalDecision::Deny => "denied",
        };
        print_line(format_args!("{verb} {}", args.approval))?;
    }
    Ok(ExitCode::SUCCESS)
}
