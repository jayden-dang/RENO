//! The `Workspace` port (ARCH-7): where a write run works.
//!
//! The kernel never runs `git` or touches a directory. It asks the port to **plan** a worktree
//! for a run (read-only: which repository, which base commit, which path), records that plan in
//! the store, and only then asks the port to **create** it (ARCH-9: intent before effect). When a
//! run ends it asks the port to **release** the worktree under a policy; the port reports what
//! it did and what the worktree held. The git implementation is `reno_harness::proc::git_workspace`,
//! adapted to this trait in `renod`; [`FakeWorkspace`] is the in-memory double the kernel's tests
//! use.
//!
//! # Placement
//!
//! [`Placement`] is the typed decision of where a run works, so nothing falls back silently:
//!
//! | Run | Placement |
//! |---|---|
//! | no `Workspace` port (a kernel built without one) | in place, [`InPlace::Unmanaged`] |
//! | sandbox `read_only` | in place, [`InPlace::ReadOnly`] |
//! | directory under the configured scratch root (the daemon's own `workspace/`) | in place, [`InPlace::Scratch`] |
//! | write run in a git repository | a worktree on `reno/<run id>` ([`Placement::Isolated`]) |
//! | write run, not a git repository, the bot has `in_place_writes` or the task opted in (`task.run` `inPlace`) | in place, [`InPlace::OptedIn`] |
//! | write run, not a git repository, no opt-in | refused: [`WorkspaceError::NotAGitRepo`] |
//!
//! A git repository is never written in place, opt-in or not.

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use reno_proto::{RunId, WorkspaceChanges, WorkspaceInfo, WorkspaceStatus};

/// Why a workspace could not be prepared, inspected or released. Typed, so the run's diagnostic
/// and the wire error say which rule applied.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkspaceError {
    /// A write run targeted a directory that is not in a git repository, and neither its bot nor
    /// the task allows in-place writes.
    #[error(
        "{} is not in a git repository, so a write run cannot be isolated there; run it in a \
         git repository, use a read-only sandbox, or opt in: `in_place_writes` in the bot, or `--in-place` on the task",
        dir.display()
    )]
    NotAGitRepo {
        /// The directory the run targeted.
        dir: PathBuf,
    },
    /// The repository has no commits, so there is nothing to branch from.
    #[error("the repository at {} has no commits yet; make one first", repo.display())]
    NoCommits {
        /// The repository.
        repo: PathBuf,
    },
    /// The repository is bare: it has no working tree.
    #[error("{} is a bare repository", repo.display())]
    BareRepository {
        /// The repository.
        repo: PathBuf,
    },
    /// The worktree's path is taken by something else.
    #[error("{} exists and is not this run's worktree", path.display())]
    PathTaken {
        /// The path.
        path: PathBuf,
    },
    /// A value was refused before it reached git.
    #[error("refused: {0}")]
    Unsafe(String),
    /// The run's workspace is in use: its run has not ended.
    #[error("the workspace of run {0} is in use")]
    InUse(RunId),
    /// Git or the file system failed, or git timed out.
    #[error("{0}")]
    Failed(String),
}

/// What a worktree and its branch hold.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Inspection {
    /// The worktree directory exists.
    pub exists: bool,
    /// The branch exists.
    pub branch_exists: bool,
    /// What it holds.
    pub changes: WorkspaceChanges,
}

/// How much [`Workspace::release`] may remove.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleasePolicy {
    /// Only what holds nothing to lose: no commits beyond the base, nothing uncommitted.
    IfClean,
    /// Also what is merged into the repository's current `HEAD`, when nothing is uncommitted.
    IfMerged,
    /// Everything, deleting the branch and losing what it held. Only on an explicit force.
    Force,
}

/// What [`Workspace::release`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Released {
    /// The worktree and the branch are gone.
    Removed,
    /// Left as it was, because of what it holds.
    Kept(WorkspaceChanges),
    /// The directory had already vanished and was pruned; the branch was kept because it holds
    /// commits.
    GoneBranchKept(WorkspaceChanges),
}

