//! Triggers and the scheduler: due times, the firing rules of ARCHITECTURE 7, re-planning and the
//! `Watch` port. Time is paused; the wall clock is a `ManualClock`, moved with the timers by
//! `step`. Nothing sleeps for real.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use reno_harness::exec::{ExecControls, ExecError, Executor, MockExecutor, MockStep};
use reno_proto::defs::{DefFile, DefKind};
use reno_proto::{ExecSpec, FiringOutcome, HealthProbe, Seq, TriggerInfo, TriggerStatus};
use tokio::sync::mpsc;

use super::defs::{FakePort, file, setup};
use super::{Fx, START_MS, id, mock, registry, until};
use crate::Clock as _;
use crate::Secrets as _;
use crate::{
    ExecutorRegistry, KernelConfig, KernelError, WakeSignal, Watch, WatchSpec, WatchStream,
};

const MIN: i64 = 60_000;
const HOUR: i64 = 3_600_000;

fn bot_file() -> DefFile {
    file(
        DefKind::Bot,
        "keeper",
        "name = \"keeper\"\ninstructions = \"x\"\nexecutors = [\"mock\"]\n",
    )
}

fn trigger(name: &str, when: &str, prompt: &str) -> DefFile {
    file(
        DefKind::Trigger,
        name,
        &format!(
            "name = \"{name}\"\n[when]\n{when}\n[target]\nbot = \"keeper\"\nprompt = \"{prompt}\"\n"
        ),
    )
}

fn files(triggers: Vec<DefFile>) -> Vec<DefFile> {
    let mut all = vec![bot_file()];
    all.extend(triggers);
    all
}

async fn start(files: Vec<DefFile>, mocks: &[MockExecutor]) -> (Fx, Arc<FakePort>) {
    start_at(START_MS, files, mocks).await
}

async fn start_at(now: i64, files: Vec<DefFile>, mocks: &[MockExecutor]) -> (Fx, Arc<FakePort>) {
    let port = FakePort::with(files);
    let fx =
        Fx::with_definitions_at(now, KernelConfig::default(), registry(mocks), setup(&port)).await;
    (fx, port)
}

fn info(fx: &Fx, name: &str) -> TriggerInfo {
    fx.kernel
        .list_triggers()
        .into_iter()
        .find(|t| t.name == name)
        .unwrap_or_else(|| panic!("no trigger {name}"))
}

/// Whether the scheduler has dealt with everything due at the clock's time.
fn caught_up(fx: &Fx) -> bool {
    let now = u64::try_from(fx.clock.now_ms()).unwrap();
    fx.kernel.list_triggers().iter().all(|t| {
        t.next_due_at_ms.is_none_or(|due| due > now)
            || t.backoff_until_ms.is_some_and(|until| until > now)
    })
}

/// Moves the wall clock and the timers by `ms` together, and waits for the scheduler to catch up.
async fn step(fx: &Fx, ms: i64) {
    fx.clock.advance(ms);
    tokio::time::advance(Duration::from_millis(u64::try_from(ms).unwrap())).await;
    until(async || caught_up(fx).then_some(())).await;
}

async fn runs(fx: &Fx) -> Vec<reno_store::RunRecord> {
    let mut all = fx.store.list_runs(None).await.unwrap();
    all.sort_by_key(|r| r.created_at);
    all
}

/// Waits until every run has ended, so the next occurrence finds its trigger free.
async fn all_ended(fx: &Fx) {
    until(async || {
        runs(fx)
            .await
            .iter()
            .all(|r| r.status.is_terminal())
            .then_some(())
    })
    .await;
}

async fn firings(fx: &Fx) -> Vec<reno_store::FiringRecord> {
    let mut all = fx.store.firings(None, 500).await.unwrap();
    all.reverse();
    all
}

async fn event_kinds(fx: &Fx) -> Vec<String> {
    fx.store
        .events_since(Seq(0), 100_000)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .filter(|k| k.starts_with("trigger."))
        .collect()
}

// ---- due times --------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn every_fires_each_interval_from_when_it_was_stored() {
    let (fx, _port) = start(
        files(vec![trigger(
            "tick",
            "kind = \"every\"\nevery = \"1h\"",
            "go {{trigger}}",
        )]),
        &[mock([])],
    )
    .await;
    let t = info(&fx, "tick");
    assert_eq!(t.status, TriggerStatus::Scheduled);
    assert_eq!(
        t.next_due_at_ms,
        Some(u64::try_from(START_MS + HOUR).unwrap())
    );

    step(&fx, 59 * MIN).await;
    assert!(runs(&fx).await.is_empty(), "nothing before it is due");
    step(&fx, MIN).await;
    let made = runs(&fx).await;
    assert_eq!(made.len(), 1);
    assert_eq!(
        made[0].goal, "go tick",
        "the template is filled, and on time adds no note"
    );
    assert_eq!(made[0].bot.as_ref().unwrap().name, "keeper");
    all_ended(&fx).await;
    step(&fx, HOUR).await;
    assert_eq!(runs(&fx).await.len(), 2);
    assert_eq!(
        info(&fx, "tick").next_due_at_ms,
        Some(u64::try_from(START_MS + 3 * HOUR).unwrap())
    );
}

