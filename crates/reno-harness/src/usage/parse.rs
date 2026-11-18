//! The replies of Claude's `/api/oauth/usage` and Codex's `/wham/usage` as meters. Pure.
//! Ported from zeron's `plan_usage.rs`.

use chrono::DateTime;
use reno_proto::UsageWindow;
use serde_json::Value;

use super::UsageReading;

/// Claude's `/api/oauth/usage`: the 5-hour session and weekly buckets, each a 0-100
/// `utilization` with an RFC 3339 `resets_at`.
pub fn claude(body: &Value) -> Option<UsageReading> {
    let mut windows = Vec::new();
    for (key, label) in [("five_hour", "Session"), ("seven_day", "Week")] {
        if let Some(w) = body.get(key)
            && let Some(utilization) = w.get("utilization").and_then(Value::as_f64)
        {
            windows.push(UsageWindow::new(
                label,
                utilization / 100.0,
                parse_when_ms(w.get("resets_at")),
            ));
        }
    }
    (!windows.is_empty()).then_some(UsageReading {
        plan_label: None,
        windows,
    })
}

/// Codex `/wham/usage`: the primary and secondary windows plus the live plan ("free", "plus",
/// "pro"), which beats the login-time JWT claim.
pub fn codex(body: &Value) -> Option<UsageReading> {
    let rate_limit = body.get("rate_limit")?;
    let mut windows = Vec::new();
    for key in ["primary_window", "secondary_window"] {
        if let Some(w) = rate_limit.get(key)
            && let Some(used) = w.get("used_percent").and_then(Value::as_f64)
        {
            let span = w
                .get("limit_window_seconds")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            windows.push(UsageWindow::new(
                codex_window_label(span),
                used / 100.0,
                parse_when_ms(w.get("reset_at")),
            ));
        }
    }
    if windows.is_empty() {
        return None;
    }
    let plan = body
        .get("plan_type")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    Some(UsageReading {
        plan_label: codex_plan(plan),
        windows,
    })
}

/// "ChatGPT Plus" from Codex's `plan_type`.
pub fn codex_plan(plan: Option<&str>) -> Option<String> {
    let plan = plan?;
    let mut chars = plan.chars();
    let first = chars.next()?;
    Some(format!(
        "ChatGPT {}{}",
        first.to_uppercase(),
        chars.as_str()
    ))
}

/// Meter label for a Codex rate-limit window from its `limit_window_seconds`: the free tier's
/// window is a 30-day month (2_592_000 s), Plus runs a 5-hour primary (about 18_000 s) with a
/// weekly secondary (604_800 s). A bare "more than a day is a week" rule mislabelled the monthly
/// window "Week"; thresholds in seconds leave the gaps to the nearest label rather than
/// guessing a plan.
pub fn codex_window_label(span_seconds: i64) -> &'static str {
    const DAY: i64 = 86_400;
    if span_seconds >= 28 * DAY {
        "Month"
    } else if span_seconds >= 5 * DAY {
        "Week"
    } else {
        "Session"
    }
}

/// A reset time, as Unix seconds or RFC 3339, in milliseconds since the Unix epoch.
pub fn parse_when_ms(value: Option<&Value>) -> Option<u64> {
    let millis = match value? {
        Value::Number(n) => n.as_i64()?.checked_mul(1000)?,
        Value::String(s) => DateTime::parse_from_rfc3339(s).ok()?.timestamp_millis(),
        _ => return None,
    };
    u64::try_from(millis).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn fractions(reading: &UsageReading) -> Vec<f32> {
        reading
            .windows
            .iter()
            .map(UsageWindow::used_fraction)
            .collect()
    }

    #[test]
    fn claude_buckets_map_to_session_and_week() {
        let body = json!({
            "five_hour": { "utilization": 6.0, "resets_at": "2026-04-08T18:59:59Z" },
            "seven_day": { "utilization": 35.0, "resets_at": "2026-04-14T16:59:59Z" },
            "extra_usage": { "is_enabled": true },
        });
        let reading = claude(&body).expect("windows");
        assert_eq!(reading.plan_label, None);
        let labels: Vec<_> = reading.windows.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, ["Session", "Week"]);
        assert_eq!(fractions(&reading), [0.06, 0.35]);
        assert_eq!(reading.windows[1].resets_at_ms, Some(1_776_185_999_000));
    }

    #[test]
    fn claude_without_any_bucket_is_unavailable() {
        assert!(claude(&json!({ "extra_usage": {} })).is_none());
        assert!(claude(&json!({ "five_hour": {} })).is_none());
    }

    #[test]
    fn claude_utilization_over_100_is_clamped_and_a_bad_reset_is_dropped() {
        let reading = claude(&json!({
            "five_hour": { "utilization": 140.0, "resets_at": "tomorrow" },
        }))
        .expect("windows");
        assert_eq!(fractions(&reading), [1.0]);
        assert_eq!(reading.windows[0].resets_at_ms, None);
    }

    #[test]
    fn codex_windows_and_live_plan() {
        let body = json!({
            "plan_type": "plus",
            "rate_limit": {
                "primary_window": {
                    "used_percent": 12.0, "limit_window_seconds": 18_000, "reset_at": 1_700_000_000
                },
                "secondary_window": {
                    "used_percent": 40.0, "limit_window_seconds": 604_800,
                    "reset_at": "2027-01-15T08:00:00Z"
                },
            },
        });
        let reading = codex(&body).expect("windows");
        assert_eq!(reading.plan_label.as_deref(), Some("ChatGPT Plus"));
        let labels: Vec<_> = reading.windows.iter().map(|w| w.label.as_str()).collect();
        assert_eq!(labels, ["Session", "Week"]);
        assert_eq!(fractions(&reading), [0.12, 0.4]);
        assert_eq!(reading.windows[0].resets_at_ms, Some(1_700_000_000_000));
        assert_eq!(reading.windows[1].resets_at_ms, Some(1_800_000_000_000));
    }

    #[test]
    fn codex_free_tier_month_window_is_not_called_a_week() {
        assert_eq!(codex_window_label(2_592_000), "Month");
        assert_eq!(codex_window_label(604_800), "Week");
        assert_eq!(codex_window_label(18_000), "Session");
        assert_eq!(codex_window_label(0), "Session");
        assert_eq!(codex_window_label(3 * 86_400), "Session");
    }

    #[test]
    fn codex_without_windows_is_unavailable_and_plan_is_capitalised() {
        assert!(codex(&json!({ "plan_type": "pro" })).is_none());
        assert!(codex(&json!({ "rate_limit": {} })).is_none());
        assert_eq!(codex_plan(Some("pro")).as_deref(), Some("ChatGPT Pro"));
        assert_eq!(codex_plan(Some("")), None);
        assert_eq!(codex_plan(None), None);
    }

    #[test]
    fn reset_times_are_seconds_or_rfc3339() {
        assert_eq!(
            parse_when_ms(Some(&json!(1_800_000_000))),
            Some(1_800_000_000_000)
        );
        assert_eq!(
            parse_when_ms(Some(&json!("2026-04-08T18:59:59Z"))),
            Some(1_775_674_799_000)
        );
        assert_eq!(parse_when_ms(Some(&json!(null))), None);
        assert_eq!(parse_when_ms(Some(&json!(-5))), None);
        assert_eq!(parse_when_ms(None), None);
    }
}
