//! Trigger state and firings: what the scheduler persists (ARCHITECTURE 7).
//!
//! # Two tables
//!
//! - `triggers`: one row per trigger in force. It holds the plan (when the trigger is next due),
//!   the run it last started and its backoff, so a restart does not recompute history. The row
//!   says which definition version and which `when` it was planned for; the kernel decides
//!   whether a new definition keeps the plan. The store only keeps what it is given.
//! - `firings`: one row per occurrence acted on, under a unique nonce
//!   ([`firing_nonce`], `<kind>:<trigger>:<scheduled_at>`). The row is written in the same
//!   transaction as the task and run it starts, the trigger's new state and the event, so a kill
//!   at any point leaves either the whole firing or none of it (ARCH-8).
//!
//! # Firing
//!
//! [`Store::fire_trigger`] decides, inside its transaction, between three results:
//!
//! - **duplicate**: the nonce exists. Nothing is written.
//! - **coalesced**: the trigger's previous run is still going. No task or run is made; the
//!   occurrence is recorded as merged into that run. A run is going while it is `queued`,
//!   `leased`, `running`, waiting for input or approval, or evaluating. `interrupted` does not
//!   count, although it is not terminal: nothing resumes an interrupted run by itself, so a
//!   trigger whose run was cut off by a crash must not be held back by it forever.
//! - **fired**: a task and a pinned run are created.
//!
//! # Time
//!
//! The store never reads the clock: every call takes `now`.

use reno_proto::defs::BotPin;
use reno_proto::{ExecutorId, FiringOutcome, RunId, RunStatus};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};

use crate::db::Store;
use crate::error::{DbResult, Result, StoreError, is_unique_violation};
use crate::events;
use crate::runs::{NewRun, NewTask, parse_status, tx_create_task, tx_enqueue_run};
use crate::writer::Durability;

/// The nonce of one occurrence: `<kind>:<trigger>:<scheduled_at>`, with `scheduled_at` in
/// milliseconds since the epoch. Firing the same occurrence twice gives the same nonce.
pub fn firing_nonce(kind: &str, trigger: &str, scheduled_at: i64) -> String {
    format!("{kind}:{trigger}:{scheduled_at}")
}

/// The most firings one [`Store::firings`] call returns.
const FIRINGS_LIMIT_MAX: u32 = 500;

/// A trigger's persisted state.
#[derive(Debug, Clone, PartialEq)]
pub struct TriggerRow {
    /// The trigger's name.
    pub name: String,
    /// The version of its definition this row was planned against.
    pub def_version: u32,
    /// The `when` it was planned for, as the definition's JSON.
    pub schedule: Value,
    /// Whether the definition is enabled.
    pub enabled: bool,
    /// When it is next due, in milliseconds since the epoch. `None` for a trigger with no time
    /// (`fs`, `manual`), a finished one-shot and a disabled trigger.
    pub next_due_at: Option<i64>,
    /// When it last fired.
    pub last_fired_at: Option<i64>,
    /// The run it last started.
    pub last_run: Option<RunId>,
    /// Consecutive failures to start for an account-level reason.
    pub failures: u32,
    /// Until when it is backed off. `Some` exactly when `failures > 0`.
    pub backoff_until: Option<i64>,
}

/// The run an occurrence starts when it fires.
#[derive(Debug, Clone, PartialEq)]
pub struct FiringRun {
    /// The task: its goal is the prompt.
    pub task: NewTask,
    /// The run's id.
    pub run: RunId,
    /// The executor.
    pub executor: ExecutorId,
    /// The bot version the run is pinned to.
    pub bot: BotPin,
    /// Where the router sent it and why, stored with the run.
    pub routing: Option<reno_proto::RoutingDecision>,
}

