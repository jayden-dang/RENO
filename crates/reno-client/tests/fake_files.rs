//! The fake daemon's Files tab: a small in-memory tree, a fixed git status and a watch a test
//! drives. No real file system.

#![cfg(feature = "fake")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use futures::StreamExt;
use reno_client::fake::{FakeConfig, FakeDaemon};
use reno_proto::{
    ErrorCode, FEATURE_FILES, FsChange, FsEntryKind, FsListParams, FsReadImageParams,
    FsReadParams, FsSearchContentParams, FsSearchFilesParams, GitChange, LineEnding, RunId,
};

fn run(name: &str) -> RunId {
    RunId::from_suffix(name).unwrap()
}

fn list(run_id: &str, dir: &str) -> FsListParams {
    FsListParams {
        run_id: run(run_id),
        dir: dir.into(),
        cursor: None,
        limit: None,
    }
}

fn read(run_id: &str, path: &str) -> FsReadParams {
    FsReadParams {
        run_id: run(run_id),
        path: path.into(),
    }
}

#[tokio::test(start_paused = true)]
async fn the_fake_lists_reads_and_pages_its_tree() {
    let client = FakeDaemon::spawn().await.unwrap();
    assert!(client.supports(FEATURE_FILES));

    let root = client.fs_list(list("builder1", "")).await.unwrap();
    let names: Vec<&str> = root.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(
        names,
        ["assets", "docs", "many", "src", "Cargo.toml", "notes.txt", "README.md"]
    );
    assert!(!root.in_place);
    assert_eq!(root.entries[0].kind, FsEntryKind::Dir);

    // `many/` has more than a page.
    let mut seen = 0;
    let mut cursor = None;
    let mut pages = 0;
    loop {
        let page = client
            .fs_list(FsListParams {
                cursor: cursor.take(),
                ..list("builder1", "many")
            })
            .await
            .unwrap();
        seen += page.entries.len();
        pages += 1;
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!((pages, seen), (2, reno_client::fake::files::MANY));

    let readme = client.fs_read(read("builder1", "README.md")).await.unwrap();
    assert!(readme.text.unwrap().starts_with("# Demo"));
    let guide = client
        .fs_read(read("builder1", "docs/guide.md"))
        .await
        .unwrap();
    assert_eq!(guide.line_ending, LineEnding::Crlf);
    let blob = client
        .fs_read(read("builder1", "assets/blob.bin"))
        .await
        .unwrap();
    assert!(blob.binary && blob.text.is_none());

    let (mime, bytes) = client
        .fs_read_image_all(run("builder1"), "assets/logo.png".into())
        .await
        .unwrap();
    assert_eq!((mime.as_str(), bytes.len()), ("image/png", 128));
    let chunk = client
        .fs_read_image(FsReadImageParams {
            run_id: run("builder1"),
            path: "assets/logo.png".into(),
            offset: 0,
        })
        .await
        .unwrap();
    assert!(chunk.eof);
}

#[tokio::test(start_paused = true)]
async fn the_fake_searches_and_reports_a_status() {
    let client = FakeDaemon::spawn().await.unwrap();
    let found = client
        .fs_search_files(FsSearchFilesParams {
            run_id: run("builder1"),
            query: "lib".into(),
            limit: None,
        })
        .await
        .unwrap();
    assert_eq!(found.files[0].path, "src/lib.rs");
    let hits = client
        .fs_search_content(FsSearchContentParams {
            run_id: run("builder1"),
            query: "alpha".into(),
            regex: false,
            ignore_case: true,
            limit: Some(1),
        })
        .await
        .unwrap();
    assert_eq!(hits.matches.len(), 1);
    assert!(hits.truncated);

    let status = client.git_status(run("builder1")).await.unwrap();
    assert!(status.repo);
    assert_eq!(status.branch.as_deref(), Some("reno/builder1"));
    for change in [
        GitChange::Added,
        GitChange::Renamed,
        GitChange::Untracked,
        GitChange::Deleted,
        GitChange::Unmodified,
    ] {
        assert!(
            status
                .files
                .iter()
                .any(|f| f.index == change || f.worktree == change),
            "{change:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn the_fake_refuses_bad_paths_serves_in_place_and_reports_a_removed_workspace() {
    let client = FakeDaemon::spawn().await.unwrap();
    for bad in ["../x", "/etc/passwd", ".git/config", "src/../../x"] {
        let error = client.fs_read(read("builder1", bad)).await.unwrap_err();
        assert_eq!(error.code(), Some(ErrorCode::PathRejected), "{bad}");
    }
    assert_eq!(
        client
            .fs_read(read("builder1", "nope"))
            .await
            .unwrap_err()
            .code(),
        Some(ErrorCode::NotFound)
    );

    // No worktree of its own: served from its folder, flagged, and no repository.
    let listed = client.fs_list(list("reviewer1", "")).await.unwrap();
    assert!(listed.in_place);
    let status = client.git_status(run("reviewer1")).await.unwrap();
    assert!(status.in_place && !status.repo && status.files.is_empty());

    // Its worktree was removed.
    let gone = client.fs_list(list("builder0", "")).await.unwrap_err();
    assert_eq!(gone.code(), Some(ErrorCode::WorkspaceUnavailable));
    let nobody = client.fs_list(list("nobody", "")).await.unwrap_err();
    assert_eq!(nobody.code(), Some(ErrorCode::NotFound));
}

#[tokio::test(start_paused = true)]
async fn the_fake_watch_delivers_what_a_test_pushes_and_counts_open_subscriptions() {
    let daemon = FakeDaemon::start(FakeConfig::default()).unwrap();
    let client = daemon.connect().await.unwrap();
    assert_eq!(daemon.fs_watches(), 0);

    let mut changes = client.watch_fs(run("builder1")).await.unwrap();
    // The subscription is open once the call returned.
    assert_eq!(daemon.fs_watches(), 1);
    daemon.push_fs_change(
        &run("keeper1"),
        FsChange {
            dirs: vec!["other".into()],
            ..FsChange::default()
        },
    );
    daemon.push_fs_change(
        &run("builder1"),
        FsChange {
            dirs: vec![String::new(), "src".into()],
            git: true,
            ..FsChange::default()
        },
    );
    let got = changes.next().await.unwrap().unwrap();
    assert_eq!(got.dirs, ["", "src"]);
    assert!(got.git);

    drop(changes);
    // The daemon notices the unsubscribe on its next turn.
    for _ in 0..50 {
        if daemon.fs_watches() == 0 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(daemon.fs_watches(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_client_refuses_up_front_on_a_daemon_without_the_files_feature() {
    let client = FakeDaemon::spawn_with(FakeConfig {
        without_features: vec![FEATURE_FILES.to_owned()],
        ..FakeConfig::default()
    })
    .await
    .unwrap();
    assert!(!client.supports(FEATURE_FILES));
    let refused = client.fs_list(list("builder1", "")).await.unwrap_err();
    assert!(refused.to_string().contains("files"), "{refused}");
    assert!(client.git_status(run("builder1")).await.is_err());
    assert!(client.watch_fs(run("builder1")).await.is_err());
}
