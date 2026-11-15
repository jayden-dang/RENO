//! The writer thread: the only owner of the write connection.
//!
//! Commands arrive on a bounded `std::sync::mpsc` channel. The thread drains what is queued, up
//! to a cap, into one `BEGIN IMMEDIATE` transaction (group commit). Each command runs inside its
//! own `SAVEPOINT`, so a command that fails rolls back only itself. After the single `COMMIT`
//! each caller gets its own result through a oneshot, so a caller that sees `Ok` knows its write
//! is committed.
//!
//! A command marked [`Durability::Full`] never joins a group. It runs alone with
//! `PRAGMA synchronous=FULL` set around its transaction, then the pragma goes back to `NORMAL`.
//!
//! The checkpoint is a deadline, not an interval (ARCH-10): after a commit the thread waits with
//! `recv_timeout`; when the idle delay passes with nothing queued it runs one
//! `wal_checkpoint(TRUNCATE)` and goes back to a blocking `recv`.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::Duration;

use rusqlite::Connection;
use tokio::sync::oneshot;

use crate::error::{Result, StoreError};

/// How hard a write must be flushed before its caller is told it succeeded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Durability {
    /// `synchronous=NORMAL`, grouped with neighbours. A power loss can drop the last commits.
    Normal,
    /// `synchronous=FULL`, alone in its transaction. For fenced effect intents, approval
    /// decisions and other records that must survive power loss.
    Full,
}

/// Counters the tests (and later metrics) read.
#[derive(Debug, Default)]
pub(crate) struct WriterStats {
    /// Transactions committed (a group counts once).
    pub(crate) commits: AtomicU64,
    /// Of those, the transactions that ran with `synchronous=FULL`.
    pub(crate) durable_commits: AtomicU64,
    /// Commands run, whether they succeeded or not.
    pub(crate) commands: AtomicU64,
    /// Idle checkpoints run.
    pub(crate) checkpoints: AtomicU64,
}

/// What a job leaves behind after running inside its savepoint.
pub(crate) struct Staged {
    /// The job returned an error; roll its savepoint back.
    failed: bool,
    /// Delivers the result once the group's fate is known. The argument is the commit error
    /// message, if the group failed to commit.
    deliver: Box<dyn FnOnce(Option<String>) + Send>,
}

/// A unit of work for the writer thread.
pub(crate) trait Job: Send {
    /// Runs inside a savepoint on the write connection.
    fn run(self: Box<Self>, conn: &Connection) -> Staged;
    /// The transaction could not even begin; fail without running.
    fn abort(self: Box<Self>, message: String);
}

struct Op<T, F> {
    f: F,
    tx: oneshot::Sender<Result<T>>,
}

impl<T, F> Job for Op<T, F>
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> Result<T> + Send + 'static,
{
    fn run(self: Box<Self>, conn: &Connection) -> Staged {
        let Op { f, tx } = *self;
        let res = match catch_unwind(AssertUnwindSafe(|| f(conn))) {
            Ok(res) => res,
            Err(_) => Err(StoreError::Closed),
        };
        Staged {
            failed: res.is_err(),
            deliver: Box::new(move |commit_err| {
                let out = match (res, commit_err) {
                    (Ok(_), Some(msg)) => Err(StoreError::Database(msg.into())),
                    (res, _) => res,
                };
                // The caller may have gone away; nothing to do then.
                let _ = tx.send(out);
            }),
        }
    }

    fn abort(self: Box<Self>, message: String) {
        let _ = self.tx.send(Err(StoreError::Database(message.into())));
    }
}

/// A queued command.
pub(crate) struct Command {
    pub(crate) durability: Durability,
    job: Box<dyn Job>,
}

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Command")
            .field("durability", &self.durability)
            .finish_non_exhaustive()
    }
}

/// Packages `f` as a command plus the receiver for its result.
pub(crate) fn command<T, F>(durability: Durability, f: F) -> (Command, oneshot::Receiver<Result<T>>)
where
    T: Send + 'static,
    F: FnOnce(&Connection) -> Result<T> + Send + 'static,
{
    let (tx, rx) = oneshot::channel();
    (
        Command {
            durability,
            job: Box::new(Op { f, tx }),
        },
        rx,
    )
}

/// Tunables of the writer thread.
#[derive(Debug, Clone)]
pub struct StoreConfig {
    /// Capacity of the command channel. Senders wait (without blocking the executor) when it is
    /// full, which is the backpressure. Default 1024: far above a burst from a busy daemon, small
    /// enough that a stuck disk shows up as waiting senders instead of unbounded memory.
    pub channel_capacity: usize,
    /// Most commands in one group commit. Default 256: it bounds how long one transaction holds
    /// the write lock and how many callers wait on a single fsync.
    pub group_cap: usize,
    /// Idle time after a write before the WAL is checkpointed once. Default 5 s.
    pub checkpoint_idle: Duration,
}

