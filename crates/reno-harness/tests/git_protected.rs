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

    /// A repository whose base commit tracks `.reno/policy.toml` and `.reno/defs/bot.toml`
    /// (protected), and `.claude/settings.json` and `.mcp.json` (harness config, not protected),
    /// and ignores `.reno/local.toml` and `.claude/settings.local.json`.
    fn repo(&self) -> PathBuf {
        let dir = self.base.join("repos").join("guarded");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::create_dir_all(dir.join(".claude")).unwrap();
        std::fs::create_dir_all(dir.join(".reno/defs")).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        std::fs::write(dir.join("src/lib.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.join(".claude/settings.json"), "{\"base\":true}\n").unwrap();
        std::fs::write(dir.join(".mcp.json"), "{}\n").unwrap();
        std::fs::write(dir.join(".reno/policy.toml"), "default = \"ask\"\n").unwrap();
        std::fs::write(dir.join(".reno/defs/bot.toml"), "name = \"b\"\n").unwrap();
        std::fs::write(
            dir.join(".gitignore"),
            ".claude/settings.local.json\n.reno/local.toml\n",
        )
        .unwrap();
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
    std::fs::write(wt.join(".reno/policy.toml"), "default = \"allow\"\n").unwrap();
    std::fs::remove_file(wt.join(".reno/defs/bot.toml")).unwrap();
    std::fs::create_dir_all(wt.join("pkg/.reno")).unwrap();
    std::fs::write(wt.join("pkg/.reno/x.toml"), "x\n").unwrap();
    std::fs::write(wt.join(".reno/local.toml"), "{}\n").unwrap();
    std::fs::write(wt.join("src/lib.rs"), "fn changed() {}\n").unwrap();

    let found = world.mgr.protected_changes(&loc).await.unwrap();
    assert_eq!(
        found,
        [
            ".reno/defs/bot.toml",
            ".reno/local.toml",
            ".reno/policy.toml",
            "pkg/.reno/x.toml",
        ]
    );
    world.mgr.revert_protected(&loc, &found).await.unwrap();

    assert_eq!(
        std::fs::read_to_string(wt.join(".reno/policy.toml")).unwrap(),
        "default = \"ask\"\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join(".reno/defs/bot.toml")).unwrap(),
        "name = \"b\"\n"
    );
    assert!(!wt.join(".reno/local.toml").exists());
    assert!(!wt.join("pkg/.reno/x.toml").exists());
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
    std::fs::write(wt.join(".reno/policy.toml"), "default = \"allow\"\n").unwrap();
    std::fs::create_dir_all(wt.join("pkg/.reno")).unwrap();
    std::fs::write(wt.join("pkg/.reno/c.toml"), "x\n").unwrap();
    std::fs::write(wt.join("keep.txt"), "k\n").unwrap();
    git(wt, &["add", "-A"]);
    git(wt, &["commit", "-qm", "agent"]);
    // Something staged that is not protected: the revert must not commit it.
    std::fs::write(wt.join("staged.txt"), "s\n").unwrap();
    git(wt, &["add", "staged.txt"]);

    let found = world.mgr.protected_changes(&loc).await.unwrap();
    assert_eq!(found, [".reno/policy.toml", "pkg/.reno/c.toml"]);
    world.mgr.revert_protected(&loc, &found).await.unwrap();

    assert!(world.mgr.protected_changes(&loc).await.unwrap().is_empty());
    assert_eq!(
        std::fs::read_to_string(wt.join(".reno/policy.toml")).unwrap(),
        "default = \"ask\"\n"
    );
    assert!(!wt.join("pkg/.reno/c.toml").exists());
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
        "../.reno/x",
        "/etc/.reno",
        ".reno/../src/lib.rs",
        // Harness config is not protected, so it is not Reno's to revert either.
        ".claude/settings.json",
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

/// A follow-up run is judged on its own writes: the guard's base is where the run started, so a
/// protected path an earlier run of the thread left changed is not charged to it, and a new
/// write is found alone, and put back to the start, not to the thread's base.
#[tokio::test]
async fn a_run_is_judged_against_where_it_started_not_the_threads_base() {
    let world = World::new();
    let loc = world.prepared("thr_f1").await;
    let wt = &loc.path;
    // An earlier run of the thread committed a changed protected file (a person allowed it).
    std::fs::write(wt.join(".reno/policy.toml"), "default = \"deny\"\n").unwrap();
    git(wt, &["commit", "-qam", "earlier run"]);
    let start = world.mgr.head(&loc).await.unwrap();
    assert_eq!(
        world.mgr.protected_changes(&loc).await.unwrap(),
        [".reno/policy.toml"],
        "against the thread's base, the earlier change shows"
    );

    // Judged from where the follow-up started, the follow-up has written nothing protected.
    let from_start = Location {
        base_sha: start,
        ..loc.clone()
    };
    assert!(
        world
            .mgr
            .protected_changes(&from_start)
            .await
            .unwrap()
            .is_empty()
    );

    // Its own write is found, and put back to what the file was when it began.
    std::fs::write(wt.join(".reno/defs/bot.toml"), "evil\n").unwrap();
    std::fs::write(wt.join(".reno/policy.toml"), "default = \"allow\"\n").unwrap();
    let found = world.mgr.protected_changes(&from_start).await.unwrap();
    assert_eq!(found, [".reno/defs/bot.toml", ".reno/policy.toml"]);
    world
        .mgr
        .revert_protected(&from_start, &found)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(wt.join(".reno/policy.toml")).unwrap(),
        "default = \"deny\"\n",
        "the earlier run's change is what the follow-up started with"
    );
    assert_eq!(
        std::fs::read_to_string(wt.join(".reno/defs/bot.toml")).unwrap(),
        "name = \"b\"\n"
    );
    assert!(
        world
            .mgr
            .protected_changes(&from_start)
            .await
            .unwrap()
            .is_empty()
    );
}

/// A project's harness and provider config is the run's to edit: whether tracked, deleted,
/// committed, untracked or ignored, the detector reports none of it.
#[tokio::test]
async fn harness_and_provider_config_changes_are_not_reported() {
    let world = World::new();
    let loc = world.prepared("r6").await;
    let wt = &loc.path;
    let files = [
        ".claude/skills/x/SKILL.md",
        ".codex/config.toml",
        ".pi/extensions/e.ts",
        "opencode.json",
        ".cursor/rules/a.mdc",
        ".grok/settings.json",
        // Ignored by the repository.
        ".claude/settings.local.json",
    ];
    for file in files {
        let path = wt.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "x\n").unwrap();
    }
    // A tracked file edited, another deleted.
    std::fs::write(wt.join(".claude/settings.json"), "{\"edited\":true}\n").unwrap();
    std::fs::remove_file(wt.join(".mcp.json")).unwrap();
    assert!(world.mgr.protected_changes(&loc).await.unwrap().is_empty());

    // Committed as well.
    git(wt, &["add", "-A"]);
    git(wt, &["commit", "-qm", "config"]);
    assert!(world.mgr.protected_changes(&loc).await.unwrap().is_empty());
}
