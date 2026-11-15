//! The fake daemon's demo triggers, definitions and approval history.
//!
//! They mirror the locked prototype: four triggers (one backing off), five bots, one policy, and
//! a short history of decided approvals. Times are logical, like the rest of the demo data.

use reno_proto::defs::{DefKind, DefRecord, DefSummary, DefsSnapshot};
use reno_proto::{
    ApprovalId, ApprovalRecord, ApprovalStatus, IdError, ResolvedBy, RunId, TriggerInfo,
    TriggerStatus,
};
use serde_json::{Value, json};

use super::data::{self, NOW_MS};

const MINUTE: u64 = 60_000;
const HOUR: u64 = 60 * MINUTE;

/// Everything beyond runs and pending approvals that the fake serves.
#[derive(Default)]
pub(super) struct Extras {
    pub(super) triggers: Vec<TriggerInfo>,
    pub(super) defs: DefsSnapshot,
    pub(super) records: Vec<DefRecord>,
    pub(super) history: Vec<ApprovalRecord>,
}

/// The bot of each seeded trigger and the prompt it fires.
pub(super) fn trigger_target(name: &str) -> Option<(&'static str, &'static str)> {
    match name {
        "nightly-deps" => Some(("keeper", "Check for outdated dependencies in atlas-api.")),
        "scout-digest" => Some(("scout", "Write the advisory digest for web-console.")),
        "pr-review" => Some(("reviewer", "Review the changed files.")),
        "hotfix-check" => Some(("builder", "Verify the hotfix branch builds.")),
        _ => None,
    }
}

fn hash(n: u64) -> String {
    format!("{n:064x}")
}

fn record(kind: DefKind, name: &str, body: Value, n: u64) -> (DefSummary, DefRecord) {
    let summary = DefSummary {
        kind,
        name: name.to_owned(),
        version: 1,
        hash: hash(n),
        created_at_ms: NOW_MS - 48 * HOUR,
    };
    let full = DefRecord {
        kind,
        name: name.to_owned(),
        version: 1,
        hash: hash(n),
        body,
        created_at_ms: NOW_MS - 48 * HOUR,
        latest: true,
    };
    (summary, full)
}

fn bot_body(name: &str, exec: &str, access: &str, team: &str, triggers: &[&str]) -> Value {
    json!({
        "name": name,
        "instructions": format!("You are {name}. Work carefully and report what you did."),
        "executors": [exec],
        "access": access,
        "sandbox": "workspace_write",
        "team": team,
        "triggers": triggers,
        "budget": { "max_concurrent_runs": 1, "max_wall_time": "45m" },
    })
}

fn trigger_body(name: &str, when: &Value, enabled: bool) -> Value {
    let (bot, prompt) = trigger_target(name).unwrap_or(("keeper", ""));
    json!({
        "name": name,
        "enabled": enabled,
        "when": when,
        "target": { "bot": bot, "prompt": prompt },
    })
}

fn info(
    name: &str,
    kind: &str,
    status: TriggerStatus,
    due: Option<u64>,
    last: Option<(u64, &str)>,
) -> Result<TriggerInfo, IdError> {
    let (bot, _) = trigger_target(name).unwrap_or(("keeper", ""));
    Ok(TriggerInfo {
        name: name.to_owned(),
        kind: kind.to_owned(),
        bot: bot.to_owned(),
        version: 1,
        status,
        next_due_at_ms: due,
        last_fired_at_ms: last.map(|(at, _)| at),
        last_run_id: last.map(|(_, run)| RunId::from_suffix(run)).transpose()?,
        failures: 0,
        backoff_until_ms: None,
        error: None,
    })
}

fn decided(
    id: &str,
    run: &str,
    subject: reno_proto::ApprovalSubject,
    status: ApprovalStatus,
    by: ResolvedBy,
    age: u64,
) -> Result<ApprovalRecord, IdError> {
    Ok(ApprovalRecord {
        id: ApprovalId::from_suffix(id)?,
        run_id: RunId::from_suffix(run)?,
        request_id: format!("req-{id}"),
        subject,
        status,
        decided_by: Some(by),
        created_at_ms: NOW_MS - age - 4 * MINUTE,
        decided_at_ms: Some(NOW_MS - age),
        expires_at_ms: NOW_MS - age + 26 * MINUTE,
    })
}

