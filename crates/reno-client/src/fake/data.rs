//! The fake daemon's demo data, in one place. It mirrors the locked UI prototype: five bots,
//! three fictional repositories, two held approvals, and runs in several states.
//!
//! The data model has no bot or repository field on a run, so each run's `title` reads
//! `"<bot> · <repo>: <what>"`, the way the prototype shows it.
//!
//! Times are logical: [`NOW_MS`] is "now" in the demo, and every offset is a constant. Nothing
//! here reads a clock.

use reno_proto::{
    ActionClass, Approval, ApprovalId, ApprovalSubject, ExecEvent, IdError, RunId, RunStatus,
    RunSummary, SessionRef, StopReason, TaskId, ToolCall,
};
use serde_json::json;

/// The demo's "now": 2026-11-15 12:00 UTC, in milliseconds since the Unix epoch.
pub(super) const NOW_MS: u64 = 1_794_744_000_000;
const MINUTE: u64 = 60_000;
const HOUR: u64 = 60 * MINUTE;

/// The bots of the demo team.
pub const BOTS: [&str; 5] = ["keeper", "scout", "builder", "reviewer", "architect"];
/// The fictional repositories.
pub const REPOS: [&str; 3] = ["atlas-api", "web-console", "reno"];

/// The held push of keeper's dependency run.
pub const KEEPER_PUSH: &str = "git push -u origin reno/keeper/deps-1115";
/// The URL scout's held network fetch asks for.
pub const SCOUT_FETCH_URL: &str = "https://advisories.example.org/feeds/web-console.json";

/// Model every demo run reports.
const MODEL: &str = "opus-5.5";

/// One seeded run and its journal so far.
pub(super) struct SeedRun {
    pub(super) summary: RunSummary,
    pub(super) journal: Vec<ExecEvent>,
}

/// Everything the fake daemon starts with.
pub(super) struct Seed {
    pub(super) runs: Vec<SeedRun>,
    pub(super) approvals: Vec<Approval>,
}

type Ids<T> = Result<T, IdError>;

fn summary(
    name: &str,
    exec: &str,
    status: RunStatus,
    stop: Option<StopReason>,
    title: &str,
    age: u64,
    last_change_age: u64,
) -> Ids<RunSummary> {
    Ok(RunSummary {
        run_id: RunId::from_suffix(name)?,
        task_id: TaskId::from_suffix(name)?,
        executor: exec.parse()?,
        status,
        attempt: 1,
        title: title.to_owned(),
        // Seeded runs have no stored prompt, as if from an older daemon: the app falls back to
        // the title.
        prompt: None,
        model: Some(MODEL.to_owned()),
        stop_reason: stop,
        created_at_ms: NOW_MS - age,
        updated_at_ms: NOW_MS - last_change_age,
    })
}

/// The start of every transcript.
pub(super) fn started() -> ExecEvent {
    ExecEvent::Started {
        session: SessionRef("sess-fake".to_owned()),
        model: MODEL.to_owned(),
    }
}

pub(super) fn text(s: &str) -> ExecEvent {
    ExecEvent::Text { text: s.to_owned() }
}

pub(super) fn tool(id: &str, name: &str, input: serde_json::Value) -> ExecEvent {
    ExecEvent::Tool {
        id: id.to_owned(),
        call: ToolCall {
            name: name.to_owned(),
            input,
        },
    }
}

pub(super) fn tool_ok(id: &str, summary: &str) -> ExecEvent {
    ExecEvent::ToolResult {
        id: id.to_owned(),
        ok: true,
        summary: Some(summary.to_owned()),
    }
}

pub(super) fn done(stop: StopReason, result: Option<&str>, diagnostic: Option<&str>) -> ExecEvent {
    ExecEvent::Done {
        stop,
        result: result.map(str::to_owned),
        diagnostic: diagnostic.map(str::to_owned),
    }
}

pub(super) fn command_subject(cmd: &str) -> ApprovalSubject {
    ApprovalSubject {
        action: ActionClass::Command,
        tool: "bash".to_owned(),
        command: Some(cmd.to_owned()),
        paths: vec![],
        raw: json!({ "command": cmd }),
    }
}

pub(super) fn network_subject(url: &str) -> ApprovalSubject {
    ApprovalSubject {
        action: ActionClass::Network,
        tool: "web_fetch".to_owned(),
        command: None,
        paths: vec![],
        raw: json!({ "url": url }),
    }
}

fn approval(name: &str, run: &str, subject: ApprovalSubject, age: u64) -> Ids<Approval> {
    Ok(Approval {
        id: ApprovalId::from_suffix(name)?,
        run_id: RunId::from_suffix(run)?,
        subject,
        requested_at_ms: NOW_MS - age,
    })
}