#[tokio::test(start_paused = true)]
async fn at_and_after_fire_once() {
    let at = "kind = \"at\"\nat = \"2023-11-15T00:00:00Z\"";
    let (fx, _port) = start(
        files(vec![
            trigger("once", at, "at"),
            trigger("later", "kind = \"after\"\nafter = \"30m\"", "after"),
        ]),
        &[mock([])],
    )
    .await;
    // The clock starts at 22:13:20Z, so `at` is 1h46m40s away and `after` 30m.
    step(&fx, 30 * MIN).await;
    assert_eq!(runs(&fx).await.len(), 1, "after fired");
    assert_eq!(info(&fx, "later").status, TriggerStatus::Done);
    step(&fx, 2 * HOUR).await;
    let goals: Vec<_> = runs(&fx).await.into_iter().map(|r| r.goal).collect();
    assert_eq!(goals.len(), 2, "{goals:?}");
    assert!(goals.iter().any(|g| g.starts_with("at")));
    assert_eq!(info(&fx, "once").status, TriggerStatus::Done);
    step(&fx, 48 * HOUR).await;
    assert_eq!(runs(&fx).await.len(), 2, "one-shots never fire again");
}

#[tokio::test(start_paused = true)]
async fn cron_fires_in_its_zone_and_fills_the_placeholders() {
    // 22:13Z is 05:13 in Ho Chi Minh City; the next 02:00 there is 19:00Z the next day.
    let when = "kind = \"cron\"\ncron = \"0 2 * * *\"\ntimezone = \"Asia/Ho_Chi_Minh\"";
    let (fx, _port) = start(
        files(vec![trigger(
            "nightly",
            when,
            "{{trigger}} for {{scheduled_at}} late {{lateness}}",
        )]),
        &[mock([])],
    )
    .await;
    let due = 1_700_000_000_000 + 20 * HOUR + 46 * MIN + 40_000;
    assert_eq!(
        info(&fx, "nightly").next_due_at_ms,
        Some(u64::try_from(due).unwrap())
    );
    step(&fx, 20 * HOUR + 46 * MIN + 40_000).await;
    let made = runs(&fx).await;
    assert_eq!(made.len(), 1);
    assert_eq!(
        made[0].goal,
        "nightly for 2023-11-16T02:00:00+07:00 late 0s"
    );
}

// ---- daylight saving ---------------------------------------------------------------------

/// Drives a daily cron through a DST change one hour at a time and returns what it fired for,
/// as UTC instants in minutes since the start.
async fn daily_across(start_ms: i64, zone: &str, expr: &str, hours: i64) -> Vec<String> {
    let when = format!("kind = \"cron\"\ncron = \"{expr}\"\ntimezone = \"{zone}\"");
    let (fx, _port) = start_at(
        start_ms,
        files(vec![trigger("daily", &when, "go")]),
        &[mock([])],
    )
    .await;
    for _ in 0..hours {
        step(&fx, HOUR).await;
    }
    firings(&fx)
        .await
        .into_iter()
        .map(|f| reno_proto::defs::format_instant(f.scheduled_at, Some(zone)))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn spring_forward_a_daily_cron_in_the_gap_fires_once_after_it() {
    // America/New_York, 2026-03-07T12:00Z, three days. 02:30 on March 8 does not exist.
    let fired = daily_across(1_772_884_800_000, "America/New_York", "30 2 * * *", 72).await;
    assert_eq!(
        fired,
        [
            "2026-03-08T03:00:00-04:00",
            "2026-03-09T02:30:00-04:00",
            "2026-03-10T02:30:00-04:00"
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn fall_back_a_daily_cron_in_the_repeated_hour_fires_once() {
    // 2026-10-31T12:00Z, three days. 01:30 on November 1 happens twice.
    let fired = daily_across(1_793_448_000_000, "America/New_York", "30 1 * * *", 72).await;
    assert_eq!(
        fired,
        [
            "2026-11-01T01:30:00-04:00",
            "2026-11-02T01:30:00-05:00",
            "2026-11-03T01:30:00-05:00"
        ]
    );
}

// ---- the nonce ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn two_schedulers_on_one_store_fire_an_occurrence_once() {
    let defs = files(vec![trigger(
        "tick",
        "kind = \"every\"\nevery = \"1h\"",
        "go",
    )]);
    let (a, _pa) = start(defs.clone(), &[mock([])]).await;
    let port_b = FakePort::with(defs);
    // B's clock is ahead by half a second, and it was started a moment later.
    let b = Fx::sharing(
        &a,
        START_MS + 500,
        KernelConfig::default(),
        registry(&[mock([])]),
        setup(&port_b),
    )
    .await;
    assert_eq!(
        info(&b, "tick").next_due_at_ms,
        info(&a, "tick").next_due_at_ms
    );

    a.clock.advance(HOUR);
    b.clock.advance(HOUR);
    tokio::time::advance(Duration::from_millis(u64::try_from(HOUR).unwrap())).await;
    until(async || (caught_up(&a) && caught_up(&b)).then_some(())).await;

    assert_eq!(runs(&a).await.len(), 1, "one run for one occurrence");
    let all = firings(&a).await;
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].nonce, format!("every:tick:{}", START_MS + HOUR));
    // The loser took the winner's state as its own.
    assert_eq!(
        info(&a, "tick").next_due_at_ms,
        info(&b, "tick").next_due_at_ms
    );
    assert_eq!(info(&a, "tick").last_run_id, info(&b, "tick").last_run_id);
}

#[tokio::test(start_paused = true)]
async fn the_same_manual_occurrence_twice_is_one_run() {
    let (fx, _port) = start(
        files(vec![trigger("poke", "kind = \"manual\"", "poked")]),
        &[mock([MockStep::Sleep(Duration::from_secs(1_000_000))])],
    )
    .await;
    assert_eq!(info(&fx, "poke").status, TriggerStatus::Manual);
    let first = fx.kernel.fire_trigger("poke").await.unwrap();
    assert_eq!(first.outcome, FiringOutcome::Fired);
    // Same clock reading, so the same nonce.
    let again = fx.kernel.fire_trigger("poke").await.unwrap();
    assert_eq!(again.outcome, FiringOutcome::Duplicate);
    assert_eq!(again.run_id, None);
    assert_eq!(runs(&fx).await.len(), 1);

    // A moment later it is a new occurrence, but the run is still going: merged into it.
    fx.clock.advance(1);
    let merged = fx.kernel.fire_trigger("poke").await.unwrap();
    assert_eq!(merged.outcome, FiringOutcome::Coalesced);
    assert_eq!(merged.run_id, first.run_id);
    assert_eq!(runs(&fx).await.len(), 1);

    let error = fx.kernel.fire_trigger("ghost").await.unwrap_err();
    assert!(
        matches!(error, KernelError::TriggerNotFound(_)),
        "{error:?}"
    );
}

// ---- missed occurrences ------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn downtime_across_three_occurrences_is_one_catch_up_run() {
    let defs = files(vec![trigger(
        "tick",
        "kind = \"every\"\nevery = \"1h\"",
        "work",
    )]);
    let (fx, _port) = start(defs.clone(), &[mock([])]).await;
    fx.kernel.shutdown().await;
    let (dir, store) = fx.into_store();

    // Down for 3h30m: occurrences at +1h, +2h and +3h were missed.
    let port = FakePort::with(defs);
    let fx = Fx::restarted_at(
        START_MS + 3 * HOUR + 30 * MIN,
        dir,
        store,
        KernelConfig::default(),
        registry(&[mock([])]),
        setup(&port),
    )
    .await;
    until(async || (!runs(&fx).await.is_empty()).then_some(())).await;
    let made = runs(&fx).await;
    assert_eq!(made.len(), 1, "exactly one catch-up run");
    assert_eq!(
        made[0].goal,
        "work\n\nThis run (trigger tick) was scheduled for 2023-11-14T23:13:20+00:00 and started \
         2h30m late; 3 occurrences were missed and merged into this one."
    );
    let fired = firings(&fx).await;
    assert_eq!((fired.len(), fired[0].missed), (1, 3));
    assert_eq!(
        fired[0].scheduled_at,
        START_MS + HOUR,
        "the earliest of them"
    );
    // History is not recomputed: the next one is the grid point after now.
    until(async || caught_up(&fx).then_some(())).await;
    assert_eq!(
        info(&fx, "tick").next_due_at_ms,
        Some(u64::try_from(START_MS + 4 * HOUR).unwrap())
    );
}

