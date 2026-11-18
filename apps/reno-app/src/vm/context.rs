//! The context window and the account's plan usage, as the picker and the composer foot show
//! them: the ring's level and label, the card's words, and the window choice a model offers.
//!
//! The ring's thresholds (context 0.75 / 0.90) and the card's words are ported from zeron's
//! `crates/ui/src/context_usage.rs`; plan usage thresholds (0.80 / 0.95) come from
//! `reno_proto::usage_level`.

use reno_proto::{
    ConfigTiming, ContextUsage, MODEL_OPTION_CONTEXT_WINDOW, ModelOption, UsageLevel, UsageWindow,
};

use super::models::ModelChoice;

/// How loud a ring is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    Muted,
    Warning,
    Danger,
}

/// A context ring's level: warning from 75%, danger from 90%.
pub(crate) fn context_level(fraction: f64) -> Level {
    if fraction >= 0.9 {
        Level::Danger
    } else if fraction >= 0.75 {
        Level::Warning
    } else {
        Level::Muted
    }
}

/// A plan ring's level, from the thresholds the protocol defines.
pub(crate) fn usage_ring_level(fraction: f32) -> Level {
    match reno_proto::usage_level(fraction) {
        UsageLevel::Normal => Level::Muted,
        UsageLevel::Warn => Level::Warning,
        UsageLevel::Critical => Level::Danger,
    }
}

/// "42%" for a fraction.
pub(crate) fn percent(fraction: f64) -> String {
    format!("{:.0}%", fraction * 100.0)
}

/// The composer-foot ring: hidden until the harness has reported a window (a harness that
/// never does gets no permanently empty ring).
pub(crate) fn has_window(usage: ContextUsage) -> bool {
    usage.window.is_some_and(|window| window > 0)
}

/// 1,048,576 as "1,048,576".
pub(crate) fn with_separators(count: u64) -> String {
    let digits = count.to_string();
    let mut grouped = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(digit);
    }
    grouped
}

/// A token count in a few characters: 84,213 is "84K", 1,000,000 is "1M".
pub(crate) fn short_tokens(count: u64) -> String {
    if count >= 1_000_000 {
        let tenths = (count as f64 / 100_000.0).round() / 10.0;
        if tenths.fract() == 0.0 {
            format!("{tenths:.0}M")
        } else {
            format!("{tenths:.1}M")
        }
    } else if count >= 1_000 {
        format!("{:.0}K", count as f64 / 1_000.0)
    } else {
        count.to_string()
    }
}

/// The context card's two lines.
pub(crate) fn details(usage: ContextUsage) -> String {
    match (usage.tokens, usage.window.filter(|w| *w > 0)) {
        (Some(tokens), Some(window)) => format!(
            "{} / {} tokens\n{} tokens remaining",
            with_separators(tokens),
            with_separators(window),
            with_separators(window.saturating_sub(tokens))
        ),
        (Some(tokens), None) => format!(
            "{} tokens used\nContext limit not reported",
            with_separators(tokens)
        ),
        (None, Some(window)) => format!(
            "{} token capacity\nWaiting for context usage",
            with_separators(window)
        ),
        (None, None) => "Context usage not reported by this harness yet".to_owned(),
    }
}

/// The picker's Context window row on a live run: "84K / 200K \u{b7} 42%".
pub(crate) fn live_words(usage: ContextUsage) -> Option<String> {
    let window = usage.window.filter(|w| *w > 0)?;
    let Some(tokens) = usage.tokens else {
        return Some(short_tokens(window));
    };
    let fraction = usage.fraction().unwrap_or(0.0);
    Some(format!(
        "{} / {} \u{b7} {}",
        short_tokens(tokens),
        short_tokens(window),
        percent(fraction)
    ))
}

// ---- the window choice -------------------------------------------------------------------------

