#![cfg(unix)]
//! A Claude child is never torn down while Claude Code is saving a refreshed
//! login: neither signalled nor sent EOF until the refresh lock is released.
//! Its own test binary — it points the process at a temp config dir.
use reno_harness::claude::refresh_lock;
use reno_harness::exec::{ExecControls, Executor};
use reno_harness::ClaudeExecutor;
use reno_proto::{AccessMode, ExecSpec, RunId, SandboxLevel};
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

const FAKE_CLAUDE: &str = r#"#!/bin/sh
dir="$(dirname "$0")"
case "$*" in
  *--input-format*) ;;
  *) echo "2.1.309 (Claude Code)"; exit 0 ;;
esac
trap 'echo term >> "$dir/events"; exit 0' TERM
: > "$dir/ready"
while read line; do :; done
echo eof >> "$dir/events"
"#;

/// A run's teardown (here: its consumer hung up) waits out a refresh too, before the CLI gets
/// EOF or a signal. The run names its config dir in its own environment, so it shares nothing
/// with any other test's.
#[tokio::test]
async fn a_run_is_torn_down_only_once_no_login_refresh_is_in_flight() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("run-config");
    std::fs::create_dir_all(&config).unwrap();
    let script = dir.path().join("claude");
    std::fs::write(&script, FAKE_CLAUDE).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let events = dir.path().join("events");
    let [held, _] = refresh_lock::lock_paths(&config);
    std::fs::create_dir(&held).unwrap();

    let spec = ExecSpec {
        run_id: RunId::from_suffix("01HZX9").unwrap(),
        prompt: "hello".into(),
        cwd: dir.path().to_path_buf(),
        model: None,
        effort: None,
        access: AccessMode::FullAccess,
        sandbox: SandboxLevel::WorkspaceWrite,
        resume: None,
        mcp: Vec::new(),
        env: [(
            "CLAUDE_SECURESTORAGE_CONFIG_DIR".to_owned(),
            config.display().to_string(),
        )]
        .into(),
        output_schema: None,
    };
    let (controls, _handle) = ExecControls::new(1);
    let stream = ClaudeExecutor::new()
        .with_executable(&script)
        .start(spec, controls)
        .await
        .unwrap();
    // Hang up only once the CLI listens for SIGTERM: a signal that beats the trap (a loaded
    // machine) would end it without a trace.
    for _ in 0..200 {
        if dir.path().join("ready").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    drop(stream);

    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        !events.exists(),
        "run torn down mid-refresh: {:?}",
        std::fs::read_to_string(&events)
    );
    std::fs::remove_dir(&held).unwrap();
    for _ in 0..100 {
        if events.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("run never torn down once the refresh was saved");
}
