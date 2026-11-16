//! The fake daemon's demo triggers, definitions and approval history.
//!
//! They mirror the locked prototype: four triggers (one backing off), five bots, one policy, and
//! a short history of decided approvals. Times are logical, like the rest of the demo data.

use reno_proto::defs::{DefKind, DefRecord, DefSummary, DefsSnapshot};
use reno_proto::{
    AccessMode, ApprovalId, ApprovalRecord, ApprovalStatus, ExecutorCaps, ExecutorDescriptor,
    ExecutorHealth, ExecutorInfo, IdError, InstallStatus, ModelCatalog, ModelInfo,
    NotInstalledReason, ResolvedBy, RunId, SandboxLevel, SignedIn, Steering, TriggerInfo,
    TriggerStatus, VersionGate, VersionRange,
};
use serde_json::{Value, json};

use super::data::{self, NOW_MS};

const MINUTE: u64 = 60_000;
const HOUR: u64 = 60 * MINUTE;

/// Everything beyond runs and pending approvals that the fake serves.
#[derive(Default)]
pub(super) struct Extras {
    pub(super) executors: Vec<ExecutorInfo>,
    pub(super) triggers: Vec<TriggerInfo>,
    pub(super) defs: DefsSnapshot,
    pub(super) records: Vec<DefRecord>,
    pub(super) history: Vec<ApprovalRecord>,
    pub(super) rules: Vec<reno_proto::StandingRule>,
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
        cwd: (name == "nightly-deps").then(|| "~/code/app".to_owned()),
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
        subject_digest: Some(subject.digest()),
        subject,
        status,
        decided_by: Some(by),
        created_at_ms: NOW_MS - age - 4 * MINUTE,
        decided_at_ms: Some(NOW_MS - age),
        expires_at_ms: NOW_MS - age + 26 * MINUTE,
        message: None,
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
        rules: rules()?,
        executors: executors()?,
        triggers,
        defs: DefsSnapshot {
            defs: summaries,
            errors: vec![],
        },
        records,
        history,
    })
}

/// The demo standing rules: one of each scope and effect, one that expires, one revoked.
fn rules() -> Result<Vec<reno_proto::StandingRule>, IdError> {
    use reno_proto::{RuleId, RuleMatcher, StandingEffect, StandingRule, StandingScope};
    let make = |id: &str,
                scope: StandingScope,
                effect: StandingEffect,
                subject: &reno_proto::ApprovalSubject,
                age: u64,
                expires_in: Option<u64>|
     -> Result<StandingRule, IdError> {
        Ok(StandingRule {
            id: RuleId::from_suffix(id)?,
            scope,
            effect,
            matcher: RuleMatcher::derive(subject, std::path::Path::new("/work")),
            created_by: "human".to_owned(),
            created_at_ms: NOW_MS - age,
            expires_at_ms: expires_in.map(|secs| NOW_MS - age + secs * 1_000),
            revoked_at_ms: None,
            source_approval: Some(ApprovalId::from_suffix("hist3")?),
        })
    };
    Ok(vec![
        make(
            "demo1",
            StandingScope::Bot {
                bot: "builder".to_owned(),
            },
            StandingEffect::Allow,
            &data::command_subject("cargo test --workspace"),
            3 * HOUR,
            None,
        )?,
        make(
            "demo2",
            StandingScope::Always,
            StandingEffect::Deny,
            &data::command_subject("git push --force"),
            26 * HOUR,
            None,
        )?,
        make(
            "demo3",
            StandingScope::Always,
            StandingEffect::Allow,
            &data::network_subject("https://registry.example.org/index.json"),
            2 * HOUR,
            Some(24 * 3_600),
        )?,
    ])
}

