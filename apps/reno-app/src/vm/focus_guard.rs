//! The mis-press guard (ui-brief, "Focus"): a panel never steals the keyboard. Digit, Enter and
//! Esc keys act on a panel only once the owner has clicked it or jumped to it (⌘J); until then
//! they stay with whatever the owner was using.

/// Who gets a key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    Panel,
    Composer,
}

/// What the owner is doing as a panel opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Facts {
    /// A panel is waiting for an answer.
    pub(crate) panel_pending: bool,
    /// The owner clicked the panel or pressed ⌘J: it has the keyboard.
    pub(crate) panel_claimed: bool,
}

/// Who gets Esc, Enter or a digit now.
pub(crate) fn key_target(facts: Facts) -> Target {
    if facts.panel_pending && facts.panel_claimed {
        Target::Panel
    } else {
        Target::Composer
    }
}

/// Whether the composer stays on screen under a freshly opened panel, so the owner's typing has
/// somewhere to go: it has the keyboard or holds a draft.
pub(crate) fn composer_stays(composer_focused: bool, has_text: bool) -> bool {
    composer_focused || has_text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(panel_pending: bool, panel_claimed: bool) -> Facts {
        Facts {
            panel_pending,
            panel_claimed,
        }
    }

    #[test]
    fn keys_stay_with_the_composer_until_the_panel_is_claimed() {
        assert_eq!(key_target(facts(true, false)), Target::Composer);
        assert_eq!(key_target(facts(true, true)), Target::Panel);
    }

    #[test]
    fn with_no_panel_nothing_goes_to_one() {
        assert_eq!(key_target(facts(false, false)), Target::Composer);
        assert_eq!(key_target(facts(false, true)), Target::Composer);
    }

    #[test]
    fn a_focused_or_filled_composer_stays_under_the_panel() {
        assert!(composer_stays(true, false));
        assert!(composer_stays(false, true));
        assert!(!composer_stays(false, false));
    }
}
