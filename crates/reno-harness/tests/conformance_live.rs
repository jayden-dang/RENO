//! The conformance suite on the real Claude Code and Codex CLIs. Opt in, and it costs money:
//!
//! ```text
//! RENO_CONFORMANCE_LIVE=claude-code,codex \
//!   cargo test -p reno-harness --test conformance_live -- --ignored --nocapture
//! ```
//!
//! `RENO_CONFORMANCE_LIVE` names the CLIs to run (`claude-code`, `codex`, or both); both must be
//! installed and signed in (`probe` says so first, and a target that is not usable is reported
//! and fails the run). It is `#[ignore]`d, so no plain `cargo test` ever starts a real CLI.
//!
//! What it spends: about seven short model runs per CLI (one tool-using turn, two approvals, a
//! question on Claude, a steer, a cancel, a resume), at the cheapest model the driver's catalog
//! offers (`claude-code`: the first `haiku`; `codex`: the first `mini`, `luna` or `spark`), each
//! with a tiny prompt in a throwaway git repository. Override the model with
//! `RENO_CONFORMANCE_CLAUDE_MODEL` / `RENO_CONFORMANCE_CODEX_MODEL`. Hard bounds: 120 s per
//! case, 900 s per CLI (`Limits::live`); a case over its bound fails, and cases left when the
//! total is spent are not run.
//!
//! What it does not prove, reported as `waived` (an unverified claim, never a pass): a deny
//! message reaching the model (Claude), a harness crash, MCP injection (needs an MCP server), an
//! unrecognised frame (a real CLI cannot be asked to send one), and, on Codex, a question
//! (`request_user_input` is experimental). The unrecognised-frame counters are still printed, and
//! the clean-path check of that case still runs. Access modes and sandbox levels the descriptor
//! claims are not each run (that would be a model call apiece); the refusals of the unclaimed
//! ones are checked, and the rest is reported `waived`.
//!
//! The suite never runs the CLIs with more than the access the case asks for, in a scratch
//! repository, but a model is a model: read the prompts below before the first run.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::sync::Arc;

use reno_harness::conformance::{
    ApprovalPlan, CaseId, Expect, Limits, Plan, Profile, Scenario, Status, Target, TargetReport,
    render_table, run_suite,
};
use reno_harness::exec::Executor;
use reno_harness::{ClaudeExecutor, CodexExecutor};
use reno_proto::{AccessMode, ActionClass, InstallStatus, SignedIn};

const APPROVAL_FILE: &str = "conformance-approval.txt";

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
        plan.approval.as_mut().map(|a| &mut a.scenario),
    ]
    .into_iter()
    .flatten()
    {
        scenario.model.clone_from(&model);
    }
    plan
}

fn live_plan(asks_questions: bool) -> Plan {
    let sleeper = |secs: u32| {
        format!("Use your shell tool to run `sleep {secs}`, then reply with exactly the word PONG.")
    };
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
        input: asks_questions.then(|| {
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
        mcp: None,
        output_schema: None,
        unknown_frames: None,
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
        CaseId::Mcp,
        "needs an MCP server to inject and a prompt that calls it",
    )
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

fn selected() -> Vec<String> {
    std::env::var("RENO_CONFORMANCE_LIVE")
        .unwrap_or_default()
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect()
}

/// `Some(why)` when the CLI cannot be used for a run.
async fn unusable(executor: &dyn Executor) -> Option<String> {
    let probe = executor.probe().await;
    match (&probe.installed, &probe.signed_in) {
        (InstallStatus::NotInstalled { why }, _) => Some(format!("not installed: {why:?}")),
        (_, SignedIn::No { hint }) => Some(format!("not signed in: {hint}")),
        _ => None,
    }
}

#[tokio::test]
#[ignore = "runs the real CLIs and costs money: set RENO_CONFORMANCE_LIVE=claude-code,codex"]
async fn the_real_clis_pass_the_suite() {
    let wanted = selected();
    assert!(
        !wanted.is_empty(),
        "set RENO_CONFORMANCE_LIVE to claude-code, codex, or both (see this file's docs)"
    );
    let mut reports: Vec<TargetReport> = Vec::new();
    let mut unavailable = Vec::new();
    for name in &wanted {
        let repo = scratch_repo();
        let (executor, model): (Arc<dyn Executor>, String) = match name.as_str() {
            "claude-code" => {
                let executor = ClaudeExecutor::new();
                let model = cheapest(&executor, "RENO_CONFORMANCE_CLAUDE_MODEL", &["haiku"]);
                (Arc::new(executor), model)
            }
            "codex" => {
                let executor = CodexExecutor::new();
                let model = cheapest(
                    &executor,
                    "RENO_CONFORMANCE_CODEX_MODEL",
                    &["mini", "luna", "spark"],
                );
                (Arc::new(executor), model)
            }
            other => panic!("unknown live target {other:?}: claude-code or codex"),
        };
        if let Some(why) = unusable(executor.as_ref()).await {
            unavailable.push(format!("{name}: {why}"));
            continue;
        }
        println!(
            "{name}: model {model}, scratch repo {}",
            repo.path().display()
        );
        let plan = with_model(live_plan(name == "claude-code"), &model);
        let target = live_target(name, executor, repo.path(), plan);
        reports.push(run_suite(&target).await);
        target.executor.shutdown().await;
    }
    println!("\n{}", render_table(&reports));
    assert!(unavailable.is_empty(), "unusable: {unavailable:?}");
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
