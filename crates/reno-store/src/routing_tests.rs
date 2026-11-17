//! Behaviour tests for the stored routing decisions: written with the run, replaced by a fenced
//! fallback, and every write leaves its event.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use reno_proto::{
    ExecutorId, RejectReason, Rejection, RoutingDecision, RuleMatched, RunId, Seq, TaskId,
};
use rusqlite::Connection;

use crate::{NewTask, Store, StoreError};

fn id(name: &str) -> ExecutorId {
    name.parse().unwrap()
}

fn decision(chosen: &str, at: u64) -> RoutingDecision {
    RoutingDecision {
        chosen: Some(id(chosen)),
        ranked: vec![id(chosen)],
        rejected: vec![Rejection {
            also: Vec::new(),
            executor: id("claude-code"),
            reason: RejectReason::SignedOut {
                hint: "run `claude login`".into(),
            },
        }],
        rule_matched: RuleMatched::Default,
        fallbacks: vec![],
        at_ms: at,
        warnings: Vec::new(),
    }
}

async fn seeded() -> (tempfile::TempDir, Store, TaskId, RunId) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("reno.db")).unwrap();
    let task: TaskId = "task_a".parse().unwrap();
    let run: RunId = "run_a".parse().unwrap();
    store
        .create_task(
            NewTask {
                id: task.clone(),
                goal: "g".into(),
                class: None,
                spec: serde_json::json!({}),
            },
            1,
        )
        .await
        .unwrap();
    (dir, store, task, run)
}

#[tokio::test]
async fn a_run_stores_the_decision_it_was_enqueued_with_and_survives_a_reopen() {
    let (dir, store, task, run) = seeded().await;
    assert_eq!(store.run_routing(&run).await.unwrap(), None);
    store
        .enqueue_routed_run(&task, &run, &id("codex"), None, &decision("codex", 5), 5)
        .await
        .unwrap();
    assert_eq!(
        store.run_routing(&run).await.unwrap(),
        Some(decision("codex", 5))
    );
    drop(store);
    let store = Store::open(dir.path().join("reno.db")).unwrap();
    assert_eq!(
        store.run_routing(&run).await.unwrap(),
        Some(decision("codex", 5))
    );
    let kinds: Vec<String> = store
        .events_since(Seq(0), 100)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(kinds.contains(&"run.routed".to_owned()), "{kinds:?}");
}

#[tokio::test]
async fn a_fallback_moves_the_run_under_its_fence_and_replaces_the_decision() {
    let (_dir, store, task, run) = seeded().await;
    let first = decision("claude-code", 5);
    store
        .enqueue_routed_run(&task, &run, &id("claude-code"), None, &first, 5)
        .await
        .unwrap();
    let claim = store.claim_next("o", 6, 60_000).await.unwrap().unwrap();

    let second = decision("codex", 7);
    let stale = store
        .reroute_run(&run, claim.fence + 1, &id("codex"), &second, 7)
        .await;
    assert!(
        matches!(stale, Err(StoreError::StaleFence { .. })),
        "{stale:?}"
    );
    let row = store.get_run(&run).await.unwrap().unwrap();
    assert_eq!(row.executor, id("claude-code"));

    store
        .reroute_run(&run, claim.fence, &id("codex"), &second, 7)
        .await
        .unwrap();
    let row = store.get_run(&run).await.unwrap().unwrap();
    assert_eq!(row.executor, id("codex"));
    assert_eq!(store.run_routing(&run).await.unwrap(), Some(second));
}

#[test]
fn migration_nine_adds_an_empty_run_routing_table() {
    let mut conn = Connection::open_in_memory().unwrap();
    crate::migrations::migrations()
        .to_version(&mut conn, 8)
        .unwrap();
    conn.execute_batch(
        "INSERT INTO tasks (id, goal, created_at) VALUES ('task_a', 'g', 1);
         INSERT INTO runs (id, task_id, executor, status, created_at)
         VALUES ('run_a', 'task_a', 'mock', 'queued', 2);",
    )
    .unwrap();
    assert!(conn.prepare("SELECT 1 FROM run_routing").is_err());
    crate::migrations::run(&mut conn).unwrap();
    let (runs, routing): (i64, i64) = conn
        .query_row(
            "SELECT (SELECT count(*) FROM runs), (SELECT count(*) FROM run_routing)",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!((runs, routing), (1, 0));
}
