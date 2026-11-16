//! The kernel's port to harnesses (ARCH-7, ARCHITECTURE section 4.2).
//!
//! [`Executor`] is the one trait the kernel sees. These implement it:
//!
//! - [`crate::ClaudeExecutor`] speaks Claude Code's wire directly and emits `ExecEvent`s, with
//!   structured approvals.
//! - [`crate::CodexExecutor`] speaks the `codex app-server` JSON-RPC.
//! - [`crate::AcpExecutor`] is one instance per ACP agent (Grok, Devin, Hermes,
//!   Antigravity, the agents of the ACP registry).
//! - [`MockExecutor`] plays a scripted run for kernel tests and the CLI's `--executor mock`.
//!
//! # What a native driver plugs into
//!
//! Everything a driver shares is here, so a new driver follows without changing the contract:
//! [`Executor`], [`ExecControls`] (steering, cancel, answers, process records),
//! [`require_access`] (fail closed before spawning, ARCH-15), and [`OpenRequests`], the table
//! that keeps approvals and input requests apart, never lets a wrong-kind answer approve, and
//! hands back every open request to be refused when the run ends.
//!
//! # The stream contract
//!
//! [`Executor::start`] returns a stream of `ExecEvent`s that always ends with exactly one
//! `ExecEvent::Done`. A harness that fails yields `Done { stop: Error, diagnostic }`; the stream
//! never carries an error. [`ExecError`] is reserved for what can be known before a run exists:
//! the executor is ineligible for the request (not installed, not signed in, cannot honour the
//! access mode), so the router can pick another one (ARCH-15).
//!
//! # Discovery
//!
//! [`Executor::probe`] finds out, without a model call, whether the harness is installed, at
//! which version, whether that version is inside the range the adapter was verified on, whether
//! it is signed in, and which models it offers (`HealthProbe`). A fact a driver cannot establish
//! is reported as unknown, never guessed: sign-in per driver is Claude's `claude auth status`
//! (or a credential in the environment), Codex's `account/read`, an ACP registry agent's
//! `session/new`, and unknown for the other ACP agents. [`Executor::install`] is the one
//! explicit install step (a managed npm adapter or a pinned release archive); `start` never
//! installs, and a harness that needs it is reported as `NeedsInstall`.
//!
//! # Controls
//!
//! The kernel creates a pair with [`ExecControls::new`]: the [`ExecControls`] half goes to
//! `start`, the [`ExecHandle`] half stays with the run supervisor. Every channel is bounded
//! (ARCH-11).
//!
//! # Process records
//!
//! An executor that spawns a child reports it through [`ExecControls::processes`] (unix only,
//! like `ProcessRecord` itself). The supervisor takes the receiving end once, with
//! [`ExecHandle::take_process_records`], and stores each record so the next start can sweep the
//! orphan (ARCHITECTURE 17.3).

#![deny(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use async_trait::async_trait;
use futures::stream::BoxStream;
use reno_proto::{AccessMode, ExecEvent, ExecSpec, ExecutorDescriptor, HealthProbe};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

#[cfg(unix)]
use crate::proc::ProcessRecord;

mod mock;
pub(crate) mod probe;
mod requests;

pub use mock::{MockExecutor, MockLog, MockStep};
pub use requests::{Open, OpenRequests, Resolved};

/// A harness run, as the kernel sees it. Object safe: the kernel holds `Arc<dyn Executor>`.
#[async_trait]
pub trait Executor: Send + Sync {
    /// What this executor declares it can do. Nothing is trusted beyond it (ARCH-15).
    fn descriptor(&self) -> ExecutorDescriptor;

    /// Finds out whether the harness is usable now, without running a model: where it is
    /// installed and at which version, whether that version is inside the range the adapter was
    /// verified on, whether it is signed in, and which models it offers. Never fails: what
    /// cannot be told is reported as such (`SignedIn::Unknown`, `VersionGate::Unchecked`), and a
    /// probe that cannot finish says `NotInstalled { ProbeFailed }`.
    ///
    /// Bounded in time and output, spawns nothing it does not reap, and installs nothing: a
    /// harness that needs an install says so (`NeedsInstall`) and waits for [`Self::install`].
    async fn probe(&self) -> HealthProbe;

