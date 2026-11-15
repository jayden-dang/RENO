//! [`HarnessExecutor`]: the stage A shim that lets any imported [`Harness`] be used as an
//! [`Executor`] (ARCHITECTURE sections 2.1 and 5.1).
//!
//! The imported drivers are not rewritten. The shim translates at the seam and nowhere else:
//!
//! - `ExecSpec` becomes a `RunRequest` ([`HarnessExecutor::start`]).
//! - `AgentEvent` becomes `ExecEvent` ([`DROPPED_AGENT_EVENTS`] lists what is dropped).
//! - `ExecControls` becomes `RunControls`: steering prompts are forwarded, the cancel token is
//!   the harness's `interrupt` token, and answers resolve the `request_input` one-shots.
//!
//! # Event mapping
//!
//! | `AgentEvent` | `ExecEvent` |
//! |---|---|
//! | `SessionStarted` | `Started { session: session_id, model }` |
//! | `TextDelta` | `Text` |
//! | `ReasoningDelta` | `Reasoning` |
//! | `ToolCall` | `Tool { id, call: { name, input } }` (see [`tool_call`]) |
//! | `ToolResult` | `ToolResult { id, ok: !is_error, summary }` (last output line, exit code) |
//! | `Usage` | `Usage` |
//! | `InputRequested` | `NeedsInput` |
//! | a `request_input` call | `NeedsInput` (the real path: drivers do not emit `InputRequested`) |
//! | `Error` | folded into the following `Done`'s `diagnostic` |
//! | `Done` | `Done { stop, result, diagnostic }` |
//! | a stream `Err`, or a stream ending without `Done` | `Done { stop: Error, diagnostic }` |
//!
//! `DoneStatus::Completed` maps to `StopReason::Completed`, `Interrupted` to `Interrupted` and
//! `Errored` to `Error`. The imported wire has no refusal or max-tokens status, so the shim never
//! produces `Refusal` or `MaxTokens`.
//!
//! Everything else is dropped; the list is [`DROPPED_AGENT_EVENTS`], and the mapping function
//! matches every variant, so a new `AgentEvent` variant fails to compile until it is placed.
//!
//! # Guarantees
//!
//! - The stream ends with exactly one `Done`. Events after the first `Done` are discarded.
//! - Requests unanswered when the cancel token fires, or when the stream ends or is dropped, are
//!   declined (an empty answer list, which every driver treats as a refusal).
//! - Steering and answer queues are bounded; every task the shim spawns belongs to the
//!   executor's `TaskTracker` and ends with the output stream (ARCH-11).
//! - A `Done` is the end of the run. The imported `Harness::run` is a persistent session that
//!   outlives a turn, so the shim ends it: at `Done` it drops the harness's stream, which every
//!   driver reads as the consumer leaving (its `event_tx.closed()` arm) and answers by tearing
//!   down its child and the child's process group. A steering prompt still queued at that moment
//!   is lost.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use reno_proto::{
    AccessMode, Effort, ExecEvent, ExecSpec, ExecutorCaps, ExecutorDescriptor, ExecutorId,
    ModelInfo, Question, SandboxLevel, SessionRef, Steering, StopReason, ToolCall, VersionRange,
};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::{CancellationToken, DropGuard};
use tokio_util::task::TaskTracker;

use super::{
    Answer, ExecAnswer, ExecControls, ExecError, Executor, ExecutorHealth, require_access,
};
use crate::upstream_proto::{
    self as up, AgentEvent, DoneStatus, UserInputAnswer, UserInputQuestion,
};
use crate::{Harness, HarnessError, RunControls, SteerMessage, TurnControl};

/// The contract version this shim implements (`ExecutorDescriptor::contract_version`).
const CONTRACT_VERSION: u32 = 1;
/// Steering prompts queued between the supervisor and the harness.
const STEER_CAPACITY: usize = 16;
/// Requests the harness may have raised but the consumer not yet read. A request that finds the
/// queue full is declined, never dropped silently into a wait.
const REQUEST_CAPACITY: usize = 64;
/// The longest tool-result summary, in characters.
const SUMMARY_CHARS: usize = 200;

/// The `AgentEvent` variants the shim drops, by name. They are UI concerns of the imported chat
/// client (ARCHITECTURE section 5.1: "Reno is not a chat client") or are carried elsewhere.
///
/// - `GeneratedImage`, `Visual`: inline media for a chat transcript.
/// - `AssistantMessageCompleted`, `Steered`: the imported chat's message segmentation.
/// - `ContextUsage`, `PlanUsage`: context-window and subscription meters. `Usage` is kept.
/// - `AvailableCommands`: slash-command catalog for a composer popup.
/// - `InputResolved`: the engine's echo of an answer; the kernel already knows what it sent.
/// - `UserMessage`, `Subagent`: a subagent's nested transcript. Its spawn `ToolCall` and result
///   on the parent feed are kept; the child's own events are not.
pub const DROPPED_AGENT_EVENTS: &[&str] = &[
    "GeneratedImage",
    "Visual",
    "AssistantMessageCompleted",
    "ContextUsage",
    "PlanUsage",
    "AvailableCommands",
    "InputResolved",
    "Steered",
    "UserMessage",
    "Subagent",
];

/// An imported [`Harness`] presented as an [`Executor`].
///
/// # Approvals are `NeedsInput` in stage A
///
/// The imported drivers raise an approval as a prose yes/no `UserInputQuestion` through
/// `request_input`, indistinguishable from an agent's own question. The shim maps every such
/// request to `ExecEvent::NeedsInput` carrying that question. It does **not** parse the prose
/// into `NeedsApproval`: Reno never reads prose to make a security decision. Structured approvals
/// (`NeedsApproval` with an `ApprovalSubject`, answered by `ExecHandle::decide`) arrive with the
/// native contract in stage B (T1.0). A `Decision` sent for a shim request is ignored with a
/// warning; the request stays open for an `Answer::Input`.
///
/// # Declared capabilities
///
/// The shim declares what the wrapped harness reports (`access_modes`, `supports_plan`,
/// steering, `deterministic_turn_end`) and, by default, no sandbox levels: the imported drivers
/// derive their sandbox from the access mode and do not take an independent one. Override with
/// the `with_*` methods.
pub struct HarnessExecutor {
    harness: Arc<dyn Harness>,
    id: ExecutorId,
    adapter_version: String,
    cli_verified: VersionRange,
    sandbox_levels: Vec<SandboxLevel>,
    models: Vec<ModelInfo>,
    tracker: TaskTracker,
}

impl std::fmt::Debug for HarnessExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HarnessExecutor")
            .field("id", &self.id)
            .field("harness", &self.harness.display_name())
            .finish_non_exhaustive()
    }
}

impl HarnessExecutor {
    /// Wrap `harness` as the executor `id`.
    pub fn new(harness: impl Harness + 'static, id: ExecutorId) -> Self {
        Self::from_arc(Arc::new(harness), id)
    }

