//! Budgets (T1.7): concurrent runs per bot, wall time, the daily token ceiling and the kill
//! switch. Time is paused and the wall clock is a `ManualClock`; the executor is the mock, which
//! plays `Usage` events and sleeps on the paused timer.

use std::path::PathBuf;
use std::time::Duration;

use reno_harness::exec::MockStep;
use reno_proto::defs::DefKind;
use reno_proto::{
    BudgetKind, BudgetStop, ExecEvent, LocalZone, RunId, RunStatus, Seq, StopReason, TriggerInfo,
};

use super::defs::{FakePort, file, setup};
use super::{Fx, START_MS, config, mock, registry, until};
use crate::db::Armed;
use crate::{Clock as _, KernelConfig, KernelError};

const MIN: i64 = 60_000;
const HOUR: i64 = 3_600_000;

/// A bot `keeper` with this `[budget]` body, and a trigger `tick` every hour for it.
fn defs(budget: &str) -> Vec<reno_proto::defs::DefFile> {
    vec![
        file(
            DefKind::Bot,
            "keeper",
            &format!(
                "name = \"keeper\"\ninstructions = \"x\"\nexecutors = [\"mock\"]\n[budget]\n{budget}\n"
            ),
        ),
        file(
            DefKind::Bot,
            "scribe",
            "name = \"scribe\"\ninstructions = \"y\"\nexecutors = [\"mock\"]\n[budget]\nmax_concurrent_runs = 4\n",
        ),
        file(
            DefKind::Trigger,
            "tick",
            "name = \"tick\"\n[when]\nkind = \"every\"\nevery = \"1h\"\n[target]\nbot = \"keeper\"\nprompt = \"tick\"\n",
        ),
    ]
}

async fn fx_with(budget: &str, steps: impl IntoIterator<Item = MockStep>, cfg: KernelConfig) -> Fx {
    let port = FakePort::with(defs(budget));
    Fx::with_definitions(cfg, registry(&[mock(steps)]), setup(&port)).await
}

async fn as_bot(fx: &Fx, bot: &str) -> RunId {
    fx.kernel
        .submit_for_bot(bot, "go", None, PathBuf::from("/work"), None)
        .await
        .unwrap()
}

fn usage(input_tokens: u64, output_tokens: u64) -> MockStep {
    MockStep::Emit(ExecEvent::Usage {
        input_tokens,
        output_tokens,
    })
}

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// The budget stop a run's journal ends with, and the stop reason it ended with. The stop is
/// the typed `reason`; the diagnostic still says the same in its legacy form.
async fn ended_by_budget(fx: &Fx, run: &RunId) -> (StopReason, Option<BudgetStop>) {
    let journal = fx.journal_with_done(run).await;
    match journal.last() {
        Some((
            _,
            ExecEvent::Done {
                stop,
                diagnostic,
                reason,
                ..
            },
        )) => {
            let typed = reason.as_ref().and_then(reno_proto::StopDetail::budget);
            let legacy = diagnostic.as_deref().and_then(BudgetStop::from_diagnostic);
            assert_eq!(typed, legacy, "the typed reason and the diagnostic agree");
            (*stop, typed)
        }
        other => panic!("no Done: {other:?}"),
    }
}

async fn events_of(fx: &Fx, prefix: &str) -> Vec<(String, serde_json::Value)> {
    fx.store
        .events_since(Seq(0), 100_000)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind.starts_with(prefix))
        .map(|e| (e.kind, e.data))
        .collect()
}

async fn step(fx: &Fx, ms: i64) {
    fx.clock.advance(ms);
    tokio::time::advance(Duration::from_millis(u64::try_from(ms).unwrap())).await;
    // Let the tasks that were woken run.
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
}

fn trigger_info(fx: &Fx) -> TriggerInfo {
    fx.kernel.list_triggers().remove(0)
}

// ---- concurrent runs --------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_bot_at_its_limit_keeps_its_next_run_queued_until_a_slot_frees() {
    let fx = fx_with(
        "max_concurrent_runs = 1",
        [MockStep::Sleep(secs(20))],
        config(8),
    )
    .await;
    let first = as_bot(&fx, "keeper").await;
    let second = as_bot(&fx, "keeper").await;
    fx.wait_status(&first, RunStatus::Running).await;
    // Plenty of kernel capacity, yet the second run is not claimed, and it is not failed.
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
    assert_eq!(fx.status(&second).await, RunStatus::Queued);
    // The end of the first run is what wakes the dispatcher: no timer, no poll.
    fx.wait_status(&first, RunStatus::Succeeded).await;
    fx.wait_status(&second, RunStatus::Running).await;
    fx.wait_status(&second, RunStatus::Succeeded).await;
}

