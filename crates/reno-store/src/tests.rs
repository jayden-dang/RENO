//! Behaviour tests for the store. No network, no wall clock: `now` is a number the test owns.
//! The one real-time wait is the idle-checkpoint test, which polls a counter with a deadline.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::Ordering;
use std::time::Duration;

use reno_proto::{ExecutorId, RunId, RunStatus, Seq, TaskId};
use rusqlite::Connection;
use tempfile::TempDir;

use crate::error::DbResult;
use crate::runs::{status_str, tx_create_task, tx_transition};
use crate::writer::{self, Durability};
use crate::{NewTask, Store, StoreConfig, StoreError};

fn task_id(n: usize) -> TaskId {
    TaskId::from_suffix(&format!("t{n}")).unwrap()
}

fn run_id(n: usize) -> RunId {
    RunId::from_suffix(&format!("r{n}")).unwrap()
}

fn exec() -> ExecutorId {
    "claude".parse().unwrap()
}

fn new_task(n: usize) -> NewTask {
    NewTask {
        id: task_id(n),
        goal: format!("goal {n}"),
        class: None,
        spec: serde_json::json!({}),
    }
}

fn open() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("reno.db")).unwrap();
    (dir, store)
}

/// A store with task 0 and `runs` queued runs `r0..`, all enqueued at `now = 0`.
async fn seeded(runs: usize) -> (TempDir, Store) {
    let (dir, store) = open();
    store.create_task(new_task(0), 0).await.unwrap();
    for n in 0..runs {
        store
            .enqueue_run(&task_id(0), &run_id(n), &exec(), 0)
            .await
            .unwrap();
    }
    (dir, store)
}

async fn count(store: &Store, table: &'static str) -> i64 {
    store
        .read(move |c| {
            c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))
                .db()
        })
        .await
        .unwrap()
}

async fn event_kinds(store: &Store) -> Vec<String> {
    store
        .events_since(Seq(0), 10_000)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect()
}

#[tokio::test]
async fn claim_heartbeat_transition_with_events() {
    let (_d, store) = seeded(1).await;
    assert_eq!(
        store.claim_next("a", 10, 100).await.unwrap(),
        Some(crate::Claim {
            run_id: run_id(0),
            fence: 1
        })
    );
    let run = store.get_run(&run_id(0)).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Leased);
    assert_eq!(run.lease_owner.as_deref(), Some("a"));
    assert_eq!(run.lease_expires_at, Some(110));

    store.heartbeat(&run_id(0), 1, 50, 100).await.unwrap();
    let run = store.get_run(&run_id(0)).await.unwrap().unwrap();
    assert_eq!(run.lease_expires_at, Some(150));

    store
        .transition(&run_id(0), 1, RunStatus::Running, 60)
        .await
        .unwrap();
    store
        .transition(&run_id(0), 1, RunStatus::Evaluating, 70)
        .await
        .unwrap();
    store
        .transition(&run_id(0), 1, RunStatus::Succeeded, 80)
        .await
        .unwrap();
    let run = store.get_run(&run_id(0)).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Succeeded);
    assert_eq!(run.started_at, Some(60));
    assert_eq!(run.ended_at, Some(80));
    assert_eq!(run.lease_owner, None);

    assert_eq!(
        event_kinds(&store).await,
        [
            "task.created",
            "run.enqueued",
            "run.claimed",
            "run.transitioned",
            "run.transitioned",
            "run.transitioned",
            "run.ended"
        ]
    );
    let attempt: String = store
        .read(|c| {
            c.query_row(
                "SELECT status FROM attempts WHERE run_id = 'run_r0' AND n = 1",
                [],
                |r| r.get(0),
            )
            .db()
        })
        .await
        .unwrap();
    assert_eq!(attempt, "succeeded");
}

