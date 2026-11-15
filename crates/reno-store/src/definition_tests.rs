//! Behaviour tests for stored definitions and for the bot pin on runs. `now` is a number the
//! test owns.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use reno_proto::defs::{BotPin, DefKind, content_hash};
use reno_proto::{ExecutorId, RunId, Seq, TaskId};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::definitions::{DefStatus, NewDefinition};
use crate::error::DbResult;
use crate::writer::Durability;
use crate::{NewTask, Store, StoreError};

fn open() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("reno.db")).unwrap();
    (dir, store)
}

fn body(text: &str) -> Value {
    json!({ "name": "keeper", "instructions": text, "executors": ["codex"] })
}

fn new_def(kind: DefKind, name: &str, text: &str) -> NewDefinition {
    let body = json!({ "name": name, "instructions": text });
    NewDefinition {
        kind,
        name: name.to_owned(),
        hash: content_hash(&body),
        body,
    }
}

async fn put(store: &Store, text: &str, now: i64) -> (u32, bool) {
    let body = body(text);
    store
        .put_definition(
            DefKind::Bot,
            "keeper",
            &content_hash(&body),
            &body,
            Some("test"),
            now,
        )
        .await
        .unwrap()
}

async fn kinds(store: &Store) -> Vec<String> {
    store
        .events_since(Seq(0), 1000)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect()
}

#[tokio::test]
async fn the_same_content_is_one_version() {
    let (_d, store) = open();
    assert_eq!(put(&store, "a", 1).await, (1, true));
    assert_eq!(put(&store, "a", 2).await, (1, false));
    assert_eq!(put(&store, "a", 3).await, (1, false));
    assert_eq!(kinds(&store).await, ["definition.created"]);
}

#[tokio::test]
async fn changed_content_is_the_next_version_and_going_back_is_a_new_one() {
    let (_d, store) = open();
    assert_eq!(put(&store, "a", 1).await, (1, true));
    assert_eq!(put(&store, "b", 2).await, (2, true));
    // Back to the first content: version 3, not "version 1 again".
    assert_eq!(put(&store, "a", 3).await, (3, true));
    assert_eq!(put(&store, "a", 4).await, (3, false));

    let v1 = store
        .get_definition(DefKind::Bot, "keeper", 1)
        .await
        .unwrap()
        .unwrap();
    let v3 = store
        .get_definition(DefKind::Bot, "keeper", 3)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(v1.hash, v3.hash);
    assert_eq!(v1.body, v3.body);
    assert_eq!(
        (v1.status, v3.status),
        (DefStatus::Superseded, DefStatus::Active)
    );
    assert_eq!((v1.created_at, v3.created_at), (1, 3));
    assert!(
        store
            .get_definition(DefKind::Bot, "keeper", 4)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn every_new_version_writes_one_event_in_the_same_commit() {
    let (_d, store) = open();
    put(&store, "a", 10).await;
    put(&store, "a", 11).await;
    put(&store, "b", 12).await;
    let events = store.events_since(Seq(0), 1000).await.unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].kind, "definition.created");
    assert_eq!(events[0].subject_type, "definition");
    assert_eq!(events[0].subject_id, "bot/keeper");
    assert_eq!(events[0].principal.as_deref(), Some("test"));
    assert_eq!(events[0].ts, 10);
    assert_eq!(events[0].data["version"], 1);
    assert_eq!(events[0].data["previous"], Value::Null);
    assert_eq!(events[1].data["version"], 2);
    assert_eq!(events[1].data["previous"], 1);
    assert_eq!(events[1].data["hash"], content_hash(&body("b")));
}

#[tokio::test]
async fn the_hash_must_match_the_body() {
    let (_d, store) = open();
    let err = store
        .put_definition(DefKind::Bot, "keeper", &"0".repeat(64), &body("a"), None, 1)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidArgument(_)), "{err:?}");
    assert!(kinds(&store).await.is_empty());
}

