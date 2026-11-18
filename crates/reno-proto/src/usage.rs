//! A signed-in harness account's plan usage: the meters ("Session 54%", "Week 21%") a client
//! shows next to an executor.
//!
//! The daemon reads them from the provider's own usage endpoint with the CLI's login (clients
//! never see a credential) and reports them on [`crate::ExecutorHealth::plan_usage`]. A missing
//! `plan_usage` means unavailable: no login of the kind that has meters, a rejected token, or
//! the endpoint did not answer. It is never a guess.
//!
//! The helpers here are pure so every client reads the meters the same way. The thresholds and
//! the "binding window" rule are zeron's, which they are ported from.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Feature: `ExecutorHealth` may carry `planUsage`, and `executor.refresh` takes `usageOnly`.
pub const FEATURE_EXECUTOR_USAGE: &str = "executor.usage";

/// From this fraction a meter is a warning (amber).
pub const USAGE_WARN_FRACTION: f32 = 0.80;
/// From this fraction a meter is critical (red).
pub const USAGE_CRITICAL_FRACTION: f32 = 0.95;

/// How full a meter is, by threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageLevel {
    /// Below 80%.
    Normal,
    /// 80% and above.
    Warn,
    /// 95% and above.
    Critical,
}

/// Threshold classification of a usage fraction.
pub fn usage_level(fraction: f32) -> UsageLevel {
    if fraction >= USAGE_CRITICAL_FRACTION {
        UsageLevel::Critical
    } else if fraction >= USAGE_WARN_FRACTION {
        UsageLevel::Warn
    } else {
        UsageLevel::Normal
    }
}

/// One meter of a plan: a rolling window with how much of it is used.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UsageWindow {
    /// "Session", "Week" or "Month".
    pub label: String,
    /// How much of the window is used, in thousandths (540 is 54.0%). Clamped to 0..=1000 by
    /// [`UsageWindow::new`]; an integer so the type stays `Eq`.
    pub used_permille: u16,
    /// When the window resets, in milliseconds since the Unix epoch, when the provider says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<u64>,
}

impl UsageWindow {
    /// A window `used_fraction` (0.0 to 1.0, clamped) full.
    pub fn new(label: impl Into<String>, used_fraction: f64, resets_at_ms: Option<u64>) -> Self {
        let clamped = if used_fraction.is_finite() {
            used_fraction.clamp(0.0, 1.0)
        } else {
            0.0
        };
        Self {
            label: label.into(),
            used_permille: (clamped * 1000.0).round() as u16,
            resets_at_ms,
        }
    }

    /// How much of the window is used, 0.0 to 1.0.
    pub fn used_fraction(&self) -> f32 {
        f32::from(self.used_permille.min(1000)) / 1000.0
    }
}

/// An account's plan meters at one moment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PlanUsage {
    /// The live plan name when the provider says ("ChatGPT Plus").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_label: Option<String>,
    /// The meters, never empty.
    pub windows: Vec<UsageWindow>,
    /// When the daemon read them, in milliseconds since the Unix epoch.
    pub fetched_at_ms: u64,
}

impl PlanUsage {
    /// The binding limit: the most-used window, 0.0 to 1.0. `None` without windows.
    pub fn used_fraction(&self) -> Option<f32> {
        self.windows
            .iter()
            .map(UsageWindow::used_fraction)
            .reduce(f32::max)
    }

    /// The threshold of [`Self::used_fraction`].
    pub fn level(&self) -> Option<UsageLevel> {
        self.used_fraction().map(usage_level)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(fractions: &[f64]) -> PlanUsage {
        PlanUsage {
            plan_label: None,
            windows: fractions
                .iter()
                .map(|f| UsageWindow::new("Session", *f, None))
                .collect(),
            fetched_at_ms: 1,
        }
    }

    #[test]
    fn the_ring_shows_the_most_used_window() {
        assert_eq!(usage(&[]).used_fraction(), None);
        assert_eq!(usage(&[0.12, 0.64]).used_fraction(), Some(0.64));
        assert_eq!(usage(&[1.4]).used_fraction(), Some(1.0));
        assert_eq!(usage(&[-0.2]).used_fraction(), Some(0.0));
        assert_eq!(usage(&[f64::NAN]).used_fraction(), Some(0.0));
    }

    #[test]
    fn thresholds_are_warn_at_80_and_critical_at_95() {
        assert_eq!(usage_level(0.0), UsageLevel::Normal);
        assert_eq!(usage_level(0.79), UsageLevel::Normal);
        assert_eq!(usage_level(0.80), UsageLevel::Warn);
        assert_eq!(usage_level(0.94), UsageLevel::Warn);
        assert_eq!(usage_level(0.95), UsageLevel::Critical);
        assert_eq!(usage_level(1.0), UsageLevel::Critical);
        assert_eq!(usage(&[0.2, 0.96]).level(), Some(UsageLevel::Critical));
        assert_eq!(usage(&[]).level(), None);
    }
}
