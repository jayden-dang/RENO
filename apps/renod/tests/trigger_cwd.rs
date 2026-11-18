//! A trigger that names a working directory (`target.cwd`), through the daemon: its run works in
//! a worktree of the registered repository and the user's checkout stays byte-identical; a
//! directory that is gone refuses the run before spawn and backs the trigger off. Real `git`, the
//! mock executor (`RENO_NO_HARNESSES`), no network, no real agent.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use common::{Fixture, within};
use futures::StreamExt;
use reno_proto::{
    ExecEvent, FiringOutcome, RunId, RunStatus, StartRefusal, StopDetail, TriggerInfo,
    TriggerStatus, WorkspaceStatus,
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

/// A repository with a staged change, an unstaged change and an untracked file.
fn repo(fx: &Fixture) -> PathBuf {
    let dir = fx
        .dir
        .path()
        .canonicalize()
        .unwrap()
        .join("checkouts")
        .join("app");
    fs::create_dir_all(dir.join("src")).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    fs::write(dir.join("src/lib.rs"), "fn a() {}\n").unwrap();
    fs::write(dir.join("README.md"), "hello\n").unwrap();
    git(&dir, &["add", "."]);
    git(&dir, &["commit", "-qm", "one"]);
    fs::write(dir.join("src/lib.rs"), "fn staged() {}\n").unwrap();
    git(&dir, &["add", "src/lib.rs"]);
    fs::write(dir.join("README.md"), "unstaged\n").unwrap();
    fs::write(dir.join("untracked.txt"), "u\n").unwrap();
    dir
}

/// The files Reno promises not to touch, read as bytes.
fn snapshot(repo: &Path) -> BTreeMap<String, Vec<u8>> {
    [
        "src/lib.rs",
        "README.md",
        "untracked.txt",
        ".git/index",
        ".git/HEAD",
        ".git/refs/heads/main",
    ]
    .into_iter()
    .map(|rel| (rel.to_owned(), fs::read(repo.join(rel)).unwrap()))
    .collect()
}

fn write(defs: &Path, rel: &str, text: &str) {
    let path = defs.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn define(fx: &Fixture, cwd: &Path) {
    let defs = &fx.paths.defs_dir;
    write(
        defs,
        "bots/keeper.toml",
        "name = \"keeper\"\ninstructions = \"x\"\nexecutors = [\"mock\"]\n",
    );
    write(
        defs,
        "triggers/nightly.toml",
        &format!(
            "name = \"nightly\"\n[when]\nkind = \"manual\"\n[target]\nbot = \"keeper\"\nprompt = \"go\"\ncwd = \"{}\"\n",
            cwd.display()
        ),
    );
}

async fn trigger_until(fx: &Fixture, ready: impl Fn(&TriggerInfo) -> bool) -> TriggerInfo {
    let mut watch = fx.client.watch_triggers().await.unwrap();
    within(async {
        loop {
            let snapshot = watch.next().await.expect("the watch ended").unwrap();
            if let Some(found) = snapshot
                .triggers
                .into_iter()
                .find(|t| t.name == "nightly" && ready(t))
            {
                return found;
            }
        }
    })
    .await
}

async fn journal(fx: &Fixture, run: &RunId) -> Vec<ExecEvent> {
    fx.events(run, None)
        .await
        .into_iter()
        .map(|item| item.event)
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_trigger_with_a_cwd_works_in_a_worktree_and_the_checkout_is_untouched() {
    let fx = Fixture::start().await;
    let repo = repo(&fx);
    let before = snapshot(&repo);
    let head = git(&repo, &["rev-parse", "HEAD"]);
    define(&fx, &repo);

    let listed = trigger_until(&fx, |t| t.cwd.is_some()).await;
    assert_eq!(listed.cwd.as_deref(), repo.to_str());

    let fired = fx.client.fire_trigger("nightly").await.unwrap();
    assert_eq!(fired.outcome, FiringOutcome::Fired);
    let run = fired.run_id.expect("a run was started");
    journal(&fx, &run).await;
    assert_eq!(fx.status(&run).await, RunStatus::Succeeded);

    // The run had a worktree of the registered repository, under the data directory.
    let info = fx.client.get_workspace(run.clone()).await.unwrap();
    assert_eq!(info.repo, repo);
    assert_eq!(info.base_sha, head);
    // The worktree and branch are the thread's, and a run a trigger started closes it at its end.
    let thread = fx.client.list_threads(None).await.unwrap()[0]
        .thread_id
        .clone();
    assert_eq!(info.branch, format!("reno/{thread}"));
    assert!(
        info.path.starts_with(fx.paths.worktrees()),
        "{:?}",
        info.path
    );
    assert_ne!(info.path, repo);
    assert_eq!(info.status, WorkspaceStatus::Removed);

    // Nothing in the user's checkout moved, and no branch was left behind.
    assert_eq!(snapshot(&repo), before);
    assert_eq!(git(&repo, &["rev-parse", "main"]), head);
    assert_eq!(git(&repo, &["branch", "--list", "reno/*"]), "");
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_trigger_whose_directory_is_gone_is_refused_before_spawn_and_backs_off() {
    let fx = Fixture::start().await;
    let gone = fx.dir.path().join("unmounted").join("app");
    define(&fx, &gone);
    trigger_until(&fx, |t| t.cwd.is_some()).await;

    let fired = fx.client.fire_trigger("nightly").await.unwrap();
    let run = fired.run_id.expect("a run was created");
    let events = journal(&fx, &run).await;
    assert_eq!(fx.status(&run).await, RunStatus::Failed);
    match events.last().unwrap() {
        ExecEvent::Done {
            reason: Some(StopDetail::StartRefused { failure, detail }),
            ..
        } => {
            assert_eq!(*failure, StartRefusal::WorkspaceRefused);
            assert!(detail.contains("does not exist"), "{detail}");
            assert!(detail.contains("unmounted"), "{detail}");
        }
        other => panic!("not a typed refusal: {other:?}"),
    }
    assert!(!gone.exists(), "nothing created the missing directory");

    let backed = trigger_until(&fx, |t| t.status == TriggerStatus::BackedOff).await;
    assert_eq!(backed.failures, 1);
    fx.stop().await;
}