/// The scheduler of `files` with a wake signal wired in, the clock at `START_MS`.
async fn start_with_wake(files: Vec<DefFile>) -> (Fx, WakeSignal) {
    let port = FakePort::with(files);
    let wake = WakeSignal::new();
    let fx = Fx::with_definitions_at(
        START_MS,
        KernelConfig::default(),
        registry(&[mock([])]),
        setup(&port).with_wake(wake.clone()),
    )
    .await;
    (fx, wake)
}

#[tokio::test(start_paused = true)]
async fn a_cron_due_while_the_machine_slept_fires_on_the_wake_signal() {
    // Due at 03:00 UTC; it is 22:13 UTC, 4 h 47 min away.
    let when = "kind = \"cron\"\ncron = \"0 3 * * *\"\ntimezone = \"UTC\"";
    let (fx, wake) = start_with_wake(files(vec![trigger("nightly", when, "go")])).await;
    until(async || caught_up(&fx).then_some(())).await;

    // The machine sleeps through the due time: the wall clock moves 6 h, the timer only 2 h. The
    // old one-hour guard would have looked at the clock by now; nothing else does.
    fx.clock.advance(6 * HOUR);
    for _ in 0..4 {
        tokio::time::advance(Duration::from_secs(30 * 60)).await;
        tokio::task::yield_now().await;
    }
    assert!(
        runs(&fx).await.is_empty(),
        "no periodic wake reads the clock"
    );

    // It wakes up, and the system says so.
    wake.notify();
    until(async || (!runs(&fx).await.is_empty()).then_some(())).await;
    until(async || caught_up(&fx).then_some(())).await;
    assert_eq!(runs(&fx).await.len(), 1, "one occurrence, one run");
}

#[tokio::test(start_paused = true)]
async fn a_wake_with_nothing_due_changes_nothing() {
    let when = "kind = \"cron\"\ncron = \"0 3 * * *\"\ntimezone = \"UTC\"";
    let (fx, wake) = start_with_wake(files(vec![trigger("nightly", when, "go")])).await;
    let before = fx.idle().await;
    fx.clock.advance(MIN);
    wake.notify();
    wake.notify();
    tokio::task::yield_now().await;
    assert_eq!(
        fx.idle().await,
        before,
        "a wake reads the clock and writes nothing"
    );
    assert!(runs(&fx).await.is_empty());
}

#[test]
fn the_scheduler_arms_one_timer_for_the_earliest_deadline_and_none_when_idle() {
    use crate::scheduler::until_deadline;
    assert_eq!(until_deadline(None, 5), None, "no deadline, no timer");
    assert_eq!(
        until_deadline(Some(5 + 30 * HOUR), 5),
        Some(Duration::from_millis(u64::try_from(30 * HOUR).unwrap())),
        "not capped to an hour"
    );
    assert_eq!(until_deadline(Some(5), 9), Some(Duration::from_millis(1)));
}