    /// The explicit install step for a harness whose probe said `NeedsInstall`: fetches and
    /// verifies the managed adapter. Never called by a run. The default has nothing to install.
    async fn install(&self) -> Result<(), crate::HarnessError> {
        Err(crate::HarnessError::Install(
            "this executor has nothing to install".into(),
        ))
    }

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

    /// Wait for the tasks this executor spawned (ARCH-11). Call at shutdown, after the runs'
    /// streams ended or were dropped: each task stops with its stream. The default has nothing
    /// to wait for.
    async fn shutdown(&self) {}
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
    /// The decision on a `NeedsApproval` request, with the reason a refusal carries (a policy
    /// rule's deny message). Each driver passes the reason on where its wire has a place for it.
    Decision(Decision, Option<String>),
}

/// An answer to a request the run raised, matched by `request_id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecAnswer {
    /// The `request_id` of the `NeedsInput` or `NeedsApproval` event.
    pub request_id: String,
    /// The answer.
    pub answer: Answer,
}

/// The executor's half of a run's controls (ARCHITECTURE section 5.1).
#[derive(Debug)]
pub struct ExecControls {
    /// Prompts to deliver at the harness's steering boundary. Bounded.
    pub steering: mpsc::Receiver<String>,
    /// Cancel to stop the run. The stream then ends with `Done { stop: Interrupted }`.
    pub cancel: CancellationToken,
    /// Answers to `NeedsInput` and `NeedsApproval` requests. Bounded. A request still unanswered
    /// when `cancel` fires is denied.
    pub answers: mpsc::Receiver<ExecAnswer>,
    /// Where to report each child process the run spawns. Bounded.
    #[cfg(unix)]
    pub processes: ProcessSink,
}

/// The executor's end of the process-record queue: report every child as soon as it is spawned.
#[cfg(unix)]
#[derive(Debug, Clone)]
pub struct ProcessSink {
    records: mpsc::Sender<ProcessRecord>,
}

#[cfg(unix)]
impl ProcessSink {
    /// Report a spawned child, waiting for room. `Err` means the supervisor no longer listens;
    /// the run goes on.
    pub async fn report(&self, record: ProcessRecord) -> Result<(), ControlsClosed> {
        self.records.send(record).await.map_err(|_| ControlsClosed)
    }

    /// Report a spawned child without waiting, for callers that cannot `.await`. The record is
    /// handed back when the queue is full or closed.
    pub fn try_report(
        &self,
        record: ProcessRecord,
    ) -> Result<(), mpsc::error::TrySendError<ProcessRecord>> {
        self.records.try_send(record)
    }
}

/// The run supervisor's half of a run's controls.
#[derive(Debug, Clone)]
pub struct ExecHandle {
    steering: mpsc::Sender<String>,
    cancel: CancellationToken,
    answers: mpsc::Sender<ExecAnswer>,
    /// The receiving end of the process-record queue, taken once.
    #[cfg(unix)]
    records: std::sync::Arc<std::sync::Mutex<Option<mpsc::Receiver<ProcessRecord>>>>,
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
        #[cfg(unix)]
        let (record_tx, record_rx) = mpsc::channel(capacity);
        (
            Self {
                steering: steer_rx,
                cancel: cancel.clone(),
                answers: answer_rx,
                #[cfg(unix)]
                processes: ProcessSink { records: record_tx },
            },
            ExecHandle {
                steering: steer_tx,
                cancel,
                answers: answer_tx,
                #[cfg(unix)]
                records: std::sync::Arc::new(std::sync::Mutex::new(Some(record_rx))),
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
        self.decide_with_message(request_id, decision, None).await
    }

    /// [`Self::decide`], with the reason for the decision: the message of the policy rule that
    /// denied it. The driver tells the agent where its wire allows (Claude's permission
    /// response, ACP's `_meta`); Codex's approval reply has no field for it.
    pub async fn decide_with_message(
        &self,
        request_id: impl Into<String>,
        decision: Decision,
        message: Option<String>,
    ) -> Result<(), ControlsClosed> {
        self.send_answer(request_id.into(), Answer::Decision(decision, message))
            .await
    }

    /// The queue of process records the executor reports, once: the first call returns it and
    /// later calls (on this handle or a clone) return `None`. It closes when the executor
    /// drops its controls.
    #[cfg(unix)]
    pub fn take_process_records(&self) -> Option<mpsc::Receiver<ProcessRecord>> {
        self.records
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
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
