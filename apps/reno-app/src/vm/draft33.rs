//! ROAD-33 draft: which of the three transcript variants is showing, and the start hooks that put
//! the app in a state for a screenshot. Used only with `--fake --draft33`; nothing is persisted.

/// A transcript direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Variant {
    /// The transcript stays text; controls show when reached for.
    A,
    /// Navigation is spread over the transcript: a ledger with a rail of matches.
    B,
    /// Every hit is listed in context, in a results tab.
    C,
}

impl Variant {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 3] = [Self::A, Self::B, Self::C];

    /// `a`, `b` or `c`, in either case.
    pub(crate) fn parse(key: &str) -> Option<Self> {
        match key.trim().to_ascii_lowercase().as_str() {
            "a" => Some(Self::A),
            "b" => Some(Self::B),
            "c" => Some(Self::C),
            _ => None,
        }
    }

    pub(crate) const fn key(self) -> &'static str {
        match self {
            Self::A => "A",
            Self::B => "B",
            Self::C => "C",
        }
    }

    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::A => "Quiet strip",
            Self::B => "Ledger",
            Self::C => "Results",
        }
    }

    /// "A — Quiet strip", as the status bar shows it.
    pub(crate) fn label(self) -> String {
        format!("{} \u{2014} {}", self.key(), self.name())
    }

    pub(crate) fn next(self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::C,
            Self::C => Self::A,
        }
    }

    pub(crate) fn prev(self) -> Self {
        match self {
            Self::A => Self::C,
            Self::B => Self::A,
            Self::C => Self::B,
        }
    }
}

/// `RENO_DRAFT33_OPEN`: the state to put the app in once the screen is up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OpenHook {
    /// Open "fix the ACP v2 handshake".
    Thread,
    /// Open it and open every "Worked for" fold.
    Folds,
    /// Open "Render check".
    Render,
    /// Start the long streaming reply and open its run.
    Stream,
}

impl OpenHook {
    pub(crate) fn parse(word: &str) -> Option<Self> {
        match word.trim().to_ascii_lowercase().as_str() {
            "thread" => Some(Self::Thread),
            "folds" => Some(Self::Folds),
            "render" => Some(Self::Render),
            "stream" => Some(Self::Stream),
            _ => None,
        }
    }
}

/// Says on the console that a hook word is not known and so is ignored.
#[allow(clippy::print_stderr)]
pub(crate) fn ignore_unknown_hook(word: &str) {
    eprintln!("reno-app: RENO_DRAFT33_OPEN: ignoring unknown hook {word:?}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_cycle_both_ways_and_wrap() {
        for v in Variant::ALL {
            assert_eq!(v.next().prev(), v);
            assert_eq!(v.prev().next(), v);
            assert_eq!(Variant::parse(v.key()), Some(v));
        }
        assert_eq!(Variant::A.prev(), Variant::C);
        assert_eq!(Variant::C.next(), Variant::A);
        assert_eq!(Variant::A.next(), Variant::B);
    }

    #[test]
    fn a_variant_parses_by_letter_and_says_its_key_and_name() {
        assert_eq!(Variant::parse(" b "), Some(Variant::B));
        assert_eq!(Variant::parse("C"), Some(Variant::C));
        assert_eq!(Variant::parse("d"), None);
        assert_eq!(Variant::parse(""), None);
        assert_eq!(Variant::A.label(), "A \u{2014} Quiet strip");
        assert_eq!(Variant::B.label(), "B \u{2014} Ledger");
        assert_eq!(Variant::C.label(), "C \u{2014} Results");
    }

    #[test]
    fn hooks_parse_by_word_and_unknown_words_are_none() {
        assert_eq!(OpenHook::parse("thread"), Some(OpenHook::Thread));
        assert_eq!(OpenHook::parse("Folds"), Some(OpenHook::Folds));
        assert_eq!(OpenHook::parse("render"), Some(OpenHook::Render));
        assert_eq!(OpenHook::parse("stream"), Some(OpenHook::Stream));
        assert_eq!(OpenHook::parse("find"), None);
        assert_eq!(OpenHook::parse(""), None);
    }
}