#[tokio::test(start_paused = true)]
async fn a_bot_at_its_limit_does_not_block_other_bots_behind_it() {
    let fx = fx_with(
        "max_concurrent_runs = 1",
        [MockStep::Sleep(secs(3600))],
        config(8),
    )
    .await;
    let first = as_bot(&fx, "keeper").await;
    let held = as_bot(&fx, "keeper").await;
    let other = as_bot(&fx, "scribe").await;
    let plain = fx.submit("no bot").await;
    fx.wait_status(&first, RunStatus::Running).await;
    fx.wait_status(&other, RunStatus::Running).await;
    fx.wait_status(&plain, RunStatus::Running).await;
    assert_eq!(fx.status(&held).await, RunStatus::Queued);
    let status = fx.kernel.budget_status().await.unwrap();
    let keeper = status.bots.iter().find(|b| b.bot == "keeper").unwrap();
    assert_eq!(
        (
            keeper.live_runs,
            keeper.queued_runs,
            keeper.max_concurrent_runs
        ),
        (1, 1, 1)
    );
}

#[tokio::test(start_paused = true)]
async fn a_bot_may_have_as_many_runs_live_as_its_limit() {
    let fx = fx_with(
        "max_concurrent_runs = 2",
        [MockStep::Sleep(secs(3600))],
        config(8),
    )
    .await;
    let runs = [
        as_bot(&fx, "keeper").await,
        as_bot(&fx, "keeper").await,
        as_bot(&fx, "keeper").await,
    ];
    fx.wait_status(&runs[0], RunStatus::Running).await;
    fx.wait_status(&runs[1], RunStatus::Running).await;
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
    assert_eq!(fx.status(&runs[2]).await, RunStatus::Queued);
}

// ---- wall time --------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_run_over_its_wall_time_is_interrupted_with_a_typed_reason() {
    let fx = fx_with(
        "max_wall_time = \"1m\"",
        [MockStep::Sleep(secs(3600))],
        config(2),
    )
    .await;
    let run = as_bot(&fx, "keeper").await;
    fx.wait_status(&run, RunStatus::Running).await;
    fx.wait_armed(Armed::WallTime, 0).await;
    step(&fx, 59_000).await;
    assert_eq!(fx.status(&run).await, RunStatus::Running, "not yet");
    step(&fx, 2_000).await;
    fx.wait_status(&run, RunStatus::Interrupted).await;

    let (stop, budget) = ended_by_budget(&fx, &run).await;
    assert_eq!(stop, StopReason::Interrupted);
    let budget = budget.expect("the diagnostic is a BudgetStop");
    assert_eq!((budget.kind, budget.limit), (BudgetKind::WallTime, 60_000));
    assert!(budget.used >= 60_000, "{budget:?}");
}

#[tokio::test(start_paused = true)]
async fn a_run_with_no_bot_has_the_kernels_default_wall_time() {
    let cfg = KernelConfig {
        default_wall_time: secs(30),
        ..config(2)
    };
    let fx = Fx::new(cfg, registry(&[mock([MockStep::Sleep(secs(3600))])])).await;
    let run = fx.submit("long").await;
    fx.wait_status(&run, RunStatus::Running).await;
    fx.wait_armed(Armed::WallTime, 0).await;
    step(&fx, 31_000).await;
    fx.wait_status(&run, RunStatus::Interrupted).await;
    let (_, budget) = ended_by_budget(&fx, &run).await;
    assert_eq!(budget.unwrap().limit, 30_000);
}

#[tokio::test(start_paused = true)]
async fn a_run_that_finishes_in_time_ends_as_it_did() {
    let fx = fx_with(
        "max_wall_time = \"1m\"",
        [MockStep::Sleep(secs(10))],
        config(2),
    )
    .await;
    let run = as_bot(&fx, "keeper").await;
    fx.wait_status(&run, RunStatus::Succeeded).await;
    let (stop, budget) = ended_by_budget(&fx, &run).await;
    assert_eq!((stop, budget), (StopReason::Completed, None));
    // And the deadline died with it: nothing wakes later.
    step(&fx, 2 * MIN).await;
    assert_eq!(fx.status(&run).await, RunStatus::Succeeded);
}

