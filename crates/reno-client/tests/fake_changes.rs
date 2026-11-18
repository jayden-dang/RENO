//! The fake daemon's `workspace.get`, `workspace.diff` and tool output.

#![cfg(feature = "fake")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use futures::StreamExt;
use reno_client::fake::FakeDaemon;
use reno_proto::{ChangeKind, ErrorCode, ExecEvent, RunId, WorkspaceStatus};

fn run(name: &str) -> RunId {
    RunId::from_suffix(name).unwrap()
}

#[tokio::test(start_paused = true)]
async fn the_fake_daemon_says_what_a_run_changed() {
    let client = FakeDaemon::spawn().await.unwrap();
    assert!(client.supports(reno_proto::FEATURE_RUN_CHANGES));

    // The list alone has no patch; a path brings the patch of that file.
    let all = client.workspace_diff(run("builder1"), None).await.unwrap();
    assert_eq!(all.branch, "reno/builder1");
    assert_eq!(all.commits, 3);
    assert_eq!(all.patch, None);
    let kinds: Vec<_> = all.files.iter().map(|f| f.kind).collect();
    for kind in [
        ChangeKind::Added,
        ChangeKind::Modified,
        ChangeKind::Deleted,
        ChangeKind::Renamed,
    ] {
        assert!(kinds.contains(&kind), "{kind:?}");
    }
    assert!(all.files.iter().any(|f| f.binary));
    let renamed = all
        .files
        .iter()
        .find(|f| f.kind == ChangeKind::Renamed)
        .unwrap();
    assert!(renamed.from.is_some());

    let one = client
        .workspace_diff(
            run("builder1"),
            Some("crates/reno-kernel/src/lease.rs".into()),
        )
        .await
        .unwrap();
    assert!(one.patch.as_deref().unwrap().contains("@@ -10,5 +10,8 @@"));
    let binary = client
        .workspace_diff(run("builder1"), Some("assets/logo.png".into()))
        .await
        .unwrap();
    assert_eq!(binary.patch, None);

    let info = client.get_workspace(run("builder1")).await.unwrap();
    assert_eq!(info.branch, "reno/builder1");
    assert_eq!(info.changes.unwrap().commits, 3);

    // No changes, a removed workspace, and a run that never had one.
    let empty = client.workspace_diff(run("scout1"), None).await.unwrap();
    assert!(empty.files.is_empty());
    let removed = client.get_workspace(run("builder0")).await.unwrap();
    assert_eq!(removed.status, WorkspaceStatus::Removed);
    let none = client.get_workspace(run("reviewer1")).await.unwrap_err();
    assert_eq!(none.code(), Some(ErrorCode::NotFound));
}

#[tokio::test(start_paused = true)]
async fn an_older_daemon_has_no_workspace_diff() {
    let daemon = reno_client::fake::FakeDaemon::start(reno_client::fake::FakeConfig {
        without_features: vec![reno_proto::FEATURE_RUN_CHANGES.to_owned()],
        ..reno_client::fake::FakeConfig::default()
    })
    .unwrap();
    let client = daemon.connect().await.unwrap();
    assert!(!client.supports(reno_proto::FEATURE_RUN_CHANGES));
    assert!(client.workspace_diff(run("builder1"), None).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn fake_tool_results_carry_output() {
    let client = FakeDaemon::spawn().await.unwrap();
    let journal: Vec<_> = client
        .run_events(run("builder0"), None)
        .map(|item| item.unwrap().event)
        .collect()
        .await;
    let failed = journal
        .iter()
        .find_map(|e| match e {
            ExecEvent::ToolResult {
                ok: false,
                output: Some(out),
                ..
            } => Some(out.clone()),
            _ => None,
        })
        .expect("a failing tool with output");
    assert!(failed.truncated);
    assert!(failed.bytes as usize > failed.text.len());
    assert!(failed.text.contains("FAILED"));
}