/// The size a window choice names, in tokens: "200k" is 200,000, "1m" 1,000,000, "256K"
/// 256,000; a bare number is taken as is. `None` for a choice that names no size.
pub(crate) fn choice_tokens(id: &str) -> Option<u64> {
    let id = id.trim().to_ascii_lowercase();
    let (number, scale) = if let Some(n) = id.strip_suffix('m') {
        (n, 1_000_000.0)
    } else if let Some(n) = id.strip_suffix('k') {
        (n, 1_000.0)
    } else {
        (id.as_str(), 1.0)
    };
    let value: f64 = number.parse().ok()?;
    let tokens = (value * scale).round();
    (tokens.is_finite() && tokens >= 1.0).then_some(tokens as u64)
}

/// The context-window option of a model, when it offers a choice of more than one size.
pub(crate) fn window_option(model: &ModelChoice) -> Option<&ModelOption> {
    model
        .options
        .iter()
        .find(|o| o.id == MODEL_OPTION_CONTEXT_WINDOW && o.choices.len() > 1)
}

/// The choice in force: the one chosen when the option lists it, else the model's default.
pub(crate) fn chosen_choice<'a>(option: &'a ModelOption, chosen: Option<&'a str>) -> &'a str {
    chosen
        .filter(|c| option.choices.iter().any(|choice| choice.id == *c))
        .unwrap_or(&option.default_choice)
}

/// The label of the choice in force ("1M").
pub(crate) fn chosen_label(option: &ModelOption, chosen: Option<&str>) -> String {
    let id = chosen_choice(option, chosen);
    option
        .choices
        .iter()
        .find(|c| c.id == id)
        .map_or_else(|| id.to_owned(), |c| c.label.clone())
}

/// The options to send for a picked model: each remembered choice the model still lists, and
/// never a read-only option.
pub(crate) fn options_for_model(
    model: &ModelChoice,
    remembered: Option<&super::composer_prefs::Options>,
) -> std::collections::BTreeMap<String, String> {
    let Some(remembered) = remembered else {
        return Default::default();
    };
    model
        .options
        .iter()
        .filter_map(|o| {
            let choice = remembered.get(&o.id)?;
            if !o.settable {
                return None;
            }
            o.choices
                .iter()
                .any(|c| &c.id == choice)
                .then(|| (o.id.clone(), choice.clone()))
        })
        .collect()
}

/// When a change of a run's effort or options takes hold, as the picker says it under the control.
pub(crate) const fn timing_words(timing: ConfigTiming) -> &'static str {
    match timing {
        ConfigTiming::Now => "applies now",
        ConfigTiming::NextTurn => "from the next turn",
        ConfigTiming::AfterRestart => "next time the run starts",
    }
}

/// When a change to `choice` of a window option takes hold on a live run: the executor's own
/// timing, except that a 200K cap is a launch setting (`after_restart`) whatever it says.
pub(crate) fn window_timing(live: Option<ConfigTiming>, choice: &str) -> ConfigTiming {
    if choice == reno_proto::CONTEXT_WINDOW_200K {
        return ConfigTiming::AfterRestart;
    }
    live.unwrap_or(ConfigTiming::AfterRestart)
}

// ---- plan usage ---------------------------------------------------------------------------------

/// When a window resets, in `zone`: "16:00" on the day of `now_ms`, "Thu 10:00" on another.
pub(crate) fn reset_clock<Tz>(at_ms: u64, now_ms: u64, zone: &Tz) -> String
where
    Tz: chrono::TimeZone,
    Tz::Offset: std::fmt::Display,
{
    let local = |ms: u64| {
        i64::try_from(ms)
            .ok()
            .and_then(chrono::DateTime::from_timestamp_millis)
            .map(|t| t.with_timezone(zone))
    };
    match (local(at_ms), local(now_ms)) {
        (Some(at), Some(now)) if at.date_naive() == now.date_naive() => {
            at.format("%H:%M").to_string()
        }
        (Some(at), _) => at.format("%a %H:%M").to_string(),
        (None, _) => "soon".to_owned(),
    }
}