#[tokio::test]
async fn claim_takes_the_oldest_and_none_when_empty() {
    let (_d, store) = open();
    assert_eq!(store.claim_next("a", 0, 10).await.unwrap(), None);
    store.create_task(new_task(0), 0).await.unwrap();
    for n in [2, 0, 1] {
        store
            .enqueue_run(&task_id(0), &run_id(n), &exec(), n as i64)
            .await
            .unwrap();
    }
    // Enqueued 2 first (created_at 2), then 0 (created_at 0), then 1: oldest by created_at is 0.
    let first = store.claim_next("a", 5, 10).await.unwrap().unwrap();
    assert_eq!(first.run_id, run_id(0));
    let second = store.claim_next("a", 5, 10).await.unwrap().unwrap();
    assert_eq!(second.run_id, run_id(1));
    let third = store.claim_next("a", 5, 10).await.unwrap().unwrap();
    assert_eq!(third.run_id, run_id(2));
    assert_eq!(store.claim_next("a", 5, 10).await.unwrap(), None);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn two_simultaneous_claimers_get_one_run() {
    let (_d, store) = seeded(1).await;
    let (a, b) = tokio::join!(
        {
            let s = store.clone();
            tokio::spawn(async move { s.claim_next("a", 1, 100).await })
        },
        {
            let s = store.clone();
            tokio::spawn(async move { s.claim_next("b", 1, 100).await })
        },
    );
    let results = [a.unwrap().unwrap(), b.unwrap().unwrap()];
    assert_eq!(results.iter().filter(|r| r.is_some()).count(), 1);
}

#[tokio::test]
async fn expired_lease_is_reclaimable_with_a_higher_fence_and_the_old_fence_is_stale() {
    let (_d, store) = seeded(1).await;
    let a = store.claim_next("a", 100, 50).await.unwrap().unwrap();
    assert_eq!(a.fence, 1);
    // One millisecond before expiry the lease still holds.
    assert_eq!(store.claim_next("b", 149, 50).await.unwrap(), None);
    // At the deadline it is expired.
    let b = store.claim_next("b", 150, 50).await.unwrap().unwrap();
    assert_eq!((b.run_id.clone(), b.fence), (run_id(0), 2));
    let run = store.get_run(&run_id(0)).await.unwrap().unwrap();
    assert_eq!(run.lease_owner.as_deref(), Some("b"));

    // The first claimer's writes are rejected, whatever they are.
    let stale = |e| {
        matches!(
            e,
            StoreError::StaleFence {
                supplied: 1,
                current: 2,
                ..
            }
        )
    };
    assert!(stale(
        store.heartbeat(&run_id(0), 1, 160, 50).await.unwrap_err()
    ));
    assert!(stale(
        store
            .transition(&run_id(0), 1, RunStatus::Running, 160)
            .await
            .unwrap_err()
    ));
    assert!(stale(
        store
            .record_process(&run_id(0), 1, 10, 10, 77, 160)
            .await
            .unwrap_err()
    ));
    // The second claimer's succeed.
    store
        .transition(&run_id(0), 2, RunStatus::Running, 160)
        .await
        .unwrap();
    // A running run with an expired lease is not re-claimable.
    assert_eq!(store.claim_next("c", 10_000, 50).await.unwrap(), None);
    let attempts = count(&store, "attempts").await;
    assert_eq!(attempts, 2, "each claim opens an attempt");
}

#[tokio::test]
async fn illegal_transition_is_rejected_and_changes_nothing() {
    let (_d, store) = seeded(1).await;
    let before = event_kinds(&store).await.len();
    let err = store
        .transition(&run_id(0), 0, RunStatus::Running, 1)
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StoreError::IllegalTransition {
            from: RunStatus::Queued,
            to: RunStatus::Running,
            ..
        }
    ));
    let run = store.get_run(&run_id(0)).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Queued);
    assert_eq!(event_kinds(&store).await.len(), before);
    // The fence is checked before the lifecycle rule.
    assert!(matches!(
        store
            .transition(&run_id(0), 9, RunStatus::Running, 1)
            .await
            .unwrap_err(),
        StoreError::StaleFence { .. }
    ));
}

#[tokio::test]
async fn heartbeat_needs_a_lease_and_missing_rows_are_not_found() {
    let (_d, store) = seeded(1).await;
    assert!(matches!(
        store.heartbeat(&run_id(0), 0, 1, 10).await.unwrap_err(),
        StoreError::NotLeased {
            status: RunStatus::Queued,
            ..
        }
    ));
    assert!(matches!(
        store.heartbeat(&run_id(7), 0, 1, 10).await.unwrap_err(),
        StoreError::NotFound { kind: "run", .. }
    ));
    assert!(matches!(
        store
            .enqueue_run(&task_id(9), &run_id(8), &exec(), 1)
            .await
            .unwrap_err(),
        StoreError::NotFound { kind: "task", .. }
    ));
    assert!(matches!(
        store
            .enqueue_run(&task_id(0), &run_id(0), &exec(), 1)
            .await
            .unwrap_err(),
        StoreError::AlreadyExists { kind: "run", .. }
    ));
    assert!(matches!(
        store.create_task(new_task(0), 1).await.unwrap_err(),
        StoreError::AlreadyExists { kind: "task", .. }
    ));
    assert_eq!(store.get_run(&run_id(7)).await.unwrap(), None);
}