#[tokio::test]
async fn names_must_be_definition_names() {
    let (_d, store) = open();
    let body = body("a");
    let err = store
        .put_definition(
            DefKind::Bot,
            "Not A Name",
            &content_hash(&body),
            &body,
            None,
            1,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidArgument(_)), "{err:?}");
}

#[tokio::test]
async fn bodies_are_stored_in_canonical_form() {
    let (_d, store) = open();
    let body = json!({ "z": 1, "a": { "y": 2, "b": 3 } });
    store
        .put_definition(
            DefKind::Policy,
            "policy",
            &content_hash(&body),
            &body,
            None,
            1,
        )
        .await
        .unwrap();
    let text: String = store
        .read(|c| {
            c.query_row("SELECT body FROM definitions", [], |r| r.get(0))
                .db()
        })
        .await
        .unwrap();
    assert_eq!(text, r#"{"a":{"b":3,"y":2},"z":1}"#);
}

#[tokio::test]
async fn a_stored_version_cannot_be_changed_or_deleted() {
    let (_d, store) = open();
    put(&store, "a", 1).await;
    for sql in [
        "UPDATE definitions SET body = '{}'",
        "UPDATE definitions SET hash = 'x'",
        "UPDATE definitions SET version = 9",
        "UPDATE definitions SET created_at = 0",
        "UPDATE definitions SET name = 'other'",
        "DELETE FROM definitions",
    ] {
        let result = store
            .write(Durability::Normal, move |c| c.execute(sql, []).db())
            .await;
        assert!(result.is_err(), "{sql} was allowed");
    }
    // Status is the one thing that moves.
    let changed = store
        .write(Durability::Normal, |c| {
            c.execute("UPDATE definitions SET status = 'retired'", [])
                .db()
        })
        .await
        .unwrap();
    assert_eq!(changed, 1);
    let record = store
        .get_definition(DefKind::Bot, "keeper", 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.body, body("a"));
}

#[tokio::test]
async fn apply_stores_a_whole_set_and_current_returns_it() {
    let (_d, store) = open();
    let set = vec![
        new_def(DefKind::Bot, "keeper", "a"),
        new_def(DefKind::Bot, "scout", "b"),
        new_def(DefKind::Trigger, "nightly", "c"),
        new_def(DefKind::Policy, "policy", "d"),
    ];
    let applied = store
        .apply_definitions(set.clone(), Some("files"), 5)
        .await
        .unwrap();
    assert!(applied.puts.iter().all(|p| p.version == 1 && p.created));
    assert!(applied.retired.is_empty());

    let current = store.current_definitions(None).await.unwrap();
    let names: Vec<_> = current.iter().map(|r| (r.kind, r.name.as_str())).collect();
    assert_eq!(
        names,
        [
            (DefKind::Bot, "keeper"),
            (DefKind::Bot, "scout"),
            (DefKind::Policy, "policy"),
            (DefKind::Trigger, "nightly"),
        ]
    );
    let bots = store.current_definitions(Some(DefKind::Bot)).await.unwrap();
    assert_eq!(bots.len(), 2);

    // The same set again changes nothing and writes nothing.
    let before = kinds(&store).await.len();
    let again = store
        .apply_definitions(set, Some("files"), 6)
        .await
        .unwrap();
    assert!(again.puts.iter().all(|p| p.version == 1 && !p.created));
    assert!(again.retired.is_empty());
    assert_eq!(kinds(&store).await.len(), before);
}

#[tokio::test]
async fn a_name_that_leaves_the_set_is_retired_and_can_come_back() {
    let (_d, store) = open();
    let keeper = new_def(DefKind::Bot, "keeper", "a");
    let scout = new_def(DefKind::Bot, "scout", "b");
    store
        .apply_definitions(vec![keeper.clone(), scout.clone()], None, 1)
        .await
        .unwrap();

    let applied = store
        .apply_definitions(vec![keeper.clone()], None, 2)
        .await
        .unwrap();
    assert_eq!(applied.retired, [(DefKind::Bot, "scout".to_owned())]);
    let current = store.current_definitions(None).await.unwrap();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].name, "keeper");
    // Its history is kept.
    let latest = store
        .latest_definition(DefKind::Bot, "scout")
        .await
        .unwrap()
        .unwrap();
    assert_eq!((latest.version, latest.status), (1, DefStatus::Retired));

    // Retiring twice is not an event twice.
    let events = kinds(&store)
        .await
        .iter()
        .filter(|k| *k == "definition.retired")
        .count();
    store
        .apply_definitions(vec![keeper.clone()], None, 3)
        .await
        .unwrap();
    let again = kinds(&store)
        .await
        .iter()
        .filter(|k| *k == "definition.retired")
        .count();
    assert_eq!((events, again), (1, 1));

    // The file returns unchanged: the same version is active again, and no version is added.
    let applied = store
        .apply_definitions(vec![keeper, scout.clone()], None, 4)
        .await
        .unwrap();
    let scout_put = applied.puts.iter().find(|p| p.name == "scout").unwrap();
    assert_eq!((scout_put.version, scout_put.created), (1, false));
    assert_eq!(store.current_definitions(None).await.unwrap().len(), 2);
    assert!(
        kinds(&store)
            .await
            .contains(&"definition.restored".to_owned())
    );
    assert_eq!(
        store
            .latest_definition(DefKind::Bot, "scout")
            .await
            .unwrap()
            .unwrap()
            .version,
        1
    );

    // It returns changed: a new version, and the retired one is history.
    store.apply_definitions(vec![], None, 5).await.unwrap();
    let changed = new_def(DefKind::Bot, "scout", "b2");
    let applied = store
        .apply_definitions(vec![changed], None, 6)
        .await
        .unwrap();
    assert_eq!(
        (applied.puts[0].version, applied.puts[0].created),
        (2, true)
    );
    let v1 = store
        .get_definition(DefKind::Bot, "scout", 1)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(v1.status, DefStatus::Superseded);
}

