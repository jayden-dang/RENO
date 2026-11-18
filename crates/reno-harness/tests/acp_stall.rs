//! Prompt-stall watchdog: a grok-spec agent that goes TOTALLY silent after the prompt (the
//! wedged-shared-leader signature from the field) must end the run with an error and a diagnostic
//! instead of waiting forever. The bound is the executor's own (`with_prompt_stall`), not a
//! process-global knob.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use reno_harness::AcpExecutor;
use reno_harness::exec::{ExecControls, Executor};
use reno_proto::{AccessMode, ExecEvent, ExecSpec, RunId, SandboxLevel, StopReason};

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-acp.sh");
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    path
}

fn spec(prompt: &str) -> ExecSpec {
    ExecSpec {
        model_options: Default::default(),
        run_id: RunId::from_suffix("01HZX9").unwrap(),
        inherit_user_config: false,
        prompt: prompt.into(),
        cwd: PathBuf::from("/tmp"),
        model: None,
        effort: None,
        access: AccessMode::FullAccess,
        sandbox: SandboxLevel::Unrestricted,
        resume: None,
        mcp: Vec::new(),
        env: Default::default(),
        output_schema: None,
        instructions: None,
    }
}

async fn events_of(executor: &AcpExecutor, prompt: &str) -> Vec<ExecEvent> {
    let (controls, _handle) = ExecControls::new(8);
    let stream = executor
        .start(spec(prompt), controls)
        .await
        .expect("run starts");
    tokio::time::timeout(Duration::from_secs(10), stream.collect::<Vec<_>>())
        .await
        .expect("the run ended in time")
}

#[tokio::test]
async fn silent_agent_errors_via_the_prompt_stall_watchdog() {
    let executor = AcpExecutor::grok()
        .with_executable(fixture_path())
        .with_prompt_stall(Some(Duration::from_millis(700)));
    let events = events_of(&executor, "scenario:prompt-stall").await;
    match events.last() {
        Some(ExecEvent::Done {
            stop: StopReason::Error,
            diagnostic: Some(diagnostic),
            ..
        }) => {
            assert!(
                diagnostic.contains("did not respond"),
                "the diagnostic names the stall: {diagnostic}"
            );
            // And what to check, from the agent's own spec.
            assert!(diagnostic.contains("--no-leader"), "{diagnostic}");
        }
        other => panic!("not an error Done: {other:?} in {events:?}"),
    }
}

#[tokio::test]
async fn an_agent_without_a_stall_bound_is_not_cut_off() {
    // Devin pins no stall bound, and the fixture's turn stays silent for a minute.
    let executor = AcpExecutor::devin().with_executable(fixture_path());
    let (controls, handle) = ExecControls::new(8);
    let mut stream = executor
        .start(spec("scenario:prompt-stall"), controls)
        .await
        .unwrap();
    // Silence for longer than the bound the test above gives Grok ends nothing.
    assert!(
        tokio::time::timeout(Duration::from_millis(1500), async {
            while let Some(event) = stream.next().await {
                if matches!(event, ExecEvent::Done { .. }) {
                    return;
                }
            }
        })
        .await
        .is_err()
    );
    handle.cancel();
    let events = tokio::time::timeout(Duration::from_secs(10), stream.collect::<Vec<_>>())
        .await
        .unwrap();
    assert!(matches!(
        events.last(),
        Some(ExecEvent::Done {
            stop: StopReason::Interrupted,
            ..
        })
    ));
}
