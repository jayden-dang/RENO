//! What the app says about a thread: whether a message continues it, why the daemon refused one,
//! and what closing it kept. Pure functions from the mirror, so each state is a unit test.

use reno_client::State;
use reno_proto::{CleanOutcome, RunId, ThreadCloseResult, ThreadId, ThreadInfo, WorkspaceStatus};

/// Why the daemon would not let a message join a thread, from its words (the wire has one code,
/// `Conflict`, for both).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// A run of the thread is live: a message to it is a steer.
    Busy,
    /// The thread is closed and its worktree is gone: there is nothing to continue.
    Gone,
}

pub(crate) fn refusal(why: &str) -> Option<Refusal> {
    if why.contains("has a run in progress") {
        Some(Refusal::Busy)
    } else if why.contains("worktree is gone") || why.contains("worktree was removed") {
        Some(Refusal::Gone)
    } else {
        None
    }
}

impl Refusal {
    /// The note under the composer.
    pub(crate) fn note(self) -> &'static str {
        match self {
            Self::Busy => {
                "A run of this thread is already going, so a message now steers it. Send it again."
            }
            Self::Gone => {
                "This thread was closed and its worktree removed. Send again to start a new thread."
            }
        }
    }
}

/// Whether the thread of `run` can take a follow-up that continues it: the daemon has threads,
/// has named this one, and it still has its worktree (or never had one). `Closed` with a kept
/// worktree continues: the daemon reopens it.
pub(crate) fn continues(state: &State, run: &RunId) -> Option<ThreadId> {
    if !state.supports(reno_proto::FEATURE_THREADS) {
        return None;
    }
    let thread = state.thread_of(run)?;
    let info = state.thread(thread)?;
    (!worktree_gone(info)).then(|| thread.clone())
}

/// A closed thread whose worktree was removed.
pub(crate) fn worktree_gone(info: &ThreadInfo) -> bool {
    info.workspace == Some(WorkspaceStatus::Removed)
}

/// What closing a thread left, in words: "Closed. Branch reno/x with 2 commits not merged is
/// kept in <path>.", or that the worktree was removed.
pub(crate) fn closed_line(result: &ThreadCloseResult) -> String {
    match result.workspace {
        Some(CleanOutcome::Kept) => {
            let branch = result.thread.branch.as_deref().unwrap_or("its branch");
            let why = result
                .reason
                .as_deref()
                .map_or(String::new(), |r| format!(" ({r})"));
            format!("Closed. The worktree and {branch} are kept{why}.")
        }
        Some(CleanOutcome::Removed) => "Closed. The worktree was removed.".to_owned(),
        _ => "Closed.".to_owned(),
    }
}

/// What closing would keep, said before it is done: the branch and its unmerged commits.
/// `None` when the worktree holds nothing to lose (or there is none).
pub(crate) fn keeps(info: &ThreadInfo, changes: Option<&reno_proto::WorkspaceChanges>) -> Option<String> {
    let held = changes.filter(|c| !c.is_clean())?;
    let branch = info.branch.as_deref().unwrap_or("the branch");
    let commits = match held.commits {
        0 => String::new(),
        1 => " with 1 commit not merged".to_owned(),
        n => format!(" with {n} commits not merged"),
    };
    let dirty = if held.dirty {
        if commits.is_empty() {
            " with uncommitted changes".to_owned()
        } else {
            " and uncommitted changes".to_owned()
        }
    } else {
        String::new()
    };
    Some(format!(
        "{branch}{commits}{dirty} stays in the worktree: closing removes nothing."
    ))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use reno_proto::WorkspaceChanges;

    use super::*;

    #[test]
    fn the_daemons_refusals_are_told_apart() {
        assert_eq!(
            refusal("thread thr_a has a run in progress (run_b)"),
            Some(Refusal::Busy)
        );
        assert_eq!(
            refusal("thread thr_a is closed and its worktree is gone; start a new one"),
            Some(Refusal::Gone)
        );
        assert_eq!(refusal("thread thr_a not found"), None);
        assert!(Refusal::Busy.note().contains("steers"));
        assert!(Refusal::Gone.note().contains("new thread"));
    }

    #[test]
    fn closing_says_what_it_keeps() {
        let info = ThreadInfo {
            thread_id: ThreadId::from_suffix("a").unwrap(),
            status: reno_proto::ThreadStatus::Idle,
            cwd: None,
            project: None,
            worktree: None,
            branch: Some("reno/thr_a".to_owned()),
            base_sha: None,
            workspace: Some(WorkspaceStatus::Kept),
            runs: Vec::new(),
            idle_expires_at_ms: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let held = |commits, dirty| WorkspaceChanges { commits, dirty };
        assert_eq!(keeps(&info, Some(&held(0, false))), None);
        assert_eq!(keeps(&info, None), None);
        assert_eq!(
            keeps(&info, Some(&held(2, false))).unwrap(),
            "reno/thr_a with 2 commits not merged stays in the worktree: closing removes nothing."
        );
        assert!(
            keeps(&info, Some(&held(1, true)))
                .unwrap()
                .contains("1 commit not merged and uncommitted changes")
        );
    }
}
