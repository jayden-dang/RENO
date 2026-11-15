//! Tasks and runs: the typed API over the run lifecycle (ARCHITECTURE 4.1).
//!
//! Every state change here updates its row and inserts its `events` row in the same transaction
//! (ARCH-8). Every write on a claimed run carries the fence and applies it in the `WHERE`
//! clause (ARCH-9).
//!
//! Design notes:
//!
//! - A claim picks the oldest run (by `created_at`, then insertion order) that is `queued`, or
//!   `leased` with an expired lease (its claimer died before starting). A `running`, waiting or
//!   evaluating run with an expired lease is *not* re-claimable: recovery moves it to
//!   `interrupted` first, because whether work with side effects may be redone is a decision
//!   (ARCHITECTURE 4.1).
//! - Every claim, including a re-claim, opens an attempt row numbered by its fence. Transitions
//!   mirror the run status into that attempt row.
//! - Heartbeats extend the lease but write no event: a lease deadline is ephemeral, not state
//!   anyone replays, and an event per 20 s per run would dominate the log.
//! - `transition` and `heartbeat` check, in order: the run exists, the fence is current, then
//!   the lifecycle rule.

use reno_proto::{ExecutorId, RunId, RunStatus, TaskId};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::json;

use crate::db::Store;
use crate::error::{DbResult, Result, StoreError, is_unique_violation};
use crate::events;
use crate::writer::Durability;

/// A task to create.
#[derive(Debug, Clone, PartialEq)]
pub struct NewTask {
    /// Its id.
    pub id: TaskId,
    /// What it is for, in words.
    pub goal: String,
    /// Optional task class, used by routing statistics.
    pub class: Option<String>,
    /// Free-form specification; `{}` when there is none.
    pub spec: serde_json::Value,
}

/// A successful claim: the caller now owns the run under this fence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    /// The claimed run.
    pub run_id: RunId,
    /// The new fence. Every later write on the run must carry it.
    pub fence: u64,
}

/// A run row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    /// Its id.
    pub id: RunId,
    /// The task it works on.
    pub task_id: TaskId,
    /// The executor that runs it.
    pub executor: ExecutorId,
    /// The model in use, once known.
    pub model: Option<String>,
    /// Where it is in its lifecycle.
    pub status: RunStatus,
    /// Who holds the lease.
    pub lease_owner: Option<String>,
    /// The current fence; 0 until first claimed.
    pub lease_fence: u64,
    /// When the lease runs out, in milliseconds since the epoch.
    pub lease_expires_at: Option<i64>,
    /// Handle for resuming the harness session.
    pub session_ref: Option<String>,
    /// When a waiting run should be woken.
    pub wake_at: Option<i64>,
    /// Process id of the executor's child, if one was recorded.
    pub pid: Option<u32>,
    /// Process group id of the executor's child, if one was recorded.
    pub pgid: Option<u32>,
    /// Opaque start time of that child (see `reno_harness::proc::ProcessRecord::start_time`), so
    /// a sweep can tell it from a stranger that inherited the pid. Set together with `pid`.
    pub process_start: Option<u64>,
    /// The goal of the run's task: the prompt it was submitted with.
    pub goal: String,
    /// The task's free-form specification (`{}` when it has none).
    pub spec: serde_json::Value,
    /// When the run last changed, in milliseconds since the epoch: the time of its newest event,
    /// or its creation time if it has none.
    pub updated_at: i64,
    /// When it was enqueued.
    pub created_at: i64,
    /// When it first ran.
    pub started_at: Option<i64>,
    /// When it reached a terminal state.
    pub ended_at: Option<i64>,
}

/// The wire/storage name of a status; matches the `CHECK` constraint and the serde names.
pub(crate) fn status_str(s: RunStatus) -> &'static str {
    match s {
        RunStatus::Queued => "queued",
        RunStatus::Leased => "leased",
        RunStatus::Running => "running",
        RunStatus::WaitingInput => "waiting_input",
        RunStatus::WaitingApproval => "waiting_approval",
        RunStatus::Evaluating => "evaluating",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Interrupted => "interrupted",
        RunStatus::Cancelled => "cancelled",
    }
}