/// One occurrence to fire.
#[derive(Debug, Clone, PartialEq)]
pub struct NewFiring {
    /// The trigger.
    pub trigger: String,
    /// The first part of the nonce: the trigger's kind (`cron`, `every`, `manual`...).
    pub kind: String,
    /// The occurrence, in milliseconds since the epoch. With the kind and the trigger it makes
    /// the nonce.
    pub scheduled_at: i64,
    /// How many occurrences this stands for: this one and the ones merged into it. At least 1.
    pub missed: u32,
    /// When the trigger is next due once this occurrence is dealt with.
    pub next_due_at: Option<i64>,
    /// The run it starts, unless it is coalesced or a duplicate.
    pub run: FiringRun,
}

/// What [`Store::fire_trigger`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FireResult {
    /// `Fired`, `Coalesced` or `Duplicate`.
    pub outcome: FiringOutcome,
    /// The run started, or the run the occurrence was merged into. `None` for a duplicate.
    pub run: Option<RunId>,
}

/// A stored firing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FiringRecord {
    /// The nonce.
    pub nonce: String,
    /// The trigger.
    pub trigger: String,
    /// The occurrence, in milliseconds since the epoch.
    pub scheduled_at: i64,
    /// When it was acted on.
    pub fired_at: i64,
    /// The run it started, or the run it was merged into.
    pub run: RunId,
    /// `Fired`, `Coalesced` or `BackedOff`.
    pub outcome: FiringOutcome,
    /// How many occurrences it stands for.
    pub missed: u32,
}

/// Whether a run in `status` holds back the trigger's next occurrence (see the module docs).
fn is_going(status: RunStatus) -> bool {
    !status.is_terminal() && status != RunStatus::Interrupted
}

fn outcome_str(outcome: FiringOutcome) -> &'static str {
    match outcome {
        FiringOutcome::Fired => "fired",
        FiringOutcome::Coalesced => "coalesced",
        FiringOutcome::BackedOff => "backed_off",
        FiringOutcome::Duplicate => "duplicate",
    }
}

fn parse_outcome(text: &str) -> Result<FiringOutcome> {
    [
        FiringOutcome::Fired,
        FiringOutcome::Coalesced,
        FiringOutcome::BackedOff,
    ]
    .into_iter()
    .find(|outcome| outcome_str(*outcome) == text)
    .ok_or_else(|| StoreError::Corrupt(format!("unknown firing outcome {text:?}")))
}

fn parse_run(text: &str) -> Result<RunId> {
    text.parse()
        .map_err(|_| StoreError::Corrupt(format!("bad run id {text:?}")))
}

const TRIGGER_COLS: &str = "name, def_version, schedule, enabled, next_due_at, last_fired_at, \
     last_run_id, failures, backoff_until";

fn trigger_from_row(row: &Row<'_>) -> rusqlite::Result<Result<TriggerRow>> {
    let name: String = row.get(0)?;
    let version: i64 = row.get(1)?;
    let schedule: String = row.get(2)?;
    let enabled: i64 = row.get(3)?;
    let next_due_at = row.get(4)?;
    let last_fired_at = row.get(5)?;
    let last_run: Option<String> = row.get(6)?;
    let failures: i64 = row.get(7)?;
    let backoff_until = row.get(8)?;
    Ok((|| {
        Ok(TriggerRow {
            def_version: u32::try_from(version)
                .map_err(|_| StoreError::Corrupt(format!("trigger {name}: bad version")))?,
            schedule: serde_json::from_str(&schedule)
                .map_err(|e| StoreError::Corrupt(format!("trigger {name}: schedule: {e}")))?,
            enabled: enabled != 0,
            next_due_at,
            last_fired_at,
            last_run: last_run.as_deref().map(parse_run).transpose()?,
            failures: u32::try_from(failures)
                .map_err(|_| StoreError::Corrupt(format!("trigger {name}: bad failures")))?,
            backoff_until,
            name,
        })
    })())
}

fn load_trigger(conn: &Connection, name: &str) -> Result<Option<TriggerRow>> {
    conn.prepare_cached(&format!(
        "SELECT {TRIGGER_COLS} FROM triggers WHERE name = ?1"
    ))
    .db()?
    .query_row([name], trigger_from_row)
    .optional()
    .db()?
    .transpose()
}

