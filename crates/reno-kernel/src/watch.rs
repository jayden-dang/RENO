//! The `Watch` port (ARCH-7, ARCH-10): how the kernel learns that files changed.
//!
//! The kernel never opens a file or a watcher. A trigger of kind `fs` hands the port a
//! [`WatchSpec`] (the paths or globs of the definition and its debounce) and gets back a
//! [`WatchStream`]: one `()` for each burst of changes after it has settled. The port is
//! implemented outside the kernel (`renod` uses `notify`) and faked in tests.
//!
//! Dropping the stream stops the watch. Each message means "something under these paths
//! changed"; the channel is small and bounded, and a change that arrives while one is pending
//! is the same signal.

use std::fmt;
use std::time::Duration;

use tokio::sync::mpsc;

use crate::error::Result;

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
    /// Starts watching. An error is a path that cannot be watched; the trigger is then shown as
    /// failed, with the message, until its definition changes.
    fn watch(&self, spec: &WatchSpec) -> Result<WatchStream>;
}
