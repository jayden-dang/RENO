//! [`MockExecutor`]: an [`Executor`] that plays a scripted run, for kernel tests (T0.8) and the
//! CLI's `--executor mock`.
//!
//! It spawns no task and reads no wall clock: every wait is a tokio timer or a channel receive,
//! so under `tokio::time::pause` a run is fully deterministic.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use reno_proto::{
    AccessMode, ExecEvent, ExecSpec, ExecutorCaps, ExecutorDescriptor, ExecutorId, ModelInfo,
    SandboxLevel, SessionRef, Steering, StopReason, VersionRange,
};

use super::{ExecAnswer, ExecControls, ExecError, Executor, ExecutorHealth, require_access};

/// One step of a mock run.
#[derive(Debug, Clone)]
pub enum MockStep {
    /// Emit this event. Emitting `ExecEvent::Done` ends the run: later steps never play.
    Emit(ExecEvent),
    /// Wait this long (tokio time).
    Sleep(Duration),
    /// Wait until the run's controls carry an answer to this request id. Answers for other ids
    /// that arrive meanwhile are kept, in order, for their own step.
    AwaitAnswer {
        /// The `request_id` of a `NeedsInput` or `NeedsApproval` event emitted earlier.
        request_id: String,
    },
    /// Wait until the run's controls carry a steering prompt.
    AwaitSteer,
}

impl MockStep {
    /// Emit `ExecEvent::Text`.
    pub fn text(text: impl Into<String>) -> Self {
        Self::Emit(ExecEvent::Text { text: text.into() })
    }

    /// Emit `ExecEvent::Started`.
    pub fn started(session: impl Into<String>, model: impl Into<String>) -> Self {
        Self::Emit(ExecEvent::Started {
            session: SessionRef(session.into()),
            model: model.into(),
        })
    }

    /// Emit `ExecEvent::Done` with this stop reason and no result or diagnostic.
    pub fn done(stop: StopReason) -> Self {
        Self::Emit(ExecEvent::Done {
            stop,
            result: None,
            diagnostic: None,
        })
    }
}

/// What a [`MockExecutor`] saw, for assertions.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MockLog {
    /// The specs of every run started, in order. A refused run is not recorded.
    pub specs: Vec<ExecSpec>,
    /// Every steering prompt received, in order.
    pub steers: Vec<String>,
    /// Every answer received, in order, matched or not.
    pub answers: Vec<ExecAnswer>,
}

/// An [`Executor`] that plays a script. Cheap to clone; clones share the log.
///
/// The script always ends with exactly one `Done`: if the steps run out without one, the run
/// ends `Done { Completed }`; if the cancel token fires, it ends `Done { Interrupted }` at the
/// next step boundary or wait.
#[derive(Debug, Clone)]
pub struct MockExecutor {
    descriptor: ExecutorDescriptor,
    steps: Vec<MockStep>,
    log: Arc<Mutex<MockLog>>,
}

impl MockExecutor {
    /// A mock named `id`, with [`Self::default_descriptor`] and an empty script (which completes
    /// at once).
    pub fn new(id: ExecutorId) -> Self {
        Self {
            descriptor: Self::default_descriptor(id),
            steps: Vec::new(),
            log: Arc::default(),
        }
    }

    /// The descriptor a mock starts with: every access mode and sandbox level, step-boundary
    /// steering, resume, MCP injection and one model, `mock-1`.
    pub fn default_descriptor(id: ExecutorId) -> ExecutorDescriptor {
        ExecutorDescriptor {
            id,
            contract_version: 1,
            adapter_version: "mock".into(),
            cli_verified: VersionRange {
                min: "0".into(),
                verified: "0".into(),
            },
            caps: ExecutorCaps {
                steering: Steering::StepBoundary,
                resume: true,
                access_modes: vec![
                    AccessMode::Supervised,
                    AccessMode::AutoAcceptEdits,
                    AccessMode::FullAccess,
                ],
                sandbox_levels: vec![
                    SandboxLevel::ReadOnly,
                    SandboxLevel::WorkspaceWrite,
                    SandboxLevel::Unrestricted,
                ],
                mcp_injection: true,
                structured_output: false,
                deterministic_turn_end: true,
                plan_mode: false,
                models: vec![ModelInfo {
                    id: "mock-1".into(),
                    display_name: Some("Mock 1".into()),
                }],
            },
        }
    }

    /// Replace the descriptor, to play an executor with other capabilities.
    #[must_use]
    pub fn with_descriptor(mut self, descriptor: ExecutorDescriptor) -> Self {
        self.descriptor = descriptor;
        self
    }