fn firing_exists(conn: &Connection, nonce: &str) -> Result<bool> {
    conn.prepare_cached("SELECT 1 FROM firings WHERE nonce = ?1")
        .db()?
        .exists([nonce])
        .db()
}

fn run_status(conn: &Connection, run: &RunId) -> Result<Option<RunStatus>> {
    conn.prepare_cached("SELECT status FROM runs WHERE id = ?1")
        .db()?
        .query_row([run.as_str()], |r| r.get::<_, String>(0))
        .optional()
        .db()?
        .map(|status| parse_status(&status))
        .transpose()
}

fn not_found(name: &str) -> StoreError {
    StoreError::NotFound {
        kind: "trigger",
        id: name.to_owned(),
    }
}

fn tx_plan(conn: &Connection, rows: &[TriggerRow], removed: &[String], now: i64) -> Result<()> {
    for row in rows {
        if load_trigger(conn, &row.name)?.as_ref() == Some(row) {
            continue;
        }
        conn.prepare_cached(
            "INSERT INTO triggers (name, def_version, schedule, enabled, next_due_at,
                                   last_fired_at, last_run_id, failures, backoff_until)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
             ON CONFLICT (name) DO UPDATE SET
                 def_version = excluded.def_version, schedule = excluded.schedule,
                 enabled = excluded.enabled, next_due_at = excluded.next_due_at,
                 last_fired_at = excluded.last_fired_at, last_run_id = excluded.last_run_id,
                 failures = excluded.failures, backoff_until = excluded.backoff_until",
        )
        .db()?
        .execute(params![
            row.name,
            row.def_version,
            row.schedule.to_string(),
            row.enabled,
            row.next_due_at,
            row.last_fired_at,
            row.last_run.as_ref().map(RunId::as_str),
            row.failures,
            row.backoff_until,
        ])
        .db()?;
        events::insert(
            conn,
            now,
            "trigger.planned",
            ("trigger", &row.name),
            None,
            &json!({
                "version": row.def_version,
                "enabled": row.enabled,
                "nextDueAt": row.next_due_at,
            }),
        )?;
    }
    for name in removed {
        let deleted = conn
            .prepare_cached("DELETE FROM triggers WHERE name = ?1")
            .db()?
            .execute([name])
            .db()?;
        if deleted > 0 {
            events::insert(
                conn,
                now,
                "trigger.removed",
                ("trigger", name),
                None,
                &json!({}),
            )?;
        }
    }
    Ok(())
}

