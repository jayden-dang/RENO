//! The conformance suite on the real Claude Code and Codex CLIs. Opt in, and it costs money:
//!
//! ```text
//! RENO_CONFORMANCE_LIVE=claude-code,codex \
//!   cargo test -p reno-harness --test conformance_live -- --ignored --nocapture
//! ```
//!
//! `RENO_CONFORMANCE_LIVE` names the CLIs to run (`claude-code`, `codex`, or both); each must be
//! installed and signed in (`probe` says so first, and a target that is not usable is reported
//! and fails the run). `auto` runs the ones that are usable and records the others as
//! unavailable, which is what the nightly job (`cargo xtask conformance-live`) uses. The test is
//! `#[ignore]`d, so no plain `cargo test` ever starts a real CLI.
//!
//! `RENO_CONFORMANCE_REPORT=<file>` also writes the run as JSON (what the nightly job reads).
//!
//! What it spends: it prints an estimate of the model runs per CLI before the first one (a
//! tool-using turn, two approvals, a question on Claude, a steer, a cancel, a resume, the mode
//! runs below and an MCP call), at the cheapest model the driver's catalog offers
//! (`claude-code`: the first `haiku`; `codex`: the first `mini`, `luna` or `spark`), each with a
//! tiny prompt in a throwaway git repository. Override the model with
//! `RENO_CONFORMANCE_CLAUDE_MODEL` / `RENO_CONFORMANCE_CODEX_MODEL`. Hard bounds: 120 s per case,
//! 900 s per CLI (`Limits::live`); a case over its bound fails, and cases left when the total is
//! spent are not run.
//!
//! What the runs show, beyond the plain turn:
//!
//! - `Supervised`: the approval cases (a shell command asks; an allow lets it run, a deny leaves
//!   the scratch repository without the file).
//! - `AutoAcceptEdits`: a file edit made with the edit tool is accepted without a question and
//!   lands; a `git tag` (not a file edit, and not a command the CLI waves through in that mode)
//!   asks, and a denial leaves no tag.
//! - Codex `ReadOnly` (with approvals denied): a prompt that asks for a file write leaves the
//!   scratch repository as it was. Codex `WorkspaceWrite`: a write inside the repository lands
//!   and a write to a directory outside it does not. The outside directory is made under
//!   `$HOME`, not under `/tmp`: Codex's workspace-write sandbox leaves `/tmp` and `$TMPDIR`
//!   writable by design, so a write there proves nothing. Set `RENO_CONFORMANCE_OUTSIDE_DIR` to
//!   choose the base directory.
//! - MCP: `conformance-ping-mcp`, a stdio server built with this test (one tool, `ping`, which
//!   writes a marker file), is injected, and the model is asked to call it. The marker is the
//!   proof. It needs no running daemon: Reno's own `reno mcp` talks to `renod`, and what is
//!   under test here is that the CLI receives and uses what the executor injects.
//!
//! What it does not prove, reported as `waived` (an unverified claim, never a pass): a deny
//! message reaching the model (Claude), a harness crash (a real CLI is not made to crash), an
//! unrecognised frame (a real CLI cannot be asked to send one) and, on Codex, a question
//! (`request_user_input` is experimental). Claude Code declares no sandbox level but
//! `Unrestricted`, so it has none to run. The unrecognised-frame counters are still printed, and
//! the clean-path check of that case still runs.
//!
//! The suite never runs the CLIs with more than the access the case asks for, in a scratch
//! repository, but a model is a model: read the prompts below before the first run.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use reno_harness::conformance::{
    ApprovalAnswer, ApprovalPlan, CaseId, Expect, Limits, ModeRun, Plan, Profile, Proof, Scenario,
    Status, Target, TargetReport, render_table, run_suite,
};
use reno_harness::exec::Executor;
use reno_harness::{ClaudeExecutor, CodexExecutor};
use reno_proto::{
    AccessMode, ActionClass, HealthProbe, InstallStatus, McpServerSpec, SandboxLevel, SignedIn,
};
use serde_json::json;

const APPROVAL_FILE: &str = "conformance-approval.txt";
const EDIT_FILE: &str = "conformance-edit.txt";
const TAG_REF: &str = ".git/refs/tags/conformance-auto-shell";
const READ_ONLY_FILE: &str = "conformance-read-only.txt";
const INSIDE_FILE: &str = "conformance-inside.txt";
const OUTSIDE_FILE: &str = "conformance-outside.txt";
const MCP_MARKER: &str = "conformance-mcp.txt";

fn git(dir: &Path, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "conformance")
        .env("GIT_AUTHOR_EMAIL", "conformance@example.invalid")
        .env("GIT_COMMITTER_NAME", "conformance")
        .env("GIT_COMMITTER_EMAIL", "conformance@example.invalid")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed");
}

