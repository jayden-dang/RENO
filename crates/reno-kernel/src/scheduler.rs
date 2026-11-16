//! The scheduler actor: fires triggers (ARCHITECTURE 7, 17.2).
//!
//! One task owns every trigger's plan. It holds the validated triggers of the definitions in
//! force, works out when each is next due (`schedule.rs`) and sleeps until the earliest of those
//! deadlines. Nothing polls (ARCH-10): it is woken by
//!
//! - the deadline itself;
//! - a change of the definitions (the `watch` the loader publishes to), which re-plans without a
//!   restart: a new trigger starts, an edited one is re-planned, a disabled or removed one stops;
//! - a command: `trigger.fire`, a change reported by the `Watch` port for an `fs` trigger, or the
//!   dispatcher saying a run started or could not start.
//!
//! - the system's wake-up ([`WakeSignal`], raised by the daemon on the OS notification). A
//!   deadline is wall-clock time and the timer is monotonic, which stops counting while a laptop
//!   sleeps; on wake the scheduler reads the clock again and fires what came due meanwhile.
//!
//! There is no periodic wake. With no deadline the scheduler arms no timer at all, and otherwise
//! exactly one, for the earliest deadline.
//!
//! # Rules (ARCHITECTURE 7)
//!
//! - **Nonce.** An occurrence is `<kind>:<trigger>:<scheduled_at>`, inserted under a unique
//!   constraint in the same transaction as the run it starts. Firing it again, from this
//!   scheduler after a restart or from another scheduler on the same store, is a no-op (the
//!   store answers `duplicate`, and the scheduler reloads what the other one wrote).
//! - **Missed occurrences coalesce.** When a trigger is found due after downtime or a long
//!   sleep, the occurrences between its due time and now are counted and merged into one run.
//!   Its `scheduled_at` is the earliest of them; its prompt says how late it is and how many were
//!   merged ([`lateness_note`]).
//! - **A reused target coalesces.** If the trigger's last run is still going the occurrence
//!   starts nothing and is recorded as `coalesced` (the store decides, in the firing's
//!   transaction).
//! - **Backoff.** When the run a trigger started cannot start for an account-level reason
//!   (`NotInstalled`, `NotSignedIn`, `NoEligibleExecutor`, `SecretMissing`), or no executor of its bot is here
//!   at all, the trigger backs off 5 minutes, doubling per consecutive failure up to 24 hours.
//!   Occurrences that come due meanwhile are merged into the one that fires when it ends. The
//!   first failure of an episode writes the one `trigger.backoff_started` event; a run that
//!   starts ends the episode (`trigger.recovered`).
//! - **Persistence.** `triggers` rows carry `next_due_at`, `last_fired_at`, the last run and the
//!   backoff, so a restart plans from them and does not recompute history. A row is kept across
//!   a new definition version when its `when` is unchanged (an edited prompt does not re-fire a
//!   finished `at`); an unfired `after` starts its delay again from the new version.
//!
//! `fs` triggers have no due time. Each gets a watch through the `Watch` port and fires when it
//! reports a settled change. A change that arrives during a backoff is remembered (in memory
//! only: a restart forgets file changes that were not acted on) and fires when the backoff ends.
//!
//! `trigger.fire` runs on this same task, so it is ordered with the timer firings.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use reno_proto::defs::DefKind;
use reno_proto::defs::{
    PromptVars, TriggerDef, TriggerKind, format_instant, format_span, lateness_note, render_prompt,
};
use reno_proto::{
    BudgetStop, FiringOutcome, RunId, TaskId, TriggerFireResult, TriggerInfo, TriggerStatus,
};
use reno_store::{FiringRun, NewFiring, NewTask, StoreError, TriggerRow};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::budget::{self, Pauses};
use crate::defs::CurrentDefs;
use crate::error::{KernelError, Result};
use crate::kernel::{bot_launch, mint};
use crate::schedule::{Schedule, backoff_step};
use crate::shared::Shared;
use crate::wake::WakeSignal;
use crate::watch::{Watch, WatchSpec};

/// Commands the scheduler accepts. The queue is bounded and every sender waits when it is full;
/// the scheduler never waits on a sender (it wakes the dispatcher through `Notify`), so the
/// waits cannot form a cycle (ARCH-11).
pub(crate) const COMMAND_QUEUE: usize = 256;

/// How often `fire` mints ids again after the store says the last ones are taken.
const MINT_ATTEMPTS: usize = 8;