// ---- reused target -----------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn an_occurrence_while_the_last_run_is_going_is_coalesced() {
    let (fx, _port) = start(
        files(vec![trigger(
            "tick",
            "kind = \"every\"\nevery = \"1h\"",
            "go",
        )]),
        &[mock([MockStep::Sleep(Duration::from_secs(1_000_000))])],
    )
    .await;
    step(&fx, HOUR).await;
    let first = runs(&fx).await;
    assert_eq!(first.len(), 1);
    step(&fx, HOUR).await;
    step(&fx, HOUR).await;

    assert_eq!(
        runs(&fx).await.len(),
        1,
        "no new run while the first is going"
    );
    let outcomes: Vec<_> = firings(&fx)
        .await
        .iter()
        .map(|f| (f.outcome, f.run.clone()))
        .collect();
    assert_eq!(
        outcomes,
        [
            (FiringOutcome::Fired, first[0].id.clone()),
            (FiringOutcome::Coalesced, first[0].id.clone()),
            (FiringOutcome::Coalesced, first[0].id.clone()),
        ]
    );
    assert_eq!(info(&fx, "tick").last_run_id.as_ref(), Some(&first[0].id));
}

// ---- backoff -----------------------------------------------------------------------------

/// An executor that refuses to start until it is "signed in".
#[derive(Debug)]
struct Flaky {
    signed_in: Arc<AtomicBool>,
    inner: MockExecutor,
}

#[async_trait]
impl Executor for Flaky {
    fn descriptor(&self) -> reno_proto::ExecutorDescriptor {
        self.inner.descriptor()
    }
    async fn probe(&self) -> HealthProbe {
        self.inner.probe().await
    }
    async fn start(
        &self,
        spec: ExecSpec,
        controls: ExecControls,
    ) -> Result<BoxStream<'static, reno_proto::ExecEvent>, ExecError> {
        if self.signed_in.load(Ordering::SeqCst) {
            self.inner.start(spec, controls).await
        } else {
            Err(ExecError::NotSignedIn("run `claude login`".into()))
        }
    }
}

fn flaky() -> (ExecutorRegistry, Arc<AtomicBool>) {
    let signed_in = Arc::new(AtomicBool::new(false));
    let executor = Flaky {
        signed_in: signed_in.clone(),
        inner: MockExecutor::new(id("mock")).with_steps([]),
    };
    (ExecutorRegistry::new().with(Arc::new(executor)), signed_in)
}

async fn start_flaky(files: Vec<DefFile>, registry: ExecutorRegistry) -> Fx {
    let port = FakePort::with(files);
    Fx::with_definitions(KernelConfig::default(), registry, setup(&port)).await
}

