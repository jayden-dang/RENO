//! Counters for the frames a wire layer did not recognise (ARCHITECTURE 5.5, T1.14 groundwork).
//!
//! A harness CLI that updates may start sending a frame the driver has never seen. It is never
//! fatal: the driver drops it and counts it here, per frame kind, so a rising number is the early
//! signal that the CLI moved before anything breaks. A frame the driver knows and deliberately
//! ignores (thinking accounting, status chatter) is *not* counted: a count means "we do not
//! know this one".
//!
//! One [`UnknownFrames`] belongs to one executor and is shared by its runs. The kinds are
//! bounded in number and length, so a harness cannot grow it without limit (ARCH-11).

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

/// How many distinct kinds are kept; further kinds count under `OVERFLOW_KEY`.
const MAX_KINDS: usize = 64;

/// The longest kind kept, in characters; longer names are cut.
const MAX_KIND_CHARS: usize = 80;

/// Where kinds beyond [`MAX_KINDS`] are counted.
const OVERFLOW_KEY: &str = "<other>";

/// A shared, bounded count of unrecognised frames per kind (`type:foo`, `method:thread/bar`,
/// `update:baz`).
#[derive(Debug, Clone, Default)]
pub struct UnknownFrames {
    counts: Arc<Mutex<BTreeMap<String, u64>>>,
}

impl UnknownFrames {
    /// An empty counter.
    pub fn new() -> Self {
        Self::default()
    }

    /// Count one unrecognised frame of `kind`.
    pub fn note(&self, kind: &str) {
        let kind: String = kind.chars().take(MAX_KIND_CHARS).collect();
        let mut counts = self.counts.lock().unwrap_or_else(PoisonError::into_inner);
        let key = if counts.contains_key(&kind) || counts.len() < MAX_KINDS {
            kind
        } else {
            OVERFLOW_KEY.to_owned()
        };
        let slot = counts.entry(key).or_insert(0);
        *slot = slot.saturating_add(1);
    }

    /// The counts so far, by kind.
    pub fn snapshot(&self) -> BTreeMap<String, u64> {
        self.counts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_per_kind() {
        let frames = UnknownFrames::new();
        frames.note("type:a");
        frames.note("type:a");
        frames.note("type:b");
        let seen = frames.snapshot();
        assert_eq!(seen.get("type:a"), Some(&2));
        assert_eq!(seen.get("type:b"), Some(&1));
    }

    #[test]
    fn clones_share_one_count() {
        let frames = UnknownFrames::new();
        let other = frames.clone();
        other.note("method:x");
        assert_eq!(frames.snapshot().get("method:x"), Some(&1));
    }

    #[test]
    fn the_number_and_length_of_kinds_are_bounded() {
        let frames = UnknownFrames::new();
        for i in 0..(MAX_KINDS + 10) {
            frames.note(&format!("type:{i}"));
        }
        let seen = frames.snapshot();
        assert_eq!(seen.len(), MAX_KINDS + 1);
        assert_eq!(seen.get(OVERFLOW_KEY), Some(&10));
        // A kind already kept still counts after the table is full.
        frames.note("type:0");
        assert_eq!(frames.snapshot().get("type:0"), Some(&2));
        frames.note(&"x".repeat(500));
        assert!(
            frames
                .snapshot()
                .keys()
                .all(|k| k.chars().count() <= MAX_KIND_CHARS.max(OVERFLOW_KEY.len()))
        );
    }
}
