//! The wake port: "the wall clock may have jumped" (ARCH-10).
//!
//! A deadline is wall-clock time, but the timer that waits for it is monotonic, and on a Mac the
//! monotonic timer stops while the machine sleeps. Something outside the kernel (the daemon, on
//! the system's wake notification) calls [`WakeSignal::notify`] when the machine woke, and the
//! scheduler then reads the clock again and fires what came due while it slept. No periodic wake
//! is needed.
//!
//! A signal is a counter in a `watch` channel: it holds no queue, so a burst of notifications
//! coalesces into one wake-up and a notifier never waits (ARCH-11). The kernel does no I/O here:
//! the handle is in-memory and is cloned to whoever hears the system.

use std::sync::Arc;

use tokio::sync::watch;

/// A handle for telling the scheduler that the system woke. Clones share one signal.
#[derive(Debug, Clone)]
pub struct WakeSignal {
    tx: Arc<watch::Sender<u64>>,
}

impl Default for WakeSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl WakeSignal {
    /// A signal nobody has raised yet.
    pub fn new() -> Self {
        Self {
            tx: Arc::new(watch::channel(0).0),
        }
    }

    /// The system woke up: the wall clock may have jumped. Never blocks.
    pub fn notify(&self) {
        self.tx.send_modify(|count| *count = count.wrapping_add(1));
    }

    /// A receiver whose `changed()` resolves after each [`notify`](Self::notify).
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.tx.subscribe()
    }
}

/// Pending until the system wakes; for ever when there is no signal (or its notifier is gone).
/// Cancel-safe, so it can sit in a `select!`.
pub(crate) async fn system_woke(woke: &mut Option<watch::Receiver<u64>>) {
    if let Some(rx) = woke.as_mut()
        && rx.changed().await.is_ok()
    {
        return;
    }
    *woke = None;
    std::future::pending().await
}
