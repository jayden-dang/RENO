//! `reno task run`, `reno run ...`, `reno approval list`, `reno approval history`,
//! `reno approve` and `reno deny`.

use std::io;
use std::process::ExitCode;

use anyhow::{Context, bail};
use futures::StreamExt;
use reno_client::Client;
use reno_proto::{
    ApprovalDecision, ApprovalHistoryResult, ApprovalListResult, ExecEvent, RUN_LIST_MAX_LIMIT,
    RunCancelResult, RunId, RunListParams, RunListResult, Seq, StopReason, TaskRunParams,
};
use serde::Serialize;

use crate::cli::{
    ApprovalCommand, ApprovalHistory, Decide, RunCancel, RunCommand, RunList, RunTail, TaskCommand,
    TaskRun,
};
use crate::env::{Env, print_json, print_line};
use crate::render::{self, Finished, Transcript};

pub(crate) async fn task(env: &Env, command: TaskCommand) -> anyhow::Result<ExitCode> {
    match command {
        TaskCommand::Run(args) => task_run(env, args).await,
    }
}

pub(crate) async fn run(env: &Env, command: RunCommand) -> anyhow::Result<ExitCode> {
    match command {
        RunCommand::List(args) => run_list(env, args).await,
        RunCommand::Tail(args) => run_tail(env, args).await,
        RunCommand::Cancel(args) => run_cancel(env, args).await,
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
}

async fn task_run(env: &Env, args: TaskRun) -> anyhow::Result<ExitCode> {
    // Without a bot the executor defaults to the mock. With one, the daemon picks the bot's
    // first executor that it has.
    let executor = match (&args.executor, &args.bot) {
        (Some(name), _) => Some(name.as_str()),
        (None, None) => Some("mock"),
        (None, Some(_)) => None,
    }
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
    let run = client
        .run_task(TaskRunParams {
            prompt: args.prompt,
            executor,
            bot: args.bot,
            cwd: Some(cwd),
            model: args.model,
        })
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&Submitted { run_id: &run })?;
    }
    follow(env, &client, run, None).await
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
