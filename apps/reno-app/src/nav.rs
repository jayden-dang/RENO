//! Navigation destinations. The sidebar lists Home, Runs and Triggers; Settings is the sidebar's
//! footer row, and Executors lives inside Settings (ui-brief 2026-11-19, settings).
//! `Nav::Executors` stays a destination so the composer, a run and the palette can deep-link to
//! Settings › Executors.

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
    /// The sidebar's rows, in order.
    pub(crate) const ROWS: [Self; 3] = [Self::Home, Self::Runs, Self::Triggers];

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
            Self::Home => "Home",
            Self::Runs => "Runs",
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
    fn the_sidebar_lists_three_rows_and_settings_is_not_one() {
        let labels: Vec<_> = Nav::ROWS.iter().map(|n| n.label()).collect();
        assert_eq!(labels, ["Home", "Runs", "Triggers"]);
    }

    #[test]
    fn every_destination_is_still_reachable() {
        let labels: Vec<_> = Nav::ALL.iter().map(|n| n.label()).collect();
        assert_eq!(
            labels,
            ["Home", "Runs", "Triggers", "Executors", "Settings"]
        );
    }
}
