//! The permission-mode chip's choices: the harness's own modes, each with a one-line subtitle.

use reno_proto::ExecutorId;

/// One mode a harness offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Mode {
    pub(crate) id: &'static str,
    pub(crate) title: &'static str,
    pub(crate) subtitle: &'static str,
    /// Drawn in the warning tone (it removes the safety net).
    pub(crate) warning: bool,
}

const fn mode(
    id: &'static str,
    title: &'static str,
    subtitle: &'static str,
    warning: bool,
) -> Mode {
    Mode {
        id,
        title,
        subtitle,
        warning,
    }
}

const CODEX: &[Mode] = &[
    mode(
        "ask",
        "Ask for approval",
        "Always ask to edit external files and use the internet",
        false,
    ),
    mode(
        "auto",
        "Approve for me",
        "Only ask for actions detected as potentially unsafe",
        false,
    ),
    mode(
        "full",
        "Full access",
        "Unrestricted access to the internet and any file on your computer",
        true,
    ),
];

const CLAUDE: &[Mode] = &[
    mode(
        "default",
        "Ask permissions",
        "Ask before each edit or command",
        false,
    ),
    mode(
        "acceptEdits",
        "Accept edits",
        "Edit files freely, ask before commands",
        false,
    ),
    mode("plan", "Plan mode", "Read and plan, change nothing", false),
    mode(
        "bypassPermissions",
        "Bypass permissions",
        "Run everything without asking",
        true,
    ),
];

const OTHER: &[Mode] = &[
    mode(
        "ask",
        "Ask for approval",
        "Ask before edits and commands",
        false,
    ),
    mode("full", "Full access", "Run everything without asking", true),
];

/// The modes of `harness`, in the order its own picker lists them. The first is the default.
pub(crate) fn modes(harness: Option<&ExecutorId>) -> &'static [Mode] {
    match harness.map(ExecutorId::as_str) {
        Some("codex") => CODEX,
        Some("claude-code") => CLAUDE,
        _ => OTHER,
    }
}

/// The mode with `id`, else the harness's default.
pub(crate) fn chosen(harness: Option<&ExecutorId>, id: Option<&str>) -> Mode {
    let all = modes(harness);
    id.and_then(|id| all.iter().find(|m| m.id == id))
        .copied()
        .unwrap_or(all[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> ExecutorId {
        s.parse().unwrap()
    }

    #[test]
    fn codex_offers_its_three_modes_and_only_full_access_warns() {
        let titles: Vec<_> = modes(Some(&id("codex"))).iter().map(|m| m.title).collect();
        assert_eq!(
            titles,
            ["Ask for approval", "Approve for me", "Full access"]
        );
        let warns: Vec<_> = modes(Some(&id("codex")))
            .iter()
            .map(|m| m.warning)
            .collect();
        assert_eq!(warns, [false, false, true]);
    }

    #[test]
    fn claude_has_its_own_modes_each_with_a_subtitle() {
        let all = modes(Some(&id("claude-code")));
        assert_eq!(all.len(), 4);
        assert!(all.iter().all(|m| !m.subtitle.is_empty()));
        assert_eq!(all.last().map(|m| m.warning), Some(true));
    }

    #[test]
    fn an_unknown_harness_gets_the_generic_pair_and_a_stale_id_falls_back() {
        assert_eq!(modes(Some(&id("pi"))).len(), 2);
        assert_eq!(modes(None).len(), 2, "Auto has not picked a harness yet");
        assert_eq!(
            chosen(Some(&id("codex")), Some("nope")).title,
            "Ask for approval"
        );
        assert_eq!(
            chosen(Some(&id("codex")), Some("full")).title,
            "Full access"
        );
    }
}
