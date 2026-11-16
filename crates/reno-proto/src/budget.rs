//! Budgets on the wire (ARCHITECTURE section 8, items 6 and 7): what stops a run, what a bot may
//! still spend today, and the kill switch (`budget.status`, `budget.pause`, `budget.resume`).
//!
//! # A run that hit a limit
//!
//! A run stopped by a budget ends `interrupted` and its journal's last event is
//! `Done { stop: interrupted, diagnostic }`. The diagnostic is [`BudgetStop::to_diagnostic`]:
//! the tag `BudgetExceeded:` and the [`BudgetStop`] as JSON. A client reads it with
//! [`BudgetStop::from_diagnostic`], which is the one decoder; nobody matches the text. The
//! existing event and status types are unchanged, so an older client shows the diagnostic as it
//! shows any other.
//!
//! # Days
//!
//! A bot's tokens are counted per local day. [`LocalZone`] says which day a moment falls in and
//! when the next one starts, with the zone's own rules (a 23-hour or 25-hour day is a day).

use std::fmt;

use jiff::Timestamp;
use jiff::tz::TimeZone;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::RunId;

/// What a budget limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BudgetKind {
    /// How long one run may take (`budget.max_wall_time`). Limits are in milliseconds.
    WallTime,
    /// What a bot may spend per local day (`budget.daily_token_ceiling`). Limits are in tokens,
    /// input and output together.
    Tokens,
}

impl BudgetKind {
    /// The name on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::WallTime => "wall_time",
            Self::Tokens => "tokens",
        }
    }
}

/// The tag a budget stop's diagnostic starts with.
pub const BUDGET_DIAGNOSTIC_TAG: &str = "BudgetExceeded:";

/// A limit a run reached: why it was stopped, or why a new one was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BudgetStop {
    /// Which limit.
    pub kind: BudgetKind,
    /// The limit: milliseconds for `wall_time`, tokens for `tokens`.
    pub limit: u64,
    /// What had been used when it was noticed, in the same unit.
    pub used: u64,
}

impl BudgetStop {
    /// The diagnostic of a `Done` that a budget caused: [`BUDGET_DIAGNOSTIC_TAG`], a space and
    /// this value as JSON.
    pub fn to_diagnostic(&self) -> String {
        let body = serde_json::to_string(self).unwrap_or_default();
        format!("{BUDGET_DIAGNOSTIC_TAG} {body}")
    }

    /// The stop a `Done` diagnostic carries, or `None` for any other diagnostic.
    pub fn from_diagnostic(diagnostic: &str) -> Option<Self> {
        let body = diagnostic.strip_prefix(BUDGET_DIAGNOSTIC_TAG)?;
        serde_json::from_str(body.trim()).ok()
    }
}

/// The `data` of an error with code `BudgetExceeded`: which bot was refused and the limit it
/// reached. Read it with `serde_json::from_value`; the message is for people.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BudgetRefusal {
    /// The bot.
    pub bot: String,
    /// The limit and what had been used.
    pub stop: BudgetStop,
}

impl fmt::Display for BudgetStop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            BudgetKind::WallTime => write!(
                f,
                "the run reached its wall-time limit of {} (used {})",
                span(self.limit),
                span(self.used)
            ),
            BudgetKind::Tokens => write!(
                f,
                "the bot reached its daily token ceiling of {} (used {})",
                self.limit, self.used
            ),
        }
    }
}

/// Milliseconds as `1h 2m 3s` (the largest units that are not zero; `0s` for none).
fn span(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    let mut parts = Vec::new();
    if h > 0 {
        parts.push(format!("{h}h"));
    }
    if m > 0 {
        parts.push(format!("{m}m"));
    }
    if s > 0 || parts.is_empty() {
        parts.push(format!("{s}s"));
    }
    parts.join(" ")
}

/// A time zone that decides which day a moment belongs to. The daemon uses the machine's; tests
/// name one.
#[derive(Debug, Clone)]
pub struct LocalZone(TimeZone);

