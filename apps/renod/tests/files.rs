//! The Files tab through the daemon (RP1): `fs.list`, `fs.read`, `fs.read_image`, `fs.search_*`,
//! `git.status` and `fs.watch`, over the real socket, against a run's worktree in a real
//! temporary git repository. Real `git`, no network, no real agent. The platform's file watcher
//! is replaced by one the test drives, so no test depends on how many FSEvents streams are free.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Fixture, id, within};
use futures::StreamExt;
use notify::event::CreateKind;
use notify::{Event, EventKind};
use reno_client::{Client, ClientError};
use reno_proto::{
    CleanTarget, ErrorCode, FEATURES, FS_IMAGE_CHUNK_BYTES, FS_READ_MAX_BYTES, FsChange,
    FsEntryKind, FsListParams, FsReadImageParams, FsReadParams, FsSearchContentParams,
    FsSearchFilesParams, GitChange, RunId, RunStatus, TaskRunParams,
};
use renod::WatcherFactory;
use renod::watcher::{Handler, WatcherGuard};

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

fn put(dir: &Path, rel: &str, text: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A repository with some history. `kept.log` matches an ignore pattern but is tracked.
fn repo(fx: &Fixture) -> PathBuf {
    let dir = fx.dir.path().canonicalize().unwrap().join("proj");
    std::fs::create_dir_all(&dir).unwrap();
    git(&dir, &["init", "-q", "-b", "main"]);
    // A person's own git configuration must not change what is read: each of these would break
    // a parser that did not pin its own settings on the command line.
    for (key, value) in [
        ("status.renames", "false"),
        ("status.showUntrackedFiles", "no"),
        ("grep.column", "true"),
        ("grep.fullName", "true"),
        ("grep.lineNumber", "false"),
        ("color.ui", "always"),
        ("core.quotePath", "true"),
    ] {
        git(&dir, &["config", key, value]);
    }
    put(&dir, ".gitignore", "ignored/\n*.log\ntarget/\n");
    put(&dir, "src/lib.rs", "fn alpha() {}\nfn beta() {}\n");
    put(&dir, "src/main.rs", "fn main() { alpha(); }\n");
    put(&dir, "README.md", "Alpha readme\n");
    put(&dir, "old.txt", "old\n");
    put(&dir, "docs/guide.md", "# Guide\n");
    put(&dir, "kept.log", "tracked log\n");
    git(&dir, &["add", "-f", "."]);
    git(&dir, &["commit", "-qm", "one"]);
    dir
}

fn task(cwd: &Path, executor: &str, in_place: bool) -> TaskRunParams {
    TaskRunParams {
        thread_id: None,
        model_options: Default::default(),
        effort: None,
        prompt: "work".into(),
        executor: Some(id(executor)),
        bot: None,
        cwd: Some(cwd.to_path_buf()),
        model: None,
        in_place,
        attachments: Vec::new(),
    }
}

async fn until_running(fx: &Fixture, run: &RunId) {
    within(async {
        loop {
            let now = fx.status(run).await;
            if now == RunStatus::Running {
                return;
            }
            assert!(!now.is_terminal(), "{run} ended {now:?}");
            tokio::task::yield_now().await;
        }
    })
    .await;
}

/// A live run in a worktree of `repo`, and the worktree.
async fn live(fx: &Fixture, repo: &Path) -> (RunId, PathBuf) {
    let run = within(fx.client.run_task(task(repo, "mock-hold", false)))
        .await
        .unwrap();
    until_running(fx, &run).await;
    let info = within(async {
        loop {
            if let Ok(info) = fx.client.get_workspace(run.clone()).await
                && info.status == reno_proto::WorkspaceStatus::Active
            {
                return info;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    (run, info.path.canonicalize().unwrap())
}

fn list(run: &RunId, dir: &str) -> FsListParams {
    FsListParams {
        run_id: run.clone(),
        dir: dir.into(),
        cursor: None,
        limit: None,
    }
}

fn read(run: &RunId, path: &str) -> FsReadParams {
    FsReadParams {
        run_id: run.clone(),
        path: path.into(),
    }
}

fn code<T: std::fmt::Debug>(result: Result<T, ClientError>) -> ErrorCode {
    result
        .expect_err("an error")
        .code()
        .expect("a daemon error")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_daemon_offers_the_files_feature_and_a_client_refuses_up_front_without_it() {
    let fx = Fixture::start().await;
    assert!(FEATURES.contains(&"files"));
    assert!(fx.client.supports("files"));
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_live_runs_worktree_is_listed_read_and_hides_git() {
    let fx = Fixture::start().await;
    let (run, tree) = live(&fx, &repo(&fx)).await;

    let root = within(fx.client.fs_list(list(&run, ""))).await.unwrap();
    let names: Vec<&str> = root.entries.iter().map(|e| e.name.as_str()).collect();
    // Directories first, then files, case-folded; `.git` (a file in a worktree) is hidden.
    assert_eq!(
        names,
        [
            "docs",
            "src",
            ".gitignore",
            "kept.log",
            "old.txt",
            "README.md"
        ]
    );
    assert!(!root.in_place);
    assert_eq!(root.entries[0].kind, FsEntryKind::Dir);
    assert_eq!(root.entries[2].size, Some(23));
    assert!(root.entries.iter().all(|e| !e.ignored));

    // What git ignores is flagged, so the tree can dim it; a tracked file is never ignored,
    // whatever the patterns say.
    put(&tree, "ignored/skip.rs", "x");
    put(&tree, "debug.log", "x");
    let again = within(fx.client.fs_list(list(&run, ""))).await.unwrap();
    let flagged: Vec<(&str, bool)> = again
        .entries
        .iter()
        .map(|e| (e.name.as_str(), e.ignored))
        .collect();
    assert!(flagged.contains(&("ignored", true)), "{flagged:?}");
    assert!(flagged.contains(&("debug.log", true)), "{flagged:?}");
    assert!(flagged.contains(&("kept.log", false)), "{flagged:?}");
    assert!(flagged.contains(&("src", false)), "{flagged:?}");
    assert!(
        within(fx.client.fs_list(list(&run, "ignored")))
            .await
            .unwrap()
            .entries
            .iter()
            .all(|e| !e.name.is_empty())
    );

    let src = within(fx.client.fs_list(list(&run, "src"))).await.unwrap();
    let paths: Vec<&str> = src.entries.iter().map(|e| e.path.as_str()).collect();
    assert_eq!(paths, ["src/lib.rs", "src/main.rs"]);

    let lib = within(fx.client.fs_read(read(&run, "src/lib.rs")))
        .await
        .unwrap();
    assert_eq!(lib.text.as_deref(), Some("fn alpha() {}\nfn beta() {}\n"));
    assert!(!lib.binary && !lib.truncated && !lib.in_place);
    assert_eq!(lib.encoding, "utf-8");
    assert_eq!(
        lib.content_hash,
        Some(reno_proto::image_hash(b"fn alpha() {}\nfn beta() {}\n"))
    );
    assert!(lib.mtime_ms.is_some());

    // A write in the worktree shows at once: nothing is cached.
    put(&tree, "src/lib.rs", "changed\n");
    let again = within(fx.client.fs_read(read(&run, "src/lib.rs")))
        .await
        .unwrap();
    assert_eq!(again.text.as_deref(), Some("changed\n"));
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_sandbox_refuses_traversal_absolute_paths_git_and_links_that_leave() {
    let fx = Fixture::start().await;
    let (run, tree) = live(&fx, &repo(&fx)).await;
    let outside = fx.dir.path().canonicalize().unwrap().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    put(&outside, "secret.txt", "secret\n");
    symlink(outside.join("secret.txt"), tree.join("leak.txt")).unwrap();
    symlink(&outside, tree.join("leakdir")).unwrap();
    symlink("src/lib.rs", tree.join("fine")).unwrap();

    for bad in [
        "../outside/secret.txt",
        "src/../../x",
        "/etc/passwd",
        ".git",
        ".git/config",
        "leak.txt",
        "leakdir/secret.txt",
        "leakdir/nothing-here",
    ] {
        assert_eq!(
            code(fx.client.fs_read(read(&run, bad)).await),
            ErrorCode::PathRejected,
            "read {bad}"
        );
        assert_eq!(
            code(fx.client.fs_list(list(&run, bad)).await),
            ErrorCode::PathRejected,
            "list {bad}"
        );
        let image = FsReadImageParams {
            run_id: run.clone(),
            path: bad.into(),
            offset: 0,
        };
        assert_eq!(
            code(fx.client.fs_read_image(image).await),
            ErrorCode::PathRejected,
            "image {bad}"
        );
    }
    // A link that stays inside is followed; one in a listing is shown as a link, not followed.
    let fine = within(fx.client.fs_read(read(&run, "fine"))).await.unwrap();
    assert!(fine.text.unwrap().contains("alpha"));
    let root = within(fx.client.fs_list(list(&run, ""))).await.unwrap();
    let leak = root.entries.iter().find(|e| e.name == "leak.txt").unwrap();
    assert_eq!(leak.kind, FsEntryKind::Symlink);
    // The refusal says which kind, typed.
    let error = fx.client.fs_read(read(&run, "../x")).await.unwrap_err();
    let ClientError::Remote(object) = error else {
        panic!("not a daemon error")
    };
    assert_eq!(object.data.unwrap()["kind"], "traversal");
    // Missing is not the same as refused.
    assert_eq!(
        code(fx.client.fs_read(read(&run, "src/none.rs")).await),
        ErrorCode::NotFound
    );
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listing_pages_by_cursor_and_binary_and_large_files_are_handled() {
    let fx = Fixture::start().await;
    let (run, tree) = live(&fx, &repo(&fx)).await;
    for n in 0..12 {
        put(&tree, &format!("many/f{n:02}.txt"), "x");
    }
    let mut cursor = None;
    let mut seen = Vec::new();
    let mut pages = 0;
    loop {
        let page = within(fx.client.fs_list(FsListParams {
            run_id: run.clone(),
            dir: "many".into(),
            cursor: cursor.take(),
            limit: Some(5),
        }))
        .await
        .unwrap();
        assert!(page.entries.len() <= 5);
        seen.extend(page.entries.into_iter().map(|e| e.name));
        pages += 1;
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    let wanted: Vec<String> = (0..12).map(|n| format!("f{n:02}.txt")).collect();
    assert_eq!(seen, wanted);
    assert_eq!(
        code(
            fx.client
                .fs_list(FsListParams {
                    cursor: Some("junk".into()),
                    ..list(&run, "many")
                })
                .await
        ),
        ErrorCode::InvalidParams
    );

    std::fs::write(tree.join("blob.bin"), b"\x00\x01\x02binary").unwrap();
    let bin = within(fx.client.fs_read(read(&run, "blob.bin")))
        .await
        .unwrap();
    assert!(bin.binary && bin.text.is_none());
    assert_eq!(bin.size, 9);

    put(&tree, "big.txt", &"line\n".repeat(FS_READ_MAX_BYTES / 4));
    let big = within(fx.client.fs_read(read(&run, "big.txt")))
        .await
        .unwrap();
    assert!(big.truncated);
    assert_eq!(big.text.unwrap().len(), FS_READ_MAX_BYTES);
    assert_eq!(big.size as usize, "line\n".len() * (FS_READ_MAX_BYTES / 4));

    // Directories are not files, and files are not directories.
    assert_eq!(
        code(fx.client.fs_read(read(&run, "src")).await),
        ErrorCode::InvalidParams
    );
    assert_eq!(
        code(fx.client.fs_list(list(&run, "README.md")).await),
        ErrorCode::InvalidParams
    );
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_image_arrives_in_chunks_and_joins_back_to_its_bytes() {
    let fx = Fixture::start().await;
    let (run, tree) = live(&fx, &repo(&fx)).await;
    let bytes: Vec<u8> = (0..FS_IMAGE_CHUNK_BYTES + 1000)
        .map(|n| (n % 253) as u8)
        .collect();
    std::fs::write(tree.join("pic.png"), &bytes).unwrap();
    let first = within(fx.client.fs_read_image(FsReadImageParams {
        run_id: run.clone(),
        path: "pic.png".into(),
        offset: 0,
    }))
    .await
    .unwrap();
    assert!(!first.eof);
    assert_eq!(first.bytes().unwrap().len(), FS_IMAGE_CHUNK_BYTES);
    let (mime, all) = within(fx.client.fs_read_image_all(run.clone(), "pic.png".into()))
        .await
        .unwrap();
    assert_eq!((mime.as_str(), all), ("image/png", bytes));
    // Not an image.
    assert_eq!(
        code(
            fx.client
                .fs_read_image(FsReadImageParams {
                    run_id: run.clone(),
                    path: "README.md".into(),
                    offset: 0,
                })
                .await
        ),
        ErrorCode::InvalidParams
    );
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_finds_files_by_name_and_lines_by_content_and_skips_ignored_files() {
    let fx = Fixture::start().await;
    let (run, tree) = live(&fx, &repo(&fx)).await;
    // Untracked but not ignored: found. Ignored and untracked: not.
    // A link to a file outside the root that holds the query: git is never asked to read it.
    let outside = fx.dir.path().canonicalize().unwrap().join("outside.txt");
    std::fs::write(&outside, "alpha outside the root\n").unwrap();
    symlink(&outside, tree.join("leak.txt")).unwrap();
    put(&tree, "src/extra.rs", "fn Alpha_extra() {}\n");
    put(&tree, "ignored/skip.rs", "fn alpha_hidden() {}\n");
    put(&tree, "debug.log", "alpha in a log\n");
    std::fs::write(tree.join("blob.bin"), b"\0alpha").unwrap();

    let files = |query: &str| FsSearchFilesParams {
        run_id: run.clone(),
        query: query.into(),
        limit: None,
    };
    let found = within(fx.client.fs_search_files(files("lrs")))
        .await
        .unwrap();
    assert_eq!(found.files[0].path, "src/lib.rs");
    let all = within(fx.client.fs_search_files(files("rs")))
        .await
        .unwrap();
    let paths: Vec<&str> = all.files.iter().map(|f| f.path.as_str()).collect();
    assert!(paths.contains(&"src/extra.rs"), "{paths:?}");
    assert!(!paths.iter().any(|p| p.contains("ignored")), "{paths:?}");
    // Ignored, but tracked: found. Ignored and not tracked: not.
    let logs = within(fx.client.fs_search_files(files("log")))
        .await
        .unwrap();
    let paths: Vec<&str> = logs.files.iter().map(|f| f.path.as_str()).collect();
    assert!(paths.contains(&"kept.log"), "{paths:?}");
    assert!(!paths.contains(&"debug.log"), "{paths:?}");
    // Nothing of .git ever.
    let git_hits = within(fx.client.fs_search_files(files("config")))
        .await
        .unwrap();
    assert!(git_hits.files.iter().all(|f| !f.path.starts_with(".git/")));
    // A deleted tracked file is no result.
    std::fs::remove_file(tree.join("old.txt")).unwrap();
    let old = within(fx.client.fs_search_files(files("old")))
        .await
        .unwrap();
    assert!(old.files.is_empty(), "{old:?}");
    // The limit cuts and says so.
    let two = within(fx.client.fs_search_files(FsSearchFilesParams {
        limit: Some(2),
        ..files("r")
    }))
    .await
    .unwrap();
    assert_eq!(two.files.len(), 2);
    assert!(two.truncated);

    let content = |query: &str, regex: bool, ignore_case: bool| FsSearchContentParams {
        run_id: run.clone(),
        query: query.into(),
        regex,
        ignore_case,
        limit: None,
    };
    let literal = within(fx.client.fs_search_content(content("alpha", false, false)))
        .await
        .unwrap();
    let hits: Vec<(&str, u32)> = literal
        .matches
        .iter()
        .map(|m| (m.path.as_str(), m.line))
        .collect();
    assert_eq!(hits, [("src/lib.rs", 1), ("src/main.rs", 1)]);
    assert_eq!(literal.matches[0].text, "fn alpha() {}");
    // The byte offset of the first match: `fn alpha` starts its match at 3.
    assert_eq!(literal.matches[0].column, 3);
    assert!(
        literal
            .matches
            .iter()
            .all(|m| m.path != "leak.txt" && !m.text.contains("outside")),
        "{literal:?}"
    );
    assert!(!literal.truncated);
    let folded = within(fx.client.fs_search_content(content("alpha", false, true)))
        .await
        .unwrap();
    let paths: Vec<&str> = folded.matches.iter().map(|m| m.path.as_str()).collect();
    assert_eq!(
        paths,
        ["README.md", "src/extra.rs", "src/lib.rs", "src/main.rs"]
    );
    let regex = within(
        fx.client
            .fs_search_content(content("fn (alpha|beta)", true, false)),
    )
    .await
    .unwrap();
    assert_eq!(regex.matches.len(), 2);
    // `.` is a plain character in a literal search.
    let dots = within(fx.client.fs_search_content(content("a.pha", false, false)))
        .await
        .unwrap();
    assert!(dots.matches.is_empty());
    // A pattern git refuses is the caller's error; an empty one too.
    assert_eq!(
        code(fx.client.fs_search_content(content("(", true, false)).await),
        ErrorCode::InvalidParams
    );
    assert_eq!(
        code(fx.client.fs_search_content(content("", false, false)).await),
        ErrorCode::InvalidParams
    );
    // The line limit cuts and says so.
    let cut = within(fx.client.fs_search_content(FsSearchContentParams {
        limit: Some(1),
        ..content("alpha", false, true)
    }))
    .await
    .unwrap();
    assert_eq!(cut.matches.len(), 1);
    assert!(cut.truncated);
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn git_status_reports_added_modified_renamed_deleted_and_untracked() {
    let fx = Fixture::start().await;
    let (run, tree) = live(&fx, &repo(&fx)).await;
    let clean = within(fx.client.git_status(run.clone())).await.unwrap();
    assert!(clean.repo && clean.files.is_empty());
    assert_eq!(clean.revision.len(), 64);
    let thread = fx.client.list_threads(None).await.unwrap()[0]
        .thread_id
        .clone();
    assert_eq!(
        clean.branch.as_deref(),
        Some(format!("reno/{thread}").as_str())
    );

    put(&tree, "README.md", "edited\n");
    put(&tree, "src/added.rs", "new\n");
    git(&tree, &["add", "src/added.rs"]);
    git(&tree, &["mv", "docs/guide.md", "docs/manual.md"]);
    std::fs::remove_file(tree.join("old.txt")).unwrap();
    put(&tree, "untracked.txt", "u\n");
    put(&tree, "src/lib.rs", "staged\n");
    git(&tree, &["add", "src/lib.rs"]);
    put(&tree, "src/lib.rs", "staged and edited again\n");

    let status = within(fx.client.git_status(run.clone())).await.unwrap();
    let of = |path: &str| status.files.iter().find(|f| f.path == path).unwrap();
    let sides = |path: &str| (of(path).index, of(path).worktree);
    assert_eq!(
        sides("README.md"),
        (GitChange::Unmodified, GitChange::Modified)
    );
    assert_eq!(
        sides("src/added.rs"),
        (GitChange::Added, GitChange::Unmodified)
    );
    assert_eq!(
        sides("docs/manual.md"),
        (GitChange::Renamed, GitChange::Unmodified)
    );
    assert_eq!(
        of("docs/manual.md").old_path.as_deref(),
        Some("docs/guide.md")
    );
    assert_eq!(
        sides("old.txt"),
        (GitChange::Unmodified, GitChange::Deleted)
    );
    assert_eq!(
        sides("untracked.txt"),
        (GitChange::Untracked, GitChange::Untracked)
    );
    assert_eq!(
        sides("src/lib.rs"),
        (GitChange::Modified, GitChange::Modified)
    );
    assert_eq!(status.files.len(), 6);
    // The revision moves with the status, and only with it.
    assert_ne!(status.revision, clean.revision);
    let same = within(fx.client.git_status(run.clone())).await.unwrap();
    assert_eq!(same.revision, status.revision);
    // A status refresh takes no lock: the run can still `git add` right after.
    assert!(!tree.join(".git").is_dir());
    git(&tree, &["add", "untracked.txt"]);
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_ended_run_whose_worktree_is_kept_is_served_and_a_removed_one_is_unavailable() {
    let fx = Fixture::start().await;
    let (run, tree) = live(&fx, &repo(&fx)).await;
    put(&tree, "agent.txt", "work\n");
    git(&tree, &["add", "agent.txt"]);
    git(&tree, &["commit", "-qm", "agent"]);
    within(fx.client.cancel_run(&run, None)).await.unwrap();
    fx.events(&run, None).await;

    // Ended; the worktree holds unmerged work and stays.
    let after = within(fx.client.fs_read(read(&run, "agent.txt")))
        .await
        .unwrap();
    assert_eq!(after.text.as_deref(), Some("work\n"));
    assert!(
        within(fx.client.git_status(run.clone()))
            .await
            .unwrap()
            .repo
    );

    // Removed: a typed, stable error for every method.
    within(fx.client.clean_workspaces(
        CleanTarget::Run {
            run_id: run.clone(),
        },
        true,
    ))
    .await
    .unwrap();
    assert!(!tree.exists());
    let gone = ErrorCode::WorkspaceUnavailable;
    assert_eq!(code(fx.client.fs_list(list(&run, "")).await), gone);
    assert_eq!(code(fx.client.fs_read(read(&run, "agent.txt")).await), gone);
    assert_eq!(code(fx.client.git_status(run.clone()).await), gone);
    assert_eq!(
        code(
            fx.client
                .fs_search_files(FsSearchFilesParams {
                    run_id: run.clone(),
                    query: "a".into(),
                    limit: None
                })
                .await
        ),
        gone
    );
    assert_eq!(
        code(fx.client.watch_fs(run.clone()).await.map(|_| ())),
        gone
    );
    assert_eq!(gone.code(), -32013);

    // An unknown run is not found.
    let nobody = RunId::from_suffix("nobody").unwrap();
    assert_eq!(
        code(fx.client.fs_list(list(&nobody, "")).await),
        ErrorCode::NotFound
    );
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_with_no_worktree_is_served_from_its_folder_and_says_so() {
    let fx = Fixture::start().await;
    let plain = fx.dir.path().canonicalize().unwrap().join("plain");
    put(&plain, "notes.txt", "hello\n");
    put(&plain, "sub/deep.txt", "deep\n");
    let run = within(fx.client.run_task(task(&plain, "mock", true)))
        .await
        .unwrap();
    fx.events(&run, None).await;
    assert!(fx.client.get_workspace(run.clone()).await.is_err());

    let listed = within(fx.client.fs_list(list(&run, ""))).await.unwrap();
    assert!(listed.in_place);
    let names: Vec<&str> = listed.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["sub", "notes.txt"]);
    let text = within(fx.client.fs_read(read(&run, "notes.txt")))
        .await
        .unwrap();
    assert!(text.in_place);
    assert_eq!(text.text.as_deref(), Some("hello\n"));
    // Not a repository: no status, said plainly. Search still works, over the folder.
    let status = within(fx.client.git_status(run.clone())).await.unwrap();
    assert!(!status.repo && status.in_place && status.files.is_empty());
    let found = within(fx.client.fs_search_files(FsSearchFilesParams {
        run_id: run.clone(),
        query: "deep".into(),
        limit: None,
    }))
    .await
    .unwrap();
    assert_eq!(found.files[0].path, "sub/deep.txt");
    assert!(found.in_place);
    let grep = within(fx.client.fs_search_content(FsSearchContentParams {
        run_id: run.clone(),
        query: "hello".into(),
        regex: false,
        ignore_case: false,
        limit: None,
    }))
    .await
    .unwrap();
    assert_eq!(grep.matches[0].path, "notes.txt");
    // The sandbox holds here too.
    assert_eq!(
        code(fx.client.fs_read(read(&run, "../plain/notes.txt")).await),
        ErrorCode::PathRejected
    );
    fx.stop().await;
}

// ----- fs.watch

/// A watcher factory the test drives: it keeps the handlers it is given, counts the watchers
/// that are alive, and can refuse the first starts.
#[derive(Default)]
struct Rig {
    handlers: Mutex<Vec<Handler>>,
    live: Arc<AtomicUsize>,
    refuse: AtomicUsize,
    attempts: AtomicUsize,
}

impl std::fmt::Debug for Rig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Rig")
    }
}

struct Alive(Arc<AtomicUsize>);

impl Drop for Alive {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl WatcherFactory for Rig {
    fn start(&self, _roots: &[PathBuf], handler: Handler) -> Result<WatcherGuard, String> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self
            .refuse
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
            .is_ok()
        {
            return Err("cannot watch: unable to start FSEvent stream".into());
        }
        self.handlers.lock().unwrap().push(handler);
        self.live.fetch_add(1, Ordering::SeqCst);
        Ok(WatcherGuard::new(Alive(Arc::clone(&self.live))))
    }
}

impl Rig {
    fn live(&self) -> usize {
        self.live.load(Ordering::SeqCst)
    }

    fn create(&self, path: &Path) {
        let event = Event::new(EventKind::Create(CreateKind::File)).add_path(path.to_owned());
        for handler in self.handlers.lock().unwrap().iter_mut() {
            handler(Ok(event.clone()));
        }
    }
}

async fn next(
    stream: &mut (impl futures::Stream<Item = Result<FsChange, ClientError>> + Unpin),
) -> FsChange {
    within(stream.next()).await.expect("an item").expect("ok")
}

async fn until_live(rig: &Rig, want: usize) {
    within(async {
        while rig.live() != want {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_watch_runs_only_while_subscribed_coalesces_bursts_and_flags_git() {
    let rig = Arc::new(Rig::default());
    let fx = Fixture::with(|config| config.watchers = rig.clone()).await;
    let (run, tree) = live(&fx, &repo(&fx)).await;
    // The daemon's own definitions watch is alive; the Files service has none yet.
    let baseline = rig.live();

    let mut changes = within(fx.client.watch_fs(run.clone())).await.unwrap();
    assert_eq!(rig.live(), baseline + 1, "armed for the subscriber");

    for path in ["src/a.rs", "docs/x.md", "src/b.rs", "top.txt"] {
        rig.create(&tree.join(path));
    }
    let burst = next(&mut changes).await;
    assert_eq!(burst.dirs, ["", "docs", "src"]);
    assert!(!burst.git && !burst.overflow && !burst.degraded);

    // Git's own state: the worktree's private git directory.
    let git_dir = PathBuf::from(git(&tree, &["rev-parse", "--absolute-git-dir"]));
    rig.create(&git_dir.join("index"));
    rig.create(&git_dir.join("index.lock"));
    rig.create(&tree.join(".git"));
    let moved = next(&mut changes).await;
    assert!(moved.git && moved.dirs.is_empty(), "{moved:?}");

    // Unsubscribing stops the watcher.
    drop(changes);
    until_live(&rig, baseline).await;
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_watch_that_cannot_start_says_so_retries_and_then_asks_for_a_rescan() {
    let rig = Arc::new(Rig::default());
    let fx = Fixture::with(|config| config.watchers = rig.clone()).await;
    let (run, tree) = live(&fx, &repo(&fx)).await;
    let baseline = rig.live();

    rig.refuse.store(2, Ordering::SeqCst);
    let attempts = rig.attempts.load(Ordering::SeqCst);
    let mut changes = within(fx.client.watch_fs(run.clone())).await.unwrap();
    let down = next(&mut changes).await;
    assert!(down.degraded);
    let back = next(&mut changes).await;
    assert!(back.overflow && !back.degraded);
    // Two refusals and the start that worked: three attempts, after the daemon's own backoff.
    assert_eq!(rig.attempts.load(Ordering::SeqCst) - attempts, 3);
    assert_eq!(rig.live(), baseline + 1);
    rig.create(&tree.join("src/z.rs"));
    assert_eq!(next(&mut changes).await.dirs, ["src"]);
    drop(changes);
    until_live(&rig, baseline).await;
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscriptions_of_one_tree_share_a_watcher_and_the_daemon_caps_how_many_trees() {
    let rig = Arc::new(Rig::default());
    let fx = Fixture::with(|config| config.watchers = rig.clone()).await;
    let repo = repo(&fx);
    let (first, _tree) = live(&fx, &repo).await;
    let baseline = rig.live();

    // Two subscriptions to one tree (two windows): one watcher between them.
    let one = within(fx.client.watch_fs(first.clone())).await.unwrap();
    let two = within(fx.client.watch_fs(first.clone())).await.unwrap();
    assert_eq!(rig.live(), baseline + 1);
    drop(one);
    // Not stopped yet: the other still looks.
    assert_eq!(rig.live(), baseline + 1);
    drop(two);
    until_live(&rig, baseline).await;

    // Each thread has its own worktree, so each of these is another tree.
    let mut held = Vec::new();
    let mut last = first.clone();
    for _ in 0..renod::files::MAX_OPEN_WATCHES {
        // A run that has ended keeps its thread's worktree while the thread is open.
        let run = within(fx.client.run_task(task(&repo, "mock", false)))
            .await
            .unwrap();
        fx.events(&run, None).await;
        held.push(within(fx.client.watch_fs(run.clone())).await.unwrap());
        last = run;
    }
    assert_eq!(rig.live(), baseline + renod::files::MAX_OPEN_WATCHES);
    // One more tree is refused, as a typed "unavailable", and costs no watcher; a tree that is
    // already watched is not refused.
    assert_eq!(
        code(fx.client.watch_fs(first.clone()).await.map(|_| ())),
        ErrorCode::Unavailable
    );
    held.push(within(fx.client.watch_fs(last)).await.unwrap());
    assert_eq!(rig.live(), baseline + renod::files::MAX_OPEN_WATCHES);
    drop(held);
    until_live(&rig, baseline).await;
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_disconnect_stops_the_watches_it_held() {
    let rig = Arc::new(Rig::default());
    let fx = Fixture::with(|config| config.watchers = rig.clone()).await;
    let (run, _tree) = live(&fx, &repo(&fx)).await;
    let baseline = rig.live();

    let other = within(Client::connect_unix(&fx.paths.socket))
        .await
        .unwrap();
    let held = within(other.watch_fs(run.clone())).await.unwrap();
    assert_eq!(rig.live(), baseline + 1);
    // The connection goes away without unsubscribing: the watch goes with it.
    drop(held);
    other.close().await;
    drop(other);
    until_live(&rig, baseline).await;
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_watch_drops_the_noise_of_ignored_build_output() {
    let rig = Arc::new(Rig::default());
    let fx = Fixture::with(|config| config.watchers = rig.clone()).await;
    let (run, tree) = live(&fx, &repo(&fx)).await;
    put(&tree, "ignored/old.txt", "x");
    let mut changes = within(fx.client.watch_fs(run.clone())).await.unwrap();

    // A build writes into `target/` (ignored by the repository, though it does not exist yet)
    // and into an ignored directory that does; nothing of that is reported, but a source file is.
    for n in 0..300 {
        rig.create(&tree.join(format!("target/debug/deps/f{n}.o")));
        rig.create(&tree.join(format!("ignored/gen/{n}/x")));
    }
    rig.create(&tree.join("src/real.rs"));
    let item = next(&mut changes).await;
    assert_eq!(item.dirs, ["src"], "{item:?}");
    assert!(!item.overflow);
    fx.stop().await;
}

// ----- Which tree a run is served from

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_queued_run_and_a_refused_run_have_no_folder_to_show() {
    let fx = Fixture::start().await;
    let repo = repo(&fx);

    // A write run held in the queue has no worktree yet, and its working directory is the
    // person's own checkout: it is not served as if it were the run's.
    within(fx.client.pause(None, false)).await.unwrap();
    let queued = within(fx.client.run_task(task(&repo, "mock", false)))
        .await
        .unwrap();
    assert_eq!(fx.status(&queued).await, RunStatus::Queued);
    assert_eq!(
        code(fx.client.fs_list(list(&queued, "")).await),
        ErrorCode::WorkspaceUnavailable
    );
    assert_eq!(
        code(fx.client.git_status(queued.clone()).await),
        ErrorCode::WorkspaceUnavailable
    );
    assert_eq!(
        code(fx.client.watch_fs(queued.clone()).await.map(|_| ())),
        ErrorCode::WorkspaceUnavailable
    );
    within(fx.client.resume(None)).await.unwrap();
    fx.events(&queued, None).await;
    // Started, it has its worktree.
    assert!(within(fx.client.fs_list(list(&queued, ""))).await.is_ok());

    // A write run in a folder that is no repository, not allowed in place, is refused before it
    // starts: nothing to show.
    let plain = fx.dir.path().canonicalize().unwrap().join("plain");
    put(&plain, "notes.txt", "x\n");
    let refused = within(fx.client.run_task(task(&plain, "mock", false)))
        .await
        .unwrap();
    fx.events(&refused, None).await;
    assert_eq!(fx.status(&refused).await, RunStatus::Failed);
    assert_eq!(
        code(fx.client.fs_list(list(&refused, "")).await),
        ErrorCode::WorkspaceUnavailable
    );
    fx.stop().await;
}

const TOKEN: &str = "ghp_exampleExampleExample1234";

/// A bot that holds on `mock-hold` and is given the secret `GITHUB_TOKEN`.
async fn write_secret_bot(fx: &Fixture) {
    let mut watch = within(fx.client.watch_defs()).await.unwrap();
    let path = fx.paths.defs_dir.join("bots/keeper.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        "name = \"keeper\"\ninstructions = \"x\"\nexecutors = [\"mock-hold\"]\nsecrets = [\"GITHUB_TOKEN\"]\n",
    )
    .unwrap();
    within(async {
        while let Some(snapshot) = watch.next().await {
            if snapshot
                .unwrap()
                .defs
                .iter()
                .any(|d| d.kind == reno_proto::defs::DefKind::Bot && d.name == "keeper")
            {
                return;
            }
        }
        panic!("the watch ended");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_secret_in_a_worktree_file_is_redacted_in_read_search_and_diff() {
    use renod::secrets::{KeychainSecrets, MemoryKeychain};
    let secrets = Arc::new(KeychainSecrets::new(Arc::new(MemoryKeychain::new())));
    let fx = Fixture::with(|c| c.secrets = Some(secrets)).await;
    write_secret_bot(&fx).await;
    within(fx.client.set_secret("GITHUB_TOKEN", TOKEN))
        .await
        .unwrap();
    let repo = repo(&fx);
    let mut spec = task(&repo, "mock-hold", false);
    spec.executor = None;
    spec.bot = Some("keeper".into());
    let run = within(fx.client.run_task(spec)).await.unwrap();
    until_running(&fx, &run).await;
    let info = within(async {
        loop {
            if let Ok(info) = fx.client.get_workspace(run.clone()).await
                && info.status == reno_proto::WorkspaceStatus::Active
            {
                return info;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    put(
        &info.path,
        "leak.env",
        &format!("GITHUB_TOKEN={TOKEN}\nother=1\n"),
    );

    let file = within(fx.client.fs_read(read(&run, "leak.env")))
        .await
        .unwrap();
    let text = file.text.unwrap();
    assert!(!text.contains(TOKEN), "{text}");
    assert_eq!(text, "GITHUB_TOKEN=[secret:GITHUB_TOKEN]\nother=1\n");

    // git grep searches tracked files, so the file is tracked first.
    git(&info.path, &["add", "leak.env"]);
    let found = within(fx.client.fs_search_content(FsSearchContentParams {
        run_id: run.clone(),
        query: "GITHUB_TOKEN".into(),
        regex: false,
        ignore_case: false,
        limit: None,
    }))
    .await
    .unwrap();
    let hit = found
        .matches
        .iter()
        .find(|m| m.path == "leak.env")
        .expect("a hit");
    assert_eq!(hit.text, "GITHUB_TOKEN=[secret:GITHUB_TOKEN]");
    assert!(!serde_json::to_string(&found).unwrap().contains(TOKEN));

    let diff = within(
        fx.client
            .workspace_diff(run.clone(), Some("leak.env".into())),
    )
    .await
    .unwrap();
    let patch = diff.patch.unwrap();
    assert!(
        patch.contains("[secret:GITHUB_TOKEN]") && !patch.contains(TOKEN),
        "{patch}"
    );
    fx.stop().await;
}