/// The demo executors: one healthy, one signed out, one with a newer CLI than the adapter was
/// verified on, one that needs its adapter installed, and the mock.
fn executors() -> Result<Vec<ExecutorInfo>, IdError> {
    let info = |id: &str,
                name: &str,
                access: &[AccessMode],
                sandbox: &[SandboxLevel],
                range: (&str, &str),
                health: ExecutorHealth|
     -> Result<ExecutorInfo, IdError> {
        Ok(ExecutorInfo {
            descriptor: ExecutorDescriptor {
                id: id.parse()?,
                contract_version: 1,
                adapter_version: format!("reno-harness demo, {name}"),
                cli_verified: VersionRange {
                    min: range.0.to_owned(),
                    verified: range.1.to_owned(),
                },
                caps: ExecutorCaps {
                    steering: Steering::StepBoundary,
                    resume: true,
                    access_modes: access.to_vec(),
                    sandbox_levels: sandbox.to_vec(),
                    mcp_injection: true,
                    structured_output: false,
                    deterministic_turn_end: true,
                    plan_mode: false,
                    models: health.models.models.clone(),
                },
            },
            health: Some(health),
        })
    };
    let models = |ids: &[(&str, &str)]| {
        ids.iter()
            .map(|(id, name)| ModelInfo {
                id: (*id).to_owned(),
                display_name: Some((*name).to_owned()),
            })
            .collect::<Vec<_>>()
    };
    let installed = |path: &str, version: &str| InstallStatus::Installed {
        path: path.into(),
        version: Some(version.to_owned()),
    };
    let all = [
        AccessMode::Supervised,
        AccessMode::AutoAcceptEdits,
        AccessMode::FullAccess,
    ];
    let unrestricted = [SandboxLevel::Unrestricted];
    let at = NOW_MS - 5 * MINUTE;
    Ok(vec![
        info(
            "claude-code",
            "Claude Code",
            &all,
            &unrestricted,
            ("2.1.228", "2.1.325"),
            ExecutorHealth {
                installed: installed("/usr/local/bin/claude", "2.1.325"),
                version_gate: VersionGate::Ok,
                signed_in: SignedIn::Yes {
                    account: Some("ada@example.com".to_owned()),
                },
                models: ModelCatalog::curated(models(&[
                    ("claude-opus-5-5", "Opus 5.5"),
                    ("claude-sonnet-5-5", "Sonnet 5.5"),
                ])),
                checked_at_ms: at,
            },
        )?,
        info(
            "codex",
            "Codex",
            &all,
            &[
                SandboxLevel::ReadOnly,
                SandboxLevel::WorkspaceWrite,
                SandboxLevel::Unrestricted,
            ],
            ("0.146.0", "0.169.0"),
            ExecutorHealth {
                installed: installed("/opt/homebrew/bin/codex", "0.171.0"),
                version_gate: VersionGate::Untested {
                    verified: "0.169.0".to_owned(),
                },
                signed_in: SignedIn::No {
                    hint: "run `codex login`".to_owned(),
                },
                models: ModelCatalog::live(models(&[("gpt-6-astra", "GPT-6-Astra")])),
                checked_at_ms: at,
            },
        )?,
        info(
            "acp.grok",
            "Grok",
            &all,
            &unrestricted,
            ("0.9.0", "0.12.0"),
            ExecutorHealth {
                installed: installed("/usr/local/bin/grok", "0.4.0"),
                version_gate: VersionGate::TooOld {
                    min: "0.9.0".to_owned(),
                },
                signed_in: SignedIn::Yes {
                    account: Some("ada@example.com".to_owned()),
                },
                models: ModelCatalog::curated(models(&[("grok-4", "Grok 4")])),
                checked_at_ms: at,
            },
        )?,
        info(
            "acp.hermes",
            "Hermes",
            &all,
            &unrestricted,
            ("0", "unverified"),
            ExecutorHealth {
                installed: InstallStatus::NotInstalled {
                    why: NotInstalledReason::NeedsInstall {
                        what: "the Hermes adapter (npm)".to_owned(),
                    },
                },
                version_gate: VersionGate::Unchecked,
                signed_in: SignedIn::Unknown,
                models: ModelCatalog::curated(vec![]),
                checked_at_ms: at,
            },
        )?,
        info(
            "mock",
            "mock",
            &all,
            &unrestricted,
            ("0", "0"),
            ExecutorHealth {
                installed: installed("mock", "mock"),
                version_gate: VersionGate::Unchecked,
                signed_in: SignedIn::Yes { account: None },
                models: ModelCatalog::curated(models(&[("mock-1", "Mock 1")])),
                checked_at_ms: at,
            },
        )?,
    ])
}
