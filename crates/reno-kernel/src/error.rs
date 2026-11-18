//! The kernel's one error type (ARCH-14).

use reno_proto::defs::DefKind;
use reno_proto::{ApprovalId, BudgetStop, ExecutorId, NotResumed, RunId, RunStatus, Seq, ThreadId};
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
    /// The run has ended (or ended first): it takes no decision, steer or model change.
    #[error("run {0} has ended")]
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
    /// No such thread.
    #[error("thread {0} not found")]
    ThreadNotFound(ThreadId),
    /// The thread has a run queued or live, so it takes no other and cannot be closed. Send to
    /// the live run instead (a steer).
    #[error("thread {thread} has a run in progress ({run})")]
    ThreadBusy {
        /// The thread.
        thread: ThreadId,
        /// Its live run.
        run: RunId,
    },
    /// The thread's worktree was removed, so there is nothing to continue: start a new thread.
    #[error("thread {0} is closed and its worktree is gone; start a new one")]
    ThreadClosed(ThreadId),
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
    /// The executor already has an install or update that is not over.
    #[error("executor {0} already has an install or update that is not over")]
    JobRunning(ExecutorId),
    /// The install or update would do nothing, for the reason given.
    #[error("nothing to do for executor {executor}: {why}")]
    NothingToDo {
        /// The executor.
        executor: ExecutorId,
        /// Why, in words.
        why: String,
    },
    /// The executor has no updater of its own.
    #[error("executor {0} has no updater")]
    NoUpdater(ExecutorId),
    /// The daemon was not given the means to run installs and updates.
    #[error("this daemon cannot install or update executors")]
    MaintenanceOff,
    /// A CLI's own updater switch could not be moved.
    #[error("could not change the CLI's own updater: {0}")]
    SelfUpdate(String),
    /// The run has secrets whose values cannot be read now, so nothing can be redacted and the
    /// text that would need it is withheld.
    #[error("the secrets of run {0} cannot be read now, so its text is withheld")]
    SecretsUnreadable(RunId),
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

impl KernelError {
    /// A store error from a call that names a thread, with the thread outcomes typed.
    pub(crate) fn of_thread(error: StoreError, thread: &ThreadId) -> Self {
        match error {
            StoreError::NotFound { kind: "thread", .. } => Self::ThreadNotFound(thread.clone()),
            StoreError::ThreadBusy { thread, run } => Self::ThreadBusy { thread, run },
            StoreError::ThreadClosed(thread) => Self::ThreadClosed(thread),
            other => Self::Store(other),
        }
    }
}