#[tokio::test]
async fn retry_requeues_with_a_new_fence_on_the_next_claim() {
    let (_d, store) = seeded(1).await;
    let c = store.claim_next("a", 0, 100).await.unwrap().unwrap();
    for to in [RunStatus::Running, RunStatus::Evaluating, RunStatus::Failed] {
        store.transition(&c.run_id, c.fence, to, 5).await.unwrap();
    }
    store
        .transition(&c.run_id, c.fence, RunStatus::Queued, 6)
        .await
        .unwrap();
    let run = store.get_run(&c.run_id).await.unwrap().unwrap();
    assert_eq!((run.ended_at, run.started_at), (None, None));
    let again = store.claim_next("a", 7, 100).await.unwrap().unwrap();
    assert_eq!(again.fence, 2);
}

#[tokio::test]
async fn listing_and_recovery_queries() {
    let (_d, store) = seeded(3).await;
    let c = store.claim_next("a", 1, 100).await.unwrap().unwrap();
    store
        .transition(&c.run_id, c.fence, RunStatus::Running, 2)
        .await
        .unwrap();
    store
        .transition(&c.run_id, c.fence, RunStatus::Interrupted, 3)
        .await
        .unwrap();
    store
        .transition(&c.run_id, c.fence, RunStatus::Cancelled, 4)
        .await
        .unwrap();
    assert_eq!(store.list_runs(None).await.unwrap().len(), 3);
    let queued = store.list_runs(Some(RunStatus::Queued)).await.unwrap();
    assert_eq!(
        queued.iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
        [run_id(1), run_id(2)]
    );
    let open = store.nonterminal_runs().await.unwrap();
    assert_eq!(open.len(), 2);
    assert!(open.iter().all(|r| r.status == RunStatus::Queued));
}

#[tokio::test]
async fn list_runs_page_is_newest_first_and_stable_across_inserts() {
    // All seven runs share created_at = 0, so the order rests on rowid.
    let (_d, store) = seeded(7).await;
    let ids = |rows: &[crate::RunRecord]| rows.iter().map(|r| r.id.clone()).collect::<Vec<_>>();

    let first = store.list_runs_page(&[], None, 3).await.unwrap();
    assert_eq!(ids(&first), [run_id(6), run_id(5), run_id(4)]);

    // Newer runs arrive between pages: the cursor neither repeats nor skips.
    for n in 7..10 {
        store
            .enqueue_run(&task_id(0), &run_id(n), &exec(), 5)
            .await
            .unwrap();
    }
    let cursor = first.last().map(|r| r.id.clone());
    let second = store.list_runs_page(&[], cursor.as_ref(), 3).await.unwrap();
    assert_eq!(ids(&second), [run_id(3), run_id(2), run_id(1)]);
    let cursor = second.last().map(|r| r.id.clone());
    let last = store.list_runs_page(&[], cursor.as_ref(), 3).await.unwrap();
    assert_eq!(ids(&last), [run_id(0)]);

    // A fresh listing sees the newer runs first.
    let head = store.list_runs_page(&[], None, 2).await.unwrap();
    assert_eq!(ids(&head), [run_id(9), run_id(8)]);

    // Status filter, and a cursor that names no run.
    let c = store.claim_next("a", 1, 100).await.unwrap().unwrap();
    let leased = store
        .list_runs_page(&[RunStatus::Leased, RunStatus::Running], None, 10)
        .await
        .unwrap();
    assert_eq!(ids(&leased), [c.run_id]);
    let missing = run_id(99);
    let err = store
        .list_runs_page(&[], Some(&missing), 3)
        .await
        .unwrap_err();
    assert!(matches!(err, StoreError::NotFound { .. }), "{err:?}");
}