// ---- tokens -----------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_run_that_crosses_the_ceiling_is_stopped_and_the_bot_is_refused_after() {
    let fx = fx_with(
        "max_concurrent_runs = 2\ndaily_token_ceiling = 100",
        [
            usage(40, 10),
            MockStep::Sleep(secs(1)),
            usage(50, 10),
            MockStep::Sleep(secs(3600)),
        ],
        config(8),
    )
    .await;
    let run = as_bot(&fx, "keeper").await;
    fx.wait_status(&run, RunStatus::Interrupted).await;
    let (stop, budget) = ended_by_budget(&fx, &run).await;
    assert_eq!(stop, StopReason::Interrupted);
    assert_eq!(
        budget,
        Some(BudgetStop {
            kind: BudgetKind::Tokens,
            limit: 100,
            used: 110
        })
    );
    // The day's count and the run's own count are kept.
    let day = fx.clock.zone().day_of(fx.clock.now_ms());
    assert_eq!(
        fx.store.bot_usage(&day).await.unwrap()["keeper"].total(),
        110
    );
    assert_eq!(fx.store.run_usage(&run).await.unwrap().total(), 110);

    // A new run of the bot is refused, typed, before anything is created.
    let runs_before = fx.store.list_runs(None).await.unwrap().len();
    let refused = fx
        .kernel
        .submit_for_bot("keeper", "again", None, PathBuf::from("/work"), None)
        .await;
    match refused {
        Err(KernelError::BudgetExhausted { bot, stop }) => {
            assert_eq!(bot, "keeper");
            assert_eq!(
                (stop.kind, stop.limit, stop.used),
                (BudgetKind::Tokens, 100, 110)
            );
        }
        other => panic!("expected a budget refusal, got {other:?}"),
    }
    assert_eq!(fx.store.list_runs(None).await.unwrap().len(), runs_before);
    // Another bot is not affected.
    as_bot(&fx, "scribe").await;
}

#[tokio::test(start_paused = true)]
async fn a_held_run_is_claimed_when_midnight_passes() {
    // A bot already at its ceiling for today, through a spender that ended.
    let fx = fx_with(
        "max_concurrent_runs = 1\ndaily_token_ceiling = 100",
        [usage(100, 0), MockStep::Sleep(secs(60))],
        config(8),
    )
    .await;
    let spender = as_bot(&fx, "keeper").await;
    fx.wait_status(&spender, RunStatus::Interrupted).await;
    // Submit is refused now, so queue through the store as a restart would find it: a run
    // enqueued before the ceiling was spent.
    let armed_before = fx.kernel.armed_count(Armed::Dispatcher);
    let (task, run) = (
        reno_proto::TaskId::from_suffix("late").unwrap(),
        reno_proto::RunId::from_suffix("late").unwrap(),
    );
    fx.store
        .create_task(
            reno_store::NewTask {
                id: task.clone(),
                goal: "late".into(),
                class: None,
                spec: serde_json::json!({"cwd": "/work", "model": null}),
            },
            START_MS,
        )
        .await
        .unwrap();
    let version = fx
        .kernel
        .current_defs()
        .version_of(DefKind::Bot, "keeper")
        .unwrap()
        .version;
    fx.store
        .enqueue_pinned_run(
            &task,
            &run,
            &"mock".parse().unwrap(),
            &reno_proto::defs::BotPin {
                name: "keeper".into(),
                version,
            },
            START_MS,
        )
        .await
        .unwrap();
    fx.kernel.cancel(&spender).await.ok();
    // Nudge the dispatcher the way a submit would.
    fx.kernel.resume(Some("keeper")).await.unwrap();
    // The dispatcher has looked at the queued run, left it held, and armed its midnight deadline.
    fx.wait_armed(Armed::Dispatcher, armed_before).await;
    assert_eq!(
        fx.status(&run).await,
        RunStatus::Queued,
        "the ceiling holds it"
    );

    // 22:13:20Z to the next UTC midnight is 1h46m40s: nothing before, a claim at it.
    step(&fx, HOUR + 46 * MIN).await;
    assert_eq!(fx.status(&run).await, RunStatus::Queued);
    step(&fx, 2 * MIN).await;
    // Claimed on the new day. (Its script spends the new day's ceiling at once, so it may already
    // be stopped by the time anyone looks.)
    until(async || (fx.status(&run).await != RunStatus::Queued).then_some(())).await;
}