impl LocalZone {
    /// UTC: days are the calendar days of UTC.
    pub fn utc() -> Self {
        Self(TimeZone::UTC)
    }

    /// The machine's zone (UTC when it cannot be found).
    pub fn system() -> Self {
        Self(TimeZone::system())
    }

    /// An IANA zone by name, such as `America/New_York`.
    pub fn named(name: &str) -> Result<Self, String> {
        TimeZone::get(name)
            .map(Self)
            .map_err(|error| format!("unknown time zone {name:?}: {error}"))
    }

    /// The local day `at_ms` (milliseconds since the epoch) falls in, as `YYYY-MM-DD`.
    pub fn day_of(&self, at_ms: i64) -> String {
        match Timestamp::from_millisecond(at_ms) {
            Ok(instant) => instant.to_zoned(self.0.clone()).date().to_string(),
            Err(_) => String::from("0000-00-00"),
        }
    }

    /// When the local day after the one `at_ms` falls in starts, in milliseconds since the
    /// epoch. Always later than `at_ms`.
    pub fn next_day_start_ms(&self, at_ms: i64) -> i64 {
        let Ok(instant) = Timestamp::from_millisecond(at_ms) else {
            return at_ms.saturating_add(86_400_000);
        };
        let today = instant.to_zoned(self.0.clone()).date();
        today
            .tomorrow()
            .ok()
            .and_then(|date| date.to_zoned(self.0.clone()).ok())
            .map(|start| start.timestamp().as_millisecond())
            .filter(|start| *start > at_ms)
            .unwrap_or_else(|| at_ms.saturating_add(86_400_000))
    }
}

/// Params of `budget.status`: none.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct BudgetStatusParams {}

/// One bot's budget and what it has used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BotBudgetStatus {
    /// The bot.
    pub bot: String,
    /// Runs of the bot that hold a slot now (claimed and not ended).
    pub live_runs: u32,
    /// The most it may have live: `budget.max_concurrent_runs` of its definition in force.
    pub max_concurrent_runs: u32,
    /// Its runs waiting to be claimed.
    pub queued_runs: u32,
    /// Tokens (input and output) its runs have used today.
    pub tokens_today: u64,
    /// The most it may use per day; absent for no ceiling.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_token_ceiling: Option<u64>,
    /// The longest one run may take, in milliseconds.
    pub max_wall_time_ms: u64,
    /// Whether the bot is paused by itself (`budget.pause` with a bot). A global pause is
    /// `BudgetStatusResult::paused`.
    pub paused: bool,
}

/// Result of `budget.status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BudgetStatusResult {
    /// Whether everything is paused (`budget.pause` with no bot).
    pub paused: bool,
    /// The local day the token counts are for, `YYYY-MM-DD`.
    pub day: String,
    /// When that day ends and the counts start again, in milliseconds since the epoch.
    pub day_ends_at_ms: u64,
    /// The longest a run with no bot may take, in milliseconds.
    pub default_wall_time_ms: u64,
    /// Every bot in the definitions in force, and any other that is paused, by name.
    pub bots: Vec<BotBudgetStatus>,
}

/// Params of `budget.pause`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BudgetPauseParams {
    /// The bot to pause; everything when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot: Option<String>,
    /// Also stop the runs that are live now (each ends `interrupted`). Without it they go on.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stop_live: bool,
}

/// Result of `budget.pause`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BudgetPauseResult {
    /// False when it was paused already (and nothing but `stop_live` was done).
    pub changed: bool,
    /// The live runs that were told to stop.
    #[serde(default)]
    pub stopped: Vec<RunId>,
}

/// Params of `budget.resume`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BudgetResumeParams {
    /// The bot to resume; the global pause when absent. A global pause and a bot's own are
    /// separate: resuming one leaves the other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot: Option<String>,
}

