//! Task classes and the route preview under the composer.
//!
//! PLACEHOLDER: routing lives in the kernel and is not in the protocol yet. Until the daemon
//! exposes it, this small static table stands in, mirroring the locked prototype's routing
//! rules. Nothing here is authoritative; the daemon picks the real route.

/// The class chips on the composer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum Class {
    #[default]
    Implement,
    Review,
    Research,
    Triage,
}

impl Class {
    pub(crate) const ALL: [Self; 4] = [Self::Implement, Self::Review, Self::Research, Self::Triage];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Implement => "implement",
            Self::Review => "review",
            Self::Research => "research",
            Self::Triage => "triage",
        }
    }
}

/// What the preview shows for a class.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Route {
    /// The executor id sent in `task.run`.
    pub(crate) executor_id: &'static str,
    /// The executor's display name.
    pub(crate) executor: &'static str,
    pub(crate) model: &'static str,
    pub(crate) bot: &'static str,
    /// The "why?" lines, first match wins.
    pub(crate) why: &'static [&'static str],
    /// The settings line: worktree and access.
    pub(crate) worktree: &'static str,
    pub(crate) access: &'static str,
}

pub(crate) const fn route_for(class: Class) -> Route {
    match class {
        Class::Implement => Route {
            executor_id: "claude-code",
            executor: "Claude Code",
            model: "Opus 5.5",
            bot: "builder",
            why: &["class = implement", "Claude Code → Codex (91% vs 84% pass)"],
            worktree: "new worktree from main",
            access: "Auto-accept edits",
        },
        Class::Review => Route {
            executor_id: "codex",
            executor: "Codex",
            model: "gpt-5.5",
            bot: "reviewer",
            why: &["class = review", "an executor different from the author's"],
            worktree: "read-only",
            access: "Supervised",
        },
        Class::Research => Route {
            executor_id: "codex",
            executor: "Codex",
            model: "gpt-5.5",
            bot: "scout",
            why: &["class = research", "Grok → Codex (88% vs 79%)"],
            worktree: "read-only",
            access: "Supervised",
        },
        Class::Triage => Route {
            executor_id: "claude-code",
            executor: "Claude Code",
            model: "Sonnet 5.5",
            bot: "keeper",
            why: &["anything else", "default: Claude Code"],
            worktree: "read-only",
            access: "Supervised",
        },
    }
}

/// The one-line preview: "Claude Code  Opus 5.5 · builder".
#[cfg(test)]
pub(crate) fn preview(route: &Route) -> String {
    format!("{} {} · {}", route.executor, route.model, route.bot)
}

#[cfg(test)]
mod tests {
    use reno_proto::ExecutorId;

    use super::*;

    #[test]
    fn every_class_has_a_route_with_a_valid_executor_id() {
        for class in Class::ALL {
            let route = route_for(class);
            assert!(
                route.executor_id.parse::<ExecutorId>().is_ok(),
                "{class:?}: {}",
                route.executor_id
            );
            assert!(!route.why.is_empty());
            assert!(route.why[0].contains(class.label()) || route.why[0] == "anything else");
        }
    }

    #[test]
    fn route_table_matches_the_prototype() {
        let r = route_for(Class::Implement);
        assert_eq!(preview(&r), "Claude Code Opus 5.5 · builder");
        assert_eq!(
            (r.worktree, r.access),
            ("new worktree from main", "Auto-accept edits")
        );
        let r = route_for(Class::Review);
        assert_eq!(preview(&r), "Codex gpt-5.5 · reviewer");
        assert_eq!((r.worktree, r.access), ("read-only", "Supervised"));
        assert_eq!(
            preview(&route_for(Class::Research)),
            "Codex gpt-5.5 · scout"
        );
        assert_eq!(
            preview(&route_for(Class::Triage)),
            "Claude Code Sonnet 5.5 · keeper"
        );
    }

    #[test]
    fn default_class_is_implement_and_labels_are_distinct() {
        assert_eq!(Class::default(), Class::Implement);
        let labels: Vec<_> = Class::ALL.iter().map(|c| c.label()).collect();
        assert_eq!(labels, ["implement", "review", "research", "triage"]);
    }
}