#[tokio::test(start_paused = true)]
async fn failing_to_start_backs_off_5_10_20_minutes_with_one_notification_then_resets() {
    let (registry, signed_in) = flaky();
    let fx = start_flaky(
        files(vec![trigger(
            "tick",
            "kind = \"every\"\nevery = \"1m\"",
            "go",
        )]),
        registry,
    )
    .await;

    // The first occurrence fails to start (not signed in): backed off for 5 minutes.
    step(&fx, MIN).await;
    let backoff_until = |fx: &Fx| {
        info(fx, "tick")
            .backoff_until_ms
            .map(|ms| i64::try_from(ms).unwrap())
    };
    until(async || info(&fx, "tick").failures.eq(&1).then_some(())).await;
    assert_eq!(info(&fx, "tick").status, TriggerStatus::BackedOff);
    assert_eq!(backoff_until(&fx), Some(START_MS + MIN + 5 * MIN));
    assert_eq!(firings(&fx).await[0].outcome, FiringOutcome::BackedOff);

    // Occurrences that come due meanwhile start nothing.
    for _ in 0..4 {
        step(&fx, MIN).await;
    }
    assert_eq!(runs(&fx).await.len(), 1);

    // When the backoff ends one catch-up fires, merging what was missed; it fails again: 10 min.
    step(&fx, MIN).await;
    until(async || info(&fx, "tick").failures.eq(&2).then_some(())).await;
    let second = backoff_until(&fx).unwrap();
    assert_eq!(second, START_MS + 6 * MIN + 10 * MIN);
    let catch_up = runs(&fx).await;
    assert_eq!(catch_up.len(), 2, "one run for the whole backoff window");
    assert!(
        catch_up[1]
            .goal
            .contains("occurrences were missed and merged into this one"),
        "{}",
        catch_up[1].goal
    );

    // Third failure: 20 minutes.
    fx.clock.set(second);
    tokio::time::advance(Duration::from_millis(u64::try_from(10 * MIN).unwrap())).await;
    until(async || info(&fx, "tick").failures.eq(&3).then_some(())).await;
    assert_eq!(backoff_until(&fx), Some(second + 20 * MIN));

    // One notification for the whole episode, however many ticks failed.
    let kinds = event_kinds(&fx).await;
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "trigger.backoff_started")
            .count(),
        1
    );
    assert_eq!(
        kinds
            .iter()
            .filter(|k| *k == "trigger.backoff_extended")
            .count(),
        2
    );

    // The account is fixed: the next attempt starts, and the backoff is over.
    signed_in.store(true, Ordering::SeqCst);
    let until_ms = backoff_until(&fx).unwrap();
    fx.clock.set(until_ms);
    tokio::time::advance(Duration::from_millis(u64::try_from(20 * MIN).unwrap())).await;
    until(async || info(&fx, "tick").failures.eq(&0).then_some(())).await;
    let healthy = info(&fx, "tick");
    assert_eq!(
        (healthy.status, healthy.backoff_until_ms),
        (TriggerStatus::Scheduled, None)
    );
    assert_eq!(
        event_kinds(&fx)
            .await
            .iter()
            .filter(|k| *k == "trigger.recovered")
            .count(),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn a_missing_secret_backs_a_trigger_off_like_a_signed_out_account_until_it_is_set() {
    let secrets = Arc::new(crate::FakeSecrets::new());
    let port = FakePort::with(vec![
        file(
            DefKind::Bot,
            "keeper",
            "name = \"keeper\"\ninstructions = \"x\"\nexecutors = [\"mock\"]\nsecrets = [\"GITHUB_TOKEN\"]\n",
        ),
        trigger("tick", "kind = \"every\"\nevery = \"1m\"", "go"),
    ]);
    let m = mock([]);
    let fx = Fx::with_definitions_at(
        START_MS,
        KernelConfig::default(),
        registry(std::slice::from_ref(&m)),
        setup(&port).with_secrets(secrets.clone()),
    )
    .await;

    step(&fx, MIN).await;
    until(async || info(&fx, "tick").failures.eq(&1).then_some(())).await;
    assert_eq!(info(&fx, "tick").status, TriggerStatus::BackedOff);
    assert_eq!(
        info(&fx, "tick")
            .backoff_until_ms
            .map(|ms| i64::try_from(ms).unwrap()),
        Some(START_MS + MIN + 5 * MIN),
        "5 minutes, the first step of the same ladder"
    );
    assert_eq!(firings(&fx).await[0].outcome, FiringOutcome::BackedOff);
    assert!(m.log().specs.is_empty(), "nothing was started");

    // A person sets the secret; when the backoff ends the next run starts and the episode ends.
    secrets
        .set(
            "GITHUB_TOKEN",
            crate::SecretValue::new("ghp_exampleExample1234"),
        )
        .await
        .unwrap();
    let until_ms = info(&fx, "tick")
        .backoff_until_ms
        .map(|ms| i64::try_from(ms).unwrap())
        .unwrap();
    fx.clock.set(until_ms);
    tokio::time::advance(Duration::from_millis(u64::try_from(5 * MIN).unwrap())).await;
    until(async || info(&fx, "tick").failures.eq(&0).then_some(())).await;
    assert_eq!(info(&fx, "tick").status, TriggerStatus::Scheduled);
    assert_eq!(m.log().specs.len(), 1);
}

/// An executor whose probe says it is signed out, and whose `start` must never be reached.
struct SignedOut(MockExecutor);

#[async_trait]
impl Executor for SignedOut {
    fn descriptor(&self) -> reno_proto::ExecutorDescriptor {
        self.0.descriptor()
    }
    async fn probe(&self) -> HealthProbe {
        HealthProbe {
            signed_in: reno_proto::SignedIn::No {
                hint: "run `mock login`".into(),
            },
            ..self.0.probe().await
        }
    }
    async fn start(
        &self,
        _: ExecSpec,
        _: ExecControls,
    ) -> Result<BoxStream<'static, reno_proto::ExecEvent>, ExecError> {
        unreachable!("an executor known to be signed out is never started")
    }
}

#[tokio::test(start_paused = true)]
async fn an_executor_known_to_be_signed_out_backs_the_trigger_off_like_a_refused_start() {
    let registry = ExecutorRegistry::new().with(Arc::new(SignedOut(MockExecutor::new(id("mock")))));
    let fx = start_flaky(
        files(vec![trigger(
            "tick",
            "kind = \"every\"\nevery = \"1m\"",
            "go",
        )]),
        registry,
    )
    .await;
    fx.kernel.refresh_executors(None).await.unwrap();

    step(&fx, MIN).await;
    until(async || info(&fx, "tick").failures.eq(&1).then_some(())).await;
    let tick = info(&fx, "tick");
    assert_eq!(tick.status, TriggerStatus::BackedOff);
    assert_eq!(
        tick.backoff_until_ms.map(|ms| i64::try_from(ms).unwrap()),
        Some(START_MS + MIN + 5 * MIN)
    );
    assert_eq!(firings(&fx).await[0].outcome, FiringOutcome::BackedOff);
}

#[tokio::test(start_paused = true)]
async fn a_bot_with_no_executor_here_backs_the_trigger_off_without_a_run() {
    // The bot lists only `codex`, which this kernel does not have.
    let bot = file(
        DefKind::Bot,
        "keeper",
        "name = \"keeper\"\ninstructions = \"x\"\nexecutors = [\"codex\"]\n",
    );
    let port = FakePort::with(vec![
        bot,
        trigger("tick", "kind = \"every\"\nevery = \"1h\"", "go"),
    ]);
    let fx =
        Fx::with_definitions(KernelConfig::default(), registry(&[mock([])]), setup(&port)).await;
    fx.clock.advance(HOUR);
    tokio::time::advance(Duration::from_millis(u64::try_from(HOUR).unwrap())).await;
    until(async || info(&fx, "tick").failures.eq(&1).then_some(())).await;
    assert!(runs(&fx).await.is_empty());
    assert_eq!(info(&fx, "tick").status, TriggerStatus::BackedOff);
    assert_eq!(
        event_kinds(&fx)
            .await
            .iter()
            .filter(|k| *k == "trigger.backoff_started")
            .count(),
        1
    );
}