/// Why a run could not start, when the reason is about the account or the machine and not about
/// the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StartFailure {
    NotInstalled,
    NotSignedIn,
    NoEligibleExecutor,
    /// A secret the bot lists is not set. Like `NotSignedIn`, only a person can fix it, so the
    /// trigger backs off rather than starting a run that fails the same way every time.
    SecretMissing,
    /// The working directory the trigger names is gone (an unmounted disk, a moved repository).
    /// Only a person can put it back, so the trigger backs off rather than failing a run on
    /// every occurrence.
    WorkingDirMissing,
}

impl StartFailure {
    fn as_str(self) -> &'static str {
        match self {
            Self::NotInstalled => "NotInstalled",
            Self::NotSignedIn => "NotSignedIn",
            Self::SecretMissing => "SecretMissing",
            Self::WorkingDirMissing => "WorkingDirMissing",
            Self::NoEligibleExecutor => "NoEligibleExecutor",
        }
    }
}

#[derive(Debug)]
pub(crate) enum SchedCmd {
    /// `trigger.fire`.
    Fire {
        name: String,
        reply: oneshot::Sender<Result<TriggerFireResult>>,
    },
    /// The `Watch` port reported a settled change for an `fs` trigger.
    FsChanged { name: String, generation: u64 },
    /// A run started: the executor accepted it.
    RunStarted { run: RunId },
    /// A run could not start, for this reason.
    StartFailed { run: RunId, why: StartFailure },
}

/// A running file watch of one trigger.
struct ActiveWatch {
    spec: WatchSpec,
    generation: u64,
    stop: CancellationToken,
}

/// A file watch that could not be started, and when the start is tried again.
struct WatchFailure {
    message: String,
    /// Failed starts in a row, from 1.
    attempts: u32,
    /// When to try again, in wall-clock milliseconds.
    retry_at: i64,
}

/// One trigger in force.
struct Plan {
    def: TriggerDef,
    schedule: Option<Schedule>,
    /// Why the trigger cannot run; it is shown as failed until its definition changes.
    error: Option<String>,
    /// What is stored.
    row: TriggerRow,
    watch: Option<ActiveWatch>,
    /// Why the watch is not running, while the start is being retried. The trigger shows as
    /// failed meanwhile; unlike `error` it ends when the start works.
    watch_failure: Option<WatchFailure>,
    /// When the first file change that has not been acted on arrived (an `fs` trigger).
    pending_fs: Option<i64>,
    /// When to try again after the store failed.
    retry_at: Option<i64>,
    /// A pause holds this trigger and the hold has been recorded; no timer is armed for it until
    /// the pause is lifted.
    held: bool,
}

impl Plan {
    fn running(&self) -> bool {
        self.def.enabled && self.error.is_none() && self.watch_failure.is_none()
    }

    /// When the start of a failed watch is tried again.
    fn watch_retry_at(&self) -> Option<i64> {
        self.watch_failure
            .as_ref()
            .filter(|_| self.def.enabled && self.error.is_none())
            .map(|failure| failure.retry_at)
    }

    /// Whether a pause holds this trigger back: everything is paused, or its bot is.
    fn paused(&self, pauses: &Pauses) -> bool {
        pauses.holds(&self.def.target.bot)
    }

    /// When the scheduler must next act for this trigger, in wall-clock milliseconds. A held
    /// trigger has none: nothing is armed while a pause lasts, and its due time stays as it was,
    /// so lifting the pause finds the occurrences due and merges them into one run.
    fn deadline(&self, pauses: &Pauses) -> Option<i64> {
        if !self.running() {
            return self.watch_retry_at();
        }
        if self.held && self.paused(pauses) {
            return None;
        }
        let backoff = self.row.backoff_until.unwrap_or(i64::MIN);
        let timed = self.row.next_due_at.map(|due| due.max(backoff));
        let fs = self.pending_fs.map(|_| backoff);
        [timed, fs, self.retry_at].into_iter().flatten().min()
    }

    fn status(&self) -> TriggerStatus {
        if !self.def.enabled {
            TriggerStatus::Disabled
        } else if self.error.is_some() || self.watch_failure.is_some() {
            TriggerStatus::Failed
        } else if self.row.failures > 0 {
            TriggerStatus::BackedOff
        } else {
            match self.def.when {
                TriggerKind::Manual {} => TriggerStatus::Manual,
                TriggerKind::Fs { .. } => TriggerStatus::Watching,
                _ if self.row.next_due_at.is_some() => TriggerStatus::Scheduled,
                _ => TriggerStatus::Done,
            }
        }
    }