#[tokio::test(start_paused = true)]
async fn tokens_are_counted_on_the_local_day_of_the_clocks_zone() {
    let fx = fx_with(
        "max_concurrent_runs = 2",
        [usage(5, 5), MockStep::Sleep(secs(1)), usage(1, 1)],
        config(8),
    )
    .await;
    fx.clock
        .set_zone(LocalZone::named("America/New_York").unwrap());
    // 22:13:20Z on 2023-11-14 is 17:13 on the 14th in New York.
    let first = as_bot(&fx, "scribe").await;
    fx.wait_status(&first, RunStatus::Succeeded).await;
    // Seven hours on it is 00:13 on the 15th there, though still the 15th in UTC too; go back to
    // a moment that separates the zones: 04:30Z is 23:30 on the 14th in New York but the 15th in
    // UTC.
    let to_0430 = 6 * HOUR + 16 * MIN + 40_000;
    step(&fx, to_0430).await;
    let second = as_bot(&fx, "scribe").await;
    fx.wait_status(&second, RunStatus::Succeeded).await;

    let tz = LocalZone::named("America/New_York").unwrap();
    assert_eq!(tz.day_of(fx.clock.now_ms()), "2023-11-14");
    let new_york = fx.store.bot_usage("2023-11-14").await.unwrap();
    assert_eq!(
        new_york["scribe"].total(),
        2 * (10 + 2),
        "both runs fall on New York's 14th"
    );
    assert!(fx.store.bot_usage("2023-11-15").await.unwrap().is_empty());
}

// ---- triggers under a spent ceiling ------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_trigger_whose_bot_is_out_of_tokens_skips_the_occurrence_without_backing_off() {
    let fx = fx_with(
        "max_concurrent_runs = 2\ndaily_token_ceiling = 100",
        [usage(100, 0), MockStep::Sleep(secs(3600))],
        config(8),
    )
    .await;
    let spender = as_bot(&fx, "keeper").await;
    fx.wait_status(&spender, RunStatus::Interrupted).await;
    let due = trigger_info(&fx).next_due_at_ms.unwrap();

    step(&fx, HOUR).await;
    until(async || (trigger_info(&fx).next_due_at_ms > Some(due)).then_some(())).await;

    let runs = fx.store.list_runs(None).await.unwrap();
    assert_eq!(runs.len(), 1, "no run was made for the occurrence");
    let skipped = events_of(&fx, "trigger.skipped").await;
    assert_eq!(skipped.len(), 1);
    assert_eq!(skipped[0].1["reason"], "budget_tokens");
    assert!(fx.store.firings(None, 10).await.unwrap().is_empty());
    let info = trigger_info(&fx);
    assert_eq!(info.failures, 0, "a spent budget is not an account failure");
    assert_eq!(info.backoff_until_ms, None);
    assert!(events_of(&fx, "trigger.backoff").await.is_empty());

    // Asking for it by hand is refused in the same typed way.
    let refused = fx.kernel.fire_trigger("tick").await;
    assert!(
        matches!(refused, Err(KernelError::BudgetExhausted { .. })),
        "{refused:?}"
    );
}

// ---- the kill switch --------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_global_pause_claims_nothing_and_resume_claims_what_waited() {
    let fx = fx_with(
        "max_concurrent_runs = 2",
        [MockStep::Sleep(secs(5))],
        config(8),
    )
    .await;
    let live = as_bot(&fx, "keeper").await;
    fx.wait_status(&live, RunStatus::Running).await;
    let paused = fx.kernel.pause(None, false).await.unwrap();
    assert!(paused.changed && paused.stopped.is_empty());
    assert!(
        !fx.kernel.pause(None, false).await.unwrap().changed,
        "already paused"
    );

    let queued = as_bot(&fx, "keeper").await;
    let plain = fx.submit("plain").await;
    // The live run is not touched.
    fx.wait_status(&live, RunStatus::Succeeded).await;
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
    assert_eq!(fx.status(&queued).await, RunStatus::Queued);
    assert_eq!(fx.status(&plain).await, RunStatus::Queued);
    assert!(fx.kernel.budget_status().await.unwrap().paused);

    // A paused dispatcher is idle: no claim, so no store write, however long it sits.
    let writes = fx.idle().await;
    step(&fx, 10 * MIN).await;
    assert_eq!(fx.idle().await, writes);

    assert!(fx.kernel.resume(None).await.unwrap().changed);
    fx.wait_status(&queued, RunStatus::Succeeded).await;
    fx.wait_status(&plain, RunStatus::Succeeded).await;
    assert!(!fx.kernel.budget_status().await.unwrap().paused);
}

