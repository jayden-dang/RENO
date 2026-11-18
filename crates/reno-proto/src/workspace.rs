//! The wire types of workspaces (T1.3): where a write run works, and what `workspace.list`,
//! `workspace.get` and `workspace.clean` carry.
//!
//! A write run never works in the user's checkout. It works in a Reno-managed git worktree on a
//! branch `reno/<thread id>` cut from the repository's `HEAD` commit when the thread's first run
//! was claimed. The worktree belongs to the thread (see [`crate::thread`]): a follow-up run
//! reuses it, and it is released when the thread closes. All times are milliseconds since the
//! Unix epoch.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::RunId;

/// Where a workspace stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceStatus {
    /// Recorded, and being created. A workspace found in this state at start-up is a crash
    /// leftover and is reconciled.
    Preparing,
    /// Created, and its thread is open: a run is working in it, or the thread is idle and may be
    /// continued by a follow-up.
    Active,
    /// Its thread was closed (or was cleaned) while it held commits or uncommitted changes, so
    /// the worktree and the branch are kept for a later integration step.
    Kept,
    /// Its worktree and branch were removed.
    Removed,
    /// Its directory vanished and was pruned; the branch may remain.
    Lost,
}

impl WorkspaceStatus {
    /// Every status.
    pub const ALL: [WorkspaceStatus; 5] = [
        Self::Preparing,
        Self::Active,
        Self::Kept,
        Self::Removed,
        Self::Lost,
    ];
}

/// What a workspace holds beyond its base commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceChanges {
    /// Commits on the branch that its base commit does not have.
    pub commits: u32,
    /// Whether the worktree has uncommitted changes or untracked files.
    pub dirty: bool,
}

impl WorkspaceChanges {
    /// True when there is nothing to lose: no commits and no uncommitted changes.
    pub fn is_clean(&self) -> bool {
        self.commits == 0 && !self.dirty
    }
}

/// One thread's workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceInfo {
    /// The run that made the worktree: the first run of its thread. A follow-up run of the
    /// thread has this same workspace.
    pub run_id: RunId,
    /// The repository's working tree root the task targeted.
    pub repo: PathBuf,
    /// The stable, collision-free key of the repository in the worktrees directory.
    pub repo_key: String,
    /// The worktree's root.
    pub path: PathBuf,
    /// The directory inside the repository the task targeted, relative to its root; empty for the
    /// root. The run's working directory is `path` joined with it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub subdir: String,
    /// The branch, `reno/<thread id>` (the thread of a run that predates threads keeps that
    /// run's own branch name).
    pub branch: String,
    /// The commit the branch was cut from.
    pub base_sha: String,
    /// Where it stands.
    pub status: WorkspaceStatus,
    /// What it held when it was last released or checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changes: Option<WorkspaceChanges>,
    /// When it was recorded.
    pub created_at_ms: u64,
    /// When it last changed.
    pub updated_at_ms: u64,
}

impl WorkspaceInfo {
    /// The working directory of the run: the worktree, plus the subdirectory the task targeted.
    pub fn cwd(&self) -> PathBuf {
        if self.subdir.is_empty() {
            self.path.clone()
        } else {
            self.path.join(&self.subdir)
        }
    }
}

/// Params of `workspace.list`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceListParams {
    /// Only workspaces in one of these statuses; every workspace when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Vec<WorkspaceStatus>>,
}

/// Result of `workspace.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceListResult {
    /// The workspaces, newest first.
    pub workspaces: Vec<WorkspaceInfo>,
}

/// Params of `workspace.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceGetParams {
    /// The run whose workspace to return.
    pub run_id: RunId,
}

/// Which workspaces `workspace.clean` looks at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum CleanTarget {
    /// One run's workspace: the worktree of the run's thread.
    Run {
        /// The run.
        run_id: RunId,
    },
    /// One thread's workspace. Needs feature `threads`.
    Thread {
        /// The thread.
        thread_id: crate::ids::ThreadId,
    },
    /// Workspaces whose branch is merged into their repository's current `HEAD`.
    Merged,
    /// Workspaces with no commits and no uncommitted changes.
    AllClean,
}

/// Params of `workspace.clean`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceCleanParams {
    /// What to clean.
    pub target: CleanTarget,
    /// Remove a workspace even when it holds unmerged commits or uncommitted changes, deleting
    /// its branch. The only way to lose work.
    #[serde(default)]
    pub force: bool,
}

/// What `workspace.clean` did with one workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CleanOutcome {
    /// The worktree and the branch were removed.
    Removed,
    /// Left as it was: see `reason`.
    Kept,
}

/// One workspace `workspace.clean` looked at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CleanItem {
    /// The run.
    pub run_id: RunId,
    /// What was done.
    pub outcome: CleanOutcome,
    /// Why it was kept, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The terminals that were open in a removed worktree and were closed with it. Needs feature
    /// `terminals`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub closed_terminals: Vec<crate::ids::TermId>,
}

/// Result of `workspace.clean`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceCleanResult {
    /// Each workspace looked at.
    pub items: Vec<CleanItem>,
}

/// What `workspace.diff` compares.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum DiffScope {
    /// Everything the thread holds against its base commit: committed, staged, unstaged and
    /// untracked. What the method did before threads, and the default.
    #[default]
    Thread,
    /// What the run changed: the worktree against the commit the branch stood at when the run
    /// started (`start_sha`). Needs feature `threads`.
    Run,
    /// One commit of the thread's branch against its parent. Needs feature `threads`.
    Commit {
        /// The commit (a hexadecimal object name, at least 7 characters).
        sha: String,
    },
}

/// Params of `workspace.diff`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDiffParams {
    /// The run whose workspace (the worktree of its thread) to compare.
    pub run_id: RunId,
    /// One file (repository-relative) whose patch to return too; none: the list only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// What to compare; the thread against its base commit when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<DiffScope>,
}

/// How a file differs from the base commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// New file.
    Added,
    /// Changed file.
    Modified,
    /// Removed file.
    Deleted,
    /// Moved, possibly edited.
    Renamed,
}

/// One changed file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChangedFile {
    /// Repository-relative path (the new one for a rename).
    pub path: String,
    /// The old path of a rename.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// How it changed.
    pub kind: ChangeKind,
    /// Lines added; 0 for a binary file.
    pub additions: u32,
    /// Lines removed; 0 for a binary file.
    pub deletions: u32,
    /// Whether git treats the file as binary (no patch).
    #[serde(default, skip_serializing_if = "is_false")]
    pub binary: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Result of `workspace.diff`: everything the run changed against its base commit, committed or
/// not, untracked files included. Computed on request from the worktree; empty once the
/// workspace has been removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDiffResult {
    /// The run.
    pub run_id: RunId,
    /// The branch, `reno/<thread id>` (the thread of a run that predates threads keeps that
    /// run's own branch name).
    pub branch: String,
    /// The commit the branch was cut from.
    pub base_sha: String,
    /// Commits on the branch beyond the base.
    pub commits: u32,
    /// The changed files, by path.
    pub files: Vec<ChangedFile>,
    /// The patch of the requested file, cut at 256 KiB; none when no file was asked for or the
    /// file is binary or unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<String>,
    /// Whether `patch` was cut.
    #[serde(default, skip_serializing_if = "is_false")]
    pub patch_truncated: bool,
}
