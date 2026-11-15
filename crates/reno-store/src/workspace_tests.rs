//! Behaviour tests for workspace rows: intent first, one event per change, replacement on a
//! second claim, and what the schema refuses. `now` is a number the test owns.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use reno_proto::{
    ExecutorId, RunId, Seq, TaskId, WorkspaceChanges, WorkspaceInfo, WorkspaceStatus,
};
use rusqlite::Connection;
use serde_json::json;
use tempfile::TempDir;

use crate::{NewTask, Store, StoreError};

fn run_id(n: usize) -> RunId {
    RunId::from_suffix(&format!("r{n}")).unwrap()
}

async fn store_with_runs(n: usize) -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("reno.db")).unwrap();
    let exec: ExecutorId = "mock".parse().unwrap();
    for i in 1..=n {
        let task = TaskId::from_suffix(&format!("t{i}")).unwrap();
        store
            .create_task(
                NewTask {
                    id: task.clone(),
                    goal: "g".into(),
                    class: None,
                    spec: json!({}),
                },
                1,
            )
            .await
            .unwrap();
        store
            .enqueue_run(&task, &run_id(i), &exec, 1)
            .await
            .unwrap();
    }
    (dir, store)
}

fn info(n: usize, path: &str) -> WorkspaceInfo {
    WorkspaceInfo {
        run_id: run_id(n),
        repo: "/repo".into(),
        repo_key: "repo-0123456789ab".into(),
        path: path.into(),
        subdir: "src".into(),
        branch: format!("reno/{}", run_id(n)),
        base_sha: "abc123".into(),
        status: WorkspaceStatus::Active, // ignored: a new row is always `preparing`
        changes: None,
        created_at_ms: 0,
        updated_at_ms: 0,
    }
}

async fn kinds(store: &Store) -> Vec<String> {
    store
        .events_since(Seq(0), 10_000)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .filter(|k| k.starts_with("workspace."))
        .collect()
}

#[tokio::test]
async fn a_workspace_is_recorded_as_preparing_before_anything_else() {
    let (_dir, store) = store_with_runs(1).await;
    store.begin_workspace(info(1, "/w/a"), 10).await.unwrap();
    let got = store.get_workspace(&run_id(1)).await.unwrap().unwrap();
    assert_eq!(got.status, WorkspaceStatus::Preparing);
    assert_eq!(got.path.to_str(), Some("/w/a"));
    assert_eq!(got.subdir, "src");
    assert_eq!((got.created_at_ms, got.updated_at_ms), (10, 10));
    assert_eq!(got.changes, None);
    assert_eq!(kinds(&store).await, ["workspace.preparing"]);
}

#[tokio::test]
async fn each_change_is_one_event_and_a_repeat_writes_nothing() {
    let (_dir, store) = store_with_runs(1).await;
    store.begin_workspace(info(1, "/w/a"), 10).await.unwrap();
    store
        .set_workspace_status(&run_id(1), WorkspaceStatus::Active, None, 11)
        .await
        .unwrap();
    store
        .set_workspace_status(&run_id(1), WorkspaceStatus::Active, None, 12)
        .await
        .unwrap();
    let changes = WorkspaceChanges {
        commits: 2,
        dirty: true,
    };
    let kept = store
        .set_workspace_status(&run_id(1), WorkspaceStatus::Kept, Some(changes), 13)
        .await
        .unwrap();
    assert_eq!(kept.changes, Some(changes));
    assert_eq!(kept.updated_at_ms, 13);
    // Last known changes survive a status change that does not know them.
    store
        .set_workspace_status(&run_id(1), WorkspaceStatus::Lost, None, 14)
        .await
        .unwrap();
    let got = store.get_workspace(&run_id(1)).await.unwrap().unwrap();
    assert_eq!(
        (got.status, got.changes),
        (WorkspaceStatus::Lost, Some(changes))
    );
    assert_eq!(
        kinds(&store).await,
        [
            "workspace.preparing",
            "workspace.active",
            "workspace.kept",
            "workspace.lost"
        ]
    );
}

