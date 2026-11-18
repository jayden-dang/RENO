//! Workspaces through the daemon (T1.3): a write run in a real temporary git repository works in
//! a worktree under the data directory, the user's checkout stays byte-identical, and
//! `workspace.list`, `workspace.get` and `workspace.clean` work over the socket. Real `git`, no
//! network, no real agent.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{Fixture, id, within};
use reno_proto::{
    CleanOutcome, CleanTarget, ErrorCode, ExecEvent, FEATURES, RunId, RunStatus, TaskRunParams,
    WorkspaceStatus,
};

/// A hermetic `git`: nothing from the user's own configuration leaks in.
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

/// A repository under a path with a space in it, with a stash entry, a staged change, an
/// unstaged change and an untracked file.
fn repo(fx: &Fixture) -> PathBuf {
    let dir = fx
        .dir
        .path()
        .canonicalize()
        .unwrap()
        .join("My Repos")
        .join("proj");
    std::fs::create_dir_all(dir.join("src")).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    std::fs::write(dir.join("src/lib.rs"), "fn a() {}\n").unwrap();
    std::fs::write(dir.join("README.md"), "hello\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "one"]);
    std::fs::write(dir.join("src/lib.rs"), "fn stashed() {}\n").unwrap();
    git(&dir, &["stash", "push", "-q"]);
    std::fs::write(dir.join("src/lib.rs"), "fn staged() {}\n").unwrap();
    git(&dir, &["add", "src/lib.rs"]);
    std::fs::write(dir.join("README.md"), "unstaged\n").unwrap();
    std::fs::write(dir.join("untracked.txt"), "u\n").unwrap();
    dir
}

/// The files Reno promises not to touch, read as bytes (git is not run in the checkout).
fn snapshot(repo: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    for rel in [
        "src/lib.rs",
        "README.md",
        "untracked.txt",
        ".git/index",
        ".git/HEAD",
        ".git/config",
        ".git/refs/heads/main",
        ".git/refs/stash",
        ".git/logs/refs/stash",
    ] {
        out.insert(rel.to_owned(), std::fs::read(repo.join(rel)).unwrap());
    }
    out
}

async fn run_in(fx: &Fixture, executor: &str, cwd: &Path) -> RunId {
    within(fx.client.run_task(TaskRunParams {
        thread_id: None,
        model_options: Default::default(),
        effort: None,
        prompt: "work".into(),
        executor: Some(id(executor)),
        bot: None,
        cwd: Some(cwd.to_path_buf()),
        model: None,
        in_place: false,
        attachments: Vec::new(),
    }))
    .await
    .expect("task.run")
}

async fn until_status(fx: &Fixture, run: &RunId, want: RunStatus) {
    within(async {
        loop {
            let now = fx.status(run).await;
            if now == want {
                return;
            }
            // A run that ended elsewhere will never get there: say where it ended.
            assert!(!now.is_terminal(), "{run} ended {now:?}, not {want:?}");
            tokio::task::yield_now().await;
        }
    })
    .await;
}

/// Waits for the end of a run's journal: the workspace is released before its `Done`.
async fn ended(fx: &Fixture, run: &RunId) -> Vec<ExecEvent> {
    fx.events(run, None)
        .await
        .into_iter()
        .map(|item| item.event)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_in_a_repository_works_in_a_worktree_and_the_checkout_is_untouched() {
    let fx = Fixture::start().await;
    assert!(FEATURES.contains(&"workspaces"));
    let repo = repo(&fx);
    let before = snapshot(&repo);
    let head = git(&repo, &["rev-parse", "HEAD"]);

    // A run that holds, so the worktree can be seen while it works.
    let run = run_in(&fx, "mock-hold", &repo.join("src")).await;
    until_status(&fx, &run, RunStatus::Running).await;
    let info = within(async {
        loop {
            if let Ok(info) = fx.client.get_workspace(run.clone()).await
                && info.status == WorkspaceStatus::Active
            {
                return info;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert_eq!(info.repo, repo);
    assert_eq!(info.subdir, "src");
    assert_eq!(info.branch, format!("reno/{run}"));
    assert_eq!(info.base_sha, head);
    assert!(
        info.path.starts_with(fx.paths.worktrees()),
        "{}",
        info.path.display()
    );
    assert_eq!(
        info.path.parent().unwrap().parent().unwrap(),
        fx.paths.worktrees()
    );
    assert!(info.cwd().join("lib.rs").exists());
    assert_eq!(
        git(&repo, &["rev-parse", "main"]),
        head,
        "the user's branch did not move"
    );

    // The agent edits and commits in its worktree, then is cancelled.
    std::fs::write(info.path.join("agent.txt"), "work\n").unwrap();
    git(&info.path, &["add", "agent.txt"]);
    git(&info.path, &["commit", "-qm", "agent"]);
    within(fx.client.cancel_run(&run, None)).await.unwrap();
    until_status(&fx, &run, RunStatus::Cancelled).await;
    ended(&fx, &run).await;

    let kept = fx.client.get_workspace(run.clone()).await.unwrap();
    assert_eq!(kept.status, WorkspaceStatus::Kept);
    assert_eq!(kept.changes.unwrap().commits, 1);
    assert!(kept.path.join("agent.txt").exists());
    let listed = fx
        .client
        .list_workspaces(Some(vec![WorkspaceStatus::Kept]))
        .await
        .unwrap();
    assert_eq!(listed.len(), 1);

    // Unmerged commits survive every clean without force.
    for target in [
        CleanTarget::Run {
            run_id: run.clone(),
        },
        CleanTarget::Merged,
        CleanTarget::AllClean,
    ] {
        let items = fx.client.clean_workspaces(target, false).await.unwrap();
        assert!(
            items.iter().all(|i| i.outcome == CleanOutcome::Kept),
            "{items:?}"
        );
    }
    assert!(kept.path.exists());
    assert!(!git(&repo, &["branch", "--list", &kept.branch]).is_empty());

    // Forced, it goes.
    let items = fx
        .client
        .clean_workspaces(
            CleanTarget::Run {
                run_id: run.clone(),
            },
            true,
        )
        .await
        .unwrap();
    assert_eq!(items[0].outcome, CleanOutcome::Removed);
    assert!(!kept.path.exists());
    assert_eq!(git(&repo, &["branch", "--list", "reno/*"]), "");
    assert_eq!(
        fx.client.get_workspace(run).await.unwrap().status,
        WorkspaceStatus::Removed
    );

    assert_eq!(
        snapshot(&repo),
        before,
        "the user's checkout is byte-identical"
    );
    assert_eq!(git(&repo, &["stash", "list"]).lines().count(), 1);
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_that_changes_nothing_leaves_no_worktree_and_no_branch() {
    let fx = Fixture::start().await;
    let repo = repo(&fx);
    let run = run_in(&fx, "mock", &repo).await;
    until_status(&fx, &run, RunStatus::Succeeded).await;
    ended(&fx, &run).await;
    let info = fx.client.get_workspace(run).await.unwrap();
    assert_eq!(info.status, WorkspaceStatus::Removed);
    assert!(!info.path.exists());
    assert_eq!(git(&repo, &["branch", "--list", "reno/*"]), "");
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_run_outside_a_repository_is_refused_and_a_run_with_no_cwd_is_not() {
    let fx = Fixture::start().await;
    let plain = fx.dir.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let run = run_in(&fx, "mock", &plain).await;
    until_status(&fx, &run, RunStatus::Failed).await;
    let events = ended(&fx, &run).await;
    let ExecEvent::Done { diagnostic, .. } = events.last().unwrap() else {
        panic!("no Done");
    };
    let why = diagnostic.clone().unwrap();
    assert!(why.starts_with("WorkspaceRefused:"), "{why}");
    assert!(why.contains("not in a git repository"), "{why}");
    assert!(fx.client.get_workspace(run.clone()).await.is_err());

    // Reno's own scratch directory is not the user's: it is worked in place.
    let scratch = fx.run("mock", "hi").await;
    until_status(&fx, &scratch, RunStatus::Succeeded).await;
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_bot_less_task_opts_in_to_in_place_writes_and_a_repository_is_still_never_written_in_place()
 {
    let fx = Fixture::start().await;
    assert!(FEATURES.contains(&"routing"));
    let plain = fx.dir.path().join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    let in_place = |cwd: &Path| TaskRunParams {
        thread_id: None,
        model_options: Default::default(),
        effort: None,
        prompt: "work".into(),
        executor: Some(id("mock")),
        bot: None,
        cwd: Some(cwd.to_path_buf()),
        model: None,
        in_place: true,
        attachments: Vec::new(),
    };

    // Opted in: the run works where it is, with no worktree.
    let run = within(fx.client.run_task(in_place(&plain))).await.unwrap();
    until_status(&fx, &run, RunStatus::Succeeded).await;
    assert!(fx.client.get_workspace(run.clone()).await.is_err());

    // Opting in changes nothing inside a repository: it still gets its own worktree.
    let repo = repo(&fx);
    let in_repo = within(fx.client.run_task(in_place(&repo))).await.unwrap();
    until_status(&fx, &in_repo, RunStatus::Succeeded).await;
    ended(&fx, &in_repo).await;
    let info = within(fx.client.get_workspace(in_repo.clone()))
        .await
        .unwrap();
    assert_eq!(info.repo, repo);
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unknown_workspaces_are_not_found() {
    let fx = Fixture::start().await;
    let missing = RunId::from_suffix("nope").unwrap();
    let err = fx.client.get_workspace(missing.clone()).await.unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::NotFound));
    let err = fx
        .client
        .clean_workspaces(CleanTarget::Run { run_id: missing }, false)
        .await
        .unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::NotFound));
    assert!(fx.client.list_workspaces(None).await.unwrap().is_empty());
    fx.stop().await;
}

/// A bare RPC connection to the daemon: `workspace.diff` is called without the typed client.
async fn raw(fx: &Fixture) -> reno_rpc::Client {
    let rpc = reno_rpc::connect_unix(&fx.paths.socket, tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    rpc.call::<reno_proto::DaemonHello>(reno_proto::HelloParams {
        client_name: "workspace-diff-test".into(),
        protocol_version: reno_proto::PROTOCOL_VERSION,
        features: FEATURES.iter().map(|f| (*f).to_owned()).collect(),
    })
    .await
    .unwrap();
    rpc
}

fn remote_code(error: reno_rpc::RpcError) -> ErrorCode {
    match error {
        reno_rpc::RpcError::Remote(object) => object.code,
        other => panic!("not a remote error: {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn workspace_diff_lists_what_a_run_changed_and_patches_one_file() {
    use reno_proto::{ChangeKind, WorkspaceDiff, WorkspaceDiffParams};
    let fx = Fixture::start().await;
    assert!(FEATURES.contains(&"run.changes"));
    let rpc = raw(&fx).await;
    let repo = repo(&fx);
    let before = snapshot(&repo);

    let run = run_in(&fx, "mock-hold", &repo).await;
    until_status(&fx, &run, RunStatus::Running).await;
    let info = within(async {
        loop {
            if let Ok(info) = fx.client.get_workspace(run.clone()).await
                && info.status == WorkspaceStatus::Active
            {
                return info;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;

    // A clean worktree has nothing yet.
    let diff = |path: Option<&str>| {
        rpc.call::<WorkspaceDiff>(WorkspaceDiffParams {
            scope: None,
            run_id: run.clone(),
            path: path.map(str::to_owned),
        })
    };
    let none = diff(None).await.unwrap();
    assert!(none.files.is_empty());
    assert_eq!(none.base_sha, info.base_sha);
    assert_eq!(none.branch, info.branch);

    // The agent commits one file, edits another and adds an untracked one with a space.
    std::fs::write(info.path.join("agent.txt"), "work\nmore\n").unwrap();
    git(&info.path, &["add", "agent.txt"]);
    git(&info.path, &["commit", "-qm", "agent"]);
    std::fs::write(info.path.join("README.md"), "changed\n").unwrap();
    std::fs::write(info.path.join("new file.txt"), "a\nb\nc\n").unwrap();

    let list = diff(None).await.unwrap();
    assert_eq!(list.commits, 1);
    let found: Vec<(&str, ChangeKind, u32)> = list
        .files
        .iter()
        .map(|f| (f.path.as_str(), f.kind, f.additions))
        .collect();
    assert_eq!(
        found,
        [
            ("README.md", ChangeKind::Modified, 1),
            ("agent.txt", ChangeKind::Added, 2),
            ("new file.txt", ChangeKind::Added, 3),
        ]
    );
    assert_eq!(list.patch, None);

    let one = diff(Some("new file.txt")).await.unwrap();
    assert!(one.patch.unwrap().contains("+a\n+b\n+c\n"));
    let committed = diff(Some("agent.txt")).await.unwrap();
    assert!(committed.patch.unwrap().contains("+work"));

    // A path out of the worktree, and an unknown run, are refused as such.
    for bad in ["../README.md", "/etc/passwd"] {
        let err = diff(Some(bad)).await.unwrap_err();
        assert_eq!(remote_code(err), ErrorCode::InvalidParams, "{bad}");
    }
    let err = rpc
        .call::<WorkspaceDiff>(WorkspaceDiffParams {
            scope: None,
            run_id: RunId::from_suffix("nope").unwrap(),
            path: None,
        })
        .await
        .unwrap_err();
    assert_eq!(remote_code(err), ErrorCode::NotFound);

    // Once the run ends and the workspace is forced away: empty, not an error.
    within(fx.client.cancel_run(&run, None)).await.unwrap();
    until_status(&fx, &run, RunStatus::Cancelled).await;
    ended(&fx, &run).await;
    fx.client
        .clean_workspaces(
            CleanTarget::Run {
                run_id: run.clone(),
            },
            true,
        )
        .await
        .unwrap();
    let gone = diff(Some("agent.txt")).await.unwrap();
    assert!(gone.files.is_empty() && gone.patch.is_none());
    assert_eq!(gone.branch, info.branch);

    assert_eq!(snapshot(&repo), before, "the user's checkout is untouched");
    fx.stop().await;
}
