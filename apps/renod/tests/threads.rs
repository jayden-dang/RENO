//! Threads through the daemon (RP0.5): over the socket, against a real temporary git repository
//! and the mock executor. A follow-up (`task.run` with `threadId`) works in the thread's worktree
//! and so sees what the run before it committed; one run of a thread is live at a time; closing
//! releases the worktree under the keep-or-remove rule; the codes are the stable ones. No
//! network, no real agent.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{Fixture, id, within};
use reno_proto::{
    CleanOutcome, DiffScope, ErrorCode, FEATURES, RunId, RunStatus, TaskRunParams, ThreadId,
    ThreadStatus, WorkspaceStatus,
};

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .expect("git runs");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn repo(fx: &Fixture) -> PathBuf {
    let dir = fx.dir.path().canonicalize().unwrap().join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "one"]);
    dir
}

fn task(executor: &str, cwd: &Path, thread: Option<ThreadId>) -> TaskRunParams {
    TaskRunParams {
        prompt: "work".into(),
        executor: Some(id(executor)),
        cwd: Some(cwd.to_path_buf()),
        thread_id: thread,
        ..TaskRunParams::default()
    }
}

async fn until_status(fx: &Fixture, run: &RunId, want: RunStatus) {
    within(async {
        loop {
            let now = fx.status(run).await;
            if now == want {
                return;
            }
            assert!(!now.is_terminal(), "{run} ended {now:?}, not {want:?}");
            tokio::task::yield_now().await;
        }
    })
    .await;
    // The run's end, and what the end does, are over once its journal is.
    fx.events(run, None).await;
}