/// One meter's line on the Account row: "Session 54% \u{b7} resets 16:00", in `zone`.
pub(crate) fn window_line<Tz>(window: &UsageWindow, now_ms: u64, zone: &Tz) -> String
where
    Tz: chrono::TimeZone,
    Tz::Offset: std::fmt::Display,
{
    let used = percent(f64::from(window.used_fraction()));
    match window.resets_at_ms {
        Some(at) => format!(
            "{} {used} \u{b7} resets {}",
            window.label,
            reset_clock(at, now_ms, zone)
        ),
        None => format!("{} {used}", window.label),
    }
}

#[cfg(test)]
mod tests {
    use reno_proto::ModelOptionChoice;

    use super::*;

    fn usage(tokens: Option<u64>, window: Option<u64>) -> ContextUsage {
        ContextUsage { tokens, window }
    }

    #[test]
    fn the_context_ring_warns_from_75_and_is_in_danger_from_90() {
        assert_eq!(context_level(0.42), Level::Muted);
        assert_eq!(context_level(0.75), Level::Warning);
        assert_eq!(context_level(0.89), Level::Warning);
        assert_eq!(context_level(0.90), Level::Danger);
        assert_eq!(usage_ring_level(0.79), Level::Muted);
        assert_eq!(usage_ring_level(0.80), Level::Warning);
        assert_eq!(usage_ring_level(0.95), Level::Danger);
    }

    #[test]
    fn indicator_needs_a_reported_window() {
        assert!(!has_window(usage(None, None)));
        assert!(!has_window(usage(Some(1_200), None)));
        assert!(!has_window(usage(Some(1_200), Some(0))));
        assert!(has_window(usage(None, Some(200_000))));
    }

    #[test]
    fn missing_usage_is_distinct_from_zero_and_overflow() {
        assert!(details(usage(None, None)).contains("not reported"));
        assert!(details(usage(Some(0), Some(200))).contains("200 tokens remaining"));
        assert!(details(usage(Some(250), Some(200))).contains("0 tokens remaining"));
        assert!(details(usage(Some(10), Some(0))).contains("limit not reported"));
        assert!(details(usage(None, Some(200))).contains("Waiting for context usage"));
    }

    #[test]
    fn token_counts_are_grouped_by_thousands() {
        assert_eq!(with_separators(0), "0");
        assert_eq!(with_separators(999), "999");
        assert_eq!(with_separators(5417), "5,417");
        assert_eq!(with_separators(1_048_576), "1,048,576");
        assert_eq!(
            details(usage(Some(5417), Some(1_048_576))),
            "5,417 / 1,048,576 tokens\n1,043,159 tokens remaining"
        );
        assert_eq!(
            details(usage(Some(84_213), Some(200_000))),
            "84,213 / 200,000 tokens\n115,787 tokens remaining"
        );
    }

    #[test]
    fn the_live_row_reads_fill_over_window_and_percent() {
        assert_eq!(
            live_words(usage(Some(84_213), Some(200_000))).as_deref(),
            Some("84K / 200K \u{b7} 42%")
        );
        assert_eq!(
            live_words(usage(Some(900), Some(1_000_000))).as_deref(),
            Some("900 / 1M \u{b7} 0%")
        );
        assert_eq!(
            live_words(usage(None, Some(200_000))).as_deref(),
            Some("200K")
        );
        assert_eq!(live_words(usage(Some(5), None)), None);
        assert_eq!(short_tokens(1_500_000), "1.5M");
    }

    fn model_with(options: Vec<ModelOption>) -> ModelChoice {
        ModelChoice {
            id: "m".into(),
            name: "M".into(),
            efforts: Vec::new(),
            default_effort: None,
            description: None,
            options,
        }
    }

    fn window_choices() -> ModelOption {
        let choice = |id: &str, label: &str| ModelOptionChoice {
            id: id.into(),
            label: label.into(),
        };
        ModelOption {
            settable: true,
            id: MODEL_OPTION_CONTEXT_WINDOW.into(),
            label: "Context window".into(),
            choices: vec![choice("200k", "200K"), choice("1m", "1M")],
            default_choice: "1m".into(),
        }
    }