    /// Wrap a shared harness, for example one the caller also keeps for catalog queries.
    pub fn from_arc(harness: Arc<dyn Harness>, id: ExecutorId) -> Self {
        Self {
            harness,
            id,
            adapter_version: format!("reno-harness {} (stage A shim)", env!("CARGO_PKG_VERSION")),
            cli_verified: VersionRange {
                min: "0".into(),
                verified: "unverified".into(),
            },
            sandbox_levels: Vec::new(),
            models: Vec::new(),
            tracker: TaskTracker::new(),
        }
    }

    /// Declare the harness CLI versions the conformance suite covers.
    #[must_use]
    pub fn with_cli_verified(mut self, cli_verified: VersionRange) -> Self {
        self.cli_verified = cli_verified;
        self
    }

    /// Declare the sandbox levels the wrapped harness enforces.
    #[must_use]
    pub fn with_sandbox_levels(mut self, levels: Vec<SandboxLevel>) -> Self {
        self.sandbox_levels = levels;
        self
    }

    /// Declare the models on offer (the descriptor is synchronous, so it cannot ask the harness).
    #[must_use]
    pub fn with_models(mut self, models: Vec<ModelInfo>) -> Self {
        self.models = models;
        self
    }

    /// Wait for every task the shim spawned. Call at shutdown, after the runs' streams ended or
    /// were dropped; the tasks stop with their stream.
    pub async fn shutdown(&self) {
        self.tracker.close();
        self.tracker.wait().await;
    }

    fn supported_access(&self) -> Vec<AccessMode> {
        self.harness
            .access_modes()
            .iter()
            .copied()
            .map(access_from_upstream)
            .collect()
    }
}

#[async_trait]
impl Executor for HarnessExecutor {
    fn descriptor(&self) -> ExecutorDescriptor {
        let steering = if self.harness.supports_steering() {
            match self.harness.steering_mode() {
                up::SteeringMode::StepBoundary => Steering::StepBoundary,
                up::SteeringMode::TurnBoundary => Steering::TurnBoundary,
            }
        } else {
            Steering::None
        };
        ExecutorDescriptor {
            id: self.id.clone(),
            contract_version: CONTRACT_VERSION,
            adapter_version: self.adapter_version.clone(),
            cli_verified: self.cli_verified.clone(),
            caps: ExecutorCaps {
                steering,
                // Every imported driver reads `RunRequest::resume`.
                resume: true,
                access_modes: self.supported_access(),
                sandbox_levels: self.sandbox_levels.clone(),
                mcp_injection: true,
                // `RunRequest` has no output schema: `start` refuses one.
                structured_output: false,
                deterministic_turn_end: self.harness.deterministic_turn_end(),
                plan_mode: self.harness.supports_plan(),
                models: self.models.clone(),
            },
        }
    }

    /// Installed and version only. Whether the harness is signed in is not knowable without
    /// starting a run, so `signed_in` stays `None`.
    async fn probe(&self) -> ExecutorHealth {
        let harness = Arc::clone(&self.harness);
        let probed = tokio::task::spawn_blocking(move || {
            let installed = harness.installed();
            let version = if installed {
                harness
                    .model_context()
                    .ok()
                    .flatten()
                    .and_then(|context| context.binary_version)
            } else {
                None
            };
            (installed, version)
        })
        .await;
        match probed {
            Ok((installed, version)) => ExecutorHealth {
                installed,
                version,
                signed_in: None,
                detail: (!installed).then(|| "the harness CLI was not found".to_owned()),
            },
            Err(error) => ExecutorHealth {
                detail: Some(format!("probe failed: {error}")),
                ..ExecutorHealth::default()
            },
        }
    }

    async fn start(
        &self,
        spec: ExecSpec,
        controls: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError> {
        // Fail closed before anything is built or spawned (ARCH-15).
        require_access(spec.access, &self.supported_access())?;
        if spec.output_schema.is_some() {
            return Err(ExecError::UnsupportedOutputSchema);
        }
        let request = run_request(&spec)?;

        let ExecControls {
            steering,
            cancel,
            answers,
        } = controls;
        let pending = Arc::new(Pending::default());
        let (prompt_tx, prompt_rx) = mpsc::channel(REQUEST_CAPACITY);
        let (steer_tx, steer_rx) = mpsc::channel(STEER_CAPACITY);
        let run_controls = RunControls {
            execution_lease: None,
            request_input: request_input(Arc::clone(&pending), prompt_tx, cancel.clone()),
            steering: steer_rx,
            interrupt: cancel.clone(),
            turn: TurnControl::default(),
        };

        let inner = match self.harness.run(request, run_controls).await {
            Ok(stream) => stream,
            Err(HarnessError::NotInstalled(message)) => {
                return Err(ExecError::NotInstalled(message));
            }
            Err(HarnessError::NotSignedIn(message)) => {
                return Err(ExecError::NotSignedIn(message));
            }
            // Any other failure to launch is a failed run, not an ineligible executor.
            Err(error) => {
                let diagnostic = crate::redact_secrets(&error.to_string());
                return Ok(futures::stream::once(async move {
                    ExecEvent::Done {
                        stop: StopReason::Error,
                        result: None,
                        diagnostic: Some(diagnostic),
                    }
                })
                .boxed());
            }
        };

        // Both tasks end when `stop` is cancelled, which the output stream does when it ends or
        // is dropped.
        let stop = CancellationToken::new();
        self.tracker
            .spawn(forward_steering(steering, steer_tx, stop.clone()));
        self.tracker.spawn(route_answers(
            answers,
            Arc::clone(&pending),
            cancel.clone(),
            stop.clone(),
        ));

        let state = Mapped {
            inner,
            prompts: prompt_rx,
            pending,
            cancel,
            last_error: None,
            finished: false,
            stop: Some(stop.drop_guard()),
        };
        Ok(futures::stream::unfold(state, |mut state| async move {
            state.next_event().await.map(|event| (event, state))
        })
        .boxed())
    }
}

/// Build the imported request from a spec. Nothing here spawns.
fn run_request(spec: &ExecSpec) -> Result<up::RunRequest, ExecError> {
    let cwd = spec
        .cwd
        .to_str()
        .ok_or_else(|| ExecError::InvalidSpec("cwd is not valid UTF-8".into()))?
        .to_owned();
    let mut servers = spec.mcp.iter().map(|server| up::McpServer {
        name: server.name.clone(),
        command: server.command.clone(),
        args: server.args.clone(),
        env: server.env.clone(),
    });
    // Reno's own server is first; the imported request has one slot for it and one list for the
    // rest.
    let mcp = servers.next();
    Ok(up::RunRequest {
        prompt: spec.prompt.clone(),
        harness: None,
        model: spec.model.clone(),
        reasoning: spec.effort.map(|effort| match effort {
            Effort::Low => up::ReasoningLevel::Low,
            Effort::Medium => up::ReasoningLevel::Medium,
            Effort::High => up::ReasoningLevel::High,
        }),
        model_options: serde_json::Map::new(),
        cwd,
        sandbox: match spec.sandbox {
            SandboxLevel::ReadOnly => up::SandboxLevel::ReadOnly,
            SandboxLevel::WorkspaceWrite => up::SandboxLevel::WorkspaceWrite,
            SandboxLevel::Unrestricted => up::SandboxLevel::DangerFullAccess,
        },
        // `auto_approve` means full access whatever `access` says; never set it, so `access` is
        // the only grant.
        auto_approve: false,
        access: access_to_upstream(spec.access),
        plan: false,
        resume: spec.resume.as_ref().map(|session| session.0.clone()),
        attachments: Vec::new(),
        worktree: None,
        mcp,
        extra_mcp: servers.collect(),
        env: spec.env.clone(),
    })
}

fn access_to_upstream(access: AccessMode) -> up::AccessMode {
    match access {
        AccessMode::Supervised => up::AccessMode::Supervised,
        AccessMode::AutoAcceptEdits => up::AccessMode::AutoAcceptEdits,
        AccessMode::FullAccess => up::AccessMode::FullAccess,
    }
}

fn access_from_upstream(access: up::AccessMode) -> AccessMode {
    match access {
        up::AccessMode::Supervised => AccessMode::Supervised,
        up::AccessMode::AutoAcceptEdits => AccessMode::AutoAcceptEdits,
        up::AccessMode::FullAccess => AccessMode::FullAccess,
    }
}

// ---------------------------------------------------------------------------
// Requests: the `request_input` bridge
// ---------------------------------------------------------------------------

type Waiter = oneshot::Sender<Vec<UserInputAnswer>>;

/// The requests the harness is blocked on, keyed by the `request_id` the consumer sees.
#[derive(Default)]
struct Pending {
    inner: Mutex<PendingInner>,
}

#[derive(Default)]
struct PendingInner {
    /// Set once the run is over or cancelled: later requests are declined on arrival.
    closed: bool,
    waiters: HashMap<String, Waiter>,
}

impl Pending {
    fn lock(&self) -> std::sync::MutexGuard<'_, PendingInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Register a waiter. When the run is already over it is dropped instead, which declines.
    fn insert(&self, request_id: &str, waiter: Waiter, cancel: &CancellationToken) -> bool {
        let mut inner = self.lock();
        if inner.closed || cancel.is_cancelled() {
            return false;
        }
        inner.waiters.insert(request_id.to_owned(), waiter);
        true
    }

