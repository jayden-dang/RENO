//! What the kernel does with the `Workspace` port: place a run, release its worktree when it
//! ends, clean on request, and reconcile at start-up.
//!
//! # Placement (see [`crate::workspace`] for the table)
//!
//! A write run's worktree is recorded (`preparing`, durably) before it is created, then marked
//! `active`. A run that is claimed again finds its row: a worktree that is still there is reused
//! (its uncommitted work and commits are the continuation), one whose directory vanished is made
//! again on the branch it had, if that survives.
//!
//! # Release policy
//!
//! When a run ends, in any state, its worktree is released under `IfClean`:
//!
//! - no commits beyond the base and nothing uncommitted: the worktree is removed and its branch
//!   deleted (compare-and-delete against the base commit), status `removed`;
//! - commits or uncommitted changes: worktree and branch stay, status `kept`, with what they
//!   hold recorded, for a later integration step;
//! - the directory had vanished: the registration is pruned; the branch goes if it holds
//!   nothing (`removed`), else it stays (`lost`).
//!
//! A branch with commits that are not merged is deleted only by `workspace.clean` with `force`.
//! A release that fails (git is gone, a timeout) leaves the row as it was; the next start's
//! sweep, or an explicit clean, tries again. `interrupted` is not special: a run that may be
//! resumed with nothing to lose simply gets a fresh worktree when it is claimed again.
//!
//! # Start-up sweep
//!
//! After run recovery no run is live, so every open workspace (`preparing`, `active`, `kept`)
//! is reconciled by the same release: partial creations are cleaned, vanished directories are
//! pruned, ended runs' worktrees are released. Directories under the worktrees area that no row
//! accounts for are logged, never removed.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use reno_proto::{
    CleanItem, CleanOutcome, CleanTarget, RunId, RunStatus, SandboxLevel, WorkspaceChanges,
    WorkspaceInfo, WorkspaceStatus,
};
use reno_store::RunRecord;

use crate::error::{KernelError, Result};
use crate::shared::Shared;
use crate::workspace::{InPlace, Placement, ReleasePolicy, Released, Workspace, WorkspaceError};

/// The task spec key a trigger sets when its definition names the working directory: the
/// directory must exist when the run starts.
pub(crate) const CWD_NAMED: &str = "cwd_named";

const OPEN: [WorkspaceStatus; 3] = [
    WorkspaceStatus::Preparing,
    WorkspaceStatus::Active,
    WorkspaceStatus::Kept,
];

fn store_failed(error: reno_store::StoreError) -> WorkspaceError {
    WorkspaceError::Failed(format!("the store failed: {error}"))
}

