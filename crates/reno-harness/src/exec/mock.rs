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
    AccessMode, ExecEvent, ExecSpec, ExecutorCaps, ExecutorDescriptor, ExecutorId, HealthProbe,
    InstallStatus, ModelCatalog, ModelInfo, SandboxLevel, SessionRef, SignedIn, Steering,
    StopReason, VersionGate, VersionRange,
};

use super::{ExecAnswer, ExecControls, ExecError, Executor, require_access, require_sandbox};
#[cfg(unix)]
use crate::proc::ProcessRecord;

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
    /// Wait until the run's controls carry a model change request, answer it (the model itself
    /// when the descriptor lists it or has it as an alias, else a refusal, as the harness does)
    /// and, when it took, emit `ExecEvent::ModelChanged` for it. An alias resolves to the
    /// first listed model.
    AwaitModel,
    /// Report a spawned child on the run's process-record queue, as a real executor does right
    /// after it spawns. Unix only, like `ProcessRecord`.
    #[cfg(unix)]
    Spawned(ProcessRecord),
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
            reason: None,
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
    /// Every model the run was asked to switch to, in order, accepted or not.
    pub model_requests: Vec<String>,
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
    /// Whether `log` is written. A long-lived daemon turns it off: the log grows with every run.
    recording: bool,
    /// Where `plan_usage` reads from; none by default.
    usage: Option<Arc<dyn crate::usage::UsageSource>>,
}

impl MockExecutor {
    /// A mock named `id`, with [`Self::default_descriptor`] and an empty script (which completes
    /// at once).
    pub fn new(id: ExecutorId) -> Self {
        Self {
            descriptor: Self::default_descriptor(id),
            steps: Vec::new(),
            log: Arc::default(),
            recording: true,
            usage: None,
        }
    }