// ---- definitions -------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_definition_change_replans_without_a_restart_and_disabling_stops_it() {
    let hourly = trigger("tick", "kind = \"every\"\nevery = \"1h\"", "go");
    let (fx, port) = start(files(vec![hourly.clone()]), &[mock([])]).await;
    step(&fx, 30 * MIN).await;

    // Edited to every 2 hours: a new plan, counted from now.
    port.set(files(vec![trigger(
        "tick",
        "kind = \"every\"\nevery = \"2h\"",
        "go",
    )]));
    port.touch_and_wait().await;
    until(async || (info(&fx, "tick").version == 2).then_some(())).await;
    assert_eq!(
        info(&fx, "tick").next_due_at_ms,
        Some(u64::try_from(START_MS + 30 * MIN + 2 * HOUR).unwrap())
    );
    step(&fx, 90 * MIN).await;
    assert!(runs(&fx).await.is_empty(), "the old hour is gone");

    // A new trigger starts.
    port.set(files(vec![
        trigger("tick", "kind = \"every\"\nevery = \"2h\"", "go"),
        trigger("extra", "kind = \"every\"\nevery = \"1h\"", "extra"),
    ]));
    port.touch_and_wait().await;
    until(async || fx.kernel.list_triggers().len().eq(&2).then_some(())).await;

    // Disabled: stops, and shows it.
    port.set(files(vec![
        file(
            DefKind::Trigger,
            "tick",
            "name = \"tick\"\nenabled = false\n[when]\nkind = \"every\"\nevery = \"2h\"\n[target]\nbot = \"keeper\"\nprompt = \"go\"\n",
        ),
        trigger("extra", "kind = \"every\"\nevery = \"1h\"", "extra"),
    ]));
    port.touch_and_wait().await;
    until(async || (info(&fx, "tick").status == TriggerStatus::Disabled).then_some(())).await;
    assert_eq!(info(&fx, "tick").next_due_at_ms, None);
    let before = runs(&fx).await.len();
    for _ in 0..6 {
        step(&fx, HOUR).await;
    }
    let goals: Vec<_> = runs(&fx)
        .await
        .into_iter()
        .skip(before)
        .map(|r| r.goal)
        .collect();
    assert!(
        !goals.is_empty() && goals.iter().all(|g| g == "extra"),
        "{goals:?}"
    );
    let error = fx.kernel.fire_trigger("tick").await.unwrap_err();
    assert!(
        matches!(error, KernelError::TriggerDisabled(_)),
        "{error:?}"
    );

    // Removed: gone from the list and from the table.
    port.set(files(vec![]));
    port.touch_and_wait().await;
    until(async || fx.kernel.list_triggers().is_empty().then_some(())).await;
    assert!(fx.store.list_triggers().await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn editing_the_prompt_keeps_the_plan_and_does_not_refire_a_finished_one_shot() {
    let at = |prompt: &str| trigger("once", "kind = \"after\"\nafter = \"10m\"", prompt);
    let (fx, port) = start(files(vec![at("one")]), &[mock([])]).await;
    step(&fx, 10 * MIN).await;
    assert_eq!(runs(&fx).await.len(), 1);

    port.set(files(vec![at("two")]));
    port.touch_and_wait().await;
    until(async || (info(&fx, "once").version == 2).then_some(())).await;
    step(&fx, 5 * HOUR).await;
    assert_eq!(runs(&fx).await.len(), 1, "the delay was not started again");
    assert_eq!(info(&fx, "once").status, TriggerStatus::Done);
}

// ---- file watches ------------------------------------------------------------------------

#[derive(Debug, Default)]
struct FakeWatch {
    open: Mutex<Vec<(WatchSpec, mpsc::Sender<()>)>>,
}

impl Watch for FakeWatch {
    fn watch(&self, spec: &WatchSpec) -> crate::Result<WatchStream> {
        let (tx, rx) = mpsc::channel(1);
        self.open.lock().unwrap().push((spec.clone(), tx));
        Ok(WatchStream::new(rx, ()))
    }
}

#[tokio::test(start_paused = true)]
async fn an_fs_trigger_fires_when_the_port_reports_a_change() {
    let watch = Arc::new(FakeWatch::default());
    let port = FakePort::with(files(vec![trigger(
        "inbox",
        "kind = \"fs\"\npaths = [\"/data/inbox/**\"]\ndebounce = \"2s\"",
        "new files for {{trigger}}",
    )]));
    let fx = Fx::with_definitions(
        KernelConfig::default(),
        registry(&[mock([])]),
        setup(&port).with_watch(watch.clone()),
    )
    .await;
    assert_eq!(info(&fx, "inbox").status, TriggerStatus::Watching);
    let (spec, tx) = watch.open.lock().unwrap()[0].clone();
    assert_eq!(spec.paths, ["/data/inbox/**"]);
    assert_eq!(spec.debounce, Duration::from_secs(2));

    tx.send(()).await.unwrap();
    until(async || (!runs(&fx).await.is_empty()).then_some(())).await;
    assert_eq!(runs(&fx).await[0].goal, "new files for inbox");
    assert!(firings(&fx).await[0].nonce.starts_with("fs:inbox:"));

    // Disabling the trigger drops its watch.
    port.set(files(vec![file(
        DefKind::Trigger,
        "inbox",
        "name = \"inbox\"\nenabled = false\n[when]\nkind = \"fs\"\npaths = [\"/data/inbox/**\"]\n[target]\nbot = \"keeper\"\nprompt = \"x\"\n",
    )]));
    port.touch_and_wait().await;
    until(async || tx.is_closed().then_some(())).await;
}

#[tokio::test(start_paused = true)]
async fn an_fs_trigger_without_a_watcher_is_shown_as_failed() {
    let (fx, _port) = start(
        files(vec![trigger(
            "inbox",
            "kind = \"fs\"\npaths = [\"/data\"]",
            "x",
        )]),
        &[mock([])],
    )
    .await;
    let t = info(&fx, "inbox");
    assert_eq!(t.status, TriggerStatus::Failed);
    assert!(t.error.unwrap().contains("no file watcher"));
}

/// Refuses the first `failures` starts, as a platform out of watch streams does, then works.
#[derive(Debug)]
struct FlakyWatch {
    failures: Mutex<u32>,
    starts: Mutex<Vec<mpsc::Sender<()>>>,
    attempts: Mutex<u32>,
}

impl Watch for FlakyWatch {
    fn watch(&self, _spec: &WatchSpec) -> crate::Result<WatchStream> {
        *self.attempts.lock().unwrap() += 1;
        {
            let mut left = self.failures.lock().unwrap();
            if *left > 0 {
                *left -= 1;
                return Err(KernelError::Definitions(
                    "unable to start FSEvent stream".into(),
                ));
            }
        }
        let (tx, rx) = mpsc::channel(1);
        self.starts.lock().unwrap().push(tx);
        Ok(WatchStream::new(rx, ()))
    }
}

#[tokio::test(start_paused = true)]
async fn an_fs_trigger_whose_watch_cannot_start_shows_failed_and_retries_with_backoff() {
    let watch = Arc::new(FlakyWatch {
        failures: Mutex::new(3),
        starts: Mutex::default(),
        attempts: Mutex::new(0),
    });
    let port = FakePort::with(files(vec![trigger(
        "inbox",
        "kind = \"fs\"\npaths = [\"/data/inbox/**\"]\ndebounce = \"2s\"",
        "new files",
    )]));
    let config = KernelConfig {
        watch_retry: crate::WatchBackoff {
            initial: Duration::from_secs(1),
            max: Duration::from_secs(60),
        },
        ..KernelConfig::default()
    };
    let fx = Fx::with_definitions(
        config,
        registry(&[mock([])]),
        setup(&port).with_watch(watch.clone()),
    )
    .await;
    let t = info(&fx, "inbox");
    assert_eq!(t.status, TriggerStatus::Failed);
    let error = t.error.unwrap();
    assert!(error.contains("unable to start FSEvent stream"), "{error}");
    assert!(error.contains("retrying"), "{error}");
    assert_eq!(*watch.attempts.lock().unwrap(), 1);

    // The delays double: 1 s, 2 s, 4 s. Nothing is tried before its time.
    for (wait_ms, attempts) in [(900, 1), (200, 2), (1_900, 2), (200, 3), (3_900, 3)] {
        fx.clock.advance(wait_ms);
        tokio::time::advance(Duration::from_millis(u64::try_from(wait_ms).unwrap())).await;
        tokio::task::yield_now().await;
        assert_eq!(
            *watch.attempts.lock().unwrap(),
            attempts,
            "after {wait_ms} ms"
        );
    }
    fx.clock.advance(200);
    tokio::time::advance(Duration::from_millis(200)).await;
    until(async || (info(&fx, "inbox").status == TriggerStatus::Watching).then_some(())).await;
    assert_eq!(*watch.attempts.lock().unwrap(), 4);
    assert!(info(&fx, "inbox").error.is_none());

    // The recovered watch fires the trigger.
    let tx = watch.starts.lock().unwrap()[0].clone();
    tx.send(()).await.unwrap();
    until(async || (!runs(&fx).await.is_empty()).then_some(())).await;
}

// ---- idleness ----------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn an_idle_scheduler_writes_nothing_for_an_hour() {
    let when = "kind = \"cron\"\ncron = \"0 3 * * *\"\ntimezone = \"UTC\"";
    let (fx, _port) = start(files(vec![trigger("nightly", when, "go")]), &[mock([])]).await;
    let before = fx.idle().await;
    for _ in 0..4 {
        fx.clock.advance(15 * MIN);
        tokio::time::advance(Duration::from_secs(15 * 60)).await;
        tokio::task::yield_now().await;
    }
    assert_eq!(
        fx.idle().await,
        before,
        "an hour with nothing due makes no store write"
    );
    assert!(runs(&fx).await.is_empty());
}