    #[test]
    fn a_model_offers_the_window_row_only_with_a_real_choice() {
        assert!(window_option(&model_with(vec![])).is_none());
        let mut single = window_choices();
        single.choices.truncate(1);
        assert!(window_option(&model_with(vec![single])).is_none());
        let model = model_with(vec![window_choices()]);
        let option = window_option(&model).unwrap();
        assert_eq!(chosen_label(option, None), "1M", "the model's default");
        assert_eq!(chosen_label(option, Some("200k")), "200K");
        assert_eq!(
            chosen_label(option, Some("2m")),
            "1M",
            "a choice it does not list falls back"
        );
    }

    #[test]
    fn a_read_only_option_is_never_sent_even_when_remembered() {
        // Grok reports its window and cannot set it: a remembered choice must not reach task.run.
        let mut grok = window_choices();
        grok.settable = false;
        let model = model_with(vec![grok]);
        let mut remembered = super::super::composer_prefs::Options::new();
        remembered.insert("contextWindow".into(), "200k".into());
        assert!(options_for_model(&model, Some(&remembered)).is_empty());
    }

    #[test]
    fn only_remembered_choices_the_model_still_lists_are_sent() {
        let model = model_with(vec![window_choices()]);
        let mut remembered = super::super::composer_prefs::Options::new();
        remembered.insert("contextWindow".into(), "200k".into());
        remembered.insert("other".into(), "x".into());
        let sent = options_for_model(&model, Some(&remembered));
        assert_eq!(sent.len(), 1);
        assert_eq!(sent["contextWindow"], "200k");
        remembered.insert("contextWindow".into(), "9m".into());
        assert!(options_for_model(&model, Some(&remembered)).is_empty());
        assert!(options_for_model(&model, None).is_empty());
    }

    #[test]
    fn a_meter_reads_label_percent_and_reset_in_the_given_zone() {
        use chrono::FixedOffset;
        const HOUR: u64 = 3_600_000;
        // 2026-11-18 12:00 UTC; the zone is UTC+2, so it is 14:00 there.
        let now = 1_795_003_200_000;
        let zone = FixedOffset::east_opt(2 * 3600).unwrap();
        let today = UsageWindow::new("Session", 0.54, Some(now + 4 * HOUR));
        assert_eq!(
            window_line(&today, now, &zone),
            "Session 54% \u{b7} resets 18:00"
        );
        // 22:00 UTC is already the next day at UTC+2: shown with its weekday.
        let tomorrow = UsageWindow::new("Week", 0.21, Some(now + 10 * HOUR));
        assert_eq!(
            window_line(&tomorrow, now, &zone),
            "Week 21% \u{b7} resets Thu 00:00"
        );
        assert_eq!(
            window_line(&UsageWindow::new("Week", 0.21, None), now, &zone),
            "Week 21%"
        );
    }

    #[test]
    fn a_window_choice_names_its_size_in_tokens() {
        assert_eq!(choice_tokens("200k"), Some(200_000));
        assert_eq!(choice_tokens("1m"), Some(1_000_000));
        assert_eq!(choice_tokens("256K"), Some(256_000));
        assert_eq!(choice_tokens("1.5m"), Some(1_500_000));
        assert_eq!(choice_tokens("500000"), Some(500_000));
        assert_eq!(choice_tokens("default"), None);
        assert_eq!(choice_tokens("0k"), None);
        assert_eq!(choice_tokens(""), None);
    }

    #[test]
    fn timings_read_as_when_the_change_takes_hold() {
        assert_eq!(timing_words(ConfigTiming::Now), "applies now");
        assert_eq!(timing_words(ConfigTiming::NextTurn), "from the next turn");
        assert_eq!(
            timing_words(ConfigTiming::AfterRestart),
            "next time the run starts"
        );
        assert_eq!(
            window_timing(Some(ConfigTiming::Now), "1m"),
            ConfigTiming::Now
        );
        assert_eq!(
            window_timing(Some(ConfigTiming::Now), "200k"),
            ConfigTiming::AfterRestart,
            "a 200K cap is a launch setting"
        );
        assert_eq!(window_timing(None, "1m"), ConfigTiming::AfterRestart);
    }
}
