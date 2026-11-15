//! Time specifications: durations, instants, cron expressions and time zones.
//!
//! Pure checks of shape and range. Nothing here reads a clock.

use std::time::Duration;

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

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];
const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];

struct Field {
    name: &'static str,
    min: u32,
    max: u32,
    /// Names that stand for `names_from`, `names_from + 1`, and so on.
    names: &'static [&'static str],
    names_from: u32,
}

const FIELDS: [Field; 5] = [
    Field {
        name: "minute",
        min: 0,
        max: 59,
        names: &[],
        names_from: 0,
    },
    Field {
        name: "hour",
        min: 0,
        max: 23,
        names: &[],
        names_from: 0,
    },
    Field {
        name: "day-of-month",
        min: 1,
        max: 31,
        names: &[],
        names_from: 0,
    },
    Field {
        name: "month",
        min: 1,
        max: 12,
        names: &MONTHS,
        names_from: 1,
    },
    Field {
        name: "day-of-week",
        min: 0,
        max: 7,
        names: &DAYS,
        names_from: 0,
    },
];

/// Checks a five-field cron expression: minute, hour, day of month, month, day of week.
///
/// Each field is a comma list of `*`, `n` or `a-b`, and `*` and ranges may be followed by
/// `/step`. Months and weekdays may be three-letter names. `L`, `W`, `#` and `?` are not
/// accepted, and a range must not run backwards.
pub(super) fn check_cron(expr: &str) -> Result<(), String> {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != FIELDS.len() {
        return Err(format!(
            "{expr:?} has {} fields; expected 5 (minute hour day-of-month month day-of-week)",
            parts.len()
        ));
    }
    for (part, field) in parts.iter().zip(&FIELDS) {
        check_field(part, field).map_err(|m| format!("{} field {part:?}: {m}", field.name))?;
    }
    Ok(())
}

fn check_field(text: &str, field: &Field) -> Result<(), String> {
    for item in text.split(',') {
        let (range, step) = match item.split_once('/') {
            Some((range, step)) => (range, Some(step)),
            None => (item, None),
        };
        if let Some(step) = step
            && !matches!(step.parse::<u32>(), Ok(n) if n >= 1)
        {
            return Err(format!(
                "the step {step:?} must be a whole number of at least 1"
            ));
        }
        if range == "*" {
            continue;
        }
        let (low, high) = match range.split_once('-') {
            Some((low, high)) => (value(low, field)?, value(high, field)?),
            None => {
                let single = value(range, field)?;
                if step.is_some() {
                    return Err(format!("a step needs `*` or a range, not {range:?}"));
                }
                (single, single)
            }
        };
        if low > high {
            return Err(format!("the range {range:?} runs backwards"));
        }
    }
    Ok(())
}

fn value(text: &str, field: &Field) -> Result<u32, String> {
    let lower = text.to_ascii_lowercase();
    let n = match field.names.iter().position(|name| *name == lower) {
        Some(index) => field.names_from + u32::try_from(index).unwrap_or(0),
        None => lower.parse::<u32>().map_err(|_| {
            let or_name = if field.names.is_empty() {
                ""
            } else {
                " or name"
            };
            format!("{text:?} is not a number{or_name}")
        })?,
    };
    if (field.min..=field.max).contains(&n) {
        Ok(n)
    } else {
        Err(format!("{n} is outside {}..={}", field.min, field.max))
    }
}
