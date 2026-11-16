//! Behaviour tests for budgets (T1.7): the claim filter (concurrency, pauses, the token
//! ceiling), token accounting by day, and the pauses. `now` and the day are values the test
//! owns.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use reno_proto::defs::{BotPin, DefKind, content_hash};
use reno_proto::{ExecutorId, RunId, RunStatus, Seq, TaskId};
use serde_json::json;
use tempfile::TempDir;

use crate::{ClaimFilter, GLOBAL_PAUSE, NewTask, Store, StoreError, Tokens};

const LEASE: u64 = 60_000;

fn exec() -> ExecutorId {
    "mock".parse().unwrap()
}

fn run_id(name: &str) -> RunId {
    RunId::from_suffix(name).unwrap()
}

fn filter(day: &str) -> ClaimFilter {
    ClaimFilter {
        day: day.to_owned(),
        paused_bots: Vec::new(),
    }
}

async fn put_bot(store: &Store, name: &str, concurrent: Option<u32>, ceiling: Option<u64>) -> u32 {
    let mut budget = json!({});
    if let Some(n) = concurrent {
        budget["max_concurrent_runs"] = json!(n);
    }
    if let Some(n) = ceiling {
        budget["daily_token_ceiling"] = json!(n);
    }
    let body = json!({
        "name": name, "instructions": "x", "executors": ["mock"], "budget": budget,
    });
    store
        .put_definition(DefKind::Bot, name, &content_hash(&body), &body, None, 1)
        .await
        .unwrap()
        .0
}

/// Queues a run of `bot` (or of no bot) named `name`, created at `at`.
async fn queue(store: &Store, name: &str, bot: Option<(&str, u32)>, at: i64) -> RunId {
    let task = TaskId::from_suffix(name).unwrap();
    store
        .create_task(
            NewTask {
                id: task.clone(),
                goal: name.to_owned(),
                class: None,
                spec: json!({}),
            },
            at,
        )
        .await
        .unwrap();
    let run = run_id(name);
    match bot {
        Some((bot, version)) => {
            let pin = BotPin {
                name: bot.to_owned(),
                version,
            };
            store
                .enqueue_pinned_run(&task, &run, &exec(), &pin, at)
                .await
                .unwrap();
        }
        None => store.enqueue_run(&task, &run, &exec(), at).await.unwrap(),
    }
    run
}

async fn open() -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("reno.db")).unwrap();
    (dir, store)
}

async fn claim(store: &Store, now: i64, filter: &ClaimFilter) -> Option<RunId> {
    store
        .claim_next_filtered("test", now, LEASE, filter)
        .await
        .unwrap()
        .map(|c| c.run_id)
}

/// Runs a claimed run to `succeeded`.
async fn finish(store: &Store, run: &RunId, now: i64) {
    let fence = store.get_run(run).await.unwrap().unwrap().lease_fence;
    for to in [
        RunStatus::Running,
        RunStatus::Evaluating,
        RunStatus::Succeeded,
    ] {
        store.transition(run, fence, to, now).await.unwrap();
    }
}

#[tokio::test]
async fn a_bot_at_its_concurrency_limit_is_passed_over_and_the_run_behind_it_goes() {
    let (_d, store) = open().await;
    let v = put_bot(&store, "keeper", Some(1), None).await;
    let first = queue(&store, "a1", Some(("keeper", v)), 10).await;
    let second = queue(&store, "a2", Some(("keeper", v)), 11).await;
    let other = queue(&store, "b1", None, 12).await;

    let f = filter("2026-11-16");
    assert_eq!(claim(&store, 100, &f).await, Some(first.clone()));
    // keeper holds its one slot: its second run is not taken, the unrelated run behind it is.
    assert_eq!(claim(&store, 101, &f).await, Some(other));
    assert_eq!(claim(&store, 102, &f).await, None);
    assert_eq!(
        store.get_run(&second).await.unwrap().unwrap().status,
        RunStatus::Queued,
        "a run held back by a limit is not failed"
    );

    // The slot frees when the run ends.
    finish(&store, &first, 103).await;
    assert_eq!(claim(&store, 104, &f).await, Some(second));
}

