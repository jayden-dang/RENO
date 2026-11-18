//! What a run changed in its worktree against its base commit (`workspace.diff`), against real
//! temporary repositories. No network, no agent, no repository of the user's.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::{Path, PathBuf};
use std::process::Command;

use reno_harness::proc::git_workspace::{GitError, GitWorktrees, Location};
use reno_proto::{ChangeKind, ChangedFile};
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

    fn repo(&self) -> PathBuf {
        let dir = self.base.join("repos").join("project");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        git(&dir, &["init", "-q", "-b", "main"]);
        let body: String = (1..=20).map(|n| format!("line {n}\n")).collect();
        std::fs::write(dir.join("src/lib.rs"), "fn a() {}\n").unwrap();
        std::fs::write(dir.join("src/old name.rs"), &body).unwrap();
        std::fs::write(dir.join("gone.txt"), "bye\nbye\n").unwrap();
        std::fs::write(dir.join("keep.txt"), "keep\n").unwrap();
        std::fs::write(dir.join("logo.bin"), [0u8, 1, 2, 3]).unwrap();
        std::fs::write(dir.join(".gitignore"), "ignored.log\n").unwrap();
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

fn file<'a>(files: &'a [ChangedFile], path: &str) -> &'a ChangedFile {
    files
        .iter()
        .find(|f| f.path == path)
        .unwrap_or_else(|| panic!("{path} not in {files:?}"))
}

#[tokio::test]
async fn an_untouched_worktree_has_no_changes() {
    let world = World::new();
    let loc = world.prepared("c0").await;
    let got = world.mgr.changes_since_base(&loc, None).await.unwrap();
    assert_eq!((got.commits, got.files.len()), (0, 0));
    assert_eq!(got.patch, None);
}

