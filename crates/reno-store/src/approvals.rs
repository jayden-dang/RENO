//! Approval records: one row per approval request a run raised, kept for life
//! (ARCHITECTURE 8 point 4 and 17.4).
//!
//! # Life of a row
//!
//! A request is created `pending` ([`Store::create_approval`]) and decided exactly once
//! ([`Store::resolve_approval`]). A request that policy answers with no question, and one denied
//! as a repeat, is inserted already decided ([`Store::record_approval`]). After that the row
//! never changes (a trigger enforces it) and is never deleted.
//!
//! # Durability and events
//!
//! Every decision is a [`Durability::Full`] write, so it survives power loss before its caller
//! is told it was made. Each write also inserts its `events` row in the same transaction
//! (ARCH-8): `approval.requested` for a new pending row and `approval.decided` for a decision.
//! The event's principal is who decided (`human`, `rule:<name>`, `expired`, `repeat_denial`).
//! Its payload carries the subject's digest and never the subject, which may hold secrets.
//!
//! # Fences
//!
//! Every write that a run's supervisor makes carries the run's fence and is refused when it is
//! stale (ARCH-9). Only the kernel's start-up recovery, which owns no run, writes without one.

use reno_proto::{ApprovalId, ApprovalOutcome, ApprovalStatus, ApprovalSubject, ResolvedBy, RunId};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::json;

use crate::db::Store;
use crate::error::{DbResult, Result, StoreError, is_unique_violation};
use crate::events;
use crate::writer::Durability;

/// An approval request to store.
#[derive(Debug, Clone, PartialEq)]
pub struct NewApproval {
    /// Its id.
    pub id: ApprovalId,
    /// The run that raised it.
    pub run_id: RunId,
    /// The harness's id for the request.
    pub request_id: String,
    /// What is being asked. Its [digest](ApprovalSubject::digest) is stored beside it.
    pub subject: ApprovalSubject,
    /// When an undecided request expires, in milliseconds since the epoch.
    pub expires_at: i64,
}

/// A stored approval request.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredApproval {
    /// Its id.
    pub id: ApprovalId,
    /// The run that raised it.
    pub run_id: RunId,
    /// The harness's id for the request.
    pub request_id: String,
    /// What was asked.
    pub subject: ApprovalSubject,
    /// The subject's digest.
    pub subject_digest: String,
    /// Where it stands.
    pub status: ApprovalStatus,
    /// Who or what decided; `None` while pending.
    pub decided_by: Option<ResolvedBy>,
    /// When it was raised, in milliseconds since the epoch.
    pub created_at: i64,
    /// When it was decided; `None` while pending.
    pub decided_at: Option<i64>,
    /// When an undecided request expires.
    pub expires_at: i64,
}

/// A pending request that was expired, so the caller can tell the run and its journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expired {
    /// The approval.
    pub id: ApprovalId,
    /// Its run.
    pub run_id: RunId,
    /// The harness's id for the request.
    pub request_id: String,
}

/// Which pending requests [`Store::expire_pending`] closes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExpireScope {
    /// Every pending request. For start-up recovery, when no run is live.
    All,
    /// The pending requests of one run, under that run's fence. For the end of a run.
    Run {
        /// The run.
        run: RunId,
        /// Its current fence.
        fence: u64,
    },
}

/// The status a decision leaves behind: an expiry is `expired` whatever it told the harness.
fn status_of(outcome: ApprovalOutcome, by: &ResolvedBy) -> ApprovalStatus {
    match (outcome, by) {
        (_, ResolvedBy::Expired) => ApprovalStatus::Expired,
        (ApprovalOutcome::Approved, _) => ApprovalStatus::Approved,
        (ApprovalOutcome::Denied, _) => ApprovalStatus::Denied,
    }
}

fn status_str(status: ApprovalStatus) -> &'static str {
    match status {
        ApprovalStatus::Pending => "pending",
        ApprovalStatus::Approved => "approved",
        ApprovalStatus::Denied => "denied",
        ApprovalStatus::Expired => "expired",
    }
}

fn parse_status(text: &str) -> Result<ApprovalStatus> {
    [
        ApprovalStatus::Pending,
        ApprovalStatus::Approved,
        ApprovalStatus::Denied,
        ApprovalStatus::Expired,
    ]
    .into_iter()
    .find(|status| status_str(*status) == text)
    .ok_or_else(|| StoreError::Corrupt(format!("unknown approval status {text:?}")))
}

