//! The MOCKED completions contract (ROAD-31): what the composer offers after `@`, `/` and `$`.
//! The real daemon has no such RPC yet, so only the fake daemon implements [`CompletionDesk`];
//! the app reads these only through `Store::new_thread_files` and `Store::harness_commands`.
//! ROAD-31's real implementation replaces the trait's implementation; no composer changes.
//!
//! What is NOT here: the files of a live run (the real `fs.search_files`, scoped to a run) and the
//! app's own commands (`/model`, `/new`, ...), which the app owns.

/// Where a harness-side entry comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandKind {
    /// A slash command of the harness (`/compact`).
    Command,
    /// A skill (`$changelog`).
    Skill,
}

/// One `/command` or `$skill` the harness offers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarnessCommand {
    /// Without the trigger character: "compact".
    pub name: String,
    pub kind: CommandKind,
    /// One line: what it does.
    pub description: String,
    /// How it is used, when it takes an argument: "/review [pull request]".
    pub usage: Option<String>,
}

/// A file a new thread's project has, with its first lines for a preview.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectFile {
    /// Relative to the project root.
    pub path: String,
    pub head: Vec<String>,
}

/// Why the harness's commands were not read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandsError {
    /// The harness did not answer, or its list could not be parsed.
    Unreadable,
}

/// The seam: files of a project that has no run yet, and the harness's commands and skills.
pub trait CompletionDesk: Send + Sync {
    /// Files of project `project` (its name) whose path matches `query` fuzzily, best first, at
    /// most `limit`. An unknown project has none.
    fn project_files(&self, project: &str, query: &str, limit: usize) -> Vec<ProjectFile>;
    /// The commands and skills of harness `harness` (an executor id). Both kinds, commands
    /// first.
    fn harness_commands(&self, harness: &str) -> Result<Vec<HarnessCommand>, CommandsError>;
}

/// Whether the characters of `query` appear in `path` in order, ignoring case.
pub fn fuzzy_matches(path: &str, query: &str) -> bool {
    let mut rest = path.chars().flat_map(char::to_lowercase);
    query
        .chars()
        .flat_map(char::to_lowercase)
        .all(|want| rest.any(|have| have == want))
}
