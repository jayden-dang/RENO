//! Tracing for the daemon process (ARCHITECTURE 17.8).
//!
//! JSON lines go to `<data-dir>/logs/renod.log` through a non-blocking writer that rotates by
//! size ([`rotate`]: 10 MiB, 3 files, so at most 30 MiB), filtered by `RENO_LOG` (an
//! `EnvFilter` expression; `info` when unset or invalid). When the daemon is attached to a
//! terminal the same events also go to stderr in the human format.
//!
//! A second, small file, `renod.stderr`, is where a detached daemon's own stderr goes (set by
//! `reno daemon start` or by the LaunchAgent): what is printed before logging is up, or by a
//! library that does not use `tracing`. A panic is also logged as an event. The launcher
//! appends to that file and never rotates it, so the daemon trims it at start
//! ([`bound_stderr_file`]).
//!
//! This is the binary's business: it sets the process-wide subscriber, so the library and its
//! tests never call it.

use std::io::{self, IsTerminal};
use std::os::fd::AsFd;
use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

pub mod rotate;

/// The log file's name inside the log directory.
pub const LOG_FILE: &str = "renod.log";
/// The name of the file a detached daemon's stderr goes to.
pub const STDERR_FILE: &str = "renod.stderr";
/// The size past which `renod.stderr` is emptied at start.
pub const STDERR_LIMIT: u64 = 1024 * 1024;

/// The environment variable that holds the log filter.
pub const FILTER_ENV: &str = "RENO_LOG";

/// Keeps the background log writer alive. Drop it last: it flushes the queue on the way out.
#[must_use = "dropping the guard stops the log writer"]
pub struct LogGuard {
    _worker: WorkerGuard,
}

impl std::fmt::Debug for LogGuard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogGuard").finish_non_exhaustive()
    }
}

/// Why logging could not start.
#[derive(Debug, thiserror::Error)]
pub enum LogError {
    /// The log directory or file could not be created.
    #[error("cannot open the log directory {}: {reason}", dir.display())]
    Open {
        /// The directory.
        dir: std::path::PathBuf,
        /// What went wrong.
        reason: String,
    },
    /// A subscriber was already installed in this process.
    #[error("a tracing subscriber is already installed")]
    AlreadySet,
}

/// Starts logging into `logs_dir`, which must exist.
pub fn init(logs_dir: &Path) -> Result<LogGuard, LogError> {
    let appender =
        rotate::RotatingFile::open(logs_dir.join(LOG_FILE), rotate::MAX_BYTES, rotate::FILES)
            .map_err(|e| LogError::Open {
                dir: logs_dir.to_path_buf(),
                reason: e.to_string(),
            })?;
    let (file, worker) = tracing_appender::non_blocking(appender);

    let filter = EnvFilter::try_from_env(FILTER_ENV).unwrap_or_else(|_| EnvFilter::new("info"));
    let human = io::stderr().is_terminal().then(|| {
        fmt::layer()
            .with_writer(io::stderr)
            .with_target(false)
            .compact()
    });
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().json().with_ansi(false).with_writer(file))
        .with(human)
        .try_init()
        .map_err(|_| LogError::AlreadySet)?;
    std::panic::set_hook(Box::new(|info| {
        tracing::error!(panic = %info, "renod panicked");
    }));
    Ok(LogGuard { _worker: worker })
}

/// Empties `file` when it is a regular file longer than `limit`. For the daemon's own stderr
/// when a launcher pointed it at a file: the launcher appends and never truncates. Anything else
/// (a terminal, a pipe, /dev/null) is left alone. Returns whether it was emptied.
pub fn bound_stderr_file(file: impl AsFd, limit: u64) -> bool {
    let Ok(stat) = rustix::fs::fstat(&file) else {
        return false;
    };
    let regular =
        rustix::fs::FileType::from_raw_mode(stat.st_mode) == rustix::fs::FileType::RegularFile;
    let too_big = u64::try_from(stat.st_size).is_ok_and(|size| size > limit);
    if !(regular && too_big && rustix::fs::ftruncate(&file, 0).is_ok()) {
        return false;
    }
    // A writer that did not open the file for appending (launchd may not) keeps its offset, and
    // the next write would leave a hole of zeros. The offset is shared with ours: reset it.
    let _ = rustix::fs::seek(&file, rustix::fs::SeekFrom::Start(0));
    true
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    #[test]
    fn an_oversized_stderr_file_is_emptied_and_other_files_are_left_alone() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("renod.stderr");
        // Not opened for appending, like a writer that keeps its own offset.
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&path)
            .expect("open");
        file.write_all(&[b'x'; 100]).expect("write");
        assert!(!bound_stderr_file(&file, 100), "at the limit is fine");
        assert_eq!(std::fs::metadata(&path).expect("meta").len(), 100);
        file.write_all(b"y").expect("write");
        assert!(bound_stderr_file(&file, 100));
        assert_eq!(std::fs::metadata(&path).expect("meta").len(), 0);
        // The offset went back to the start, so the next write is not a hole.
        file.write_all(b"after").expect("write");
        assert_eq!(std::fs::read(&path).expect("read"), b"after");

        let null = std::fs::File::open("/dev/null").expect("null");
        assert!(!bound_stderr_file(&null, 0));
    }
}