impl Default for StoreConfig {
    fn default() -> Self {
        Self {
            channel_capacity: 1024,
            group_cap: 256,
            checkpoint_idle: Duration::from_secs(5),
        }
    }
}

/// The writer loop. Returns when every sender is gone.
pub(crate) fn run(
    conn: Connection,
    rx: Receiver<Command>,
    config: &StoreConfig,
    stats: &Arc<WriterStats>,
) {
    let group_cap = config.group_cap.max(1);
    let mut pending: Option<Command> = None;
    let mut dirty = false;
    loop {
        let first = match pending.take() {
            Some(cmd) => cmd,
            None if dirty => match rx.recv_timeout(config.checkpoint_idle) {
                Ok(cmd) => cmd,
                Err(RecvTimeoutError::Timeout) => {
                    checkpoint(&conn, stats);
                    dirty = false;
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(cmd) => cmd,
                Err(_) => break,
            },
        };

        let mut batch = vec![first];
        if batch[0].durability == Durability::Normal {
            while batch.len() < group_cap {
                match rx.try_recv() {
                    Ok(cmd) if cmd.durability == Durability::Full => {
                        pending = Some(cmd);
                        break;
                    }
                    Ok(cmd) => batch.push(cmd),
                    Err(TryRecvError::Empty | TryRecvError::Disconnected) => break,
                }
            }
        }
        commit_group(&conn, batch, stats);
        dirty = true;
    }
    if dirty {
        checkpoint(&conn, stats);
    }
}

fn checkpoint(conn: &Connection, stats: &WriterStats) {
    match conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(())) {
        Ok(()) => {
            stats.checkpoints.fetch_add(1, Ordering::Relaxed);
        }
        Err(err) => tracing::warn!(%err, "wal checkpoint failed"),
    }
}

/// Runs one group in one transaction and delivers every result after the commit.
fn commit_group(conn: &Connection, batch: Vec<Command>, stats: &WriterStats) {
    let full = batch.iter().any(|c| c.durability == Durability::Full);
    stats
        .commands
        .fetch_add(batch.len() as u64, Ordering::Relaxed);

    if full && let Err(err) = conn.execute_batch("PRAGMA synchronous=FULL") {
        abort_all(batch, &err.to_string());
        return;
    }
    let staged = match conn.execute_batch("BEGIN IMMEDIATE") {
        Ok(()) => run_in_savepoints(conn, batch),
        Err(err) => {
            abort_all(batch, &err.to_string());
            reset_synchronous(conn, full);
            return;
        }
    };

    let commit_err = match staged.group_err {
        Some(msg) => {
            if let Err(err) = conn.execute_batch("ROLLBACK") {
                tracing::error!(%err, "rollback failed");
            }
            Some(msg)
        }
        None => match conn.execute_batch("COMMIT") {
            Ok(()) => {
                stats.commits.fetch_add(1, Ordering::Relaxed);
                if full {
                    stats.durable_commits.fetch_add(1, Ordering::Relaxed);
                }
                None
            }
            Err(err) => {
                tracing::error!(%err, "commit failed");
                if let Err(err) = conn.execute_batch("ROLLBACK") {
                    // A failed COMMIT may already have ended the transaction.
                    tracing::debug!(%err, "rollback after failed commit");
                }
                Some(err.to_string())
            }
        },
    };
    reset_synchronous(conn, full);
    for item in staged.items {
        (item.deliver)(commit_err.clone());
    }
}

fn reset_synchronous(conn: &Connection, full: bool) {
    if full && let Err(err) = conn.execute_batch("PRAGMA synchronous=NORMAL") {
        tracing::error!(%err, "could not reset synchronous=NORMAL");
    }
}

fn abort_all(batch: Vec<Command>, message: &str) {
    for cmd in batch {
        cmd.job.abort(message.to_owned());
    }
}

struct Savepointed {
    items: Vec<Staged>,
    /// Set when the savepoint machinery itself failed; the whole group must roll back.
    group_err: Option<String>,
}

fn run_in_savepoints(conn: &Connection, batch: Vec<Command>) -> Savepointed {
    let mut items = Vec::with_capacity(batch.len());
    let mut jobs = batch.into_iter();
    while let Some(cmd) = jobs.next() {
        if let Err(err) = conn.execute_batch("SAVEPOINT cmd") {
            let message = err.to_string();
            cmd.job.abort(message.clone());
            abort_all(jobs.collect(), &message);
            return Savepointed {
                items,
                group_err: Some(message),
            };
        }
        let staged = cmd.job.run(conn);
        let end = if staged.failed {
            "ROLLBACK TO cmd; RELEASE cmd"
        } else {
            "RELEASE cmd"
        };
        let end_result = conn.execute_batch(end);
        items.push(staged);
        if let Err(err) = end_result {
            let message = err.to_string();
            abort_all(jobs.collect(), &message);
            return Savepointed {
                items,
                group_err: Some(message),
            };
        }
    }
    Savepointed {
        items,
        group_err: None,
    }
}