#[tokio::test]
async fn events_since_pages_in_order() {
    let (_d, store) = seeded(4).await;
    let all = store.events_since(Seq(0), 100).await.unwrap();
    assert_eq!(all.len(), 5);
    assert!(all.windows(2).all(|w| w[0].seq < w[1].seq));
    let page = store.events_since(Seq(0), 2).await.unwrap();
    assert_eq!(page.len(), 2);
    let rest = store.events_since(page[1].seq, 100).await.unwrap();
    assert_eq!(rest.len(), 3);
    assert_eq!(rest[0], all[2]);
    assert_eq!(all[1].kind, "run.enqueued");
    assert_eq!(all[1].subject_type, "run");
    assert_eq!(all[1].data["task"], "task_t0");
}

#[tokio::test]
async fn every_status_satisfies_the_check_constraint() {
    let (_d, store) = seeded(0).await;
    store
        .write(Durability::Normal, |c| {
            for (i, s) in RunStatus::ALL.into_iter().enumerate() {
                c.execute(
                    "INSERT INTO threads (id, created_at, updated_at) VALUES (?1, 0, 0)",
                    rusqlite::params![format!("thr_s{i}")],
                )
                .db()?;
                c.execute(
                    "INSERT INTO runs (id, task_id, executor, status, created_at, thread_id)
                     VALUES (?1, 'task_t0', 'claude', ?2, 0, ?3)",
                    rusqlite::params![format!("run_s{i}"), status_str(s), format!("thr_s{i}")],
                )
                .db()?;
                // The storage name is also the serde name.
                assert_eq!(serde_json::to_value(s).unwrap(), status_str(s));
            }
            Ok(())
        })
        .await
        .unwrap();
    let all = store.list_runs(None).await.unwrap();
    assert_eq!(
        all.iter().map(|r| r.status).collect::<Vec<_>>(),
        RunStatus::ALL
    );
    let bad = store
        .write(Durability::Normal, |c| {
            c.execute(
                "INSERT INTO runs (id, task_id, executor, status, created_at)
                 VALUES ('run_bad', 'task_t0', 'claude', 'bogus', 0)",
                [],
            )
            .db()
        })
        .await;
    assert!(matches!(bad, Err(StoreError::Database(_))));
}

#[tokio::test]
async fn foreign_keys_are_enforced() {
    let (_d, store) = seeded(0).await;
    let bad = store
        .write(Durability::Normal, |c| {
            c.execute(
                "INSERT INTO runs (id, task_id, executor, created_at)
                 VALUES ('run_x', 'task_missing', 'claude', 0)",
                [],
            )
            .db()
        })
        .await;
    assert!(matches!(bad, Err(StoreError::Database(_))));
}

// ---------------------------------------------------------------------------------------------
// Writer: groups, failures, durability, checkpoints
// ---------------------------------------------------------------------------------------------