#[tokio::test]
async fn committed_staged_unstaged_and_untracked_changes_are_all_listed() {
    let world = World::new();
    let loc = world.prepared("c1").await;
    let wt = &loc.path;
    // Committed: a modification.
    std::fs::write(wt.join("src/lib.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    git(wt, &["commit", "-qam", "edit lib"]);
    // Staged: a new file, and a deletion.
    std::fs::write(wt.join("staged.txt"), "s1\ns2\n").unwrap();
    git(wt, &["add", "staged.txt"]);
    git(wt, &["rm", "-q", "gone.txt"]);
    // Unstaged: an edit of a tracked file.
    std::fs::write(wt.join("keep.txt"), "keep\nmore\nmore\n").unwrap();
    // Untracked, with a space and a newline in the name, and an ignored file that must not show.
    std::fs::write(wt.join("new file.txt"), "a\nb\nc").unwrap();
    std::fs::write(wt.join("odd\nname.txt"), "x\n").unwrap();
    std::fs::write(wt.join("ignored.log"), "noise\n").unwrap();

    let got = world.mgr.changes_since_base(&loc, None).await.unwrap();
    assert_eq!(got.commits, 1);
    let paths: Vec<&str> = got.files.iter().map(|f| f.path.as_str()).collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted, "files are by path");
    assert!(!paths.contains(&"ignored.log"));

    let lib = file(&got.files, "src/lib.rs");
    assert_eq!(
        (lib.kind, lib.additions, lib.deletions),
        (ChangeKind::Modified, 1, 0)
    );
    let staged = file(&got.files, "staged.txt");
    assert_eq!(
        (staged.kind, staged.additions, staged.deletions),
        (ChangeKind::Added, 2, 0)
    );
    let gone = file(&got.files, "gone.txt");
    assert_eq!(
        (gone.kind, gone.additions, gone.deletions),
        (ChangeKind::Deleted, 0, 2)
    );
    let keep = file(&got.files, "keep.txt");
    assert_eq!(
        (keep.kind, keep.additions, keep.deletions),
        (ChangeKind::Modified, 2, 0)
    );
    // Untracked files are Added; a last line without a newline still counts.
    let spaced = file(&got.files, "new file.txt");
    assert_eq!((spaced.kind, spaced.additions), (ChangeKind::Added, 3));
    let odd = file(&got.files, "odd\nname.txt");
    assert_eq!((odd.kind, odd.additions), (ChangeKind::Added, 1));
}

#[tokio::test]
async fn a_rename_is_one_entry_with_its_old_path_and_a_binary_has_no_counts() {
    let world = World::new();
    let loc = world.prepared("c2").await;
    let wt = &loc.path;
    git(wt, &["mv", "src/old name.rs", "src/new name.rs"]);
    let mut body = std::fs::read_to_string(wt.join("src/new name.rs")).unwrap();
    body.push_str("extra\n");
    std::fs::write(wt.join("src/new name.rs"), body).unwrap();
    std::fs::write(wt.join("logo.bin"), [0u8, 9, 9, 9, 9, 9]).unwrap();
    std::fs::write(wt.join("fresh.bin"), [0u8, 1, 2]).unwrap();

    let got = world.mgr.changes_since_base(&loc, None).await.unwrap();
    let renamed = file(&got.files, "src/new name.rs");
    assert_eq!(renamed.kind, ChangeKind::Renamed);
    assert_eq!(renamed.from.as_deref(), Some("src/old name.rs"));
    assert_eq!((renamed.additions, renamed.deletions), (1, 0));
    assert!(
        got.files.iter().all(|f| f.path != "src/old name.rs"),
        "{:?}",
        got.files
    );
    let bin = file(&got.files, "logo.bin");
    assert_eq!(
        (bin.kind, bin.binary, bin.additions, bin.deletions),
        (ChangeKind::Modified, true, 0, 0)
    );
    let fresh = file(&got.files, "fresh.bin");
    assert_eq!(
        (fresh.kind, fresh.binary, fresh.additions),
        (ChangeKind::Added, true, 0)
    );
}

#[tokio::test]
async fn an_untracked_file_over_a_mebibyte_is_listed_without_counts() {
    let world = World::new();
    let loc = world.prepared("c3").await;
    std::fs::write(loc.path.join("big.txt"), "x\n".repeat(600 * 1024)).unwrap();
    let got = world.mgr.changes_since_base(&loc, None).await.unwrap();
    let big = file(&got.files, "big.txt");
    assert_eq!(
        (big.kind, big.additions, big.binary),
        (ChangeKind::Added, 0, false)
    );
}

#[tokio::test]
async fn the_patch_of_one_file_is_what_git_diff_against_the_base_says() {
    let world = World::new();
    let loc = world.prepared("c4").await;
    let wt = &loc.path;
    std::fs::write(wt.join("src/lib.rs"), "fn a() {}\nfn committed() {}\n").unwrap();
    git(wt, &["commit", "-qam", "edit"]);
    std::fs::write(
        wt.join("src/lib.rs"),
        "fn a() {}\nfn committed() {}\nfn dirty() {}\n",
    )
    .unwrap();
    let got = world
        .mgr
        .changes_since_base(&loc, Some("src/lib.rs"))
        .await
        .unwrap();
    let patch = got.patch.expect("a patch");
    assert!(!got.patch_truncated);
    assert!(patch.contains("+fn committed() {}"), "{patch}");
    assert!(patch.contains("+fn dirty() {}"), "{patch}");
    assert!(
        patch.starts_with("diff --git a/src/lib.rs b/src/lib.rs"),
        "{patch}"
    );
    // The list is still the whole list.
    assert_eq!(got.files.len(), 1);
}

#[tokio::test]
async fn an_untracked_file_has_a_new_file_patch_and_a_rename_shows_as_one() {
    let world = World::new();
    let loc = world.prepared("c5").await;
    let wt = &loc.path;
    std::fs::write(wt.join("my notes.md"), "hello\nworld\n").unwrap();
    let got = world
        .mgr
        .changes_since_base(&loc, Some("my notes.md"))
        .await
        .unwrap();
    let patch = got.patch.expect("a patch");
    assert!(patch.contains("new file mode"), "{patch}");
    assert!(patch.contains("+hello\n+world\n"), "{patch}");
    assert!(patch.contains("b/my notes.md"), "{patch}");
    // The index is untouched: the file is still untracked (no intent-to-add).
    assert_eq!(
        git(wt, &["ls-files", "--others", "--exclude-standard"]),
        "my notes.md"
    );

    git(wt, &["mv", "src/old name.rs", "src/moved.rs"]);
    let got = world
        .mgr
        .changes_since_base(&loc, Some("src/moved.rs"))
        .await
        .unwrap();
    let patch = got.patch.expect("a patch");
    assert!(patch.contains("rename from src/old name.rs"), "{patch}");
}

#[tokio::test]
async fn a_binary_an_unchanged_or_an_unlisted_file_has_no_patch() {
    let world = World::new();
    let loc = world.prepared("c6").await;
    std::fs::write(loc.path.join("logo.bin"), [0u8, 7, 7]).unwrap();
    std::fs::write(loc.path.join("a.txt"), "a\n").unwrap();
    for path in ["logo.bin", "keep.txt", "nope.txt"] {
        let got = world
            .mgr
            .changes_since_base(&loc, Some(path))
            .await
            .unwrap();
        assert_eq!(got.patch, None, "{path}");
        assert_eq!(got.files.len(), 2, "{path}");
    }
}

#[tokio::test]
async fn a_huge_patch_is_cut_at_256_kib_on_a_line() {
    let world = World::new();
    let loc = world.prepared("c7").await;
    let body: String = (0..40_000)
        .map(|n| format!("row {n} {}\n", "é".repeat(10)))
        .collect();
    std::fs::write(loc.path.join("big.txt"), &body).unwrap();
    let got = world
        .mgr
        .changes_since_base(&loc, Some("big.txt"))
        .await
        .unwrap();
    let patch = got.patch.expect("a patch");
    assert!(got.patch_truncated);
    assert!(patch.len() <= 256 * 1024, "{}", patch.len());
    assert!(patch.len() > 200 * 1024, "{}", patch.len());
    assert!(patch.ends_with('\n'));
    assert_eq!(file(&got.files, "big.txt").kind, ChangeKind::Added);
}

#[tokio::test]
async fn a_path_that_leaves_the_worktree_is_refused() {
    let world = World::new();
    let loc = world.prepared("c8").await;
    for bad in [
        "../x",
        "src/../../x",
        "/etc/passwd",
        "",
        "a//b",
        "a\0b",
        "src/./lib.rs",
    ] {
        let err = world
            .mgr
            .changes_since_base(&loc, Some(bad))
            .await
            .unwrap_err();
        assert!(matches!(err, GitError::Unsafe(_)), "{bad:?}: {err:?}");
    }
}

#[tokio::test]
async fn an_untracked_symlink_is_never_followed() {
    let world = World::new();
    let loc = world.prepared("c9").await;
    let secret = world.base.join("secret.txt");
    std::fs::write(&secret, "TOPSECRET\n").unwrap();
    std::os::unix::fs::symlink(&secret, loc.path.join("link.txt")).unwrap();
    std::os::unix::fs::symlink(&world.base, loc.path.join("linkdir")).unwrap();
    let got = world
        .mgr
        .changes_since_base(&loc, Some("link.txt"))
        .await
        .unwrap();
    assert_eq!(file(&got.files, "link.txt").additions, 0);
    assert!(
        !got.patch.unwrap_or_default().contains("TOPSECRET"),
        "followed a symlink out of the worktree"
    );
    // A path through a symlinked directory is not a listed file.
    let got = world
        .mgr
        .changes_since_base(&loc, Some("linkdir/secret.txt"))
        .await
        .unwrap();
    assert_eq!(got.patch, None);
}

#[tokio::test]
async fn a_vanished_worktree_has_no_changes() {
    let world = World::new();
    let loc = world.prepared("c10").await;
    std::fs::remove_dir_all(&loc.path).unwrap();
    let got = world.mgr.changes_since_base(&loc, None).await.unwrap();
    assert!(got.files.is_empty());
}

#[tokio::test]
async fn reading_the_changes_leaves_the_index_and_head_alone() {
    let world = World::new();
    let loc = world.prepared("c11").await;
    let wt = &loc.path;
    std::fs::write(wt.join("keep.txt"), "changed\n").unwrap();
    std::fs::write(wt.join("fresh.txt"), "n\n").unwrap();
    let index = wt.join(git(wt, &["rev-parse", "--git-dir"])).join("index");
    let status = git(wt, &["status", "--porcelain"]);
    let before = std::fs::read(&index).unwrap();
    world.mgr.changes_since_base(&loc, None).await.unwrap();
    world
        .mgr
        .changes_since_base(&loc, Some("fresh.txt"))
        .await
        .unwrap();
    assert_eq!(std::fs::read(&index).unwrap(), before);
    assert_eq!(git(wt, &["status", "--porcelain"]), status);
}

#[tokio::test]
async fn head_follows_the_branch_and_a_run_scope_starts_there() {
    let world = World::new();
    let loc = world.prepared("thr_h1").await;
    let wt = &loc.path;
    assert_eq!(world.mgr.head(&loc).await.unwrap(), loc.base_sha);

    // The first run commits one file; the second starts from there.
    std::fs::write(wt.join("first.txt"), "one\n").unwrap();
    git(wt, &["add", "first.txt"]);
    git(wt, &["commit", "-qm", "first run"]);
    let start = world.mgr.head(&loc).await.unwrap();
    assert_ne!(start, loc.base_sha);
    std::fs::write(wt.join("second.txt"), "two\n").unwrap();

    // The whole thread against its base holds both; the second run, against where it began,
    // only its own.
    let thread = world.mgr.changes_since_base(&loc, None).await.unwrap();
    let mut names: Vec<_> = thread.files.iter().map(|f| f.path.as_str()).collect();
    names.sort_unstable();
    assert_eq!(
        (thread.commits, names),
        (1, vec!["first.txt", "second.txt"])
    );
    let from_start = Location {
        base_sha: start,
        ..loc.clone()
    };
    let run = world
        .mgr
        .changes_since_base(&from_start, None)
        .await
        .unwrap();
    assert_eq!(
        (
            run.commits,
            run.files
                .iter()
                .map(|f| f.path.as_str())
                .collect::<Vec<_>>()
        ),
        (0, vec!["second.txt"])
    );
}

#[tokio::test]
async fn one_commit_is_shown_against_its_parent_and_only_a_commit_of_the_branch() {
    let world = World::new();
    let loc = world.prepared("thr_c1").await;
    let wt = &loc.path;
    std::fs::write(wt.join("src/lib.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    git(wt, &["commit", "-qam", "edit lib"]);
    let first = world.mgr.head(&loc).await.unwrap();
    std::fs::write(wt.join("added.txt"), "x\ny\n").unwrap();
    git(wt, &["add", "added.txt"]);
    git(wt, &["commit", "-qm", "add a file"]);
    let second = world.mgr.head(&loc).await.unwrap();
    // Dirty and untracked work is not in any commit.
    std::fs::write(wt.join("untracked.txt"), "u\n").unwrap();
    std::fs::write(wt.join("keep.txt"), "changed\n").unwrap();

    let one = world.mgr.commit_changes(&loc, &first, None).await.unwrap();
    assert_eq!(one.commits, 1);
    assert_eq!(one.files.len(), 1);
    let lib = file(&one.files, "src/lib.rs");
    assert_eq!(
        (lib.kind, lib.additions, lib.deletions),
        (ChangeKind::Modified, 1, 0)
    );
    assert_eq!(one.patch, None, "no path, no patch");

    // The patch of one file of it, by an abbreviated name.
    let abbreviated = &second[..10];
    let two = world
        .mgr
        .commit_changes(&loc, abbreviated, Some("added.txt"))
        .await
        .unwrap();
    assert_eq!(two.files.len(), 1);
    assert_eq!(file(&two.files, "added.txt").kind, ChangeKind::Added);
    let patch = two.patch.unwrap();
    assert!(patch.contains("+x") && patch.contains("+y"), "{patch}");

    // The repository's own first commit is before the branch: not this thread's.
    for refused in [
        loc.base_sha.clone(),
        "deadbeefdeadbeef".to_owned(),
        "xyz1234".to_owned(),
        "HEAD".to_owned(),
        "--output=x".to_owned(),
        format!("{first};ls"),
    ] {
        let got = world.mgr.commit_changes(&loc, &refused, None).await;
        assert!(
            matches!(got, Err(GitError::Unsafe(_))),
            "{refused}: {got:?}"
        );
    }
    // A worktree that is gone has nothing to show.
    std::fs::remove_dir_all(wt).unwrap();
    let gone = world.mgr.commit_changes(&loc, &first, None).await.unwrap();
    assert_eq!((gone.commits, gone.files.len()), (0, 0));
}