/// The two held actions of the prototype's Home screen.
fn approvals() -> Ids<Vec<Approval>> {
    Ok(vec![
        approval(
            "keeperpush",
            "keeper1",
            command_subject(KEEPER_PUSH),
            6 * HOUR,
        )?,
        approval(
            "scoutfetch",
            "scout1",
            network_subject(SCOUT_FETCH_URL),
            2 * HOUR,
        )?,
    ])
}

/// The demo world at startup.
pub(super) fn seed() -> Ids<Seed> {
    let ls = || tool("t1", "bash", json!({ "command": "ls" }));
    let runs = vec![
        SeedRun {
            summary: summary(
                "keeper1",
                "claude-code",
                RunStatus::WaitingApproval,
                None,
                "keeper · atlas-api: bump minor dependencies",
                7 * HOUR,
                6 * HOUR,
            )?,
            journal: vec![
                started(),
                text("Three crates have minor updates. I will bump them and open a branch."),
                ls(),
                tool_ok("t1", "Cargo.toml, Cargo.lock, src/"),
                tool("t2", "bash", json!({ "command": KEEPER_PUSH })),
                ExecEvent::NeedsApproval {
                    request_id: "req-keeper-push".to_owned(),
                    subject: command_subject(KEEPER_PUSH),
                },
            ],
        },
        SeedRun {
            summary: summary(
                "scout1",
                "codex",
                RunStatus::WaitingApproval,
                None,
                "scout · web-console: check upstream advisories",
                3 * HOUR,
                2 * HOUR,
            )?,
            journal: vec![
                started(),
                text("I need the advisory feed to compare against the lockfile."),
                tool("t1", "web_fetch", json!({ "url": SCOUT_FETCH_URL })),
                ExecEvent::NeedsApproval {
                    request_id: "req-scout-fetch".to_owned(),
                    subject: network_subject(SCOUT_FETCH_URL),
                },
            ],
        },
        SeedRun {
            summary: summary(
                "builder1",
                "claude-code",
                RunStatus::Running,
                None,
                "builder · reno: add the run inspector to the app shell",
                HOUR,
                5 * MINUTE,
            )?,
            journal: vec![
                started(),
                text("Reading the shell layout before adding the inspector pane."),
                ls(),
                tool_ok("t1", "apps/, crates/, docs/"),
            ],
        },
        SeedRun {
            summary: summary(
                "reviewer1",
                "codex",
                RunStatus::Succeeded,
                Some(StopReason::Completed),
                "reviewer · web-console: review PR 214, cache headers",
                26 * HOUR,
                25 * HOUR,
            )?,
            journal: vec![
                started(),
                text("The cache-control change is correct; one nit on the max-age constant."),
                done(StopReason::Completed, Some("Approved with one nit."), None),
            ],
        },
        SeedRun {
            summary: summary(
                "architect1",
                "claude-code",
                RunStatus::Queued,
                None,
                "architect · reno: draft the scheduler lease design",
                10 * MINUTE,
                10 * MINUTE,
            )?,
            journal: vec![],
        },
        SeedRun {
            summary: summary(
                "builder0",
                "claude-code",
                RunStatus::Failed,
                Some(StopReason::Error),
                "builder · atlas-api: fix the flaky pagination test",
                5 * HOUR,
                4 * HOUR,
            )?,
            journal: vec![
                started(),
                text("Running the pagination test in a loop to reproduce the flake."),
                done(
                    StopReason::Error,
                    None,
                    Some("the test still fails 1 run in 20"),
                ),
            ],
        },
    ];
    Ok(Seed {
        runs,
        approvals: approvals()?,
    })
}

/// What a prompt asks the fake's `task.run` script to do. This is a test hook, not a feature of
/// the real daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Scenario {
    /// Work, then succeed.
    Plain,
    /// Work, then hold on a `git push` for approval.
    HoldPush,
    /// Work, then hold on a network fetch for approval.
    HoldFetch,
    /// Emit this many text events with no delay between them, then succeed. Prompt: `/burst N`.
    Burst(u32),
}

impl Scenario {
    pub(super) fn of(prompt: &str) -> Self {
        let lower = prompt.to_lowercase();
        if let Some(n) = lower.strip_prefix("/burst ") {
            return Self::Burst(n.trim().parse().unwrap_or(100));
        }
        if lower.contains("push") {
            Self::HoldPush
        } else if lower.contains("fetch") || lower.contains("network") {
            Self::HoldFetch
        } else {
            Self::Plain
        }
    }
}