/// A throwaway repository with one commit.
fn scratch_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    git(dir.path(), &["init", "-q"]);
    std::fs::write(dir.path().join("README.md"), "scratch\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-q", "-m", "scratch"]);
    dir
}

/// An empty directory outside the scratch repository and outside `/tmp` and `$TMPDIR`, removed
/// when it goes out of scope, a failed run included.
struct Outside(PathBuf);

impl Outside {
    fn new() -> Self {
        let base = std::env::var_os("RENO_CONFORMANCE_OUTSIDE_DIR")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
            .expect("set RENO_CONFORMANCE_OUTSIDE_DIR (or HOME)");
        let dir = base.join(format!(".reno-conformance-outside-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn file(&self) -> PathBuf {
        self.0.join(OUTSIDE_FILE)
    }
}

impl Drop for Outside {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The model to use: the environment's, else the first catalog entry containing one of `hints`.
fn cheapest(executor: &dyn Executor, env: &str, hints: &[&str]) -> String {
    if let Ok(model) = std::env::var(env)
        && !model.is_empty()
    {
        return model;
    }
    let models = executor.descriptor().caps.models;
    hints
        .iter()
        .find_map(|hint| models.iter().find(|m| m.id.contains(hint)))
        .map(|m| m.id.clone())
        .unwrap_or_else(|| {
            panic!(
                "no cheap model found in {:?}: set {env}",
                models.iter().map(|m| &m.id).collect::<Vec<_>>()
            )
        })
}

fn with_model(mut plan: Plan, model: &str) -> Plan {
    let model = Some(model.to_owned());
    for scenario in [
        plan.lifecycle.as_mut(),
        plan.input.as_mut(),
        plan.steer.as_mut(),
        plan.cancel.as_mut(),
        plan.resume.as_mut(),
        plan.mcp.as_mut(),
        plan.approval.as_mut().map(|a| &mut a.scenario),
    ]
    .into_iter()
    .flatten()
    .chain(plan.mode_runs.iter_mut().map(|m| &mut m.scenario))
    {
        scenario.model.clone_from(&model);
    }
    plan
}

/// What differs between the two CLIs' plans.
struct Shape {
    /// Claude asks questions with a tool of its own; Codex's is experimental.
    asks_questions: bool,
    /// Codex honours sandbox levels below `Unrestricted`; Claude Code declares none.
    sandboxes: bool,
}

fn live_plan(shape: &Shape, repo: &Path, outside: &Outside) -> Plan {
    let sleeper = |secs: u32| {
        format!("Use your shell tool to run `sleep {secs}`, then reply with exactly the word PONG.")
    };
    let mut mode_runs = vec![
        // The edit goes through the edit tool, which this mode accepts unasked.
        ModeRun {
            proves: Proof::Access(AccessMode::AutoAcceptEdits),
            scenario: Scenario::new(format!(
                "Create a file named {EDIT_FILE} containing the single word EDITED. Use your file \
                 editing or writing tool for it, not the shell. Then reply with exactly the word PONG."
            ))
            .access(AccessMode::AutoAcceptEdits)
            .fresh(EDIT_FILE)
            .expect(Expect::Tool {
                name: String::new(),
                class: ActionClass::Edit,
            }),
            answer: ApprovalAnswer::Allow,
            expect: vec![
                Expect::NoApprovalOf(ActionClass::Edit),
                Expect::FileContains {
                    path: EDIT_FILE.into(),
                    text: "EDITED".into(),
                },
            ],
        },
        // A command that is not a file edit asks, and a denial leaves no tag.
        ModeRun {
            proves: Proof::Access(AccessMode::AutoAcceptEdits),
            scenario: Scenario::new(
                "Use your shell tool to run `git tag conformance-auto-shell`, then reply with exactly the word PONG.",
            )
            .access(AccessMode::AutoAcceptEdits)
            .fresh(TAG_REF),
            answer: ApprovalAnswer::Deny,
            expect: vec![
                Expect::ApprovalClass(ActionClass::Command),
                Expect::PathAbsent(TAG_REF.into()),
            ],
        },
    ];
    if shape.sandboxes {
        // `FullAccess` never asks, so the sandbox alone decides; an approval that arrives anyway
        // (a request to leave the sandbox) is denied.
        mode_runs.push(ModeRun {
            proves: Proof::Sandbox(SandboxLevel::ReadOnly),
            scenario: Scenario::new(format!(
                "Use your shell tool to run `echo blocked > {READ_ONLY_FILE}`. If it is refused or \
                 fails, do not try anything else. Then reply with exactly the word PONG."
            ))
            .sandbox(SandboxLevel::ReadOnly)
            .fresh(READ_ONLY_FILE),
            answer: ApprovalAnswer::Deny,
            expect: vec![
                Expect::PathAbsent(READ_ONLY_FILE.into()),
                Expect::FileContains {
                    path: "README.md".into(),
                    text: "scratch".into(),
                },
            ],
        });
        let outside_file = outside.file();
        mode_runs.push(ModeRun {
            proves: Proof::Sandbox(SandboxLevel::WorkspaceWrite),
            scenario: Scenario::new(format!(
                "Use your shell tool to run these two commands, one after the other, even if the \
                 first fails or the second is refused: `echo inside > {INSIDE_FILE}` and then \
                 `echo outside > {}`. Then reply with exactly the word PONG.",
                outside_file.display()
            ))
            .sandbox(SandboxLevel::WorkspaceWrite)
            .fresh(INSIDE_FILE)
            .fresh(outside_file.display().to_string()),
            answer: ApprovalAnswer::Deny,
            expect: vec![
                Expect::FileContains {
                    path: INSIDE_FILE.into(),
                    text: "inside".into(),
                },
                Expect::PathAbsent(outside_file.display().to_string()),
            ],
        });
    }
    Plan {
        lifecycle: Some(
            Scenario::new(
                "Use your shell tool to run `echo conformance-tool`, then reply with exactly the word PONG.",
            )
            .expect(Expect::TextContains("PONG".into()))
            .expect(Expect::Tool {
                name: String::new(),
                class: ActionClass::Command,
            }),
        ),
        approval: Some(ApprovalPlan {
            scenario: Scenario::new(format!(
                "Use your shell tool to run `touch {APPROVAL_FILE}`, then reply with exactly the word PONG."
            ))
            .access(AccessMode::Supervised)
            .fresh(APPROVAL_FILE)
            .expect(Expect::ApprovalClass(ActionClass::Command)),
            on_allow: vec![Expect::PathExists(APPROVAL_FILE.into())],
            on_deny: vec![Expect::PathAbsent(APPROVAL_FILE.into())],
            // The model's reaction to a reason is its own: nothing here proves it arrived.
            on_deny_message: Vec::new(),
        }),
        input: shape.asks_questions.then(|| {
            Scenario::new(
                "Ask me which colour I prefer with your question tool (options: red, blue), then reply with the colour I chose and the word PONG.",
            )
            .access(AccessMode::Supervised)
            .expect(Expect::TextContains("PONG".into()))
        }),
        steer: Some(
            Scenario::new(sleeper(10))
                .steer_text("Change of plan: end your final reply with the word STEERED.")
                .expect(Expect::TextContains("STEERED".into())),
        ),
        cancel: Some(Scenario::new(sleeper(120))),
        crash: None,
        resume: Some(
            Scenario::new("Reply with exactly the word PONG, using no tools.")
                .resume_lifecycle_session()
                .expect(Expect::TextContains("PONG".into())),
        ),
        mcp: Some(
            Scenario::new(
                "Call the `ping` tool of the MCP server named `ping` exactly once and use no other tool. Then reply with exactly the word PONG.",
            )
            .mcp(vec![McpServerSpec {
                name: "ping".into(),
                command: env!("CARGO_BIN_EXE_conformance-ping-mcp").into(),
                args: vec![repo.join(MCP_MARKER).display().to_string()],
                env: Default::default(),
            }])
            .fresh(MCP_MARKER)
            .expect(Expect::FileContains {
                path: MCP_MARKER.into(),
                text: "pong".into(),
            })
            .expect(Expect::TextContains("PONG".into())),
        ),
        output_schema: None,
        unknown_frames: None,
        mode_runs,
    }
}

fn live_target(name: &str, executor: Arc<dyn Executor>, cwd: &Path, plan: Plan) -> Target {
    let mut target = Target::new(
        name,
        executor,
        Profile {
            // Claude's wire carries a deny reason; Codex's approval reply has no field for it.
            deny_message_on_wire: name == "claude-code",
            ..Profile::driver()
        },
        cwd.to_path_buf(),
        Limits::live(),
    )
    .with_plan(plan)
    .waive(
        CaseId::DenyMessage,
        "the model's reaction to a reason is not assertable",
    )
    .waive(CaseId::Crash, "a real CLI is not made to crash on purpose")
    .waive(
        CaseId::UnknownFrames,
        "a real CLI cannot be asked to send a frame it does not know",
    );
    target.exhaustive = false;
    if name == "codex" {
        target = target.waive(
            CaseId::Input,
            "request_user_input is experimental and not offered to a model by default",
        );
    }
    target
}

/// What `RENO_CONFORMANCE_LIVE` asks for: the names, and whether an unusable CLI is a failure.
fn selected() -> (Vec<String>, bool) {
    let value = std::env::var("RENO_CONFORMANCE_LIVE").unwrap_or_default();
    let names: Vec<String> = value
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    if names.len() == 1 && names[0] == "auto" {
        (vec!["claude-code".into(), "codex".into()], false)
    } else {
        (names, true)
    }
}

/// `Some(why)` when the CLI cannot be used for a run.
fn unusable(probe: &HealthProbe) -> Option<String> {
    match (&probe.installed, &probe.signed_in) {
        (InstallStatus::NotInstalled { why }, _) => Some(format!("not installed: {why:?}")),
        (_, SignedIn::No { hint }) => Some(format!("not signed in: {hint}")),
        _ => None,
    }
}

fn gate_name(probe: &HealthProbe) -> &'static str {
    use reno_proto::VersionGate;
    match probe.version_gate {
        VersionGate::Ok => "ok",
        VersionGate::TooOld { .. } => "too_old",
        VersionGate::Untested { .. } => "untested",
        VersionGate::Unchecked => "unchecked",
    }
}

/// The JSON record of one target the nightly job reads.
fn target_json(
    report: &TargetReport,
    executor: &dyn Executor,
    probe: &HealthProbe,
    model: &str,
    estimated_runs: usize,
) -> serde_json::Value {
    let descriptor = executor.descriptor();
    let mut value = report.to_json();
    value["model"] = json!(model);
    value["cliVersion"] = json!(probe.installed.version());
    value["cliMin"] = json!(descriptor.cli_verified.min);
    value["cliVerified"] = json!(descriptor.cli_verified.verified);
    value["versionGate"] = json!(gate_name(probe));
    value["estimatedRuns"] = json!(estimated_runs);
    value
}

#[tokio::test]
#[ignore = "runs the real CLIs and costs money: set RENO_CONFORMANCE_LIVE=claude-code,codex"]
async fn the_real_clis_pass_the_suite() {
    let (wanted, strict) = selected();
    assert!(
        !wanted.is_empty(),
        "set RENO_CONFORMANCE_LIVE to claude-code, codex, both or auto (see this file's docs)"
    );
    let outside = Outside::new();
    let mut reports: Vec<TargetReport> = Vec::new();
    let mut records: Vec<serde_json::Value> = Vec::new();
    let mut unavailable = Vec::new();
    for name in &wanted {
        let repo = scratch_repo();
        let executor: Arc<dyn Executor> = match name.as_str() {
            "claude-code" => Arc::new(ClaudeExecutor::new()),
            "codex" => Arc::new(CodexExecutor::new()),
            other => panic!("unknown live target {other:?}: claude-code or codex"),
        };
        // Probe first: the model lists change with it (Claude drops what its CLI is too old
        // for, Codex swaps in the account's live list).
        let probe = executor.probe().await;
        if let Some(why) = unusable(&probe) {
            records.push(json!({"target": name, "unavailable": why}));
            unavailable.push(format!("{name}: {why}"));
            continue;
        }
        let model = if name == "claude-code" {
            cheapest(
                executor.as_ref(),
                "RENO_CONFORMANCE_CLAUDE_MODEL",
                &["haiku"],
            )
        } else {
            cheapest(
                executor.as_ref(),
                "RENO_CONFORMANCE_CODEX_MODEL",
                &["mini", "luna", "spark"],
            )
        };
        let shape = Shape {
            asks_questions: name == "claude-code",
            sandboxes: name == "codex",
        };
        let plan = with_model(live_plan(&shape, repo.path(), &outside), &model);
        let estimated = plan.estimated_runs();
        println!(
            "{name}: model {model}, about {estimated} model runs, scratch repo {}",
            repo.path().display()
        );
        let target = live_target(name, Arc::clone(&executor), repo.path(), plan);
        let report = run_suite(&target).await;
        records.push(target_json(
            &report,
            executor.as_ref(),
            &probe,
            &model,
            estimated,
        ));
        reports.push(report);
        target.executor.shutdown().await;
    }
    println!("\n{}", render_table(&reports));
    if let Some(path) = std::env::var_os("RENO_CONFORMANCE_REPORT") {
        let report = json!({"schema": 1, "targets": records});
        std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
    }
    if strict {
        assert!(unavailable.is_empty(), "unusable: {unavailable:?}");
    } else {
        for line in &unavailable {
            println!("unavailable (not a failure under auto): {line}");
        }
    }
    for report in &reports {
        assert!(
            report.is_green(),
            "{} failed live: {:?}",
            report.target,
            report.failures()
        );
        let unverified = report
            .results
            .iter()
            .filter(|r| matches!(r.status, Status::Waived(_)))
            .count();
        println!(
            "{}: {unverified} case(s) waived (unverified)",
            report.target
        );
    }
}