#[tokio::test]
async fn apply_is_all_or_nothing() {
    let (_d, store) = open();
    let good = new_def(DefKind::Bot, "keeper", "a");
    let mut bad = new_def(DefKind::Bot, "scout", "b");
    bad.hash = "0".repeat(64);
    let err = store
        .apply_definitions(vec![good, bad], None, 1)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::InvalidArgument(_)), "{err:?}");
    assert!(store.current_definitions(None).await.unwrap().is_empty());
    assert!(kinds(&store).await.is_empty());
}

#[tokio::test]
async fn an_empty_store_has_no_definitions() {
    let (_d, store) = open();
    assert!(store.current_definitions(None).await.unwrap().is_empty());
    assert!(
        store
            .latest_definition(DefKind::Bot, "keeper")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .get_definition(DefKind::Bot, "keeper", 1)
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn definitions_survive_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reno.db");
    {
        let store = Store::open(&path).unwrap();
        put(&store, "a", 1).await;
        put(&store, "b", 2).await;
    }
    let store = Store::open(&path).unwrap();
    let current = store.current_definitions(None).await.unwrap();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].version, 2);
    assert_eq!(put(&store, "b", 3).await, (2, false));
}

// ---- the pin on a run ---------------------------------------------------------------------

fn task_id() -> TaskId {
    TaskId::from_suffix("t1").unwrap()
}

fn run_id(n: u32) -> RunId {
    RunId::from_suffix(&format!("r{n}")).unwrap()
}

fn exec() -> ExecutorId {
    "mock".parse().unwrap()
}

async fn with_task(store: &Store) {
    store
        .create_task(
            NewTask {
                id: task_id(),
                goal: "g".into(),
                class: None,
                spec: json!({}),
            },
            0,
        )
        .await
        .unwrap();
}

