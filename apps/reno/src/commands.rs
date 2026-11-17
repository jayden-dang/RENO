//! `reno task run`, `reno run ...`, `reno trigger list|fire`, `reno workspace list|show|clean`, `reno approval list`, `reno approval history`,
//! `reno approve` and `reno deny`.

use std::collections::HashSet;
use std::io;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, bail};
use futures::StreamExt;
use reno_client::Client;
use reno_proto::{
    ApprovalDecision, ApprovalHistoryResult, ApprovalListResult, CleanOutcome, CleanTarget,
    ExecEvent, ExecutorFramesResult, ExecutorListResult, RUN_LIST_MAX_LIMIT, RoutingDecision,
    RunCancelResult, RunId, RunListParams, RunListResult, RunSummary, Seq, StopReason,
    TaskRunParams, TriggerListResult, WorkspaceCleanResult, WorkspaceListResult,
};
use serde::Serialize;

use crate::cli::{
    ApprovalCommand, ApprovalHistory, Decide, ExecutorCommand, ExecutorFrames, ExecutorInstall,
    ExecutorRefresh, Pause, Resume, RunCancel, RunCommand, RunList, RunResume, RunShow, RunTail,
    TaskCommand, TaskRun, TriggerCommand, TriggerFire, WorkspaceClean, WorkspaceCommand,
    WorkspaceList, WorkspaceShow,
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
        RunCommand::Resume(args) => run_resume(env, args).await,
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
        ExecutorCommand::Frames(args) => executor_frames(env, args).await,
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
    /// The model the executor reported when the run began, when it differs from `run.model`
    /// (which is what was asked for, for example an alias).
    #[serde(skip_serializing_if = "Option::is_none")]
    started_model: Option<String>,
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
    let started = started_model(&client, &run.run_id).await;
    if env.json {
        let started_model = started.filter(|model| run.model.as_ref() != Some(model));
        print_json(&Shown {
            run,
            routing,
            started_model,
        })?;
        return Ok(ExitCode::SUCCESS);
    }
    print_line(render::run_line(&run))?;
    if let Some(line) = render::model_line(&run, started.as_deref()) {
        print_line(line)?;
    }
    for line in render::stop_lines(&run) {
        print_line(line)?;
    }
    match &routing {
        Some(decision) => {
            for line in render::routing_lines(decision, args.verbose) {
                print_line(line)?;
            }
        }
        None => print_line("routing: none recorded (the run predates routing)")?,
    }
    Ok(ExitCode::SUCCESS)
}

/// The model the run's executor reported when it began (the first event of the journal), if the
/// journal has one to read at once. A run that has not started yet has none, and is not waited
/// for.
async fn started_model(client: &Client, run: &RunId) -> Option<String> {
    let mut events = Box::pin(client.run_events(run.clone(), None));
    let first = tokio::time::timeout(Duration::from_millis(750), events.next())
        .await
        .ok()??
        .ok()?;
    match first.event {
        ExecEvent::Started { model, .. } => Some(model),
        _ => None,
    }
}

async fn run_tail(env: &Env, args: RunTail) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    follow(env, &client, args.run, args.from).await
}

/// Streams a run's events to stdout until the stream ends, which it does after the run's final
/// `Done` (one that a `Resumed` follows is not final). Exits 0 only for a completed run, and 1
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
    // Approval ids already pointed at, so two held requests never show the same one.
    let mut claimed = HashSet::new();
    while let Some(item) = events.next().await {
        let item = item.map_err(|e| env.explain(e))?;
        // A `Done` ends the run unless a `Resumed` follows it: then the next attempt goes on.
        if matches!(item.event, ExecEvent::Resumed { .. }) {
            finished = None;
        }
        if let Some(done) = transcript.event(&item)? {
            finished = Some(done);
        }
        if let ExecEvent::NeedsApproval {
            request_id,
            subject,
        } = &item.event
            && !env.json
        {
            // The daemon records the approval before the event is published.
            let pending: Vec<_> = client
                .list_approvals()
                .await
                .map_err(|e| env.explain(e))?
                .into_iter()
                .filter(|approval| approval.run_id == run)
                .collect();
            // Requests answered since (a replay of an old run) are in the history, with who
            // decided; a daemon without it only knows the held ones.
            let history = if client.supports(reno_proto::FEATURE_APPROVAL_HISTORY) {
                client
                    .approval_history(
                        Some(run.clone()),
                        Some(reno_proto::APPROVAL_HISTORY_MAX_LIMIT),
                    )
                    .await
                    .map_err(|e| env.explain(e))?
            } else {
                Vec::new()
            };
            let hint =
                render::approval_hint_for(request_id, subject, &pending, &history, &mut claimed);
            transcript.approval_hint(&hint)?;
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

async fn run_resume(env: &Env, args: RunResume) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let attempt = client
        .resume_run(&args.run, args.prompt)
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&reno_proto::RunResumeResult {
            status: reno_proto::RunStatus::Queued,
            attempt,
        })?;
    } else {
        print_line(format_args!(
            "resuming {} as attempt {attempt}; follow it with `reno run tail {}`",
            args.run, args.run
        ))?;
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

async fn executor_frames(env: &Env, args: ExecutorFrames) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let executors = client
        .executor_frames(args.id)
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&ExecutorFramesResult { executors })?;
    } else {
        for line in render::frames_lines(&executors) {
            print_line(line)?;
        }
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
    let named_run = matches!(target, CleanTarget::Run { .. });
    let client = env.connect().await?;
    let items = client
        .clean_workspaces(target, args.force)
        .await
        .map_err(|e| env.explain(e))?;
    let refused = named_run && items.iter().any(|i| i.outcome == CleanOutcome::Kept);
    if env.json {
        print_json(&WorkspaceCleanResult { items })?;
    } else if items.is_empty() {
        print_line("nothing to clean")?;
    } else {
        for item in &items {
            print_line(render::clean_line(item))?;
        }
    }
    // Asking for one run and being refused is a failure a script must see; a sweep that skips
    // some workspaces is best-effort and succeeds.
    Ok(if refused {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
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
    message: Option<String>,
    decision: ApprovalDecision,
) -> anyhow::Result<ExitCode> {
    if message.as_deref().is_some_and(|m| m.trim().is_empty()) {
        anyhow::bail!("--message is empty: say why, or leave it out");
    }
    let client = env.connect().await?;
    if message.is_some() && !client.supports(reno_proto::FEATURE_APPROVAL_MESSAGE) {
        anyhow::bail!(
            "this daemon cannot take a reason with a denial (it lacks the `{}` feature): \
             update renod, or deny without --message",
            reno_proto::FEATURE_APPROVAL_MESSAGE
        );
    }
    let remember = args.remember.map(|scope| reno_proto::Remember {
        scope: match scope {
            crate::cli::RememberArg::Run => reno_proto::RememberScope::Run,
            crate::cli::RememberArg::Bot => reno_proto::RememberScope::Bot,
            crate::cli::RememberArg::Project => reno_proto::RememberScope::Project,
            crate::cli::RememberArg::Always => reno_proto::RememberScope::Always,
        },
        expires_in_secs: args.expires_in,
    });
    client
        .decide_remembering(&args.approval, decision, message, remember)
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
