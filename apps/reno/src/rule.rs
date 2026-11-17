//! `reno rule list|revoke`: the standing rules a person made by answering an approval with
//! `--remember` (or the app's scope choice). A rule holds exactly the action it came from; this
//! is where one is looked at and taken back.

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use reno_proto::{RuleListResult, StandingRule, StandingScope};
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
            let over = args.all && run_is_over(&client, rule, now).await;
            print_line(render::rule_line(rule, now, over))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Whether `rule` belongs to one run and that run has ended: the rule holds no more, though
/// nobody revoked it. Only a rule still otherwise in force needs the look; a run the daemon
/// cannot find (or cannot be asked about) is not reported as over.
async fn run_is_over(client: &reno_client::Client, rule: &StandingRule, now: u64) -> bool {
    let StandingScope::Run { run } = &rule.scope else {
        return false;
    };
    if !rule.is_active(now) {
        return false;
    }
    client
        .get_run(run)
        .await
        .is_ok_and(|run| run.status.is_terminal())
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
