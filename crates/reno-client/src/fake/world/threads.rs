//! The fake daemon's threads: every seeded run is a thread of its own, and a `task.run` with
//! `threadId` joins one. A thread's worktree is the one its first run has (see `changes`).

use std::collections::BTreeSet;

use reno_proto::{
    CleanOutcome, ErrorCode, ErrorObject, RunId, ThreadCloseResult, ThreadId, ThreadInfo,
    ThreadRun, ThreadStatus, WorkspaceStatus,
};

use super::World;
use crate::fake::changes;

/// What a thread has been through that its runs do not say.
#[derive(Debug, Default)]
pub(in crate::fake) struct ThreadState {
    /// Closed by a person.
    closed: BTreeSet<ThreadId>,
    /// Closed, and the worktree removed with it.
    removed: BTreeSet<ThreadId>,
}

/// The thread a run starts: `thr_` plus the run's suffix, as the daemon names it.
pub(in crate::fake) fn thread_of_run(run: &RunId) -> ThreadId {
    let suffix = run.as_str().trim_start_matches("run_");
    ThreadId::from_suffix(suffix).unwrap_or_else(|_| ThreadId::from_suffix("x").expect("an id"))
}

/// The idle limit the fake shows: seven days, as the daemon's default.
const IDLE_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;

fn conflict(why: String) -> ErrorObject {
    ErrorObject::new(ErrorCode::Conflict, why)
}

impl World {
    fn thread_run_ids(&self, thread: &ThreadId) -> Vec<&super::RunRecord> {
        let mut runs: Vec<_> = self.runs.iter().filter(|r| &r.thread == thread).collect();
        runs.sort_by_key(|r| (r.summary.created_at_ms, r.summary.run_id.clone()));
        runs
    }

    /// Whether `task.run` may join `thread`: it exists, has no live run, and still has a
    /// worktree to continue (or never had one).
    pub(in crate::fake) fn check_thread_follow_up(
        &self,
        thread: &ThreadId,
    ) -> Result<(), ErrorObject> {
        let runs = self.thread_run_ids(thread);
        let Some(latest) = runs.last() else {
            return Err(ErrorObject::new(
                ErrorCode::NotFound,
                format!("thread {thread} not found"),
            ));
        };
        if let Some(live) = runs.iter().find(|r| !r.summary.status.is_terminal()) {
            return Err(conflict(format!(
                "thread {thread} has a run in progress ({})",
                live.summary.run_id
            )));
        }
        if self.threads.removed.contains(thread) {
            return Err(conflict(format!(
                "thread {thread} is closed and its worktree is gone; start a new one"
            )));
        }
        let _ = latest;
        Ok(())
    }

    /// A run was added to `thread`: it is open again.
    pub(in crate::fake) fn reopen_thread(&mut self, thread: &ThreadId) {
        self.threads.closed.remove(thread);
    }

    /// The thread's newest run, which the next one follows.
    pub(in crate::fake) fn latest_run_of(&self, thread: &ThreadId) -> Option<RunId> {
        self.thread_run_ids(thread)
            .last()
            .map(|r| r.summary.run_id.clone())
    }

    /// The thread's first run: the one whose worktree it is.
    fn owner_of(&self, thread: &ThreadId) -> Option<RunId> {
        self.thread_run_ids(thread)
            .first()
            .map(|r| r.summary.run_id.clone())
    }

    /// The worktree `run` works in: its thread's, as `workspace.get` shows it.
    pub(in crate::fake) fn workspace_of(&self, run: &RunId) -> Option<reno_proto::WorkspaceInfo> {
        let record = self.run(run)?;
        let owner = self.owner_of(&record.thread)?;
        let owner_status = self.run(&owner)?.summary.status;
        let mut info = changes::info(&owner, owner_status)?;
        // The fake keeps what it always showed (a run that ended holds its worktree as `kept`);
        // only a close that removed the worktree changes it.
        if self.threads.removed.contains(&record.thread) {
            info.status = WorkspaceStatus::Removed;
        }
        // The owner may have ended while a follow-up runs.
        info.run_id = owner;
        Some(info)
    }

    pub(in crate::fake) fn thread_info(&self, thread: &ThreadId) -> Option<ThreadInfo> {
        let runs = self.thread_run_ids(thread);
        let first = runs.first()?;
        let live = runs.iter().any(|r| !r.summary.status.is_terminal());
        let status = if self.threads.closed.contains(thread) {
            ThreadStatus::Closed
        } else if live {
            ThreadStatus::Active
        } else {
            ThreadStatus::Idle
        };
        let workspace = self.workspace_of(&first.summary.run_id);
        let updated = runs
            .iter()
            .map(|r| r.summary.updated_at_ms)
            .max()
            .unwrap_or(first.summary.created_at_ms);
        Some(ThreadInfo {
            thread_id: thread.clone(),
            status,
            cwd: first.summary.cwd.clone(),
            project: workspace.as_ref().map(|w| w.repo.clone()),
            worktree: workspace.as_ref().map(|w| w.path.clone()),
            branch: workspace.as_ref().map(|w| w.branch.clone()),
            base_sha: workspace.as_ref().map(|w| w.base_sha.clone()),
            workspace: workspace.as_ref().map(|w| w.status),
            runs: runs
                .iter()
                .enumerate()
                .map(|(n, r)| ThreadRun {
                    run_id: r.summary.run_id.clone(),
                    parent_run: r.parent.clone(),
                    start_sha: workspace.as_ref().map(|w| {
                        if n == 0 {
                            w.base_sha.clone()
                        } else {
                            changes::FOLLOW_UP_START.to_owned()
                        }
                    }),
                })
                .collect(),
            idle_expires_at_ms: (status == ThreadStatus::Idle).then_some(updated + IDLE_TTL_MS),
            created_at_ms: first.summary.created_at_ms,
            updated_at_ms: updated,
        })
    }

