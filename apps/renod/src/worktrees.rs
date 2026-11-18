//! The `Workspace` port over git worktrees (T1.3): a thin adapter from the kernel's trait to
//! `reno_harness::proc::git_workspace`, which owns every `git` invocation, the layout under the
//! data directory's `worktrees/` and the rules that keep the user's checkout untouched.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use reno_harness::proc::git_workspace::{
    Changes, GitError, GitWorktrees, Inspection as GitInspection, Location, Release,
    Released as GitReleased,
};
use reno_kernel::{Inspection, ReleasePolicy, Released, Workspace, WorkspaceError};
use reno_proto::{RunId, WorkspaceChanges, WorkspaceInfo, WorkspaceStatus};

/// Git worktrees under `<data dir>/worktrees/<repo key>/<run id>`.
#[derive(Debug)]
pub struct GitWorkspace {
    git: GitWorktrees,
}

impl GitWorkspace {
    /// A port keeping worktrees under `root`.
    pub fn new(root: PathBuf) -> Self {
        Self {
            git: GitWorktrees::new(root),
        }
    }
}

fn location(info: &WorkspaceInfo) -> Location {
    Location {
        repo: info.repo.clone(),
        repo_key: info.repo_key.clone(),
        path: info.path.clone(),
        subdir: info.subdir.clone(),
        branch: info.branch.clone(),
        base_sha: info.base_sha.clone(),
    }
}

fn changes(changes: Changes) -> WorkspaceChanges {
    WorkspaceChanges {
        commits: changes.commits,
        dirty: changes.dirty,
    }
}

fn error(error: GitError) -> WorkspaceError {
    match error {
        GitError::NoCommits { repo } => WorkspaceError::NoCommits { repo },
        GitError::Bare { repo } => WorkspaceError::BareRepository { repo },
        GitError::PathTaken { path } => WorkspaceError::PathTaken { path },
        GitError::Unsafe(why) => WorkspaceError::Unsafe(why),
        GitError::NotARepo { dir } => WorkspaceError::NotAGitRepo { dir },
        error @ (GitError::GitMissing(_)
        | GitError::Failed { .. }
        | GitError::Timeout { .. }
        | GitError::Io { .. }) => WorkspaceError::Failed(error.to_string()),
    }
}

#[async_trait]
impl Workspace for GitWorkspace {
    async fn plan(&self, run: &RunId, dir: &Path) -> Result<Option<WorkspaceInfo>, WorkspaceError> {
        let planned = self.git.plan(run.as_str(), dir).await.map_err(error)?;
        Ok(planned.map(|loc| WorkspaceInfo {
            run_id: run.clone(),
            repo: loc.repo,
            repo_key: loc.repo_key,
            path: loc.path,
            subdir: loc.subdir,
            branch: loc.branch,
            base_sha: loc.base_sha,
            status: WorkspaceStatus::Preparing,
            changes: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }))
    }

    async fn project_of(&self, dir: &Path) -> Option<PathBuf> {
        reno_harness::proc::git_workspace::project_root(dir).await
    }

    async fn dir_exists(&self, dir: &Path) -> bool {
        tokio::fs::metadata(dir)
            .await
            .is_ok_and(|meta| meta.is_dir())
    }

    async fn create(&self, plan: &WorkspaceInfo) -> Result<(), WorkspaceError> {
        self.git.create(&location(plan)).await.map_err(error)
    }

    async fn inspect(&self, workspace: &WorkspaceInfo) -> Result<Inspection, WorkspaceError> {
        let GitInspection {
            exists,
            branch_exists,
            changes: held,
        } = self
            .git
            .inspect(&location(workspace))
            .await
            .map_err(error)?;
        Ok(Inspection {
            exists,
            branch_exists,
            changes: changes(held),
        })
    }

    async fn release(
        &self,
        workspace: &WorkspaceInfo,
        policy: ReleasePolicy,
    ) -> Result<Released, WorkspaceError> {
        let how = match policy {
            ReleasePolicy::IfClean => Release::IfClean,
            ReleasePolicy::IfMerged => Release::IfMerged,
            ReleasePolicy::Force => Release::Force,
        };
        Ok(
            match self
                .git
                .release(&location(workspace), how)
                .await
                .map_err(error)?
            {
                GitReleased::Removed => Released::Removed,
                GitReleased::Kept(held) => Released::Kept(changes(held)),
                GitReleased::GoneBranchKept(held) => Released::GoneBranchKept(changes(held)),
            },
        )
    }

    async fn prune(&self, repo: &Path) -> Result<(), WorkspaceError> {
        self.git.prune(repo).await.map_err(error)
    }

    async fn unknown_dirs(&self, known: &HashSet<PathBuf>) -> Vec<PathBuf> {
        self.git.unknown_dirs(known).await
    }

    async fn protected_changes(
        &self,
        workspace: &WorkspaceInfo,
    ) -> Result<Vec<String>, WorkspaceError> {
        self.git
            .protected_changes(&location(workspace))
            .await
            .map_err(error)
    }

    async fn revert_protected(
        &self,
        workspace: &WorkspaceInfo,
        paths: &[String],
    ) -> Result<(), WorkspaceError> {
        self.git
            .revert_protected(&location(workspace), paths)
            .await
            .map_err(error)
    }
}