pub(super) fn seed() -> Result<Extras, IdError> {
    let mut triggers = vec![
        info(
            "nightly-deps",
            "cron",
            TriggerStatus::Scheduled,
            Some(NOW_MS + 18 * HOUR),
            Some((NOW_MS - 6 * HOUR, "keeper1")),
        )?,
        info(
            "scout-digest",
            "every",
            TriggerStatus::BackedOff,
            Some(NOW_MS + 40 * MINUTE),
            Some((NOW_MS - 3 * HOUR, "scout1")),
        )?,
        info("pr-review", "fs", TriggerStatus::Watching, None, None)?,
        info(
            "hotfix-check",
            "manual",
            TriggerStatus::Manual,
            None,
            Some((NOW_MS - 26 * HOUR, "builder0")),
        )?,
    ];
    if let Some(t) = triggers.get_mut(1) {
        t.failures = 3;
        t.backoff_until_ms = Some(NOW_MS + 40 * MINUTE);
    }

    let bots = [
        ("keeper", "claude-code", "supervised", &["nightly-deps"][..]),
        ("scout", "codex", "supervised", &["scout-digest"][..]),
        (
            "builder",
            "claude-code",
            "auto_accept_edits",
            &["hotfix-check"][..],
        ),
        ("reviewer", "codex", "supervised", &["pr-review"][..]),
        ("architect", "claude-code", "supervised", &[][..]),
    ];
    let mut summaries = Vec::new();
    let mut records = Vec::new();
    let mut n = 1;
    for (name, exec, access, trigs) in bots {
        let (s, r) = record(
            DefKind::Bot,
            name,
            bot_body(name, exec, access, "core", trigs),
            n,
        );
        n += 1;
        summaries.push(s);
        records.push(r);
    }
    let whens = [
        (
            "nightly-deps",
            json!({ "kind": "cron", "cron": "0 6 * * *", "timezone": "UTC" }),
        ),
        ("scout-digest", json!({ "kind": "every", "every": "6h" })),
        (
            "pr-review",
            json!({ "kind": "fs", "paths": ["~/code/web-console/src"], "debounce": "500ms" }),
        ),
        ("hotfix-check", json!({ "kind": "manual" })),
    ];
    for (name, when) in &whens {
        let (s, r) = record(DefKind::Trigger, name, trigger_body(name, when, true), n);
        n += 1;
        summaries.push(s);
        records.push(r);
    }
    let policy = json!({
        "default": "ask",
        "approval_ttl": "30m",
        "rules": [
            { "note": "never reset hard", "effect": "deny",
              "match": { "action": "command", "command": "git reset --hard*" } },
            { "note": "cargo is fine for builders", "effect": "allow",
              "match": { "action": "command", "command": "cargo *", "scope": ["builder", "keeper"] } },
            { "note": "reads are free", "effect": "allow", "match": { "action": "read" } },
        ],
    });
    let (s, r) = record(DefKind::Policy, "policy", policy, n);
    summaries.push(s);
    records.push(r);

    let history = vec![
        decided(
            "hist3",
            "builder0",
            data::command_subject("cargo test --workspace"),
            ApprovalStatus::Approved,
            ResolvedBy::Rule {
                name: "cargo is fine for builders".to_owned(),
            },
            4 * HOUR,
        )?,
        decided(
            "hist2",
            "reviewer1",
            data::command_subject("git reset --hard origin/main"),
            ApprovalStatus::Denied,
            ResolvedBy::Rule {
                name: "never reset hard".to_owned(),
            },
            25 * HOUR,
        )?,
        decided(
            "hist1",
            "reviewer1",
            data::network_subject("https://registry.example.org/index.json"),
            ApprovalStatus::Approved,
            ResolvedBy::Human,
            26 * HOUR,
        )?,
    ];
    Ok(Extras {
        triggers,
        defs: DefsSnapshot {
            defs: summaries,
            errors: vec![],
        },
        records,
        history,
    })
}
