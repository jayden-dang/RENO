//! Behaviour tests for trigger state and firings: atomic firing, the nonce, coalescing, backoff
//! bookkeeping and what the schema refuses. `now` is a number the test owns.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use reno_proto::defs::{BotPin, DefKind, content_hash};
use reno_proto::{ExecutorId, FiringOutcome, RunId, RunStatus, Seq, TaskId};
use rusqlite::Connection;
use serde_json::json;
use tempfile::TempDir;

use crate::triggers::{FiringRun, NewFiring, TriggerRow, firing_nonce};
use crate::{NewTask, Store, StoreError};

fn run_id(n: usize) -> RunId {
    RunId::from_suffix(&format!("r{n}")).unwrap()
}

fn task_id(n: usize) -> TaskId {
    TaskId::from_suffix(&format!("t{n}")).unwrap()
}

fn exec() -> ExecutorId {
    "mock".parse().unwrap()
}

fn row(name: &str) -> TriggerRow {
    TriggerRow {
        name: name.to_owned(),
        def_version: 1,
        schedule: json!({"kind": "every", "every": "1h"}),
        enabled: true,
        next_due_at: Some(1_000),
        last_fired_at: None,
        last_run: None,
        failures: 0,
        backoff_until: None,
    }
}

fn firing(trigger: &str, n: usize, scheduled_at: i64) -> NewFiring {
    NewFiring {
        trigger: trigger.to_owned(),
        kind: "every".to_owned(),
        scheduled_at,
        missed: 1,
        next_due_at: Some(scheduled_at + 3_600_000),
        run: FiringRun {
            task: NewTask {
                id: task_id(n),
                goal: format!("prompt {n}"),
                class: None,
                spec: json!({}),
            },
            run: run_id(n),
            executor: exec(),
            bot: BotPin {
                name: "keeper".into(),
                version: 1,
            },
        },
    }
}

/// A store holding bot `keeper` v1 and a planned trigger `nightly`.
async fn planned() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("reno.db")).unwrap();
    let body = json!({"name": "keeper", "instructions": "x", "executors": ["mock"]});
    store
        .put_definition(DefKind::Bot, "keeper", &content_hash(&body), &body, None, 1)
        .await
        .unwrap();
    store
        .plan_triggers(vec![row("nightly")], vec![], 2)
        .await
        .unwrap();
    (dir, store)
}

async fn kinds(store: &Store) -> Vec<String> {
    store
        .events_since(Seq(0), 10_000)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .filter(|k| k.starts_with("trigger."))
        .collect()
}

async fn trigger(store: &Store, name: &str) -> TriggerRow {
    store
        .list_triggers()
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.name == name)
        .unwrap()
}

fn raw(dir: &TempDir) -> Connection {
    Connection::open(dir.path().join("reno.db")).unwrap()
}

#[tokio::test]
async fn the_nonce_is_kind_trigger_and_instant() {
    assert_eq!(
        firing_nonce("cron", "nightly", 1_700_000_000_000),
        "cron:nightly:1700000000000"
    );
}

#[tokio::test]
async fn planning_stores_rows_and_skips_what_is_already_planned() {
    let (_d, store) = planned().await;
    assert_eq!(trigger(&store, "nightly").await, row("nightly"));
    assert_eq!(kinds(&store).await, ["trigger.planned"]);

    // The same row again: no write, no event.
    store
        .plan_triggers(vec![row("nightly")], vec![], 3)
        .await
        .unwrap();
    assert_eq!(kinds(&store).await, ["trigger.planned"]);

    // A changed one replaces it; a removed one goes.
    let mut next = row("nightly");
    next.def_version = 2;
    next.next_due_at = Some(9_000);
    store
        .plan_triggers(vec![next.clone(), row("poke")], vec![], 4)
        .await
        .unwrap();
    assert_eq!(trigger(&store, "nightly").await, next);
    store
        .plan_triggers(vec![], vec!["poke".into(), "ghost".into()], 5)
        .await
        .unwrap();
    let names: Vec<_> = store
        .list_triggers()
        .await
        .unwrap()
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert_eq!(names, ["nightly"]);
    assert_eq!(
        kinds(&store).await,
        [
            "trigger.planned",
            "trigger.planned",
            "trigger.planned",
            "trigger.removed"
        ],
        "removing a trigger that is not there writes nothing"
    );
}

