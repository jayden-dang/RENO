//! Time specifications: durations, instants, cron expressions and time zones.
//!
//! Pure checks and pure arithmetic. Nothing here reads a clock: every function that needs "now"
//! takes it as an argument.
//!
//! # Cron
//!
//! [`CronSchedule`] is the one place a cron expression is parsed, by `croner`, and the one place
//! the next occurrence is computed. Validation (`check_cron`) and the scheduler in the kernel
//! both go through it, so an expression that validates is an expression that fires.
//!
//! The dialect is croner's, with the seconds and year fields switched off, so an expression has
//! exactly five fields (minute hour day-of-month month day-of-week) and nothing can fire more
//! often than once a minute. Croner also accepts `L`, `W`, `#`, `?` and the `@daily` family of
//! nicknames.
//!
//! ## Daylight saving time
//!
//! Croner follows Vixie cron, and the tests of this module pin the behaviour:
//!
//! - **Spring forward** (a wall-clock time that does not exist): a job with a fixed minute and
//!   hour (`30 2 * * *`) runs once that day, at the first instant after the gap. A job with a
//!   wildcard or a step in its minute or hour (`*/30 * * * *`) skips the times inside the gap.
//! - **Fall back** (a wall-clock time that happens twice): a fixed job runs once, at its first
//!   pass. A wildcard or step job runs at every matching time of both passes.
//!
//! So a daily job fires exactly once per calendar day in both directions.

use std::time::Duration;

use croner::Cron;
use croner::parser::{CronParser, Seconds, Year};
use jiff::tz::TimeZone;

/// The shortest `every` interval. Each firing starts an agent run, so a sub-minute schedule is
/// almost always a unit typo (`30s` for `30m`).
pub const MIN_EVERY: Duration = Duration::from_secs(60);

/// The longest timer: a year and a day, so a leap year fits.
pub(super) const MAX_TIMER: Duration = Duration::from_secs(366 * 24 * 3600);

/// Parses `45m`, `6h`, `1h30m`, `500ms`: one or more `<integer><unit>` parts, units `ms`, `s`,
/// `m`, `h`, `d`, in any order. Zero is rejected.
pub fn parse_duration(text: &str) -> Result<Duration, String> {
    if text.is_empty() {
        return Err("a duration is empty; write for example 45m or 6h".to_owned());
    }
    let mut rest = text;
    let mut total_ms: u64 = 0;
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return Err(format!(
                "{text:?} is not a duration: expected digits at {rest:?}"
            ));
        }
        let (number, tail) = rest.split_at(digits);
        let number: u64 = number
            .parse()
            .map_err(|_| format!("{text:?} is not a duration: {number} is too large"))?;
        let unit_len = tail.bytes().take_while(u8::is_ascii_lowercase).count();
        let (unit, next) = tail.split_at(unit_len);
        let unit_ms: u64 = match unit {
            "ms" => 1,
            "s" => 1_000,
            "m" => 60_000,
            "h" => 3_600_000,
            "d" => 86_400_000,
            "" => {
                return Err(format!(
                    "{text:?} is not a duration: {number} needs a unit (ms, s, m, h or d)"
                ));
            }
            other => {
                return Err(format!(
                    "{text:?} is not a duration: unknown unit {other:?} (use ms, s, m, h or d)"
                ));
            }
        };
        total_ms = number
            .checked_mul(unit_ms)
            .and_then(|part| total_ms.checked_add(part))
            .ok_or_else(|| format!("{text:?} is too long"))?;
        rest = next;
    }
    if total_ms == 0 {
        return Err(format!("{text:?} is zero"));
    }
    Ok(Duration::from_millis(total_ms))
}

/// Parses a duration and checks it against `min..=max`, with `what` naming it in the message.
pub(super) fn check_duration(
    text: &str,
    min: Duration,
    max: Duration,
    what: &str,
) -> Result<Duration, String> {
    let duration = parse_duration(text)?;
    if duration < min {
        return Err(format!(
            "{what} {text:?} is under the minimum of {}",
            show(min)
        ));
    }
    if duration > max {
        return Err(format!(
            "{what} {text:?} is over the maximum of {}",
            show(max)
        ));
    }
    Ok(duration)
}

