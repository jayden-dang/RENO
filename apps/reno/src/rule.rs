//! `reno rule list|revoke`: the standing rules a person made by answering an approval with
//! `--remember` (or the app's scope choice). A rule holds exactly the action it came from; this
//! is where one is looked at and taken back.

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use reno_proto::RuleListResult;
use serde::Serialize;

use crate::cli::{RuleAdd, RuleCommand, RuleList, RuleRevoke};
use crate::env::{Env, print_json, print_line};
use crate::render;

pub(crate) async fn run(env: &Env, command: RuleCommand) -> anyhow::Result<ExitCode> {
    match command {
        RuleCommand::List(args) => list(env, args).await,
        RuleCommand::Add(args) => add(env, args).await,
        RuleCommand::Revoke(args) => revoke(env, args).await,
    }
}

/// `dir` as the daemon, which runs somewhere else, can read it.
fn absolute(dir: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    Ok(std::path::absolute(dir)?)
}

async fn add(env: &Env, args: RuleAdd) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let rule = client
        .add_rule(reno_proto::RuleAddParams {
            project: absolute(&args.project)?,
            effect: if args.deny {
                reno_proto::StandingEffect::Deny
            } else {
                reno_proto::StandingEffect::Allow
            },
            command: args.command.join(" "),
            tool: args.tool,
            expires_in_secs: args.expires_in,
        })
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&rule)?;
    } else {
        print_line(render::rule_line(&rule, now_ms()))?;
    }
    Ok(ExitCode::SUCCESS)
}

async fn list(env: &Env, args: RuleList) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let rules = match &args.project {
        Some(project) => {
            client
                .list_project_rules(&absolute(project)?, args.all)
                .await
        }
        None => client.list_rules(args.all).await,
    }
    .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&RuleListResult { rules })?;
    } else if rules.is_empty() {
        print_line("no standing rules")?;
    } else {
        let now = now_ms();
        for rule in &rules {
            print_line(render::rule_line(rule, now))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Revoked<'a> {
    id: &'a reno_proto::RuleId,
}

async fn revoke(env: &Env, args: RuleRevoke) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    client
        .revoke_rule(&args.id)
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&Revoked { id: &args.id })?;
    } else {
        print_line(format_args!("revoked {}", args.id))?;
    }
    Ok(ExitCode::SUCCESS)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}
