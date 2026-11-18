//! The conformance suite (ARCHITECTURE 5.3) on the mock and on every driver over its fake CLI.
//!
//! One table, case by executor. The labels of every column are pinned below: a case that quietly
//! turns into a skip, or a claim that stops being exercised, fails here. The suite itself is
//! tested too (`broken.rs`): an executor that breaks the contract must fail the
//! cases that prove it.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod broken;
mod modes;
mod targets;

use reno_harness::conformance::{CaseId, Status, Target, TargetReport, render_table, run_suite};

use targets::{
    claude_target, codex_target, cursor_target, devin_target, grok_target, hermes_target,
    mock_target, pi_target, registry_target,
};

async fn run_all(targets: Vec<Target>) -> Vec<TargetReport> {
    let runs = targets
        .into_iter()
        .map(|t| async move { run_suite(&t).await });
    futures::future::join_all(runs).await
}

/// Every case is `pass` unless `exceptions` says otherwise.
fn pinned(exceptions: &[(&str, &str)]) -> Vec<String> {
    CaseId::ALL
        .iter()
        .map(|case| {
            let label = exceptions
                .iter()
                .find(|(name, _)| *name == case.name())
                .map_or("pass", |(_, label)| label);
            format!("{}={label}", case.name())
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_mock_and_every_driver_pass_the_suite() {
    let reports = run_all(vec![
        mock_target(),
        claude_target(),
        codex_target(),
        grok_target(),
        registry_target(),
        devin_target(),
        hermes_target(),
        pi_target(),
        cursor_target(),
    ])
    .await;
    println!("\n{}", render_table(&reports));
    for report in &reports {
        assert!(
            report.is_green(),
            "{} failed: {:?}",
            report.target,
            report.failures()
        );
        // A claim that is waived is unverified; on a fake nothing may be.
        for result in &report.results {
            assert!(
                !matches!(result.status, Status::Waived(_)),
                "{} / {} is waived",
                report.target,
                result.case.name()
            );
        }
    }
    let labels = |name: &str| reports.iter().find(|r| r.target == name).unwrap().labels();
    assert_eq!(
        labels("mock"),
        pinned(&[
            ("process-records", "skip"),
            ("deny-message", "skip"),
            ("plan-mode", "skip"),
            ("user-config", "skip"),
            ("unknown-frames", "skip"),
        ])
    );
    assert_eq!(
        labels("claude-code"),
        pinned(&[("plan-mode", "skip")]),
        "Claude carries a deny message to the model"
    );
    assert_eq!(
        labels("codex"),
        pinned(&[("deny-message", "skip"), ("plan-mode", "skip")]),
        "Codex's approval reply has no field for a reason"
    );
    let acp_full_access_only = [
        ("approval-allow", "skip"),
        ("approval-deny", "skip"),
        ("deny-message", "skip"),
        ("plan-mode", "skip"),
        ("user-config", "skip"),
    ];
    for name in ["acp.grok", "acp.devin", "acp.hermes"] {
        assert_eq!(labels(name), pinned(&acp_full_access_only), "{name}");
    }
    // Pi asks for no approval: full access only, like the ACP agents above, but it isolates the
    // owner's tools with launch flags, so the user-config case runs.
    assert_eq!(
        labels("pi"),
        pinned(&[
            ("approval-allow", "skip"),
            ("approval-deny", "skip"),
            ("deny-message", "skip"),
            ("plan-mode", "skip"),
        ])
    );
    // Cursor asks for no approval and has no way to take a question's answer back: full access
    // only, no input case, and it isolates the owner's settings, so the user-config case runs.
    assert_eq!(
        labels("cursor"),
        pinned(&[
            ("approval-allow", "skip"),
            ("approval-deny", "skip"),
            ("deny-message", "skip"),
            ("input", "skip"),
            ("plan-mode", "skip"),
        ])
    );
    assert_eq!(
        labels("acp.registry"),
        pinned(&[("plan-mode", "skip"), ("user-config", "skip")])
    );

    // The counters: only what the fakes made up, never the clean runs.
    let counted = |name: &str| {
        reports
            .iter()
            .find(|r| r.target == name)
            .unwrap()
            .unknown_frames
            .clone()
    };
    assert_eq!(
        counted("claude-code").into_iter().collect::<Vec<_>>(),
        vec![("type:zz_future_frame".to_owned(), 1)]
    );
    assert_eq!(
        counted("codex").into_iter().collect::<Vec<_>>(),
        vec![("method:zz/futureNotification".to_owned(), 1)]
    );
    // ACP's unknown-frame scenario is the fake's `happy`, which makes up two frames; everything
    // else the suite ran (approvals, questions, steering, resume, a crash) counted nothing.
    for name in ["acp.registry", "acp.grok", "acp.devin", "acp.hermes"] {
        assert_eq!(
            counted(name).into_iter().collect::<Vec<_>>(),
            vec![
                ("method:some/unknownNotification".to_owned(), 1),
                ("update:totally_unknown_kind".to_owned(), 1),
            ],
            "{name}"
        );
    }
    assert_eq!(
        counted("pi").into_iter().collect::<Vec<_>>(),
        vec![
            ("message_update:audio_delta".to_owned(), 1),
            ("type:agent_teleported".to_owned(), 1),
        ]
    );
    assert_eq!(
        counted("cursor").into_iter().collect::<Vec<_>>(),
        vec![
            ("ev:hologram".to_owned(), 1),
            ("tool:teleport".to_owned(), 1)
        ]
    );
    assert!(counted("mock").is_empty());
}