fn parse_status(s: &str) -> Result<RunStatus> {
    RunStatus::ALL
        .into_iter()
        .find(|&c| status_str(c) == s)
        .ok_or_else(|| StoreError::Corrupt(format!("unknown run status {s:?}")))
}

/// Statuses in which a run holds a live lease.
fn holds_lease(s: RunStatus) -> bool {
    matches!(
        s,
        RunStatus::Leased
            | RunStatus::Running
            | RunStatus::WaitingInput
            | RunStatus::WaitingApproval
            | RunStatus::Evaluating
    )
}

fn to_i64(v: u64, what: &str) -> Result<i64> {
    i64::try_from(v).map_err(|_| StoreError::InvalidArgument(format!("{what} out of range")))
}

fn from_i64(v: i64, what: &str) -> Result<u64> {
    u64::try_from(v).map_err(|_| StoreError::Corrupt(format!("negative {what}")))
}

fn parse_id<T: std::str::FromStr>(s: &str, what: &str) -> Result<T> {
    s.parse()
        .map_err(|_| StoreError::Corrupt(format!("bad {what} {s:?}")))
}

fn expiry(now: i64, lease_ms: u64) -> Result<i64> {
    i64::try_from(lease_ms)
        .ok()
        .and_then(|l| now.checked_add(l))
        .ok_or_else(|| StoreError::InvalidArgument("lease overflows".into()))
}

const RUN_COLS: &str = "id, task_id, executor, model, status, lease_owner, lease_fence, \
     lease_expires_at, session_ref, wake_at, pid, pgid, created_at, started_at, ended_at, \
     process_start, \
     (SELECT goal FROM tasks WHERE tasks.id = runs.task_id), \
     (SELECT spec FROM tasks WHERE tasks.id = runs.task_id), \
     COALESCE((SELECT MAX(ts) FROM events \
               WHERE subject_type = 'run' AND subject_id = runs.id), runs.created_at)";

fn run_from_row(row: &Row<'_>) -> rusqlite::Result<Result<RunRecord>> {
    let id: String = row.get(0)?;
    let task_id: String = row.get(1)?;
    let executor: String = row.get(2)?;
    let status: String = row.get(4)?;
    let fence: i64 = row.get(6)?;
    let pid: Option<i64> = row.get(10)?;
    let pgid: Option<i64> = row.get(11)?;
    let model = row.get(3)?;
    let lease_owner = row.get(5)?;
    let lease_expires_at = row.get(7)?;
    let session_ref = row.get(8)?;
    let wake_at = row.get(9)?;
    let created_at = row.get(12)?;
    let started_at = row.get(13)?;
    let ended_at = row.get(14)?;
    let process_start: Option<i64> = row.get(15)?;
    let goal: String = row.get(16)?;
    let spec: String = row.get(17)?;
    let updated_at = row.get(18)?;
    Ok((|| {
        let id_u32 = |v: Option<i64>| {
            v.map(|v| u32::try_from(v).map_err(|_| StoreError::Corrupt(format!("bad pid {v}"))))
                .transpose()
        };
        Ok(RunRecord {
            id: parse_id(&id, "run id")?,
            task_id: parse_id(&task_id, "task id")?,
            executor: parse_id(&executor, "executor id")?,
            model,
            status: parse_status(&status)?,
            lease_owner,
            lease_fence: from_i64(fence, "fence")?,
            lease_expires_at,
            session_ref,
            wake_at,
            pid: id_u32(pid)?,
            pgid: id_u32(pgid)?,
            process_start: process_start
                .map(|v| from_i64(v, "process start time"))
                .transpose()?,
            goal,
            spec: serde_json::from_str(&spec)
                .map_err(|e| StoreError::Corrupt(format!("task spec: {e}")))?,
            updated_at,
            created_at,
            started_at,
            ended_at,
        })
    })())
}