#[tokio::test]
async fn the_limit_is_the_one_of_the_version_the_run_is_pinned_to() {
    let (_d, store) = open().await;
    let v1 = put_bot(&store, "keeper", Some(1), None).await;
    let body = json!({
        "name": "keeper", "instructions": "y", "executors": ["mock"],
        "budget": {"max_concurrent_runs": 2},
    });
    let v2 = store
        .put_definition(DefKind::Bot, "keeper", &content_hash(&body), &body, None, 2)
        .await
        .unwrap()
        .0;
    assert_eq!((v1, v2), (1, 2));
    queue(&store, "a1", Some(("keeper", v2)), 10).await;
    let b = queue(&store, "a2", Some(("keeper", v2)), 11).await;
    let old = queue(&store, "a3", Some(("keeper", v1)), 12).await;
    let f = filter("d");
    assert!(claim(&store, 100, &f).await.is_some());
    assert_eq!(
        claim(&store, 101, &f).await,
        Some(b),
        "version 2 allows two"
    );
    // The run pinned to version 1 sees two live runs against a limit of one.
    assert_eq!(claim(&store, 102, &f).await, None);
    assert_eq!(
        store.get_run(&old).await.unwrap().unwrap().status,
        RunStatus::Queued
    );
}

#[tokio::test]
async fn a_leased_run_whose_lease_ran_out_holds_no_slot() {
    let (_d, store) = open().await;
    let v = put_bot(&store, "keeper", Some(1), None).await;
    let dead = queue(&store, "a1", Some(("keeper", v)), 10).await;
    let f = filter("d");
    assert_eq!(claim(&store, 100, &f).await, Some(dead.clone()));
    // Its claimer died: once the lease has run out the same run is claimable again, even though
    // it is itself "live" and the bot's limit is one.
    assert_eq!(claim(&store, 100 + LEASE as i64 + 1, &f).await, Some(dead));
}

#[tokio::test]
async fn an_unpinned_run_and_an_unstored_pin_are_never_limited() {
    let (_d, store) = open().await;
    for n in 0..3 {
        queue(&store, &format!("n{n}"), None, 10 + n).await;
    }
    let f = filter("d");
    for _ in 0..3 {
        assert!(claim(&store, 100, &f).await.is_some());
    }
}

#[tokio::test]
async fn a_paused_bot_is_passed_over() {
    let (_d, store) = open().await;
    let v = put_bot(&store, "keeper", Some(4), None).await;
    let held = queue(&store, "a1", Some(("keeper", v)), 10).await;
    let free = queue(&store, "b1", None, 11).await;
    let mut f = filter("d");
    f.paused_bots = vec!["keeper".to_owned()];
    assert_eq!(claim(&store, 100, &f).await, Some(free));
    assert_eq!(claim(&store, 101, &f).await, None);
    f.paused_bots.clear();
    assert_eq!(claim(&store, 102, &f).await, Some(held));
}

#[tokio::test]
async fn tokens_add_up_per_bot_per_day_and_per_run() {
    let (_d, store) = open().await;
    let v = put_bot(&store, "keeper", Some(4), None).await;
    let a = queue(&store, "a1", Some(("keeper", v)), 10).await;
    let b = queue(&store, "a2", Some(("keeper", v)), 11).await;
    let f = filter("2026-11-16");
    claim(&store, 100, &f).await.unwrap();
    claim(&store, 100, &f).await.unwrap();
    let add = |input, output| Tokens { input, output };

    let total = store
        .record_usage(&a, 1, Some("keeper"), "2026-11-16", add(10, 5), 200)
        .await
        .unwrap();
    assert_eq!(total, add(10, 5));
    let total = store
        .record_usage(&b, 1, Some("keeper"), "2026-11-16", add(1, 2), 201)
        .await
        .unwrap();
    assert_eq!(total, add(11, 7), "the bot's total is over both runs");
    store
        .record_usage(&a, 1, Some("keeper"), "2026-11-16", add(4, 0), 202)
        .await
        .unwrap();
    // The next day starts again from nothing; the old day stays.
    let total = store
        .record_usage(&a, 1, Some("keeper"), "2026-11-17", add(3, 3), 203)
        .await
        .unwrap();
    assert_eq!(total, add(3, 3));

    assert_eq!(store.run_usage(&a).await.unwrap(), add(17, 8));
    assert_eq!(store.run_usage(&b).await.unwrap(), add(1, 2));
    let day = store.bot_usage("2026-11-16").await.unwrap();
    assert_eq!(day.get("keeper"), Some(&add(15, 7)));
    assert_eq!(day["keeper"].total(), 22);
    assert_eq!(
        store.bot_usage("2026-11-17").await.unwrap()["keeper"],
        add(3, 3)
    );
    assert!(store.bot_usage("2026-11-18").await.unwrap().is_empty());
}