    fn info(&self) -> TriggerInfo {
        let ms = |t: i64| u64::try_from(t).unwrap_or(0);
        TriggerInfo {
            name: self.def.name.clone(),
            kind: self.def.when.name().to_owned(),
            bot: self.def.target.bot.clone(),
            version: self.row.def_version,
            status: self.status(),
            next_due_at_ms: self.row.next_due_at.filter(|_| self.running()).map(ms),
            last_fired_at_ms: self.row.last_fired_at.map(ms),
            last_run_id: self.row.last_run.clone(),
            failures: self.row.failures,
            backoff_until_ms: self.row.backoff_until.map(ms),
            error: self.error.clone().or_else(|| {
                self.watch_failure.as_ref().map(|failure| {
                    format!(
                        "{}; retrying (failed start {})",
                        failure.message, failure.attempts
                    )
                })
            }),
            cwd: self.def.target.cwd.clone(),
        }
    }
}

/// The wait until `earliest` (wall-clock milliseconds), at least a millisecond; `None` for no
/// deadline.
pub(crate) fn until_deadline(earliest: Option<i64>, now: i64) -> Option<Duration> {
    earliest.map(|due| {
        let ms = u64::try_from(due.saturating_sub(now)).unwrap_or(0).max(1);
        Duration::from_millis(ms)
    })
}

/// Sleeps for `wait`, or for ever when there is no deadline.
async fn sleep_until_due(wait: Option<Duration>) {
    match wait {
        Some(wait) => sleep(wait).await,
        None => std::future::pending().await,
    }
}

/// What a fired occurrence is, for the firing and the prompt.
struct Occurrence {
    /// The first part of the nonce.
    kind: &'static str,
    scheduled_at: i64,
    missed: u32,
    /// When the trigger is due next; `None` for no time.
    next_due_at: Option<i64>,
}

pub(crate) struct Scheduler {
    shared: Arc<Shared>,
    watch_port: Option<Arc<dyn Watch>>,
    plans: BTreeMap<String, Plan>,
    commands: mpsc::Receiver<SchedCmd>,
    defs: watch::Receiver<Arc<CurrentDefs>>,
    /// What is paused; a change makes the loop read the clock again, so a lifted pause catches
    /// up at once.
    pauses: watch::Receiver<Arc<Pauses>>,
    /// The system's wake-ups, when the daemon reports them.
    wake: Option<watch::Receiver<u64>>,
    next_generation: u64,
}

/// Plans the triggers in force and spawns the scheduler task. Returns once the first plan is
/// made and published, so `trigger.list` is never empty because it was early.
pub(crate) async fn start(
    shared: &Arc<Shared>,
    watch_port: Option<Arc<dyn Watch>>,
    wake: Option<WakeSignal>,
) -> Result<()> {
    let Some(commands) = shared.take_sched_commands() else {
        return Ok(());
    };
    let mut scheduler = Scheduler {
        shared: Arc::clone(shared),
        watch_port,
        plans: BTreeMap::new(),
        commands,
        defs: shared.defs_tx.subscribe(),
        pauses: shared.pauses_tx.subscribe(),
        wake: wake.map(|signal| signal.subscribe()),
        next_generation: 0,
    };
    let current = scheduler.defs.borrow_and_update().clone();
    scheduler.replan(&current).await?;
    shared.spawn(scheduler.run());
    Ok(())
}

impl Scheduler {
    fn now(&self) -> i64 {
        self.shared.db.now_ms()
    }

