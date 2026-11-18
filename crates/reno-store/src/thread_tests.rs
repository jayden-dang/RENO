//! Behaviour tests for threads: a run starts one, a follow-up joins it, at most one run of a
//! thread is live, the status follows the runs, and the worktree belongs to the thread.
//! `now` is a number the test owns.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use reno_proto::defs::BotPin;
use reno_proto::{
    ExecutorId, RoutingDecision, RuleMatched, RunId, RunStatus, Seq, TaskId, ThreadId,
    ThreadStatus, WorkspaceInfo, WorkspaceStatus,
};
use serde_json::json;
use tempfile::TempDir;

use crate::routing::RoutedRun;
use crate::{NewTask, Store, StoreError};

fn exec() -> ExecutorId {
    "mock".parse().unwrap()
}

async fn open() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("reno.db")).unwrap();
    (dir, store)
}

fn decision() -> RoutingDecision {
    RoutingDecision {
        chosen: Some(exec()),
        ranked: vec![exec()],
        rejected: Vec::new(),
        rule_matched: RuleMatched::Default,
        fallbacks: Vec::new(),
        at_ms: 0,
        warnings: Vec::new(),
    }
}

/// Queues run `n` as a new thread, or as a follow-up in `thread`.
async fn queue(
    store: &Store,
    n: usize,
    thread: Option<&ThreadId>,
    now: i64,
) -> Result<RunId, StoreError> {
    let task = TaskId::from_suffix(&format!("t{n}")).unwrap();
    store
        .create_task(
            NewTask {
                id: task.clone(),
                goal: "g".into(),
                class: None,
                spec: json!({ "cwd": "/work/app" }),
            },
            now,
        )
        .await
        .unwrap();
    let run = RunId::from_suffix(&format!("r{n}")).unwrap();
    store
        .enqueue_routed(
            RoutedRun {
                task,
                run: run.clone(),
                executor: exec(),
                bot: None::<BotPin>,
                decision: decision(),
                thread: thread.cloned(),
            },
            now,
        )
        .await?;
    Ok(run)
}

/// Runs `run` to its end: claimed, started, succeeded.
async fn finish(store: &Store, run: &RunId, now: i64) {
    let claim = store.claim_next("t", now, 60_000).await.unwrap().unwrap();
    assert_eq!(&claim.run_id, run);
    store
        .transition(run, claim.fence, RunStatus::Running, now)
        .await
        .unwrap();
    store
        .transition(run, claim.fence, RunStatus::Evaluating, now)
        .await
        .unwrap();
    store
        .transition(run, claim.fence, RunStatus::Succeeded, now)
        .await
        .unwrap();
    store.thread_went_idle(run, now).await.unwrap();
}

fn workspace(run: &RunId, thread: &ThreadId) -> WorkspaceInfo {
    WorkspaceInfo {
        run_id: run.clone(),
        repo: "/work/app".into(),
        repo_key: "app-0123456789ab".into(),
        path: format!("/w/{thread}").into(),
        subdir: String::new(),
        branch: format!("reno/{thread}"),
        base_sha: "abc123".into(),
        status: WorkspaceStatus::Active,
        changes: None,
        created_at_ms: 0,
        updated_at_ms: 0,
    }
}

#[tokio::test]
async fn a_run_with_no_thread_starts_one_of_its_own() {
    let (_dir, store) = open().await;
    let run = queue(&store, 1, None, 10).await.unwrap();
    let record = store.get_run(&run).await.unwrap().unwrap();
    assert_eq!(record.thread_id.as_str(), "thr_r1");
    assert_eq!(record.parent_run, None);
    assert_eq!(record.start_sha, None);

    let thread = store.get_thread(&record.thread_id).await.unwrap().unwrap();
    assert_eq!(thread.status, ThreadStatus::Active);
    assert_eq!(
        thread.cwd.as_deref().and_then(|p| p.to_str()),
        Some("/work/app")
    );
    assert_eq!(thread.runs.len(), 1);
    assert_eq!(thread.runs[0].run_id, run);
    assert_eq!((thread.created_at_ms, thread.updated_at_ms), (10, 10));
    assert_eq!(thread.workspace, None);
}