/// Decides and prepares where `row`'s run works. `opted_in` is whether its bot allows writes in
/// a directory that is not a git repository.
pub(crate) async fn place(
    shared: &Shared,
    row: &RunRecord,
    cwd: PathBuf,
    sandbox: SandboxLevel,
    opted_in: bool,
) -> Result<Placement, WorkspaceError> {
    let in_place = |why: InPlace| Placement::InPlace {
        cwd: cwd.clone(),
        why,
    };
    let Some(port) = shared.workspace.as_ref() else {
        return Ok(in_place(InPlace::Unmanaged));
    };
    // A trigger that names its directory needs it to be there; Reno's own scratch is made on
    // demand and is not checked.
    let named = row.spec.get(CWD_NAMED).and_then(serde_json::Value::as_bool);
    if named == Some(true) && !port.dir_exists(&cwd).await {
        return Err(WorkspaceError::DirMissing { dir: cwd });
    }
    let decided = if sandbox == SandboxLevel::ReadOnly {
        Some(InPlace::ReadOnly)
    } else if shared
        .config
        .scratch_root
        .as_ref()
        .is_some_and(|root| cwd.starts_with(root))
    {
        Some(InPlace::Scratch)
    } else {
        None
    };
    if let Some(why) = decided {
        record_in_place(shared, row, &cwd, why).await;
        return Ok(in_place(why));
    }

    let previous = shared
        .db
        .get_workspace(&row.id)
        .await
        .map_err(store_failed)?;
    if let Some(old) = previous.as_ref().filter(|w| OPEN.contains(&w.status)) {
        let seen = port.inspect(old).await?;
        if seen.exists {
            let info = shared
                .db
                .set_workspace_status(&row.id, WorkspaceStatus::Active, None)
                .await
                .map_err(store_failed)?;
            return Ok(Placement::Isolated(info));
        }
    }

    let Some(mut plan) = port.plan(&row.id, &cwd).await? else {
        if opted_in {
            record_in_place(shared, row, &cwd, InPlace::OptedIn).await;
            return Ok(in_place(InPlace::OptedIn));
        }
        return Err(WorkspaceError::NotAGitRepo { dir: cwd });
    };
    // A branch that survived a vanished worktree keeps the base it was cut from.
    if let Some(old) = previous.as_ref().filter(|w| OPEN.contains(&w.status))
        && old.branch == plan.branch
        && port.inspect(old).await?.branch_exists
    {
        plan.base_sha.clone_from(&old.base_sha);
    }
    shared
        .db
        .begin_workspace(plan.clone())
        .await
        .map_err(store_failed)?;
    port.create(&plan).await?;
    let info = shared
        .db
        .set_workspace_status(&row.id, WorkspaceStatus::Active, None)
        .await
        .map_err(store_failed)?;
    Ok(Placement::Isolated(info))
}

async fn record_in_place(shared: &Shared, row: &RunRecord, cwd: &Path, why: InPlace) {
    let shown = cwd.to_string_lossy().into_owned();
    if let Err(error) = shared
        .db
        .record_in_place(&row.id, shown, why.reason())
        .await
    {
        tracing::warn!(run = %row.id, %error, "could not record where the run works");
    }
}

/// Releases `run`'s worktree after the run ended, under `IfClean`. Never fails the caller: a
/// problem is logged and the row left for the sweep or an explicit clean.
pub(crate) async fn release_after_run(shared: &Shared, run: &RunId) {
    let Some(port) = shared.workspace.as_ref() else {
        return;
    };
    match shared.db.get_workspace(run).await {
        Ok(Some(info)) if OPEN.contains(&info.status) => {
            if let Err(error) = release(shared, port, &info, ReleasePolicy::IfClean).await {
                tracing::warn!(%run, %error, "could not release the run's workspace");
            }
        }
        Ok(_) => {}
        Err(error) => tracing::warn!(%run, %error, "could not read the run's workspace"),
    }
}

/// Releases one workspace under `policy` and records the result.
async fn release(
    shared: &Shared,
    port: &Arc<dyn Workspace>,
    info: &WorkspaceInfo,
    policy: ReleasePolicy,
) -> Result<Released, WorkspaceError> {
    let released = port.release(info, policy).await?;
    let (status, changes) = match released {
        Released::Removed => (
            WorkspaceStatus::Removed,
            Some(WorkspaceChanges {
                commits: 0,
                dirty: false,
            }),
        ),
        Released::Kept(changes) => (WorkspaceStatus::Kept, Some(changes)),
        Released::GoneBranchKept(changes) => (WorkspaceStatus::Lost, Some(changes)),
    };
    // A force removal loses what it held: keep the last known changes, not a false "clean".
    let changes = if policy == ReleasePolicy::Force && released == Released::Removed {
        None
    } else {
        changes
    };
    shared
        .db
        .set_workspace_status(&info.run_id, status, changes)
        .await
        .map_err(store_failed)?;
    Ok(released)
}

/// Start-up: reconciles every open workspace and reports directories nobody accounts for.
pub(crate) async fn sweep(shared: &Shared) -> Result<()> {
    let Some(port) = shared.workspace.as_ref() else {
        return Ok(());
    };
    for info in shared.db.list_workspaces(&OPEN).await? {
        if let Err(error) = release(shared, port, &info, ReleasePolicy::IfClean).await {
            tracing::warn!(run = %info.run_id, %error, "could not reconcile a workspace");
        }
    }
    let known: HashSet<PathBuf> = shared
        .db
        .list_workspaces(&OPEN)
        .await?
        .into_iter()
        .map(|w| w.path)
        .collect();
    for stray in port.unknown_dirs(&known).await {
        tracing::warn!(path = %stray.display(), "a directory in the worktrees area belongs to no run");
    }
    Ok(())
}

