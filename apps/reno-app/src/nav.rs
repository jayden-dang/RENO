//! Navigation destinations. Home and a single run are built in Phase 0.

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
        }
    }

    /// Whether the screen exists yet.
    pub(crate) const fn is_built(self) -> bool {
        matches!(self, Self::Home)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_home_is_built_and_labels_are_unique() {
        let all: Vec<Nav> = Nav::MAIN.into_iter().chain(Nav::ORCHESTRATION).collect();
        assert_eq!(all.len(), 12);
        assert_eq!(all.iter().filter(|n| n.is_built()).count(), 1);
        assert!(Nav::Home.is_built());
        let mut labels: Vec<_> = all.iter().map(|n| n.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), 12);
    }
}
