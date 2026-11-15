//! The kernel's port to harnesses (ARCH-7, ARCHITECTURE section 4.2).
//!
//! [`Executor`] is the one trait the kernel sees. Two things implement it today:
//!
//! - [`HarnessExecutor`] wraps an imported [`crate::Harness`] (stage A, ARCHITECTURE section 2.1).
//!   After stage B each driver implements [`Executor`] directly and the shim goes away.
//! - [`MockExecutor`] plays a scripted run for kernel tests and the CLI's `--executor mock`.
//!
//! # The stream contract
//!
//! [`Executor::start`] returns a stream of `ExecEvent`s that always ends with exactly one
//! `ExecEvent::Done`. A harness that fails yields `Done { stop: Error, diagnostic }`; the stream
//! never carries an error. [`ExecError`] is reserved for what can be known before a run exists:
//! the executor is ineligible for the request (not installed, not signed in, cannot honour the
//! access mode), so the router can pick another one (ARCH-15).
//!
//! # Controls
//!
//! The kernel creates a pair with [`ExecControls::new`]: the [`ExecControls`] half goes to
//! `start`, the [`ExecHandle`] half stays with the run supervisor. Every channel is bounded
//! (ARCH-11).

#![deny(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use futures::stream::BoxStream;
use reno_proto::{AccessMode, ExecEvent, ExecSpec, ExecutorDescriptor};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

mod mock;
mod shim;

pub use mock::{MockExecutor, MockLog, MockStep};
pub use shim::{DROPPED_AGENT_EVENTS, HarnessExecutor};

/// A harness run, as the kernel sees it. Object safe: the kernel holds `Arc<dyn Executor>`.
#[async_trait]
pub trait Executor: Send + Sync {
    /// What this executor declares it can do. Nothing is trusted beyond it (ARCH-15).
    fn descriptor(&self) -> ExecutorDescriptor;

    /// Whether the harness is usable now. Never fails: a probe that cannot tell reports `None`
    /// for that field.
    async fn probe(&self) -> ExecutorHealth;

    /// Start one run.
    ///
    /// Returns [`ExecError`] only when the executor is ineligible for `spec`, before anything
    /// was spawned. Otherwise the stream ends with exactly one `ExecEvent::Done`, also when the
    /// harness crashes, fails to launch or is cancelled through `controls`.
    async fn start(
        &self,
        spec: ExecSpec,
        controls: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError>;
}

/// What [`Executor::probe`] found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecutorHealth {
    /// The harness CLI is present on this machine.
    pub installed: bool,
    /// The CLI's version, when it can be read.
    pub version: Option<String>,
    /// Whether the harness is signed in. `None` when it cannot be known without starting a run.
    pub signed_in: Option<bool>,
    /// Anything worth showing a person: why `installed` is false, for example.
    pub detail: Option<String>,
}

/// Why [`Executor::start`] refused a run. Every variant means nothing was spawned.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    /// The executor cannot honour the requested access mode, and never runs with more than asked
    /// (ARCH-15).
    #[error("access mode {requested:?} is not supported; supported: {supported:?}")]
    UnsupportedAccess {
        /// What the spec asked for.
        requested: AccessMode,
        /// What the executor declares.
        supported: Vec<AccessMode>,
    },
    /// The harness CLI is not installed.
    #[error("harness is not installed: {0}")]
    NotInstalled(String),
    /// The harness has no login.
    #[error("harness is not signed in: {0}")]
    NotSignedIn(String),
    /// The spec asks for structured output, which this executor cannot guarantee. Running without
    /// the schema would silently weaken the request.
    #[error("structured output (`output_schema`) is not supported by this executor")]
    UnsupportedOutputSchema,
    /// The spec cannot be expressed to this executor (a working directory that is not UTF-8, for
    /// example).
    #[error("invalid exec spec: {0}")]
    InvalidSpec(String),
}

/// A decision on a `NeedsApproval` request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Let the action proceed.
    Allow,
    /// Refuse it.
    Deny,
}

/// One answer to one question of a `NeedsInput` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputAnswer {
    /// The `Question::id` this answers.
    pub question_id: String,
    /// The chosen options, or the free text as the only label.
    pub labels: Vec<String>,
}

/// The payload of an [`ExecAnswer`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Answers to a `NeedsInput` request. An empty list declines it.
    Input(Vec<InputAnswer>),
    /// The decision on a `NeedsApproval` request.
    Decision(Decision),
}

