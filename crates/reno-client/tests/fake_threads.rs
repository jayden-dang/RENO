//! Threads against the fake daemon: `thread.list`, `thread.get`, `thread.close`, a follow-up
//! that joins a thread, the diff scopes, and the mirror's view of it all. No network, no wall
//! clock: tokio time is paused.

#![cfg(feature = "fake")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use futures::StreamExt;
use reno_client::fake::FakeDaemon;
use reno_client::{Client, Feed, FeedState, Mirror, MirrorConfig, State, Updates};
use reno_proto::{
    ChangeKind, CleanOutcome, DiffScope, ErrorCode, RunId, RunStatus, TaskRunParams, ThreadId,
    ThreadStatus, WorkspaceStatus,
};

fn task(prompt: &str) -> TaskRunParams {
    TaskRunParams {
        prompt: prompt.to_owned(),
        executor: Some("claude-code".parse().unwrap()),
        ..TaskRunParams::default()
    }
}

fn run(name: &str) -> RunId {
    RunId::from_suffix(name).unwrap()
}

fn thread(name: &str) -> ThreadId {
    ThreadId::from_suffix(name).unwrap()
}

/// Lets the scripted runs play out.
async fn settle() {
    tokio::time::sleep(Duration::from_secs(30)).await;
}

async fn drain(rx: &mut Updates, state: &mut State) {
    while let Ok(Some(update)) = tokio::time::timeout(Duration::from_secs(1), rx.next()).await {
        state.apply(update);
    }
}

#[tokio::test(start_paused = true)]
async fn the_fake_daemon_has_threads_and_every_seeded_run_is_its_own() {
    let client = FakeDaemon::spawn().await.unwrap();
    assert!(client.supports(reno_proto::FEATURE_THREADS));

    let threads = client.list_threads(None).await.unwrap();
    assert_eq!(threads.len(), 6, "one thread per seeded run");
    for t in &threads {
        assert_eq!(t.runs.len(), 1);
        assert_eq!(t.runs[0].parent_run, None);
    }
    let builder = client.get_thread(thread("builder1")).await.unwrap();
    assert_eq!(builder.runs[0].run_id, run("builder1"));
    let workspace = client.get_workspace(run("builder1")).await.unwrap();
    assert_eq!(builder.branch.as_deref(), Some(workspace.branch.as_str()));
    assert_eq!(builder.worktree.as_deref(), Some(workspace.path.as_path()));
    assert_eq!(
        builder.base_sha.as_deref(),
        Some(workspace.base_sha.as_str())
    );

    // A run that never had a worktree has a thread without one.
    let reviewer = client.get_thread(thread("reviewer1")).await.unwrap();
    assert_eq!(
        (reviewer.worktree, reviewer.branch, reviewer.workspace),
        (None, None, None)
    );

    let missing = client.get_thread(thread("nope")).await.unwrap_err();
    assert_eq!(missing.code(), Some(ErrorCode::NotFound));
    let only_idle = client
        .list_threads(Some(vec![ThreadStatus::Idle]))
        .await
        .unwrap();
    assert!(only_idle.iter().all(|t| t.status == ThreadStatus::Idle));
}

#[tokio::test(start_paused = true)]
async fn a_follow_up_joins_the_thread_of_its_run_and_only_when_that_run_is_not_live() {
    let client = FakeDaemon::spawn().await.unwrap();
    let first = client
        .submit_task(task("/long start a feature"))
        .await
        .unwrap();
    let thread_id = first
        .thread_id
        .clone()
        .expect("the daemon names the thread");
    assert_eq!(
        client.get_thread(thread_id.clone()).await.unwrap().status,
        ThreadStatus::Active
    );

    // The run is live: a follow-up would be a second live run of the thread.
    let mut follow = task("and then the tests");
    follow.thread_id = Some(thread_id.clone());
    let busy = client.submit_task(follow.clone()).await.unwrap_err();
    assert_eq!(busy.code(), Some(ErrorCode::Conflict), "{busy}");
    let busy_close = client
        .close_thread(thread_id.clone(), false)
        .await
        .unwrap_err();
    assert_eq!(busy_close.code(), Some(ErrorCode::Conflict), "{busy_close}");

    // Sending to a live run steers it; the run then finishes.
    client
        .steer(&first.run_id, "carry on".into())
        .await
        .unwrap();
    settle().await;
    assert_eq!(
        client.get_run(&first.run_id).await.unwrap().status,
        RunStatus::Succeeded
    );

    let second = client.submit_task(follow).await.unwrap();
    assert_eq!(second.thread_id.as_ref(), Some(&thread_id));
    let info = client.get_thread(thread_id.clone()).await.unwrap();
    assert_eq!(info.runs.len(), 2);
    assert_eq!(info.runs[0].run_id, first.run_id);
    assert_eq!(info.runs[1].run_id, second.run_id);
    assert_eq!(info.runs[1].parent_run.as_ref(), Some(&first.run_id));
    assert_ne!(info.runs[0].start_sha, info.runs[1].start_sha);

    let nowhere = {
        let mut p = task("x");
        p.thread_id = Some(thread("nowhere"));
        client.submit_task(p).await.unwrap_err()
    };
    assert_eq!(nowhere.code(), Some(ErrorCode::NotFound));
}

