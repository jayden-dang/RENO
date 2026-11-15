//! The kernel's one error type (ARCH-14).

use reno_proto::{ApprovalId, RunId, RunStatus, Seq};
use reno_store::StoreError;

/// Everything a kernel call can fail with.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
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
    /// The run ended before it took the decision.
    #[error("run {0} no longer accepts decisions")]
    RunEnded(RunId),
    /// The kernel is shutting down.
    #[error("the kernel is shutting down")]
    ShuttingDown,
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
