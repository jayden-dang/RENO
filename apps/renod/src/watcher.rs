//! Starting the platform's file watcher, behind a factory.
//!
//! Both file watches of the daemon (the definitions, in [`defs`](crate::defs), and `fs` triggers,
//! in [`fswatch`](crate::fswatch)) start their `notify` watcher through a [`WatcherFactory`], so
//! tests can inject one that fails and so every live platform stream is counted.
//!
//! # Streams are scarce
//!
//! On macOS each `notify` watcher is one FSEvents stream, and the system has a finite pool of
//! them shared by every process, not a share per process (measured on a development Mac: two
//! processes each got 8 of about 16 free; the free count varied from 0 to 17 as other processes
//! came and went, and stayed low for seconds after a burst was released).
//! `FSEventStreamStart` refuses when none is free, and `notify` reports
//! "unable to start FSEvent stream". A start can therefore fail for a while and work later, when
//! another process lets go of its streams. The owners retry with backoff (see
//! `reno_kernel::WatchBackoff`). Each stream is held by one [`WatcherGuard`], which stops it when
//! dropped. [`Counting`] counts the live ones, so a test can tell that none leaks.

use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

/// What a watcher calls with each event, on the platform's thread.
pub type Handler = Box<dyn FnMut(notify::Result<notify::Event>) + Send + 'static>;

/// A running platform watcher. Dropping it stops the watcher (and its stream), waiting for the
/// platform thread to end.
pub struct WatcherGuard(#[allow(dead_code)] Box<dyn Send + Sync>);

impl WatcherGuard {
    /// A guard that keeps `inner` alive.
    pub fn new(inner: impl Send + Sync + 'static) -> Self {
        Self(Box::new(inner))
    }
}

impl fmt::Debug for WatcherGuard {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("WatcherGuard")
    }
}

/// Starts watchers.
pub trait WatcherFactory: Send + Sync + fmt::Debug {
    /// Watches every directory or file of `roots`, recursively, calling `handler` for each event.
    /// The error says what the platform refused.
    fn start(&self, roots: &[PathBuf], handler: Handler) -> Result<WatcherGuard, String>;
}

impl<T: WatcherFactory + ?Sized> WatcherFactory for Arc<T> {
    fn start(&self, roots: &[PathBuf], handler: Handler) -> Result<WatcherGuard, String> {
        (**self).start(roots, handler)
    }
}

/// The real watcher, `notify`'s platform backend.
#[derive(Debug, Default, Clone, Copy)]
pub struct NotifyWatchers;

impl WatcherFactory for NotifyWatchers {
    fn start(&self, roots: &[PathBuf], handler: Handler) -> Result<WatcherGuard, String> {
        let mut watcher: RecommendedWatcher = notify::recommended_watcher(handler)
            .map_err(|e| format!("cannot create a watcher: {e}"))?;
        for root in roots {
            watcher
                .watch(root, RecursiveMode::Recursive)
                .map_err(|e| format!("cannot watch {}: {e}", root.display()))?;
        }
        Ok(WatcherGuard::new(watcher))
    }
}

/// Counts the watchers started through `inner` that are still alive. A watcher is counted from
/// its start until its guard has been dropped and the stream behind it stopped, so a count that
/// does not return to zero is a leaked stream. For tests.
#[derive(Debug)]
pub struct Counting<F> {
    inner: F,
    live: Arc<AtomicUsize>,
}

/// One counted watcher. The fields drop in order: the stream stops, then the count goes down.
struct Counted {
    _guard: WatcherGuard,
    _count: Release,
}

struct Release(Arc<AtomicUsize>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl<F: WatcherFactory> Counting<F> {
    /// Counts the watchers of `inner`.
    pub fn new(inner: F) -> Self {
        Self {
            inner,
            live: Arc::default(),
        }
    }

    /// How many started watchers are alive.
    pub fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }
}

impl<F: WatcherFactory> WatcherFactory for Counting<F> {
    fn start(&self, roots: &[PathBuf], handler: Handler) -> Result<WatcherGuard, String> {
        let guard = self.inner.start(roots, handler)?;
        self.live.fetch_add(1, Ordering::SeqCst);
        Ok(WatcherGuard::new(Counted {
            _guard: guard,
            _count: Release(Arc::clone(&self.live)),
        }))
    }
}

/// Starts nothing real and keeps the handlers it is given, so a test can raise the events itself.
/// No platform stream is used, so a test with it does not depend on how many others are in use.
#[derive(Default)]
pub struct Captured {
    handlers: Mutex<Vec<Handler>>,
}

impl fmt::Debug for Captured {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Captured")
    }
}

impl Captured {
    /// Reports `event` to every watcher that was started.
    pub fn raise(&self, event: notify::Event) {
        for handler in self
            .handlers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter_mut()
        {
            handler(Ok(event.clone()));
        }
    }
}

impl WatcherFactory for Captured {
    fn start(&self, _roots: &[PathBuf], handler: Handler) -> Result<WatcherGuard, String> {
        self.handlers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(handler);
        Ok(WatcherGuard::new(()))
    }
}

/// Fails the first starts, as a platform out of streams does, then hands on to another factory.
/// For tests.
#[derive(Debug)]
pub struct FailingFirst<F> {
    inner: F,
    left: Mutex<u32>,
    attempts: AtomicUsize,
}

impl<F: WatcherFactory> FailingFirst<F> {
    /// Refuses the first `failures` starts with the message "unable to start FSEvent stream",
    /// then starts through `inner`.
    pub fn new(inner: F, failures: u32) -> Self {
        Self {
            inner,
            left: Mutex::new(failures),
            attempts: AtomicUsize::new(0),
        }
    }

    /// How many starts were asked for, failed or not.
    pub fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }
}

impl<F: WatcherFactory> WatcherFactory for FailingFirst<F> {
    fn start(&self, roots: &[PathBuf], handler: Handler) -> Result<WatcherGuard, String> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        {
            let mut left = self.left.lock().unwrap_or_else(|e| e.into_inner());
            if *left > 0 {
                *left -= 1;
                return Err("cannot watch: unable to start FSEvent stream".to_owned());
            }
        }
        self.inner.start(roots, handler)
    }
}
