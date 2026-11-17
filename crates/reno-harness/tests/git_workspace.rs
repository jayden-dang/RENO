//! Behaviour of the git worktree manager against real temporary repositories. No network, no
//! agent, no repository of the user's: every repository is made here, under a path with a space
//! in it (the real data directory is `Application Support`).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use reno_harness::proc::git_workspace::{
    Changes, GitError, GitWorktrees, Location, Release, Released,
};
use tempfile::TempDir;

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
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

struct World {
    _tmp: TempDir,
    /// `<tmp>/Application Support`, resolved (macOS `/var` is `/private/var`).
    base: PathBuf,
    mgr: GitWorktrees,
}

impl World {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp
            .path()
            .canonicalize()
            .unwrap()
            .join("Application Support");
        std::fs::create_dir_all(&base).unwrap();
        let mgr = GitWorktrees::new(base.join("worktrees"));
        Self {
            _tmp: tmp,
            base,
            mgr,
        }
    }

    /// A repository with two commits on `main`, a `src/lib.rs` and a `docs/` directory.
    fn repo(&self, name: &str) -> PathBuf {
        let dir = self.base.join("repos").join(name);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join("docs")).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("src/lib.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.join("docs/a.md"), "a\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "one"]);
        std::fs::write(dir.join("README.md"), "hello\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "two"]);
        dir
    }

    async fn prepared(&self, run: &str, repo: &Path) -> Location {
        let loc = self.mgr.plan(run, repo).await.unwrap().expect("a repo");
        self.mgr.create(&loc).await.unwrap();
        loc
    }
}

fn commit_in(loc: &Location, file: &str) {
    std::fs::write(loc.path.join(file), "x\n").unwrap();
    git(&loc.path, &["add", file]);
    git(&loc.path, &["commit", "-qm", "agent work"]);
}

#[tokio::test]
async fn a_worktree_is_cut_from_the_head_sha_and_the_users_branch_does_not_move() {
    let w = World::new();
    let repo = w.repo("proj");
    let head = git(&repo, &["rev-parse", "HEAD"]);
    let loc = w.prepared("run_a", &repo).await;
    assert_eq!(loc.base_sha, head);
    assert_eq!(loc.branch, "reno/run_a");
    assert_eq!(loc.repo, repo);
    assert_eq!(
        loc.path,
        w.base.join("worktrees").join(&loc.repo_key).join("run_a")
    );
    assert_eq!(git(&loc.path, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&loc.path, &["branch", "--show-current"]), "reno/run_a");
    assert_eq!(git(&repo, &["branch", "--show-current"]), "main");
    assert_eq!(git(&repo, &["rev-parse", "main"]), head);
    assert!(loc.path.join("README.md").exists());
}

