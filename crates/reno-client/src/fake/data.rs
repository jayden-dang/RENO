//! The fake daemon's demo data, in one place. It mirrors the locked UI prototype: five bots,
//! three fictional repositories, two held approvals, and runs in several states.
//!
//! The data model has no bot or repository field on a run, so each run's `title` reads
//! `"<bot> · <repo>: <what>"`, the way the prototype shows it.
//!
//! Times are logical: [`NOW_MS`] is "now" in the demo, and every offset is a constant. Nothing
//! here reads a clock.

use reno_proto::defs::BotPin;
use reno_proto::{
    ActionClass, Approval, ApprovalId, ApprovalSubject, ExecEvent, IdError, RunId, RunStatus,
    RunSummary, SessionRef, StopReason, TaskId, ToolCall,
};
use serde_json::json;

/// The demo's "now": 2026-11-15 12:00 UTC, in milliseconds since the Unix epoch.
pub(super) const NOW_MS: u64 = 1_794_744_000_000;
pub(super) const MINUTE: u64 = 60_000;
pub(super) const HOUR: u64 = 60 * MINUTE;

/// The bots of the demo team.
pub const BOTS: [&str; 5] = ["keeper", "scout", "builder", "reviewer", "architect"];

/// The secrets some demo bots ask for, as `(bot, secret)`. None is set until `secret.set`.
pub(super) const BOT_SECRETS: [(&str, &str); 2] =
    [("builder", "GITHUB_TOKEN"), ("keeper", "GITHUB_TOKEN")];

/// The executors the router takes when the task names neither an executor nor a bot.
pub(super) const DEFAULT_EXECUTORS: [&str; 2] = ["claude-code", "codex"];

/// A demo bot's executors, most preferred first. `None` for a name that is not a demo bot.
pub(super) fn bot_preference(bot: &str) -> Option<&'static [&'static str]> {
    Some(match bot {
        "keeper" | "architect" => &["claude-code", "codex"],
        "scout" | "reviewer" => &["codex", "claude-code"],
        "builder" => &["acp.grok", "claude-code", "codex"],
        _ => return None,
    })
}
/// The fictional repositories.
pub const REPOS: [&str; 3] = ["atlas-api", "web-console", "reno"];

/// The held push of keeper's dependency run.
pub const KEEPER_PUSH: &str = "git push -u origin reno/keeper/deps-1115";
/// The URL scout's held network fetch asks for.
pub const SCOUT_FETCH_URL: &str = "https://advisories.example.org/feeds/web-console.json";

/// Model every demo run reports.
pub(super) const MODEL: &str = "opus-5.5";

/// One seeded run and its journal so far.
pub(super) struct SeedRun {
    pub(super) summary: RunSummary,
    pub(super) journal: Vec<ExecEvent>,
}

/// Everything the fake daemon starts with.
pub(super) struct Seed {
    pub(super) runs: Vec<SeedRun>,
    pub(super) approvals: Vec<Approval>,
    pub(super) extras: super::extras::Extras,
}

pub(super) type Ids<T> = Result<T, IdError>;