#[tokio::test]
async fn firing_creates_the_run_the_firing_and_the_new_state_together() {
    let (_d, store) = planned().await;
    let result = store
        .fire_trigger(firing("nightly", 1, 1_000), 1_005)
        .await
        .unwrap();
    assert_eq!(result.outcome, FiringOutcome::Fired);
    assert_eq!(result.run, Some(run_id(1)));

    let run = store.get_run(&run_id(1)).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Queued);
    assert_eq!(run.goal, "prompt 1");
    assert_eq!(
        run.bot,
        Some(BotPin {
            name: "keeper".into(),
            version: 1
        })
    );

    let state = trigger(&store, "nightly").await;
    assert_eq!(state.next_due_at, Some(1_000 + 3_600_000));
    assert_eq!(state.last_fired_at, Some(1_005));
    assert_eq!(state.last_run, Some(run_id(1)));

    let firings = store.firings(Some("nightly"), 10).await.unwrap();
    assert_eq!(firings.len(), 1);
    assert_eq!(firings[0].nonce, "every:nightly:1000");
    assert_eq!(firings[0].scheduled_at, 1_000);
    assert_eq!(firings[0].fired_at, 1_005);
    assert_eq!(firings[0].run, run_id(1));
    assert_eq!(firings[0].outcome, FiringOutcome::Fired);
    assert_eq!(kinds(&store).await, ["trigger.planned", "trigger.fired"]);
}

#[tokio::test]
async fn the_same_occurrence_twice_is_a_no_op() {
    let (_d, store) = planned().await;
    store
        .fire_trigger(firing("nightly", 1, 1_000), 1_005)
        .await
        .unwrap();
    // A second scheduler fires the same occurrence with its own ids and its own clock.
    let again = store
        .fire_trigger(firing("nightly", 2, 1_000), 1_900)
        .await
        .unwrap();
    assert_eq!(again.outcome, FiringOutcome::Duplicate);
    assert_eq!(again.run, None);

    assert_eq!(store.count_runs(&RunStatus::ALL).await.unwrap(), 1);
    assert_eq!(store.firings(None, 10).await.unwrap().len(), 1);
    assert_eq!(
        trigger(&store, "nightly").await.last_fired_at,
        Some(1_005),
        "the loser changed nothing"
    );
    assert_eq!(kinds(&store).await, ["trigger.planned", "trigger.fired"]);
}

#[tokio::test]
async fn a_running_previous_run_coalesces_the_next_occurrence() {
    let (_d, store) = planned().await;
    store
        .fire_trigger(firing("nightly", 1, 1_000), 1_001)
        .await
        .unwrap();

    let merged = store
        .fire_trigger(firing("nightly", 2, 2_000), 2_001)
        .await
        .unwrap();
    assert_eq!(merged.outcome, FiringOutcome::Coalesced);
    assert_eq!(merged.run, Some(run_id(1)), "merged into the busy run");
    assert_eq!(
        store.count_runs(&RunStatus::ALL).await.unwrap(),
        1,
        "no task and no run were made"
    );
    let state = trigger(&store, "nightly").await;
    assert_eq!(state.last_run, Some(run_id(1)));
    assert_eq!(state.last_fired_at, Some(2_001));
    let firings = store.firings(None, 10).await.unwrap();
    assert_eq!(firings[0].outcome, FiringOutcome::Coalesced);
    assert_eq!(firings[0].run, run_id(1));
}

#[tokio::test]
async fn a_finished_or_interrupted_previous_run_does_not_hold_the_trigger() {
    // (claim first, then the statuses the run passes through)
    let cases: [(bool, &[RunStatus]); 4] = [
        (
            true,
            &[
                RunStatus::Running,
                RunStatus::Evaluating,
                RunStatus::Succeeded,
            ],
        ),
        (
            true,
            &[RunStatus::Running, RunStatus::Evaluating, RunStatus::Failed],
        ),
        (true, &[RunStatus::Running, RunStatus::Interrupted]),
        (false, &[RunStatus::Cancelled]),
    ];
    for (n, (claimed, path)) in cases.into_iter().enumerate() {
        let n = n + 1;
        let (_d, store) = planned().await;
        store
            .fire_trigger(firing("nightly", n, 1_000), 1_001)
            .await
            .unwrap();
        let fence = if claimed {
            store
                .claim_next("test", 1_002, 60_000)
                .await
                .unwrap()
                .unwrap()
                .fence
        } else {
            0
        };
        for status in path {
            store
                .transition(&run_id(n), fence, *status, 1_003)
                .await
                .unwrap();
        }
        let next = store
            .fire_trigger(firing("nightly", 10 + n, 2_000), 2_001)
            .await
            .unwrap();
        assert_eq!(next.outcome, FiringOutcome::Fired, "case {n}");
    }
}

#[tokio::test]
async fn a_firing_that_cannot_complete_leaves_nothing_behind() {
    let (_d, store) = planned().await;
    // The pinned bot version is not stored.
    let mut missing = firing("nightly", 1, 1_000);
    missing.run.bot.version = 7;
    let error = store.fire_trigger(missing, 1_001).await.unwrap_err();
    assert!(matches!(error, StoreError::NotFound { .. }), "{error:?}");
    // An unknown trigger.
    let error = store
        .fire_trigger(firing("ghost", 1, 1_000), 1_001)
        .await
        .unwrap_err();
    assert!(matches!(error, StoreError::NotFound { .. }), "{error:?}");

    assert_eq!(store.count_runs(&RunStatus::ALL).await.unwrap(), 0);
    assert!(store.firings(None, 10).await.unwrap().is_empty());
    assert_eq!(trigger(&store, "nightly").await, row("nightly"));
}

