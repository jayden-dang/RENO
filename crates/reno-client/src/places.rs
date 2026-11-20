//! The MOCKED places contract (ROAD-31): where a new task runs. The projects Reno knows, the
//! recent ones, a project's branches (a page and a total), its existing worktrees, cloning a
//! repository from a URL, and the place a started run was given. The real daemon has no such
//! RPC (and `task.run` has no branch or checkout field, only `cwd`), so only the fake daemon
//! implements [`PlaceDesk`]; the app reads it only through the `Store::place_*` entries.

use std::fmt;
use std::path::PathBuf;

use reno_proto::RunId;

/// A repository on this Mac.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Project {
    /// The folder's name: "reno".
    pub name: String,
    pub path: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    /// The repository's default branch.
    pub default: bool,
    /// A worktree already has this branch checked out.
    pub worktree: Option<String>,
}

/// Some of a project's branches, and how many there are in all (that match the query).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BranchPage {
    pub branches: Vec<Branch>,
    pub total: usize,
}

/// A worktree that exists already.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    /// The folder's name: "reno-model-chip".
    pub name: String,
    pub path: PathBuf,
    pub branch: String,
}

/// Where in the project the task works.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Checkout {
    /// A fresh worktree on its own branch off the chosen branch (the default).
    NewWorktree,
    /// The project's own checkout, on the chosen branch.
    ThisCheckout,
    /// An existing worktree, by name.
    Worktree(String),
}

/// A project, a branch and a checkout: where a task runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Place {
    pub project: String,
    pub branch: String,
    pub checkout: Checkout,
}

/// Why a clone did not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CloneError {
    /// No such repository, or it is private.
    NotFound,
    /// Nothing was typed.
    Empty,
}

impl fmt::Display for CloneError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotFound => "Could not clone: repository not found",
            Self::Empty => "Paste a repository URL to clone",
        })
    }
}

impl std::error::Error for CloneError {}

/// The seam.
pub trait PlaceDesk: Send + Sync {
    /// Every project, by name.
    fn projects(&self) -> Vec<Project>;
    /// The projects used lately, most recent first.
    fn recent_projects(&self) -> Vec<Project>;
    /// Branches of `project` whose name contains `query`, default first, at most `limit`;
    /// `total` counts all that match.
    fn branches(&self, project: &str, query: &str, limit: usize) -> BranchPage;
    /// The worktrees `project` already has.
    fn worktrees(&self, project: &str) -> Vec<Worktree>;
    /// How long cloning `url` takes, in ms: the app shows progress that long, then calls
    /// [`PlaceDesk::clone_project`].
    fn clone_duration_ms(&self, url: &str) -> u64;
    /// Clones `url`; the project is known afterwards.
    fn clone_project(&self, url: &str) -> Result<Project, CloneError>;
    /// Binds the place a task was started in to its run.
    fn record_place(&self, run: &RunId, place: Place);
    /// The place the run was started in; `None` for a run that was not started from here.
    fn run_place(&self, run: &RunId) -> Option<Place>;
}
