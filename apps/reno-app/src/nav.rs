//! Navigation destinations: the sidebar's five rows (ui-brief 2026-11-17). Screens of later
//! phases (flows, bots, teams, boards, messages, evaluations, improvements) get a row when they
//! are built, not before.

use crate::ui::icon::Icon;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Nav {
    Home,
    /// Being retired: answers live in each run (ui-brief 2026-11-17). Reached only from the
    /// palette and ⌘-shortcut until its history has moved to the project page.
    Inbox,
    Runs,
    Triggers,
    Executors,
    Settings,
}

impl Nav {
    /// The sidebar's rows, in order.
    pub(crate) const ROWS: [Self; 5] = [
        Self::Home,
        Self::Runs,
        Self::Triggers,
        Self::Executors,
        Self::Settings,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Inbox => "Inbox",
            Self::Runs => "Runs",
            Self::Triggers => "Triggers",
            Self::Executors => "Executors",
            Self::Settings => "Settings",
        }
    }

    /// The row icon.
    pub(crate) const fn icon(self) -> Icon {
        match self {
            Self::Home => Icon::Home,
            Self::Inbox => Icon::Inbox,
            Self::Runs => Icon::Terminal,
            Self::Triggers => Icon::Clock,
            Self::Executors => Icon::Cpu,
            Self::Settings => Icon::Settings,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sidebar_has_five_rows_and_no_inbox() {
        let labels: Vec<_> = Nav::ROWS.iter().map(|n| n.label()).collect();
        assert_eq!(
            labels,
            ["Home", "Runs", "Triggers", "Executors", "Settings"]
        );
        assert!(!Nav::ROWS.contains(&Nav::Inbox));
    }
}
