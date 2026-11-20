//! The message queue in words: what waits for a run's turn to end, and where a dragged
//! message lands. Pure; the composer draws the card (`ui/composer/queue.rs`).

/// The finding on a run with a queue; `None` when nothing waits.
pub(crate) fn queue_finding(waiting: usize) -> Option<String> {
    match waiting {
        0 => None,
        1 => Some("1 message waits for this turn to end".to_owned()),
        n => Some(format!("{n} messages wait for this turn to end")),
    }
}

/// Where a dragged message lands: the place of the row it is dropped on.
pub(crate) fn drop_place(len: usize, target: usize) -> usize {
    target.min(len.saturating_sub(1))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_finding_counts_what_waits_in_words() {
        assert_eq!(queue_finding(0), None);
        assert_eq!(
            queue_finding(1).as_deref(),
            Some("1 message waits for this turn to end")
        );
        assert_eq!(
            queue_finding(2).as_deref(),
            Some("2 messages wait for this turn to end")
        );
    }

    #[test]
    fn a_drop_never_lands_past_the_last_place() {
        assert_eq!(drop_place(3, 1), 1);
        assert_eq!(drop_place(3, 9), 2);
        assert_eq!(drop_place(0, 0), 0);
    }
}