/// Result of `budget.resume`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BudgetResumeResult {
    /// False when it was not paused.
    pub changed: bool,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const HOUR: i64 = 3_600_000;

    fn zone(name: &str) -> LocalZone {
        LocalZone::named(name).unwrap()
    }

    fn at(text: &str) -> i64 {
        text.parse::<Timestamp>().unwrap().as_millisecond()
    }

    #[test]
    fn a_stop_round_trips_through_its_diagnostic() {
        let stop = BudgetStop {
            kind: BudgetKind::WallTime,
            limit: 2_700_000,
            used: 2_700_004,
        };
        let text = stop.to_diagnostic();
        assert!(text.starts_with("BudgetExceeded: {"), "{text}");
        assert_eq!(BudgetStop::from_diagnostic(&text), Some(stop));
        assert_eq!(BudgetStop::from_diagnostic("NoEligibleExecutor: x"), None);
        assert_eq!(BudgetStop::from_diagnostic("BudgetExceeded: nope"), None);
    }

    #[test]
    fn stops_read_as_sentences() {
        let wall = BudgetStop {
            kind: BudgetKind::WallTime,
            limit: 2_700_000,
            used: 2_701_000,
        };
        assert!(wall.to_string().contains("45m"), "{wall}");
        let tokens = BudgetStop {
            kind: BudgetKind::Tokens,
            limit: 100,
            used: 120,
        };
        assert!(tokens.to_string().contains("100"), "{tokens}");
    }

    #[test]
    fn the_day_changes_at_local_midnight() {
        let ny = zone("America/New_York");
        // 2023-11-14T04:59:59Z is 23:59:59 on the 13th in New York (UTC-5); a second later is
        // the 14th.
        assert_eq!(ny.day_of(at("2023-11-14T04:59:59Z")), "2023-11-13");
        assert_eq!(ny.day_of(at("2023-11-14T05:00:00Z")), "2023-11-14");
        assert_eq!(
            ny.next_day_start_ms(at("2023-11-14T04:59:59Z")),
            at("2023-11-14T05:00:00Z")
        );
        let utc = LocalZone::utc();
        assert_eq!(utc.day_of(at("2023-11-14T23:59:59Z")), "2023-11-14");
        assert_eq!(utc.day_of(at("2023-11-15T00:00:00Z")), "2023-11-15");
    }

    #[test]
    fn a_day_with_a_dst_change_is_one_day() {
        let ny = zone("America/New_York");
        // Spring forward 2024-03-10: the day is 23 hours long.
        let start = at("2024-03-10T05:00:00Z");
        assert_eq!(ny.day_of(start), "2024-03-10");
        assert_eq!(ny.next_day_start_ms(start), start + 23 * HOUR);
        assert_eq!(ny.day_of(start + 23 * HOUR - 1), "2024-03-10");
        assert_eq!(ny.day_of(start + 23 * HOUR), "2024-03-11");
        // Fall back 2024-11-03: the day is 25 hours long.
        let start = at("2024-11-03T04:00:00Z");
        assert_eq!(ny.day_of(start), "2024-11-03");
        assert_eq!(ny.next_day_start_ms(start), start + 25 * HOUR);
        assert_eq!(ny.day_of(start + 25 * HOUR - 1), "2024-11-03");
        assert_eq!(ny.day_of(start + 25 * HOUR), "2024-11-04");
    }

    #[test]
    fn a_day_that_skips_midnight_still_ends() {
        // Sao Paulo sprang forward at midnight in 2018, so 2018-11-04 had no 00:00: that day
        // starts at 01:00 (UTC-3 until then, UTC-2 after).
        let sp = zone("America/Sao_Paulo");
        let before = at("2018-11-04T02:59:59Z"); // 23:59:59 on the 3rd (UTC-3)
        assert_eq!(sp.day_of(before), "2018-11-03");
        let next = sp.next_day_start_ms(before);
        assert_eq!(next, at("2018-11-04T03:00:00Z"));
        assert_eq!(sp.day_of(next), "2018-11-04");
        assert_eq!(sp.next_day_start_ms(next), at("2018-11-05T02:00:00Z"));
    }

    #[test]
    fn an_unknown_zone_is_an_error() {
        assert!(LocalZone::named("Mars/Olympus").is_err());
    }
}
