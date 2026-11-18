//! The one error type of the crate. SQLite errors are wrapped, never exposed as a type (ARCH-5).

use reno_proto::{RunId, RunStatus, ThreadId};

/// Everything that can go wrong in `reno-store`.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The database failed. The cause is kept as an opaque error so that no SQLite type leaks.
    #[error("database error: {0}")]
    Database(Box<dyn std::error::Error + Send + Sync>),
    /// A schema migration failed or the migration set is invalid.
    #[error("migration error: {0}")]
    Migration(String),
    /// The named row does not exist.
    #[error("{kind} {id} not found")]
    NotFound {
        /// What was looked up (`"run"`, `"task"`).
        kind: &'static str,
        /// The id that was looked up.
        id: String,
    },
    /// A row with this id already exists.
    #[error("{kind} {id} already exists")]
    AlreadyExists {
        /// What was inserted (`"run"`, `"task"`).
        kind: &'static str,
        /// The id that collided.
        id: String,
    },
    /// The write carried a fence that is not the run's current fence (ARCH-9).
    #[error("stale fence for {run}: supplied {supplied}, current {current}")]
    StaleFence {
        /// The run written to.
        run: RunId,
        /// The fence the caller held.
        supplied: u64,
        /// The fence in the database.
        current: u64,
    },
    /// The lifecycle diagram has no edge between the two statuses.
    #[error("illegal transition for {run}: {from:?} -> {to:?}")]
    IllegalTransition {
        /// The run.
        run: RunId,
        /// Its current status.
        from: RunStatus,
        /// The requested status.
        to: RunStatus,
    },
    /// The run holds no lease (it is queued, interrupted or terminal), so it cannot be extended.
    #[error("{run} holds no lease (status {status:?})")]
    NotLeased {
        /// The run.
        run: RunId,
        /// Its current status.
        status: RunStatus,
    },
    /// The thread has a run queued or live, so it takes no other (at most one run of a thread is
    /// live at a time).
    #[error("thread {thread} has a run in progress ({run})")]
    ThreadBusy {
        /// The thread.
        thread: ThreadId,
        /// Its live run.
        run: RunId,
    },
    /// The thread's worktree was removed, so there is nothing left to continue.
    #[error("thread {0} is closed and its worktree is gone; start a new one")]
    ThreadClosed(ThreadId),
    /// A stored value does not parse. The database was edited by something other than this crate.
    #[error("corrupt stored value: {0}")]
    Corrupt(String),
    /// A caller-supplied value is not acceptable.
    #[error("invalid argument: {0}")]
    InvalidArgument(String),
    /// The writer thread is gone or dropped the command before answering.
    #[error("the store is closed")]
    Closed,
    /// A journal file operation failed.
    #[error("journal I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A journal line could not be serialised.
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

/// Result alias for this crate.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// Wraps a SQLite error. A function rather than a `From` impl so the public API never mentions
/// the SQLite crate.
pub(crate) fn db(err: rusqlite::Error) -> StoreError {
    StoreError::Database(Box::new(err))
}

/// `.db()` on SQLite results.
pub(crate) trait DbResult<T> {
    /// Converts the SQLite error into a [`StoreError`].
    fn db(self) -> Result<T>;
}

impl<T> DbResult<T> for std::result::Result<T, rusqlite::Error> {
    fn db(self) -> Result<T> {
        self.map_err(db)
    }
}

/// True when the error is a UNIQUE or PRIMARY KEY violation.
pub(crate) fn is_unique_violation(err: &rusqlite::Error) -> bool {
    matches!(
        err,
        rusqlite::Error::SqliteFailure(e, _)
            if e.code == rusqlite::ErrorCode::ConstraintViolation
                && matches!(
                    e.extended_code,
                    rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
                        | rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE
                )
    )
}