    /// Every thread, most recently changed first.
    pub(in crate::fake) fn thread_infos(&self) -> Vec<ThreadInfo> {
        let mut ids: Vec<ThreadId> = Vec::new();
        for run in &self.runs {
            if !ids.contains(&run.thread) {
                ids.push(run.thread.clone());
            }
        }
        let mut all: Vec<ThreadInfo> = ids.iter().filter_map(|t| self.thread_info(t)).collect();
        all.sort_by(|a, b| (b.updated_at_ms, &b.thread_id).cmp(&(a.updated_at_ms, &a.thread_id)));
        all
    }

    /// `thread.close`. A worktree that holds work is kept unless `force`.
    pub(in crate::fake) fn close_thread(
        &mut self,
        thread: &ThreadId,
        force: bool,
    ) -> Result<ThreadCloseResult, ErrorObject> {
        let info = self.thread_info(thread).ok_or_else(|| {
            ErrorObject::new(ErrorCode::NotFound, format!("thread {thread} not found"))
        })?;
        if info.status == ThreadStatus::Active {
            let run = info
                .latest_run()
                .map(|r| r.run_id.to_string())
                .unwrap_or_default();
            return Err(conflict(format!(
                "thread {thread} has a run in progress ({run})"
            )));
        }
        self.threads.closed.insert(thread.clone());
        let mut outcome = None;
        let mut reason = None;
        if let Some(workspace) = self.workspace_of(&info.runs[0].run_id)
            && workspace.status != WorkspaceStatus::Removed
        {
            let held = workspace.changes.filter(|c| !c.is_clean());
            match held {
                Some(held) if !force => {
                    outcome = Some(CleanOutcome::Kept);
                    reason = Some(format!(
                        "{} commit(s) not merged and uncommitted changes; use --force to remove it anyway",
                        held.commits
                    ));
                }
                _ => {
                    outcome = Some(CleanOutcome::Removed);
                    self.threads.removed.insert(thread.clone());
                }
            }
        }
        let thread = self.thread_info(thread).ok_or_else(|| {
            ErrorObject::new(ErrorCode::NotFound, "the thread vanished".to_owned())
        })?;
        Ok(ThreadCloseResult {
            thread,
            workspace: outcome,
            reason,
        })
    }

    /// `workspace.diff` with a scope. The thread's whole change is the fixture of its first run;
    /// a follow-up's own run is the last three files over one commit; one commit is the first
    /// file. A removed worktree has nothing.
    pub(in crate::fake) fn diff_for(
        &self,
        run: &RunId,
        path: Option<&str>,
        scope: &reno_proto::DiffScope,
    ) -> Result<Option<reno_proto::WorkspaceDiffResult>, ErrorObject> {
        use reno_proto::DiffScope;
        let Some(record) = self.run(run) else {
            return Ok(None);
        };
        let Some(owner) = self.owner_of(&record.thread) else {
            return Ok(None);
        };
        if let DiffScope::Commit { sha } = scope
            && !((7..=64).contains(&sha.len()) && sha.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(ErrorObject::new(
                ErrorCode::InvalidParams,
                "a commit is named by 7 to 64 hexadecimal characters",
            ));
        }
        let Some(mut whole) = changes::diff(&owner, path) else {
            return Ok(None);
        };
        whole.run_id = run.clone();
        if self.threads.removed.contains(&record.thread) {
            whole.files.clear();
            whole.commits = 0;
            whole.patch = None;
            return Ok(Some(whole));
        }
        let keep: Option<(usize, usize, &str, u32)> = match scope {
            DiffScope::Thread => None,
            DiffScope::Run if record.parent.is_some() => Some((3, 6, changes::FOLLOW_UP_START, 1)),
            DiffScope::Run => None,
            DiffScope::Commit { sha } => Some((0, 1, sha.as_str(), 1)),
        };
        if let Some((from, to, base, commits)) = keep {
            whole.files = whole.files.into_iter().skip(from).take(to - from).collect();
            whole.commits = commits;
            base.clone_into(&mut whole.base_sha);
            if path.is_some_and(|p| !whole.files.iter().any(|f| f.path == p)) {
                whole.patch = None;
            }
        }
        Ok(Some(whole))
    }
}
