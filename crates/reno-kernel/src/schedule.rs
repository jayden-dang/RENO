//! When a trigger is due: the arithmetic of `at`, `after`, `every` and `cron`, and the backoff
//! steps. Pure: no clock, no store, no task. The scheduler (`scheduler.rs`) owns the state and
//! calls these with the time it read from the `Clock`.
//!
//! Times are milliseconds since the Unix epoch.
//!
//! # Anchors
//!
//! A schedule is a function of the definition and of one stored instant, never of when the
//! daemon happened to start, so two schedulers (or a restart) compute the same occurrences:
//!
//! - `at`: the instant in the definition.
//! - `after`: the instant the version of the definition was first stored, plus the delay.
//! - `every`: the same instant plus a whole number of intervals.
//! - `cron`: the zone's wall clock, through [`CronSchedule`], the parser that validated it.

use std::time::Duration;

use reno_proto::defs::{
    CronSchedule, MAX_MISSED_COUNT, TriggerKind, parse_duration, parse_instant_ms,
};

/// The first backoff, after the first failure to start.
pub(crate) const BACKOFF_FIRST: Duration = Duration::from_secs(5 * 60);
/// The longest backoff.
pub(crate) const BACKOFF_MAX: Duration = Duration::from_secs(24 * 3600);

/// How long a trigger is backed off after its `failures`-th consecutive failure (counted from
/// 1): 5 minutes, doubling at each failure, up to 24 hours.
pub(crate) fn backoff_step(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    BACKOFF_FIRST
        .saturating_mul(1 << doublings)
        .min(BACKOFF_MAX)
}

/// A trigger's `when`, parsed for scheduling.
#[derive(Debug, Clone)]
pub(crate) enum Schedule {
    /// Once, at an instant.
    At { at_ms: i64 },
    /// Once, a delay after the definition version was stored.
    After { delay_ms: i64 },
    /// Repeatedly, from the instant the definition version was stored.
    Every { interval_ms: i64 },
    /// On a cron expression in a zone.
    Cron {
        cron: Box<CronSchedule>,
        zone: String,
    },
    /// On file changes: no time.
    Fs,
    /// Only on request: no time.
    Manual,
}

/// What is due at a moment: how many occurrences have come due, and when the next one is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Due {
    /// Occurrences due at or before now, starting from the one asked about. At least 1, and at
    /// most [`MAX_MISSED_COUNT`].
    pub(crate) count: u32,
    /// The first occurrence after now. `None` for a one-shot, and for a cron with no later date.
    pub(crate) next: Option<i64>,
}

