//! The kernel's one error type (ARCH-14).

use reno_proto::defs::DefKind;
use reno_proto::{ApprovalId, ExecutorId, RunId, RunStatus, Seq};
use reno_store::StoreError;

use crate::workspace::WorkspaceError;

/// Everything a kernel call can fail with.
#[derive(Debug, thiserror::Error)]
pub enum KernelError {
    /// The store failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// No such run.
    #[error("run {0} not found")]
    RunNotFound(RunId),
    /// No such pending approval: it was decided already, or its run ended.
    #[error("approval {0} not found")]
    ApprovalNotFound(ApprovalId),
    /// The run is in a state that has nothing to cancel.
    #[error("run {run} cannot be cancelled while {status:?}")]
    NotCancellable {
        /// The run.
        run: RunId,
        /// Its status.
        status: RunStatus,
    },
    /// The run's steering queue is full; try again once the executor has taken some.
    #[error("run {0} has too many steering prompts queued")]
    SteerQueueFull(RunId),
    /// The run ended before it took the decision.
    #[error("run {0} no longer accepts decisions")]
    RunEnded(RunId),
    /// The kernel is shutting down.
    #[error("the kernel is shutting down")]
    ShuttingDown,
    /// No executor with this id is registered.
    #[error("executor {0} not found")]
    ExecutorNotFound(ExecutorId),
    /// No such definition, or no such version of it.
    #[error("{kind} {name:?} {}", match version { Some(v) => format!("version {v} not found"), None => "not found".to_owned() })]
    DefinitionNotFound {
        /// The kind that was looked up.
        kind: DefKind,
        /// The name that was looked up.
        name: String,
        /// The version, when one was asked for.
        version: Option<u32>,
    },
    /// None of the executors of a bot is available on this daemon.
    #[error("none of the executors of bot {0:?} is available on this daemon")]
    NoExecutor(String),
    /// No trigger by this name is in force.
    #[error("trigger {0:?} not found")]
    TriggerNotFound(String),
    /// The trigger is disabled in its definition, so it cannot be fired.
    #[error("trigger {0:?} is disabled")]
    TriggerDisabled(String),
    /// No workspace was recorded for this run: it ran in place, or has not started.
    #[error("run {0} has no workspace")]
    WorkspaceNotFound(RunId),
    /// The `Workspace` port refused or failed.
    #[error(transparent)]
    Workspace(#[from] WorkspaceError),
    /// The `Definitions` port failed.
    #[error("definitions: {0}")]
    Definitions(String),
    /// A request value cannot be stored (a working directory that is not UTF-8, for example).
    #[error("invalid request: {0}")]
    Invalid(String),
    /// A subscriber fell behind and was cut off (ARCH-12). Resubscribe with `from` set to
    /// `resume_from` to continue without loss.
    #[error("subscriber fell behind; resume from seq {resume_from}")]
    Gap {
        /// The first event the subscriber did not get.
        resume_from: Seq,
    },
}

/// Result alias for the kernel.
pub type Result<T, E = KernelError> = std::result::Result<T, E>;