fn show(d: Duration) -> String {
    let ms = d.as_millis();
    if ms.is_multiple_of(3_600_000) {
        format!("{}h", ms / 3_600_000)
    } else if ms.is_multiple_of(60_000) {
        format!("{}m", ms / 60_000)
    } else if ms.is_multiple_of(1_000) {
        format!("{}s", ms / 1_000)
    } else {
        format!("{ms}ms")
    }
}

/// Checks an RFC 3339 timestamp with an offset or `Z`, such as `2026-12-01T09:00:00+07:00`.
/// A timestamp with no offset is ambiguous and is refused.
pub(super) fn check_instant(text: &str) -> Result<(), String> {
    text.parse::<jiff::Timestamp>().map(|_| ()).map_err(|e| {
        format!(
            "{text:?} is not an RFC 3339 timestamp with an offset, \
             such as 2026-12-01T09:00:00+07:00 ({e})"
        )
    })
}

/// Checks an IANA time zone name against the bundled database.
pub(super) fn check_timezone(name: &str) -> Result<(), String> {
    let known = !name.is_empty() && name.len() <= 64 && jiff::tz::TimeZone::get(name).is_ok();
    if known {
        Ok(())
    } else {
        Err(format!(
            "{name:?} is not an IANA time zone name such as Asia/Ho_Chi_Minh"
        ))
    }
}

/// The dialect every cron expression in a definition is read in: five fields.
fn parser() -> CronParser {
    CronParser::builder()
        .seconds(Seconds::Disallowed)
        .year(Year::Disallowed)
        .build()
}

fn parse_cron(expr: &str) -> Result<Cron, String> {
    parser().parse(expr).map_err(|e| {
        format!(
            "{expr:?} is not a cron expression: {e} \
             (expected five fields: minute hour day-of-month month day-of-week)"
        )
    })
}

/// 2026-01-01T00:00:00Z, the instant `check_cron` searches from. Any fixed instant would do; the
/// search only has to prove that the expression can match.
const REFERENCE_MS: i64 = 1_767_225_600_000;

/// Checks a cron expression: it parses (see the module docs for the dialect) and it matches at
/// least once, so `0 0 31 2 *` is refused instead of never firing.
pub(super) fn check_cron(expr: &str) -> Result<(), String> {
    CronSchedule::with_zone(expr, TimeZone::UTC).map(|_| ())
}

/// A cron expression in a time zone: when it fires, as instants.
///
/// The type validation and the scheduler share (see the module docs).
#[derive(Debug, Clone)]
pub struct CronSchedule {
    cron: Cron,
    zone: TimeZone,
}

impl CronSchedule {
    /// Parses `expr` and looks `timezone` up. The error says which of the two is wrong.
    pub fn new(expr: &str, timezone: &str) -> Result<Self, String> {
        check_timezone(timezone)?;
        let zone = TimeZone::get(timezone).map_err(|e| format!("{timezone:?}: {e}"))?;
        Self::with_zone(expr, zone)
    }

    /// The expression must also match some date: `0 0 31 2 *` parses, and never fires.
    fn with_zone(expr: &str, zone: TimeZone) -> Result<Self, String> {
        let schedule = Self {
            cron: parse_cron(expr)?,
            zone,
        };
        if schedule.next_after(REFERENCE_MS).is_none() {
            return Err(format!("{expr:?} never matches any date"));
        }
        Ok(schedule)
    }

    /// The first occurrence strictly after `after_ms` (milliseconds since the Unix epoch), in
    /// the same unit, or `None` when there is none (the search is bounded: croner stops at the
    /// year 5000, and a pattern that cannot match gives up).
    ///
    /// Strictly after: feeding the result back in walks the occurrences one by one, and a
    /// fixed-time job in a repeated hour appears once (see the module docs).
    pub fn next_after(&self, after_ms: i64) -> Option<i64> {
        let from = jiff::Timestamp::from_millisecond(after_ms)
            .ok()?
            .to_zoned(self.zone.clone());
        let next = self.cron.find_next_occurrence(&from, false).ok()?;
        Some(next.timestamp().as_millisecond())
    }
}

