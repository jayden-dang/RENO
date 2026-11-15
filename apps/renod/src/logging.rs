//! Tracing for the daemon process (ARCHITECTURE 17.8).
//!
//! JSON lines go to `<data-dir>/logs/renod.<date>.log` through a non-blocking appender, filtered
//! by `RENO_LOG` (an `EnvFilter` expression; `info` when unset or invalid). When the daemon is
//! attached to a terminal the same events also go to stderr in the human format.
//!
//! This is the binary's business: it sets the process-wide subscriber, so the library and its
//! tests never call it.

use std::io::{self, IsTerminal};
use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, fmt};

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
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("renod")
        .filename_suffix("log")
        .build(logs_dir)
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
    Ok(LogGuard { _worker: worker })
}