#[tokio::test]
async fn usage_of_a_run_with_no_bot_counts_for_the_run_only() {
    let (_d, store) = open().await;
    let run = queue(&store, "n1", None, 10).await;
    claim(&store, 100, &filter("d")).await.unwrap();
    let total = store
        .record_usage(
            &run,
            1,
            None,
            "d",
            Tokens {
                input: 2,
                output: 3,
            },
            200,
        )
        .await
        .unwrap();
    assert_eq!(
        total,
        Tokens {
            input: 2,
            output: 3
        }
    );
    assert!(store.bot_usage("d").await.unwrap().is_empty());
}

#[tokio::test]
async fn usage_is_fenced_and_leaves_an_event() {
    let (_d, store) = open().await;
    let run = queue(&store, "n1", None, 10).await;
    claim(&store, 100, &filter("d")).await.unwrap();
    let stale = store
        .record_usage(
            &run,
            9,
            None,
            "d",
            Tokens {
                input: 1,
                output: 1,
            },
            200,
        )
        .await;
    assert!(
        matches!(stale, Err(StoreError::StaleFence { .. })),
        "{stale:?}"
    );
    store
        .record_usage(
            &run,
            1,
            None,
            "d",
            Tokens {
                input: 1,
                output: 1,
            },
            201,
        )
        .await
        .unwrap();
    let kinds: Vec<String> = store
        .events_since(Seq(0), 1000)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert!(kinds.contains(&"run.usage".to_owned()), "{kinds:?}");
    assert_eq!(store.run_usage(&run).await.unwrap().total(), 2);
}

#[tokio::test]
async fn a_bot_at_its_token_ceiling_is_passed_over_until_the_day_changes() {
    let (_d, store) = open().await;
    let v = put_bot(&store, "keeper", Some(4), Some(100)).await;
    let first = queue(&store, "a1", Some(("keeper", v)), 10).await;
    let second = queue(&store, "a2", Some(("keeper", v)), 11).await;
    let day = filter("2026-11-16");
    assert_eq!(claim(&store, 100, &day).await, Some(first.clone()));
    store
        .record_usage(
            &first,
            1,
            Some("keeper"),
            "2026-11-16",
            Tokens {
                input: 60,
                output: 39,
            },
            150,
        )
        .await
        .unwrap();
    // 99 of 100: still allowed.
    assert!(
        store
            .token_blocked_bots("2026-11-16")
            .await
            .unwrap()
            .is_empty()
    );
    store
        .record_usage(
            &first,
            1,
            Some("keeper"),
            "2026-11-16",
            Tokens {
                input: 0,
                output: 1,
            },
            151,
        )
        .await
        .unwrap();
    // At the ceiling exactly: refused.
    assert_eq!(claim(&store, 152, &day).await, None);
    assert_eq!(
        store.token_blocked_bots("2026-11-16").await.unwrap(),
        ["keeper"]
    );
    // Tomorrow is another day.
    assert_eq!(
        claim(&store, 153, &filter("2026-11-17")).await,
        Some(second)
    );
}

#[tokio::test]
async fn pauses_are_kept_and_logged_once() {
    let (dir, store) = open().await;
    assert!(store.pauses().await.unwrap().is_empty());
    assert!(store.pause(GLOBAL_PAUSE, 10).await.unwrap());
    assert!(
        !store.pause(GLOBAL_PAUSE, 11).await.unwrap(),
        "already paused"
    );
    assert!(store.pause("keeper", 12).await.unwrap());
    let kept = store.pauses().await.unwrap();
    assert_eq!(
        kept.get(GLOBAL_PAUSE),
        Some(&10),
        "the first pause time stays"
    );
    assert_eq!(kept.get("keeper"), Some(&12));

    // A new handle over the same file sees them: they outlive the daemon.
    drop(store);
    let store = Store::open(dir.path().join("reno.db")).unwrap();
    assert_eq!(store.pauses().await.unwrap().len(), 2);
    assert!(store.resume(GLOBAL_PAUSE, 20).await.unwrap());
    assert!(!store.resume(GLOBAL_PAUSE, 21).await.unwrap(), "not paused");
    assert_eq!(
        store.pauses().await.unwrap().keys().collect::<Vec<_>>(),
        ["keeper"]
    );
    let kinds: Vec<String> = store
        .events_since(Seq(0), 1000)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(
        kinds,
        ["budget.paused", "budget.paused", "budget.resumed"],
        "a repeat leaves no event"
    );
}