/// The `Workspace` port: git worktrees for write runs.
#[async_trait]
pub trait Workspace: Send + Sync + fmt::Debug {
    /// Where the worktree for `run` would be, for a task targeting `dir`: repository, key,
    /// path, branch, base commit and the relative subdirectory. Read-only. `Ok(None)` means
    /// `dir` is not in a git repository. The returned status is `Preparing`.
    async fn plan(&self, run: &RunId, dir: &Path) -> Result<Option<WorkspaceInfo>, WorkspaceError>;

    /// Creates the worktree and branch of `plan`. Idempotent for the same plan.
    async fn create(&self, plan: &WorkspaceInfo) -> Result<(), WorkspaceError>;

    /// What the worktree and branch of `workspace` hold now.
    async fn inspect(&self, workspace: &WorkspaceInfo) -> Result<Inspection, WorkspaceError>;

    /// Removes what `policy` allows. A branch with commits that are not merged is never deleted
    /// unless the policy is [`ReleasePolicy::Force`].
    async fn release(
        &self,
        workspace: &WorkspaceInfo,
        policy: ReleasePolicy,
    ) -> Result<Released, WorkspaceError>;

    /// Forgets the worktrees of `repo` whose directory vanished.
    async fn prune(&self, repo: &Path) -> Result<(), WorkspaceError>;

    /// Directories in the worktrees area that no workspace in `known` accounts for. Reported,
    /// never removed: they may hold work.
    async fn unknown_dirs(&self, known: &HashSet<PathBuf>) -> Vec<PathBuf>;
}

/// Why a run works in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InPlace {
    /// The kernel has no `Workspace` port.
    Unmanaged,
    /// The run's sandbox is read-only: it writes nothing.
    ReadOnly,
    /// The directory is Reno's own scratch directory, not the user's.
    Scratch,
    /// The directory is not a git repository and the bot allows in-place writes.
    OptedIn,
}

impl InPlace {
    /// The word the `workspace.in_place` event carries.
    pub fn reason(self) -> &'static str {
        match self {
            Self::Unmanaged => "unmanaged",
            Self::ReadOnly => "read_only",
            Self::Scratch => "scratch",
            Self::OptedIn => "opted_in",
        }
    }
}

/// Where a run works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Placement {
    /// In the directory the task named.
    InPlace {
        /// That directory.
        cwd: PathBuf,
        /// Why.
        why: InPlace,
    },
    /// In a Reno-managed worktree.
    Isolated(WorkspaceInfo),
}

impl Placement {
    /// The run's working directory.
    pub fn cwd(&self) -> PathBuf {
        match self {
            Self::InPlace { cwd, .. } => cwd.clone(),
            Self::Isolated(info) => info.cwd(),
        }
    }
}

/// An in-memory `Workspace` for tests: repositories are directories you declare, worktrees are
/// records, and nothing touches a disk. It keeps a log of its calls.
#[derive(Debug, Default)]
pub struct FakeWorkspace {
    state: Mutex<FakeState>,
}

#[derive(Debug, Default)]
struct FakeState {
    repos: Vec<PathBuf>,
    trees: BTreeMap<RunId, FakeTree>,
    calls: Vec<String>,
    fail_create: Option<WorkspaceError>,
    merged: HashSet<RunId>,
}

#[derive(Debug)]
struct FakeTree {
    exists: bool,
    branch: bool,
    changes: WorkspaceChanges,
}

/// Where the fake puts worktrees.
pub const FAKE_WORKTREES: &str = "/fake/worktrees";

impl FakeWorkspace {
    /// A port that knows no repository.
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Declares `repo` a git repository: every directory under it is in it.
    #[must_use]
    pub fn with_repo(self, repo: impl Into<PathBuf>) -> Self {
        self.lock().repos.push(repo.into());
        self
    }

    /// Makes the next `create` fail with `error`.
    pub fn fail_next_create(&self, error: WorkspaceError) {
        self.lock().fail_create = Some(error);
    }

    /// Says what `run`'s worktree holds, as an agent would have left it.
    pub fn set_changes(&self, run: &RunId, changes: WorkspaceChanges) {
        if let Some(tree) = self.lock().trees.get_mut(run) {
            tree.changes = changes;
        }
    }

    /// Marks `run`'s branch as merged into its repository's `HEAD`.
    pub fn set_merged(&self, run: &RunId) {
        self.lock().merged.insert(run.clone());
    }