    /// Gives `plan_usage` a source to read.
    pub fn with_usage_source(mut self, source: Arc<dyn crate::usage::UsageSource>) -> Self {
        self.usage = Some(source);
        self
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
                    options: Vec::new(),
                    default_effort: None,
                    efforts: vec![],
                    id: "mock-1".into(),
                    display_name: Some("Mock 1".into()),
                    description: None,
                }],
                model_switch: true,
                isolates_user_config: false,
                model_aliases: vec![],
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

    /// Stop recording what the mock receives: [`Self::log`] stays empty. For a mock that lives in
    /// a daemon, where the log would hold every run's spec for as long as the daemon runs.
    #[must_use]
    pub fn without_log(mut self) -> Self {
        self.recording = false;
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

    /// Always installed and signed in: the mock is the daemon's own script, nothing to find.
    async fn plan_usage(&self) -> Option<crate::usage::UsageReading> {
        self.usage.as_ref()?.read().await
    }

    async fn probe(&self) -> HealthProbe {
        HealthProbe {
            installed: InstallStatus::Installed {
                path: std::path::PathBuf::from("mock"),
                version: Some("mock".into()),
            },
            version_gate: VersionGate::Unchecked,
            signed_in: SignedIn::Yes { account: None },
            models: ModelCatalog::curated(self.descriptor.caps.models.clone()),
        }
    }

    async fn start(
        &self,
        spec: ExecSpec,
        controls: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError> {
        require_access(spec.access, &self.descriptor.caps.access_modes)?;
        require_sandbox(spec.sandbox, &self.descriptor.caps.sandbox_levels)?;
        if spec.output_schema.is_some() && !self.descriptor.caps.structured_output {
            return Err(ExecError::UnsupportedOutputSchema);
        }
        if self.recording {
            lock(&self.log).specs.push(spec);
        }
        let run = Run {
            descriptor: self.descriptor.clone(),
            steps: self.steps.iter().cloned().collect(),
            controls,
            log: Arc::clone(&self.log),
            recording: self.recording,
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
    descriptor: ExecutorDescriptor,
    steps: VecDeque<MockStep>,
    controls: ExecControls,
    log: Arc<Mutex<MockLog>>,
    recording: bool,
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
            reason: None,
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
                MockStep::AwaitModel => match self.wait_model().await {
                    (Waited::Got, Some(event)) => return Some(event),
                    (waited, _) => waited,
                },
                #[cfg(unix)]
                MockStep::Spawned(record) => self.report_process(record).await,
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
            if self.recording {
                lock(&self.log).answers.push(answer.clone());
            }
            if answer.request_id == request_id {
                return Waited::Got;
            }
            self.stashed_answers.push(answer);
        }
    }

    /// A supervisor that stopped listening does not end the run: the report is simply lost.
    #[cfg(unix)]
    async fn report_process(&mut self, record: ProcessRecord) -> Waited {
        tokio::select! {
            biased;
            () = self.controls.cancel.cancelled() => Waited::Cancelled,
            _ = self.controls.processes.report(record) => Waited::Got,
        }
    }

    /// What the descriptor makes of a requested model: itself when listed, the first listed
    /// model for an alias, else the harness's refusal.
    fn resolve_model(&self, model: &str) -> Result<String, String> {
        let caps = &self.descriptor.caps;
        let base = caps.base_model(model);
        if caps.models.iter().any(|known| known.id == base) {
            return Ok(base.to_owned());
        }
        match caps.models.first() {
            Some(first) if caps.is_model_alias(model) => Ok(first.id.clone()),
            _ => Err(format!("Model '{model}' not found")),
        }
    }

    async fn wait_model(&mut self) -> (Waited, Option<ExecEvent>) {
        let received = tokio::select! {
            biased;
            () = self.controls.cancel.cancelled() => return (Waited::Cancelled, None),
            received = self.controls.models.recv() => received,
        };
        let Some(request) = received else {
            return (Waited::Closed, None);
        };
        if self.recording {
            lock(&self.log).model_requests.push(request.model.clone());
        }
        let resolved = self.resolve_model(&request.model);
        let event = resolved
            .clone()
            .ok()
            .map(|model| ExecEvent::ModelChanged { model });
        let _ = request.reply.send(resolved);
        (Waited::Got, event)
    }

    async fn wait_steer(&mut self) -> Waited {
        let received = tokio::select! {
            biased;
            () = self.controls.cancel.cancelled() => return Waited::Cancelled,
            received = self.controls.steering.recv() => received,
        };
        match received {
            Some(prompt) => {
                if self.recording {
                    lock(&self.log).steers.push(prompt);
                }
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
            model_options: Default::default(),
            run_id: RunId::from_suffix("01HZX9").unwrap(),
            inherit_user_config: false,
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
            instructions: None,
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
            reason: None,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_mock_without_a_log_keeps_nothing_across_runs() {
        // A daemon's mock plays every run for as long as the daemon lives: its log must not
        // grow with them (it held a copy of each run's spec, a leak of a few KiB per run).
        let mock = MockExecutor::new(id("mock"))
            .with_steps([MockStep::started("s1", "mock-1"), MockStep::text("hi")])
            .without_log();
        for _ in 0..50 {
            let (controls, _handle) = ExecControls::new(4);
            let events: Vec<_> = mock
                .start(spec(AccessMode::Supervised), controls)
                .await
                .unwrap()
                .collect()
                .await;
            assert_eq!(events.len(), 3, "the script still plays");
        }
        assert_eq!(mock.log(), MockLog::default());
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
                reason: None,
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
                diagnostic: Some("boom".into()),
                reason: None,
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

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_model_request_is_answered_and_announced_or_refused() {
        let mut descriptor = MockExecutor::default_descriptor(id("mock"));
        descriptor.caps.model_aliases = vec!["small".into()];
        let mock = MockExecutor::new(id("mock"))
            .with_descriptor(descriptor)
            .with_steps([
                MockStep::AwaitModel,
                MockStep::AwaitModel,
                MockStep::AwaitModel,
                MockStep::text("after"),
            ]);
        let (controls, handle) = ExecControls::new(4);
        let mut stream = mock
            .start(spec(AccessMode::Supervised), controls)
            .await
            .unwrap();
        let refused = handle.try_set_model("nope").unwrap();
        let alias = handle.try_set_model("small").unwrap();
        let listed = handle.try_set_model("mock-1").unwrap();
        assert_eq!(
            stream.next().await,
            Some(ExecEvent::ModelChanged {
                model: "mock-1".into()
            })
        );
        assert_eq!(
            stream.next().await,
            Some(ExecEvent::ModelChanged {
                model: "mock-1".into()
            })
        );
        assert_eq!(
            stream.next().await,
            Some(ExecEvent::Text {
                text: "after".into()
            })
        );
        assert_eq!(
            refused.await.unwrap(),
            Err("Model 'nope' not found".to_owned())
        );
        assert_eq!(alias.await.unwrap(), Ok("mock-1".to_owned()));
        assert_eq!(listed.await.unwrap(), Ok("mock-1".to_owned()));
        assert_eq!(mock.log().model_requests, ["nope", "small", "mock-1"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_spawned_step_reports_the_record_once_on_the_handle() {
        let record = ProcessRecord {
            pid: 4242,
            pgid: 4242,
            start_time: 9,
        };
        let mock = MockExecutor::new(id("mock"))
            .with_steps([MockStep::Spawned(record), MockStep::text("go")]);
        let (controls, handle) = ExecControls::new(4);
        let mut records = handle.take_process_records().unwrap();
        assert!(
            handle.clone().take_process_records().is_none(),
            "taken once"
        );
        let events: Vec<_> = mock
            .start(spec(AccessMode::Supervised), controls)
            .await
            .unwrap()
            .collect()
            .await;
        assert_eq!(events.len(), 2);
        assert_eq!(records.recv().await, Some(record));
        assert_eq!(records.recv().await, None, "closed with the controls");
    }

    #[tokio::test]
    async fn probe_reports_a_healthy_mock_and_the_trait_is_object_safe() {
        let mock: std::sync::Arc<dyn Executor> = std::sync::Arc::new(MockExecutor::new(id("mock")));
        let health = mock.probe().await;
        assert!(matches!(health.installed, InstallStatus::Installed { .. }));
        assert_eq!(health.signed_in, SignedIn::Yes { account: None });
        assert_eq!(health.ineligible(), None);
        assert_eq!(mock.descriptor().id, id("mock"));
    }
}