#[tokio::test(start_paused = true)]
async fn closing_a_thread_removes_or_keeps_its_worktree_and_a_removed_one_takes_no_follow_up() {
    let client = FakeDaemon::spawn().await.unwrap();
    // A finished run's worktree holds three commits: kept, until forced.
    let made = client.submit_task(task("work")).await.unwrap();
    settle().await;
    let id = made.thread_id.unwrap();
    let closed = client.close_thread(id.clone(), false).await.unwrap();
    assert_eq!(closed.thread.status, ThreadStatus::Closed);
    assert_eq!(closed.workspace, Some(CleanOutcome::Kept));
    assert!(closed.reason.unwrap().contains("--force"));
    assert_eq!(
        client
            .get_workspace(made.run_id.clone())
            .await
            .unwrap()
            .status,
        WorkspaceStatus::Kept
    );
    // Kept work can still be continued, which opens the thread again.
    let mut follow = task("more");
    follow.thread_id = Some(id.clone());
    let again = client.submit_task(follow.clone()).await.unwrap();
    assert_eq!(
        client.get_thread(id.clone()).await.unwrap().status,
        ThreadStatus::Active
    );
    settle().await;
    assert_eq!(
        client.get_run(&again.run_id).await.unwrap().status,
        RunStatus::Succeeded
    );

    let forced = client.close_thread(id.clone(), true).await.unwrap();
    assert_eq!(forced.workspace, Some(CleanOutcome::Removed));
    assert_eq!(
        client.get_workspace(made.run_id).await.unwrap().status,
        WorkspaceStatus::Removed
    );
    let refused = client.submit_task(follow).await.unwrap_err();
    assert_eq!(refused.code(), Some(ErrorCode::Conflict));

    // A thread with no worktree closes with nothing to release.
    let reviewer = client
        .close_thread(thread("reviewer1"), false)
        .await
        .unwrap();
    assert_eq!((reviewer.workspace, reviewer.reason), (None, None));
}

#[tokio::test(start_paused = true)]
async fn the_diff_scopes_are_additive_and_the_default_is_the_whole_thread() {
    let client = FakeDaemon::spawn().await.unwrap();
    let first = client.submit_task(task("first")).await.unwrap();
    settle().await;
    let mut p = task("second");
    p.thread_id = first.thread_id.clone();
    let second = client.submit_task(p).await.unwrap();
    settle().await;

    // No scope is the thread against its base, exactly what the method did before.
    let before = client
        .workspace_diff(second.run_id.clone(), None)
        .await
        .unwrap();
    let whole = client
        .workspace_diff_scoped(second.run_id.clone(), None, DiffScope::Thread)
        .await
        .unwrap();
    assert_eq!(before, whole);
    assert_eq!(whole.run_id, second.run_id);
    assert_eq!(whole.files.len(), 6);

    // This run alone: fewer files, from where it started.
    let this_run = client
        .workspace_diff_scoped(second.run_id.clone(), None, DiffScope::Run)
        .await
        .unwrap();
    assert!(this_run.files.len() < whole.files.len());
    assert_ne!(this_run.base_sha, whole.base_sha);
    // The first run of a thread starts at the thread's base: its scope is the whole.
    let first_run = client
        .workspace_diff_scoped(first.run_id.clone(), None, DiffScope::Run)
        .await
        .unwrap();
    assert_eq!(first_run.files, whole.files);

    let commit = client
        .workspace_diff_scoped(
            second.run_id.clone(),
            None,
            DiffScope::Commit {
                sha: "abcdef12".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!((commit.commits, commit.files.len()), (1, 1));
    assert_eq!(commit.files[0].kind, ChangeKind::Modified);
    let refused = client
        .workspace_diff_scoped(
            second.run_id.clone(),
            None,
            DiffScope::Commit { sha: "HEAD".into() },
        )
        .await
        .unwrap_err();
    assert_eq!(refused.code(), Some(ErrorCode::InvalidParams));
}

async fn mirrored(client: Client) -> (Mirror, Updates, State) {
    let (mirror, mut rx) = Mirror::start(client, MirrorConfig::default());
    let mut state = State::new();
    drain(&mut rx, &mut state).await;
    (mirror, rx, state)
}

#[tokio::test(start_paused = true)]
async fn the_mirror_holds_the_threads_and_says_which_thread_a_run_is_in() {
    let client = FakeDaemon::spawn().await.unwrap();
    let (mirror, mut rx, mut state) = mirrored(client.clone()).await;
    assert_eq!(state.feed(Feed::Threads), FeedState::Live);
    assert_eq!(state.threads().len(), 6);
    assert_eq!(state.thread_of(&run("builder1")), Some(&thread("builder1")));
    assert_eq!(
        state.thread(&thread("builder1")).unwrap().runs[0].run_id,
        run("builder1")
    );
    assert_eq!(state.thread_runs(&thread("builder1")).len(), 1);

    // A new run, and its follow-up, arrive through the runs feed; the mirror refetches threads.
    let first = client.submit_task(task("one")).await.unwrap();
    settle().await;
    let mut p = task("two");
    p.thread_id = first.thread_id.clone();
    let second = client.submit_task(p).await.unwrap();
    settle().await;
    drain(&mut rx, &mut state).await;
    let t = first.thread_id.unwrap();
    assert_eq!(state.thread_of(&first.run_id), Some(&t));
    assert_eq!(state.thread_of(&second.run_id), Some(&t));
    assert_eq!(state.thread(&t).unwrap().runs.len(), 2);
    let runs: Vec<_> = state
        .thread_runs(&t)
        .iter()
        .map(|r| r.run_id.clone())
        .collect();
    assert_eq!(runs, [first.run_id, second.run_id]);

    // Closing is a call of the person's; the mirror learns it on request.
    client.close_thread(t.clone(), true).await.unwrap();
    mirror.refresh_threads();
    drain(&mut rx, &mut state).await;
    assert_eq!(state.thread(&t).unwrap().status, ThreadStatus::Closed);
    mirror.shutdown().await;
}
