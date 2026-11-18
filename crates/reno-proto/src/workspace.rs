//! The wire types of workspaces (T1.3): where a write run works, and what `workspace.list`,
//! `workspace.get` and `workspace.clean` carry.
//!
//! A write run never works in the user's checkout. It works in a Reno-managed git worktree on a
//! branch `reno/<run id>` cut from the repository's `HEAD` commit when the run was claimed. All
//! times are milliseconds since the Unix epoch.

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
    /// Created, and its run has not ended (or may be resumed).
    Active,
    /// Its run ended and it holds commits or uncommitted changes, so the worktree and the branch
    /// are kept for a later integration step.
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

/// One run's workspace.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceInfo {
    /// The run it belongs to.
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
    /// The branch, `reno/<run id>`.
    pub branch: String,
    /// The commit the branch was cut from.
    pub base_sha: String,
    /// Where it stands.
    pub status: WorkspaceStatus,
    /// What it held when its run last ended.
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
    /// One run's workspace.
    Run {
        /// The run.
        run_id: RunId,
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
}

/// Result of `workspace.clean`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceCleanResult {
    /// Each workspace looked at.
    pub items: Vec<CleanItem>,
}

/// Params of `workspace.diff`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceDiffParams {
    /// The run whose workspace to compare with its base commit.
    pub run_id: RunId,
    /// One file (repository-relative) whose patch to return too; none: the list only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
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
    /// The branch, `reno/<run id>`.
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