    fn remove(&self, request_id: &str) {
        self.lock().waiters.remove(request_id);
    }

    /// Resolve one request. False when it is unknown or already resolved.
    fn answer(&self, request_id: &str, answers: Vec<UserInputAnswer>) -> bool {
        match self.lock().waiters.remove(request_id) {
            Some(waiter) => {
                let _ = waiter.send(answers);
                true
            }
            None => false,
        }
    }

    /// Decline every open request and every later one.
    fn deny_all(&self) {
        let waiters: Vec<Waiter> = {
            let mut inner = self.lock();
            inner.closed = true;
            inner.waiters.drain().map(|(_, waiter)| waiter).collect()
        };
        for waiter in waiters {
            let _ = waiter.send(Vec::new());
        }
    }
}

/// The imported host-side bridge: mint a request id, tell the consumer, hand the harness a
/// one-shot for the answer.
fn request_input(
    pending: Arc<Pending>,
    prompts: mpsc::Sender<ExecEvent>,
    cancel: CancellationToken,
) -> Box<dyn Fn(Vec<UserInputQuestion>) -> oneshot::Receiver<Vec<UserInputAnswer>> + Send + Sync> {
    Box::new(move |questions| {
        let (waiter, receiver) = oneshot::channel();
        let request_id = format!("req_{}", uuid::Uuid::new_v4().simple());
        // A closed run drops the waiter, which the harness reads as a decline.
        if !pending.insert(&request_id, waiter, &cancel) {
            return receiver;
        }
        let event = ExecEvent::NeedsInput {
            request_id: request_id.clone(),
            questions: questions.into_iter().map(question).collect(),
        };
        // A consumer that cannot take the request has not seen it; waiting would hang the run.
        if prompts.try_send(event).is_err() {
            pending.remove(&request_id);
        }
        receiver
    })
}

fn question(question: UserInputQuestion) -> Question {
    Question {
        id: question.id,
        prompt: if question.header.is_empty() {
            question.question
        } else {
            format!("{}: {}", question.header, question.question)
        },
        options: question.options,
        multi_select: question.multi_select,
    }
}

/// Forward steering prompts to the harness. After the supervisor's side closes, the harness's
/// mailbox is kept open until the run ends: a closed mailbox tells some drivers to finish.
async fn forward_steering(
    mut steering: mpsc::Receiver<String>,
    to_harness: mpsc::Sender<SteerMessage>,
    stop: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            prompt = steering.recv() => match prompt {
                Some(prompt) => {
                    tokio::select! {
                        biased;
                        () = stop.cancelled() => return,
                        sent = to_harness.send(SteerMessage::text(prompt)) => {
                            if sent.is_err() {
                                return;
                            }
                        }
                    }
                }
                None => {
                    stop.cancelled().await;
                    return;
                }
            },
        }
    }
}

/// Resolve open requests from the consumer's answers; decline them all on cancel or when the run
/// ends.
async fn route_answers(
    mut answers: mpsc::Receiver<ExecAnswer>,
    pending: Arc<Pending>,
    cancel: CancellationToken,
    stop: CancellationToken,
) {
    let mut open = true;
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => break,
            () = cancel.cancelled() => break,
            answer = answers.recv(), if open => match answer {
                Some(answer) => route(&pending, answer),
                None => open = false,
            },
        }
    }
    pending.deny_all();
}