#[tokio::test]
async fn a_follow_up_joins_the_thread_after_its_run_ended() {
    let (_dir, store) = open().await;
    let first = queue(&store, 1, None, 10).await.unwrap();
    let thread = store.get_run(&first).await.unwrap().unwrap().thread_id;
    finish(&store, &first, 20).await;
    let idle = store.get_thread(&thread).await.unwrap().unwrap();
    assert_eq!(idle.status, ThreadStatus::Idle);
    assert_eq!(idle.updated_at_ms, 20);

    let second = queue(&store, 2, Some(&thread), 30).await.unwrap();
    let record = store.get_run(&second).await.unwrap().unwrap();
    assert_eq!(record.thread_id, thread);
    assert_eq!(record.parent_run, Some(first.clone()));
    let info = store.get_thread(&thread).await.unwrap().unwrap();
    assert_eq!(info.status, ThreadStatus::Active);
    assert_eq!(
        info.runs
            .iter()
            .map(|r| r.run_id.clone())
            .collect::<Vec<_>>(),
        [first, second]
    );
}

#[tokio::test]
async fn a_thread_takes_one_live_run_at_a_time() {
    let (_dir, store) = open().await;
    let first = queue(&store, 1, None, 10).await.unwrap();
    let thread = store.get_run(&first).await.unwrap().unwrap().thread_id;
    let refused = queue(&store, 2, Some(&thread), 11).await.unwrap_err();
    assert!(
        matches!(&refused, StoreError::ThreadBusy { run, .. } if run == &first),
        "{refused}"
    );
    // The refused run left nothing behind.
    assert!(
        store
            .get_run(&RunId::from_suffix("r2").unwrap())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_thread_that_does_not_exist_is_refused() {
    let (_dir, store) = open().await;
    let nobody = ThreadId::from_suffix("nobody").unwrap();
    let refused = queue(&store, 1, Some(&nobody), 10).await.unwrap_err();
    assert!(matches!(
        refused,
        StoreError::NotFound { kind: "thread", .. }
    ));
}

#[tokio::test]
async fn closing_needs_an_idle_thread_and_is_idempotent() {
    let (_dir, store) = open().await;
    let run = queue(&store, 1, None, 10).await.unwrap();
    let thread = store.get_run(&run).await.unwrap().unwrap().thread_id;
    let busy = store.close_thread(&thread, 15).await.unwrap_err();
    assert!(matches!(busy, StoreError::ThreadBusy { .. }), "{busy}");

    finish(&store, &run, 20).await;
    let closed = store.close_thread(&thread, 30).await.unwrap();
    assert_eq!(closed.status, ThreadStatus::Closed);
    // A second close changes nothing, and writes no second event.
    store.close_thread(&thread, 40).await.unwrap();
    let kinds: Vec<String> = store
        .events_since(Seq(0), 1000)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .filter(|k| k.starts_with("thread."))
        .collect();
    // A thread's birth is on the `run.enqueued` event of its first run.
    assert_eq!(kinds, ["thread.idle", "thread.closed"]);
}

#[tokio::test]
async fn a_closed_thread_with_its_worktree_kept_reopens_with_a_follow_up() {
    let (_dir, store) = open().await;
    let first = queue(&store, 1, None, 10).await.unwrap();
    let thread = store.get_run(&first).await.unwrap().unwrap().thread_id;
    store
        .begin_workspace(workspace(&first, &thread), 11)
        .await
        .unwrap();
    store
        .set_workspace_status(&first, WorkspaceStatus::Active, None, 12)
        .await
        .unwrap();
    finish(&store, &first, 20).await;
    store.close_thread(&thread, 30).await.unwrap();
    store
        .set_workspace_status(&first, WorkspaceStatus::Kept, None, 31)
        .await
        .unwrap();

    let second = queue(&store, 2, Some(&thread), 40).await.unwrap();
    let info = store.get_thread(&thread).await.unwrap().unwrap();
    assert_eq!(info.status, ThreadStatus::Active);
    // The follow-up finds the thread's worktree, made by the first run.
    let seen = store.get_workspace(&second).await.unwrap().unwrap();
    assert_eq!(seen.run_id, first);
    assert_eq!(seen.branch, format!("reno/{thread}"));
    assert_eq!(info.branch.as_deref(), Some(seen.branch.as_str()));
    assert_eq!(info.worktree, Some(seen.path));
    assert_eq!(info.workspace, Some(WorkspaceStatus::Kept));
}

#[tokio::test]
async fn a_thread_whose_worktree_was_removed_takes_no_follow_up() {
    let (_dir, store) = open().await;
    let first = queue(&store, 1, None, 10).await.unwrap();
    let thread = store.get_run(&first).await.unwrap().unwrap().thread_id;
    store
        .begin_workspace(workspace(&first, &thread), 11)
        .await
        .unwrap();
    finish(&store, &first, 20).await;
    store
        .set_workspace_status(&first, WorkspaceStatus::Removed, None, 21)
        .await
        .unwrap();
    let refused = queue(&store, 2, Some(&thread), 30).await.unwrap_err();
    assert!(matches!(refused, StoreError::ThreadClosed(_)), "{refused}");
}

#[tokio::test]
async fn the_idle_limit_runs_from_the_end_of_the_last_run() {
    let (_dir, store) = open().await;
    assert_eq!(store.oldest_idle_since().await.unwrap(), None);
    let a = queue(&store, 1, None, 10).await.unwrap();
    let b = queue(&store, 2, None, 11).await.unwrap();
    // Nothing is idle while its run is queued.
    assert_eq!(store.oldest_idle_since().await.unwrap(), None);
    finish(&store, &a, 100).await;
    finish(&store, &b, 200).await;
    assert_eq!(store.oldest_idle_since().await.unwrap(), Some(100));
    let due = store.idle_threads_since(150).await.unwrap();
    assert_eq!(
        due.iter().map(ThreadId::as_str).collect::<Vec<_>>(),
        ["thr_r1"]
    );
    let thread = store.get_run(&a).await.unwrap().unwrap().thread_id;
    store.close_thread(&thread, 160).await.unwrap();
    assert_eq!(store.oldest_idle_since().await.unwrap(), Some(200));
    assert!(store.idle_threads_since(150).await.unwrap().is_empty());
}

#[tokio::test]
async fn the_start_commit_is_fenced_and_kept_once_set() {
    let (_dir, store) = open().await;
    let run = queue(&store, 1, None, 10).await.unwrap();
    let claim = store.claim_next("t", 11, 60_000).await.unwrap().unwrap();
    let stale = store
        .record_start_sha(&run, claim.fence + 1, "aaa", 12)
        .await
        .unwrap_err();
    assert!(matches!(stale, StoreError::StaleFence { .. }));
    store
        .record_start_sha(&run, claim.fence, "aaa", 12)
        .await
        .unwrap();
    // A resumed attempt does not move where the run began.
    store
        .record_start_sha(&run, claim.fence, "bbb", 13)
        .await
        .unwrap();
    let record = store.get_run(&run).await.unwrap().unwrap();
    assert_eq!(record.start_sha.as_deref(), Some("aaa"));
    let thread = store.get_thread(&record.thread_id).await.unwrap().unwrap();
    assert_eq!(thread.runs[0].start_sha.as_deref(), Some("aaa"));
}

#[tokio::test]
async fn threads_list_newest_change_first_and_filter_by_status() {
    let (_dir, store) = open().await;
    let a = queue(&store, 1, None, 10).await.unwrap();
    let _b = queue(&store, 2, None, 20).await.unwrap();
    finish(&store, &a, 30).await;
    let all = store.list_threads(&[], 100).await.unwrap();
    assert_eq!(
        all.iter().map(|t| t.thread_id.as_str()).collect::<Vec<_>>(),
        ["thr_r1", "thr_r2"]
    );
    let active = store
        .list_threads(&[ThreadStatus::Active], 100)
        .await
        .unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].thread_id.as_str(), "thr_r2");
}