/// Queues a command that holds the writer until the returned sender fires, so that everything
/// submitted next is guaranteed to be in the queue together.
fn block_writer(store: &Store) -> std::sync::mpsc::Sender<()> {
    let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let (cmd, _rx) = writer::command(Durability::Normal, move |_: &Connection| {
        started_tx.send(()).ok();
        release_rx.recv().ok();
        Ok(())
    });
    assert!(store.try_submit(cmd).unwrap().is_ok());
    started_rx.recv().unwrap();
    release_tx
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn queued_commands_share_one_commit() {
    let (_d, store) = open();
    let release = block_writer(&store);
    let mut rxs = Vec::new();
    for n in 0..50 {
        let task = new_task(n);
        let (cmd, rx) = writer::command(Durability::Normal, move |c: &Connection| {
            tx_create_task(c, &task, 1)
        });
        assert!(store.try_submit(cmd).unwrap().is_ok());
        rxs.push(rx);
    }
    let commits_before = store.stats().commits.load(Ordering::Relaxed);
    release.send(()).unwrap();
    for rx in rxs {
        rx.await.unwrap().unwrap();
    }
    let commits = store.stats().commits.load(Ordering::Relaxed) - commits_before;
    // The blocker's own transaction commits too; the 50 tasks share exactly one more.
    assert_eq!(commits, 2);
    assert_eq!(count(&store, "tasks").await, 50);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failing_command_rolls_back_only_itself_and_leaves_no_orphans() {
    let (_d, store) = open();
    let release = block_writer(&store);
    let mut rxs = Vec::new();
    for n in 0..3 {
        let task = new_task(n);
        let (cmd, rx) = writer::command(Durability::Normal, move |c: &Connection| {
            tx_create_task(c, &task, 1)?;
            if n == 1 {
                // Both the task row and its event row are written, then the command fails.
                return Err(StoreError::InvalidArgument("forced".into()));
            }
            Ok(n)
        });
        assert!(store.try_submit(cmd).unwrap().is_ok());
        rxs.push(rx);
    }
    release.send(()).unwrap();
    let mut results = Vec::new();
    for rx in rxs {
        results.push(rx.await.unwrap());
    }
    assert_eq!(*results[0].as_ref().unwrap(), 0);
    assert!(matches!(results[1], Err(StoreError::InvalidArgument(_))));
    assert_eq!(*results[2].as_ref().unwrap(), 2);

    assert_eq!(count(&store, "tasks").await, 2);
    assert_eq!(count(&store, "events").await, 2, "no orphan event row");
    let ids: Vec<String> = store
        .read(|c| {
            let mut s = c
                .prepare("SELECT subject_id FROM events ORDER BY seq")
                .db()?;
            s.query_map([], |r| r.get(0))
                .db()?
                .collect::<Result<_, _>>()
                .db()
        })
        .await
        .unwrap();
    assert_eq!(ids, ["task_t0", "task_t2"]);
}

#[tokio::test]
async fn a_typed_change_and_its_event_roll_back_together() {
    let (_d, store) = seeded(1).await;
    let c = store.claim_next("a", 0, 100).await.unwrap().unwrap();
    let events = count(&store, "events").await;
    let run = c.run_id.clone();
    let res: Result<(), StoreError> = store
        .write(Durability::Normal, move |conn| {
            tx_transition(conn, &run, 1, RunStatus::Running, &Default::default(), 5)?;
            Err(StoreError::InvalidArgument(
                "forced after the transition".into(),
            ))
        })
        .await;
    assert!(res.is_err());
    let run = store.get_run(&c.run_id).await.unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Leased);
    assert_eq!(count(&store, "events").await, events);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_thousand_concurrent_writes_all_land() {
    let (_d, store) = open();
    let mut handles = Vec::new();
    for n in 0..1000 {
        let store = store.clone();
        handles.push(tokio::spawn(async move {
            store.create_task(new_task(n), n as i64).await
        }));
    }
    for h in handles {
        h.await.unwrap().unwrap();
    }
    assert_eq!(count(&store, "tasks").await, 1000);
    let events = store.events_since(Seq(0), 5000).await.unwrap();
    assert_eq!(events.len(), 1000);
    assert!(events.windows(2).all(|w| w[0].seq < w[1].seq));
    let stats = store.stats();
    assert!(stats.commits.load(Ordering::Relaxed) <= 1000);
    assert_eq!(stats.commands.load(Ordering::Relaxed), 1000);
}

#[tokio::test]
async fn full_durability_runs_alone_with_synchronous_full() {
    let (_d, store) = open();
    let sync = |c: &Connection| {
        c.query_row("PRAGMA synchronous", [], |r| r.get::<_, i64>(0))
            .db()
    };
    assert_eq!(store.write(Durability::Normal, sync).await.unwrap(), 1);
    assert_eq!(store.write(Durability::Full, sync).await.unwrap(), 2);
    assert_eq!(
        store.write(Durability::Normal, sync).await.unwrap(),
        1,
        "reset to NORMAL after the durable write"
    );
    // A failing durable write also resets the pragma.
    let failed: Result<(), _> = store
        .write(Durability::Full, |_| {
            Err(StoreError::InvalidArgument("x".into()))
        })
        .await;
    assert!(failed.is_err());
    assert_eq!(store.write(Durability::Normal, sync).await.unwrap(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_durable_command_never_joins_a_group() {
    let (_d, store) = open();
    let release = block_writer(&store);
    let mut rxs = Vec::new();
    let dur = [Durability::Normal, Durability::Full, Durability::Normal];
    for (n, d) in dur.into_iter().enumerate() {
        let (cmd, rx) = writer::command(d, move |c: &Connection| {
            let s: i64 = c.query_row("PRAGMA synchronous", [], |r| r.get(0)).db()?;
            tx_create_task(c, &new_task(n), 1)?;
            Ok(s)
        });
        assert!(store.try_submit(cmd).unwrap().is_ok());
        rxs.push(rx);
    }
    let before = store.stats().commits.load(Ordering::Relaxed);
    release.send(()).unwrap();
    let mut seen = Vec::new();
    for rx in rxs {
        seen.push(rx.await.unwrap().unwrap());
    }
    assert_eq!(seen, [1, 2, 1]);
    // blocker + one group of one + the durable one + one more = 4 commits in total.
    assert_eq!(store.stats().commits.load(Ordering::Relaxed) - before, 4);
}

#[tokio::test]
async fn record_process_is_durable_and_fenced() {
    let (_d, store) = seeded(1).await;
    let c = store.claim_next("a", 0, 100).await.unwrap().unwrap();
    store
        .record_process(&c.run_id, c.fence, 4242, 4240, 99, 3)
        .await
        .unwrap();
    let run = store.get_run(&c.run_id).await.unwrap().unwrap();
    assert_eq!((run.pid, run.pgid), (Some(4242), Some(4240)));
    assert_eq!(run.process_start, Some(99));
    assert!(
        event_kinds(&store)
            .await
            .contains(&"run.process_recorded".to_owned())
    );
    assert!(matches!(
        store
            .record_process(&c.run_id, 99, 1, 1, 1, 4)
            .await
            .unwrap_err(),
        StoreError::StaleFence { .. }
    ));
}

#[tokio::test]
async fn a_run_row_carries_its_task_and_its_last_change() {
    let (_d, store) = open();
    let mut task = new_task(0);
    task.spec = serde_json::json!({ "cwd": "/w" });
    store.create_task(task, 5).await.unwrap();
    store
        .enqueue_run(&task_id(0), &run_id(0), &exec(), 7)
        .await
        .unwrap();
    let run = store.get_run(&run_id(0)).await.unwrap().unwrap();
    assert_eq!(run.goal, "goal 0");
    assert_eq!(run.spec, serde_json::json!({ "cwd": "/w" }));
    assert_eq!((run.created_at, run.updated_at), (7, 7));
    let c = store.claim_next("a", 40, 100).await.unwrap().unwrap();
    store
        .transition(&c.run_id, c.fence, RunStatus::Running, 50)
        .await
        .unwrap();
    let run = store.get_run(&run_id(0)).await.unwrap().unwrap();
    assert_eq!(run.updated_at, 50);
}

#[tokio::test]
async fn checkpoint_runs_once_after_idle_and_never_on_a_timer() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open_with(
        dir.path().join("reno.db"),
        StoreConfig {
            checkpoint_idle: Duration::from_millis(100),
            ..StoreConfig::default()
        },
    )
    .unwrap();
    // No write yet: nothing to checkpoint, however long we wait.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(store.stats().checkpoints.load(Ordering::Relaxed), 0);

    store.create_task(new_task(0), 0).await.unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while store.stats().checkpoints.load(Ordering::Relaxed) == 0 {
        assert!(std::time::Instant::now() < deadline, "no checkpoint");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Once, not repeatedly.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(store.stats().checkpoints.load(Ordering::Relaxed), 1);
    let wal = dir.path().join("reno.db-wal");
    assert_eq!(std::fs::metadata(wal).map(|m| m.len()).unwrap_or(0), 0);
}

#[test]
fn journal_writer_reports_its_next_seq() {
    let dir = tempfile::tempdir().unwrap();
    let journals = crate::Journals::new(dir.path());
    let mut w = journals.writer(&run_id(0)).unwrap();
    assert_eq!(w.next_seq(), Seq(1));
    let done = reno_proto::ExecEvent::Text { text: "a".into() };
    w.append(&done).unwrap();
    assert_eq!(w.next_seq(), Seq(2));
    w.flush().unwrap();
    drop(w);
    assert_eq!(journals.writer(&run_id(0)).unwrap().next_seq(), Seq(2));
}

#[tokio::test]
async fn data_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reno.db");
    {
        let store = Store::open(&path).unwrap();
        store.create_task(new_task(0), 1).await.unwrap();
        store
            .enqueue_run(&task_id(0), &run_id(0), &exec(), 2)
            .await
            .unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(store.list_runs(None).await.unwrap().len(), 1);
    assert_eq!(
        store.journals().path(&run_id(0)),
        dir.path().join("journals/run_r0.jsonl")
    );
}

// ---------------------------------------------------------------------------------------------
// Model test
// ---------------------------------------------------------------------------------------------

mod model {
    use proptest::prelude::*;

    use super::*;

    #[derive(Debug, Clone)]
    enum Op {
        Enqueue,
        Claim {
            owner: u8,
            lease: u64,
        },
        Heartbeat {
            run: usize,
            stale: bool,
            lease: u64,
        },
        Transition {
            run: usize,
            pick: usize,
            legal: bool,
            stale: bool,
        },
        Process {
            run: usize,
            stale: bool,
        },
        Advance(u64),
    }

    #[derive(Debug, PartialEq)]
    enum Out {
        Claimed(Option<(usize, u64)>),
        Ok,
        NotFound,
        Stale,
        Illegal,
        NotLeased,
        Skipped,
    }

    #[derive(Debug, Clone, PartialEq)]
    struct MRun {
        status: RunStatus,
        fence: u64,
        owner: Option<String>,
        expires: Option<i64>,
    }

    #[derive(Default)]
    struct Model {
        runs: Vec<MRun>,
        now: i64,
        events: usize,
    }

    fn holds_lease(s: RunStatus) -> bool {
        use RunStatus::*;
        matches!(
            s,
            Leased | Running | WaitingInput | WaitingApproval | Evaluating
        )
    }

    impl Model {
        fn claim(&mut self, owner: &str, lease: u64) -> Option<(usize, u64)> {
            let now = self.now;
            let i = self.runs.iter().position(|r| {
                r.status == RunStatus::Queued
                    || (r.status == RunStatus::Leased && r.expires.is_some_and(|e| e <= now))
            })?;
            let r = &mut self.runs[i];
            r.status = RunStatus::Leased;
            r.fence += 1;
            r.owner = Some(owner.to_owned());
            r.expires = Some(now + lease as i64);
            self.events += 1;
            Some((i, r.fence))
        }

        fn fenced(&self, i: usize, fence: u64) -> Result<(), Out> {
            if self.runs[i].fence == fence {
                Ok(())
            } else {
                Err(Out::Stale)
            }
        }

        fn heartbeat(&mut self, i: usize, fence: u64, lease: u64) -> Out {
            if let Err(o) = self.fenced(i, fence) {
                return o;
            }
            if !holds_lease(self.runs[i].status) {
                return Out::NotLeased;
            }
            self.runs[i].expires = Some(self.now + lease as i64);
            Out::Ok
        }

        fn transition(&mut self, i: usize, fence: u64, to: RunStatus) -> Out {
            if let Err(o) = self.fenced(i, fence) {
                return o;
            }
            let r = &mut self.runs[i];
            if !r.status.can_transition_to(to) {
                return Out::Illegal;
            }
            r.status = to;
            if matches!(
                to,
                RunStatus::Queued
                    | RunStatus::Interrupted
                    | RunStatus::Succeeded
                    | RunStatus::Failed
                    | RunStatus::Cancelled
            ) {
                r.owner = None;
                r.expires = None;
            }
            self.events += 1 + usize::from(to.is_terminal());
            Out::Ok
        }
    }

    fn classify(e: &StoreError) -> Out {
        match e {
            StoreError::NotFound { .. } => Out::NotFound,
            StoreError::StaleFence { .. } => Out::Stale,
            StoreError::IllegalTransition { .. } => Out::Illegal,
            StoreError::NotLeased { .. } => Out::NotLeased,
            other => panic!("unexpected error {other}"),
        }
    }

    fn out(r: Result<(), StoreError>) -> Out {
        r.map_or_else(|e| classify(&e), |()| Out::Ok)
    }

    fn op() -> impl Strategy<Value = Op> {
        prop_oneof![
            3 => Just(Op::Enqueue),
            5 => (0u8..3, 1u64..50).prop_map(|(owner, lease)| Op::Claim { owner, lease }),
            3 => (any::<usize>(), prop::bool::weighted(0.2), 1u64..50)
                .prop_map(|(run, stale, lease)| Op::Heartbeat { run, stale, lease }),
            8 => (any::<usize>(), any::<usize>(), prop::bool::weighted(0.8), prop::bool::weighted(0.1))
                .prop_map(|(run, pick, legal, stale)| Op::Transition { run, pick, legal, stale }),
            1 => (any::<usize>(), prop::bool::weighted(0.2))
                .prop_map(|(run, stale)| Op::Process { run, stale }),
            4 => (0u64..60).prop_map(Op::Advance),
        ]
    }

    fn cases() -> u32 {
        std::env::var("PROPTEST_CASES")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(1000)
    }

    async fn check(ops: Vec<Op>) -> Result<(), TestCaseError> {
        let (_dir, store) = open();
        store.create_task(new_task(0), 0).await.unwrap();
        let mut m = Model {
            now: 1000,
            events: 1,
            ..Model::default()
        };

        for op in ops {
            let now = m.now;
            let (want, got) = match op {
                Op::Enqueue => {
                    let n = m.runs.len();
                    store
                        .enqueue_run(&task_id(0), &run_id(n), &exec(), now)
                        .await
                        .unwrap();
                    m.runs.push(MRun {
                        status: RunStatus::Queued,
                        fence: 0,
                        owner: None,
                        expires: None,
                    });
                    m.events += 1;
                    continue;
                }
                Op::Claim { owner, lease } => {
                    let name = format!("o{owner}");
                    let want = Out::Claimed(m.claim(&name, lease));
                    let got = store.claim_next(&name, now, lease).await.unwrap();
                    (
                        want,
                        Out::Claimed(got.map(|c| {
                            let idx = (0..m.runs.len()).find(|&i| run_id(i) == c.run_id).unwrap();
                            (idx, c.fence)
                        })),
                    )
                }
                Op::Heartbeat { run, stale, lease } => {
                    if m.runs.is_empty() {
                        continue;
                    }
                    let i = run % m.runs.len();
                    let f = pick_fence(m.runs[i].fence, stale);
                    (
                        m.heartbeat(i, f, lease),
                        out(store.heartbeat(&run_id(i), f, now, lease).await),
                    )
                }
                Op::Transition {
                    run,
                    pick,
                    legal,
                    stale,
                } => {
                    if m.runs.is_empty() {
                        continue;
                    }
                    let i = run % m.runs.len();
                    let from = m.runs[i].status;
                    let to = if legal {
                        let targets: Vec<_> = RunStatus::ALL
                            .into_iter()
                            .filter(|&t| from.can_transition_to(t))
                            .collect();
                        if targets.is_empty() {
                            RunStatus::ALL[pick % 10]
                        } else {
                            targets[pick % targets.len()]
                        }
                    } else {
                        RunStatus::ALL[pick % 10]
                    };
                    let f = pick_fence(m.runs[i].fence, stale);
                    (
                        m.transition(i, f, to),
                        out(store.transition(&run_id(i), f, to, now).await),
                    )
                }
                Op::Process { run, stale } => {
                    if m.runs.is_empty() {
                        continue;
                    }
                    let i = run % m.runs.len();
                    let f = pick_fence(m.runs[i].fence, stale);
                    let want = match m.fenced(i, f) {
                        Ok(()) => {
                            m.events += 1;
                            Out::Ok
                        }
                        Err(o) => o,
                    };
                    (
                        want,
                        out(store.record_process(&run_id(i), f, 100, 100, 5, now).await),
                    )
                }
                Op::Advance(d) => {
                    m.now += d as i64;
                    (Out::Skipped, Out::Skipped)
                }
            };
            prop_assert_eq!(want, got);
        }

        let real = store.list_runs(None).await.unwrap();
        prop_assert_eq!(real.len(), m.runs.len());
        for (i, (r, mr)) in real.iter().zip(&m.runs).enumerate() {
            prop_assert_eq!(&r.id, &run_id(i));
            prop_assert_eq!(r.status, mr.status);
            prop_assert_eq!(r.lease_fence, mr.fence);
            prop_assert_eq!(&r.lease_owner, &mr.owner);
            prop_assert_eq!(r.lease_expires_at, mr.expires);
        }
        let events = store.events_since(Seq(0), 1_000_000).await.unwrap();
        prop_assert_eq!(events.len(), m.events);
        let nonterminal = store.nonterminal_runs().await.unwrap().len();
        prop_assert_eq!(
            nonterminal,
            m.runs.iter().filter(|r| !r.status.is_terminal()).count()
        );
        Ok(())
    }

    fn pick_fence(current: u64, stale: bool) -> u64 {
        match (stale, current) {
            (false, c) => c,
            (true, 0) => 1,
            (true, c) => c - 1,
        }
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(cases()))]

        /// Random lifecycle sequences behave exactly like the in-memory model: the same claims,
        /// fences, statuses, leases and event counts.
        #[test]
        fn store_matches_model(ops in prop::collection::vec(op(), 1..60)) {
            let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
            rt.block_on(check(ops))?;
        }
    }
}
