//! Threads (RP0.5): the conversations that own a worktree. See `reno_proto::thread` for what a
//! thread is on the wire, and `reno_store::threads` for how it is stored.
//!
//! What the kernel adds:
//!
//! - **Reading.** A thread's status and runs come from the store; the kernel adds the deadline
//!   an idle thread closes at, which only it knows ([`crate::KernelConfig::thread_idle_ttl`]).
//! - **Closing.** A person closes a thread (`thread.close`), or the idle limit does. Either way
//!   the thread is marked closed first, then its worktree is released under `IfMerged` (`Force`
//!   when asked): removed when it holds nothing unmerged, else kept. A release that fails
//!   leaves the worktree where it is for the next start's sweep or an explicit clean.
//! - **The idle limit** is a deadline (ARCH-10): one timer, armed for the thread that has been
//!   idle the longest, re-armed when a thread goes idle or closes, and re-read when the system
//!   wakes (the limit is wall-clock time; the timer is not). Nothing polls.

use std::sync::Arc;
use std::time::Duration;

use reno_proto::{
    CleanOutcome, THREAD_LIST_DEFAULT_LIMIT, THREAD_LIST_MAX_LIMIT, ThreadCloseResult, ThreadId,
    ThreadInfo, ThreadStatus, WorkspaceStatus,
};

use crate::error::{KernelError, Result};
use crate::shared::Shared;
use crate::wake::{WakeSignal, system_woke};
use crate::workspace::{ReleasePolicy, Released};
use crate::workspaces;

const OPEN: [WorkspaceStatus; 3] = [
    WorkspaceStatus::Preparing,
    WorkspaceStatus::Active,
    WorkspaceStatus::Kept,
];

fn ttl_ms(shared: &Shared) -> i64 {
    i64::try_from(shared.config.thread_idle_ttl.as_millis()).unwrap_or(i64::MAX)
}

/// Adds the time an idle thread closes at.
fn with_deadline(shared: &Shared, mut info: ThreadInfo) -> ThreadInfo {
    if info.status == ThreadStatus::Idle {
        let at = i64::try_from(info.updated_at_ms)
            .unwrap_or(i64::MAX)
            .saturating_add(ttl_ms(shared));
        info.idle_expires_at_ms = Some(u64::try_from(at).unwrap_or(0));
    }
    info
}

/// One thread.
pub(crate) async fn get(shared: &Shared, thread: &ThreadId) -> Result<ThreadInfo> {
    shared
        .db
        .get_thread(thread)
        .await?
        .map(|info| with_deadline(shared, info))
        .ok_or_else(|| KernelError::ThreadNotFound(thread.clone()))
}

/// Threads in one of `statuses` (all when empty), most recently changed first, at most `limit`
/// (the protocol's default when `None`, and never above its maximum).
pub(crate) async fn list(
    shared: &Shared,
    statuses: &[ThreadStatus],
    limit: Option<u32>,
) -> Result<Vec<ThreadInfo>> {
    let limit = limit
        .unwrap_or(THREAD_LIST_DEFAULT_LIMIT)
        .clamp(1, THREAD_LIST_MAX_LIMIT);
    Ok(shared
        .db
        .list_threads(statuses, limit)
        .await?
        .into_iter()
        .map(|info| with_deadline(shared, info))
        .collect())
}

/// Closes `thread` and releases its worktree: see the module docs.
pub(crate) async fn close(
    shared: &Shared,
    thread: &ThreadId,
    force: bool,
) -> Result<ThreadCloseResult> {
    let policy = if force {
        ReleasePolicy::Force
    } else {
        ReleasePolicy::IfMerged
    };
    close_with(shared, thread, policy).await
}

/// [`close`] under an explicit release policy.
pub(crate) async fn close_with(
    shared: &Shared,
    thread: &ThreadId,
    policy: ReleasePolicy,
) -> Result<ThreadCloseResult> {
    let before = get(shared, thread).await?;
    if before.status == ThreadStatus::Active
        && let Some(run) = before.latest_run()
    {
        return Err(KernelError::ThreadBusy {
            thread: thread.clone(),
            run: run.run_id.clone(),
        });
    }
    // Intent first (ARCH-9): the thread is closed before the worktree is touched.
    shared
        .db
        .close_thread(thread)
        .await
        .map_err(|error| KernelError::of_thread(error, thread))?;
    shared.thread_idle.notify_one();

    let mut outcome = None;
    let mut reason = None;
    if let Some(port) = shared.workspace.as_ref()
        && let Some(info) = shared.db.get_thread_workspace(thread).await?
        && OPEN.contains(&info.status)
    {
        match workspaces::release(shared, port, &info, policy).await? {
            Released::Removed => outcome = Some(CleanOutcome::Removed),
            Released::Kept(changes) => {
                outcome = Some(CleanOutcome::Kept);
                reason = Some(workspaces::describe(changes, policy));
            }
            Released::GoneBranchKept(changes) => {
                outcome = Some(CleanOutcome::Kept);
                reason = Some(format!(
                    "the directory was gone; branch {} kept with {} commit(s)",
                    info.branch, changes.commits
                ));
            }
        }
    }
    Ok(ThreadCloseResult {
        thread: get(shared, thread).await?,
        workspace: outcome,
        reason,
        closed_terminals: Vec::new(),
    })
}

/// Closes every thread that has been idle past the limit. Whether all of them were handled.
async fn close_due(shared: &Shared) -> bool {
    let cutoff = shared.db.now_ms().saturating_sub(ttl_ms(shared));
    let due = match shared.db.idle_threads_since(cutoff).await {
        Ok(due) => due,
        Err(error) => {
            tracing::warn!(%error, "could not look for idle threads");
            return false;
        }
    };
    let mut all = true;
    for thread in due {
        match close(shared, &thread, false).await {
            Ok(_) => tracing::info!(%thread, "closed a thread that sat idle past the limit"),
            // A run came in between: the thread is not idle any more.
            Err(KernelError::ThreadBusy { .. }) => {}
            Err(error) => {
                tracing::warn!(%thread, %error, "could not close an idle thread");
                all = false;
            }
        }
    }
    all
}

/// How long until the thread that has been idle the longest reaches the limit; `None` when no
/// thread is idle. `Err` when the store cannot say.
async fn next_delay(shared: &Shared) -> Result<Option<Duration>> {
    let Some(since) = shared.db.oldest_idle_since().await? else {
        return Ok(None);
    };
    let left = since
        .saturating_add(ttl_ms(shared))
        .saturating_sub(shared.db.now_ms());
    Ok(Some(Duration::from_millis(
        u64::try_from(left).unwrap_or(0),
    )))
}

/// The idle-limit timer: one deadline, for the thread idle the longest.
pub(crate) async fn reaper(shared: Arc<Shared>) {
    let mut woke = shared.system_wake.as_ref().map(WakeSignal::subscribe);
    loop {
        let handled = close_due(&shared).await;
        let delay = match next_delay(&shared).await {
            _ if !handled => Some(shared.config.store_retry),
            Ok(delay) => delay,
            Err(error) => {
                tracing::warn!(%error, "could not read when the next thread goes idle past the limit");
                Some(shared.config.store_retry)
            }
        };
        tokio::select! {
            biased;
            () = shared.root.cancelled() => return,
            () = shared.thread_idle.notified() => {}
            () = system_woke(&mut woke) => {}
            () = async {
                match delay {
                    Some(delay) => tokio::time::sleep(delay).await,
                    None => std::future::pending().await,
                }
            } => {}
        }
    }
}
