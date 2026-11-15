//! The `events` table: the global, ordered record of every state change (ARCH-8).
//!
//! Rows are only written by `insert`, which every state-changing statement in this crate calls
//! inside the same transaction as the change itself.

use reno_proto::Seq;
use rusqlite::{Connection, params};

use crate::db::Store;
use crate::error::{DbResult, Result, StoreError};

/// One row of the global event log.
#[derive(Debug, Clone, PartialEq)]
pub struct EventRecord {
    /// Global sequence number, strictly increasing, assigned by the database.
    pub seq: Seq,
    /// When it happened, in milliseconds since the epoch, as passed by the caller.
    pub ts: i64,
    /// Event kind, such as `run.claimed`.
    pub kind: String,
    /// What kind of thing it is about (`task`, `run`).
    pub subject_type: String,
    /// The id of that thing.
    pub subject_id: String,
    /// Who caused it, when known.
    pub principal: Option<String>,
    /// Kind-specific payload.
    pub data: serde_json::Value,
}

/// Appends one event row on `conn`. Callers run this inside their own transaction.
pub(crate) fn insert(
    conn: &Connection,
    now: i64,
    kind: &str,
    subject: (&str, &str),
    principal: Option<&str>,
    data: &serde_json::Value,
) -> Result<()> {
    conn.prepare_cached(
        "INSERT INTO events (ts, kind, subject_type, subject_id, principal, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .db()?
    .execute(params![
        now,
        kind,
        subject.0,
        subject.1,
        principal,
        data.to_string()
    ])
    .db()?;
    Ok(())
}

fn since(conn: &Connection, after: Seq, limit: usize) -> Result<Vec<EventRecord>> {
    let after = i64::try_from(after.0)
        .map_err(|_| StoreError::InvalidArgument("seq out of range".into()))?;
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut stmt = conn
        .prepare_cached(
            "SELECT seq, ts, kind, subject_type, subject_id, principal, data
             FROM events WHERE seq > ?1 ORDER BY seq LIMIT ?2",
        )
        .db()?;
    let rows = stmt
        .query_map(params![after, limit], |r| {
            let seq: i64 = r.get(0)?;
            let data: String = r.get(6)?;
            Ok((
                seq,
                EventRecord {
                    seq: Seq(0),
                    ts: r.get(1)?,
                    kind: r.get(2)?,
                    subject_type: r.get(3)?,
                    subject_id: r.get(4)?,
                    principal: r.get(5)?,
                    data: serde_json::Value::Null,
                },
                data,
            ))
        })
        .db()?;
    let mut out = Vec::new();
    for row in rows {
        let (seq, mut rec, data) = row.db()?;
        rec.seq =
            Seq(u64::try_from(seq)
                .map_err(|_| StoreError::Corrupt(format!("negative seq {seq}")))?);
        rec.data = serde_json::from_str(&data)
            .map_err(|e| StoreError::Corrupt(format!("event {seq} data: {e}")))?;
        out.push(rec);
    }
    Ok(out)
}

impl Store {
    /// Events with `seq` greater than `after`, oldest first, at most `limit`. Pass `Seq(0)` to
    /// start from the beginning. This is how subscriptions resume (ARCHITECTURE 17.4).
    pub async fn events_since(&self, after: Seq, limit: usize) -> Result<Vec<EventRecord>> {
        self.read(move |conn| since(conn, after, limit)).await
    }
}