#[tokio::test]
async fn a_run_pins_the_version_it_was_submitted_under() {
    let (_d, store) = open();
    with_task(&store).await;
    put(&store, "a", 1).await;
    put(&store, "b", 2).await;

    let pin = BotPin {
        name: "keeper".into(),
        version: 1,
    };
    store
        .enqueue_pinned_run(&task_id(), &run_id(1), &exec(), &pin, 3)
        .await
        .unwrap();
    store
        .enqueue_run(&task_id(), &run_id(2), &exec(), 3)
        .await
        .unwrap();

    // A newer version later does not move the pin.
    put(&store, "c", 4).await;
    let pinned = store.get_run(&run_id(1)).await.unwrap().unwrap();
    assert_eq!(pinned.bot, Some(pin.clone()));
    assert_eq!(store.get_run(&run_id(2)).await.unwrap().unwrap().bot, None);

    // The pin is in the enqueue event, and listings carry it.
    let enqueued = store
        .events_since(Seq(0), 1000)
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "run.enqueued" && e.subject_id == "run_r1")
        .unwrap();
    assert_eq!(
        enqueued.data["bot"],
        json!({ "name": "keeper", "version": 1 })
    );
    let listed = store.list_runs(None).await.unwrap();
    assert_eq!(listed.iter().filter(|r| r.bot.is_some()).count(), 1);
}

#[tokio::test]
async fn a_pin_to_a_version_that_does_not_exist_is_refused() {
    let (_d, store) = open();
    with_task(&store).await;
    put(&store, "a", 1).await;
    for (name, version) in [("keeper", 2), ("ghost", 1)] {
        let pin = BotPin {
            name: name.into(),
            version,
        };
        let err = store
            .enqueue_pinned_run(&task_id(), &run_id(1), &exec(), &pin, 2)
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::NotFound {
                    kind: "definition",
                    ..
                }
            ),
            "{err:?}"
        );
    }
    assert!(store.get_run(&run_id(1)).await.unwrap().is_none());
}

#[tokio::test]
async fn a_pin_never_changes_and_is_never_half_set() {
    let (_d, store) = open();
    with_task(&store).await;
    put(&store, "a", 1).await;
    let pin = BotPin {
        name: "keeper".into(),
        version: 1,
    };
    store
        .enqueue_pinned_run(&task_id(), &run_id(1), &exec(), &pin, 2)
        .await
        .unwrap();
    for sql in [
        "UPDATE runs SET bot_version = 2",
        "UPDATE runs SET bot_name = 'other'",
        "UPDATE runs SET bot_name = NULL",
        "UPDATE runs SET bot_version = NULL",
        "INSERT INTO runs (id, task_id, executor, created_at, bot_name) VALUES ('run_x', 'task_t1', 'mock', 1, 'a')",
        "INSERT INTO runs (id, task_id, executor, created_at, bot_version) VALUES ('run_y', 'task_t1', 'mock', 1, 1)",
    ] {
        let result = store
            .write(Durability::Normal, move |c| c.execute(sql, []).db())
            .await;
        assert!(result.is_err(), "{sql} was allowed");
    }
    assert_eq!(
        store.get_run(&run_id(1)).await.unwrap().unwrap().bot,
        Some(pin)
    );
}

#[tokio::test]
async fn a_pinned_run_keeps_its_pin_through_its_lifecycle() {
    let (_d, store) = open();
    with_task(&store).await;
    put(&store, "a", 1).await;
    let pin = BotPin {
        name: "keeper".into(),
        version: 1,
    };
    store
        .enqueue_pinned_run(&task_id(), &run_id(1), &exec(), &pin, 2)
        .await
        .unwrap();
    let claim = store.claim_next("me", 3, 60_000).await.unwrap().unwrap();
    store
        .transition(
            &claim.run_id,
            claim.fence,
            reno_proto::RunStatus::Running,
            4,
        )
        .await
        .unwrap();
    store
        .transition(
            &claim.run_id,
            claim.fence,
            reno_proto::RunStatus::Interrupted,
            5,
        )
        .await
        .unwrap();
    assert_eq!(
        store.get_run(&run_id(1)).await.unwrap().unwrap().bot,
        Some(pin)
    );
}