    /// Replace the script.
    #[must_use]
    pub fn with_steps(mut self, steps: impl IntoIterator<Item = MockStep>) -> Self {
        self.steps = steps.into_iter().collect();
        self
    }

    /// A snapshot of what this mock (and its clones) received so far.
    pub fn log(&self) -> MockLog {
        lock(&self.log).clone()
    }
}

fn lock(log: &Mutex<MockLog>) -> std::sync::MutexGuard<'_, MockLog> {
    log.lock().unwrap_or_else(PoisonError::into_inner)
}

#[async_trait]
impl Executor for MockExecutor {
    fn descriptor(&self) -> ExecutorDescriptor {
        self.descriptor.clone()
    }

    async fn probe(&self) -> ExecutorHealth {
        ExecutorHealth {
            installed: true,
            version: Some("mock".into()),
            signed_in: Some(true),
            detail: None,
        }
    }

    async fn start(
        &self,
        spec: ExecSpec,
        controls: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError> {
        require_access(spec.access, &self.descriptor.caps.access_modes)?;
        lock(&self.log).specs.push(spec);
        let run = Run {
            steps: self.steps.iter().cloned().collect(),
            controls,
            log: Arc::clone(&self.log),
            stashed_answers: Vec::new(),
            finished: false,
        };
        Ok(futures::stream::unfold(run, |mut run| async move {
            run.next_event().await.map(|event| (event, run))
        })
        .boxed())
    }
}

/// How a wait ended.
enum Waited {
    Got,
    Cancelled,
    /// The supervisor dropped its handle, so what was awaited can never arrive.
    Closed,
}

/// The state of one playing script.
struct Run {
    steps: VecDeque<MockStep>,
    controls: ExecControls,
    log: Arc<Mutex<MockLog>>,
    stashed_answers: Vec<ExecAnswer>,
    finished: bool,
}

impl Run {
    fn finish(&mut self, event: ExecEvent) -> Option<ExecEvent> {
        self.finished = true;
        Some(event)
    }

    fn done(&mut self, stop: StopReason, diagnostic: Option<String>) -> Option<ExecEvent> {
        self.finish(ExecEvent::Done {
            stop,
            result: None,
            diagnostic,
        })
    }

    async fn next_event(&mut self) -> Option<ExecEvent> {
        if self.finished {
            return None;
        }
        loop {
            if self.controls.cancel.is_cancelled() {
                return self.done(StopReason::Interrupted, None);
            }
            let Some(step) = self.steps.pop_front() else {
                return self.done(StopReason::Completed, None);
            };
            let waited = match step {
                MockStep::Emit(event) => {
                    if matches!(event, ExecEvent::Done { .. }) {
                        return self.finish(event);
                    }
                    return Some(event);
                }
                MockStep::Sleep(duration) => {
                    tokio::select! {
                        biased;
                        () = self.controls.cancel.cancelled() => Waited::Cancelled,
                        () = tokio::time::sleep(duration) => Waited::Got,
                    }
                }
                MockStep::AwaitAnswer { request_id } => self.wait_answer(&request_id).await,
                MockStep::AwaitSteer => self.wait_steer().await,
            };
            match waited {
                Waited::Got => {}
                Waited::Cancelled => return self.done(StopReason::Interrupted, None),
                Waited::Closed => {
                    return self.done(
                        StopReason::Error,
                        Some("run controls were dropped while the script was waiting".into()),
                    );
                }
            }
        }
    }

    async fn wait_answer(&mut self, request_id: &str) -> Waited {
        if let Some(at) = self
            .stashed_answers
            .iter()
            .position(|answer| answer.request_id == request_id)
        {
            self.stashed_answers.remove(at);
            return Waited::Got;
        }
        loop {
            let received = tokio::select! {
                biased;
                () = self.controls.cancel.cancelled() => return Waited::Cancelled,
                received = self.controls.answers.recv() => received,
            };
            let Some(answer) = received else {
                return Waited::Closed;
            };
            lock(&self.log).answers.push(answer.clone());
            if answer.request_id == request_id {
                return Waited::Got;
            }
            self.stashed_answers.push(answer);
        }
    }

