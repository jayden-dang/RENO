//! The settings navigation (ui-brief 2026-11-19, settings): the categories, their groups, and the
//! search that filters them.

use reno_client::ConnectionStatus;

/// A page of Settings.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum Category {
    #[default]
    Executors,
    Registry,
    Definitions,
    Policy,
    Notifications,
    Daemon,
}

impl Category {
    /// In the order the navigation lists them.
    pub(crate) const ALL: [Self; 6] = [
        Self::Executors,
        Self::Registry,
        Self::Definitions,
        Self::Policy,
        Self::Notifications,
        Self::Daemon,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Executors => "Executors",
            Self::Registry => "Add from registry",
            Self::Definitions => "Definitions",
            Self::Policy => "Policy",
            Self::Notifications => "Notifications",
            Self::Daemon => "Daemon",
        }
    }

    /// The element id and test selector stem: `settings-<key>`.
    pub(crate) const fn key(self) -> &'static str {
        match self {
            Self::Executors => "executors",
            Self::Registry => "registry",
            Self::Definitions => "definitions",
            Self::Policy => "policy",
            Self::Notifications => "notifications",
            Self::Daemon => "daemon",
        }
    }

    /// The heading the category sits under.
    pub(crate) const fn group(self) -> &'static str {
        match self {
            Self::Executors | Self::Registry => "Agents",
            Self::Definitions | Self::Policy => "Automation",
            Self::Notifications => "App",
            Self::Daemon => "System",
        }
    }

    /// Words a search matches besides the label: what the page holds.
    const fn keywords(self) -> &'static str {
        match self {
            Self::Executors => {
                "executor agent harness cli install sign in login update auto-update version check models claude codex grok cursor muse pi opencode antigravity"
            }
            Self::Registry => "acp registry add agent your own agent keys environment",
            Self::Definitions => "definitions bots triggers files validate schedule",
            Self::Policy => "policy rules allow deny ask standing rules revoke approvals",
            Self::Notifications => "notifications banners dock badge finished background",
            Self::Daemon => "daemon renod socket version protocol process uptime features",
        }
    }

    fn matches(self, query: &str) -> bool {
        self.label().to_lowercase().contains(query) || self.keywords().contains(query)
    }
}

/// The categories a search shows, in navigation order. Blank shows all.
pub(crate) fn matching(query: &str) -> Vec<Category> {
    let query = query.trim().to_lowercase();
    Category::ALL
        .into_iter()
        .filter(|c| query.is_empty() || c.matches(&query))
        .collect()
}

/// The category to show after a search: the current one while it still matches, else the first
/// match, else the current one (the page stays; the list says nothing matches).
pub(crate) fn after_search(current: Category, shown: &[Category]) -> Category {
    if shown.contains(&current) {
        current
    } else {
        shown.first().copied().unwrap_or(current)
    }
}

/// How the daemon's state marks the gear of the sidebar's Settings row: nothing while connected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Pip {
    Warning,
    Danger,
}

pub(crate) const fn daemon_pip(status: ConnectionStatus) -> Option<Pip> {
    match status {
        ConnectionStatus::Connected => None,
        ConnectionStatus::Connecting => Some(Pip::Warning),
        ConnectionStatus::Disconnected => Some(Pip::Danger),
    }
}

/// The Settings row's accessible label: what the count means, said out.
pub(crate) fn settings_label(needing: usize) -> String {
    match needing {
        0 => "Settings".to_owned(),
        1 => "Settings, 1 executor needs you".to_owned(),
        n => format!("Settings, {n} executors need you"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_categories_sit_under_four_groups_in_order() {
        let listed: Vec<_> = Category::ALL
            .iter()
            .map(|c| (c.group(), c.label()))
            .collect();
        assert_eq!(
            listed,
            [
                ("Agents", "Executors"),
                ("Agents", "Add from registry"),
                ("Automation", "Definitions"),
                ("Automation", "Policy"),
                ("App", "Notifications"),
                ("System", "Daemon"),
            ]
        );
        assert_eq!(Category::default(), Category::Executors);
    }

    #[test]
    fn search_matches_labels_and_what_a_page_holds() {
        assert_eq!(matching(""), Category::ALL.to_vec());
        assert_eq!(matching("  "), Category::ALL.to_vec());
        assert_eq!(matching("POLI"), [Category::Policy]);
        assert_eq!(matching("auto-update"), [Category::Executors]);
        assert_eq!(matching("dock"), [Category::Notifications]);
        assert_eq!(matching("socket"), [Category::Daemon]);
        assert_eq!(matching("registry"), [Category::Registry]);
        assert!(matching("zzz").is_empty());
    }

    #[test]
    fn a_search_keeps_the_page_while_it_still_matches() {
        let shown = matching("e");
        assert_eq!(
            after_search(Category::Policy, &shown),
            Category::Policy,
            "policy holds an e"
        );
        assert_eq!(
            after_search(Category::Daemon, &[Category::Policy]),
            Category::Policy
        );
        assert_eq!(after_search(Category::Daemon, &[]), Category::Daemon);
    }

    #[test]
    fn the_gear_is_marked_only_when_the_daemon_is_not_connected() {
        assert_eq!(daemon_pip(ConnectionStatus::Connected), None);
        assert_eq!(daemon_pip(ConnectionStatus::Connecting), Some(Pip::Warning));
        assert_eq!(
            daemon_pip(ConnectionStatus::Disconnected),
            Some(Pip::Danger)
        );
    }

    #[test]
    fn the_settings_row_says_its_count() {
        assert_eq!(settings_label(0), "Settings");
        assert_eq!(settings_label(1), "Settings, 1 executor needs you");
        assert_eq!(settings_label(2), "Settings, 2 executors need you");
    }
}