    async fn run(mut self) {
        loop {
            self.fire_due().await;
            let wait = self.sleep_for();
            let woke = async {
                match self.wake.as_mut() {
                    Some(wake) => wake.changed().await.is_ok(),
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                () = self.shared.root.cancelled() => break,
                command = self.commands.recv() => match command {
                    Some(command) => self.command(command).await,
                    None => break,
                },
                changed = self.defs.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let current = self.defs.borrow_and_update().clone();
                    if let Err(error) = self.replan(&current).await {
                        tracing::error!(%error, "could not plan the triggers");
                    }
                }
                changed = self.pauses.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    // A trigger that is no longer held is due again at once (the loop's next
                    // turn fires what is due); the rest stay held.
                    let pauses = self.pauses.borrow_and_update().clone();
                    for plan in self.plans.values_mut() {
                        if !plan.paused(&pauses) {
                            plan.held = false;
                        }
                    }
                    self.publish();
                }
                alive = woke => {
                    // The loop's next turn reads the clock and fires what came due.
                    if !alive {
                        self.wake = None;
                    }
                }
                () = sleep_until_due(wait) => {}
            }
        }
        for plan in self.plans.values_mut() {
            if let Some(active) = plan.watch.take() {
                active.stop.cancel();
            }
        }
    }

    /// How long to sleep: until the earliest deadline, or `None` when there is none, in which
    /// case no timer is armed.
    fn sleep_for(&self) -> Option<Duration> {
        let pauses = self.pauses.borrow().clone();
        let earliest = self
            .plans
            .values()
            .filter_map(|plan| plan.deadline(&pauses))
            .min();
        until_deadline(earliest, self.now())
    }

    fn publish(&self) {
        let infos: Vec<TriggerInfo> = self.plans.values().map(Plan::info).collect();
        self.shared.triggers_tx.send_if_modified(|current| {
            if **current == infos {
                return false;
            }
            *current = Arc::new(infos);
            true
        });
    }

    // ---- planning ----------------------------------------------------------------------

    /// Brings the plans in line with the definitions in force and the rows in the store, and
    /// (re)starts or stops the file watches.
    async fn replan(&mut self, current: &CurrentDefs) -> Result<()> {
        let now = self.now();
        let stored: BTreeMap<String, TriggerRow> = self
            .shared
            .db
            .list_triggers()
            .await?
            .into_iter()
            .map(|row| (row.name.clone(), row))
            .collect();

        let mut plans = BTreeMap::new();
        let mut rows = Vec::new();
        for (name, validated) in current.set().triggers() {
            let Some(version) = current.version_of(DefKind::Trigger, name) else {
                continue;
            };
            let def = validated.def().clone();
            let before = self.plans.remove(name);
            let known = before
                .as_ref()
                .map(|plan| plan.row.clone())
                .or_else(|| stored.get(name).cloned());
            let (schedule, error) = match Schedule::from_kind(&def.when) {
                Ok(schedule) => (Some(schedule), None),
                Err(message) => (None, Some(message)),
            };
            let row = plan_row(
                &def,
                version.version,
                i64::try_from(version.created_at_ms).unwrap_or(i64::MAX),
                schedule.as_ref(),
                known,
                now,
            );
            if stored.get(name) != Some(&row) {
                rows.push(row.clone());
            }
            let mut plan = Plan {
                def,
                schedule,
                error,
                row,
                watch: None,
                watch_failure: None,
                pending_fs: before.as_ref().and_then(|p| p.pending_fs),
                retry_at: None,
                held: before.as_ref().is_some_and(|p| p.held),
            };
            if let Some(mut old) = before {
                plan.watch = old.watch.take();
            }
            plans.insert(name.clone(), plan);
        }
        let removed: Vec<String> = stored
            .keys()
            .filter(|name| !plans.contains_key(*name))
            .cloned()
            .collect();
        // Plans that left the definitions stop watching.
        for (_, mut gone) in std::mem::take(&mut self.plans) {
            if let Some(active) = gone.watch.take() {
                active.stop.cancel();
            }
        }
        if !rows.is_empty() || !removed.is_empty() {
            self.shared.db.plan_triggers(rows, removed).await?;
        }
        self.plans = plans;
        self.sync_watches();
        self.publish();
        Ok(())
    }

    /// Starts, keeps or stops the file watch of every `fs` trigger.
    fn sync_watches(&mut self) {
        let names: Vec<String> = self.plans.keys().cloned().collect();
        for name in names {
            self.sync_watch(&name);
        }
    }

    /// Tries again every watch whose retry time has come.
    fn retry_watches(&mut self, now: i64) {
        let names: Vec<String> = self
            .plans
            .iter()
            .filter(|(_, plan)| plan.watch_retry_at().is_some_and(|at| at <= now))
            .map(|(name, _)| name.clone())
            .collect();
        for name in names {
            self.sync_watch(&name);
        }
    }

    /// Starts, keeps or stops the file watch of one `fs` trigger. A start that fails is retried
    /// at `watch_retry`'s next delay, until it works or the definition changes.
    fn sync_watch(&mut self, name: &str) {
        let backoff = self.shared.config.watch_retry;
        let now = self.now();
        let wanted = self.plans.get(name).and_then(|plan| {
            let enabled = plan.def.enabled && plan.error.is_none();
            match &plan.def.when {
                TriggerKind::Fs { paths, debounce } if enabled => {
                    let debounce = reno_proto::defs::parse_duration(debounce).ok()?;
                    Some(WatchSpec {
                        trigger: name.to_owned(),
                        paths: paths.clone(),
                        debounce,
                    })
                }
                _ => None,
            }
        });
        let Some(plan) = self.plans.get_mut(name) else {
            return;
        };
        let keep = matches!((&plan.watch, &wanted), (Some(a), Some(w)) if a.spec == *w);
        if keep {
            return;
        }
        if let Some(old) = plan.watch.take() {
            old.stop.cancel();
        }
        let Some(spec) = wanted else {
            plan.watch_failure = None;
            return;
        };
        let Some(port) = self.watch_port.clone() else {
            plan.error = Some("this daemon has no file watcher".to_owned());
            return;
        };
        match port.watch(&spec) {
            Ok(stream) => {
                if let Some(failure) = plan.watch_failure.take() {
                    tracing::info!(
                        trigger = %name, attempts = failure.attempts,
                        "the file watch started; the trigger is watching again"
                    );
                }
                self.next_generation += 1;
                let generation = self.next_generation;
                let stop = self.shared.root.child_token();
                plan.watch = Some(ActiveWatch {
                    spec,
                    generation,
                    stop: stop.clone(),
                });
                self.shared.spawn(forward(
                    self.shared.clone(),
                    name.to_owned(),
                    generation,
                    stream,
                    stop,
                ));
            }
            Err(error) => {
                let attempts = plan
                    .watch_failure
                    .as_ref()
                    .map_or(1, |failure| failure.attempts.saturating_add(1));
                let delay = i64::try_from(backoff.delay(attempts).as_millis()).unwrap_or(i64::MAX);
                if attempts == 1 {
                    // Once: the rest of the retries are visible in `trigger.list`.
                    tracing::warn!(
                        trigger = %name, %error,
                        "cannot watch the paths; the trigger is failed and the start is retried"
                    );
                }
                plan.watch_failure = Some(WatchFailure {
                    message: error.to_string(),
                    attempts,
                    retry_at: now.saturating_add(delay),
                });
            }
        }
    }

    // ---- commands ----------------------------------------------------------------------

    async fn command(&mut self, command: SchedCmd) {
        match command {
            SchedCmd::Fire { name, reply } => {
                let result = self.fire_now(&name).await;
                let _ = reply.send(result);
            }
            SchedCmd::FsChanged { name, generation } => {
                let now = self.now();
                if let Some(plan) = self.plans.get_mut(&name)
                    && plan
                        .watch
                        .as_ref()
                        .is_some_and(|w| w.generation == generation)
                {
                    plan.pending_fs.get_or_insert(now);
                }
            }
            SchedCmd::RunStarted { run } => self.run_started(&run).await,
            SchedCmd::StartFailed { run, why } => self.start_failed(&run, why).await,
        }
        self.publish();
    }

    fn plan_of_run(&self, run: &RunId) -> Option<String> {
        self.plans
            .iter()
            .find(|(_, plan)| plan.row.last_run.as_ref() == Some(run))
            .map(|(name, _)| name.clone())
    }

    /// A run started: the trigger that made it is healthy.
    async fn run_started(&mut self, run: &RunId) {
        let Some(name) = self.plan_of_run(run) else {
            return;
        };
        let healthy = self.plans.get(&name).is_some_and(|p| p.row.failures > 0);
        if !healthy {
            return;
        }
        match self.shared.db.trigger_recovered(&name, run).await {
            Ok(true) => {
                if let Some(plan) = self.plans.get_mut(&name) {
                    plan.row.failures = 0;
                    plan.row.backoff_until = None;
                }
                tracing::info!(trigger = %name, "the trigger started a run again; backoff over");
            }
            Ok(false) => {}
            Err(error) => tracing::warn!(trigger = %name, %error, "could not record recovery"),
        }
    }

    /// A run could not start for an account-level reason: back its trigger off.
    async fn start_failed(&mut self, run: &RunId, why: StartFailure) {
        let Some(name) = self.plan_of_run(run) else {
            return;
        };
        self.back_off(&name, Some(run), why).await;
    }

    async fn back_off(&mut self, name: &str, run: Option<&RunId>, why: StartFailure) {
        let now = self.now();
        let Some(plan) = self.plans.get_mut(name) else {
            return;
        };
        let failures = plan.row.failures.saturating_add(1);
        let step = i64::try_from(backoff_step(failures).as_millis()).unwrap_or(i64::MAX);
        let until = now.saturating_add(step);
        let stored = self
            .shared
            .db
            .trigger_start_failed(name, run, failures, until, why.as_str())
            .await;
        match stored {
            Ok(true) => {
                plan.row.failures = failures;
                plan.row.backoff_until = Some(until);
                if failures == 1 {
                    tracing::warn!(
                        trigger = %name, reason = why.as_str(),
                        "the trigger cannot start runs; backing off"
                    );
                }
            }
            Ok(false) => {}
            Err(error) => tracing::warn!(trigger = %name, %error, "could not record backoff"),
        }
    }

    // ---- firing ------------------------------------------------------------------------

    /// Fires every trigger that is due now.
    async fn fire_due(&mut self) {
        let now = self.now();
        self.retry_watches(now);
        let pauses = self.pauses.borrow().clone();
        let mut held = Vec::new();
        let mut due = Vec::new();
        for (name, plan) in &self.plans {
            let Some(at) = plan.deadline(&pauses).filter(|at| *at <= now) else {
                continue;
            };
            if plan.paused(&pauses) {
                held.push((name.clone(), at));
            } else {
                due.push(name.clone());
            }
        }
        for (name, at) in held {
            self.hold(&name, at).await;
        }
        for name in due {
            self.fire_scheduled(&name, now).await;
        }
        if !self.plans.is_empty() {
            self.publish();
        }
    }

    /// A pause holds `name`, which came due at `due_at`: say so once, and arm nothing for it
    /// until the pause is lifted. Its due time stays, so the catch-up finds every occurrence
    /// that came due meanwhile and merges them into one run.
    async fn hold(&mut self, name: &str, due_at: i64) {
        // An `fs` trigger has no due time of its own; its deadline is the end of a backoff.
        let due_at = if due_at == i64::MIN {
            self.plans
                .get(name)
                .and_then(|plan| plan.pending_fs)
                .unwrap_or_else(|| self.now())
        } else {
            due_at
        };
        if let Some(plan) = self.plans.get_mut(name) {
            plan.held = true;
        }
        match self.shared.db.hold_trigger_occurrence(name, due_at).await {
            Ok(()) => tracing::info!(trigger = %name, "paused: the trigger waits for the resume"),
            Err(error) => tracing::warn!(trigger = %name, %error, "could not record the hold"),
        }
    }

    /// An occurrence whose bot has used its daily tokens: recorded as skipped and not run. The
    /// trigger moves on to its next due time, and it is not backed off: the bot is not failing
    /// to start, it is out of budget.
    async fn skip(&mut self, name: &str, occurrence: &Occurrence, stop: BudgetStop) {
        let reason = format!("budget_{}", stop.kind.as_str());
        let recorded = self
            .shared
            .db
            .skip_trigger_occurrence(
                name,
                occurrence.scheduled_at,
                occurrence.missed,
                occurrence.next_due_at,
                &reason,
            )
            .await;
        match recorded {
            Ok(()) => {
                if let Some(plan) = self.plans.get_mut(name) {
                    plan.row.next_due_at = occurrence.next_due_at;
                }
                tracing::warn!(trigger = %name, %stop, "skipped an occurrence: the bot is out of budget");
            }
            Err(error) => {
                tracing::error!(trigger = %name, %error, "could not record a skipped occurrence");
                self.restore_pending(name, occurrence);
                if let Some(plan) = self.plans.get_mut(name) {
                    plan.retry_at = Some(self.shared.db.now_ms().saturating_add(
                        i64::try_from(self.shared.config.store_retry.as_millis()).unwrap_or(1_000),
                    ));
                }
            }
        }
    }

    async fn fire_scheduled(&mut self, name: &str, now: i64) {
        let Some(plan) = self.plans.get_mut(name) else {
            return;
        };
        plan.retry_at = None;
        let occurrence = if let Some(due) = plan.row.next_due_at {
            let Some(schedule) = &plan.schedule else {
                return;
            };
            let found = schedule.due_at(due, now);
            Occurrence {
                kind: plan.def.when.name(),
                scheduled_at: due,
                missed: found.count,
                next_due_at: found.next,
            }
        } else if let Some(since) = plan.pending_fs.take() {
            Occurrence {
                kind: "fs",
                scheduled_at: since,
                missed: 1,
                next_due_at: None,
            }
        } else {
            return;
        };
        match self.fire(name, &occurrence, now).await {
            Ok(_) => {}
            Err(KernelError::BudgetExhausted { stop, .. }) => {
                self.skip(name, &occurrence, stop).await;
            }
            Err(KernelError::NoExecutor(message)) => {
                tracing::warn!(trigger = %name, %message, "no executor for the trigger's bot");
                // The occurrence stays due; it fires, merged with the ones after it, when the
                // backoff ends.
                self.restore_pending(name, &occurrence);
                self.back_off(name, None, StartFailure::NoEligibleExecutor)
                    .await;
            }
            Err(error) => {
                tracing::error!(trigger = %name, %error, "could not fire the trigger");
                self.restore_pending(name, &occurrence);
                if let Some(plan) = self.plans.get_mut(name) {
                    plan.retry_at = Some(now.saturating_add(
                        i64::try_from(self.shared.config.store_retry.as_millis()).unwrap_or(1_000),
                    ));
                }
            }
        }
    }

    /// Puts back what a failed firing took, so the occurrence is tried again.
    fn restore_pending(&mut self, name: &str, occurrence: &Occurrence) {
        if let Some(plan) = self.plans.get_mut(name)
            && occurrence.kind == "fs"
        {
            plan.pending_fs = Some(occurrence.scheduled_at);
        }
    }

    /// `trigger.fire`: an occurrence at this moment, whatever the schedule says. It does not move
    /// the schedule, and it is not held back by a backoff.
    async fn fire_now(&mut self, name: &str) -> Result<TriggerFireResult> {
        let now = self.now();
        let plan = self
            .plans
            .get(name)
            .ok_or_else(|| KernelError::TriggerNotFound(name.to_owned()))?;
        if !plan.def.enabled {
            return Err(KernelError::TriggerDisabled(name.to_owned()));
        }
        if let Some(error) = &plan.error {
            return Err(KernelError::Invalid(format!(
                "trigger {name:?} cannot run: {error}"
            )));
        }
        let occurrence = Occurrence {
            kind: "manual",
            scheduled_at: now,
            missed: 1,
            next_due_at: plan.row.next_due_at,
        };
        let outcome = self.fire(name, &occurrence, now).await?;
        Ok(TriggerFireResult {
            outcome: outcome.0,
            run_id: outcome.1,
            scheduled_at_ms: u64::try_from(now).unwrap_or(0),
        })
    }

    /// Fires one occurrence through the store and applies the result to the plan.
    async fn fire(
        &mut self,
        name: &str,
        occurrence: &Occurrence,
        now: i64,
    ) -> Result<(FiringOutcome, Option<RunId>)> {
        let plan = self
            .plans
            .get(name)
            .ok_or_else(|| KernelError::TriggerNotFound(name.to_owned()))?;
        let prompt = prompt_for(plan, occurrence, now);
        if let Some(stop) = budget::token_refusal(&self.shared, &plan.def.target.bot).await? {
            return Err(KernelError::BudgetExhausted {
                bot: plan.def.target.bot.clone(),
                stop,
            });
        }
        let (executor, bot, routing, model) = bot_launch(&self.shared, &plan.def.target.bot)?;
        // The definition's directory when it names one (checked when the run starts), else the
        // daemon's scratch workspace.
        let named = match plan
            .def
            .target
            .resolved_cwd(self.shared.config.home.as_deref())
        {
            Some(Ok(dir)) => Some(dir),
            Some(Err(why)) => {
                // Keep the text as written: the run is refused at start, naming it.
                tracing::warn!(trigger = %name, %why, "cannot expand the trigger's directory");
                plan.def.target.cwd.as_deref().map(std::path::PathBuf::from)
            }
            None => None,
        };
        let cwd = named.as_ref().unwrap_or(&self.shared.config.trigger_cwd);
        let cwd = cwd
            .to_str()
            .ok_or_else(|| {
                KernelError::Invalid("the trigger working directory is not UTF-8".into())
            })?
            .to_owned();
        let mut spec = serde_json::json!({ "cwd": cwd, "model": model, "trigger": name });
        if named.is_some() {
            spec[crate::workspaces::CWD_NAMED] = true.into();
        }

        let mut result = None;
        for _ in 0..MINT_ATTEMPTS {
            let fire = NewFiring {
                trigger: name.to_owned(),
                kind: occurrence.kind.to_owned(),
                scheduled_at: occurrence.scheduled_at,
                missed: occurrence.missed,
                next_due_at: occurrence.next_due_at,
                run: FiringRun {
                    task: NewTask {
                        id: mint(&self.shared, TaskId::from_suffix)?,
                        goal: prompt.clone(),
                        class: None,
                        spec: spec.clone(),
                    },
                    run: mint(&self.shared, RunId::from_suffix)?,
                    executor: executor.clone(),
                    bot: bot.clone(),
                    routing: Some(routing.clone()),
                },
            };
            match self.shared.db.fire_trigger(fire).await {
                Ok(fired) => {
                    result = Some(fired);
                    break;
                }
                Err(StoreError::AlreadyExists { kind: "firing", .. }) => {
                    result = Some(reno_store::FireResult {
                        outcome: FiringOutcome::Duplicate,
                        run: None,
                    });
                    break;
                }
                Err(StoreError::AlreadyExists { .. }) => {}
                Err(error) => return Err(error.into()),
            }
        }
        let result = result.ok_or_else(|| KernelError::Invalid("no free task or run id".into()))?;

        match result.outcome {
            FiringOutcome::Duplicate => self.reload(name).await,
            FiringOutcome::Fired | FiringOutcome::Coalesced | FiringOutcome::BackedOff => {
                if let Some(plan) = self.plans.get_mut(name) {
                    plan.row.last_fired_at = Some(now);
                    plan.row.next_due_at = occurrence.next_due_at;
                    if result.outcome == FiringOutcome::Fired {
                        plan.row.last_run = result.run.clone();
                    }
                }
                if result.outcome == FiringOutcome::Fired {
                    self.shared.wake.notify_one();
                    self.shared.mark_runs_dirty();
                }
            }
        }
        Ok((result.outcome, result.run))
    }

    /// Takes the stored row as the truth, after another scheduler fired first.
    async fn reload(&mut self, name: &str) {
        match self.shared.db.list_triggers().await {
            Ok(rows) => {
                if let Some(row) = rows.into_iter().find(|row| row.name == name)
                    && let Some(plan) = self.plans.get_mut(name)
                {
                    plan.row = row;
                }
            }
            Err(error) => tracing::warn!(trigger = %name, %error, "could not reload the trigger"),
        }
    }
}

