//! The wire types of threads (RP0.5, decided as D10): a **thread** is one conversation in one
//! project. Its runs share one worktree and one branch `reno/<thread id>`, so a follow-up sees
//! what the run before it committed.
//!
//! - The first run of a thread makes the worktree (when it is a write run in a git repository).
//!   A follow-up, a `task.run` with `threadId`, reuses it.
//! - At most one run of a thread is live at a time: sending to a live run steers it, and a
//!   `task.run` for a thread whose run is live is refused (`Conflict`).
//! - The worktree is released when the thread is **closed**: by a person (`thread.close`) or when
//!   the thread has been idle for the daemon's idle limit (7 days by default). The same
//!   keep-or-remove rule as `workspace.clean` applies: a worktree that holds unmerged work is
//!   kept.
//! - A run with no worktree (a read-only run, a run in a folder that is not a repository) belongs
//!   to a thread too, a thread without a worktree.
//!
//! All times are milliseconds since the Unix epoch.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::{RunId, ThreadId};
use crate::workspace::{CleanOutcome, WorkspaceStatus};

/// Where a thread stands.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    /// One of its runs is queued or live.
    Active,
    /// No run is live; a follow-up may continue it. It closes by itself after the idle limit.
    Idle,
    /// A person closed it, or it sat idle for the limit, and its worktree was released (or kept
    /// for the work it holds). A follow-up may still reopen a closed thread whose worktree was
    /// kept; one whose worktree was removed is refused.
    Closed,
}

impl ThreadStatus {
    /// Every status.
    pub const ALL: [ThreadStatus; 3] = [Self::Active, Self::Idle, Self::Closed];
}

/// One run of a thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadRun {
    /// The run.
    pub run_id: RunId,
    /// The run it follows, when it is a follow-up: the thread's previous run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run: Option<RunId>,
    /// The commit the branch stood at when the run started. A run is judged by what it changed
    /// since then (`workspace.diff` with scope `run`, and the protected-path guard). Absent for
    /// a run with no worktree and for a run that has not started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_sha: Option<String>,
}

/// One thread.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadInfo {
    /// The thread.
    pub thread_id: ThreadId,
    /// Where it stands.
    pub status: ThreadStatus,
    /// The working directory its first run asked for; a follow-up uses it too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// The project: the root of the repository the worktree was cut from. Absent while the
    /// thread has no worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<PathBuf>,
    /// The worktree's root. Absent for a thread with no worktree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<PathBuf>,
    /// The branch, `reno/<thread id>` (a thread of an older run keeps that run's branch name).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The commit the branch was cut from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_sha: Option<String>,
    /// Where the worktree stands, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<WorkspaceStatus>,
    /// Its runs, oldest first.
    pub runs: Vec<ThreadRun>,
    /// When an idle thread closes by itself. Absent unless the thread is idle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_expires_at_ms: Option<u64>,
    /// When it was created.
    pub created_at_ms: u64,
    /// When it last changed: a run was added, or its last run ended.
    pub updated_at_ms: u64,
}

impl ThreadInfo {
    /// The thread's newest run.
    pub fn latest_run(&self) -> Option<&ThreadRun> {
        self.runs.last()
    }
}

/// Params of `thread.list`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListParams {
    /// Only threads in one of these statuses; every thread when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Vec<ThreadStatus>>,
}

/// Result of `thread.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadListResult {
    /// The threads, most recently changed first.
    pub threads: Vec<ThreadInfo>,
}

/// Params of `thread.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadGetParams {
    /// The thread.
    pub thread_id: ThreadId,
}

/// Params of `thread.close`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCloseParams {
    /// The thread to close.
    pub thread_id: ThreadId,
    /// Remove the worktree even when it holds unmerged commits or uncommitted changes, deleting
    /// its branch. The only way to lose work.
    #[serde(default)]
    pub force: bool,
}

/// Result of `thread.close`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThreadCloseResult {
    /// The thread, closed.
    pub thread: ThreadInfo,
    /// What became of its worktree; absent for a thread that had none (or whose worktree was
    /// already released).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<CleanOutcome>,
    /// Why the worktree was kept, in words.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