fn load(conn: &Connection, run: &RunId) -> Result<Option<RunRecord>> {
    conn.prepare_cached(&format!("SELECT {RUN_COLS} FROM runs WHERE id = ?1"))
        .db()?
        .query_row([run.as_str()], run_from_row)
        .optional()
        .db()?
        .transpose()
}

/// Loads a run and checks the fence: the common prologue of every fenced write.
fn load_fenced(conn: &Connection, run: &RunId, fence: u64) -> Result<RunRecord> {
    let rec = load(conn, run)?.ok_or_else(|| StoreError::NotFound {
        kind: "run",
        id: run.to_string(),
    })?;
    if rec.lease_fence != fence {
        return Err(StoreError::StaleFence {
            run: run.clone(),
            supplied: fence,
            current: rec.lease_fence,
        });
    }
    Ok(rec)
}

pub(crate) fn tx_create_task(conn: &Connection, task: &NewTask, now: i64) -> Result<()> {
    let spec = task.spec.to_string();
    conn.prepare_cached(
        "INSERT INTO tasks (id, class, goal, spec, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
    )
    .db()?
    .execute(params![task.id.as_str(), task.class, task.goal, spec, now])
    .map_err(|e| {
        if is_unique_violation(&e) {
            StoreError::AlreadyExists {
                kind: "task",
                id: task.id.to_string(),
            }
        } else {
            crate::error::db(e)
        }
    })?;
    events::insert(
        conn,
        now,
        "task.created",
        ("task", task.id.as_str()),
        None,
        &json!({ "goal": task.goal, "class": task.class }),
    )
}

pub(crate) fn tx_enqueue_run(
    conn: &Connection,
    task: &TaskId,
    run: &RunId,
    executor: &ExecutorId,
    now: i64,
) -> Result<()> {
    let task_exists = conn
        .prepare_cached("SELECT 1 FROM tasks WHERE id = ?1")
        .db()?
        .exists([task.as_str()])
        .db()?;
    if !task_exists {
        return Err(StoreError::NotFound {
            kind: "task",
            id: task.to_string(),
        });
    }
    conn.prepare_cached(
        "INSERT INTO runs (id, task_id, executor, status, created_at)
         VALUES (?1, ?2, ?3, 'queued', ?4)",
    )
    .db()?
    .execute(params![run.as_str(), task.as_str(), executor.as_str(), now])
    .map_err(|e| {
        if is_unique_violation(&e) {
            StoreError::AlreadyExists {
                kind: "run",
                id: run.to_string(),
            }
        } else {
            crate::error::db(e)
        }
    })?;
    events::insert(
        conn,
        now,
        "run.enqueued",
        ("run", run.as_str()),
        None,
        &json!({ "task": task, "executor": executor }),
    )
}