fn route(pending: &Pending, answer: ExecAnswer) {
    match answer.answer {
        Answer::Input(given) => {
            let given = given
                .into_iter()
                .map(|a| UserInputAnswer {
                    question_id: a.question_id,
                    labels: a.labels,
                })
                .collect();
            if !pending.answer(&answer.request_id, given) {
                tracing::warn!(
                    target: "reno_harness::exec",
                    request_id = %answer.request_id,
                    "answer for an unknown or already resolved request"
                );
            }
        }
        // Imported approvals are prose questions, not structured approvals (stage B, T1.0).
        Answer::Decision(_) => tracing::warn!(
            target: "reno_harness::exec",
            request_id = %answer.request_id,
            "a decision cannot answer a stage A request; send an input answer"
        ),
    }
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

type HarnessStream = BoxStream<'static, Result<AgentEvent, HarnessError>>;

/// The output stream's state: the harness's stream merged with the requests its bridge raised.
struct Mapped {
    inner: HarnessStream,
    prompts: mpsc::Receiver<ExecEvent>,
    pending: Arc<Pending>,
    cancel: CancellationToken,
    /// The last `AgentEvent::Error`, reported by the `Done` that follows it.
    last_error: Option<String>,
    finished: bool,
    /// Cancels the shim's tasks when dropped: at `Done`, or when the stream is dropped.
    stop: Option<DropGuard>,
}

enum Next {
    Harness(Option<Result<AgentEvent, HarnessError>>),
    Prompt(ExecEvent),
}

impl Mapped {
    async fn next_event(&mut self) -> Option<ExecEvent> {
        if self.finished {
            return None;
        }
        loop {
            // The harness's own events are preferred, so text it emitted before it asked a
            // question is delivered before the question.
            let next = tokio::select! {
                biased;
                item = self.inner.next() => Next::Harness(item),
                Some(prompt) = self.prompts.recv() => Next::Prompt(prompt),
            };
            match next {
                Next::Prompt(event) => return Some(event),
                Next::Harness(Some(Ok(event))) => {
                    if let Some(mapped) = self.map(event) {
                        return Some(mapped);
                    }
                }
                Next::Harness(Some(Err(error))) => {
                    let diagnostic = crate::redact_secrets(&error.to_string());
                    return Some(self.finish(StopReason::Error, None, Some(diagnostic)));
                }
                Next::Harness(None) => {
                    return Some(if self.cancel.is_cancelled() {
                        self.finish(StopReason::Interrupted, None, None)
                    } else {
                        let diagnostic = self.last_error.take().map_or_else(
                            || "the harness ended without reporting a result".to_owned(),
                            |message| crate::redact_secrets(&message),
                        );
                        self.finish(StopReason::Error, None, Some(diagnostic))
                    });
                }
            }
        }
    }

    /// The one place a `Done` is built: ends the run, declines open requests and drops the
    /// harness stream so it tears down even if nobody polls again.
    fn finish(
        &mut self,
        stop: StopReason,
        result: Option<String>,
        diagnostic: Option<String>,
    ) -> ExecEvent {
        self.finished = true;
        self.pending.deny_all();
        // A persistent driver keeps its runtime alive after a turn's `Done`; what ends it is the
        // consumer going away. Dropping the harness's stream here is that signal, and stopping
        // the shim's tasks closes the steering mailbox.
        self.inner = futures::stream::empty().boxed();
        self.stop = None;
        ExecEvent::Done {
            stop,
            result,
            diagnostic,
        }
    }

    /// Map one imported event; `None` drops it. No wildcard arm: a new variant must be placed.
    fn map(&mut self, event: AgentEvent) -> Option<ExecEvent> {
        match event {
            AgentEvent::SessionStarted {
                model, session_id, ..
            } => Some(ExecEvent::Started {
                session: SessionRef(session_id),
                model,
            }),
            AgentEvent::TextDelta { text } => Some(ExecEvent::Text { text }),
            AgentEvent::ReasoningDelta { text } => Some(ExecEvent::Reasoning { text }),
            AgentEvent::ToolCall { id, call } => Some(ExecEvent::Tool {
                id,
                call: tool_call(call),
            }),
            AgentEvent::ToolResult {
                id,
                is_error,
                output,
                exit_code,
                ..
            } => Some(ExecEvent::ToolResult {
                id,
                ok: !is_error,
                summary: summarize(output.as_deref(), exit_code),
            }),
            AgentEvent::Usage {
                input_tokens,
                output_tokens,
            } => Some(ExecEvent::Usage {
                input_tokens,
                output_tokens,
            }),
            AgentEvent::InputRequested {
                request_id,
                questions,
            } => Some(ExecEvent::NeedsInput {
                request_id,
                questions: questions.into_iter().map(question).collect(),
            }),
            AgentEvent::Error { message } => {
                self.last_error = Some(message);
                None
            }
            AgentEvent::Done {
                status,
                result,
                error,
                session_id: _,
            } => {
                let (stop, diagnostic) = match status {
                    DoneStatus::Completed => (StopReason::Completed, None),
                    DoneStatus::Interrupted => (StopReason::Interrupted, None),
                    DoneStatus::Errored => (
                        StopReason::Error,
                        Some(
                            error
                                .or_else(|| self.last_error.take())
                                .map(|message| crate::redact_secrets(&message))
                                .unwrap_or_else(|| {
                                    "the harness reported an error without detail".into()
                                }),
                        ),
                    ),
                };
                Some(self.finish(stop, result, diagnostic))
            }
            // Dropped: see DROPPED_AGENT_EVENTS.
            AgentEvent::GeneratedImage { .. }
            | AgentEvent::Visual { .. }
            | AgentEvent::AssistantMessageCompleted { .. }
            | AgentEvent::ContextUsage { .. }
            | AgentEvent::PlanUsage { .. }
            | AgentEvent::AvailableCommands { .. }
            | AgentEvent::InputResolved { .. }
            | AgentEvent::Steered { .. }
            | AgentEvent::UserMessage { .. }
            | AgentEvent::Subagent { .. } => None,
        }
    }
}

/// A decoded imported tool call as Reno's `{ name, input }`.
///
/// Names are snake case (`exec`, `read_file`, `write_file`, `edit_file`, `apply_patch`, `search`,
/// `glob`, `web_fetch`, `web_search`, `todo`); `input` holds the call's fields. An MCP call is
/// named `mcp__<server>__<tool>` and an unknown tool keeps the name the harness gave it, both
/// with the harness's own input.
fn tool_call(call: up::ToolCall) -> ToolCall {
    let name = match &call {
        up::ToolCall::Exec { .. } => "exec".to_owned(),
        up::ToolCall::ReadFile { .. } => "read_file".to_owned(),
        up::ToolCall::WriteFile { .. } => "write_file".to_owned(),
        up::ToolCall::EditFile { .. } => "edit_file".to_owned(),
        up::ToolCall::ApplyPatch { .. } => "apply_patch".to_owned(),
        up::ToolCall::Search { .. } => "search".to_owned(),
        up::ToolCall::Glob { .. } => "glob".to_owned(),
        up::ToolCall::WebFetch { .. } => "web_fetch".to_owned(),
        up::ToolCall::WebSearch { .. } => "web_search".to_owned(),
        up::ToolCall::Todo { .. } => "todo".to_owned(),
        up::ToolCall::Mcp { server, tool, .. } => format!("mcp__{server}__{tool}"),
        up::ToolCall::Unknown { name, .. } => name.clone(),
    };
    let input = match call {
        up::ToolCall::Mcp { input, .. } | up::ToolCall::Unknown { input, .. } => {
            input.unwrap_or(serde_json::Value::Null)
        }
        other => {
            // The imported type is internally tagged by `kind`; the input is everything else.
            let mut value = serde_json::to_value(&other).unwrap_or(serde_json::Value::Null);
            if let Some(fields) = value.as_object_mut() {
                fields.remove("kind");
            }
            value
        }
    };
    ToolCall { name, input }
}

/// A one-line outcome: the exit code, then the last non-empty line of output, redacted.
fn summarize(output: Option<&str>, exit_code: Option<i32>) -> Option<String> {
    let last = output
        .and_then(|output| {
            output
                .lines()
                .rev()
                .map(str::trim)
                .find(|line| !line.is_empty())
        })
        .map(|line| {
            let line = crate::redact_secrets(line);
            if line.chars().count() > SUMMARY_CHARS {
                let cut: String = line.chars().take(SUMMARY_CHARS).collect();
                format!("{cut}…")
            } else {
                line
            }
        });
    match (exit_code, last) {
        (Some(code), Some(line)) => Some(format!("exit code {code}: {line}")),
        (Some(code), None) => Some(format!("exit code {code}")),
        (None, line) => line,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use reno_proto::{McpServerSpec, RunId};

    use super::*;
    use crate::exec::InputAnswer;
    use crate::mock::MockHarness;

    fn id() -> ExecutorId {
        "mock-shim".parse().unwrap()
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

    fn done(status: DoneStatus) -> AgentEvent {
        AgentEvent::Done {
            status,
            result: None,
            error: None,
            session_id: None,
        }
    }

    fn tool_result(id: &str) -> AgentEvent {
        AgentEvent::ToolResult {
            id: id.into(),
            is_error: false,
            output: Some("compiling\nall 3 tests passed\n".into()),
            diff: None,
            preview_paths: Vec::new(),
            image_payloads: Vec::new(),
            exit_code: Some(0),
        }
    }

    /// One sample of every dropped variant, named as `DROPPED_AGENT_EVENTS` names it.
    fn dropped_samples() -> Vec<(&'static str, AgentEvent)> {
        vec![
            (
                "GeneratedImage",
                AgentEvent::GeneratedImage {
                    id: "i".into(),
                    path: "/p".into(),
                    name: "n".into(),
                    mime_type: "image/png".into(),
                },
            ),
            (
                "Visual",
                AgentEvent::Visual {
                    id: "v".into(),
                    path: "/p".into(),
                    title: "t".into(),
                },
            ),
            (
                "AssistantMessageCompleted",
                AgentEvent::AssistantMessageCompleted {
                    assistant_message_id: "m".into(),
                },
            ),
            (
                "ContextUsage",
                AgentEvent::ContextUsage {
                    tokens: Some(1),
                    window: Some(2),
                },
            ),
            (
                "PlanUsage",
                AgentEvent::PlanUsage {
                    windows: Vec::new(),
                    plan_label: None,
                },
            ),
            (
                "AvailableCommands",
                AgentEvent::AvailableCommands {
                    commands: Vec::new(),
                },
            ),
            (
                "InputResolved",
                AgentEvent::InputResolved {
                    request_id: "r".into(),
                    answers: None,
                },
            ),
            (
                "Steered",
                AgentEvent::Steered {
                    assistant_message_id: None,
                    next_assistant_message_id: None,
                },
            ),
            ("UserMessage", AgentEvent::UserMessage { text: "u".into() }),
            (
                "Subagent",
                AgentEvent::Subagent {
                    parent_tool_use_id: "t".into(),
                    event: Box::new(AgentEvent::TextDelta { text: "x".into() }),
                },
            ),
        ]
    }

    /// A harness that plays a fixed list and counts how often it was run.
    struct Stub {
        runs: Arc<AtomicUsize>,
        modes: &'static [up::AccessMode],
        play: Box<dyn Fn(RunControls) -> Result<HarnessStream, HarnessError> + Send + Sync>,
    }

    impl Stub {
        fn new(
            play: impl Fn(RunControls) -> Result<HarnessStream, HarnessError> + Send + Sync + 'static,
        ) -> (Self, Arc<AtomicUsize>) {
            let runs = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    runs: Arc::clone(&runs),
                    modes: &up::AccessMode::ALL,
                    play: Box::new(play),
                },
                runs,
            )
        }

        fn playing(events: Vec<Result<AgentEvent, HarnessError>>) -> (Self, Arc<AtomicUsize>) {
            let events = Mutex::new(Some(events));
            Self::new(move |_| {
                let events = events.lock().unwrap().take().unwrap_or_default();
                Ok(futures::stream::iter(events).boxed())
            })
        }
    }

    #[async_trait]
    impl Harness for Stub {
        fn id(&self) -> up::HarnessId {
            up::HarnessId::Mock
        }
        fn display_name(&self) -> &str {
            "Stub"
        }
        fn supports_steering(&self) -> bool {
            true
        }
        fn steering_mode(&self) -> up::SteeringMode {
            up::SteeringMode::TurnBoundary
        }
        fn reasoning_levels(&self) -> &[up::ReasoningLevel] {
            &[]
        }
        fn access_modes(&self) -> &[up::AccessMode] {
            self.modes
        }
        async fn models(&self) -> Result<Vec<up::Model>, HarnessError> {
            Ok(Vec::new())
        }
        async fn run(
            &self,
            _request: up::RunRequest,
            controls: RunControls,
        ) -> Result<HarnessStream, HarnessError> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            (self.play)(controls)
        }
    }

    async fn collect(executor: &HarnessExecutor, spec: ExecSpec) -> Vec<ExecEvent> {
        let (controls, _handle) = ExecControls::new(4);
        executor
            .start(spec, controls)
            .await
            .unwrap()
            .collect::<Vec<_>>()
            .await
    }

    fn only_one_done_and_last(events: &[ExecEvent]) {
        let dones = events
            .iter()
            .filter(|e| matches!(e, ExecEvent::Done { .. }))
            .count();
        assert_eq!(dones, 1, "{events:?}");
        assert!(matches!(events.last(), Some(ExecEvent::Done { .. })));
    }

    #[test]
    fn the_dropped_list_matches_the_mapping() {
        let samples = dropped_samples();
        let names: Vec<&str> = samples.iter().map(|(name, _)| *name).collect();
        assert_eq!(names, DROPPED_AGENT_EVENTS);
        let mut state = Mapped {
            inner: futures::stream::empty().boxed(),
            prompts: mpsc::channel(1).1,
            pending: Arc::default(),
            cancel: CancellationToken::new(),
            last_error: None,
            finished: false,
            stop: None,
        };
        for (name, event) in samples {
            assert_eq!(state.map(event), None, "{name} must be dropped");
        }
    }

    /// The imported mock harness with a script naming every kept variant, plus dropped ones in
    /// between: the mapping is complete, ordered, and ends in exactly one Done.
    #[tokio::test]
    async fn maps_the_imported_mock_harness_script() {
        let script = vec![
            AgentEvent::SessionStarted {
                harness: up::HarnessId::Mock,
                model: "mock-1".into(),
                tools: Vec::new(),
                cwd: "/tmp".into(),
                session_id: "sess-1".into(),
                assistant_message_id: "m1".into(),
            },
            AgentEvent::ReasoningDelta {
                text: "thinking".into(),
            },
            AgentEvent::ContextUsage {
                tokens: Some(10),
                window: Some(100),
            },
            AgentEvent::TextDelta { text: "hi ".into() },
            AgentEvent::ToolCall {
                id: "t1".into(),
                call: up::ToolCall::Exec {
                    command: "cargo test".into(),
                },
            },
            tool_result("t1"),
            AgentEvent::ToolCall {
                id: "t2".into(),
                call: up::ToolCall::Mcp {
                    server: "reno".into(),
                    tool: "board_read".into(),
                    input: Some(serde_json::json!({"key": "k"})),
                },
            },
            AgentEvent::ToolResult {
                id: "t2".into(),
                is_error: true,
                output: None,
                diff: None,
                preview_paths: Vec::new(),
                image_payloads: Vec::new(),
                exit_code: None,
            },
            AgentEvent::AssistantMessageCompleted {
                assistant_message_id: "m1".into(),
            },
            AgentEvent::Usage {
                input_tokens: 7,
                output_tokens: 9,
            },
            AgentEvent::TextDelta { text: "bye".into() },
            AgentEvent::Done {
                status: DoneStatus::Completed,
                result: Some("fin".into()),
                error: None,
                session_id: Some("sess-1".into()),
            },
        ];
        let executor = HarnessExecutor::new(MockHarness { script }, id());
        let events = collect(&executor, spec(AccessMode::FullAccess)).await;
        assert_eq!(
            events,
            vec![
                ExecEvent::Started {
                    session: SessionRef("sess-1".into()),
                    model: "mock-1".into()
                },
                ExecEvent::Reasoning {
                    text: "thinking".into()
                },
                ExecEvent::Text { text: "hi ".into() },
                ExecEvent::Tool {
                    id: "t1".into(),
                    call: ToolCall {
                        name: "exec".into(),
                        input: serde_json::json!({"command": "cargo test"}),
                    },
                },
                ExecEvent::ToolResult {
                    id: "t1".into(),
                    ok: true,
                    summary: Some("exit code 0: all 3 tests passed".into()),
                },
                ExecEvent::Tool {
                    id: "t2".into(),
                    call: ToolCall {
                        name: "mcp__reno__board_read".into(),
                        input: serde_json::json!({"key": "k"}),
                    },
                },
                ExecEvent::ToolResult {
                    id: "t2".into(),
                    ok: false,
                    summary: None,
                },
                ExecEvent::Usage {
                    input_tokens: 7,
                    output_tokens: 9
                },
                ExecEvent::Text { text: "bye".into() },
                ExecEvent::Done {
                    stop: StopReason::Completed,
                    result: Some("fin".into()),
                    diagnostic: None,
                },
            ]
        );
    }

    #[tokio::test]
    async fn an_unsupported_access_mode_fails_before_the_harness_runs() {
        let (mut stub, runs) = Stub::playing(vec![Ok(done(DoneStatus::Completed))]);
        stub.modes = &[up::AccessMode::Supervised];
        let executor = HarnessExecutor::new(stub, id());
        assert_eq!(
            executor.descriptor().caps.access_modes,
            vec![AccessMode::Supervised]
        );
        let (controls, _handle) = ExecControls::new(1);
        let Err(error) = executor.start(spec(AccessMode::FullAccess), controls).await else {
            panic!("full access must be refused");
        };
        assert!(matches!(
            error,
            ExecError::UnsupportedAccess {
                requested: AccessMode::FullAccess,
                ref supported,
            } if supported == &[AccessMode::Supervised]
        ));
        assert_eq!(runs.load(Ordering::SeqCst), 0, "the harness must not run");
    }

    #[tokio::test]
    async fn the_imported_default_of_full_access_is_not_inherited() {
        // A harness that does not override `access_modes` lists only full access: asking it for
        // less must fail, not silently run with more.
        struct Bare(Arc<AtomicUsize>);
        #[async_trait]
        impl Harness for Bare {
            fn id(&self) -> up::HarnessId {
                up::HarnessId::Mock
            }
            fn display_name(&self) -> &str {
                "Bare"
            }
            fn supports_steering(&self) -> bool {
                false
            }
            fn steering_mode(&self) -> up::SteeringMode {
                up::SteeringMode::TurnBoundary
            }
            fn reasoning_levels(&self) -> &[up::ReasoningLevel] {
                &[]
            }
            async fn models(&self) -> Result<Vec<up::Model>, HarnessError> {
                Ok(Vec::new())
            }
            async fn run(
                &self,
                _: up::RunRequest,
                _: RunControls,
            ) -> Result<HarnessStream, HarnessError> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(futures::stream::empty().boxed())
            }
        }
        let runs = Arc::new(AtomicUsize::new(0));
        let executor = HarnessExecutor::new(Bare(Arc::clone(&runs)), id());
        assert_eq!(executor.descriptor().caps.steering, Steering::None);
        let (controls, _handle) = ExecControls::new(1);
        assert!(matches!(
            executor.start(spec(AccessMode::Supervised), controls).await,
            Err(ExecError::UnsupportedAccess { .. })
        ));
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_structured_output_request_is_refused() {
        let (stub, runs) = Stub::playing(Vec::new());
        let executor = HarnessExecutor::new(stub, id());
        let mut request = spec(AccessMode::FullAccess);
        request.output_schema = Some(serde_json::json!({"type": "object"}));
        let (controls, _handle) = ExecControls::new(1);
        assert!(matches!(
            executor.start(request, controls).await,
            Err(ExecError::UnsupportedOutputSchema)
        ));
        assert_eq!(runs.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_spec_becomes_the_request() {
        let mut request = spec(AccessMode::AutoAcceptEdits);
        request.model = Some("m".into());
        request.effort = Some(Effort::High);
        request.sandbox = SandboxLevel::Unrestricted;
        request.resume = Some(SessionRef("sess".into()));
        request.env.insert("K".into(), "V".into());
        let server = |name: &str| McpServerSpec {
            name: name.into(),
            command: "cmd".into(),
            args: vec!["a".into()],
            env: Default::default(),
        };
        request.mcp = vec![server("reno"), server("other")];
        let built = run_request(&request).unwrap();
        assert_eq!(built.cwd, "/tmp");
        assert_eq!(built.model.as_deref(), Some("m"));
        assert_eq!(built.reasoning, Some(up::ReasoningLevel::High));
        assert_eq!(built.access, up::AccessMode::AutoAcceptEdits);
        assert_eq!(built.sandbox, up::SandboxLevel::DangerFullAccess);
        assert!(!built.auto_approve, "access is the only grant");
        assert_eq!(built.resume.as_deref(), Some("sess"));
        assert_eq!(built.env.get("K").map(String::as_str), Some("V"));
        assert_eq!(built.mcp.as_ref().map(|s| s.name.as_str()), Some("reno"));
        assert_eq!(built.extra_mcp.len(), 1);
        assert_eq!(built.extra_mcp[0].name, "other");
    }

    #[tokio::test]
    async fn a_failing_harness_yields_done_error_not_a_stream_error() {
        let (stub, _) = Stub::playing(vec![
            Ok(AgentEvent::TextDelta { text: "a".into() }),
            Err(HarnessError::Protocol("bad frame sk-secret123".into())),
            Ok(AgentEvent::TextDelta {
                text: "after".into(),
            }),
        ]);
        let executor = HarnessExecutor::new(stub, id());
        let events = collect(&executor, spec(AccessMode::FullAccess)).await;
        only_one_done_and_last(&events);
        let ExecEvent::Done {
            stop, diagnostic, ..
        } = events.last().unwrap()
        else {
            unreachable!()
        };
        assert_eq!(*stop, StopReason::Error);
        let diagnostic = diagnostic.as_deref().unwrap();
        assert!(diagnostic.contains("bad frame"), "{diagnostic}");
        assert!(!diagnostic.contains("secret123"), "{diagnostic}");
        assert_eq!(events.len(), 2, "nothing after the failure");
    }

    #[tokio::test]
    async fn an_error_event_becomes_the_done_diagnostic() {
        let (stub, _) = Stub::playing(vec![
            Ok(AgentEvent::Error {
                message: "rate limited".into(),
            }),
            Ok(done(DoneStatus::Errored)),
        ]);
        let executor = HarnessExecutor::new(stub, id());
        let events = collect(&executor, spec(AccessMode::FullAccess)).await;
        assert_eq!(
            events,
            vec![ExecEvent::Done {
                stop: StopReason::Error,
                result: None,
                diagnostic: Some("rate limited".into()),
            }]
        );
    }

    #[tokio::test]
    async fn done_statuses_map_to_stop_reasons() {
        for (status, stop) in [
            (DoneStatus::Completed, StopReason::Completed),
            (DoneStatus::Interrupted, StopReason::Interrupted),
            (DoneStatus::Errored, StopReason::Error),
        ] {
            let (stub, _) = Stub::playing(vec![Ok(done(status))]);
            let executor = HarnessExecutor::new(stub, id());
            let events = collect(&executor, spec(AccessMode::FullAccess)).await;
            let [ExecEvent::Done { stop: got, .. }] = events.as_slice() else {
                panic!("{events:?}");
            };
            assert_eq!(*got, stop);
        }
    }

    #[tokio::test]
    async fn a_stream_without_done_still_ends_with_one() {
        let (stub, _) = Stub::playing(vec![Ok(AgentEvent::TextDelta { text: "a".into() })]);
        let executor = HarnessExecutor::new(stub, id());
        let events = collect(&executor, spec(AccessMode::FullAccess)).await;
        only_one_done_and_last(&events);
        assert!(matches!(
            events.last(),
            Some(ExecEvent::Done {
                stop: StopReason::Error,
                diagnostic: Some(_),
                ..
            })
        ));
    }

    /// A persistent driver keeps running after a turn's `Done`; the shim must end it.
    #[tokio::test]
    async fn a_harness_that_outlives_its_done_is_torn_down_at_done() {
        struct SetOnDrop(Arc<AtomicBool>);
        impl Drop for SetOnDrop {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&dropped);
        let (stub, _) = Stub::new(move |_| {
            let session = SetOnDrop(Arc::clone(&flag));
            Ok(
                futures::stream::unfold((session, false), |(session, sent)| async move {
                    if sent {
                        futures::future::pending::<()>().await;
                    }
                    Some((Ok(done(DoneStatus::Completed)), (session, true)))
                })
                .boxed(),
            )
        });
        let executor = HarnessExecutor::new(stub, id());
        let (controls, _handle) = ExecControls::new(1);
        let mut stream = executor
            .start(spec(AccessMode::FullAccess), controls)
            .await
            .unwrap();
        assert!(matches!(stream.next().await, Some(ExecEvent::Done { .. })));
        assert!(
            dropped.load(Ordering::SeqCst),
            "the harness's stream is dropped as Done is yielded"
        );
        // The shim's tasks stop at Done too, before the consumer polls again.
        tokio::time::timeout(Duration::from_secs(5), executor.shutdown())
            .await
            .expect("shim tasks end at Done");
        assert_eq!(stream.next().await, None);
    }

    #[tokio::test]
    async fn an_error_event_without_a_done_is_the_diagnostic() {
        let (stub, _) = Stub::playing(vec![Ok(AgentEvent::Error {
            message: "token sk-abc123 rejected".into(),
        })]);
        let executor = HarnessExecutor::new(stub, id());
        let events = collect(&executor, spec(AccessMode::FullAccess)).await;
        let [
            ExecEvent::Done {
                stop: StopReason::Error,
                diagnostic: Some(diagnostic),
                ..
            },
        ] = events.as_slice()
        else {
            panic!("{events:?}");
        };
        assert!(diagnostic.contains("rejected") && !diagnostic.contains("abc123"));
    }

    #[tokio::test]
    async fn events_after_the_first_done_are_discarded() {
        let (stub, _) = Stub::playing(vec![
            Ok(done(DoneStatus::Completed)),
            Ok(AgentEvent::TextDelta {
                text: "late".into(),
            }),
            Ok(done(DoneStatus::Errored)),
        ]);
        let executor = HarnessExecutor::new(stub, id());
        let events = collect(&executor, spec(AccessMode::FullAccess)).await;
        assert_eq!(events.len(), 1);
        only_one_done_and_last(&events);
    }

    #[tokio::test]
    async fn a_harness_that_is_not_installed_is_an_exec_error_other_launch_failures_are_done() {
        let (stub, _) = Stub::new(|_| Err(HarnessError::NotInstalled("claude".into())));
        let executor = HarnessExecutor::new(stub, id());
        let (controls, _handle) = ExecControls::new(1);
        assert!(matches!(
            executor.start(spec(AccessMode::FullAccess), controls).await,
            Err(ExecError::NotInstalled(_))
        ));

        let (stub, _) = Stub::new(|_| Err(HarnessError::NotSignedIn("run login".into())));
        let executor = HarnessExecutor::new(stub, id());
        let (controls, _handle) = ExecControls::new(1);
        assert!(matches!(
            executor.start(spec(AccessMode::FullAccess), controls).await,
            Err(ExecError::NotSignedIn(_))
        ));

        let (stub, _) = Stub::new(|_| Err(HarnessError::Protocol("handshake failed".into())));
        let executor = HarnessExecutor::new(stub, id());
        let events = collect(&executor, spec(AccessMode::FullAccess)).await;
        only_one_done_and_last(&events);
    }

    #[tokio::test]
    async fn steering_prompts_reach_the_harness() {
        let (stub, _) = Stub::new(|mut controls| {
            Ok(futures::stream::once(async move {
                let steer = controls.steering.recv().await.map(|m| m.prompt);
                (steer, controls)
            })
            .flat_map(|(steer, _controls)| {
                futures::stream::iter(vec![
                    Ok(AgentEvent::TextDelta {
                        text: steer.unwrap_or_default(),
                    }),
                    Ok(done(DoneStatus::Completed)),
                ])
            })
            .boxed())
        });
        let executor = HarnessExecutor::new(stub, id());
        let (controls, handle) = ExecControls::new(2);
        let stream = executor
            .start(spec(AccessMode::FullAccess), controls)
            .await
            .unwrap();
        handle.steer("left").await.unwrap();
        let events: Vec<_> = stream.collect().await;
        assert_eq!(
            events[0],
            ExecEvent::Text {
                text: "left".into()
            }
        );
        only_one_done_and_last(&events);
        executor.shutdown().await;
    }

    #[tokio::test]
    async fn the_cancel_token_is_the_harness_interrupt() {
        let (stub, _) = Stub::new(|controls| {
            Ok(futures::stream::once(async move {
                controls.interrupt.cancelled().await;
                Ok(done(DoneStatus::Interrupted))
            })
            .boxed())
        });
        let executor = HarnessExecutor::new(stub, id());
        let (controls, handle) = ExecControls::new(1);
        let stream = executor
            .start(spec(AccessMode::FullAccess), controls)
            .await
            .unwrap();
        handle.cancel();
        let events: Vec<_> = stream.collect().await;
        assert_eq!(
            events,
            vec![ExecEvent::Done {
                stop: StopReason::Interrupted,
                result: None,
                diagnostic: None
            }]
        );
    }

    /// Supervised access makes the imported mock harness ask before its command, through
    /// `request_input`, exactly as the real drivers do.
    async fn supervised_mock_asks(
        executor: &HarnessExecutor,
        controls: ExecControls,
    ) -> (BoxStream<'static, ExecEvent>, String, Question) {
        let mut stream = executor
            .start(spec(AccessMode::Supervised), controls)
            .await
            .unwrap();
        let first = stream.next().await.unwrap();
        assert!(
            matches!(&first, ExecEvent::Text { text } if text.starts_with("I'll run")),
            "{first:?}"
        );
        let ExecEvent::NeedsInput {
            request_id,
            mut questions,
        } = stream.next().await.unwrap()
        else {
            panic!("expected NeedsInput after the text");
        };
        assert_eq!(questions.len(), 1);
        (stream, request_id, questions.remove(0))
    }

    #[tokio::test(start_paused = true)]
    async fn a_prose_approval_is_needs_input_and_its_answer_reaches_the_harness() {
        let executor = HarnessExecutor::new(MockHarness { script: Vec::new() }, id());
        let (controls, handle) = ExecControls::new(2);
        let (stream, request_id, question) = supervised_mock_asks(&executor, controls).await;
        assert!(question.prompt.contains("cargo test --workspace"));
        assert_eq!(question.options, vec!["Yes".to_string(), "No".to_string()]);
        handle
            .answer_input(
                &request_id,
                vec![InputAnswer {
                    question_id: question.id.clone(),
                    labels: vec!["Yes".into()],
                }],
            )
            .await
            .unwrap();
        let rest: Vec<_> = stream.collect().await;
        assert!(
            matches!(&rest[0], ExecEvent::Tool { id, .. } if id == "mock-approved-tool"),
            "{rest:?}"
        );
        only_one_done_and_last(&rest);
        executor.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn an_unanswered_request_is_denied_when_cancel_fires() {
        let executor = HarnessExecutor::new(MockHarness { script: Vec::new() }, id());
        let (controls, handle) = ExecControls::new(2);
        let (stream, _request_id, _question) = supervised_mock_asks(&executor, controls).await;
        handle.cancel();
        let rest: Vec<_> = stream.collect().await;
        // The mock reads a decline as a refusal and never runs the command.
        assert!(
            !rest.iter().any(|e| matches!(e, ExecEvent::Tool { .. })),
            "{rest:?}"
        );
        assert!(matches!(
            &rest[0],
            ExecEvent::Text { text } if text.starts_with("Understood")
        ));
        only_one_done_and_last(&rest);
        executor.shutdown().await;
    }

    #[tokio::test(start_paused = true)]
    async fn a_no_answer_and_a_decision_do_not_approve() {
        let executor = HarnessExecutor::new(MockHarness { script: Vec::new() }, id());
        let (controls, handle) = ExecControls::new(2);
        let (stream, request_id, question) = supervised_mock_asks(&executor, controls).await;
        // A structured decision is not a stage A answer: it is ignored and the request stays open.
        handle
            .decide(&request_id, crate::exec::Decision::Allow)
            .await
            .unwrap();
        // An unknown request id is ignored too.
        handle
            .answer_input("req_unknown", Vec::new())
            .await
            .unwrap();
        handle
            .answer_input(
                &request_id,
                vec![InputAnswer {
                    question_id: question.id,
                    labels: vec!["No".into()],
                }],
            )
            .await
            .unwrap();
        let rest: Vec<_> = stream.collect().await;
        assert!(matches!(
            &rest[0],
            ExecEvent::Text { text } if text.starts_with("Understood")
        ));
        only_one_done_and_last(&rest);
    }

    #[tokio::test]
    async fn a_request_after_cancel_is_declined_at_once() {
        let pending = Arc::new(Pending::default());
        let (tx, mut rx) = mpsc::channel(4);
        let cancel = CancellationToken::new();
        let ask = request_input(Arc::clone(&pending), tx, cancel.clone());
        cancel.cancel();
        let answers = ask(vec![crate::approval::command("A", "ls")]).await;
        assert_eq!(answers.unwrap_or_default(), Vec::new());
        assert!(rx.try_recv().is_err(), "a declined request is never shown");
    }

    #[tokio::test]
    async fn dropping_the_stream_stops_the_shim_tasks_and_declines_requests() {
        let executor = HarnessExecutor::new(MockHarness { script: Vec::new() }, id());
        let (controls, _handle) = ExecControls::new(2);
        let (stream, _request_id, _question) = supervised_mock_asks(&executor, controls).await;
        drop(stream);
        tokio::time::timeout(Duration::from_secs(5), executor.shutdown())
            .await
            .expect("shim tasks end with the stream");
    }

    #[tokio::test]
    async fn probe_reports_installed_for_the_mock_and_the_executor_is_a_dyn_executor() {
        let executor: Arc<dyn Executor> = Arc::new(HarnessExecutor::new(
            MockHarness { script: Vec::new() },
            id(),
        ));
        let health = executor.probe().await;
        assert!(health.installed);
        assert_eq!(health.signed_in, None);
        let caps = executor.descriptor().caps;
        assert_eq!(caps.access_modes.len(), 3);
        assert_eq!(caps.steering, Steering::StepBoundary);
        assert!(caps.plan_mode);
    }

    #[test]
    fn tool_names_and_inputs() {
        let call = tool_call(up::ToolCall::ReadFile {
            path: "a.rs".into(),
        });
        assert_eq!(call.name, "read_file");
        assert_eq!(call.input, serde_json::json!({"path": "a.rs"}));
        let call = tool_call(up::ToolCall::Unknown {
            name: "Agent: scan".into(),
            input: None,
        });
        assert_eq!(call.name, "Agent: scan");
        assert_eq!(call.input, serde_json::Value::Null);
    }

    #[test]
    fn summaries_are_one_short_redacted_line() {
        assert_eq!(summarize(None, None), None);
        assert_eq!(summarize(Some("\n\n"), None), None);
        assert_eq!(summarize(None, Some(2)), Some("exit code 2".into()));
        let long = "x".repeat(500);
        let summary = summarize(Some(&format!("first\n{long}")), None).unwrap();
        assert_eq!(summary.chars().count(), SUMMARY_CHARS + 1);
        assert_eq!(
            summarize(Some("Authorization: Bearer abc123"), None),
            Some("Authorization: Bearer [REDACTED]".into())
        );
    }
}
