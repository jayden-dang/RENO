//! The fake daemon's side of the mocked completions contract (see [`crate::completions`]): a
//! small file tree for each of the three demo projects, five harness commands and three skills,
//! and a switch that makes the harness's commands unreadable.

use std::sync::atomic::{AtomicBool, Ordering};

use super::FakeDaemon;
use crate::completions::{
    CommandKind, CommandsError, CompletionDesk, HarnessCommand, ProjectFile, fuzzy_matches,
};

/// `commands_unreadable`: set by [`FakeDaemon::set_commands_unreadable`].
#[derive(Debug, Default)]
pub(super) struct Sim {
    unreadable: AtomicBool,
}

const TREE: &[(&str, &[&str])] = &[
    (
        "src/main.rs",
        &[
            "use reno::App;",
            "",
            "fn main() {",
            "    App::new().run();",
            "}",
        ],
    ),
    (
        "src/lib.rs",
        &["pub mod app;", "pub mod store;", "pub mod vm;"],
    ),
    (
        "src/app.rs",
        &["pub struct App {", "    store: Store,", "}"],
    ),
    (
        "src/store.rs",
        &["pub struct Store {", "    runs: Vec<Run>,", "}"],
    ),
    (
        "src/ui/composer.rs",
        &["//! The composer: one multi-line text input."],
    ),
    (
        "src/ui/kit.rs",
        &["//! Small controls every screen shares."],
    ),
    (
        "src/vm/queue.rs",
        &["//! What waits for a run's turn to end."],
    ),
    ("Cargo.toml", &["[workspace]", "members = [\"crates/*\"]"]),
    ("README.md", &["# reno", "", "A transparent orchestrator."]),
    (
        "docs/ARCHITECTURE.md",
        &["# Architecture", "", "The daemon owns every run."],
    ),
    (
        "docs/design/app-shell.md",
        &["# App shell", "", "Geist, 13 pt rows."],
    ),
    (
        "tests/handshake.rs",
        &["#[test]", "fn negotiates_lower_version() {}"],
    ),
];

fn command(
    name: &str,
    kind: CommandKind,
    description: &str,
    usage: Option<&str>,
) -> HarnessCommand {
    HarnessCommand {
        name: name.to_owned(),
        kind,
        description: description.to_owned(),
        usage: usage.map(str::to_owned),
    }
}

fn commands() -> Vec<HarnessCommand> {
    use CommandKind::{Command, Skill};
    vec![
        command(
            "compact",
            Command,
            "Summarise the conversation to free context",
            Some("/compact [what to keep]"),
        ),
        command(
            "review",
            Command,
            "Review a pull request or the current changes",
            Some("/review [pull request]"),
        ),
        command(
            "init",
            Command,
            "Write a CLAUDE.md for this repository",
            None,
        ),
        command(
            "memory",
            Command,
            "Edit what the agent remembers about you",
            None,
        ),
        command("agents", Command, "Manage subagents", None),
        command(
            "changelog",
            Skill,
            "Write the changelog entry for the changes",
            Some("$changelog [version]"),
        ),
        command(
            "release-notes",
            Skill,
            "Draft release notes from merged pull requests",
            None,
        ),
        command(
            "frontend-design",
            Skill,
            "Distinctive, intentional interface design",
            None,
        ),
    ]
}

impl FakeDaemon {
    /// Makes the harness's commands unreadable (or readable again): the failure branch.
    pub fn set_commands_unreadable(&self, unreadable: bool) {
        self.completions
            .unreadable
            .store(unreadable, Ordering::SeqCst);
    }
}

impl CompletionDesk for FakeDaemon {
    fn project_files(&self, project: &str, query: &str, limit: usize) -> Vec<ProjectFile> {
        if !matches!(project, "reno" | "web-console" | "atlas-api") {
            return Vec::new();
        }
        let mut found: Vec<&(&str, &[&str])> = TREE
            .iter()
            .filter(|(path, _)| fuzzy_matches(path, query))
            .collect();
        // A prefix of the path first, then shorter paths.
        found.sort_by_key(|(path, _)| (!path.starts_with(query), path.len()));
        found
            .into_iter()
            .take(limit)
            .map(|(path, head)| ProjectFile {
                path: (*path).to_owned(),
                head: head.iter().map(|l| (*l).to_owned()).collect(),
            })
            .collect()
    }

    fn harness_commands(&self, _harness: &str) -> Result<Vec<HarnessCommand>, CommandsError> {
        if self.completions.unreadable.load(Ordering::SeqCst) {
            return Err(CommandsError::Unreadable);
        }
        Ok(commands())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn daemon() -> FakeDaemon {
        FakeDaemon::start(super::super::FakeConfig::default()).unwrap()
    }

    #[test]
    fn files_of_a_project_match_fuzzily_and_an_unknown_project_has_none() {
        let d = daemon();
        let found = d.project_files("reno", "src/", 20);
        assert!(found.len() >= 6);
        assert!(found.iter().take(6).all(|f| f.path.starts_with("src/")));
        let fuzzy = d.project_files("reno", "cmp", 20);
        assert!(fuzzy.iter().any(|f| f.path == "src/ui/composer.rs"));
        assert_eq!(d.project_files("reno", "src/", 3).len(), 3);
        assert!(d.project_files("elsewhere", "", 20).is_empty());
    }

    #[test]
    fn the_harness_has_five_commands_then_three_skills() {
        let all = daemon().harness_commands("claude-code").unwrap();
        let of = |kind| all.iter().filter(|c| c.kind == kind).count();
        assert_eq!((of(CommandKind::Command), of(CommandKind::Skill)), (5, 3));
        assert_eq!(all[0].name, "compact");
    }

    #[test]
    fn unreadable_commands_are_an_error_until_the_switch_is_turned_back() {
        let d = daemon();
        d.set_commands_unreadable(true);
        assert_eq!(
            d.harness_commands("claude-code"),
            Err(CommandsError::Unreadable)
        );
        d.set_commands_unreadable(false);
        assert!(d.harness_commands("claude-code").is_ok());
    }
}
