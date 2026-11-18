//! What the kernel does with the `Workspace` port: place a run, release a thread's worktree when
//! the thread closes, clean on request, and reconcile at start-up.
//!
//! # Placement (see [`crate::workspace`] for the table)
//!
//! A worktree belongs to a **thread** (see [`crate::threads`]), not to a run. The first run of a
//! thread records its worktree (`preparing`, durably) before creating it, then marks it
//! `active`, on the branch `reno/<thread>`. A follow-up run of the thread finds the row and
//! reuses the worktree as it stands: its commits and uncommitted work are the continuation. One
//! whose directory vanished is made again on the branch it had, if that survives. A run that is
//! claimed again (retried or resumed) is the same case.
//!
//! # Release policy
//!
//! A run's end releases nothing: it only starts the thread's idle clock. The worktree is
//! released when the thread closes (a person, or the idle limit), under `IfMerged`:
//!
//! - no commits beyond the base and nothing uncommitted, or merged and clean: the worktree is
//!   removed and its branch deleted (compare-and-delete against the branch tip), status
//!   `removed`;
//! - unmerged commits or uncommitted changes: worktree and branch stay, status `kept`, with what
//!   they hold recorded, for a later integration step;
//! - the directory had vanished: the registration is pruned; the branch goes if it holds
//!   nothing (`removed`), else it stays (`lost`).
//!
//! A branch with commits that are not merged is deleted only with `force`. A release that fails
//! (git is gone, a timeout) leaves the row as it was; the next start's sweep, or an explicit
//! clean, tries again.
//!
//! # Start-up sweep
//!
//! After run recovery no run is live, so every open workspace (`preparing`, `active`, `kept`)
//! is reconciled: a partial creation is cleaned, and the worktree of a thread that is closed (or
//! unknown) is released again under `IfClean`. The worktree of an open thread is left alone: an
//! idle thread keeps its worktree until it closes. Directories under the worktrees area that no
//! row accounts for are logged, never removed.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use reno_proto::{
    CleanItem, CleanOutcome, CleanTarget, RunId, SandboxLevel, ThreadStatus, WorkspaceChanges,
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

    let Some(mut plan) = port.plan(&row.thread_id, &row.id, &cwd).await? else {
        if opted_in {
            record_in_place(shared, row, &cwd, InPlace::OptedIn).await;
            return Ok(in_place(InPlace::OptedIn));
        }
        return Err(WorkspaceError::NotAGitRepo { dir: cwd });
    };
    // The thread has one worktree row, the one its first run made: it is replaced, not added to.
    if let Some(old) = previous.as_ref() {
        plan.run_id = old.run_id.clone();
        // A branch that survived a vanished worktree keeps the base it was cut from.
        if OPEN.contains(&old.status)
            && old.branch == plan.branch
            && port.inspect(old).await?.branch_exists
        {
            plan.base_sha.clone_from(&old.base_sha);
        }
    }
    shared
        .db
        .begin_workspace(plan.clone())
        .await
        .map_err(store_failed)?;
    if let Err(error) = port.create(&plan).await {
        // A half-made worktree is not left for the next start to find.
        if let Err(why) = release(shared, port, &plan, ReleasePolicy::IfClean).await {
            tracing::warn!(run = %row.id, %why, "could not clean up a worktree that was not made");
        }
        return Err(error);
    }
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

/// A run ended: its thread's idle clock starts. The worktree stays, for the next run of the
/// thread, except for a run a trigger started (see [`close_if_unattended`]). Never fails the
/// caller: a problem is logged.
///
/// The clock starts on a task of its own: the run's slot is freed, and its `Done` published,
/// without waiting for a store write that only starts a clock.
pub(crate) async fn run_ended(shared: &Arc<Shared>, run: &RunId) {
    let (task, id) = (Arc::clone(shared), run.clone());
    shared.spawn(async move {
        if let Err(error) = task.db.thread_went_idle(&id).await {
            tracing::warn!(run = %id, %error, "could not record that the run's thread went idle");
        }
        task.thread_idle.notify_one();
    });
    close_if_unattended(shared, run).await;
}