#[tokio::test]
async fn a_task_in_a_subdirectory_keeps_its_relative_directory() {
    let w = World::new();
    let repo = w.repo("proj");
    let loc = w
        .mgr
        .plan("run_a", &repo.join("src"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(loc.subdir, "src");
    w.mgr.create(&loc).await.unwrap();
    assert_eq!(loc.cwd(), loc.path.join("src"));
    assert!(loc.cwd().join("lib.rs").exists());
}

#[tokio::test]
async fn an_untracked_subdirectory_is_made_empty_in_the_worktree() {
    let w = World::new();
    let repo = w.repo("proj");
    std::fs::create_dir_all(repo.join("scratch/deep")).unwrap();
    let loc = w
        .mgr
        .plan("run_a", &repo.join("scratch/deep"))
        .await
        .unwrap()
        .unwrap();
    w.mgr.create(&loc).await.unwrap();
    assert!(loc.cwd().is_dir());
}

#[tokio::test]
async fn refusals_are_typed() {
    let w = World::new();
    // Not a repository.
    let plain = w.base.join("plain");
    std::fs::create_dir_all(&plain).unwrap();
    assert!(w.mgr.plan("run_a", &plain).await.unwrap().is_none());
    // A repository with no commits.
    let unborn = w.base.join("unborn");
    std::fs::create_dir_all(&unborn).unwrap();
    git(&unborn, &["init", "-q", "-b", "main"]);
    assert!(matches!(
        w.mgr.plan("run_a", &unborn).await,
        Err(GitError::NoCommits { .. })
    ));
    // A bare repository.
    let bare = w.base.join("bare.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare"]);
    assert!(matches!(
        w.mgr.plan("run_a", &bare).await,
        Err(GitError::Bare { .. })
    ));
    // A run id that is not a safe ref name.
    let repo = w.repo("proj");
    for bad in ["", "a b", "../x", "a;b", "run/1"] {
        assert!(
            matches!(w.mgr.plan(bad, &repo).await, Err(GitError::Unsafe(_))),
            "{bad:?}"
        );
    }
}

#[tokio::test]
async fn create_is_idempotent_and_refuses_a_path_that_is_not_its_worktree() {
    let w = World::new();
    let repo = w.repo("proj");
    let loc = w.prepared("run_a", &repo).await;
    std::fs::write(loc.path.join("keep.txt"), "x").unwrap();
    w.mgr.create(&loc).await.unwrap();
    assert!(loc.path.join("keep.txt").exists(), "reused, not recreated");

    let other = w.mgr.plan("run_b", &repo).await.unwrap().unwrap();
    std::fs::create_dir_all(&other.path).unwrap();
    assert!(matches!(
        w.mgr.create(&other).await,
        Err(GitError::PathTaken { .. })
    ));
}

#[tokio::test]
async fn two_runs_on_one_repository_get_separate_worktrees_at_once() {
    let w = World::new();
    let repo = w.repo("proj");
    let (a, b, c) = (
        w.mgr.plan("run_a", &repo).await.unwrap().unwrap(),
        w.mgr.plan("run_b", &repo).await.unwrap().unwrap(),
        w.mgr.plan("run_c", &repo).await.unwrap().unwrap(),
    );
    let (ra, rb, rc) = tokio::join!(w.mgr.create(&a), w.mgr.create(&b), w.mgr.create(&c));
    ra.unwrap();
    rb.unwrap();
    rc.unwrap();
    for loc in [&a, &b, &c] {
        assert!(loc.path.join("README.md").exists());
    }
    assert_ne!(a.path, b.path);
    let list = git(&repo, &["worktree", "list", "--porcelain"]);
    assert_eq!(list.matches("worktree ").count(), 4);
}

#[tokio::test]
async fn the_repo_key_is_stable_across_subdirectories_and_distinct_across_repositories() {
    let w = World::new();
    let one = w.repo("proj");
    let other_parent = w.base.join("elsewhere");
    std::fs::create_dir_all(&other_parent).unwrap();
    let two = other_parent.join("proj");
    std::fs::create_dir_all(&two).unwrap();
    git(&two, &["init", "-q", "-b", "main"]);
    std::fs::write(two.join("f"), "x").unwrap();
    git(&two, &["add", "."]);
    git(&two, &["commit", "-qm", "c"]);

    let at_root = w.mgr.plan("run_a", &one).await.unwrap().unwrap();
    let at_sub = w
        .mgr
        .plan("run_a", &one.join("src"))
        .await
        .unwrap()
        .unwrap();
    let same_name = w.mgr.plan("run_a", &two).await.unwrap().unwrap();
    assert_eq!(at_root.repo_key, at_sub.repo_key);
    assert_ne!(at_root.repo_key, same_name.repo_key);
    assert!(at_root.repo_key.starts_with("proj-"));
    // A linked worktree of the repository has the repository's key.
    let linked = w.prepared("run_l", &one).await;
    let from_linked = w.mgr.plan("run_m", &linked.path).await.unwrap().unwrap();
    assert_eq!(from_linked.repo_key, at_root.repo_key);
}

#[tokio::test]
async fn a_clean_worktree_is_removed_with_its_branch() {
    let w = World::new();
    let repo = w.repo("proj");
    let loc = w.prepared("run_a", &repo).await;
    let seen = w.mgr.inspect(&loc).await.unwrap();
    assert!(seen.exists && seen.branch_exists && seen.changes.is_clean());
    assert_eq!(
        w.mgr.release(&loc, Release::IfClean).await.unwrap(),
        Released::Removed
    );
    assert!(!loc.path.exists());
    assert!(
        !loc.path.parent().unwrap().exists(),
        "no empty key directory is left"
    );
    assert_eq!(git(&repo, &["branch", "--list", "reno/*"]), "");
    assert_eq!(
        git(&repo, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
}

#[tokio::test]
async fn commits_and_uncommitted_changes_keep_the_worktree_and_the_branch() {
    let w = World::new();
    let repo = w.repo("proj");

    let committed = w.prepared("run_a", &repo).await;
    commit_in(&committed, "new.txt");
    let released = w.mgr.release(&committed, Release::IfClean).await.unwrap();
    assert_eq!(
        released,
        Released::Kept(Changes {
            commits: 1,
            dirty: false
        })
    );
    assert!(committed.path.join("new.txt").exists());
    assert!(!git(&repo, &["branch", "--list", "reno/run_a"]).is_empty());

    let untracked = w.prepared("run_b", &repo).await;
    std::fs::write(untracked.path.join("scratch.txt"), "x").unwrap();
    assert_eq!(
        w.mgr.release(&untracked, Release::IfClean).await.unwrap(),
        Released::Kept(Changes {
            commits: 0,
            dirty: true
        })
    );

    let edited = w.prepared("run_c", &repo).await;
    std::fs::write(edited.path.join("README.md"), "changed\n").unwrap();
    assert_eq!(
        w.mgr.release(&edited, Release::IfClean).await.unwrap(),
        Released::Kept(Changes {
            commits: 0,
            dirty: true
        })
    );
}

#[tokio::test]
async fn merged_work_is_removed_only_when_merged_and_not_dirty() {
    let w = World::new();
    let repo = w.repo("proj");
    let loc = w.prepared("run_a", &repo).await;
    commit_in(&loc, "new.txt");
    // Not merged yet.
    assert!(matches!(
        w.mgr.release(&loc, Release::IfMerged).await.unwrap(),
        Released::Kept(_)
    ));
    // The user merges it (in a copy of their branch, as a person would).
    git(&repo, &["merge", "-q", "--ff-only", "reno/run_a"]);
    // Merged, but the worktree is dirty: kept.
    std::fs::write(loc.path.join("late.txt"), "x").unwrap();
    assert!(matches!(
        w.mgr.release(&loc, Release::IfMerged).await.unwrap(),
        Released::Kept(Changes { dirty: true, .. })
    ));
    std::fs::remove_file(loc.path.join("late.txt")).unwrap();
    assert_eq!(
        w.mgr.release(&loc, Release::IfMerged).await.unwrap(),
        Released::Removed
    );
    assert!(!loc.path.exists());
    assert_eq!(git(&repo, &["branch", "--list", "reno/*"]), "");
}

#[tokio::test]
async fn only_force_removes_unmerged_work() {
    let w = World::new();
    let repo = w.repo("proj");
    let loc = w.prepared("run_a", &repo).await;
    commit_in(&loc, "new.txt");
    std::fs::write(loc.path.join("dirty.txt"), "x").unwrap();
    assert!(matches!(
        w.mgr.release(&loc, Release::IfClean).await.unwrap(),
        Released::Kept(_)
    ));
    assert!(matches!(
        w.mgr.release(&loc, Release::IfMerged).await.unwrap(),
        Released::Kept(_)
    ));
    assert_eq!(
        w.mgr.release(&loc, Release::Force).await.unwrap(),
        Released::Removed
    );
    assert!(!loc.path.exists());
    assert_eq!(git(&repo, &["branch", "--list", "reno/*"]), "");
}

#[tokio::test]
async fn a_vanished_directory_is_pruned_and_the_branch_kept_only_if_it_has_commits() {
    let w = World::new();
    let repo = w.repo("proj");

    let empty = w.prepared("run_a", &repo).await;
    std::fs::remove_dir_all(&empty.path).unwrap();
    assert_eq!(
        w.mgr.release(&empty, Release::IfClean).await.unwrap(),
        Released::Removed
    );
    assert_eq!(git(&repo, &["branch", "--list", "reno/run_a"]), "");

    let worked = w.prepared("run_b", &repo).await;
    commit_in(&worked, "new.txt");
    std::fs::remove_dir_all(&worked.path).unwrap();
    assert_eq!(
        w.mgr.release(&worked, Release::IfClean).await.unwrap(),
        Released::GoneBranchKept(Changes {
            commits: 1,
            dirty: false
        })
    );
    assert!(!git(&repo, &["branch", "--list", "reno/run_b"]).is_empty());
    assert_eq!(
        git(&repo, &["worktree", "list", "--porcelain"])
            .matches("worktree ")
            .count(),
        1
    );
    // The kept branch lets the run's worktree be made again.
    w.mgr.create(&worked).await.unwrap();
    assert!(worked.path.join("new.txt").exists());
}

#[tokio::test]
async fn unknown_directories_are_reported_and_never_removed() {
    let w = World::new();
    let repo = w.repo("proj");
    let loc = w.prepared("run_a", &repo).await;
    let stray = w
        .base
        .join("worktrees")
        .join("proj-000000000000")
        .join("run_z");
    std::fs::create_dir_all(&stray).unwrap();
    let known: HashSet<PathBuf> = [loc.path.clone()].into();
    assert_eq!(w.mgr.unknown_dirs(&known).await, vec![stray.clone()]);
    assert!(stray.exists());
}

#[tokio::test]
async fn a_git_command_that_overruns_is_ended() {
    let w = World::new();
    let repo = w.repo("proj");
    let slow = GitWorktrees::new(w.base.join("worktrees")).with_timeout(Duration::from_millis(1));
    let err = slow.plan("run_a", &repo).await.unwrap_err();
    assert!(matches!(err, GitError::Timeout { .. }), "{err:?}");
}

/// Every byte of the user's checkout that Reno promises not to touch, read from the files: the
/// working tree, the index, `HEAD`, the config, the branch's ref, the stash ref and its reflog.
/// Git is not run in the checkout to take it (that is what is being checked).
fn snapshot(repo: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if rel == ".git" {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.insert(rel, std::fs::read(&path).unwrap());
            }
        }
    }
    walk(repo, repo, &mut out);
    for file in [
        ".git/index",
        ".git/HEAD",
        ".git/config",
        ".git/refs/heads/main",
        ".git/refs/stash",
        ".git/logs/refs/stash",
        ".git/logs/HEAD",
        ".git/logs/refs/heads/main",
        ".git/packed-refs",
        ".git/info/exclude",
        ".git/description",
    ] {
        let value = std::fs::read(repo.join(file)).ok().unwrap_or_default();
        out.insert(format!("<{file}>"), value);
    }
    out
}

#[tokio::test]
async fn the_users_checkout_is_byte_identical_after_a_whole_lifecycle() {
    let w = World::new();
    let repo = w.repo("proj");
    // A stash entry, a staged change, an unstaged change and an untracked file.
    std::fs::write(repo.join("src/lib.rs"), "fn stashed() {}\n").unwrap();
    git(&repo, &["stash", "push", "-q", "-m", "wip"]);
    std::fs::write(repo.join("docs/a.md"), "staged\n").unwrap();
    git(&repo, &["add", "docs/a.md"]);
    std::fs::write(repo.join("README.md"), "unstaged\n").unwrap();
    std::fs::write(repo.join("untracked.txt"), "u\n").unwrap();
    assert!(repo.join(".git/refs/stash").exists(), "the stash is real");

    let before = snapshot(&repo);
    assert!(before["<.git/index>"].len() > 10);
    assert!(!before["<.git/refs/stash>"].is_empty());

    // Kept: commits.
    let kept = w.prepared("run_kept", &repo).await;
    commit_in(&kept, "work.txt");
    assert!(matches!(
        w.mgr.release(&kept, Release::IfClean).await.unwrap(),
        Released::Kept(_)
    ));
    // Clean: removed.
    let clean = w.prepared("run_clean", &repo).await;
    assert_eq!(
        w.mgr.release(&clean, Release::IfClean).await.unwrap(),
        Released::Removed
    );
    // Vanished directory, pruned.
    let gone = w.prepared("run_gone", &repo).await;
    std::fs::remove_dir_all(&gone.path).unwrap();
    w.mgr.prune(&repo).await.unwrap();
    // Subdirectory task, dirty, then forced out.
    let sub = w
        .mgr
        .plan("run_sub", &repo.join("src"))
        .await
        .unwrap()
        .unwrap();
    w.mgr.create(&sub).await.unwrap();
    std::fs::write(sub.cwd().join("x.rs"), "x").unwrap();
    w.mgr.release(&sub, Release::Force).await.unwrap();
    // Merged and then removed.
    let merged = w.prepared("run_merged", &repo).await;
    w.mgr.release(&merged, Release::IfMerged).await.unwrap();
    // Forced out.
    w.mgr.release(&kept, Release::Force).await.unwrap();

    assert_eq!(snapshot(&repo), before);
    // The stash and the user's changes are still there for git too.
    assert_eq!(git(&repo, &["stash", "list"]).lines().count(), 1);
    assert_eq!(git(&repo, &["branch", "--show-current"]), "main");
}

// ---- project_root -------------------------------------------------------------------------
//
// Project-scoped standing rules match on this path, so every spelling of a directory of a
// repository has to give the same one.

mod project_root {
    use std::path::{Path, PathBuf};

    use reno_harness::proc::git_workspace::project_root;

    /// A directory with a `.git` directory in it, canonical (macOS `/var` is `/private/var`).
    fn repo() -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::create_dir(root.join(".git")).unwrap();
        (dir, root)
    }

    #[tokio::test]
    async fn the_root_and_every_directory_below_it_are_the_same_project() {
        let (_dir, root) = repo();
        let deep = root.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();
        assert_eq!(project_root(&root).await, Some(root.clone()));
        assert_eq!(project_root(&deep).await, Some(root));
    }

    #[tokio::test]
    async fn a_git_file_marks_a_root_too_and_the_nearest_one_wins() {
        let (_dir, outer) = repo();
        let inner = outer.join("vendor").join("lib");
        std::fs::create_dir_all(&inner).unwrap();
        std::fs::write(inner.join(".git"), "gitdir: ../../.git/modules/lib\n").unwrap();
        assert_eq!(project_root(&inner).await, Some(inner.clone()));
        assert_eq!(project_root(inner.parent().unwrap()).await, Some(outer));
    }

    #[tokio::test]
    async fn every_spelling_of_a_directory_is_one_project() {
        let (_dir, root) = repo();
        std::fs::create_dir(root.join("src")).unwrap();
        let links = tempfile::tempdir().unwrap();
        let link = links.path().join("porch");
        std::os::unix::fs::symlink(&root, &link).unwrap();
        assert_eq!(project_root(&link).await, Some(root.clone()));
        assert_eq!(project_root(&link.join("src")).await, Some(root.clone()));
        // `..` and a trailing slash too.
        let dotted = root.join("src").join("..").join("src");
        assert_eq!(project_root(&dotted).await, Some(root));
    }

    #[tokio::test]
    async fn a_missing_directory_has_no_project() {
        assert_eq!(
            project_root(Path::new("/definitely/not/here/at/all")).await,
            None
        );
    }

    #[tokio::test]
    async fn the_walk_stops_at_the_nearest_root_and_never_reports_the_directory_without_a_dot_git()
    {
        // The temp dir has no `.git` of its own, so whatever is found (a repository the machine
        // keeps above `$TMPDIR`, or nothing) is a strict ancestor of it, never it.
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let found = project_root(&root).await;
        assert!(found.is_none_or(|p| root.starts_with(&p) && p != root));
    }
}
