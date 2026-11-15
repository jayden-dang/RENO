//! `AcpExecutor` durability cases against `tests/fixtures/fake-hardened-acp.py`: dialogs a cancel
//! must answer, crashes during the handshake, sign-in retries and session restore.

#![cfg(unix)]

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;

use reno_harness::AcpExecutor;
use reno_harness::exec::{ExecControls, ExecHandle, Executor};
use reno_proto::{AccessMode, ExecEvent, ExecSpec, RunId, SandboxLevel, SessionRef, StopReason};

fn fixture_path() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("fake-hardened-acp.py");
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    path
}

fn executor() -> AcpExecutor {
    AcpExecutor::grok().with_executable(fixture_path())
}

fn spec(mode: &str) -> ExecSpec {
    ExecSpec {
        run_id: RunId::from_suffix("01HZX9").unwrap(),
        prompt: "hello".into(),
        cwd: PathBuf::from("/tmp"),
        model: None,
        effort: None,
        access: AccessMode::FullAccess,
        sandbox: SandboxLevel::WorkspaceWrite,
        resume: None,
        mcp: Vec::new(),
        env: [("FAKE_ACP_MODE".to_owned(), mode.to_owned())]
            .into_iter()
            .collect(),
        output_schema: None,
    }
}

struct Run {
    stream: BoxStream<'static, ExecEvent>,
    handle: ExecHandle,
}

impl Run {
    async fn next(&mut self) -> Option<ExecEvent> {
        tokio::time::timeout(Duration::from_secs(10), self.stream.next())
            .await
            .expect("event in time")
    }

    /// Every event up to and including the `Done`.
    async fn until_done(&mut self) -> Vec<ExecEvent> {
        let mut events = Vec::new();
        loop {
            let event = self.next().await.expect("stream ended before Done");
            let done = matches!(event, ExecEvent::Done { .. });
            events.push(event);
            if done {
                return events;
            }
        }
    }

    async fn until_text(&mut self, wanted: &str) {
        loop {
            match self.next().await {
                Some(ExecEvent::Text { text }) if text == wanted => return,
                Some(_) => {}
                None => panic!("stream ended before {wanted:?}"),
            }
        }
    }
}

async fn start(spec: ExecSpec) -> Run {
    let (controls, handle) = ExecControls::new(8);
    let stream = executor().start(spec, controls).await.expect("run starts");
    Run { stream, handle }
}

fn done_of(events: &[ExecEvent]) -> (StopReason, Option<String>) {
    events
        .iter()
        .find_map(|e| match e {
            ExecEvent::Done {
                stop, diagnostic, ..
            } => Some((*stop, diagnostic.clone())),
            _ => None,
        })
        .expect("a Done")
}

fn texts(events: &[ExecEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| match e {
            ExecEvent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// ACP requires every pending permission request to be answered `cancelled` once the prompt is
/// cancelled; the agent here waits on exactly that. The question is raised as `NeedsInput` and
/// is never answered by the person.
#[tokio::test]
async fn a_cancel_answers_an_open_question_cancelled() {
    let mut run = start(spec("question")).await;
    run.until_text("asking").await;
    loop {
        match run.next().await.expect("the question is raised") {
            ExecEvent::NeedsInput { questions, .. } => {
                assert_eq!(questions[0].options, ["A", "B"]);
                break;
            }
            ExecEvent::Done { .. } => panic!("ended before the question"),
            _ => {}
        }
    }
    run.handle.cancel();
    let events = run.until_done().await;
    assert_eq!(texts(&events), ["question cancelled"]);
    assert_eq!(done_of(&events), (StopReason::Interrupted, None));
}

/// Grok's cancel carries the user gesture; the fixture settles a gesture-less cancel as a
/// refusal. Its initialize asserts no client type is declared.
#[tokio::test]
async fn grok_cancel_carries_its_meta_and_initialize_names_no_client_type() {
    let mut run = start(spec("grok-meta")).await;
    run.until_text("working").await;
    run.handle.cancel();
    let events = run.until_done().await;
    assert_eq!(done_of(&events), (StopReason::Interrupted, None));
}

/// A config switch the agent never answers must not hold off a cancel.
#[tokio::test]
async fn a_cancel_gets_through_an_unanswered_config_switch() {
    let mut spec = spec("config-hang");
    // The session opens on grok-4.5, so this model needs a switch the fixture never answers.
    spec.model = Some("grok-4-fast".into());
    let mut run = start(spec).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = std::time::Instant::now();
    run.handle.cancel();
    let events = run.until_done().await;
    assert_eq!(done_of(&events), (StopReason::Interrupted, None));
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

/// The adapter dies mid-handshake while a detached descendant holds stdout open: the crash
/// surfaces at once, not after the 120s handshake bound.
#[tokio::test]
async fn a_crash_during_the_handshake_reports_without_waiting_for_eof() {
    let started = std::time::Instant::now();
    let mut run = start(spec("handshake-crash")).await;
    let events = run.until_done().await;
    let (stop, diagnostic) = done_of(&events);
    assert_eq!(stop, StopReason::Error);
    let diagnostic = diagnostic.unwrap_or_default();
    assert!(
        diagnostic.contains("exited during the ACP handshake"),
        "{diagnostic}"
    );
    assert!(diagnostic.contains("adapter blew up"), "{diagnostic}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );
}

/// auth_required on session/new: the run authenticates with the agent's own default method once
/// and retries.
#[tokio::test]
async fn auth_required_signs_in_with_the_default_method_and_retries() {
    let mut run = start(spec("auth")).await;
    let events = run.until_done().await;
    assert_eq!(texts(&events), ["signed in"]);
    assert_eq!(done_of(&events), (StopReason::Completed, None));
}

#[tokio::test]
async fn a_terminal_only_sign_in_is_refused_with_a_clear_message() {
    let mut run = start(spec("auth-terminal")).await;
    let events = run.until_done().await;
    let (stop, diagnostic) = done_of(&events);
    assert_eq!(stop, StopReason::Error);
    let diagnostic = diagnostic.unwrap_or_default();
    assert!(diagnostic.contains("Grok isn't signed in"), "{diagnostic}");
    assert!(
        diagnostic.contains("(terminal) can't run inside a run"),
        "{diagnostic}"
    );
}

/// An agent advertising session/resume but not loadSession is restored with resume, not a load
/// it never offered.
#[tokio::test]
async fn resume_uses_the_advertised_restore_method() {
    let mut spec = spec("resume");
    spec.resume = Some(SessionRef("old-session".into()));
    let mut run = start(spec).await;
    let events = run.until_done().await;
    assert!(matches!(
        &events[0],
        ExecEvent::Started { session, .. } if session.0 == "old-session"
    ));
    assert_eq!(texts(&events), ["resumed old-session"]);
    assert_eq!(done_of(&events), (StopReason::Completed, None));
}