#[tokio::test]
async fn occupancy_counts_slots_and_queue_by_bot() {
    let (_d, store) = open().await;
    let v = put_bot(&store, "keeper", Some(1), None).await;
    queue(&store, "a1", Some(("keeper", v)), 10).await;
    queue(&store, "a2", Some(("keeper", v)), 11).await;
    queue(&store, "n1", None, 12).await;
    claim(&store, 100, &filter("d")).await.unwrap();
    let busy = store.occupancy().await.unwrap();
    assert_eq!(busy.len(), 1, "a run with no bot is in nobody's count");
    assert_eq!((busy["keeper"].live, busy["keeper"].queued), (1, 1));
}

#[tokio::test]
async fn skipping_an_occurrence_moves_the_trigger_on_and_says_why() {
    let (_d, store) = open().await;
    let row = crate::TriggerRow {
        name: "nightly".to_owned(),
        def_version: 1,
        schedule: json!({"kind": "every", "every": "1h"}),
        enabled: true,
        next_due_at: Some(1_000),
        last_fired_at: None,
        last_run: None,
        failures: 0,
        backoff_until: None,
    };
    store.plan_triggers(vec![row], vec![], 2).await.unwrap();
    store
        .skip_trigger_occurrence("nightly", 1_000, 3, Some(5_000), "budget_tokens", 1_001)
        .await
        .unwrap();
    let state = store.list_triggers().await.unwrap().remove(0);
    assert_eq!(state.next_due_at, Some(5_000));
    assert_eq!(state.last_fired_at, None, "nothing fired");
    assert!(store.firings(Some("nightly"), 10).await.unwrap().is_empty());
    let events = store.events_since(Seq(0), 100).await.unwrap();
    let skipped = events.iter().find(|e| e.kind == "trigger.skipped").unwrap();
    assert_eq!(skipped.data["reason"], "budget_tokens");
    assert_eq!(skipped.data["missed"], 3);
    let missing = store
        .skip_trigger_occurrence("nope", 1, 1, None, "budget_tokens", 2)
        .await;
    assert!(matches!(missing, Err(StoreError::NotFound { .. })));
}

/// The day label the kernel derives from its clock's zone, fed to the store: tokens used either
/// side of local midnight, and across both DST changes of a zone, land on the right days.
#[tokio::test]
async fn the_local_day_boundary_splits_the_tokens_across_midnight_and_dst() {
    use reno_proto::LocalZone;
    let ny = LocalZone::named("America/New_York").unwrap();
    let at = |text: &str| text.parse::<jiff::Timestamp>().unwrap().as_millisecond();

    let (_d, store) = open().await;
    let v = put_bot(&store, "keeper", Some(4), None).await;
    let run = queue(&store, "a1", Some(("keeper", v)), 10).await;
    claim(&store, 100, &filter("x")).await.unwrap();
    let add = Tokens {
        input: 1,
        output: 1,
    };
    // One minute before and after local midnight, a spring-forward day and a fall-back day.
    let moments = [
        "2023-11-14T04:59:00Z", // 23:59 on the 13th
        "2023-11-14T05:01:00Z", // 00:01 on the 14th
        "2024-03-10T04:59:00Z", // 23:59 on the 9th
        "2024-03-10T05:01:00Z", // 00:01 on the 10th, a 23-hour day
        "2024-03-11T03:59:00Z", // 23:59 on the 10th (EDT)
        "2024-03-11T04:01:00Z", // 00:01 on the 11th
        "2024-11-03T03:59:00Z", // 23:59 on the 2nd
        "2024-11-03T04:01:00Z", // 00:01 on the 3rd, a 25-hour day
        "2024-11-04T04:59:00Z", // 23:59 on the 3rd (EST)
        "2024-11-04T05:01:00Z", // 00:01 on the 4th
    ];
    for (n, moment) in moments.iter().enumerate() {
        let day = ny.day_of(at(moment));
        store
            .record_usage(&run, 1, Some("keeper"), &day, add, 200 + n as i64)
            .await
            .unwrap();
    }
    let days = [
        "2023-11-13",
        "2023-11-14",
        "2024-03-09",
        "2024-03-10",
        "2024-03-11",
        "2024-11-02",
        "2024-11-03",
        "2024-11-04",
    ];
    for day in days {
        let used = store.bot_usage(day).await.unwrap();
        let expect = match day {
            "2024-03-10" | "2024-11-03" => 4, // both sides of the day's own change: 2 events
            _ => 2,
        };
        assert_eq!(used["keeper"].total(), expect, "{day}");
    }
    assert_eq!(store.run_usage(&run).await.unwrap().total(), 20);
}
