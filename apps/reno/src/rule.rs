//! `reno rule list|revoke`: the standing rules a person made by answering an approval with
//! `--remember` (or the app's scope choice). A rule holds exactly the action it came from; this
//! is where one is looked at and taken back.

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use reno_proto::RuleListResult;
use serde::Serialize;

use crate::cli::{RuleCommand, RuleList, RuleRevoke};
use crate::env::{Env, print_json, print_line};
use crate::render;

pub(crate) async fn run(env: &Env, command: RuleCommand) -> anyhow::Result<ExitCode> {
    match command {
        RuleCommand::List(args) => list(env, args).await,
        RuleCommand::Revoke(args) => revoke(env, args).await,
    }
}

async fn list(env: &Env, args: RuleList) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let rules = client
        .list_rules(args.all)
        .await
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
