//! The silent-npm-failure condition, reproduced: npm dying silently with an errno-encoded exit
//! (254 = ENOENT) during the adapter fallback. A bare `npx -y` path surfaced this as "harness
//! protocol error: initialize: app-server exited before responding; ... (exit code 254)" with
//! nothing actionable. The managed install is an explicit step (`Executor::install`), never part
//! of a run: discovery reports `NeedsInstall`, `start` refuses the run without touching npm, and
//! the install step fails with the decoded errno and a recovery hint.
//!
//! Single-test binary: it mutates PATH/SHELL/RENO_* env process-wide.

// Tests unwrap freely and set environment variables.
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;

use reno_harness::AcpExecutor;
use reno_harness::exec::{ExecControls, ExecError, Executor};
use reno_proto::{AccessMode, ExecSpec, InstallStatus, NotInstalledReason, RunId, SandboxLevel};

#[tokio::test]
async fn silent_npm_enoent_death_surfaces_decoded_error_from_the_explicit_install() {
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

    let executor = AcpExecutor::grok();

    // Discovery says the adapter needs its explicit install; it does not run npm to find out.
    let health = executor.probe().await;
    assert!(
        matches!(
            &health.installed,
            InstallStatus::NotInstalled {
                why: NotInstalledReason::NeedsInstall { what }
            } if what.contains("@xai-official/grok")
        ),
        "{health:?}"
    );

    // A run never installs: it is refused as not installed, and nothing was fetched.
    let (controls, _handle) = ExecControls::new(1);
    let spec = ExecSpec {
        run_id: RunId::from_suffix("01HZX9").unwrap(),
        inherit_user_config: false,
        prompt: "hi".into(),
        cwd: "/tmp".into(),
        model: None,
        effort: None,
        access: AccessMode::FullAccess,
        sandbox: SandboxLevel::Unrestricted,
        resume: None,
        mcp: Vec::new(),
        env: Default::default(),
        output_schema: None,
        instructions: None,
    };
    let refused = executor.start(spec, controls).await.err().expect("refused");
    assert!(matches!(refused, ExecError::NotInstalled(_)), "{refused:?}");
    assert!(
        !dir.path().join("adapters").exists(),
        "start() must not have started an install"
    );

    // The explicit install step is where npm runs, and where its failure is decoded.
    let error = executor.install().await.expect_err("npm dies silently");
    let message = error.to_string();
    assert!(message.contains("exit code 254"), "{message}");
    assert!(message.contains("ENOENT"), "{message}");
    assert!(message.contains("failed silently"), "{message}");
    assert!(
        message.contains("@xai-official/grok"),
        "names the package: {message}"
    );
}
