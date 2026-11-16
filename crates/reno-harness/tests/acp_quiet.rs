//! Regression tests: quiet output never relinquishes a pending prompt. An agent that goes silent
//! after text, thoughts or tools is still working on its turn; only its `session/prompt`
//! response (or a cancel) ends it, and steering prompts queued meanwhile continue the same run.
//!
//! Against `tests/fixtures/acp-lifecycle.py`, an agent that rejects overlapping prompts the way a
//! real one does while it awaits its model.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use reno_harness::AcpExecutor;
use reno_harness::exec::{ExecControls, ExecHandle, Executor};
use reno_proto::{AccessMode, ExecEvent, ExecSpec, RunId, SandboxLevel, StopReason};

/// How long silence lasts before the test acts, in milliseconds.
const QUIET_MS: u64 = 1200;

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("acp-lifecycle.py");
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    path
}

fn executor() -> AcpExecutor {
    AcpExecutor::grok().with_executable(fixture_path())
}

fn spec(prompt: &str) -> ExecSpec {
    ExecSpec {
        run_id: RunId::from_suffix("01HZX9").unwrap(),
        prompt: prompt.into(),
        cwd: PathBuf::from("/tmp"),
        model: Some("grok-4.5".into()),
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

async fn start(prompt: &str) -> (BoxStream<'static, ExecEvent>, ExecHandle) {
    let (controls, handle) = ExecControls::new(8);
    let stream = executor().start(spec(prompt), controls).await.unwrap();
    (stream, handle)
}

fn text(s: &str) -> ExecEvent {
    ExecEvent::Text { text: s.into() }
}

async fn collect_to_end(stream: &mut BoxStream<'static, ExecEvent>) -> Vec<ExecEvent> {
    tokio::time::timeout(Duration::from_secs(15), async {
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    })
    .await
    .expect("the run must settle")
}

fn dones(events: &[ExecEvent]) -> Vec<(StopReason, Option<&str>)> {
    events
        .iter()
        .filter_map(|e| match e {
            ExecEvent::Done {
                stop, diagnostic, ..
            } => Some((*stop, diagnostic.as_deref())),
            _ => None,
        })
        .collect()
}

fn assert_one_done(events: &[ExecEvent], expected: StopReason) {
    let dones = dones(events);
    assert_eq!(dones.len(), 1, "{events:?}");
    assert_eq!(dones[0].0, expected, "{events:?}");
    if expected != StopReason::Error {
        assert_eq!(dones[0].1, None);
    }
}

/// The agent goes quiet after `scenario`'s opening; two follow-ups queue while the first prompt is
/// outstanding.
async fn delayed_turn(scenario: &str) {
    let (mut stream, handle) = start(scenario).await;
    let mut events = Vec::new();
    // Observe the scenario before steering. Grok re-sends a prompt cancelled before any progress,
    // so queuing immediately after the start races that path instead of exercising the intended
    // text/tool/reasoning state.
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let event = stream.next().await.expect("scenario event");
            let ready = match scenario {
                "open-tool" => matches!(&event, ExecEvent::Tool { id, .. } if id == "3"),
                "tools" => matches!(&event, ExecEvent::ToolResult { id, .. } if id == "3"),
                "reasoning" => matches!(&event, ExecEvent::Reasoning { .. }),
                "usage" | "text" => event == text("working"),
                _ => panic!("unknown scenario: {scenario}"),
            };
            assert!(
                !matches!(event, ExecEvent::Done { .. }),
                "premature Done: {event:?}"
            );
            events.push(event);
            if ready {
                break;
            }
        }
    })
    .await
    .expect("the scenario must start before steering");
    // Queue two follow-ups while the first prompt remains outstanding.
    handle.try_steer("second").unwrap();
    handle.try_steer("third").unwrap();
    events.extend(collect_to_end(&mut stream).await);

    // Each steer reached the agent exactly once, together, as one prompt that continued the run.
    let together = events
        .iter()
        .filter(|e| **e == text("second\n\nthird"))
        .count();
    assert_eq!(together, 1, "{events:?}");
    let finished = events.iter().position(|e| *e == text("finished"));
    let continued = events
        .iter()
        .position(|e| *e == text("second\n\nthird"))
        .unwrap();
    if scenario == "open-tool" {
        // A running tool is never preempted: the prompt keeps its outcome, then both steers
        // continue together as the next prompt.
        assert!(finished.is_some_and(|f| f < continued), "{events:?}");
    } else {
        // Silence is not completion, but steers preempt the generation immediately and continue
        // the same run together.
        assert_eq!(finished, None, "steers must preempt: {events:?}");
    }
    // One run, one Done, and it comes last.
    assert_one_done(&events, StopReason::Completed);
    assert!(matches!(events.last(), Some(ExecEvent::Done { .. })));
}

#[tokio::test]
async fn completed_tools_then_silence_preserves_prompt_and_followups() {
    delayed_turn("tools").await;
}
#[tokio::test]
async fn partial_text_then_silence_preserves_prompt_and_followups() {
    delayed_turn("text").await;
}
#[tokio::test]
async fn usage_is_not_completion() {
    delayed_turn("usage").await;
}
#[tokio::test]
async fn reasoning_then_silence_preserves_prompt() {
    delayed_turn("reasoning").await;
}
#[tokio::test]
async fn open_tools_then_silence_preserves_prompt() {
    delayed_turn("open-tool").await;
}

async fn cancel_quiet(scenario: &str) {
    let (mut stream, handle) = start(scenario).await;
    while let Some(event) = stream.next().await {
        if matches!(event, ExecEvent::ToolResult { id, .. } if id == "3") {
            break;
        }
    }
    assert!(
        tokio::time::timeout(Duration::from_millis(QUIET_MS * 2), stream.next())
            .await
            .is_err(),
        "silence must not emit Done"
    );
    handle.try_steer("must not run").unwrap();
    handle.cancel();
    let events = collect_to_end(&mut stream).await;
    assert_one_done(&events, StopReason::Interrupted);
    assert!(!events.iter().any(|e| matches!(e, ExecEvent::Text { .. })));
}

#[tokio::test]
async fn quiet_turn_can_be_cancelled_without_promoting_followup() {
    cancel_quiet("cancel").await;
}
#[tokio::test]
async fn unresponsive_quiet_turn_is_killed_on_cancel() {
    cancel_quiet("wedge").await;
}

#[tokio::test]
async fn missing_response_at_eof_is_error_not_success() {
    let (mut stream, _handle) = start("eof").await;
    assert_one_done(&collect_to_end(&mut stream).await, StopReason::Error);
}

#[tokio::test]
async fn protocol_error_keeps_code_and_agent_detail() {
    let (mut stream, _handle) = start("error").await;
    let events = collect_to_end(&mut stream).await;
    assert_one_done(&events, StopReason::Error);
    let diagnostic = dones(&events)[0].1.unwrap();
    for expected in [
        "session/prompt",
        "Invalid request",
        "-32600",
        "A prompt is already running",
        "retryable",
    ] {
        assert!(diagnostic.contains(expected), "{diagnostic}");
    }
}