#[tokio::test(start_paused = true)]
async fn pausing_a_bot_holds_only_that_bot() {
    let fx = fx_with(
        "max_concurrent_runs = 2",
        [MockStep::Sleep(secs(1))],
        config(8),
    )
    .await;
    fx.kernel.pause(Some("keeper"), false).await.unwrap();
    let held = as_bot(&fx, "keeper").await;
    let free = as_bot(&fx, "scribe").await;
    fx.wait_status(&free, RunStatus::Succeeded).await;
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
    assert_eq!(fx.status(&held).await, RunStatus::Queued);
    let status = fx.kernel.budget_status().await.unwrap();
    assert!(!status.paused);
    assert!(
        status
            .bots
            .iter()
            .find(|b| b.bot == "keeper")
            .unwrap()
            .paused
    );
    assert!(
        !status
            .bots
            .iter()
            .find(|b| b.bot == "scribe")
            .unwrap()
            .paused
    );

    fx.kernel.resume(Some("keeper")).await.unwrap();
    fx.wait_status(&held, RunStatus::Succeeded).await;
    let unknown = fx.kernel.pause(Some("nobody"), false).await;
    assert!(
        matches!(unknown, Err(KernelError::DefinitionNotFound { .. })),
        "{unknown:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn stop_live_cancels_live_runs_and_only_those_in_scope() {
    let fx = fx_with(
        "max_concurrent_runs = 2",
        [MockStep::Sleep(secs(3600))],
        config(8),
    )
    .await;
    let keeper = as_bot(&fx, "keeper").await;
    let scribe = as_bot(&fx, "scribe").await;
    fx.wait_status(&keeper, RunStatus::Running).await;
    fx.wait_status(&scribe, RunStatus::Running).await;

    let result = fx.kernel.pause(Some("keeper"), true).await.unwrap();
    assert_eq!(result.stopped, std::slice::from_ref(&keeper));
    fx.wait_status(&keeper, RunStatus::Interrupted).await;
    assert_eq!(fx.status(&scribe).await, RunStatus::Running);

    let result = fx.kernel.pause(None, true).await.unwrap();
    assert_eq!(result.stopped, std::slice::from_ref(&scribe));
    fx.wait_status(&scribe, RunStatus::Interrupted).await;
}

#[tokio::test(start_paused = true)]
async fn a_trigger_is_held_during_a_pause_and_caught_up_once_on_resume() {
    let fx = fx_with(
        "max_concurrent_runs = 2",
        [MockStep::Sleep(secs(1))],
        config(8),
    )
    .await;
    fx.kernel.pause(Some("keeper"), false).await.unwrap();
    // Three occurrences come due while paused, and the pause outlasts them.
    step(&fx, HOUR).await;
    step(&fx, HOUR).await;
    step(&fx, HOUR + 30 * MIN).await;
    // The hold is a store write, and the writer is a real thread: wait for it to land.
    until(async || (!events_of(&fx, "trigger.held").await.is_empty()).then_some(())).await;
    assert!(
        fx.store.list_runs(None).await.unwrap().is_empty(),
        "nothing fires"
    );
    assert_eq!(
        events_of(&fx, "trigger.held").await.len(),
        1,
        "the hold is recorded once"
    );
    let first_due = u64::try_from(START_MS + HOUR).unwrap();
    until(async || (trigger_info(&fx).next_due_at_ms == Some(first_due)).then_some(())).await;

    fx.kernel.resume(Some("keeper")).await.unwrap();
    // The catch-up is a run and its firing, written one after the other: wait for both.
    until(async || {
        let both = !fx.store.list_runs(None).await.unwrap().is_empty()
            && !fx.store.firings(None, 10).await.unwrap().is_empty();
        both.then_some(())
    })
    .await;
    let runs = fx.store.list_runs(None).await.unwrap();
    assert_eq!(runs.len(), 1, "the missed occurrences are one catch-up run");
    assert!(
        runs[0].goal.contains("late"),
        "it says how late it is: {}",
        runs[0].goal
    );
    let firings = fx.store.firings(None, 10).await.unwrap();
    assert_eq!(firings.len(), 1);
    assert!(firings[0].missed >= 3, "{firings:?}");
    // The schedule goes on from there.
    let after = u64::try_from(START_MS + 3 * HOUR).unwrap();
    until(async || {
        trigger_info(&fx)
            .next_due_at_ms
            .filter(|due| *due > after)
            .map(|_| ())
    })
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_pause_survives_a_restart() {
    let fx = fx_with(
        "max_concurrent_runs = 2",
        [MockStep::Sleep(secs(1))],
        config(8),
    )
    .await;
    fx.kernel.pause(None, false).await.unwrap();
    fx.kernel.pause(Some("keeper"), false).await.unwrap();
    fx.kernel.shutdown().await;
    let (dir, store) = fx.into_store();

    let port = FakePort::with(defs("max_concurrent_runs = 2"));
    let fx = Fx::over_definitions(
        dir,
        store,
        config(8),
        registry(&[mock([MockStep::Sleep(secs(1))])]),
        setup(&port),
    )
    .await;
    let status = fx.kernel.budget_status().await.unwrap();
    assert!(status.paused);
    assert!(
        status
            .bots
            .iter()
            .find(|b| b.bot == "keeper")
            .unwrap()
            .paused
    );
    let run = as_bot(&fx, "scribe").await;
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        fx.status(&run).await,
        RunStatus::Queued,
        "still paused after the restart"
    );
    fx.kernel.resume(None).await.unwrap();
    fx.wait_status(&run, RunStatus::Succeeded).await;
}

#[tokio::test(start_paused = true)]
async fn the_global_pause_is_not_a_bot_name() {
    let fx = fx_with(
        "max_concurrent_runs = 2",
        [MockStep::Sleep(secs(1))],
        config(8),
    )
    .await;
    fx.kernel.pause(None, false).await.unwrap();
    let wrong = fx.kernel.resume(Some("*")).await;
    assert!(matches!(wrong, Err(KernelError::Invalid(_))), "{wrong:?}");
    let wrong = fx.kernel.pause(Some("*"), false).await;
    assert!(matches!(wrong, Err(KernelError::Invalid(_))), "{wrong:?}");
    assert!(
        fx.kernel.budget_status().await.unwrap().paused,
        "the pause is still there, in memory and in the store"
    );
    assert!(fx.store.pauses().await.unwrap().contains_key("*"));
}

#[tokio::test(start_paused = true)]
async fn an_expired_lease_the_claim_would_pass_over_does_not_wake_the_dispatcher() {
    let fx = fx_with(
        "max_concurrent_runs = 2",
        [MockStep::Sleep(secs(1))],
        config(8),
    )
    .await;
    fx.kernel.pause(Some("keeper"), false).await.unwrap();
    let run = as_bot(&fx, "keeper").await;
    // Someone else leased it and died: the lease runs out, but the bot is paused.
    let claimed = fx
        .store
        .claim_next("someone-else", fx.clock.now_ms(), 60_000)
        .await
        .unwrap();
    assert_eq!(claimed.unwrap().run_id, run);
    step(&fx, 5 * MIN).await;
    // A turn of the dispatcher with the lease expired and the bot still paused.
    fx.kernel.resume(Some("scribe")).await.unwrap();
    // It must settle: no one-millisecond wake-ups claiming nothing, however long it sits.
    fx.idle().await;
    // (The hour trigger of the paused bot is held once; that is its own, single write.)
    step(&fx, HOUR).await;
    let writes = fx.idle().await;
    step(&fx, 2 * HOUR).await;
    assert_eq!(fx.idle().await, writes);
    assert_eq!(fx.status(&run).await, RunStatus::Leased);

    // Resuming the bot is what lets the claim take the run over.
    fx.kernel.resume(Some("keeper")).await.unwrap();
    fx.wait_status(&run, RunStatus::Succeeded).await;
}