/// Forwards a watch's settled changes to the scheduler until it is stopped or the port closes.
async fn forward(
    shared: Arc<Shared>,
    name: String,
    generation: u64,
    mut stream: crate::watch::WatchStream,
    stop: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            change = stream.changes.recv() => {
                if change.is_none() {
                    tracing::warn!(trigger = %name, "the file watch ended");
                    return;
                }
                let command = SchedCmd::FsChanged { name: name.clone(), generation };
                if shared.sched_tx.send(command).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// The row a trigger should have, given what is known of it.
///
/// A row whose `when` is unchanged keeps its plan, backoff and history (a restart, an edited
/// prompt). Otherwise the plan is new: due from `now`, with the version stored at `anchor`.
fn plan_row(
    def: &TriggerDef,
    version: u32,
    anchor: i64,
    schedule: Option<&Schedule>,
    known: Option<TriggerRow>,
    now: i64,
) -> TriggerRow {
    let when = serde_json::to_value(&def.when).unwrap_or(serde_json::Value::Null);
    let history = |known: &Option<TriggerRow>| {
        (
            known.as_ref().and_then(|k| k.last_fired_at),
            known.as_ref().and_then(|k| k.last_run.clone()),
        )
    };
    let (last_fired_at, last_run) = history(&known);
    let mut row = TriggerRow {
        name: def.name.clone(),
        def_version: version,
        schedule: when.clone(),
        enabled: def.enabled,
        next_due_at: None,
        last_fired_at,
        last_run,
        failures: 0,
        backoff_until: None,
    };
    if !def.enabled {
        return row;
    }
    let unchanged = known.as_ref().filter(|k| k.schedule == when && k.enabled);
    match (unchanged, schedule) {
        (Some(k), Some(Schedule::After { delay_ms })) if k.last_fired_at.is_none() => {
            // An unfired `after` counts its delay from the version now in force.
            row.next_due_at = Some(anchor.saturating_add(*delay_ms));
            row.failures = k.failures;
            row.backoff_until = k.backoff_until;
        }
        (Some(k), _) => {
            row.next_due_at = k.next_due_at;
            row.failures = k.failures;
            row.backoff_until = k.backoff_until;
        }
        (None, Some(schedule)) => {
            // A one-shot that fired under this same `when` stays done after a re-enable.
            let finished = matches!(schedule, Schedule::At { .. } | Schedule::After { .. })
                && known
                    .as_ref()
                    .is_some_and(|k| k.schedule == when && k.last_fired_at.is_some());
            if !finished {
                row.next_due_at = schedule.first_due(anchor, now);
            }
        }
        (None, None) => {}
    }
    row
}

/// The prompt of an occurrence: the template filled in, and the note when it is late.
fn prompt_for(plan: &Plan, occurrence: &Occurrence, now: i64) -> String {
    let zone = plan.schedule.as_ref().and_then(Schedule::zone);
    let scheduled = format_instant(occurrence.scheduled_at, zone);
    let late = Duration::from_millis(u64::try_from(now - occurrence.scheduled_at).unwrap_or(0));
    let lateness = format_span(late);
    let rendered = render_prompt(
        &plan.def.target.prompt,
        &PromptVars {
            trigger: &plan.def.name,
            scheduled_at: &scheduled,
            lateness: &lateness,
        },
    );
    match lateness_note(&scheduled, late, occurrence.missed, &plan.def.name) {
        Some(note) => format!("{rendered}\n\n{note}"),
        None => rendered,
    }
}