/// Waits for the thread to be in `want` (its idle clock starts just after the run's end).
async fn until_thread(fx: &Fixture, thread: &ThreadId, want: ThreadStatus) {
    within(async {
        loop {
            if fx.client.get_thread(thread.clone()).await.unwrap().status == want {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
}

fn code<T: std::fmt::Debug>(result: Result<T, reno_client::ClientError>) -> ErrorCode {
    result.unwrap_err().code().expect("a remote error")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_follow_up_works_in_the_threads_worktree_and_sees_what_the_first_run_committed() {
    let fx = Fixture::start().await;
    assert!(FEATURES.contains(&"threads"));
    assert!(fx.client.supports(reno_proto::FEATURE_THREADS));
    let repo = repo(&fx);
    let base = git(&repo, &["rev-parse", "HEAD"]);

    // Run one holds, so the worktree can be seen and committed in while it works.
    let first = within(fx.client.submit_task(task("mock-hold", &repo, None)))
        .await
        .unwrap();
    let thread = first
        .thread_id
        .clone()
        .expect("the daemon names the thread");
    until_status_running(&fx, &first.run_id).await;
    let info = fx.client.get_thread(thread.clone()).await.unwrap();
    assert_eq!(info.status, ThreadStatus::Active);
    assert_eq!(info.project.as_deref(), Some(repo.as_path()));
    assert_eq!(info.base_sha.as_deref(), Some(base.as_str()));
    let worktree = info.worktree.clone().unwrap();
    assert_eq!(info.branch, Some(format!("reno/{thread}")));
    std::fs::write(worktree.join("agent.txt"), "one\n").unwrap();
    git(&worktree, &["add", "agent.txt"]);
    git(&worktree, &["commit", "-qm", "run one"]);
    let after_one = git(&worktree, &["rev-parse", "HEAD"]);

    // A run of the thread is live: another is a conflict, and the thread cannot be closed.
    let busy = within(
        fx.client
            .submit_task(task("mock", &repo, Some(thread.clone()))),
    )
    .await;
    assert_eq!(code(busy), ErrorCode::Conflict);
    assert_eq!(
        code(fx.client.close_thread(thread.clone(), false).await),
        ErrorCode::Conflict
    );
    within(fx.client.cancel_run(&first.run_id, None))
        .await
        .unwrap();
    until_status(&fx, &first.run_id, RunStatus::Cancelled).await;
    until_thread(&fx, &thread, ThreadStatus::Idle).await;

    // Run two, a follow-up. It names another folder; the thread's own is used.
    let elsewhere = fx.dir.path().join("elsewhere");
    std::fs::create_dir_all(&elsewhere).unwrap();
    let second = within(
        fx.client
            .submit_task(task("mock", &elsewhere, Some(thread.clone()))),
    )
    .await
    .unwrap();
    assert_eq!(second.thread_id.as_ref(), Some(&thread));
    until_status(&fx, &second.run_id, RunStatus::Succeeded).await;
    until_thread(&fx, &thread, ThreadStatus::Idle).await;

    let shown = fx
        .client
        .get_workspace(second.run_id.clone())
        .await
        .unwrap();
    assert_eq!(shown.path, worktree, "the same worktree");
    assert_eq!(shown.status, WorkspaceStatus::Active);
    assert_eq!(
        std::fs::read_to_string(shown.path.join("agent.txt")).unwrap(),
        "one\n",
        "run two sees run one's commit"
    );
    let info = fx.client.get_thread(thread.clone()).await.unwrap();
    assert_eq!(info.runs.len(), 2);
    assert_eq!(info.runs[0].start_sha.as_deref(), Some(base.as_str()));
    assert_eq!(info.runs[1].start_sha.as_deref(), Some(after_one.as_str()));
    assert_eq!(info.runs[1].parent_run.as_ref(), Some(&first.run_id));
    assert!(info.idle_expires_at_ms.is_some());

    // Diff scopes: the default is the thread, a run is its own, a commit is one.
    let paths = |d: reno_proto::WorkspaceDiffResult| -> Vec<String> {
        d.files.into_iter().map(|f| f.path).collect()
    };
    let whole = fx.client.workspace_diff(second.run_id.clone(), None).await;
    assert_eq!(paths(whole.unwrap()), ["agent.txt"]);
    let this_run = fx
        .client
        .workspace_diff_scoped(second.run_id.clone(), None, DiffScope::Run)
        .await
        .unwrap();
    assert!(this_run.files.is_empty());
    assert_eq!(this_run.base_sha, after_one);
    let commit = fx
        .client
        .workspace_diff_scoped(
            second.run_id.clone(),
            Some("agent.txt".into()),
            DiffScope::Commit {
                sha: after_one.clone(),
            },
        )
        .await
        .unwrap();
    assert_eq!(paths(commit.clone()), ["agent.txt"]);
    assert!(commit.patch.unwrap().contains("+one"));
    for sha in ["HEAD", "zz", &base] {
        let refused = fx
            .client
            .workspace_diff_scoped(
                second.run_id.clone(),
                None,
                DiffScope::Commit { sha: sha.into() },
            )
            .await;
        assert_eq!(code(refused), ErrorCode::InvalidParams, "{sha}");
    }

    // Closing keeps what holds unmerged commits; forced, it goes. Nothing of the person's moved.
    let kept = fx.client.close_thread(thread.clone(), false).await.unwrap();
    assert_eq!(kept.workspace, Some(CleanOutcome::Kept));
    assert_eq!(kept.thread.status, ThreadStatus::Closed);
    assert!(worktree.exists());
    let gone = fx.client.close_thread(thread.clone(), true).await.unwrap();
    assert_eq!(gone.workspace, Some(CleanOutcome::Removed));
    assert!(!worktree.exists());
    assert_eq!(git(&repo, &["branch", "--list", "reno/*"]), "");
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), base);
    assert_eq!(
        code(
            fx.client
                .submit_task(task("mock", &repo, Some(thread.clone())))
                .await
        ),
        ErrorCode::Conflict,
        "a removed worktree takes no follow-up"
    );
    fx.stop().await;
}

async fn until_status_running(fx: &Fixture, run: &RunId) {
    within(async {
        while fx.status(run).await != RunStatus::Running {
            tokio::task::yield_now().await;
        }
    })
    .await;
    // The worktree is recorded, then made, then marked active: only then can it be written to.
    within(async {
        loop {
            if let Ok(info) = fx.client.get_workspace(run.clone()).await
                && info.status == WorkspaceStatus::Active
            {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn threads_are_listed_filtered_and_unknown_ones_are_not_found() {
    let fx = Fixture::start().await;
    let repo = repo(&fx);
    let a = within(fx.client.submit_task(task("mock", &repo, None)))
        .await
        .unwrap();
    until_status(&fx, &a.run_id, RunStatus::Succeeded).await;
    let b = within(fx.client.submit_task(task("mock", &repo, None)))
        .await
        .unwrap();
    until_status(&fx, &b.run_id, RunStatus::Succeeded).await;
    let (ta, tb) = (a.thread_id.unwrap(), b.thread_id.unwrap());
    assert_ne!(ta, tb, "two tasks are two threads");
    until_thread(&fx, &ta, ThreadStatus::Idle).await;
    until_thread(&fx, &tb, ThreadStatus::Idle).await;
    fx.client.close_thread(ta.clone(), false).await.unwrap();

    let all = fx.client.list_threads(None).await.unwrap();
    assert_eq!(all.len(), 2);
    let closed = fx
        .client
        .list_threads(Some(vec![ThreadStatus::Closed]))
        .await
        .unwrap();
    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].thread_id, ta);

    let nobody = ThreadId::from_suffix("nobody").unwrap();
    assert_eq!(
        code(fx.client.get_thread(nobody.clone()).await),
        ErrorCode::NotFound
    );
    assert_eq!(
        code(fx.client.close_thread(nobody.clone(), false).await),
        ErrorCode::NotFound
    );
    assert_eq!(
        code(
            fx.client
                .submit_task(task("mock", &repo, Some(nobody)))
                .await
        ),
        ErrorCode::NotFound
    );
    fx.stop().await;
}
