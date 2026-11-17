//! The kernel's one error type (ARCH-14).

use reno_proto::defs::DefKind;
use reno_proto::{ApprovalId, BudgetStop, ExecutorId, NotResumed, RunId, RunStatus, Seq};
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
    /// No such standing rule.
    #[error("standing rule {0} not found")]
    RuleNotFound(reno_proto::RuleId),
    /// The run is in a state that has nothing to cancel.
    #[error("run {run} cannot be cancelled while {status:?}")]
    NotCancellable {
        /// The run.
        run: RunId,
        /// Its status.
        status: RunStatus,
    },
    /// The run cannot be resumed, for the reason given.
    #[error("run {run} cannot be resumed: {why}")]
    NotResumable {
        /// The run.
        run: RunId,
        /// Why not.
        why: NotResumed,
    },
    /// The run's steering queue is full; try again once the executor has taken some.
    #[error("run {0} has too many steering prompts queued")]
    SteerQueueFull(RunId),
    /// The harness did not answer a model change in time.
    #[error("run {0}'s harness did not answer the model change in time")]
    ModelSwitchTimedOut(RunId),
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
    /// The bot has used its daily token ceiling, so no new run of it is accepted until the next
    /// local day. Typed: it is not an account failure and does not back a trigger off.
    #[error("bot {bot:?} is refused: {stop}")]
    BudgetExhausted {
        /// The bot.
        bot: String,
        /// The limit and what had been used.
        stop: BudgetStop,
    },
    /// A run token was refused: it is unknown, belongs to another run, was revoked, has expired,
    /// or its run is no longer live. One answer for every cause, so a caller learns nothing
    /// about which tokens exist.
    #[error("the run token is not valid")]
    AgentDenied,
    /// A board write named a version that is not the key's current one. Nothing was written.
    #[error("the board entry changed since the version this write was based on")]
    BoardStale {
        /// The entry as it stands, with its value; `None` when the key does not exist.
        current: Option<Box<reno_proto::BoardEntry>>,
    },
    /// A board scope already holds as many keys as it may, and this key is new.
    #[error("board scope {scope:?} is full")]
    BoardFull {
        /// The scope.
        scope: String,
    },
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