/// Interrupts `run` (claimed, with a session) so that it can be resumed.
async fn interrupt(store: &Store, run: &RunId, now: i64) -> u64 {
    let claim = store.claim_next("t", now, 60_000).await.unwrap().unwrap();
    assert_eq!(&claim.run_id, run);
    store
        .transition(run, claim.fence, RunStatus::Running, now)
        .await
        .unwrap();
    store
        .record_session(run, claim.fence, &reno_proto::SessionRef("s".into()), now)
        .await
        .unwrap();
    store
        .transition(run, claim.fence, RunStatus::Interrupted, now)
        .await
        .unwrap();
    store.thread_went_idle(run, now).await.unwrap();
    claim.fence
}

#[tokio::test]
async fn resuming_a_run_reopens_its_closed_thread() {
    let (_dir, store) = open().await;
    let run = queue(&store, 1, None, 10).await.unwrap();
    let thread = store.get_run(&run).await.unwrap().unwrap().thread_id;
    let fence = interrupt(&store, &run, 20).await;
    store.close_thread(&thread, 30).await.unwrap();
    store.resume_run(&run, fence, None, 40).await.unwrap();
    let info = store.get_thread(&thread).await.unwrap().unwrap();
    assert_eq!(
        info.status,
        ThreadStatus::Active,
        "a live run is never in a closed thread"
    );
}