fn tx_fire(conn: &Connection, fire: &NewFiring, now: i64) -> Result<FireResult> {
    let nonce = firing_nonce(&fire.kind, &fire.trigger, fire.scheduled_at);
    if firing_exists(conn, &nonce)? {
        return Ok(FireResult {
            outcome: FiringOutcome::Duplicate,
            run: None,
        });
    }
    let row = load_trigger(conn, &fire.trigger)?.ok_or_else(|| not_found(&fire.trigger))?;
    let going = match &row.last_run {
        Some(run) => run_status(conn, run)?
            .filter(|status| is_going(*status))
            .map(|_| run.clone()),
        None => None,
    };
    let (outcome, run) = match going {
        Some(into) => (FiringOutcome::Coalesced, into),
        None => {
            tx_create_task(conn, &fire.run.task, now)?;
            tx_enqueue_run(
                conn,
                &NewRun {
                    task: &fire.run.task.id,
                    run: &fire.run.run,
                    executor: &fire.run.executor,
                    bot: Some(&fire.run.bot),
                    routing: fire.run.routing.as_ref(),
                    thread: None,
                },
                now,
            )?;
            (FiringOutcome::Fired, fire.run.run.clone())
        }
    };
    conn.prepare_cached(
        "INSERT INTO firings (nonce, trigger_name, scheduled_at, fired_at, run_id, outcome, missed)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )
    .db()?
    .execute(params![
        nonce,
        fire.trigger,
        fire.scheduled_at,
        now,
        run.as_str(),
        outcome_str(outcome),
        fire.missed.max(1),
    ])
    .map_err(|e| {
        // The nonce was free a moment ago and the writer is one thread, so this is the database
        // refusing what the check above missed. The caller's transaction rolls back whole.
        if is_unique_violation(&e) {
            StoreError::AlreadyExists {
                kind: "firing",
                id: nonce.clone(),
            }
        } else {
            crate::error::db(e)
        }
    })?;
    conn.prepare_cached(
        "UPDATE triggers SET next_due_at = ?2, last_fired_at = ?3,
                last_run_id = CASE WHEN ?4 THEN ?5 ELSE last_run_id END
         WHERE name = ?1",
    )
    .db()?
    .execute(params![
        fire.trigger,
        fire.next_due_at,
        now,
        outcome == FiringOutcome::Fired,
        run.as_str(),
    ])
    .db()?;
    events::insert(
        conn,
        now,
        if outcome == FiringOutcome::Fired {
            "trigger.fired"
        } else {
            "trigger.coalesced"
        },
        ("trigger", &fire.trigger),
        None,
        &json!({
            "nonce": nonce,
            "scheduledAt": fire.scheduled_at,
            "missed": fire.missed.max(1),
            "run": run,
            "nextDueAt": fire.next_due_at,
        }),
    )?;
    Ok(FireResult {
        outcome,
        run: Some(run),
    })
}

fn tx_start_failed(
    conn: &Connection,
    name: &str,
    run: Option<&RunId>,
    failures: u32,
    backoff_until: i64,
    reason: &str,
    now: i64,
) -> Result<bool> {
    let changed = conn
        .prepare_cached(
            "UPDATE triggers SET failures = ?3, backoff_until = ?4
             WHERE name = ?1 AND (?2 IS NULL OR last_run_id = ?2)",
        )
        .db()?
        .execute(params![
            name,
            run.map(RunId::as_str),
            failures,
            backoff_until
        ])
        .db()?;
    if changed == 0 {
        return Ok(false);
    }
    if let Some(run) = run {
        conn.prepare_cached(
            "UPDATE firings SET outcome = 'backed_off'
             WHERE trigger_name = ?1 AND run_id = ?2 AND outcome = 'fired'",
        )
        .db()?
        .execute(params![name, run.as_str()])
        .db()?;
    }
    events::insert(
        conn,
        now,
        // The first failure of an episode is the one notification; the rest only extend it.
        if failures == 1 {
            "trigger.backoff_started"
        } else {
            "trigger.backoff_extended"
        },
        ("trigger", name),
        None,
        &json!({
            "run": run,
            "failures": failures,
            "backoffUntil": backoff_until,
            "reason": reason,
        }),
    )?;
    Ok(true)
}

fn tx_recovered(conn: &Connection, name: &str, run: &RunId, now: i64) -> Result<bool> {
    let changed = conn
        .prepare_cached(
            "UPDATE triggers SET failures = 0, backoff_until = NULL
             WHERE name = ?1 AND last_run_id = ?2 AND failures > 0",
        )
        .db()?
        .execute(params![name, run.as_str()])
        .db()?;
    if changed == 0 {
        return Ok(false);
    }
    events::insert(
        conn,
        now,
        "trigger.recovered",
        ("trigger", name),
        None,
        &json!({ "run": run }),
    )?;
    Ok(true)
}

impl Store {
    /// Every trigger row, by name.
    pub async fn list_triggers(&self) -> Result<Vec<TriggerRow>> {
        self.read(|conn| {
            let mut stmt = conn
                .prepare_cached(&format!(
                    "SELECT {TRIGGER_COLS} FROM triggers ORDER BY name"
                ))
                .db()?;
            let rows = stmt.query_map([], trigger_from_row).db()?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.db()??);
            }
            Ok(out)
        })
        .await
    }

    /// Makes `rows` the state of their triggers and deletes the rows of `removed`, in one
    /// transaction. A row equal to what is stored is skipped, with no write and no event, so
    /// planning what is already planned costs nothing. Each change writes a `trigger.planned` or
    /// `trigger.removed` event.
    pub async fn plan_triggers(
        &self,
        rows: Vec<TriggerRow>,
        removed: Vec<String>,
        now: i64,
    ) -> Result<()> {
        self.write(Durability::Normal, move |c| {
            tx_plan(c, &rows, &removed, now)
        })
        .await
    }

    /// Fires one occurrence: see the module docs for the three results. The firing row, the task
    /// and run it starts, the trigger's new state and the event commit together.
    ///
    /// Errors: [`StoreError::NotFound`] for an unknown trigger or an unstored bot version;
    /// [`StoreError::AlreadyExists`] for a task or run id that is taken (mint new ones and call
    /// again). Nothing is written on an error.
    pub async fn fire_trigger(&self, fire: NewFiring, now: i64) -> Result<FireResult> {
        self.write(Durability::Normal, move |c| tx_fire(c, &fire, now))
            .await
    }

    /// Records that trigger `name` could not start a run for an account-level reason: it has now
    /// failed `failures` times in a row and is backed off until `backoff_until`.
    ///
    /// With `run`, the run it last started failed to start: the firing of that run becomes
    /// `backed_off`, and nothing is written when `run` is no longer the trigger's last run
    /// (the answer is `false`). With no `run`, no run could be made (no executor of the bot is
    /// here), so there is no firing to mark.
    ///
    /// The first failure of an episode (`failures == 1`) writes `trigger.backoff_started`, the
    /// one notification; later ones write `trigger.backoff_extended`.
    pub async fn trigger_start_failed(
        &self,
        name: &str,
        run: Option<&RunId>,
        failures: u32,
        backoff_until: i64,
        reason: &str,
        now: i64,
    ) -> Result<bool> {
        let (name, run, reason) = (name.to_owned(), run.cloned(), reason.to_owned());
        self.write(Durability::Normal, move |c| {
            tx_start_failed(
                c,
                &name,
                run.as_ref(),
                failures,
                backoff_until,
                &reason,
                now,
            )
        })
        .await
    }

    /// Ends a backoff: `run`, the trigger's last run, started. Writes `trigger.recovered` and
    /// returns `true`, or returns `false` writing nothing when the trigger was not backed off or
    /// `run` is not its last run.
    pub async fn trigger_recovered(&self, name: &str, run: &RunId, now: i64) -> Result<bool> {
        let (name, run) = (name.to_owned(), run.clone());
        self.write(Durability::Normal, move |c| {
            tx_recovered(c, &name, &run, now)
        })
        .await
    }

    /// The newest firings, newest first: of one trigger, or of all. `limit` is cut to 500.
    pub async fn firings(&self, trigger: Option<&str>, limit: u32) -> Result<Vec<FiringRecord>> {
        let trigger = trigger.map(str::to_owned);
        let limit = limit.clamp(1, FIRINGS_LIMIT_MAX);
        self.read(move |conn| {
            let mut stmt = conn
                .prepare_cached(
                    "SELECT nonce, trigger_name, scheduled_at, fired_at, run_id, outcome, missed
                     FROM firings WHERE (?1 IS NULL OR trigger_name = ?1)
                     ORDER BY id DESC LIMIT ?2",
                )
                .db()?;
            let rows = stmt
                .query_map(params![trigger, limit], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, i64>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, String>(5)?,
                        r.get::<_, i64>(6)?,
                    ))
                })
                .db()?;
            let mut out = Vec::new();
            for row in rows {
                let (nonce, trigger, scheduled_at, fired_at, run, outcome, missed) = row.db()?;
                out.push(FiringRecord {
                    nonce,
                    trigger,
                    scheduled_at,
                    fired_at,
                    run: parse_run(&run)?,
                    outcome: parse_outcome(&outcome)?,
                    missed: u32::try_from(missed)
                        .map_err(|_| StoreError::Corrupt("bad firing missed count".into()))?,
                });
            }
            Ok(out)
        })
        .await
    }
}