/// A run a trigger started is not a conversation: nobody sends a follow-up to a nightly job, and
/// a worktree kept for a week per firing would pile up. When such a run has ended for good, its
/// thread closes with it, releasing its worktree as a run's end always did: removed when it
/// holds nothing, kept (and listed) when it holds work. An `interrupted` run may still be
/// resumed, so it keeps its worktree.
async fn close_if_unattended(shared: &Shared, run: &RunId) {
    let row = match shared.run_record(run).await {
        Ok(row) => row,
        Err(error) => {
            tracing::warn!(%run, %error, "could not read the run that ended");
            return;
        }
    };
    if row.spec.get("trigger").is_none() || !row.status.is_terminal() {
        return;
    }
    if let Err(error) =
        crate::threads::close_with(shared, &row.thread_id, ReleasePolicy::IfClean).await
    {
        tracing::warn!(%run, %error, "could not close the thread of a run a trigger started");
    }
}

/// Records where the run's branch stood when it started, for its guard and its "this run" diff.
/// A run that works in place has none. A failure is logged: the guard then falls back to the
/// thread's base commit.
pub(crate) async fn record_start(shared: &Shared, run: &RunId, fence: u64, placement: &Placement) {
    let (Some(port), Placement::Isolated(info)) = (shared.workspace.as_ref(), placement) else {
        return;
    };
    let head = match port.head(info).await {
        Ok(head) => head,
        Err(error) => {
            tracing::warn!(%run, %error, "could not read where the run's branch stands");
            return;
        }
    };
    if let Err(error) = shared.db.record_start_sha(run, fence, &head).await {
        tracing::warn!(%run, %error, "could not record where the run started");
    }
}

/// Releases one workspace under `policy` and records the result.
pub(crate) async fn release(
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
        // An idle or active thread keeps its worktree; only a half-made one is cleaned.
        let keep = info.status == WorkspaceStatus::Active && thread_is_open(shared, &info).await;
        if keep {
            continue;
        }
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

/// Whether the thread that owns `info` is still open (not closed). A thread that cannot be read
/// counts as closed, so the sweep releases what nobody can continue.
async fn thread_is_open(shared: &Shared, info: &WorkspaceInfo) -> bool {
    let Ok(row) = shared.run_record(&info.run_id).await else {
        return false;
    };
    matches!(
        shared.db.get_thread(&row.thread_id).await,
        Ok(Some(thread)) if thread.status != ThreadStatus::Closed
    )
}

/// `workspace.clean`: see `CleanTarget`. A workspace is a thread's, so cleaning one acts on its
/// thread: a worktree that is removed closes the thread.
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
        CleanTarget::Thread { thread_id } => {
            if shared.db.get_thread(thread_id).await?.is_none() {
                return Err(KernelError::ThreadNotFound(thread_id.clone()));
            }
            let info = shared
                .db
                .get_thread_workspace(thread_id)
                .await?
                .ok_or_else(|| {
                    KernelError::Invalid(format!("thread {thread_id} has no workspace"))
                })?;
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
    let single = matches!(target, CleanTarget::Run { .. } | CleanTarget::Thread { .. });

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
        if thread_is_live(shared, &info).await? {
            if single {
                return Err(WorkspaceError::InUse(info.run_id).into());
            }
            continue;
        }
        match release(shared, port, &info, policy).await {
            Ok(Released::Removed) => {
                close_thread_of(shared, &info).await;
                items.push(CleanItem {
                    run_id: info.run_id,
                    outcome: CleanOutcome::Removed,
                    reason: None,
                });
            }
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

/// Closes the thread whose worktree was just removed: there is nothing left to continue. A
/// thread that cannot be closed (it is busy again, or gone) is left as it is.
async fn close_thread_of(shared: &Shared, info: &WorkspaceInfo) {
    let Ok(row) = shared.run_record(&info.run_id).await else {
        return;
    };
    if let Err(error) = shared.db.close_thread(&row.thread_id).await {
        tracing::debug!(thread = %row.thread_id, %error, "left a thread open after its worktree went");
    }
}

pub(crate) fn describe(changes: WorkspaceChanges, policy: ReleasePolicy) -> String {
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

/// Whether a run of the workspace's thread is queued or live, which holds the worktree.
async fn thread_is_live(shared: &Shared, info: &WorkspaceInfo) -> Result<bool> {
    let row = shared.run_record(&info.run_id).await?;
    Ok(matches!(
        shared.db.get_thread(&row.thread_id).await?,
        Some(thread) if thread.status == ThreadStatus::Active
    ))
}