/// The `decided_by` column and the `rule` column for `by`.
fn columns_of(by: &ResolvedBy) -> (&'static str, Option<&str>) {
    match by {
        ResolvedBy::Human => ("human", None),
        ResolvedBy::Rule { name } => ("rule", Some(name.as_str())),
        ResolvedBy::Expired => ("expired", None),
        ResolvedBy::RepeatDenial => ("repeat_denial", None),
    }
}

fn by_from_columns(decided_by: Option<&str>, rule: Option<String>) -> Result<Option<ResolvedBy>> {
    Ok(match (decided_by, rule) {
        (None, _) => None,
        (Some("human"), _) => Some(ResolvedBy::Human),
        (Some("rule"), Some(name)) => Some(ResolvedBy::Rule { name }),
        (Some("expired"), _) => Some(ResolvedBy::Expired),
        (Some("repeat_denial"), _) => Some(ResolvedBy::RepeatDenial),
        (Some(other), _) => {
            return Err(StoreError::Corrupt(format!(
                "unknown approval decider {other:?}"
            )));
        }
    })
}

/// The principal an event is attributed to.
fn principal_of(by: &ResolvedBy) -> String {
    match by {
        ResolvedBy::Human => "human".to_owned(),
        ResolvedBy::Rule { name } => format!("rule:{name}"),
        ResolvedBy::Expired => "expired".to_owned(),
        ResolvedBy::RepeatDenial => "repeat_denial".to_owned(),
    }
}

const COLUMNS: &str = "id, run_id, request_id, subject, subject_digest, status, decided_by, rule, \
     created_at, decided_at, expires_at";

fn from_row(row: &Row<'_>) -> rusqlite::Result<Result<StoredApproval>> {
    let id: String = row.get(0)?;
    let run_id: String = row.get(1)?;
    let request_id = row.get(2)?;
    let subject: String = row.get(3)?;
    let subject_digest = row.get(4)?;
    let status: String = row.get(5)?;
    let decided_by: Option<String> = row.get(6)?;
    let rule: Option<String> = row.get(7)?;
    let created_at = row.get(8)?;
    let decided_at = row.get(9)?;
    let expires_at = row.get(10)?;
    Ok((|| {
        Ok(StoredApproval {
            id: id
                .parse()
                .map_err(|_| StoreError::Corrupt(format!("bad approval id {id:?}")))?,
            run_id: run_id
                .parse()
                .map_err(|_| StoreError::Corrupt(format!("bad run id {run_id:?}")))?,
            request_id,
            subject: serde_json::from_str(&subject)
                .map_err(|e| StoreError::Corrupt(format!("approval subject: {e}")))?,
            subject_digest,
            status: parse_status(&status)?,
            decided_by: by_from_columns(decided_by.as_deref(), rule)?,
            created_at,
            decided_at,
            expires_at,
        })
    })())
}

fn query(
    conn: &Connection,
    sql: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<StoredApproval>> {
    let mut stmt = conn.prepare_cached(sql).db()?;
    let rows = stmt.query_map(args, from_row).db()?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.db()??);
    }
    Ok(out)
}

/// Checks that `fence` is the run's current one (ARCH-9).
fn check_fence(conn: &Connection, run: &RunId, fence: u64) -> Result<()> {
    let current: Option<i64> = conn
        .prepare_cached("SELECT lease_fence FROM runs WHERE id = ?1")
        .db()?
        .query_row([run.as_str()], |r| r.get(0))
        .optional()
        .db()?;
    let current = current.ok_or_else(|| StoreError::NotFound {
        kind: "run",
        id: run.to_string(),
    })?;
    let current = u64::try_from(current)
        .map_err(|_| StoreError::Corrupt(format!("negative fence on {run}")))?;
    if current == fence {
        Ok(())
    } else {
        Err(StoreError::StaleFence {
            run: run.clone(),
            supplied: fence,
            current,
        })
    }
}

fn insert(conn: &Connection, new: &NewApproval, digest: &str, now: i64) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO approvals
             (id, run_id, request_id, subject, subject_digest, status, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?7)",
    )
    .db()?
    .execute(params![
        new.id.as_str(),
        new.run_id.as_str(),
        new.request_id,
        serde_json::to_string(&new.subject)?,
        digest,
        now,
        new.expires_at,
    ])
    .map_err(|e| {
        if is_unique_violation(&e) {
            StoreError::AlreadyExists {
                kind: "approval",
                id: new.id.to_string(),
            }
        } else {
            crate::error::db(e)
        }
    })?;
    Ok(())
}

