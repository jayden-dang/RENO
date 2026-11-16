//! Behaviour tests for run tokens and boards: what is stored, what is refused, and what the
//! event log is allowed to see.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use reno_proto::{
    BOARD_KEY_MAX_BYTES, BOARD_MAX_KEYS, BOARD_VALUE_MAX_BYTES, ExecutorId, RunId, Seq, TaskId,
    token_digest,
};
use serde_json::json;
use tempfile::TempDir;

use crate::{BoardWrite, NewRunToken, NewTask, Store, StoreError};

fn run_id(n: usize) -> RunId {
    RunId::from_suffix(&format!("r{n}")).unwrap()
}

async fn store_with_runs(n: usize) -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("reno.db")).unwrap();
    let exec: ExecutorId = "mock".parse().unwrap();
    for i in 1..=n {
        let task = TaskId::from_suffix(&format!("t{i}")).unwrap();
        let new = NewTask {
            id: task.clone(),
            goal: "g".into(),
            class: None,
            spec: json!({}),
        };
        store.create_task(new, 1).await.unwrap();
        store
            .enqueue_run(&task, &run_id(i), &exec, 1)
            .await
            .unwrap();
    }
    (dir, store)
}

fn token(run: usize, secret: &str, expires_at: i64) -> NewRunToken {
    NewRunToken {
        token_hash: token_digest(secret),
        run_id: run_id(run),
        bot: Some("keeper".into()),
        scopes: vec!["bot:keeper".into()],
        expires_at,
    }
}