// ---- a trigger's working directory --------------------------------------------------------

fn trigger_in(name: &str, when: &str, cwd: &str) -> DefFile {
    file(
        DefKind::Trigger,
        name,
        &format!(
            "name = \"{name}\"\n[when]\n{when}\n[target]\nbot = \"keeper\"\nprompt = \"go\"\ncwd = \"{cwd}\"\n"
        ),
    )
}

const NIGHTLY: &str = "kind = \"cron\"\ncron = \"0 3 * * *\"\ntimezone = \"UTC\"";

async fn start_in_workspace(
    files: Vec<DefFile>,
    config: KernelConfig,
    ws: &Arc<crate::FakeWorkspace>,
    m: &MockExecutor,
) -> Fx {
    let port = FakePort::with(files);
    Fx::with_workspace(
        config,
        registry(std::slice::from_ref(m)),
        Some(setup(&port)),
        ws.clone(),
    )
    .await
}

#[tokio::test(start_paused = true)]
async fn a_cron_trigger_with_a_cwd_runs_in_a_worktree_of_that_repository() {
    let ws = Arc::new(crate::FakeWorkspace::new().with_repo("/repos/app"));
    let m = mock([]);
    let fx = start_in_workspace(
        files(vec![trigger_in("nightly", NIGHTLY, "/repos/app")]),
        KernelConfig::default(),
        &ws,
        &m,
    )
    .await;
    assert_eq!(
        info(&fx, "nightly").cwd.as_deref(),
        Some("/repos/app"),
        "the list shows where the trigger works"
    );

    step(&fx, 5 * HOUR).await;
    all_ended(&fx).await;
    let made = runs(&fx).await;
    assert_eq!(made.len(), 1);
    let cwd = m.log().specs[0].cwd.clone();
    let thread = fx.kernel.run_thread(&made[0].id).await.unwrap();
    assert_eq!(
        cwd,
        std::path::PathBuf::from(crate::FAKE_WORKTREES)
            .join("app-000000000000")
            .join(thread.as_str()),
        "the run works in a worktree, never in the repository itself"
    );
    assert_ne!(cwd, std::path::PathBuf::from("/repos/app"));
}