/// Decides a pending row. Fails with `NotFound` when it is not pending (decided already, or not
/// there): a decision is made once.
fn decide(
    conn: &Connection,
    id: &ApprovalId,
    status: ApprovalStatus,
    by: &ResolvedBy,
    now: i64,
) -> Result<()> {
    let (decided_by, rule) = columns_of(by);
    let changed = conn
        .prepare_cached(
            "UPDATE approvals SET status = ?2, decided_by = ?3, rule = ?4, decided_at = ?5
             WHERE id = ?1 AND status = 'pending'",
        )
        .db()?
        .execute(params![
            id.as_str(),
            status_str(status),
            decided_by,
            rule,
            now
        ])
        .db()?;
    if changed == 1 {
        Ok(())
    } else {
        Err(StoreError::NotFound {
            kind: "pending approval",
            id: id.to_string(),
        })
    }
}

/// What an `approval.decided` event says about the request it decides.
struct Decided<'a> {
    id: &'a ApprovalId,
    run: &'a RunId,
    request_id: &'a str,
    digest: &'a str,
}

fn decided_event(
    conn: &Connection,
    now: i64,
    of: &Decided<'_>,
    status: ApprovalStatus,
    by: &ResolvedBy,
) -> Result<()> {
    events::insert(
        conn,
        now,
        "approval.decided",
        ("approval", of.id.as_str()),
        Some(&principal_of(by)),
        &json!({
            "run": of.run,
            "requestId": of.request_id,
            "digest": of.digest,
            "status": status,
            "decidedBy": by,
        }),
    )
}

fn tx_create(conn: &Connection, new: &NewApproval, fence: u64, now: i64) -> Result<()> {
    check_fence(conn, &new.run_id, fence)?;
    let digest = new.subject.digest();
    insert(conn, new, &digest, now)?;
    events::insert(
        conn,
        now,
        "approval.requested",
        ("approval", new.id.as_str()),
        None,
        &json!({
            "run": new.run_id,
            "requestId": new.request_id,
            "digest": digest,
            "expiresAt": new.expires_at,
        }),
    )
}

fn tx_record(
    conn: &Connection,
    new: &NewApproval,
    outcome: ApprovalOutcome,
    by: &ResolvedBy,
    fence: u64,
    now: i64,
) -> Result<()> {
    check_fence(conn, &new.run_id, fence)?;
    let digest = new.subject.digest();
    insert(conn, new, &digest, now)?;
    let status = status_of(outcome, by);
    decide(conn, &new.id, status, by, now)?;
    decided_event(
        conn,
        now,
        &Decided {
            id: &new.id,
            run: &new.run_id,
            request_id: &new.request_id,
            digest: &digest,
        },
        status,
        by,
    )
}

fn tx_resolve(
    conn: &Connection,
    id: &ApprovalId,
    outcome: ApprovalOutcome,
    by: &ResolvedBy,
    fence: u64,
    now: i64,
) -> Result<()> {
    let row = query(
        conn,
        &format!("SELECT {COLUMNS} FROM approvals WHERE id = ?1"),
        &[&id.as_str()],
    )?
    .into_iter()
    .next()
    .ok_or_else(|| StoreError::NotFound {
        kind: "approval",
        id: id.to_string(),
    })?;
    check_fence(conn, &row.run_id, fence)?;
    let status = status_of(outcome, by);
    decide(conn, id, status, by, now)?;
    decided_event(
        conn,
        now,
        &Decided {
            id,
            run: &row.run_id,
            request_id: &row.request_id,
            digest: &row.subject_digest,
        },
        status,
        by,
    )
}

fn tx_expire(conn: &Connection, scope: &ExpireScope, now: i64) -> Result<Vec<Expired>> {
    let pending = match scope {
        ExpireScope::All => query(
            conn,
            &format!(
                "SELECT {COLUMNS} FROM approvals WHERE status = 'pending' ORDER BY created_at, rowid"
            ),
            &[],
        )?,
        ExpireScope::Run { run, fence } => {
            check_fence(conn, run, *fence)?;
            query(
                conn,
                &format!(
                    "SELECT {COLUMNS} FROM approvals WHERE status = 'pending' AND run_id = ?1
                     ORDER BY created_at, rowid"
                ),
                &[&run.as_str()],
            )?
        }
    };
    let by = ResolvedBy::Expired;
    let mut expired = Vec::with_capacity(pending.len());
    for row in pending {
        decide(conn, &row.id, ApprovalStatus::Expired, &by, now)?;
        decided_event(
            conn,
            now,
            &Decided {
                id: &row.id,
                run: &row.run_id,
                request_id: &row.request_id,
                digest: &row.subject_digest,
            },
            ApprovalStatus::Expired,
            &by,
        )?;
        expired.push(Expired {
            id: row.id,
            run_id: row.run_id,
            request_id: row.request_id,
        });
    }
    Ok(expired)
}

