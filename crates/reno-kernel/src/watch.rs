//! The `Watch` port (ARCH-7, ARCH-10): how the kernel learns that files changed.
//!
//! The kernel never opens a file or a watcher. A trigger of kind `fs` hands the port a
//! [`WatchSpec`] (the paths or globs of the definition and its debounce) and gets back a
//! [`WatchStream`]: one `()` for each burst of changes after it has settled. The port is
//! implemented outside the kernel (`renod` uses `notify`) and faked in tests.
//!
//! Starting a watch can fail for reasons that pass (the platform refuses a new stream while the
//! machine is out of them). The scheduler then shows the trigger as failed and tries again after
//! a [`WatchBackoff`] delay, longer each time. That is a retry-on-error timer, armed only while
//! a start is failing; it never asks whether files changed (ARCH-10).
//!
//! Dropping the stream stops the watch. Each message means "something under these paths
//! changed"; the channel is small and bounded, and a change that arrives while one is pending
//! is the same signal.

use std::fmt;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::error::Result;

/// How long to wait before trying to start a failed watch again: `initial` after the first
/// failure, doubling after each, never more than `max`. A deadline that exists only while a
/// start is failing, not a poll (ARCH-10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchBackoff {
    /// The wait after the first failure.
    pub initial: Duration,
    /// The longest wait.
    pub max: Duration,
}

impl Default for WatchBackoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(250),
            max: Duration::from_secs(30),
        }
    }
}

impl WatchBackoff {
    /// The wait after `failures` failures in a row (at least 1).
    pub fn delay(&self, failures: u32) -> Duration {
        let factor = 2u32.saturating_pow(failures.saturating_sub(1));
        self.initial.saturating_mul(factor).min(self.max)
    }
}

/// What to watch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchSpec {
    /// The trigger asking, for the port's logs.
    pub trigger: String,
    /// Absolute paths, or globs, or paths starting with `~/`: as the definition wrote them. The
    /// port expands `~` and resolves what the platform reports.
    pub paths: Vec<String>,
    /// How long the paths must be quiet before a change is reported.
    pub debounce: Duration,
}

/// A running watch. Dropping it stops the watch.
pub struct WatchStream {
    /// One message per settled burst of changes. It closes if the port can no longer watch.
    pub changes: mpsc::Receiver<()>,
    _guard: Box<dyn Send + Sync>,
}

impl WatchStream {
    /// A stream of `changes`, kept running by `guard`: whatever the port must hold on to while
    /// the watch lives (a platform watcher, the task that debounces). It is dropped with the
    /// stream.
    pub fn new(changes: mpsc::Receiver<()>, guard: impl Send + Sync + 'static) -> Self {
        Self {
            changes,
            _guard: Box::new(guard),
        }
    }
}

impl fmt::Debug for WatchStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WatchStream").finish_non_exhaustive()
    }
}

/// The `Watch` port: file changes, event-driven (ARCH-10).
pub trait Watch: Send + Sync + fmt::Debug {
    /// Starts watching. An error is a path that cannot be watched, or a platform that refuses;
    /// the trigger is then shown as failed, with the message, and the start is tried again with
    /// backoff ([`WatchBackoff`]) until it works or the definition changes.
    fn watch(&self, spec: &WatchSpec) -> Result<WatchStream>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_backoff_doubles_to_its_ceiling_and_never_overflows() {
        let backoff = WatchBackoff {
            initial: Duration::from_millis(250),
            max: Duration::from_secs(30),
        };
        let secs = |failures| backoff.delay(failures).as_millis();
        assert_eq!(
            [secs(0), secs(1), secs(2), secs(3), secs(8), secs(9)],
            [250, 250, 500, 1_000, 30_000, 30_000]
        );
        assert_eq!(backoff.delay(u32::MAX), Duration::from_secs(30));
    }
}
