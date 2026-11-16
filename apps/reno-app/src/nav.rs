//! Navigation destinations. Home, Inbox, Runs, Triggers, Executors and Settings are built; the
//! rest are listed in the sidebar and say so when chosen.

use crate::ui::icon::Icon;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Nav {
    Home,
    Inbox,
    Runs,
    Flows,
    Bots,
    Teams,
    Boards,
    Messages,
    Evaluations,
    Improvements,
    Triggers,
    Executors,
    /// Reached from the sidebar footer and the palette, not from a row.
    Settings,
}

impl Nav {
    /// The top group of the sidebar.
    pub(crate) const MAIN: [Self; 6] = [
        Self::Home,
        Self::Inbox,
        Self::Runs,
        Self::Flows,
        Self::Bots,
        Self::Teams,
    ];

    /// The "ORCHESTRATION" group.
    pub(crate) const ORCHESTRATION: [Self; 6] = [
        Self::Boards,
        Self::Messages,
        Self::Evaluations,
        Self::Improvements,
        Self::Triggers,
        Self::Executors,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Home => "Home",
            Self::Inbox => "Inbox",
            Self::Runs => "Runs",
            Self::Flows => "Flows",
            Self::Bots => "Bots",
            Self::Teams => "Teams",
            Self::Boards => "Boards",
            Self::Messages => "Messages",
            Self::Evaluations => "Evaluations",
            Self::Improvements => "Improvements",
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
            Self::Flows => Icon::Flow,
            Self::Bots => Icon::Bot,
            Self::Teams => Icon::Team,
            Self::Boards => Icon::Board,
            Self::Messages => Icon::Msg,
            Self::Evaluations => Icon::Eval,
            Self::Improvements => Icon::Wand,
            Self::Triggers => Icon::Clock,
            Self::Executors => Icon::Cpu,
            Self::Settings => Icon::Settings,
        }
    }

    /// Whether the screen exists yet.
    pub(crate) const fn is_built(self) -> bool {
        matches!(
            self,
            Self::Home
                | Self::Inbox
                | Self::Runs
                | Self::Triggers
                | Self::Executors
                | Self::Settings
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_screens_and_unique_labels() {
        let all: Vec<Nav> = Nav::MAIN.into_iter().chain(Nav::ORCHESTRATION).collect();
        assert_eq!(all.len(), 12);
        let built: Vec<_> = all
            .iter()
            .filter(|n| n.is_built())
            .map(|n| n.label())
            .collect();
        assert_eq!(built, ["Home", "Inbox", "Runs", "Triggers", "Executors"]);
        assert!(Nav::Settings.is_built() && !all.contains(&Nav::Settings));
        let mut labels: Vec<_> = all.iter().map(|n| n.label()).collect();
        labels.push(Nav::Settings.label());
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), 13);
    }
}
