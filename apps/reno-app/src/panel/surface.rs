//! What a tab can show. Every per-kind decision (title, icon, count, persisted form) is a match
//! in this one file; the panel itself only knows `Surface`.

use reno_proto::RunId;
use serde::{Deserialize, Serialize};

use crate::ui::icon::Icon;
use crate::vm::panel_home::HomeKind;

/// One thing the panel can show.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Surface {
    /// The New-tab page. Also what the panel shows with no tab.
    Home,
    /// Run facts: state, usage, executor.
    Session,
    /// The changes the run made.
    Review,
    /// A file in the worktree, by path.
    File(String),
    /// A shell, by terminal id.
    Terminal(u64),
    /// Commits on the branch.
    History,
    /// One commit, by sha.
    Commit(String),
    /// A side conversation, by its run.
    SideChat(RunId),
    /// A tab whose feature arrives in a later phase: the page says so. It goes away when the
    /// feature gets its own kind, and is not saved.
    Planned(HomeKind),
}

/// The facts a tab count is read from.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counts {
    /// Files the run changed.
    pub(crate) changed_files: usize,
}

impl Surface {
    /// The label on the tab.
    pub(crate) fn title(&self) -> String {
        match self {
            Self::Home => "New tab".into(),
            Self::Session => "Session".into(),
            Self::Review => "Review".into(),
            Self::File(path) => path
                .rsplit('/')
                .find(|s| !s.is_empty())
                .unwrap_or(path)
                .to_owned(),
            Self::Terminal(_) => "Terminal".into(),
            Self::History => "History".into(),
            Self::Commit(sha) => sha.chars().take(7).collect(),
            Self::SideChat(_) => "Side chat".into(),
            Self::Planned(kind) => kind.label().into(),
        }
    }

    /// The tab a home row opens: Changes is Review; the others are placeholders for now.
    pub(crate) fn for_row(kind: HomeKind) -> Self {
        match kind {
            HomeKind::Changes => Self::Review,
            other => Self::Planned(other),
        }
    }

    /// A stable name for selectors and accessible labels: "review", "planned-files".
    pub(crate) fn slug(&self) -> String {
        match self {
            Self::Home => "home".into(),
            Self::Session => "session".into(),
            Self::Review => "review".into(),
            Self::File(path) => format!("file-{path}"),
            Self::Terminal(id) => format!("terminal-{id}"),
            Self::History => "history".into(),
            Self::Commit(sha) => format!("commit-{sha}"),
            Self::SideChat(run) => format!("side-chat-{run}"),
            Self::Planned(kind) => {
                format!("planned-{}", kind.label().to_lowercase().replace(' ', "-"))
            }
        }
    }

    /// The tab's icon.
    pub(crate) fn icon(&self) -> Icon {
        match self {
            Self::Home => Icon::Plus,
            Self::Session => Icon::Sliders,
            Self::Review => Icon::Branch,
            Self::File(_) => Icon::File,
            Self::Terminal(_) => Icon::Terminal,
            Self::History => Icon::History,
            Self::Commit(_) => Icon::Merge,
            Self::SideChat(_) => Icon::Msg,
            Self::Planned(kind) => kind.icon(),
        }
    }

    /// The number after the label ("Review 1"), when the kind has one and it is not zero.
    pub(crate) fn count(&self, counts: &Counts) -> Option<usize> {
        match self {
            Self::Review => Some(counts.changed_files).filter(|n| *n > 0),
            _ => None,
        }
    }

    /// Whether the tab can be closed. Session is pinned.
    pub(crate) fn closable(&self) -> bool {
        !matches!(self, Self::Session)
    }

    /// The form that is saved.
    pub(crate) fn descriptor(&self) -> Descriptor {
        let (kind, key) = match self {
            Self::Home => (Kind::Home, String::new()),
            Self::Session => (Kind::Session, String::new()),
            Self::Review => (Kind::Review, String::new()),
            Self::File(path) => (Kind::File, path.clone()),
            Self::Terminal(id) => (Kind::Terminal, id.to_string()),
            Self::History => (Kind::History, String::new()),
            Self::Commit(sha) => (Kind::Commit, sha.clone()),
            Self::SideChat(run) => (Kind::SideChat, run.as_str().to_owned()),
            // A placeholder is not saved: a build that knows no such kind reads it as nothing.
            Self::Planned(_) => (Kind::Unknown, String::new()),
        };
        Descriptor { kind, key }
    }
}