    async fn wait_steer(&mut self) -> Waited {
        let received = tokio::select! {
            biased;
            () = self.controls.cancel.cancelled() => return Waited::Cancelled,
            received = self.controls.steering.recv() => received,
        };
        match received {
            Some(prompt) => {
                lock(&self.log).steers.push(prompt);
                Waited::Got
            }
            None => Waited::Closed,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::path::PathBuf;

    use reno_proto::{Question, RunId};

    use super::*;
    use crate::exec::{Answer, InputAnswer};

    fn id(s: &str) -> ExecutorId {
        s.parse().unwrap()
    }

    fn spec(access: AccessMode) -> ExecSpec {
        ExecSpec {
            run_id: RunId::from_suffix("01HZX9").unwrap(),
            prompt: "do it".into(),
            cwd: PathBuf::from("/tmp"),
            model: None,
            effort: None,
            access,
            sandbox: SandboxLevel::WorkspaceWrite,
            resume: None,
            mcp: Vec::new(),
            env: Default::default(),
            output_schema: None,
        }
    }

    fn needs_input(request_id: &str) -> ExecEvent {
        ExecEvent::NeedsInput {
            request_id: request_id.into(),
            questions: vec![Question {
                id: "q1".into(),
                prompt: "which?".into(),
                options: vec!["a".into(), "b".into()],
                multi_select: false,
            }],
        }
    }

    fn done(stop: StopReason) -> ExecEvent {
        ExecEvent::Done {
            stop,
            result: None,
            diagnostic: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn plays_steps_in_order_and_ends_with_one_done() {
        let mock = MockExecutor::new(id("mock")).with_steps([
            MockStep::started("s1", "mock-1"),
            MockStep::text("hello"),
            MockStep::Sleep(Duration::from_secs(5)),
            MockStep::text("world"),
        ]);
        let (controls, _handle) = ExecControls::new(4);
        let began = tokio::time::Instant::now();
        let events: Vec<_> = mock
            .start(spec(AccessMode::Supervised), controls)
            .await
            .unwrap()
            .collect()
            .await;
        assert_eq!(
            events,
            vec![
                ExecEvent::Started {
                    session: SessionRef("s1".into()),
                    model: "mock-1".into()
                },
                ExecEvent::Text {
                    text: "hello".into()
                },
                ExecEvent::Text {
                    text: "world".into()
                },
                done(StopReason::Completed),
            ]
        );
        // Paused time advances by exactly what the script slept.
        assert_eq!(began.elapsed(), Duration::from_secs(5));
        assert_eq!(mock.log().specs.len(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_scripted_done_ends_the_run_and_later_steps_never_play() {
        let mock = MockExecutor::new(id("mock")).with_steps([
            MockStep::text("a"),
            MockStep::Emit(ExecEvent::Done {
                stop: StopReason::Error,
                result: None,
                diagnostic: Some("boom".into()),
            }),
            MockStep::text("never"),
            MockStep::done(StopReason::Completed),
        ]);
        let (controls, _handle) = ExecControls::new(1);
        let events: Vec<_> = mock
            .start(spec(AccessMode::FullAccess), controls)
            .await
            .unwrap()
            .collect()
            .await;
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[1],
            ExecEvent::Done {
                stop: StopReason::Error,
                result: None,
                diagnostic: Some("boom".into())
            }
        );
    }

    #[tokio::test(start_paused = true)]
    async fn waits_for_the_answer_to_its_request_id_and_keeps_others() {
        let mock = MockExecutor::new(id("mock")).with_steps([
            MockStep::Emit(needs_input("r1")),
            MockStep::Emit(needs_input("r2")),
            MockStep::AwaitAnswer {
                request_id: "r1".into(),
            },
            MockStep::text("r1 answered"),
            MockStep::AwaitAnswer {
                request_id: "r2".into(),
            },
            MockStep::text("r2 answered"),
        ]);
        let (controls, handle) = ExecControls::new(4);
        let mut stream = mock
            .start(spec(AccessMode::Supervised), controls)
            .await
            .unwrap();
        assert!(matches!(
            stream.next().await,
            Some(ExecEvent::NeedsInput { .. })
        ));
        assert!(matches!(
            stream.next().await,
            Some(ExecEvent::NeedsInput { .. })
        ));
        // The answers arrive in the other order than the script awaits them.
        let answer = |label: &str| {
            vec![InputAnswer {
                question_id: "q1".into(),
                labels: vec![label.into()],
            }]
        };
        handle.answer_input("r2", answer("b")).await.unwrap();
        handle.answer_input("r1", answer("a")).await.unwrap();
        let rest: Vec<_> = stream.collect().await;
        assert_eq!(
            rest,
            vec![
                ExecEvent::Text {
                    text: "r1 answered".into()
                },
                ExecEvent::Text {
                    text: "r2 answered".into()
                },
                done(StopReason::Completed),
            ]
        );
        let ids: Vec<_> = mock
            .log()
            .answers
            .into_iter()
            .map(|a| (a.request_id, matches!(a.answer, Answer::Input(_))))
            .collect();
        assert_eq!(ids, vec![("r2".into(), true), ("r1".into(), true)]);
    }

    #[tokio::test(start_paused = true)]
    async fn waits_for_a_steer() {
        let mock = MockExecutor::new(id("mock")).with_steps([
            MockStep::text("working"),
            MockStep::AwaitSteer,
            MockStep::text("steered"),
        ]);
        let (controls, handle) = ExecControls::new(2);
        let mut stream = mock
            .start(spec(AccessMode::Supervised), controls)
            .await
            .unwrap();
        assert!(matches!(stream.next().await, Some(ExecEvent::Text { .. })));
        let pending = tokio::time::timeout(Duration::from_secs(60), stream.next()).await;
        assert!(pending.is_err(), "the run waits for the steer");
        handle.steer("go left").await.unwrap();
        let rest: Vec<_> = stream.collect().await;
        assert_eq!(rest.len(), 2);
        assert_eq!(mock.log().steers, vec!["go left".to_string()]);
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_during_a_sleep_ends_with_interrupted() {
        let mock = MockExecutor::new(id("mock")).with_steps([
            MockStep::text("a"),
            MockStep::Sleep(Duration::from_secs(3600)),
            MockStep::text("never"),
        ]);
        let (controls, handle) = ExecControls::new(1);
        let mut stream = mock
            .start(spec(AccessMode::Supervised), controls)
            .await
            .unwrap();
        assert!(matches!(stream.next().await, Some(ExecEvent::Text { .. })));
        let canceller = async {
            tokio::time::sleep(Duration::from_secs(1)).await;
            handle.cancel();
        };
        let (last, ()) = tokio::join!(stream.next(), canceller);
        assert_eq!(last, Some(done(StopReason::Interrupted)));
        assert_eq!(stream.next().await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn cancel_while_awaiting_an_answer_ends_with_interrupted() {
        let mock = MockExecutor::new(id("mock")).with_steps([
            MockStep::Emit(needs_input("r1")),
            MockStep::AwaitAnswer {
                request_id: "r1".into(),
            },
        ]);
        let (controls, handle) = ExecControls::new(1);
        let mut stream = mock
            .start(spec(AccessMode::Supervised), controls)
            .await
            .unwrap();
        stream.next().await;
        handle.cancel();
        assert_eq!(stream.next().await, Some(done(StopReason::Interrupted)));
        assert_eq!(stream.next().await, None);
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_handle_while_waiting_is_an_error_done() {
        let mock = MockExecutor::new(id("mock")).with_steps([MockStep::AwaitSteer]);
        let (controls, handle) = ExecControls::new(1);
        let stream = mock
            .start(spec(AccessMode::Supervised), controls)
            .await
            .unwrap();
        drop(handle);
        let events: Vec<_> = stream.collect().await;
        assert!(matches!(
            events.as_slice(),
            [ExecEvent::Done {
                stop: StopReason::Error,
                diagnostic: Some(_),
                ..
            }]
        ));
    }

    #[tokio::test]
    async fn an_unsupported_access_mode_is_refused_before_anything_runs() {
        let mut descriptor = MockExecutor::default_descriptor(id("mock"));
        descriptor.caps.access_modes = vec![AccessMode::Supervised];
        let mock = MockExecutor::new(id("mock"))
            .with_descriptor(descriptor)
            .with_steps([MockStep::text("never")]);
        let (controls, _handle) = ExecControls::new(1);
        let Err(error) = mock.start(spec(AccessMode::FullAccess), controls).await else {
            panic!("full access must be refused");
        };
        assert!(matches!(
            error,
            ExecError::UnsupportedAccess {
                requested: AccessMode::FullAccess,
                ref supported,
            } if supported == &[AccessMode::Supervised]
        ));
        assert!(mock.log().specs.is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn two_runs_of_one_script_are_identical() {
        let mock = MockExecutor::new(id("mock")).with_steps([
            MockStep::started("s", "mock-1"),
            MockStep::Sleep(Duration::from_millis(250)),
            MockStep::text("x"),
        ]);
        let mut runs = Vec::new();
        for _ in 0..2 {
            let (controls, _handle) = ExecControls::new(1);
            let events: Vec<_> = mock
                .start(spec(AccessMode::Supervised), controls)
                .await
                .unwrap()
                .collect()
                .await;
            runs.push(events);
        }
        assert_eq!(runs[0], runs[1]);
    }

    #[tokio::test]
    async fn probe_reports_a_healthy_mock_and_the_trait_is_object_safe() {
        let mock: std::sync::Arc<dyn Executor> = std::sync::Arc::new(MockExecutor::new(id("mock")));
        let health = mock.probe().await;
        assert!(health.installed);
        assert_eq!(health.signed_in, Some(true));
        assert_eq!(mock.descriptor().id, id("mock"));
    }
}