pub(crate) fn tx_claim_next(
    conn: &Connection,
    owner: &str,
    now: i64,
    lease_ms: u64,
) -> Result<Option<Claim>> {
    let expires = expiry(now, lease_ms)?;
    let candidate: Option<(String, i64, String, Option<String>)> = conn
        .prepare_cached(
            "SELECT id, lease_fence, status, lease_owner FROM runs
             WHERE status IN ('queued', 'leased')
               AND (status = 'queued' OR lease_expires_at <= ?1)
             ORDER BY created_at, rowid LIMIT 1",
        )
        .db()?
        .query_row([now], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
        .optional()
        .db()?;
    let Some((id, old_fence, from, previous_owner)) = candidate else {
        return Ok(None);
    };
    let run: RunId = parse_id(&id, "run id")?;
    let fence = old_fence
        .checked_add(1)
        .ok_or_else(|| StoreError::Corrupt("fence overflow".into()))?;
    let changed = conn
        .prepare_cached(
            "UPDATE runs SET status = 'leased', lease_owner = ?3, lease_fence = ?4,
                 lease_expires_at = ?5
             WHERE id = ?1 AND lease_fence = ?2",
        )
        .db()?
        .execute(params![id, old_fence, owner, fence, expires])
        .db()?;
    if changed != 1 {
        return Err(StoreError::Corrupt("claim lost its own row".into()));
    }
    conn.prepare_cached(
        "INSERT INTO attempts (run_id, n, fence, status) VALUES (?1, ?2, ?2, 'leased')",
    )
    .db()?
    .execute(params![id, fence])
    .db()?;
    events::insert(
        conn,
        now,
        "run.claimed",
        ("run", &id),
        Some(owner),
        &json!({
            "fence": fence,
            "from": from,
            "previousOwner": previous_owner,
            "leaseExpiresAt": expires,
        }),
    )?;
    Ok(Some(Claim {
        run_id: run,
        fence: from_i64(fence, "fence")?,
    }))
}

pub(crate) fn tx_heartbeat(
    conn: &Connection,
    run: &RunId,
    fence: u64,
    now: i64,
    lease_ms: u64,
) -> Result<()> {
    let rec = load_fenced(conn, run, fence)?;
    if !holds_lease(rec.status) {
        return Err(StoreError::NotLeased {
            run: run.clone(),
            status: rec.status,
        });
    }
    let expires = expiry(now, lease_ms)?;
    let changed = conn
        .prepare_cached("UPDATE runs SET lease_expires_at = ?3 WHERE id = ?1 AND lease_fence = ?2")
        .db()?
        .execute(params![run.as_str(), to_i64(fence, "fence")?, expires])
        .db()?;
    if changed != 1 {
        return Err(StoreError::Corrupt("heartbeat lost its row".into()));
    }
    Ok(())
}

pub(crate) fn tx_transition(
    conn: &Connection,
    run: &RunId,
    fence: u64,
    to: RunStatus,
    now: i64,
) -> Result<()> {
    let rec = load_fenced(conn, run, fence)?;
    let from = rec.status;
    if !from.can_transition_to(to) {
        return Err(StoreError::IllegalTransition {
            run: run.clone(),
            from,
            to,
        });
    }
    let releases = matches!(
        to,
        RunStatus::Queued
            | RunStatus::Interrupted
            | RunStatus::Succeeded
            | RunStatus::Failed
            | RunStatus::Cancelled
    );
    let set_ended = to.is_terminal();
    let requeue = to == RunStatus::Queued;
    let fence_i = to_i64(fence, "fence")?;
    let changed = conn
        .prepare_cached(
            "UPDATE runs SET status = ?3,
                 lease_owner = CASE WHEN ?4 THEN NULL ELSE lease_owner END,
                 lease_expires_at = CASE WHEN ?4 THEN NULL ELSE lease_expires_at END,
                 started_at = CASE WHEN ?3 = 'running' AND started_at IS NULL THEN ?7
                                   WHEN ?6 THEN NULL ELSE started_at END,
                 ended_at = CASE WHEN ?5 THEN ?7 WHEN ?6 THEN NULL ELSE ended_at END
             WHERE id = ?1 AND lease_fence = ?2",
        )
        .db()?
        .execute(params![
            run.as_str(),
            fence_i,
            status_str(to),
            releases,
            set_ended,
            requeue,
            now
        ])
        .db()?;
    if changed != 1 {
        return Err(StoreError::Corrupt("transition lost its row".into()));
    }
    conn.prepare_cached("UPDATE attempts SET status = ?3 WHERE run_id = ?1 AND fence = ?2")
        .db()?
        .execute(params![run.as_str(), fence_i, status_str(to)])
        .db()?;
    events::insert(
        conn,
        now,
        "run.transitioned",
        ("run", run.as_str()),
        None,
        &json!({ "from": from, "to": to, "fence": fence }),
    )?;
    if set_ended {
        events::insert(
            conn,
            now,
            "run.ended",
            ("run", run.as_str()),
            None,
            &json!({ "status": to, "fence": fence }),
        )?;
    }
    Ok(())
}

pub(crate) fn tx_record_process(
    conn: &Connection,
    run: &RunId,
    fence: u64,
    pid: u32,
    pgid: u32,
    start_time: u64,
    now: i64,
) -> Result<()> {
    load_fenced(conn, run, fence)?;
    let changed = conn
        .prepare_cached(
            "UPDATE runs SET pid = ?3, pgid = ?4, process_start = ?5
             WHERE id = ?1 AND lease_fence = ?2",
        )
        .db()?
        .execute(params![
            run.as_str(),
            to_i64(fence, "fence")?,
            pid,
            pgid,
            to_i64(start_time, "process start time")?
        ])
        .db()?;
    if changed != 1 {
        return Err(StoreError::Corrupt("record_process lost its row".into()));
    }
    events::insert(
        conn,
        now,
        "run.process_recorded",
        ("run", run.as_str()),
        None,
        &json!({ "pid": pid, "pgid": pgid, "startTime": start_time, "fence": fence }),
    )
}

fn query_runs(
    conn: &Connection,
    sql: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<RunRecord>> {
    let mut stmt = conn.prepare_cached(sql).db()?;
    let rows = stmt.query_map(args, run_from_row).db()?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.db()??);
    }
    Ok(out)
}