pub(super) fn summary(
    name: &str,
    exec: &str,
    status: RunStatus,
    stop: Option<StopReason>,
    title: &str,
    age: u64,
    last_change_age: u64,
) -> Ids<RunSummary> {
    Ok(RunSummary {
        model_options: Default::default(),
        effort: None,
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
        bot: title.split_once(" \u{b7} ").map(|(bot, _)| BotPin {
            name: bot.to_owned(),
            version: 1,
        }),
        // "keeper · atlas-api: bump …" was submitted in the demo repository `/demo/atlas-api`.
        cwd: title
            .split_once(" \u{b7} ")
            .and_then(|(_, rest)| rest.split_once(':'))
            .map(|(repo, _)| std::path::PathBuf::from(format!("/demo/{repo}"))),
        created_at_ms: NOW_MS - age,
        updated_at_ms: NOW_MS - last_change_age,
        stop: None,
        not_resumed: None,
        session: None,
        report: None,
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
    let class = match name {
        "bash" | "exec" => ActionClass::Command,
        "fetch" | "web_fetch" => ActionClass::Network,
        "read" | "read_file" => ActionClass::Read,
        "edit" | "write" | "edit_file" => ActionClass::Edit,
        _ => ActionClass::Other,
    };
    ExecEvent::Tool {
        id: id.to_owned(),
        call: ToolCall::new(name, input, class),
    }
}

/// A tool result that carries the text the tool printed.
pub(super) fn tool_output(id: &str, ok: bool, summary: &str, text: &str) -> ExecEvent {
    ExecEvent::ToolResult {
        id: id.to_owned(),
        ok,
        summary: Some(summary.to_owned()),
        output: Some(reno_proto::ToolOutput {
            text: text.to_owned(),
            bytes: text.len() as u64,
            truncated: false,
        }),
    }
}

/// The listing `ls` prints in the demo repository.
pub(super) const LS_OUTPUT: &str = "Cargo.toml\nCargo.lock\ncrates\ndocs\napps\n";

/// A failing `cargo test` whose output was longer than the journal keeps: the head and the tail
/// are there, with the marker the daemon joins them with.
fn failing_test_output() -> ExecEvent {
    let mut text = String::from("running 48 tests\n");
    for i in 0..18 {
        text.push_str(&format!("test lease::tests::case_{i:02} ... ok\n"));
    }
    text.push_str("\n[... 476 KiB cut from the middle ...]\n\n");
    text.push_str(
        "test lease::tests::expires_after_renewals ... FAILED\n\n\
failures:\n\n\
---- lease::tests::expires_after_renewals stdout ----\n\
thread 'lease::tests::expires_after_renewals' panicked at crates/reno-kernel/src/lease.rs:48:9:\n\
assertion `left == right` failed\n  left: Active\n right: Expired\n\n\
failures:\n    lease::tests::expires_after_renewals\n\n\
test result: FAILED. 47 passed; 1 failed; 0 ignored\n",
    );
    ExecEvent::ToolResult {
        id: "t2".to_owned(),
        ok: false,
        summary: Some("1 test failed".to_owned()),
        output: Some(reno_proto::ToolOutput {
            text,
            bytes: 493_568,
            truncated: true,
        }),
    }
}

pub(super) fn tool_ok(id: &str, summary: &str) -> ExecEvent {
    ExecEvent::ToolResult {
        id: id.to_owned(),
        ok: true,
        summary: Some(summary.to_owned()),
        output: None,
    }
}

pub(super) fn done(stop: StopReason, result: Option<&str>, diagnostic: Option<&str>) -> ExecEvent {
    done_because(stop, result, diagnostic, None)
}

/// Like [`done`], with the typed reason the daemon gives.
pub(super) fn done_because(
    stop: StopReason,
    result: Option<&str>,
    diagnostic: Option<&str>,
    reason: Option<reno_proto::StopDetail>,
) -> ExecEvent {
    ExecEvent::Done {
        stop,
        result: result.map(str::to_owned),
        diagnostic: diagnostic.map(str::to_owned),
        reason,
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

/// An empty world: no runs, no approvals, no triggers, no definitions, no history.
pub(super) fn empty_seed() -> Seed {
    Seed {
        runs: vec![],
        approvals: vec![],
        extras: super::extras::Extras::default(),
    }
}

/// The demo world at startup.
/// A run whose agent asked a question and waits for the answer.
pub(super) fn question_run() -> Ids<SeedRun> {
    Ok(SeedRun {
        summary: summary(
            "asker1",
            "claude-code",
            RunStatus::WaitingInput,
            None,
            "builder \u{b7} web-console: pick the retry policy",
            40 * MINUTE,
            3 * MINUTE,
        )?,
        journal: vec![
            started(),
            text("Two retry policies fit the webhook trigger. I need you to choose."),
            ExecEvent::NeedsInput {
                request_id: "req-asker-policy".to_owned(),
                questions: vec![reno_proto::Question {
                    id: "policy".to_owned(),
                    prompt: "Which retry policy should the webhook trigger use?".to_owned(),
                    options: vec![
                        "Bounded retry with jitter".to_owned(),
                        "Fail fast and alert".to_owned(),
                    ],
                    multi_select: false,
                }],
            },
        ],
    })
}

/// A run on an ACP agent (Grok) that asked a pick-one question: it takes no typed answer.
pub(super) fn grok_question_run() -> Ids<SeedRun> {
    Ok(SeedRun {
        summary: summary(
            "grok1",
            "grok",
            RunStatus::WaitingInput,
            None,
            "scout \u{b7} atlas-api: choose the log format",
            35 * MINUTE,
            2 * MINUTE,
        )?,
        journal: vec![
            started(),
            text("Two log formats are in use. I need you to choose one."),
            ExecEvent::NeedsInput {
                request_id: "req-grok-format".to_owned(),
                questions: vec![reno_proto::Question {
                    id: "format".to_owned(),
                    prompt: "Which log format should the daemon use?".to_owned(),
                    options: vec!["JSON lines".to_owned(), "Plain text".to_owned()],
                    multi_select: false,
                }],
            },
        ],
    })
}

/// A run that asked two questions at once, the second a multi-select: the card pages.
pub(super) fn paged_question_run() -> Ids<SeedRun> {
    Ok(SeedRun {
        summary: summary(
            "pager1",
            "claude-code",
            RunStatus::WaitingInput,
            None,
            "builder \u{b7} web-console: set up the release",
            30 * MINUTE,
            MINUTE,
        )?,
        journal: vec![
            started(),
            text("Before I cut the release I need two answers."),
            ExecEvent::NeedsInput {
                request_id: "req-pager-release".to_owned(),
                questions: vec![
                    reno_proto::Question {
                        id: "channel".to_owned(),
                        prompt: "Which channel should it ship to?".to_owned(),
                        options: vec!["Stable".to_owned(), "Beta".to_owned()],
                        multi_select: false,
                    },
                    reno_proto::Question {
                        id: "targets".to_owned(),
                        prompt: "Which platforms should it build for?".to_owned(),
                        options: vec!["macOS".to_owned(), "Linux".to_owned(), "Windows".to_owned()],
                        multi_select: true,
                    },
                ],
            },
        ],
    })
}

/// What the reviewer's run fetched.
const CACHE_DOC_URL: &str = "https://docs.example/cache-control";

/// The long brief the pagination run was given: over the 320 characters a message shows whole.
const LONG_PROMPT: &str = "The pagination test in atlas-api fails about one run in twenty, and only on CI. Reproduce the flake locally first: run the test in a loop, pin the random seed when it fails, and keep the failing seed in the commit message. Then find the cause rather than adding a retry: look at how pages are cut when two rows share a timestamp, and at whether the cursor survives a concurrent insert. Fix it in the query, add a test that fails before the fix and passes after, and leave the other tests alone.";

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
                // The live demo run reports how full its context is, so the ring shows.
                ExecEvent::ContextUsage {
                    tokens: Some(84_213),
                    window: Some(200_000),
                },
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
                tool_output("t1", true, "apps/, crates/, docs/", LS_OUTPUT),
            ],
        },
        SeedRun {
            // It worked for 3 minutes 12 seconds, start to finish.
            summary: summary(
                "reviewer1",
                "codex",
                RunStatus::Succeeded,
                Some(StopReason::Completed),
                "reviewer · web-console: review PR 214, cache headers",
                26 * HOUR,
                26 * HOUR - (3 * MINUTE + 12_000),
            )?,
            journal: vec![
                started(),
                text("I will read the change before judging it."),
                tool("r1", "Read", json!({ "file_path": "src/cache.rs" })),
                tool_ok("r1", "42 lines"),
                tool("r2", "bash", json!({ "command": "cargo test -p cache" })),
                tool_ok("r2", "7 passed"),
                tool("r3", "web_fetch", json!({ "url": CACHE_DOC_URL })),
                tool_ok("r3", "200"),
                text("The headers are right."),
                tool("r5", "edit", json!({ "file_path": "src/cache.rs" })),
                tool_ok("r5", "ok"),
                tool("r6", "edit", json!({ "file_path": "src/headers.rs" })),
                tool_ok("r6", "ok"),
                text("The cache-control change is correct; I named the max-age constant."),
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
            summary: RunSummary {
                prompt: Some(LONG_PROMPT.to_owned()),
                ..summary(
                    "builder0",
                    "claude-code",
                    RunStatus::Failed,
                    Some(StopReason::Error),
                    "builder · atlas-api: fix the flaky pagination test",
                    5 * HOUR,
                    4 * HOUR,
                )?
            },
            journal: vec![
                started(),
                text("Running the pagination test in a loop to reproduce the flake."),
                // Asked and never answered: the question closes with the time limit.
                ExecEvent::NeedsInput {
                    request_id: "req-builder0-seed".to_owned(),
                    questions: vec![reno_proto::Question {
                        id: "seed".to_owned(),
                        prompt: "Pin the random seed for the loop?".to_owned(),
                        options: vec!["Pin it".to_owned(), "Leave it".to_owned()],
                        multi_select: false,
                    }],
                },
                ExecEvent::InputExpired {
                    request_id: "req-builder0-seed".to_owned(),
                },
                ExecEvent::NeedsInput {
                    request_id: "req-builder0-loop".to_owned(),
                    questions: vec![reno_proto::Question {
                        id: "loop".to_owned(),
                        prompt: "How many times should the loop run?".to_owned(),
                        options: vec!["20 times".to_owned(), "100 times".to_owned()],
                        multi_select: false,
                    }],
                },
                tool("t1", "AskUserQuestion", json!({})),
                tool_ok("t1", "User answered: \"100 times\""),
                tool(
                    "t2",
                    "bash",
                    json!({ "command": "cargo test -p reno-kernel" }),
                ),
                failing_test_output(),
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
        extras: super::extras::seed()?,
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
    /// Work, then stay live until the run is cancelled or steered; a steer ends it in success.
    /// Prompt: `/long`. For tests that need a run that is still going when they act on it.
    Long,
    /// Fail at once, as the real daemon does for an executor that is not usable (not installed,
    /// too old, signed out): a `NoEligibleExecutor` diagnostic and nothing run. Prompt:
    /// `/refused`. For tests of what the Run screen says about the executor.
    Refused,
    /// Work, then fail after the harness reported a session: the run `run.resume` can take back.
    /// Prompt: `/fails`.
    Fails,
    /// Stream one very long reply (headings, code blocks, math) in many small steps at the
    /// fake's pace, then finish with that reply as the final message. Prompt: `/longreply`.
    LongReply,
}

impl Scenario {
    pub(super) fn of(prompt: &str) -> Self {
        let lower = prompt.to_lowercase();
        // Before `/long`, which it starts with.
        if lower.starts_with("/longreply") {
            return Self::LongReply;
        }
        if lower.starts_with("/refused") {
            return Self::Refused;
        }
        if lower.starts_with("/fails") {
            return Self::Fails;
        }
        if lower.starts_with("/long") {
            return Self::Long;
        }
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

/// ROAD-35: the busy Claude Code run (`plan1`) the plan, background-task and subagent
/// variants open on, and an empty one (`plan0`) where none of that shows. The plan, tasks and
/// agents themselves are [`crate::activity::RunActivity`], client-side only.
pub(super) fn activity_runs() -> Ids<Vec<SeedRun>> {
    Ok(vec![
        SeedRun {
            summary: summary(
                "plan1",
                "claude-code",
                RunStatus::Running,
                None,
                "builder \u{b7} reno: fix the ACP v2 handshake",
                14 * MINUTE,
                30_000,
            )?,
            journal: vec![
                started(),
                text(
                    "The ACP handshake test fails on a version mismatch. I will read the handshake code and the failing test first, and look up what the v2 spec changes while I work.",
                ),
                tool(
                    "p1",
                    "read",
                    json!({ "file_path": "crates/reno-harness/src/acp/handshake.rs" }),
                ),
                tool_ok("p1", "212 lines"),
                text(
                    "The client sends protocol version 1 and never reads the agent's counter-offer. I wrote the negotiation fix: it takes the lower of the two versions and fails with a named error when they share none.",
                ),
                tool(
                    "p2",
                    "edit",
                    json!({ "file_path": "crates/reno-harness/src/acp/handshake.rs" }),
                ),
                tool_ok("p2", "ok"),
                text(
                    "Starting the dev server so I can try the client by hand, then running the whole test suite.",
                ),
                tool("p3", "bash", json!({ "command": "npm run dev" })),
                tool_ok("p3", "listening on :5173"),
                tool("p4", "bash", json!({ "command": "cargo test --workspace" })),
            ],
        },
        SeedRun {
            summary: summary(
                "plan0",
                "claude-code",
                RunStatus::Running,
                None,
                "builder \u{b7} reno: rename the settings tab",
                3 * MINUTE,
                20_000,
            )?,
            journal: vec![
                started(),
                text(
                    "Renaming the Settings tab label in the sidebar footer; reading where it is defined.",
                ),
                tool(
                    "q1",
                    "read",
                    json!({ "file_path": "apps/reno-app/src/ui/sidebar.rs" }),
                ),
            ],
        },
    ])
}

/// One finished run of the sidebar demo: it worked, said so, and ended.
fn finished_run(name: &str, title: &str, age: u64, ok: bool) -> Ids<SeedRun> {
    let (status, stop) = if ok {
        (RunStatus::Succeeded, StopReason::Completed)
    } else {
        (RunStatus::Failed, StopReason::Error)
    };
    Ok(SeedRun {
        summary: summary(
            name,
            "claude-code",
            status,
            Some(stop),
            title,
            age,
            age - 20 * MINUTE,
        )?,
        journal: vec![
            started(),
            text("I read the code around this and made the change."),
            done(
                stop,
                ok.then_some("Done."),
                (!ok).then_some("the test still fails"),
            ),
        ],
    })
}

/// ROAD-32: the sidebar's demo threads. `atlas-api` gets eleven more (fourteen in all, none
/// pinned or archived, so "Show 6 more" reads the same in every variant), and `reno` and
/// `web-console` get two archived ones each (see [`crate::thread_desk`]).
pub(super) fn sidebar_runs() -> Ids<Vec<SeedRun>> {
    const DAY: u64 = 24 * HOUR;
    let atlas = [
        "builder \u{b7} atlas-api: add cursor pagination to /v1/events",
        "scout \u{b7} atlas-api: audit the rate limiter",
        "builder \u{b7} atlas-api: split the billing module",
        "reviewer \u{b7} atlas-api: review PR 188, retry middleware",
        "builder \u{b7} atlas-api: move config to typed settings",
        "architect \u{b7} atlas-api: sketch the webhook fan-out",
        "builder \u{b7} atlas-api: drop the legacy v0 routes",
        "scout \u{b7} atlas-api: map the slow queries",
        "builder \u{b7} atlas-api: add request ids to the logs",
        "keeper \u{b7} atlas-api: refresh the lockfile",
        "builder \u{b7} atlas-api: document the error codes",
    ];
    let archived = [
        "builder \u{b7} reno: trim the CI cache keys",
        "reviewer \u{b7} reno: review PR 201, sandbox flags",
        "builder \u{b7} web-console: remove the unused banner",
        "scout \u{b7} web-console: check the bundle size",
    ];
    let mut runs = Vec::new();
    for (n, title) in atlas.iter().enumerate() {
        let age = (n as u64 + 2) * DAY;
        runs.push(finished_run(
            &format!("atlas{:02}", n + 1),
            title,
            age,
            n != 6,
        )?);
    }
    for (n, title) in archived.iter().enumerate() {
        runs.push(finished_run(
            &format!("arch{}", n + 1),
            title,
            (n as u64 + 3) * DAY,
            true,
        )?);
    }
    Ok(runs)
}

#[cfg(test)]
mod tests {
    use super::Scenario;

    #[test]
    fn the_scripts_keep_their_prompts_and_longreply_is_not_long() {
        assert_eq!(Scenario::of("/long"), Scenario::Long);
        assert_eq!(Scenario::of("/longreply"), Scenario::LongReply);
        assert_eq!(Scenario::of("/LongReply now"), Scenario::LongReply);
        assert_eq!(Scenario::of("/burst 5"), Scenario::Burst(5));
        assert_eq!(Scenario::of("/refused"), Scenario::Refused);
        assert_eq!(Scenario::of("/fails"), Scenario::Fails);
        assert_eq!(Scenario::of("do it"), Scenario::Plain);
    }
}
