//! Plan-window labelling helper used by the Codex driver.

/// Label a Codex rate-limit window by its span: a 5-hour session primary
/// (~18_000s) with a weekly secondary (604_800s). A bare "> 1 day = week" rule
/// mislabels the monthly window "Week"; thresholds in seconds leave the middle
/// gaps to the nearest label rather than guessing a plan.
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