impl Store {
    /// Creates a task and its `task.created` event. `now` is milliseconds since the epoch.
    pub async fn create_task(&self, task: NewTask, now: i64) -> Result<()> {
        self.write(Durability::Normal, move |c| tx_create_task(c, &task, now))
            .await
    }

    /// Queues a new run of `task` on `executor`, with fence 0.
    pub async fn enqueue_run(
        &self,
        task: &TaskId,
        run: &RunId,
        executor: &ExecutorId,
        now: i64,
    ) -> Result<()> {
        let (task, run, executor) = (task.clone(), run.clone(), executor.clone());
        self.write(Durability::Normal, move |c| {
            tx_enqueue_run(c, &task, &run, &executor, now)
        })
        .await
    }

    /// Atomically leases the oldest claimable run to `owner` with `fence + 1`, for `lease_ms`
    /// from `now`. Claimable means `queued`, or `leased` with an expired lease. Returns `None`
    /// when nothing is claimable.
    pub async fn claim_next(&self, owner: &str, now: i64, lease_ms: u64) -> Result<Option<Claim>> {
        let owner = owner.to_owned();
        self.write(Durability::Normal, move |c| {
            tx_claim_next(c, &owner, now, lease_ms)
        })
        .await
    }

    /// Extends the lease to `now + lease_ms`. Fails with [`StoreError::StaleFence`] if the run was
    /// re-claimed, and with [`StoreError::NotLeased`] if it holds no lease. Writes no event.
    pub async fn heartbeat(&self, run: &RunId, fence: u64, now: i64, lease_ms: u64) -> Result<()> {
        let run = run.clone();
        self.write(Durability::Normal, move |c| {
            tx_heartbeat(c, &run, fence, now, lease_ms)
        })
        .await
    }

    /// Moves a run along an edge of the lifecycle. Fails with [`StoreError::StaleFence`] when
    /// `fence` is not current, and with [`StoreError::IllegalTransition`] when the diagram has no
    /// such edge. The fence is checked first.
    pub async fn transition(&self, run: &RunId, fence: u64, to: RunStatus, now: i64) -> Result<()> {
        let run = run.clone();
        self.write(Durability::Normal, move |c| {
            tx_transition(c, &run, fence, to, now)
        })
        .await
    }

    /// Records the executor child's pid, process group and start time for the orphan sweep
    /// (ARCHITECTURE 17.3). The start time is what lets the sweep refuse a reused pid. A durable
    /// write: it runs alone with `synchronous=FULL`, because a lost record leaves an orphan
    /// nobody can find. A run holds one record; a later call replaces it.
    pub async fn record_process(
        &self,
        run: &RunId,
        fence: u64,
        pid: u32,
        pgid: u32,
        start_time: u64,
        now: i64,
    ) -> Result<()> {
        let run = run.clone();
        self.write(Durability::Full, move |c| {
            tx_record_process(c, &run, fence, pid, pgid, start_time, now)
        })
        .await
    }

