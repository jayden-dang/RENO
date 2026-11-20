//! ROAD-38 draft: which of the three first-run variants is showing, and the hooks that put the
//! app in a state for a screenshot. Used only with `--fake --draft38`.

/// A first-run direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Variant {
    /// A guided five-step wizard in a full-window flow.
    A,
    /// A checklist beside the new-thread state.
    B,
    /// One scrolling page about this Mac.
    C,
}

impl Variant {
    #[cfg(test)]
    pub(crate) const ALL: [Self; 3] = [Self::A, Self::B, Self::C];

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
            Self::A => "Wizard",
            Self::B => "Checklist",
            Self::C => "One page",
        }
    }

    /// "A \u{2014} Wizard", as the status bar shows it.
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

/// `RENO_DRAFT38_OPEN`.
pub(crate) const OPEN_HOOKS: [&str; 5] =
    ["restart", "restart-card", "restart-fail", "updates", "link"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_cycle_both_ways_and_parse_by_letter() {
        for v in Variant::ALL {
            assert_eq!(v.next().prev(), v);
            assert_eq!(Variant::parse(v.key()), Some(v));
        }
        assert_eq!(Variant::A.prev(), Variant::C);
        assert_eq!(Variant::C.next(), Variant::A);
        assert_eq!(Variant::parse(" b "), Some(Variant::B));
        assert_eq!(Variant::parse("d"), None);
        assert_eq!(Variant::B.label(), "B \u{2014} Checklist");
    }
}
