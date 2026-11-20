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
pub(crate) fn keeps(
    info: &ThreadInfo,
    changes: Option<&reno_proto::WorkspaceChanges>,
) -> Option<String> {
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

/// Why a live thread cannot be closed (the Session tab's line).
pub(crate) const BUSY: &str = "A run is going in this thread. Stop it before closing.";

/// What closing says before it is asked, always: what stays when the worktree holds unmerged
/// work ([`keeps`]), else that nothing is lost. `info` is `None` while the thread is unknown.
pub(crate) fn close_question(
    info: &ThreadInfo,
    changes: Option<&reno_proto::WorkspaceChanges>,
) -> String {
    if info.status == reno_proto::ThreadStatus::Active {
        return BUSY.to_owned();
    }
    if info.worktree.is_none() || worktree_gone(info) {
        return "This thread has no worktree: closing only ends it.".to_owned();
    }
    keeps(info, changes)
        .unwrap_or_else(|| "Closing releases the worktree; nothing in it is unmerged.".to_owned())
}

// ---- the sidebar row menu (ROAD-32) ---------------------------------------------------------

/// What one entry of a thread row's menu does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Item {
    Rename,
    Pin,
    Unpin,
    Archive,
    /// Opens the Copy sub-list.
    Copy,
    CopyPath,
    CopyThreadId,
    CopySession,
    /// Asks, then closes the thread (the real `thread.close`).
    Close,
}

/// One line of the menu.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Entry {
    Item {
        item: Item,
        label: &'static str,
        /// A key that does the same on the thread, drawn right-aligned.
        hint: Option<&'static str>,
        enabled: bool,
        /// Why it is off, said in words beside it.
        note: Option<&'static str>,
        danger: bool,
    },
    Divider,
}

/// What the menu needs to know about the thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Facts {
    /// The thread is known to the daemon (a row with no thread has nothing to act on).
    pub(crate) known: bool,
    /// Renaming, pinning and archiving exist (`Capabilities::thread_edit`).
    pub(crate) edit: bool,
    pub(crate) pinned: bool,
    pub(crate) has_path: bool,
    pub(crate) has_session: bool,
}

fn item(item: Item, label: &'static str) -> Entry {
    Entry::Item {
        item,
        label,
        hint: None,
        enabled: true,
        note: None,
        danger: false,
    }
}

fn hinted(entry: Entry, key: &'static str) -> Entry {
    match entry {
        Entry::Item {
            item,
            label,
            enabled,
            note,
            danger,
            ..
        } => Entry::Item {
            item,
            label,
            hint: Some(key),
            enabled,
            note,
            danger,
        },
        divider => divider,
    }
}

/// The row menu: Rename, Pin or Unpin, Archive, Copy \u{25b8}, a divider, "Close thread\u{2026}".
/// Without `edit` (the real daemon) Rename, Pin and Archive are not there at all.
pub(crate) fn menu(facts: Facts) -> Vec<Entry> {
    if !facts.known {
        return Vec::new();
    }
    let mut out = Vec::new();
    if facts.edit {
        out.push(item(Item::Rename, "Rename"));
        let (pin, label) = if facts.pinned {
            (Item::Unpin, "Unpin")
        } else {
            (Item::Pin, "Pin")
        };
        out.push(hinted(item(pin, label), "\u{2318}\u{21e7}P"));
        out.push(hinted(item(Item::Archive, "Archive"), "\u{2318}\u{21e7}A"));
    }
    out.push(item(Item::Copy, "Copy"));
    out.push(Entry::Divider);
    // It always asks first; a live run is said there.
    out.push(Entry::Item {
        item: Item::Close,
        label: "Close thread\u{2026}",
        hint: None,
        enabled: true,
        note: None,
        danger: true,
    });
    out
}

/// The Copy sub-list: a value the thread does not have yet is off, with the reason.
pub(crate) fn copy_entries(facts: Facts) -> Vec<Entry> {
    let mut path = item(Item::CopyPath, "Path");
    let mut session = item(Item::CopySession, "Harness session ID");
    for (entry, has, why) in [
        (&mut path, facts.has_path, "no worktree"),
        (&mut session, facts.has_session, "not started"),
    ] {
        if let Entry::Item { enabled, note, .. } = entry {
            *enabled = has;
            *note = (!has).then_some(why);
        }
    }
    vec![path, item(Item::CopyThreadId, "Thread ID"), session]
}

/// The count that says what the archive holds, in words.
pub(crate) fn archived_label(count: usize) -> String {
    format!("Archived \u{b7} {count}")
}

/// "Show 6 more", or "Show less" once the folder lists them all.
pub(crate) fn more_label(hidden: usize, expanded: bool) -> Option<String> {
    if expanded {
        Some("Show less".to_owned())
    } else {
        (hidden > 0).then(|| format!("Show {hidden} more"))
    }
}

/// What resting on a row says: the full title, project, branch, harness and status.
pub(crate) fn hover_text(
    row: &super::projects::ThreadRow,
    info: Option<&ThreadInfo>,
    title: &str,
) -> String {
    use super::projects::Dot;
    let status = if info.is_some_and(|i| i.status == reno_proto::ThreadStatus::Closed) {
        "Closed"
    } else {
        match row.dot {
            Dot::Running => "Running",
            Dot::Waiting => row.badge.map_or("Waiting", |b| b.label()),
            Dot::New | Dot::Seen => "Finished",
            Dot::Failed => "Failed",
            Dot::Stopped => "Stopped",
        }
    };
    [
        title.to_owned(),
        row.project.clone(),
        info.and_then(|i| i.branch.clone())
            .unwrap_or_else(|| "no worktree".to_owned()),
        crate::summary::executor_name(row.executor.as_str()),
        status.to_owned(),
    ]
    .join(" \u{b7} ")
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

    #[test]
    fn the_close_question_always_has_something_to_say() {
        let mut info = ThreadInfo {
            thread_id: ThreadId::from_suffix("a").unwrap(),
            status: reno_proto::ThreadStatus::Idle,
            cwd: None,
            project: None,
            worktree: Some("/w/a".into()),
            branch: Some("reno/thr_a".to_owned()),
            base_sha: None,
            workspace: Some(WorkspaceStatus::Kept),
            runs: Vec::new(),
            idle_expires_at_ms: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        let held = WorkspaceChanges {
            commits: 3,
            dirty: true,
        };
        assert!(
            close_question(&info, Some(&held)).starts_with("reno/thr_a with 3 commits"),
            "unmerged work: what stays"
        );
        let clean = WorkspaceChanges {
            commits: 0,
            dirty: false,
        };
        let nothing = "Closing releases the worktree; nothing in it is unmerged.";
        assert_eq!(close_question(&info, Some(&clean)), nothing);
        assert_eq!(close_question(&info, None), nothing, "not read yet");
        info.worktree = None;
        assert!(close_question(&info, Some(&held)).contains("no worktree"));
        info.status = reno_proto::ThreadStatus::Active;
        assert_eq!(
            close_question(&info, Some(&held)),
            BUSY,
            "a live run is said first"
        );
    }
}
