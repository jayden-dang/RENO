//! Behaviour tests for the stored executor health: the latest snapshot per executor survives a
//! reopen, a newer probe replaces an older one, and every write leaves its event.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use reno_proto::{
    ExecutorHealth, ExecutorId, InstallStatus, ModelCatalog, NotInstalledReason, SignedIn,
    VersionGate,
};
use rusqlite::Connection;

use crate::Store;

fn health(at: u64, signed_in: SignedIn) -> ExecutorHealth {
    ExecutorHealth {
        installed: InstallStatus::Installed {
            path: "/bin/claude".into(),
            version: Some("2.1.325".into()),
        },
        version_gate: VersionGate::Ok,
        signed_in,
        models: ModelCatalog::curated(vec![]),
        checked_at_ms: at,
        plan_usage: None,
    }
}

fn id(name: &str) -> ExecutorId {
    name.parse().unwrap()
}

#[tokio::test]
async fn the_latest_health_of_each_executor_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reno.db");
    {
        let store = Store::open(&path).unwrap();
        assert!(store.executor_health().await.unwrap().is_empty());
        store
            .put_executor_health(&id("claude-code"), &health(10, SignedIn::Unknown), 10)
            .await
            .unwrap();
        store
            .put_executor_health(
                &id("codex"),
                &ExecutorHealth {
                    installed: InstallStatus::NotInstalled {
                        why: NotInstalledReason::NotFound { hint: "x".into() },
                    },
                    version_gate: VersionGate::Unchecked,
                    signed_in: SignedIn::Unknown,
                    models: ModelCatalog::curated(vec![]),
                    checked_at_ms: 11,
                    plan_usage: None,
                },
                11,
            )
            .await
            .unwrap();
        // A newer probe of the same executor replaces the older.
        store
            .put_executor_health(
                &id("claude-code"),
                &health(20, SignedIn::Yes { account: None }),
                20,
            )
            .await
            .unwrap();
    }
    let store = Store::open(&path).unwrap();
    let stored = store.executor_health().await.unwrap();
    let ids: Vec<&str> = stored.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(ids, ["claude-code", "codex"]);
    assert_eq!(stored[0].1, health(20, SignedIn::Yes { account: None }));
    assert_eq!(stored[1].1.checked_at_ms, 11);
}

#[tokio::test]
async fn every_write_leaves_one_event_for_its_executor() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("reno.db")).unwrap();
    store
        .put_executor_health(&id("codex"), &health(5, SignedIn::Unknown), 5)
        .await
        .unwrap();
    store
        .put_executor_health(&id("codex"), &health(6, SignedIn::Unknown), 6)
        .await
        .unwrap();
    let events: Vec<_> = store
        .events_since(reno_proto::Seq(0), 100)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.subject_type == "executor" && e.subject_id == "codex")
        .collect();
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|e| e.kind == "executor.health"));
}

#[test]
fn the_table_refuses_a_row_that_is_not_json() {
    let mut conn = Connection::open_in_memory().unwrap();
    crate::migrations::run(&mut conn).unwrap();
    let error = conn
        .execute(
            "INSERT INTO executor_health (executor, health, checked_at) VALUES ('x', 'nope', 1)",
            [],
        )
        .unwrap_err();
    assert!(error.to_string().contains("CHECK"), "{error}");
}
