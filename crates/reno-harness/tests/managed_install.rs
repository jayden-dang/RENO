//! Real-world E2E for the managed adapter install: with no `grok` binary anywhere, the explicit
//! `install()` step npm-installs the pinned adapter into `$RENO_ADAPTERS_DIR`, and a run then
//! spawns it via node and reaches `Started` (the full initialize then session/new handshake).
//!
//! Ignored: needs network and npm. Run with
//! `cargo test -p reno-harness --test managed_install -- --ignored`.
//!
//! Single-test binary: it mutates RENO_ADAPTERS_DIR process-wide.

// Tests unwrap freely and set environment variables.
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]

use futures::StreamExt;
use reno_harness::AcpExecutor;
use reno_harness::exec::{ExecControls, Executor};
use reno_proto::{AccessMode, ExecEvent, ExecSpec, RunId, SandboxLevel};

#[tokio::test]
#[ignore = "network + npm; installs the pinned Grok CLI for real"]
async fn managed_install_reaches_started() {
    let adapters = tempfile::tempdir().unwrap();
    // SAFETY: single-test binary: nothing else reads env concurrently.
    unsafe {
        std::env::set_var("RENO_ADAPTERS_DIR", adapters.path());
        std::env::remove_var("GROK_EXECUTABLE");
    }

    let executor = AcpExecutor::grok();
    tokio::time::timeout(std::time::Duration::from_secs(600), executor.install())
        .await
        .expect("the install finished in time")
        .expect("the adapter installed");

    let (controls, handle) = ExecControls::new(1);
    let spec = ExecSpec {
        run_id: RunId::from_suffix("01HZX9").unwrap(),
        inherit_user_config: false,
        prompt: "say the word ok and stop".into(),
        cwd: std::env::temp_dir(),
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
    let mut stream = tokio::time::timeout(
        std::time::Duration::from_secs(600),
        executor.start(spec, controls),
    )
    .await
    .expect("start() finished in time")
    .expect("start() resolved a launch");

    // The handshake proves the whole chain: managed install, node spawn, initialize,
    // session/new. Cancel as soon as the session opens so the test never burns a real turn.
    let mut started = false;
    while let Some(event) = tokio::time::timeout(std::time::Duration::from_secs(180), stream.next())
        .await
        .expect("stream made progress")
    {
        match event {
            ExecEvent::Started { session, .. } => {
                assert!(!session.0.is_empty(), "handshake produced a session id");
                started = true;
                handle.cancel();
            }
            ExecEvent::Done {
                stop, diagnostic, ..
            } => {
                assert!(started, "run died before Started: {stop:?} {diagnostic:?}");
                break;
            }
            _ => {}
        }
    }
    assert!(started, "stream ended without Started");

    // The install landed in the managed dir (not the user's npm state) and is marked complete, so
    // the next launch skips npm entirely.
    let root = adapters.path().join("xai-official__grok");
    let version_dir = std::fs::read_dir(&root)
        .expect("managed install dir exists")
        .flatten()
        .next()
        .expect("a pinned version dir")
        .path();
    assert!(version_dir.join(".reno-install-ok").exists());
}