/// The kind of a saved tab. A kind this build does not know reads as `Unknown`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Kind {
    Home,
    Session,
    Review,
    File,
    Terminal,
    History,
    Commit,
    SideChat,
    #[serde(other)]
    Unknown,
}

/// A tab as saved: a kind and the key that identifies it. Small and stable on purpose; the live
/// entity behind it is rebuilt on demand.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Descriptor {
    pub(crate) kind: Kind,
    #[serde(default)]
    pub(crate) key: String,
}

impl Descriptor {
    /// The tab this describes, or `None` for a kind or key this build cannot read.
    pub(crate) fn surface(&self) -> Option<Surface> {
        let key = self.key.as_str();
        match self.kind {
            Kind::Home => Some(Surface::Home),
            Kind::Session => Some(Surface::Session),
            Kind::Review => Some(Surface::Review),
            Kind::History => Some(Surface::History),
            Kind::File => (!key.is_empty()).then(|| Surface::File(key.to_owned())),
            Kind::Commit => (!key.is_empty()).then(|| Surface::Commit(key.to_owned())),
            Kind::Terminal => key.parse().ok().map(Surface::Terminal),
            Kind::SideChat => key.parse().ok().map(Surface::SideChat),
            Kind::Unknown => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> RunId {
        RunId::from_suffix("a1").expect("id")
    }

    #[test]
    fn every_kind_round_trips_through_its_descriptor() {
        for s in [
            Surface::Home,
            Surface::Session,
            Surface::Review,
            Surface::File("src/a.rs".into()),
            Surface::Terminal(7),
            Surface::History,
            Surface::Commit("abcdef0123".into()),
            Surface::SideChat(run()),
        ] {
            assert_eq!(s.descriptor().surface(), Some(s.clone()), "{s:?}");
            let json = serde_json::to_string(&s.descriptor()).expect("json");
            let back: Descriptor = serde_json::from_str(&json).expect("parse");
            assert_eq!(back.surface(), Some(s));
        }
    }

    #[test]
    fn unknown_kind_deserializes_and_is_dropped() {
        let d: Descriptor = serde_json::from_str(r#"{"kind":"browser","key":"x"}"#).expect("parse");
        assert_eq!(d.kind, Kind::Unknown);
        assert_eq!(d.surface(), None);
    }

    #[test]
    fn missing_key_defaults_and_bad_keys_drop() {
        let d: Descriptor = serde_json::from_str(r#"{"kind":"review"}"#).expect("parse");
        assert_eq!(d.surface(), Some(Surface::Review));
        for (kind, key) in [
            (Kind::File, ""),
            (Kind::Commit, ""),
            (Kind::Terminal, "nope"),
            (Kind::SideChat, "not a run id"),
        ] {
            let d = Descriptor {
                kind,
                key: key.into(),
            };
            assert_eq!(d.surface(), None, "{kind:?} {key:?}");
        }
    }

    #[test]
    fn titles() {
        assert_eq!(Surface::Home.title(), "New tab");
        assert_eq!(Surface::File("src/ui/a.rs".into()).title(), "a.rs");
        assert_eq!(Surface::File("a.rs".into()).title(), "a.rs");
        assert_eq!(Surface::Commit("abcdef0123".into()).title(), "abcdef0");
        assert_eq!(Surface::Terminal(3).title(), "Terminal");
    }

    #[test]
    fn icons_are_distinct() {
        let all = [
            Surface::Home,
            Surface::Session,
            Surface::Review,
            Surface::File("a".into()),
            Surface::Terminal(1),
            Surface::History,
            Surface::Commit("a".into()),
            Surface::SideChat(run()),
        ];
        let mut icons: Vec<_> = all.iter().map(|s| s.icon().path()).collect();
        icons.sort_unstable();
        icons.dedup();
        assert_eq!(icons.len(), all.len());
    }

    #[test]
    fn only_review_has_a_count_and_zero_is_none() {
        let c = Counts { changed_files: 3 };
        assert_eq!(Surface::Review.count(&c), Some(3));
        assert_eq!(Surface::Review.count(&Counts::default()), None);
        assert_eq!(Surface::Session.count(&c), None);
    }

    #[test]
    fn only_session_is_pinned() {
        assert!(!Surface::Session.closable());
        assert!(Surface::Review.closable());
        assert!(Surface::Home.closable());
    }
}