#[tokio::test(start_paused = true)]
async fn a_home_relative_cwd_is_expanded_against_the_configured_home() {
    let ws = Arc::new(crate::FakeWorkspace::new().with_repo("/home/u/code/app"));
    let m = mock([]);
    let config = KernelConfig {
        home: Some("/home/u".into()),
        ..KernelConfig::default()
    };
    let fx = start_in_workspace(
        files(vec![trigger_in("nightly", NIGHTLY, "~/code/app")]),
        config,
        &ws,
        &m,
    )
    .await;
    step(&fx, 5 * HOUR).await;
    all_ended(&fx).await;
    let made = runs(&fx).await;
    assert_eq!(made.len(), 1);
    assert!(
        made[0].spec["cwd"] == "/home/u/code/app",
        "the task records the expanded directory: {}",
        made[0].spec
    );
    assert!(
        m.log().specs[0].cwd.starts_with(crate::FAKE_WORKTREES),
        "{:?}",
        m.log().specs[0].cwd
    );
}

#[tokio::test(start_paused = true)]
async fn a_trigger_without_a_cwd_still_uses_the_trigger_directory() {
    let ws = Arc::new(crate::FakeWorkspace::new());
    let m = mock([]);
    let config = KernelConfig {
        trigger_cwd: "/data/workspace".into(),
        scratch_root: Some("/data/workspace".into()),
        ..KernelConfig::default()
    };
    let fx = start_in_workspace(
        files(vec![trigger("nightly", NIGHTLY, "go")]),
        config,
        &ws,
        &m,
    )
    .await;
    step(&fx, 5 * HOUR).await;
    all_ended(&fx).await;
    assert_eq!(
        m.log().specs[0].cwd,
        std::path::PathBuf::from("/data/workspace")
    );
    assert_eq!(info(&fx, "nightly").cwd, None);
}

#[tokio::test(start_paused = true)]
async fn a_missing_cwd_refuses_the_run_before_spawn_and_backs_the_trigger_off_until_it_returns() {
    let ws = Arc::new(crate::FakeWorkspace::new());
    let m = mock([]);
    let fx = start_in_workspace(
        files(vec![trigger_in("nightly", NIGHTLY, "/mnt/gone/app")]),
        KernelConfig::default(),
        &ws,
        &m,
    )
    .await;

    step(&fx, 5 * HOUR).await;
    until(async || info(&fx, "nightly").failures.eq(&1).then_some(())).await;
    // Nothing was spawned, and the run carries the typed refusal.
    assert!(m.log().specs.is_empty());
    let made = runs(&fx).await;
    assert_eq!(made.len(), 1);
    match fx
        .journal_with_done(&made[0].id)
        .await
        .last()
        .unwrap()
        .1
        .clone()
    {
        reno_proto::ExecEvent::Done {
            reason: Some(reno_proto::StopDetail::StartRefused { failure, detail }),
            ..
        } => {
            assert_eq!(failure, reno_proto::StartRefusal::WorkspaceRefused);
            assert!(detail.contains("/mnt/gone/app"), "{detail}");
        }
        other => panic!("not a typed refusal: {other:?}"),
    }
    // A missing repository needs a person, like a signed-out account: the trigger backs off.
    let t = info(&fx, "nightly");
    assert_eq!(t.status, TriggerStatus::BackedOff);
    assert!(t.backoff_until_ms.is_some());
    assert_eq!(firings(&fx).await[0].outcome, FiringOutcome::BackedOff);

    // The disk comes back; when the backoff ends the next run starts and the episode is over.
    ws.declare_repo("/mnt/gone/app");
    // (The cron's next occurrence is tomorrow's, past the backoff.)
    step(&fx, 24 * HOUR).await;
    until(async || info(&fx, "nightly").failures.eq(&0).then_some(())).await;
    assert_eq!(m.log().specs.len(), 1);
    assert!(m.log().specs[0].cwd.starts_with(crate::FAKE_WORKTREES));
}

#[tokio::test(start_paused = true)]
async fn a_home_relative_cwd_with_no_home_refuses_like_a_missing_directory() {
    let ws = Arc::new(crate::FakeWorkspace::new());
    let m = mock([]);
    let fx = start_in_workspace(
        files(vec![trigger_in("nightly", NIGHTLY, "~/code/app")]),
        KernelConfig::default(),
        &ws,
        &m,
    )
    .await;
    step(&fx, 5 * HOUR).await;
    until(async || info(&fx, "nightly").failures.eq(&1).then_some(())).await;
    assert!(m.log().specs.is_empty());
}