fn millis(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

impl Schedule {
    /// Parses what a validated definition says. An error means the definition was valid when
    /// stored and no longer is (the time-zone database changed under it), or is reserved.
    pub(crate) fn from_kind(kind: &TriggerKind) -> Result<Self, String> {
        let duration = |text: &str| parse_duration(text).map(millis);
        Ok(match kind {
            TriggerKind::At { at } => Self::At {
                at_ms: parse_instant_ms(at)?,
            },
            TriggerKind::After { after } => Self::After {
                delay_ms: duration(after)?,
            },
            TriggerKind::Every { every } => Self::Every {
                interval_ms: duration(every)?,
            },
            TriggerKind::Cron { cron, timezone } => Self::Cron {
                cron: Box::new(CronSchedule::new(cron, timezone)?),
                zone: timezone.clone(),
            },
            TriggerKind::Fs { .. } => Self::Fs,
            TriggerKind::Manual {} => Self::Manual,
            reserved => return Err(format!("kind {:?} is not available yet", reserved.name())),
        })
    }

    /// The time zone occurrences are shown in: the cron's zone, UTC for the rest.
    pub(crate) fn zone(&self) -> Option<&str> {
        match self {
            Self::Cron { zone, .. } => Some(zone),
            _ => None,
        }
    }

    /// When a new plan is first due, for a definition version stored at `anchor`, planned at
    /// `now`. `None` for a trigger with no time.
    ///
    /// An `at` or `after` already in the past is due now: it was asked for, and it is late, not
    /// skipped.
    pub(crate) fn first_due(&self, anchor: i64, now: i64) -> Option<i64> {
        match self {
            Self::At { at_ms } => Some(*at_ms),
            Self::After { delay_ms } => Some(anchor.saturating_add(*delay_ms)),
            Self::Every { interval_ms } => {
                let elapsed = now.saturating_sub(anchor).max(0);
                let intervals = elapsed / interval_ms + 1;
                Some(anchor.saturating_add(intervals.saturating_mul(*interval_ms)))
            }
            Self::Cron { cron, .. } => cron.next_after(now),
            Self::Fs | Self::Manual => None,
        }
    }

    /// What is due at `now`, for an occurrence due at `due` (`due <= now`).
    ///
    /// The occurrences from `due` up to `now` are counted, up to [`MAX_MISSED_COUNT`]; they are
    /// merged into one run by the caller. A count that stops at the limit is a lower bound, and
    /// `next` is then looked up from `now` instead of walked to.
    pub(crate) fn due_at(&self, due: i64, now: i64) -> Due {
        match self {
            Self::At { .. } | Self::After { .. } | Self::Fs | Self::Manual => Due {
                count: 1,
                next: None,
            },
            Self::Every { interval_ms } => {
                let passed = now.saturating_sub(due).max(0) / interval_ms + 1;
                Due {
                    count: u32::try_from(passed)
                        .unwrap_or(MAX_MISSED_COUNT)
                        .min(MAX_MISSED_COUNT),
                    next: Some(due.saturating_add(passed.saturating_mul(*interval_ms))),
                }
            }
            Self::Cron { cron, .. } => {
                let mut count = 1;
                let mut at = due;
                while count < MAX_MISSED_COUNT {
                    match cron.next_after(at) {
                        Some(next) if next <= now => {
                            count += 1;
                            at = next;
                        }
                        next => return Due { count, next },
                    }
                }
                Due {
                    count,
                    next: cron.next_after(now),
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3_600_000;

    /// A schedule from `when` as JSON, the shape the definitions are stored in.
    fn schedule(when: &str) -> Schedule {
        let kind: TriggerKind = serde_json::from_str(when).unwrap();
        Schedule::from_kind(&kind).unwrap()
    }

    #[test]
    fn backoff_doubles_from_five_minutes_to_a_day() {
        let minutes: Vec<u64> = (1..=11).map(|n| backoff_step(n).as_secs() / 60).collect();
        assert_eq!(
            minutes,
            [5, 10, 20, 40, 80, 160, 320, 640, 1280, 1440, 1440]
        );
        assert_eq!(backoff_step(0), BACKOFF_FIRST);
        assert_eq!(backoff_step(u32::MAX), BACKOFF_MAX);
    }

    #[test]
    fn at_is_once_and_a_past_instant_is_due_now() {
        let s = schedule(r#"{"kind":"at","at":"2026-12-01T09:00:00+07:00"}"#);
        let at = 1_796_090_400_000; // 2026-12-01T02:00:00Z
        assert_eq!(s.first_due(0, 0), Some(at));
        assert_eq!(s.first_due(0, at + 5 * HOUR), Some(at), "late, not skipped");
        assert_eq!(
            s.due_at(at, at + 5 * HOUR),
            Due {
                count: 1,
                next: None
            }
        );
    }

    #[test]
    fn after_is_relative_to_the_stored_version() {
        let s = schedule(r#"{"kind":"after","after":"30m"}"#);
        assert_eq!(s.first_due(1_000, 99_999_999), Some(1_000 + HOUR / 2));
    }

    #[test]
    fn every_counts_whole_intervals_from_the_anchor() {
        let s = schedule(r#"{"kind":"every","every":"1h"}"#);
        let anchor = 10 * HOUR + 7;
        // Planned at the anchor: the first occurrence is one interval later.
        assert_eq!(s.first_due(anchor, anchor), Some(anchor + HOUR));
        // Planned later: the next grid point after now.
        assert_eq!(
            s.first_due(anchor, anchor + 3 * HOUR + 1),
            Some(anchor + 4 * HOUR)
        );
        // Three intervals were missed: one run for the lot, then the grid goes on.
        let due = anchor + HOUR;
        assert_eq!(
            s.due_at(due, due + 2 * HOUR + 5),
            Due {
                count: 3,
                next: Some(due + 3 * HOUR)
            }
        );
        // On time: one, and the next is an interval on.
        assert_eq!(
            s.due_at(due, due),
            Due {
                count: 1,
                next: Some(due + HOUR)
            }
        );
    }

    #[test]
    fn cron_walks_the_occurrences_between_due_and_now() {
        let s = schedule(r#"{"kind":"cron","cron":"0 * * * *","timezone":"UTC"}"#);
        let due = 5 * HOUR;
        assert_eq!(s.first_due(0, due + 1), Some(6 * HOUR));
        assert_eq!(
            s.due_at(due, due + 2 * HOUR + 30),
            Due {
                count: 3,
                next: Some(8 * HOUR)
            }
        );
        assert_eq!(s.zone(), Some("UTC"));
    }

    #[test]
    fn the_missed_count_is_bounded() {
        let s = schedule(r#"{"kind":"cron","cron":"* * * * *","timezone":"UTC"}"#);
        // A year of minutes is more than the bound.
        let due = 60_000;
        let now = due + 366 * 24 * HOUR;
        let found = s.due_at(due, now);
        assert_eq!(found.count, MAX_MISSED_COUNT);
        assert!(found.next.unwrap() > now);
        let every = schedule(r#"{"kind":"every","every":"1m"}"#).due_at(due, now);
        assert_eq!(every.count, MAX_MISSED_COUNT);
        assert!(every.next.unwrap() > now);
    }

    #[test]
    fn fs_and_manual_have_no_time() {
        for when in [
            r#"{"kind":"fs","paths":["/tmp/x"]}"#,
            r#"{"kind":"manual"}"#,
        ] {
            assert_eq!(schedule(when).first_due(0, 100), None);
        }
    }
}
