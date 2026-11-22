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
    /// As `Stream`, then scroll the view up, so that "jump to latest" shows.
    Latest,
    /// Open "Render check", select from the middle of the prompt to the middle of the reply's
    /// first paragraph, copy it (⌘C) and print the clipboard text.
    Select,
    /// Open "Render check", press the Rust code block's Copy and print the clipboard text.
    CopyCode,
}

impl OpenHook {
    /// The word that names it in `RENO_DRAFT33_OPEN`, and in the `ready <word>` line it prints.
    pub(crate) const fn word(self) -> &'static str {
        match self {
            Self::Thread => "thread",
            Self::Folds => "folds",
            Self::Render => "render",
            Self::Stream => "stream",
            Self::Latest => "latest",
            Self::Select => "select",
            Self::CopyCode => "copycode",
        }
    }

    pub(crate) fn parse(word: &str) -> Option<Self> {
        match word.trim().to_ascii_lowercase().as_str() {
            "thread" => Some(Self::Thread),
            "folds" => Some(Self::Folds),
            "render" => Some(Self::Render),
            "stream" => Some(Self::Stream),
            "latest" => Some(Self::Latest),
            "select" => Some(Self::Select),
            "copycode" => Some(Self::CopyCode),
            _ => None,
        }
    }
}

/// The find hooks of `RENO_DRAFT33_OPEN`: each opens "fix the ACP v2 handshake" with its folds
/// closed and the find bar on [`FindHook::QUERY`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FindHook {
    /// The bar open with the query typed: "1 of 7".
    Find,
    /// As `Find`, then Enter: turn 3's fold opens at its `cargo test` step.
    Enter,
    /// As `Find`, then the third hit.
    Enter3,
    /// As `Enter`, then Escape: the fold search opened closes again.
    Escape,
}

impl FindHook {
    pub(crate) const fn word(self) -> &'static str {
        match self {
            Self::Find => "find",
            Self::Enter => "enter",
            Self::Enter3 => "enter3",
            Self::Escape => "escape",
        }
    }

    /// What the hooks look for.
    pub(crate) const QUERY: &'static str = "supervisor.rs";

    pub(crate) fn parse(word: &str) -> Option<Self> {
        match word.trim().to_ascii_lowercase().as_str() {
            "find" => Some(Self::Find),
            "enter" => Some(Self::Enter),
            "enter3" => Some(Self::Enter3),
            "escape" => Some(Self::Escape),
            _ => None,
        }
    }

    /// The hit to go to (from 0), and whether to close search after it.
    pub(crate) const fn goto(self) -> Option<(usize, bool)> {
        match self {
            Self::Find => None,
            Self::Enter => Some((0, false)),
            Self::Enter3 => Some((2, false)),
            Self::Escape => Some((0, true)),
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
    fn the_find_hooks_are_their_own_words_and_say_where_they_go() {
        for (word, want, goto) in [
            ("find", FindHook::Find, None),
            ("Enter", FindHook::Enter, Some((0, false))),
            ("enter3", FindHook::Enter3, Some((2, false))),
            ("escape", FindHook::Escape, Some((0, true))),
        ] {
            assert_eq!(FindHook::parse(word), Some(want), "{word}");
            assert_eq!(want.goto(), goto, "{word}");
        }
        assert_eq!(FindHook::parse("thread"), None);
        assert_eq!(OpenHook::parse("latest"), Some(OpenHook::Latest));
        assert_eq!(FindHook::QUERY, reno_client::fake::transcript::NEEDLE);
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

    #[test]
    fn the_selection_and_code_copy_hooks_are_open_hooks() {
        assert_eq!(OpenHook::parse("select"), Some(OpenHook::Select));
        assert_eq!(OpenHook::parse("CopyCode"), Some(OpenHook::CopyCode));
        assert_eq!(FindHook::parse("select"), None);
    }

    #[test]
    fn every_hook_word_parses_back_to_its_hook() {
        for hook in [
            OpenHook::Thread,
            OpenHook::Folds,
            OpenHook::Render,
            OpenHook::Stream,
            OpenHook::Latest,
            OpenHook::Select,
            OpenHook::CopyCode,
        ] {
            assert_eq!(OpenHook::parse(hook.word()), Some(hook));
        }
        for hook in [
            FindHook::Find,
            FindHook::Enter,
            FindHook::Enter3,
            FindHook::Escape,
        ] {
            assert_eq!(FindHook::parse(hook.word()), Some(hook));
        }
    }
}