/// `workspace.clean`: see `CleanTarget`.
pub(crate) async fn clean(
    shared: &Shared,
    target: CleanTarget,
    force: bool,
) -> Result<Vec<CleanItem>> {
    let Some(port) = shared.workspace.as_ref() else {
        return Ok(Vec::new());
    };
    let (candidates, policy) = match &target {
        CleanTarget::Run { run_id } => {
            let info = shared
                .db
                .get_workspace(run_id)
                .await?
                .ok_or_else(|| KernelError::WorkspaceNotFound(run_id.clone()))?;
            (vec![info], ReleasePolicy::IfMerged)
        }
        CleanTarget::Merged => (
            shared.db.list_workspaces(&[WorkspaceStatus::Kept]).await?,
            ReleasePolicy::IfMerged,
        ),
        CleanTarget::AllClean => (
            shared.db.list_workspaces(&OPEN).await?,
            ReleasePolicy::IfClean,
        ),
    };
    let policy = if force { ReleasePolicy::Force } else { policy };
    let single = matches!(target, CleanTarget::Run { .. });

    let mut items = Vec::new();
    for info in candidates {
        if !OPEN.contains(&info.status) {
            if single {
                items.push(CleanItem {
                    run_id: info.run_id,
                    outcome: CleanOutcome::Kept,
                    reason: Some(format!("already {:?}", info.status).to_lowercase()),
                });
            }
            continue;
        }
        if run_is_live(shared, &info.run_id).await? {
            if single {
                return Err(WorkspaceError::InUse(info.run_id).into());
            }
            continue;
        }
        match release(shared, port, &info, policy).await {
            Ok(Released::Removed) => items.push(CleanItem {
                run_id: info.run_id,
                outcome: CleanOutcome::Removed,
                reason: None,
            }),
            Ok(Released::Kept(changes)) => items.push(CleanItem {
                run_id: info.run_id,
                outcome: CleanOutcome::Kept,
                reason: Some(describe(changes, policy)),
            }),
            Ok(Released::GoneBranchKept(changes)) => items.push(CleanItem {
                run_id: info.run_id,
                outcome: CleanOutcome::Kept,
                reason: Some(format!(
                    "the directory was gone; branch {} kept with {} commit(s)",
                    info.branch, changes.commits
                )),
            }),
            Err(error) if single => return Err(error.into()),
            Err(error) => items.push(CleanItem {
                run_id: info.run_id,
                outcome: CleanOutcome::Kept,
                reason: Some(error.to_string()),
            }),
        }
    }
    Ok(items)
}

fn describe(changes: WorkspaceChanges, policy: ReleasePolicy) -> String {
    let mut parts = Vec::new();
    if changes.commits > 0 {
        parts.push(match policy {
            ReleasePolicy::IfClean => format!("{} commit(s) on the branch", changes.commits),
            _ => format!("{} commit(s) not merged", changes.commits),
        });
    }
    if changes.dirty {
        parts.push("uncommitted changes".to_owned());
    }
    if parts.is_empty() {
        parts.push("not removable".to_owned());
    }
    format!("{}; use --force to remove it anyway", parts.join(" and "))
}

/// Whether a run still holds its workspace: it is live in this kernel or in a state that is
/// not at rest.
async fn run_is_live(shared: &Shared, run: &RunId) -> Result<bool> {
    if shared.is_live(run) {
        return Ok(true);
    }
    let row = shared.run_record(run).await?;
    Ok(matches!(
        row.status,
        RunStatus::Leased
            | RunStatus::Running
            | RunStatus::WaitingInput
            | RunStatus::WaitingApproval
            | RunStatus::Evaluating
    ))
}