#[tokio::test]
async fn a_token_is_found_by_its_digest_under_its_own_run_only() {
    let (_dir, store) = store_with_runs(2).await;
    store
        .put_run_token(token(1, "rnt_one", 1_000), 10)
        .await
        .unwrap();

    let found = store
        .run_token(&run_id(1), &token_digest("rnt_one"))
        .await
        .unwrap()
        .expect("found");
    assert_eq!(found.bot.as_deref(), Some("keeper"));
    assert_eq!(found.scopes, ["bot:keeper"]);
    assert_eq!((found.expires_at, found.revoked_at), (1_000, None));

    // The right token under the wrong run, and a wrong token under the right run.
    assert!(
        store
            .run_token(&run_id(2), &token_digest("rnt_one"))
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .run_token(&run_id(1), &token_digest("rnt_two"))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn revoking_marks_every_open_token_of_the_run_and_only_that_run() {
    let (_dir, store) = store_with_runs(2).await;
    store.put_run_token(token(1, "a", 1_000), 10).await.unwrap();
    store.put_run_token(token(1, "b", 1_000), 10).await.unwrap();
    store.put_run_token(token(2, "c", 1_000), 10).await.unwrap();

    assert_eq!(store.revoke_run_tokens(&run_id(1), 20).await.unwrap(), 2);
    assert_eq!(store.revoke_run_tokens(&run_id(1), 30).await.unwrap(), 0);

    let revoked_at = |secret: &'static str, run: usize| {
        let store = store.clone();
        async move {
            store
                .run_token(&run_id(run), &token_digest(secret))
                .await
                .unwrap()
                .unwrap()
                .revoked_at
        }
    };
    assert_eq!(revoked_at("a", 1).await, Some(20));
    assert_eq!(revoked_at("b", 1).await, Some(20));
    assert_eq!(revoked_at("c", 2).await, None);
}

#[tokio::test]
async fn starting_revokes_every_open_token() {
    let (_dir, store) = store_with_runs(2).await;
    store.put_run_token(token(1, "a", 1_000), 10).await.unwrap();
    store.put_run_token(token(2, "b", 1_000), 10).await.unwrap();
    store.revoke_run_tokens(&run_id(1), 15).await.unwrap();

    let runs = store.revoke_open_run_tokens(20).await.unwrap();
    assert_eq!(runs, [run_id(2)]);
    assert!(
        store
            .run_token(&run_id(2), &token_digest("b"))
            .await
            .unwrap()
            .unwrap()
            .revoked_at
            .is_some()
    );
    assert!(store.revoke_open_run_tokens(30).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_token_for_a_run_that_does_not_exist_is_refused() {
    let (_dir, store) = store_with_runs(1).await;
    let error = store
        .put_run_token(token(9, "a", 1_000), 10)
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::NotFound { kind: "run", .. }));
}

#[tokio::test]
async fn neither_the_token_nor_its_digest_reaches_the_event_log() {
    let (_dir, store) = store_with_runs(1).await;
    store
        .put_run_token(token(1, "rnt_secret", 1_000), 10)
        .await
        .unwrap();
    store.revoke_run_tokens(&run_id(1), 20).await.unwrap();
    let events = store.events_since(Seq(0), 1_000).await.unwrap();
    let text = serde_json::to_string(
        &events
            .iter()
            .map(|e| (&e.kind, &e.data))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(text.contains("run.token_issued") && text.contains("run.token_revoked"));
    assert!(!text.contains("rnt_secret"));
    assert!(!text.contains(&token_digest("rnt_secret")));
}

#[tokio::test]
async fn board_writes_are_versioned_and_a_stale_one_changes_nothing() {
    let (_dir, store) = store_with_runs(1).await;
    let run = run_id(1);
    let put = |value: &'static str, expected: Option<u64>| {
        let (store, run) = (store.clone(), run.clone());
        async move {
            store
                .board_put("bot:keeper", "plan", value, expected, &run, 5)
                .await
                .unwrap()
        }
    };

    // The key does not exist: expecting version 0 creates it, expecting 0 again does not.
    assert_eq!(put("v1", Some(0)).await, BoardWrite::Written { version: 1 });
    assert!(matches!(
        put("x", Some(0)).await,
        BoardWrite::Stale { current: Some(row) } if row.value == "v1" && row.version == 1
    ));
    assert_eq!(put("v2", Some(1)).await, BoardWrite::Written { version: 2 });
    // No expectation: always written.
    assert_eq!(put("v3", None).await, BoardWrite::Written { version: 3 });

    let row = store
        .board_get("bot:keeper", "plan")
        .await
        .unwrap()
        .unwrap();
    assert_eq!((row.value.as_str(), row.version, row.size), ("v3", 3, 2));
    assert!(matches!(
        store
            .board_put("bot:keeper", "other", "x", Some(4), &run, 5)
            .await
            .unwrap(),
        BoardWrite::Stale { current: None }
    ));
}

#[tokio::test]
async fn scopes_do_not_see_each_other() {
    let (_dir, store) = store_with_runs(1).await;
    let run = run_id(1);
    store
        .board_put("bot:a", "k", "from a", None, &run, 5)
        .await
        .unwrap();
    store
        .board_put("bot:b", "k", "from b", None, &run, 5)
        .await
        .unwrap();
    let a = store.board_get("bot:a", "k").await.unwrap().unwrap();
    let b = store.board_get("bot:b", "k").await.unwrap().unwrap();
    assert_eq!((a.value.as_str(), b.value.as_str()), ("from a", "from b"));
    assert_eq!(store.board_list("bot:a").await.unwrap().len(), 1);
    assert!(store.board_list("bot:c").await.unwrap().is_empty());
}

#[tokio::test]
async fn limits_are_enforced() {
    let (_dir, store) = store_with_runs(1).await;
    let run = run_id(1);
    let too_big = "x".repeat(BOARD_VALUE_MAX_BYTES + 1);
    assert!(matches!(
        store.board_put("s", "k", &too_big, None, &run, 5).await,
        Err(StoreError::InvalidArgument(_))
    ));
    let at_limit = "x".repeat(BOARD_VALUE_MAX_BYTES);
    store
        .board_put("s", "k", &at_limit, None, &run, 5)
        .await
        .unwrap();
    let long_key = "k".repeat(BOARD_KEY_MAX_BYTES + 1);
    assert!(matches!(
        store.board_put("s", &long_key, "v", None, &run, 5).await,
        Err(StoreError::InvalidArgument(_))
    ));
    assert!(matches!(
        store.board_put("s", "", "v", None, &run, 5).await,
        Err(StoreError::InvalidArgument(_))
    ));

    // Fill the scope: a new key is refused, an existing one still takes writes.
    for n in 1..BOARD_MAX_KEYS {
        store
            .board_put("s", &format!("key{n}"), "v", None, &run, 5)
            .await
            .unwrap();
    }
    assert_eq!(
        store
            .board_put("s", "one-more", "v", None, &run, 5)
            .await
            .unwrap(),
        BoardWrite::Full
    );
    assert_eq!(
        store
            .board_put("s", "key1", "w", None, &run, 5)
            .await
            .unwrap(),
        BoardWrite::Written { version: 2 }
    );
}

#[tokio::test]
async fn a_board_event_carries_the_digest_and_never_the_value() {
    let (_dir, store) = store_with_runs(1).await;
    store
        .board_put("bot:keeper", "k", "top secret value", None, &run_id(1), 5)
        .await
        .unwrap();
    let events = store.events_since(Seq(0), 1_000).await.unwrap();
    let event = events.iter().find(|e| e.kind == "board.written").unwrap();
    assert_eq!(event.subject_id, "bot:keeper");
    assert_eq!(event.data["key"], "k");
    assert_eq!(event.data["version"], 1);
    assert_eq!(event.data["size"], 16);
    assert_eq!(event.data["digest"], token_digest("top secret value"));
    assert!(!event.data.to_string().contains("top secret"));
}