/// An instant as RFC 3339 text with its offset, in `timezone` (an IANA name) or UTC when `None`
/// or unknown: `2026-12-01T02:00:00+07:00`. Seconds are the finest unit shown.
pub fn format_instant(ms: i64, timezone: Option<&str>) -> String {
    let Ok(instant) = jiff::Timestamp::from_millisecond(ms) else {
        return format!("{ms}ms");
    };
    let zone = timezone
        .and_then(|name| TimeZone::get(name).ok())
        .unwrap_or(TimeZone::UTC);
    instant
        .to_zoned(zone)
        .strftime("%Y-%m-%dT%H:%M:%S%:z")
        .to_string()
}

/// A span as `6h12m`, `45m`, `30s` or `0s`: the two largest units that are not zero, from days
/// down to seconds. Used for "started 6h12m late".
pub fn format_span(span: Duration) -> String {
    let total = span.as_secs();
    let parts = [
        (total / 86_400, "d"),
        (total % 86_400 / 3_600, "h"),
        (total % 3_600 / 60, "m"),
        (total % 60, "s"),
    ];
    let shown: String = parts
        .iter()
        .filter(|(n, _)| *n > 0)
        .take(2)
        .map(|(n, unit)| format!("{n}{unit}"))
        .collect();
    if shown.is_empty() {
        "0s".to_owned()
    } else {
        shown
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(rfc3339: &str) -> i64 {
        rfc3339.parse::<jiff::Timestamp>().unwrap().as_millisecond()
    }

    fn shown(ms: i64, zone: &str) -> String {
        format_instant(ms, Some(zone))
    }

    /// Every occurrence of `expr` in `zone` from `from` (exclusive) up to `to` (inclusive),
    /// walked one by one the way the scheduler does.
    fn occurrences(expr: &str, zone: &str, from: &str, to: &str) -> Vec<String> {
        let schedule = CronSchedule::new(expr, zone).unwrap();
        let end = ms(to);
        let mut at = ms(from);
        let mut out = Vec::new();
        while let Some(next) = schedule.next_after(at) {
            if next > end {
                break;
            }
            out.push(shown(next, zone));
            at = next;
        }
        out
    }

    #[test]
    fn a_daily_cron_is_the_next_wall_clock_time_in_its_zone() {
        let hcm = CronSchedule::new("0 2 * * *", "Asia/Ho_Chi_Minh").unwrap();
        let next = hcm.next_after(ms("2026-11-30T00:00:00Z")).unwrap();
        assert_eq!(shown(next, "Asia/Ho_Chi_Minh"), "2026-12-01T02:00:00+07:00");
        assert_eq!(format_instant(next, None), "2026-11-30T19:00:00+00:00");
        // Strictly after: asking at the occurrence itself gives the next day's.
        let after = hcm.next_after(next).unwrap();
        assert_eq!(after - next, 24 * 3_600_000);
    }

    #[test]
    fn spring_forward_a_fixed_time_in_the_gap_runs_once_after_the_gap() {
        // America/New_York 2026-03-08: 02:00 -> 03:00, so 02:30 does not exist.
        let got = occurrences(
            "30 2 * * *",
            "America/New_York",
            "2026-03-07T12:00:00Z",
            "2026-03-10T12:00:00Z",
        );
        assert_eq!(
            got,
            [
                "2026-03-08T03:00:00-04:00",
                "2026-03-09T02:30:00-04:00",
                "2026-03-10T02:30:00-04:00",
            ],
            "the day with the gap runs once, at the first instant after it: {got:?}"
        );
    }

    #[test]
    fn spring_forward_a_step_job_skips_the_gap() {
        let got = occurrences(
            "*/30 * * * *",
            "America/New_York",
            "2026-03-08T06:00:00Z",
            "2026-03-08T08:00:00Z",
        );
        // 01:00 EST (06:00Z) is excluded as the start. Nothing between 01:30 and 03:00.
        assert_eq!(
            got,
            [
                "2026-03-08T01:30:00-05:00",
                "2026-03-08T03:00:00-04:00",
                "2026-03-08T03:30:00-04:00",
                "2026-03-08T04:00:00-04:00",
            ]
        );
    }

    #[test]
    fn fall_back_a_fixed_time_in_the_repeated_hour_runs_once() {
        // America/New_York 2026-11-01: 02:00 EDT -> 01:00 EST, so 01:30 happens twice.
        let got = occurrences(
            "30 1 * * *",
            "America/New_York",
            "2026-10-31T12:00:00Z",
            "2026-11-03T12:00:00Z",
        );
        assert_eq!(
            got,
            [
                "2026-11-01T01:30:00-04:00",
                "2026-11-02T01:30:00-05:00",
                "2026-11-03T01:30:00-05:00",
            ],
            "the repeated 01:30 runs at its first pass only: {got:?}"
        );
    }

    #[test]
    fn fall_back_a_step_job_runs_in_both_passes() {
        let got = occurrences(
            "*/30 * * * *",
            "America/New_York",
            "2026-11-01T04:30:00Z",
            "2026-11-01T07:00:00Z",
        );
        assert_eq!(
            got,
            [
                "2026-11-01T01:00:00-04:00",
                "2026-11-01T01:30:00-04:00",
                "2026-11-01T01:00:00-05:00",
                "2026-11-01T01:30:00-05:00",
                "2026-11-01T02:00:00-05:00",
            ]
        );
    }

    #[test]
    fn a_daily_job_fires_exactly_once_per_calendar_day_across_both_transitions() {
        for (from, to, days) in [
            ("2026-03-06T00:00:00Z", "2026-03-11T23:59:00Z", 6),
            ("2026-10-30T00:00:00Z", "2026-11-04T23:59:00Z", 6),
        ] {
            let got = occurrences("30 2 * * *", "America/New_York", from, to);
            assert_eq!(got.len(), days, "{got:?}");
            let dates: Vec<&str> = got.iter().map(|s| &s[..10]).collect();
            let mut unique = dates.clone();
            unique.dedup();
            assert_eq!(dates, unique, "one per calendar day: {got:?}");
        }
    }

    #[test]
    fn validation_is_the_parser_the_scheduler_uses() {
        for good in [
            "0 2 * * *",
            "*/15 9-17 * * mon-fri",
            "0 6 * jan,jul MON",
            "0 0 * * 7",
            "0 9 L * *",
            "0 9 * * 5#2",
            "@daily",
            "10-50/10 * * * *",
        ] {
            assert!(check_cron(good).is_ok(), "{good}");
            assert!(
                CronSchedule::new(good, "UTC")
                    .unwrap()
                    .next_after(0)
                    .is_some()
            );
        }
        for bad in [
            "",
            "0 2 * *",
            "0 0 2 * * *",
            "0 0 2 * * * 2030",
            "60 2 * * *",
            "0 24 * * *",
            "0 2 * 13 *",
            "*/0 2 * * *",
            "0 0 31 2 *",
            "every day",
        ] {
            assert!(check_cron(bad).is_err(), "{bad:?} should be refused");
            assert!(CronSchedule::new(bad, "UTC").is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_leap_day_is_found_and_a_missing_date_is_bounded() {
        let leap = CronSchedule::new("0 0 29 2 *", "UTC").unwrap();
        let next = leap.next_after(ms("2026-03-01T00:00:00Z")).unwrap();
        assert_eq!(format_instant(next, None), "2028-02-29T00:00:00+00:00");
        // Feb 31 never exists: refused, and the search that found out comes back at once.
        let never = CronSchedule::new("0 0 31 2 *", "UTC").unwrap_err();
        assert!(never.contains("never matches"), "{never}");
    }

    #[test]
    fn instants_and_spans_are_shown_for_people() {
        assert_eq!(format_instant(0, None), "1970-01-01T00:00:00+00:00");
        assert_eq!(
            format_instant(ms("2026-12-01T02:00:00+07:00"), Some("Asia/Ho_Chi_Minh")),
            "2026-12-01T02:00:00+07:00"
        );
        assert_eq!(
            format_instant(0, Some("No/Such_Zone")),
            "1970-01-01T00:00:00+00:00"
        );
        let span = |s| format_span(Duration::from_secs(s));
        assert_eq!(span(0), "0s");
        assert_eq!(span(59), "59s");
        assert_eq!(span(6 * 3600 + 12 * 60 + 40), "6h12m");
        assert_eq!(span(3 * 3600), "3h");
        assert_eq!(span(90_000), "1d1h");
        assert_eq!(span(86_400), "1d");
    }
}