#[tokio::test]
async fn a_taken_task_id_rolls_the_whole_firing_back() {
    let (_d, store) = planned().await;
    store
        .fire_trigger(firing("nightly", 1, 1_000), 1_001)
        .await
        .unwrap();
    let claim = store
        .claim_next("test", 1_002, 60_000)
        .await
        .unwrap()
        .unwrap();
    for status in [
        RunStatus::Running,
        RunStatus::Evaluating,
        RunStatus::Succeeded,
    ] {
        store
            .transition(&run_id(1), claim.fence, status, 1_003)
            .await
            .unwrap();
    }
    // Same task id again: the firing must not be recorded.
    let error = store
        .fire_trigger(firing("nightly", 1, 2_000), 2_001)
        .await
        .unwrap_err();
    assert!(
        matches!(error, StoreError::AlreadyExists { kind: "task", .. }),
        "{error:?}"
    );
    assert_eq!(store.firings(None, 10).await.unwrap().len(), 1);
    assert_eq!(trigger(&store, "nightly").await.last_fired_at, Some(1_001));
    // With fresh ids it goes through.
    let ok = store
        .fire_trigger(firing("nightly", 2, 2_000), 2_002)
        .await
        .unwrap();
    assert_eq!(ok.outcome, FiringOutcome::Fired);
}

#[tokio::test]
async fn a_start_failure_backs_the_trigger_off_once_per_episode() {
    let (_d, store) = planned().await;
    store
        .fire_trigger(firing("nightly", 1, 1_000), 1_001)
        .await
        .unwrap();

    let applied = store
        .trigger_start_failed("nightly", &run_id(1), 1, 301_001, "not signed in", 1_002)
        .await
        .unwrap();
    assert!(applied);
    let state = trigger(&store, "nightly").await;
    assert_eq!((state.failures, state.backoff_until), (1, Some(301_001)));
    assert_eq!(
        store.firings(None, 1).await.unwrap()[0].outcome,
        FiringOutcome::BackedOff
    );

    // A second failure extends the backoff and writes no second notification.
    store
        .trigger_start_failed("nightly", &run_id(1), 2, 601_001, "not signed in", 301_100)
        .await
        .unwrap();
    assert_eq!(
        kinds(&store).await,
        [
            "trigger.planned",
            "trigger.fired",
            "trigger.backoff_started",
            "trigger.backoff_extended"
        ]
    );

    // A run that is not the trigger's last one changes nothing.
    let stale = store
        .trigger_start_failed("nightly", &run_id(9), 3, 9, "x", 301_200)
        .await
        .unwrap();
    assert!(!stale);
    assert_eq!(trigger(&store, "nightly").await.failures, 2);

    // Starting ends it.
    assert!(
        !store
            .trigger_recovered("nightly", &run_id(9), 601_002)
            .await
            .unwrap()
    );
    assert!(
        store
            .trigger_recovered("nightly", &run_id(1), 601_002)
            .await
            .unwrap()
    );
    let state = trigger(&store, "nightly").await;
    assert_eq!((state.failures, state.backoff_until), (0, None));
    assert!(
        !store
            .trigger_recovered("nightly", &run_id(1), 601_003)
            .await
            .unwrap(),
        "a healthy trigger has nothing to recover from"
    );
    assert_eq!(kinds(&store).await.last().unwrap(), "trigger.recovered");
}

#[tokio::test]
async fn the_schema_keeps_firings_and_backoff_consistent() {
    let (dir, store) = planned().await;
    store
        .fire_trigger(firing("nightly", 1, 1_000), 1_001)
        .await
        .unwrap();
    drop(store);
    let conn = raw(&dir);

    // Failures and backoff come together.
    assert!(
        conn.execute("UPDATE triggers SET failures = 1", [])
            .is_err()
    );
    assert!(
        conn.execute("UPDATE triggers SET backoff_until = 5", [])
            .is_err()
    );
    // The nonce is unique, firings are kept, and the one allowed change is fired -> backed_off.
    assert!(
        conn.execute(
            "INSERT INTO firings (nonce, trigger_name, scheduled_at, fired_at, run_id, outcome)
             VALUES ('every:nightly:1000', 'nightly', 1000, 1, 'run_r1', 'fired')",
            []
        )
        .is_err()
    );
    assert!(conn.execute("DELETE FROM firings", []).is_err());
    assert!(
        conn.execute("UPDATE firings SET outcome = 'coalesced'", [])
            .is_err()
    );
    assert!(
        conn.execute("UPDATE firings SET scheduled_at = 5", [])
            .is_err()
    );
    conn.execute("UPDATE firings SET outcome = 'backed_off'", [])
        .unwrap();
    assert!(
        conn.execute("UPDATE firings SET outcome = 'fired'", [])
            .is_err(),
        "a backed-off firing stays"
    );
    // `duplicate` is never stored.
    assert!(
        conn.execute("UPDATE firings SET outcome = 'duplicate'", [])
            .is_err()
    );
}