#[tokio::test]
async fn a_run_claimed_again_replaces_its_row() {
    let (_dir, store) = store_with_runs(1).await;
    store.begin_workspace(info(1, "/w/a"), 10).await.unwrap();
    store
        .set_workspace_status(&run_id(1), WorkspaceStatus::Removed, None, 11)
        .await
        .unwrap();
    store.begin_workspace(info(1, "/w/b"), 20).await.unwrap();
    let got = store.get_workspace(&run_id(1)).await.unwrap().unwrap();
    assert_eq!(
        (got.status, got.path.to_str(), got.created_at_ms),
        (WorkspaceStatus::Preparing, Some("/w/b"), 20)
    );
    assert_eq!(store.list_workspaces(&[]).await.unwrap().len(), 1);
}

#[tokio::test]
async fn listing_filters_by_status_newest_first() {
    let (_dir, store) = store_with_runs(3).await;
    for n in 1..=3 {
        store
            .begin_workspace(info(n, &format!("/w/{n}")), 10 * n as i64)
            .await
            .unwrap();
    }
    store
        .set_workspace_status(&run_id(2), WorkspaceStatus::Kept, None, 50)
        .await
        .unwrap();
    let all: Vec<_> = store
        .list_workspaces(&[])
        .await
        .unwrap()
        .into_iter()
        .map(|w| w.run_id)
        .collect();
    assert_eq!(all, [run_id(3), run_id(2), run_id(1)]);
    let kept = store
        .list_workspaces(&[WorkspaceStatus::Kept])
        .await
        .unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].run_id, run_id(2));
}

#[tokio::test]
async fn unknown_runs_and_workspaces_are_not_found() {
    let (_dir, store) = store_with_runs(1).await;
    assert!(matches!(
        store.begin_workspace(info(9, "/w/x"), 1).await,
        Err(StoreError::NotFound { kind: "run", .. })
    ));
    assert!(matches!(
        store
            .set_workspace_status(&run_id(1), WorkspaceStatus::Active, None, 1)
            .await,
        Err(StoreError::NotFound {
            kind: "workspace",
            ..
        })
    ));
    assert_eq!(store.get_workspace(&run_id(1)).await.unwrap(), None);
}

#[tokio::test]
async fn two_live_workspaces_cannot_share_a_path_but_a_removed_one_frees_it() {
    let (_dir, store) = store_with_runs(2).await;
    store.begin_workspace(info(1, "/w/a"), 10).await.unwrap();
    assert!(store.begin_workspace(info(2, "/w/a"), 11).await.is_err());
    store
        .set_workspace_status(&run_id(1), WorkspaceStatus::Removed, None, 12)
        .await
        .unwrap();
    store.begin_workspace(info(2, "/w/a"), 13).await.unwrap();
}

#[tokio::test]
async fn in_place_runs_leave_an_event_and_no_row() {
    let (_dir, store) = store_with_runs(1).await;
    store
        .record_in_place(&run_id(1), "/some/dir".into(), "read_only", 5)
        .await
        .unwrap();
    assert_eq!(kinds(&store).await, ["workspace.in_place"]);
    assert_eq!(store.get_workspace(&run_id(1)).await.unwrap(), None);
}

#[tokio::test]
async fn the_schema_refuses_half_a_change_record() {
    let (dir, store) = store_with_runs(1).await;
    store.begin_workspace(info(1, "/w/a"), 10).await.unwrap();
    let raw = Connection::open(dir.path().join("reno.db")).unwrap();
    assert!(
        raw.execute("UPDATE workspaces SET commits = 1", [])
            .is_err(),
        "commits without dirty"
    );
    assert!(
        raw.execute("UPDATE workspaces SET status = 'bogus'", [])
            .is_err()
    );
}
