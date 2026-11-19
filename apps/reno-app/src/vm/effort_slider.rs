//! The effort popover's 5-step slider: which steps a model offers.

use reno_proto::Effort;

/// The five steps, least to most.
pub(crate) const STEPS: [Effort; 5] = [
    Effort::Low,
    Effort::Medium,
    Effort::High,
    Effort::XHigh,
    Effort::Max,
];

/// One step of the slider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Step {
    pub(crate) effort: Effort,
    /// The executor offers it; a step it does not offer cannot be picked.
    pub(crate) selectable: bool,
    /// At or below the chosen level (drawn lit).
    pub(crate) lit: bool,
}

/// The slider's steps for a model that `offers` some levels, with `chosen` (else the model's
/// `default`) as the level in force.
pub(crate) fn steps(
    offers: &[Effort],
    chosen: Option<Effort>,
    default: Option<Effort>,
) -> Vec<Step> {
    let level = chosen.or(default);
    STEPS
        .iter()
        .map(|&effort| Step {
            effort,
            selectable: offers.contains(&effort),
            lit: level.is_some_and(|l| effort <= l),
        })
        .collect()
}

/// The popover's title: the level in force, "Default" when the harness did not say.
pub(crate) fn title(chosen: Option<Effort>, default: Option<Effort>) -> &'static str {
    chosen
        .or(default)
        .map_or("Default", super::picker::effort_label)
}

#[cfg(test)]
mod tests {
    use Effort::{High, Low, Max, Medium, Minimal, XHigh};

    use super::*;

    #[test]
    fn there_are_always_five_steps_and_only_the_offered_ones_can_be_picked() {
        let s = steps(&[Low, Medium, High], None, Some(Medium));
        assert_eq!(s.len(), 5);
        let pick: Vec<_> = s.iter().map(|s| s.selectable).collect();
        assert_eq!(pick, [true, true, true, false, false]);
        let lit: Vec<_> = s.iter().map(|s| s.lit).collect();
        assert_eq!(
            lit,
            [true, true, false, false, false],
            "lit up to the default"
        );
    }

    #[test]
    fn a_choice_beats_the_default_and_a_model_with_no_levels_offers_none() {
        let s = steps(&[Low, Medium, High, XHigh, Max], Some(XHigh), Some(Medium));
        assert_eq!(s.iter().filter(|s| s.lit).count(), 4);
        assert!(
            steps(&[], None, None)
                .iter()
                .all(|s| !s.selectable && !s.lit)
        );
        assert!(!steps(&[Minimal], None, None)[0].selectable);
    }

    #[test]
    fn the_title_is_the_level_in_force() {
        assert_eq!(title(Some(High), Some(Medium)), "High");
        assert_eq!(title(None, Some(Medium)), "Medium");
        assert_eq!(title(None, None), "Default");
        assert_eq!(title(Some(XHigh), None), "X-High");
    }
}