/// An answer to a request the run raised, matched by `request_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecAnswer {
    /// The `request_id` of the `NeedsInput` or `NeedsApproval` event.
    pub request_id: String,
    /// The answer.
    pub answer: Answer,
}

/// The executor's half of a run's controls (ARCHITECTURE section 5.1: kept from the imported
/// code, now owned by Reno).
#[derive(Debug)]
pub struct ExecControls {
    /// Prompts to deliver at the harness's steering boundary. Bounded.
    pub steering: mpsc::Receiver<String>,
    /// Cancel to stop the run. The stream then ends with `Done { stop: Interrupted }`.
    pub cancel: CancellationToken,
    /// Answers to `NeedsInput` and `NeedsApproval` requests. Bounded. A request still unanswered
    /// when `cancel` fires is denied.
    pub answers: mpsc::Receiver<ExecAnswer>,
}

/// The run supervisor's half of a run's controls.
#[derive(Debug, Clone)]
pub struct ExecHandle {
    steering: mpsc::Sender<String>,
    cancel: CancellationToken,
    answers: mpsc::Sender<ExecAnswer>,
}

/// The run has ended or dropped its controls.
#[derive(Debug, thiserror::Error)]
#[error("the run no longer accepts controls")]
pub struct ControlsClosed;

/// Why [`ExecHandle::try_steer`] did not queue the prompt.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SteerRejected {
    /// The steering queue is full; try again later.
    #[error("steering queue is full")]
    Full,
    /// The run no longer accepts controls.
    #[error("the run no longer accepts controls")]
    Closed,
}

impl ExecControls {
    /// A controls pair whose steering and answer queues each hold `capacity` items (at least 1).
    pub fn new(capacity: usize) -> (Self, ExecHandle) {
        Self::with_cancel(capacity, CancellationToken::new())
    }

    /// Like [`Self::new`], cancelled through `cancel`: pass a child of the run's token so
    /// shutdown reaches the executor (ARCH-11).
    pub fn with_cancel(capacity: usize, cancel: CancellationToken) -> (Self, ExecHandle) {
        let capacity = capacity.max(1);
        let (steer_tx, steer_rx) = mpsc::channel(capacity);
        let (answer_tx, answer_rx) = mpsc::channel(capacity);
        (
            Self {
                steering: steer_rx,
                cancel: cancel.clone(),
                answers: answer_rx,
            },
            ExecHandle {
                steering: steer_tx,
                cancel,
                answers: answer_tx,
            },
        )
    }
}

impl ExecHandle {
    /// Queue a steering prompt, waiting for room.
    pub async fn steer(&self, prompt: impl Into<String>) -> Result<(), ControlsClosed> {
        self.steering
            .send(prompt.into())
            .await
            .map_err(|_| ControlsClosed)
    }

    /// Queue a steering prompt without waiting.
    pub fn try_steer(&self, prompt: impl Into<String>) -> Result<(), SteerRejected> {
        self.steering.try_send(prompt.into()).map_err(|e| match e {
            mpsc::error::TrySendError::Full(_) => SteerRejected::Full,
            mpsc::error::TrySendError::Closed(_) => SteerRejected::Closed,
        })
    }

    /// Answer a `NeedsInput` request. An empty `answers` declines it.
    pub async fn answer_input(
        &self,
        request_id: impl Into<String>,
        answers: Vec<InputAnswer>,
    ) -> Result<(), ControlsClosed> {
        self.send_answer(request_id.into(), Answer::Input(answers))
            .await
    }

    /// Decide a `NeedsApproval` request.
    pub async fn decide(
        &self,
        request_id: impl Into<String>,
        decision: Decision,
    ) -> Result<(), ControlsClosed> {
        self.send_answer(request_id.into(), Answer::Decision(decision))
            .await
    }

    /// Stop the run.
    pub fn cancel(&self) {
        self.cancel.cancel();
    }

    /// The token this handle cancels.
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel
    }

    async fn send_answer(&self, request_id: String, answer: Answer) -> Result<(), ControlsClosed> {
        self.answers
            .send(ExecAnswer { request_id, answer })
            .await
            .map_err(|_| ControlsClosed)
    }
}

/// Fail with [`ExecError::UnsupportedAccess`] unless `supported` lists `requested` (ARCH-15).
pub(crate) fn require_access(
    requested: AccessMode,
    supported: &[AccessMode],
) -> Result<(), ExecError> {
    if supported.contains(&requested) {
        Ok(())
    } else {
        Err(ExecError::UnsupportedAccess {
            requested,
            supported: supported.to_vec(),
        })
    }
}