impl Store {
    /// Stores a new `pending` request raised by `new.run_id`, which holds `fence`, and its
    /// `approval.requested` event. Fails with `AlreadyExists` for a used id and `StaleFence` for
    /// a run that someone else now owns.
    pub async fn create_approval(&self, new: NewApproval, fence: u64, now: i64) -> Result<()> {
        self.write(Durability::Normal, move |c| tx_create(c, &new, fence, now))
            .await
    }

    /// Stores a request that is decided at once (by a policy rule, or as a repeat of a denied
    /// one) as `outcome` by `by`, with its `approval.decided` event. A [`Durability::Full`]
    /// write.
    pub async fn record_approval(
        &self,
        new: NewApproval,
        outcome: ApprovalOutcome,
        by: ResolvedBy,
        fence: u64,
        now: i64,
    ) -> Result<()> {
        self.write(Durability::Full, move |c| {
            tx_record(c, &new, outcome, &by, fence, now)
        })
        .await
    }

    /// Decides a pending request as `outcome` by `by`, with its `approval.decided` event. The
    /// status becomes `expired` when `by` is [`ResolvedBy::Expired`], else `approved` or
    /// `denied`. Fails with `NotFound` when the request is not pending, so a decision is made
    /// once. A [`Durability::Full`] write.
    pub async fn resolve_approval(
        &self,
        id: &ApprovalId,
        outcome: ApprovalOutcome,
        by: ResolvedBy,
        fence: u64,
        now: i64,
    ) -> Result<()> {
        let id = id.clone();
        self.write(Durability::Full, move |c| {
            tx_resolve(c, &id, outcome, &by, fence, now)
        })
        .await
    }

    /// Expires pending requests (`expired`, decided by [`ResolvedBy::Expired`]) and returns
    /// them, each with its `approval.decided` event, in one [`Durability::Full`] transaction.
    pub async fn expire_pending(&self, scope: ExpireScope, now: i64) -> Result<Vec<Expired>> {
        self.write(Durability::Full, move |c| tx_expire(c, &scope, now))
            .await
    }

    /// The pending requests, oldest first.
    pub async fn pending_approvals(&self) -> Result<Vec<StoredApproval>> {
        self.read(|conn| {
            query(
                conn,
                &format!(
                    "SELECT {COLUMNS} FROM approvals WHERE status = 'pending'
                     ORDER BY created_at, rowid"
                ),
                &[],
            )
        })
        .await
    }

    /// One request, pending or decided.
    pub async fn get_approval(&self, id: &ApprovalId) -> Result<Option<StoredApproval>> {
        let id = id.clone();
        self.read(move |conn| {
            Ok(query(
                conn,
                &format!("SELECT {COLUMNS} FROM approvals WHERE id = ?1"),
                &[&id.as_str()],
            )?
            .into_iter()
            .next())
        })
        .await
    }

    /// Decided requests (never pending ones), newest decision first, at most `limit`; only
    /// `run`'s when given.
    pub async fn approval_history(
        &self,
        run: Option<&RunId>,
        limit: u32,
    ) -> Result<Vec<StoredApproval>> {
        let run = run.cloned();
        self.read(move |conn| {
            let run = run.as_ref().map(RunId::as_str);
            query(
                conn,
                &format!(
                    "SELECT {COLUMNS} FROM approvals
                     WHERE status <> 'pending' AND (?1 IS NULL OR run_id = ?1)
                     ORDER BY decided_at DESC, rowid DESC LIMIT ?2"
                ),
                &[&run, &i64::from(limit)],
            )
        })
        .await
    }

    /// Whether `run` has a request with this subject digest that was refused: `denied` (by a
    /// person, a rule, or as a repeat) or `expired` (no answer means deny, ARCHITECTURE 8).
    pub async fn was_denied(&self, run: &RunId, digest: &str) -> Result<bool> {
        let run = run.clone();
        let digest = digest.to_owned();
        self.read(move |conn| {
            conn.prepare_cached(
                "SELECT 1 FROM approvals
                 WHERE run_id = ?1 AND subject_digest = ?2 AND status IN ('denied', 'expired') LIMIT 1",
            )
            .db()?
            .exists(params![run.as_str(), digest])
            .db()
        })
        .await
    }
}
