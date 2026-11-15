//! The silent-npm-failure condition, reproduced: npm dying silently with an errno-encoded exit
//! (254 = ENOENT) during the adapter fallback. A bare `npx -y` path surfaced this as "harness
//! protocol error: initialize: app-server exited before responding; ... (exit code 254)" with
//! nothing actionable. The managed install must instead fail the run with the decoded errno and a
//! recovery hint: a failed run (`Done { Error }`), not an executor that is ineligible.
//!
//! Single-test binary: it mutates PATH/SHELL/RENO_* env process-wide.

// Tests unwrap freely and set environment variables.
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;

use futures::StreamExt;
use reno_harness::AcpExecutor;
use reno_harness::exec::{ExecControls, Executor};
use reno_proto::{AccessMode, ExecEvent, ExecSpec, RunId, SandboxLevel, StopReason};

#[tokio::test]
async fn silent_npm_enoent_death_surfaces_decoded_error() {
    let dir = tempfile::tempdir().unwrap();
    let bin = dir.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    // npm as users hit it: dies with 254 and says nothing.
    let npm = bin.join("npm");
    std::fs::write(&npm, "#!/bin/sh\nexit 254\n").unwrap();
    std::fs::set_permissions(&npm, std::fs::Permissions::from_mode(0o755)).unwrap();

    // SAFETY: single-test binary: nothing else reads env concurrently.
    unsafe {
        std::env::set_var("RENO_ADAPTERS_DIR", dir.path().join("adapters"));
        std::env::set_var("RENO_NO_LOGIN_SHELL", "1");
        std::env::set_var("PATH", &bin);
        std::env::set_var("HOME", dir.path());
        std::env::remove_var("GROK_EXECUTABLE");
    }

    let (controls, _handle) = ExecControls::new(1);
    let spec = ExecSpec {
        run_id: RunId::from_suffix("01HZX9").unwrap(),
        prompt: "hi".into(),
        cwd: "/tmp".into(),
        model: None,
        effort: None,
        access: AccessMode::FullAccess,
        sandbox: SandboxLevel::WorkspaceWrite,
        resume: None,
        mcp: Vec::new(),
        env: Default::default(),
        output_schema: None,
    };
    let stream = AcpExecutor::grok()
        .start(spec, controls)
        .await
        .expect("a failed install is a failed run, not an ineligible executor");
    let events: Vec<ExecEvent> = stream.collect().await;
    let Some(ExecEvent::Done {
        stop: StopReason::Error,
        diagnostic: Some(message),
        ..
    }) = events.last()
    else {
        panic!("expected an error Done, got: {events:?}");
    };
    assert!(message.contains("exit code 254"), "{message}");
    assert!(message.contains("ENOENT"), "{message}");
    assert!(message.contains("failed silently"), "{message}");
    assert!(
        message.contains("@xai-official/grok"),
        "names the package: {message}"
    );
}
