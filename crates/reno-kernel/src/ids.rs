//! Minting ids with no entropy source (ARCH-7).
//!
//! The kernel owns no randomness port, so an id is built from the `Clock` and a counter:
//! 13 hex digits of the millisecond clock, then 6 of a per-process counter. The suffix sorts by
//! creation time and is unique within one process. Two processes, or two runs of one process
//! with a clock that went back, can still produce the same suffix; the store refuses a
//! duplicate with `AlreadyExists`, and the caller mints again (the counter has moved on).

use std::sync::atomic::{AtomicU64, Ordering};

/// Per-process counter behind every minted id.
#[derive(Debug, Default)]
pub(crate) struct IdMint {
    counter: AtomicU64,
}

impl IdMint {
    /// A fresh id suffix for `now_ms`: ASCII hex, 19 characters.
    pub(crate) fn suffix(&self, now_ms: i64) -> String {
        let n = self.counter.fetch_add(1, Ordering::Relaxed) & 0xFF_FFFF;
        format!("{:013x}{:06x}", now_ms.max(0), n)
    }
}

#[cfg(test)]
mod tests {
    use reno_proto::RunId;

    use super::*;

    #[test]
    fn suffixes_are_valid_unique_and_sorted() {
        let mint = IdMint::default();
        let a = mint.suffix(1_700_000_000_000);
        let b = mint.suffix(1_700_000_000_000);
        let c = mint.suffix(1_700_000_000_001);
        assert!(a < b && b < c, "{a} {b} {c}");
        assert!(RunId::from_suffix(&a).is_ok());
        assert_eq!(a.len(), 19);
    }

    #[test]
    fn a_negative_clock_does_not_break_the_shape() {
        assert!(RunId::from_suffix(&IdMint::default().suffix(-5)).is_ok());
    }
}
