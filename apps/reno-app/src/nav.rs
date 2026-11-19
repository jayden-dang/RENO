//! Navigation destinations. Home is the new-thread state ("Start a task"), Runs is All runs
//! (from the bar's "N live" and ⌘K), Triggers comes from the bar's "…" menu and ⌘K; Settings is
//! the sidebar's footer row, and Executors lives inside Settings (app-shell brief, "Where
//! today's screens go"). `Nav::Executors` stays a destination so the composer, a run and the
//! palette can deep-link to Settings › Executors.

use crate::ui::icon::Icon;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Nav {
    Home,
    Runs,
    Triggers,
    /// Settings › Executors.
    Executors,
    Settings,
}

impl Nav {
    /// Every destination, in the order the palette offers them.
    pub(crate) const ALL: [Self; 5] = [
        Self::Home,
        Self::Runs,
        Self::Triggers,
        Self::Executors,
        Self::Settings,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Home => "Start a task",
            Self::Runs => "All runs",
            Self::Triggers => "Triggers",
            Self::Executors => "Executors",
            Self::Settings => "Settings",
        }
    }

    /// The row icon.
    pub(crate) const fn icon(self) -> Icon {
        match self {
            Self::Home => Icon::PhHouse,
            Self::Runs => Icon::PhTerminal,
            Self::Triggers => Icon::PhTimer,
            Self::Executors => Icon::PhCpu,
            Self::Settings => Icon::PhGear,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_destination_is_still_reachable_under_its_new_name() {
        let labels: Vec<_> = Nav::ALL.iter().map(|n| n.label()).collect();
        assert_eq!(
            labels,
            [
                "Start a task",
                "All runs",
                "Triggers",
                "Executors",
                "Settings"
            ]
        );
    }
}
