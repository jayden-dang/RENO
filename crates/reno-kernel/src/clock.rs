//! The `Clock` port (ARCH-7): the only way the kernel learns what time it is.
//!
//! `now_ms` is wall-clock time in milliseconds since the Unix epoch, the unit the store stores.
//! Deadlines and durations are a different thing: they go through `tokio::time`, which tests
//! pause, so no test waits on a real clock (ARCH-16).
//!
//! This is the one module of the kernel allowed to read the system clock. `cargo xtask check`
//! enforces that by path: `SystemTime::now` is forbidden in every other kernel source.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use reno_proto::LocalZone;

/// A source of wall-clock time, in milliseconds since the Unix epoch.
pub trait Clock: Send + Sync {
    /// The current time.
    fn now_ms(&self) -> i64;

    /// The zone whose midnight ends a budget day. UTC unless a clock knows better.
    fn zone(&self) -> LocalZone {
        LocalZone::utc()
    }
}

/// The real clock. Used by the daemon; the only reader of the system clock in the kernel crate.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
            .unwrap_or(0)
    }

    fn zone(&self) -> LocalZone {
        LocalZone::system()
    }
}

/// A clock that moves only when told to, for tests.
#[derive(Debug, Default)]
pub struct ManualClock {
    now: AtomicI64,
    zone: Mutex<Option<LocalZone>>,
}

impl ManualClock {
    /// A clock showing `now_ms`.
    pub fn new(now_ms: i64) -> Self {
        Self {
            now: AtomicI64::new(now_ms),
            zone: Mutex::new(None),
        }
    }

    /// The zone this clock reports (UTC by default).
    pub fn set_zone(&self, zone: LocalZone) {
        *self.zone.lock().unwrap_or_else(PoisonError::into_inner) = Some(zone);
    }

    /// Set the time.
    pub fn set(&self, now_ms: i64) {
        self.now.store(now_ms, Ordering::SeqCst);
    }

    /// Move the time forward (or back, for a negative `by_ms`).
    pub fn advance(&self, by_ms: i64) {
        self.now.fetch_add(by_ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> i64 {
        self.now.load(Ordering::SeqCst)
    }

    fn zone(&self) -> LocalZone {
        self.zone
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or_else(LocalZone::utc)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manual_clock_moves_only_when_told() {
        let clock = ManualClock::new(10);
        assert_eq!(clock.now_ms(), 10);
        clock.advance(5);
        assert_eq!(clock.now_ms(), 15);
        clock.set(2);
        assert_eq!(clock.now_ms(), 2);
    }

    #[test]
    fn system_clock_is_after_2020() {
        assert!(SystemClock.now_ms() > 1_577_836_800_000);
    }
}
