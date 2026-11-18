//! The fake daemon's History: a small deterministic log with a merge, tags and refs, and the two
//! sides of the demo diff's files. No real `git`.

#![cfg(feature = "fake")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use reno_client::fake::{FakeConfig, FakeDaemon};
use reno_proto::{
    DiffScope, ErrorCode, FEATURE_HISTORY, FileSourcesParams, GitLogParams, GitLogRev,
    GitLogSearchParams, GitRefKind, RunId,
};

fn run(name: &str) -> RunId {
    RunId::from_suffix(name).unwrap()
}

fn log(run_id: &str) -> GitLogParams {
    GitLogParams {
        run_id: run(run_id),
        cursor: None,
        limit: None,
        rev: None,
    }
}

fn sources(run_id: &str, path: &str, scope: Option<DiffScope>) -> FileSourcesParams {
    FileSourcesParams {
        run_id: run(run_id),
        path: path.into(),
        scope,
        checksum: None,
    }
}

#[tokio::test(start_paused = true)]
async fn the_fake_serves_a_log_with_a_merge_tags_and_refs_and_pages_it() {
    let client = FakeDaemon::spawn().await.unwrap();
    assert!(client.supports(FEATURE_HISTORY));

    let page = client.git_log(log("builder1")).await.unwrap();
    assert!(page.repo && !page.in_place && page.next_cursor.is_none());
    assert_eq!(page.commits.len(), 6);
    assert_eq!(page.head.as_deref(), Some(page.commits[0].sha.as_str()));
    // A merge, first; every commit before its parents.
    assert_eq!(page.commits[0].parents.len(), 2);
    for (n, commit) in page.commits.iter().enumerate() {
        for parent in &commit.parents {
            let at = page.commits.iter().position(|c| &c.sha == parent).unwrap();
            assert!(n < at);
        }
    }
    assert!(page.commits.last().unwrap().parents.is_empty());
    let kinds: Vec<GitRefKind> = page
        .commits
        .iter()
        .flat_map(|c| c.refs.iter().map(|r| r.kind))
        .collect();
    for kind in [
        GitRefKind::Head,
        GitRefKind::Branch,
        GitRefKind::Tag,
        GitRefKind::Remote,
    ] {
        assert!(kinds.contains(&kind), "{kind:?}");
    }
    assert!(!serde_json::to_string(&page).unwrap().contains('@'));

    // Paged by cursor, the same commits.
    let mut seen = Vec::new();
    let mut params = GitLogParams {
        limit: Some(4),
        ..log("builder1")
    };
    loop {
        let one = client.git_log(params.clone()).await.unwrap();
        seen.extend(one.commits);
        match one.next_cursor {
            Some(next) => params.cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(seen, page.commits);
    params.cursor = Some("junk".into());
    let bad = client.git_log(params).await.unwrap_err();
    assert_eq!(bad.code(), Some(ErrorCode::InvalidParams));

    // The thread's own commits, and a follow-up run's alone.
    let thread = client
        .git_log(GitLogParams {
            rev: Some(GitLogRev::Thread),
            ..log("builder1")
        })
        .await
        .unwrap();
    assert_eq!(thread.commits.len(), 3);
}

#[tokio::test(start_paused = true)]
async fn the_fake_searches_and_answers_for_each_kind_of_run() {
    let client = FakeDaemon::spawn().await.unwrap();
    let found = client
        .git_log_search(GitLogSearchParams {
            run_id: run("builder1"),
            query: "LEASE".into(),
            limit: None,
            rev: None,
        })
        .await
        .unwrap();
    assert_eq!(found.commits.len(), 3);
    assert!(!found.truncated);
    let by_author = client
        .git_log_search(GitLogSearchParams {
            run_id: run("builder1"),
            query: "ada".into(),
            limit: Some(1),
            rev: None,
        })
        .await
        .unwrap();
    assert!(by_author.truncated && by_author.commits.len() == 1);
    let empty = client
        .git_log_search(GitLogSearchParams {
            run_id: run("builder1"),
            query: " ".into(),
            limit: None,
            rev: None,
        })
        .await
        .unwrap_err();
    assert_eq!(empty.code(), Some(ErrorCode::InvalidParams));

    // No worktree: served from its folder, no repository, and no scope.
    let plain = client.git_log(log("reviewer1")).await.unwrap();
    assert!(!plain.repo && plain.in_place && plain.commits.is_empty());
    let scoped = client
        .git_log(GitLogParams {
            rev: Some(GitLogRev::Thread),
            ..log("reviewer1")
        })
        .await
        .unwrap_err();
    assert_eq!(scoped.code(), Some(ErrorCode::InvalidParams));
    // Removed, and unknown.
    let gone = client.git_log(log("builder0")).await.unwrap_err();
    assert_eq!(gone.code(), Some(ErrorCode::WorkspaceUnavailable));
    let nobody = client.git_log(log("nobody")).await.unwrap_err();
    assert_eq!(nobody.code(), Some(ErrorCode::NotFound));
}

#[tokio::test(start_paused = true)]
async fn the_fake_gives_both_sides_of_the_diffs_files_and_says_when_stale() {
    let client = FakeDaemon::spawn().await.unwrap();
    let lease = "crates/reno-kernel/src/lease.rs";
    let diff = client
        .workspace_diff_scoped(run("builder1"), Some(lease.into()), DiffScope::Thread)
        .await
        .unwrap();
    assert!(!diff.checksum.is_empty());
    let both = client
        .workspace_file_sources(FileSourcesParams {
            checksum: Some(diff.checksum.clone()),
            ..sources("builder1", lease, None)
        })
        .await
        .unwrap();
    assert!(both.old_text.is_some() && both.new_text.is_some() && !both.stale);
    assert_eq!(both.checksum, diff.checksum);
    let stale = client
        .workspace_file_sources(FileSourcesParams {
            checksum: Some("old".into()),
            ..sources("builder1", lease, None)
        })
        .await
        .unwrap();
    assert!(stale.stale);

    // Added, deleted, renamed, binary.
    let added = client
        .workspace_file_sources(sources(
            "builder1",
            "crates/reno-kernel/src/lease_tests.rs",
            None,
        ))
        .await
        .unwrap();
    assert!(added.old_text.is_none() && added.new_text.is_some());
    let deleted = client
        .workspace_file_sources(sources("builder1", "docs/old-notes.md", None))
        .await
        .unwrap();
    assert!(deleted.old_text.is_some() && deleted.new_text.is_none());
    let renamed = client
        .workspace_file_sources(sources("builder1", "apps/renod/src/main.rs", None))
        .await
        .unwrap();
    assert_eq!(renamed.old_path.as_deref(), Some("apps/renod/src/start.rs"));
    let binary = client
        .workspace_file_sources(sources("builder1", "assets/logo.png", None))
        .await
        .unwrap();
    assert!(binary.binary && binary.old_text.is_none() && binary.new_text.is_none());

    // The working tree has its own files and a checksum that differs from the thread's.
    let tree = client
        .workspace_diff_scoped(run("builder1"), None, DiffScope::WorkingTree)
        .await
        .unwrap();
    assert_eq!((tree.files.len(), tree.commits), (2, 0));
    let thread = client
        .workspace_diff_scoped(run("builder1"), None, DiffScope::Thread)
        .await
        .unwrap();
    assert_ne!(tree.checksum, thread.checksum);
    let missing = client
        .workspace_file_sources(sources(
            "builder1",
            "Cargo.toml",
            Some(DiffScope::WorkingTree),
        ))
        .await
        .unwrap_err();
    assert_eq!(missing.code(), Some(ErrorCode::NotFound));

    // A run with no worktree has no diff; a removed one is unavailable.
    let none = client
        .workspace_file_sources(sources("reviewer1", "a", None))
        .await
        .unwrap_err();
    assert_eq!(none.code(), Some(ErrorCode::WorkspaceUnavailable));
    let gone = client
        .workspace_file_sources(sources("builder0", "a", None))
        .await
        .unwrap_err();
    assert_eq!(gone.code(), Some(ErrorCode::WorkspaceUnavailable));
}

#[tokio::test(start_paused = true)]
async fn a_client_refuses_up_front_on_a_daemon_without_the_history_feature() {
    let client = FakeDaemon::spawn_with(FakeConfig {
        without_features: vec![FEATURE_HISTORY.to_owned()],
        ..FakeConfig::default()
    })
    .await
    .unwrap();
    assert!(!client.supports(FEATURE_HISTORY));
    let refused = client.git_log(log("builder1")).await.unwrap_err();
    assert!(refused.to_string().contains("history"), "{refused}");
    assert!(
        client
            .git_log_search(GitLogSearchParams {
                run_id: run("builder1"),
                query: "a".into(),
                limit: None,
                rev: None
            })
            .await
            .is_err()
    );
    assert!(
        client
            .workspace_file_sources(sources("builder1", "Cargo.toml", None))
            .await
            .is_err()
    );
    // The working-tree scope is the new part of `workspace.diff`: refused up front too, while the
    // older scopes still work.
    assert!(
        client
            .workspace_diff_scoped(run("builder1"), None, DiffScope::WorkingTree)
            .await
            .is_err()
    );
    assert!(
        client
            .workspace_diff_scoped(run("builder1"), None, DiffScope::Thread)
            .await
            .is_ok()
    );
}
