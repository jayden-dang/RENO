//! Finding and undoing writes to protected paths in a run's worktree, against real temporary
//! repositories. No network, no agent, no repository of the user's.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use reno_harness::proc::git_workspace::{GitError, GitWorktrees, Location};
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

    /// A repository whose base commit tracks `.claude/settings.json` and `.mcp.json`, and
    /// ignores `.claude/settings.local.json`.
    fn repo(&self) -> PathBuf {
        let dir = self.base.join("repos").join("guarded");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("src/lib.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.join(".claude/settings.json"), "{\"base\":true}\n").unwrap();
        std::fs::write(dir.join(".mcp.json"), "{}\n").unwrap();
        std::fs::write(dir.join(".gitignore"), ".claude/settings.local.json\n").unwrap();
        git(&dir, &["add", "-A"]);
        git(&dir, &["commit", "-qm", "one"]);
        dir
    }

    async fn prepared(&self, run: &str) -> Location {
        let loc = self
            .mgr
            .plan(run, &self.repo())
            .await
            .unwrap()
            .expect("a repo");
        self.mgr.create(&loc).await.unwrap();
        loc
    }
}

#[tokio::test]
async fn a_worktree_that_changed_nothing_protected_reports_nothing() {
    let world = World::new();
    let loc = world.prepared("r1").await;
    std::fs::write(loc.path.join("src/lib.rs"), "fn b() {}\n").unwrap();
    std::fs::write(loc.path.join("new.txt"), "n\n").unwrap();
    assert!(world.mgr.protected_changes(&loc).await.unwrap().is_empty());
}

#[tokio::test]
async fn every_way_of_writing_a_protected_path_is_found_and_put_back() {
    let world = World::new();
    let loc = world.prepared("r2").await;
    let wt = &loc.path;
    // Modified (tracked), deleted (tracked), created untracked, created and ignored, created
    // deep in a new directory, and an ordinary change that must stay.
    std::fs::write(wt.join(".claude/settings.json"), "{\"evil\":true}\n").unwrap();
    std::fs::remove_file(wt.join(".mcp.json")).unwrap();
    std::fs::create_dir_all(wt.join("pkg/.codex")).unwrap();
    std::fs::write(wt.join("pkg/.codex/config.toml"), "x\n").unwrap();
    std::fs::write(wt.join(".claude/settings.local.json"), "{}\n").unwrap();
    std::fs::create_dir_all(wt.join(".reno")).unwrap();
    std::fs::write(wt.join(".reno/policy.toml"), "default = \"allow\"\n").unwrap();
    std::fs::write(wt.join("src/lib.rs"), "fn changed() {}\n").unwrap();

    let found = world.mgr.protected_changes(&loc).await.unwrap();
    assert_eq!(
        found,
        [
            ".claude/settings.json",
            ".claude/settings.local.json",
            ".mcp.json",
            ".reno/policy.toml",
            "pkg/.codex/config.toml",
        ]
    );
    world.mgr.revert_protected(&loc, &found).await.unwrap();

    assert_eq!(
        std::fs::read_to_string(wt.join(".claude/settings.json")).unwrap(),
        "{\"base\":true}\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join(".mcp.json")).unwrap(),
        "{}\n"
    );
    assert!(!wt.join(".claude/settings.local.json").exists());
    assert!(!wt.join(".reno/policy.toml").exists());
    assert!(!wt.join("pkg/.codex/config.toml").exists());
    // The ordinary change is untouched, and nothing protected is left.
    assert_eq!(
        std::fs::read_to_string(wt.join("src/lib.rs")).unwrap(),
        "fn changed() {}\n"
    );
    assert!(world.mgr.protected_changes(&loc).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_committed_write_to_a_protected_path_is_undone_in_a_commit_of_its_own() {
    let world = World::new();
    let loc = world.prepared("r3").await;
    let wt = &loc.path;
    std::fs::write(wt.join(".claude/settings.json"), "{\"evil\":true}\n").unwrap();
    std::fs::create_dir_all(wt.join(".codex")).unwrap();
    std::fs::write(wt.join(".codex/config.toml"), "x\n").unwrap();
    std::fs::write(wt.join("keep.txt"), "k\n").unwrap();
    git(wt, &["add", "-A"]);
    git(wt, &["commit", "-qm", "agent"]);
    // Something staged that is not protected: the revert must not commit it.
    std::fs::write(wt.join("staged.txt"), "s\n").unwrap();
    git(wt, &["add", "staged.txt"]);

    let found = world.mgr.protected_changes(&loc).await.unwrap();
    assert_eq!(found, [".claude/settings.json", ".codex/config.toml"]);
    world.mgr.revert_protected(&loc, &found).await.unwrap();

    assert!(world.mgr.protected_changes(&loc).await.unwrap().is_empty());
    assert_eq!(
        std::fs::read_to_string(wt.join(".claude/settings.json")).unwrap(),
        "{\"base\":true}\n"
    );
    assert!(!wt.join(".codex").exists());
    // Against the base commit, the branch now differs only by the ordinary file.
    assert_eq!(
        git(wt, &["diff", "--name-only", &loc.base_sha, "HEAD"]),
        "keep.txt"
    );
    assert_eq!(git(wt, &["diff", "--cached", "--name-only"]), "staged.txt");
}

#[tokio::test]
async fn a_revert_refuses_a_path_that_is_not_protected_or_leaves_the_worktree() {
    let world = World::new();
    let loc = world.prepared("r4").await;
    for bad in [
        "src/lib.rs",
        "../.claude/x",
        "/etc/.claude",
        ".claude/../src/lib.rs",
    ] {
        assert!(
            matches!(
                world.mgr.revert_protected(&loc, &[bad.to_owned()]).await,
                Err(GitError::Unsafe(_))
            ),
            "{bad}"
        );
    }
    assert!(loc.path.join("src/lib.rs").exists());
}