    /// One run, or `None`.
    pub async fn get_run(&self, run: &RunId) -> Result<Option<RunRecord>> {
        let run = run.clone();
        self.read(move |c| load(c, &run)).await
    }

    /// Runs oldest first, optionally only those in `status`.
    pub async fn list_runs(&self, status: Option<RunStatus>) -> Result<Vec<RunRecord>> {
        self.read(move |c| match status {
            Some(s) => query_runs(
                c,
                &format!(
                    "SELECT {RUN_COLS} FROM runs WHERE status = ?1 ORDER BY created_at, rowid"
                ),
                &[&status_str(s)],
            ),
            None => query_runs(
                c,
                &format!("SELECT {RUN_COLS} FROM runs ORDER BY created_at, rowid"),
                &[],
            ),
        })
        .await
    }

    /// One page of runs, **newest first** (`created_at`, then rowid, both descending), at most
    /// `limit` rows. `status` keeps only runs in one of those states (all runs when empty).
    /// `before` is a cursor: only runs older than that run, so a page never repeats or skips a
    /// run when newer ones are inserted meanwhile. An unknown cursor is [`StoreError::NotFound`].
    pub async fn list_runs_page(
        &self,
        status: &[RunStatus],
        before: Option<&RunId>,
        limit: u32,
    ) -> Result<Vec<RunRecord>> {
        let status: Vec<&'static str> = status.iter().map(|s| status_str(*s)).collect();
        let before = before.cloned();
        self.read(move |c| {
            let mut sql = format!("SELECT {RUN_COLS} FROM runs WHERE 1 = 1");
            let mut args: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
            if !status.is_empty() {
                let marks = vec!["?"; status.len()].join(", ");
                sql.push_str(&format!(" AND status IN ({marks})"));
                for s in &status {
                    args.push(Box::new(*s));
                }
            }
            if let Some(cursor) = &before {
                let at: Option<(i64, i64)> = c
                    .query_row(
                        "SELECT created_at, rowid FROM runs WHERE id = ?1",
                        [cursor.as_str()],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()
                    .db()?;
                let (created, rowid) = at.ok_or_else(|| StoreError::NotFound {
                    kind: "run",
                    id: cursor.to_string(),
                })?;
                sql.push_str(" AND (created_at, rowid) < (?, ?)");
                args.push(Box::new(created));
                args.push(Box::new(rowid));
            }
            sql.push_str(" ORDER BY created_at DESC, rowid DESC LIMIT ?");
            args.push(Box::new(i64::from(limit)));
            let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|a| &**a).collect();
            query_runs(c, &sql, &refs)
        })
        .await
    }

    /// How many runs are in one of `status` (an index scan, no rows loaded).
    pub async fn count_runs(&self, status: &[RunStatus]) -> Result<u64> {
        let status: Vec<&'static str> = status.iter().map(|s| status_str(*s)).collect();
        self.read(move |c| {
            let marks = vec!["?"; status.len()].join(", ");
            let n: i64 = c
                .query_row(
                    &format!("SELECT count(*) FROM runs WHERE status IN ({marks})"),
                    rusqlite::params_from_iter(status.iter()),
                    |r| r.get(0),
                )
                .db()?;
            Ok(u64::try_from(n).unwrap_or(0))
        })
        .await
    }

    /// Every run that is not terminal (including `interrupted`), oldest first. Recovery reads
    /// this at boot.
    pub async fn nonterminal_runs(&self) -> Result<Vec<RunRecord>> {
        self.read(|c| {
            query_runs(
                c,
                &format!(
                    "SELECT {RUN_COLS} FROM runs
                     WHERE status NOT IN ('succeeded', 'failed', 'cancelled')
                     ORDER BY created_at, rowid"
                ),
                &[],
            )
        })
        .await
    }
}