#[tokio::test]
async fn a_run_cannot_be_resumed_beside_a_newer_run_of_its_thread() {
    let (_dir, store) = open().await;
    let first = queue(&store, 1, None, 10).await.unwrap();
    let thread = store.get_run(&first).await.unwrap().unwrap().thread_id;
    let fence = interrupt(&store, &first, 20).await;
    // The person followed up instead of resuming.
    let second = queue(&store, 2, Some(&thread), 30).await.unwrap();
    let refused = store.resume_run(&first, fence, None, 40).await.unwrap_err();
    assert!(
        matches!(&refused, StoreError::ThreadBusy { run, .. } if run == &second),
        "{refused}"
    );
    // The refused resume changed nothing.
    assert_eq!(
        store.get_run(&first).await.unwrap().unwrap().status,
        RunStatus::Interrupted
    );
}

#[tokio::test]
async fn the_status_filter_and_the_limit_are_applied_in_the_query() {
    let (_dir, store) = open().await;
    let a = queue(&store, 1, None, 10).await.unwrap();
    let b = queue(&store, 2, None, 20).await.unwrap();
    let _live = queue(&store, 3, None, 30).await.unwrap();
    finish(&store, &a, 40).await;
    finish(&store, &b, 50).await;
    let closed = store.get_run(&a).await.unwrap().unwrap().thread_id;
    store.close_thread(&closed, 60).await.unwrap();

    let names = |list: Vec<reno_proto::ThreadInfo>| -> Vec<String> {
        list.into_iter().map(|t| t.thread_id.to_string()).collect()
    };
    assert_eq!(
        names(
            store
                .list_threads(&[ThreadStatus::Active], 10)
                .await
                .unwrap()
        ),
        ["thr_r3"]
    );
    assert_eq!(
        names(store.list_threads(&[ThreadStatus::Idle], 10).await.unwrap()),
        ["thr_r2"]
    );
    assert_eq!(
        names(
            store
                .list_threads(&[ThreadStatus::Closed], 10)
                .await
                .unwrap()
        ),
        ["thr_r1"]
    );
    // Two statuses at once, most recently changed first.
    assert_eq!(
        names(
            store
                .list_threads(&[ThreadStatus::Idle, ThreadStatus::Active], 10)
                .await
                .unwrap()
        ),
        ["thr_r2", "thr_r3"]
    );
    assert_eq!(
        names(store.list_threads(&[], 2).await.unwrap()),
        ["thr_r2", "thr_r1"],
        "the limit keeps the most recently changed"
    );
}