    /// Makes `run`'s worktree directory vanish, as if deleted by hand.
    pub fn vanish(&self, run: &RunId) {
        if let Some(tree) = self.lock().trees.get_mut(run) {
            tree.exists = false;
        }
    }

    /// Whether `run` has a worktree directory.
    pub fn exists(&self, run: &RunId) -> bool {
        self.lock().trees.get(run).is_some_and(|t| t.exists)
    }

    /// Whether `run`'s branch exists.
    pub fn branch_exists(&self, run: &RunId) -> bool {
        self.lock().trees.get(run).is_some_and(|t| t.branch)
    }

    /// Every call made so far, as `plan <run>`, `create <run>`, `release <run> <policy>`...
    pub fn calls(&self) -> Vec<String> {
        self.lock().calls.clone()
    }
}

#[async_trait]
impl Workspace for FakeWorkspace {
    async fn plan(&self, run: &RunId, dir: &Path) -> Result<Option<WorkspaceInfo>, WorkspaceError> {
        let mut state = self.lock();
        state.calls.push(format!("plan {run}"));
        let Some(repo) = state
            .repos
            .iter()
            .find(|repo| dir.starts_with(repo))
            .cloned()
        else {
            return Ok(None);
        };
        let subdir = dir
            .strip_prefix(&repo)
            .map(|rel| rel.to_string_lossy().into_owned())
            .unwrap_or_default();
        let key = format!(
            "{}-000000000000",
            repo.file_name()
                .map_or_else(|| "repo".into(), |n| n.to_string_lossy())
        );
        Ok(Some(WorkspaceInfo {
            run_id: run.clone(),
            path: Path::new(FAKE_WORKTREES).join(&key).join(run.as_str()),
            repo,
            repo_key: key,
            subdir,
            branch: format!("reno/{run}"),
            base_sha: "0".repeat(40),
            status: WorkspaceStatus::Preparing,
            changes: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }))
    }

    async fn create(&self, plan: &WorkspaceInfo) -> Result<(), WorkspaceError> {
        let mut state = self.lock();
        state.calls.push(format!("create {}", plan.run_id));
        if let Some(error) = state.fail_create.take() {
            return Err(error);
        }
        let tree = state.trees.entry(plan.run_id.clone()).or_insert(FakeTree {
            exists: true,
            branch: true,
            changes: WorkspaceChanges {
                commits: 0,
                dirty: false,
            },
        });
        tree.exists = true;
        tree.branch = true;
        Ok(())
    }

    async fn inspect(&self, workspace: &WorkspaceInfo) -> Result<Inspection, WorkspaceError> {
        let mut state = self.lock();
        state.calls.push(format!("inspect {}", workspace.run_id));
        Ok(match state.trees.get(&workspace.run_id) {
            Some(tree) => Inspection {
                exists: tree.exists,
                branch_exists: tree.branch,
                changes: tree.changes,
            },
            None => Inspection {
                exists: false,
                branch_exists: false,
                changes: WorkspaceChanges {
                    commits: 0,
                    dirty: false,
                },
            },
        })
    }

    async fn release(
        &self,
        workspace: &WorkspaceInfo,
        policy: ReleasePolicy,
    ) -> Result<Released, WorkspaceError> {
        let mut state = self.lock();
        state
            .calls
            .push(format!("release {} {policy:?}", workspace.run_id));
        let merged = state.merged.contains(&workspace.run_id);
        let Some(tree) = state.trees.get_mut(&workspace.run_id) else {
            return Ok(Released::Removed);
        };
        let removable = match policy {
            ReleasePolicy::Force => true,
            ReleasePolicy::IfClean => tree.changes.is_clean(),
            ReleasePolicy::IfMerged => tree.changes.is_clean() || (merged && !tree.changes.dirty),
        };
        if !removable {
            return Ok(if tree.exists {
                Released::Kept(tree.changes)
            } else {
                tree.branch = true;
                Released::GoneBranchKept(tree.changes)
            });
        }
        tree.exists = false;
        tree.branch = false;
        Ok(Released::Removed)
    }

    async fn prune(&self, repo: &Path) -> Result<(), WorkspaceError> {
        self.lock().calls.push(format!("prune {}", repo.display()));
        Ok(())
    }

    async fn unknown_dirs(&self, _known: &HashSet<PathBuf>) -> Vec<PathBuf> {
        Vec::new()
    }
}
