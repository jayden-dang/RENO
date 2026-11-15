//! Schema migrations (`user_version` based, via `rusqlite_migration`).
//!
//! Migration 1 creates the Phase 0 tables of ARCHITECTURE section 9. Status columns carry
//! `CHECK` constraints that [`RunStatus`](reno_proto::RunStatus) round-trips through (a test
//! inserts every variant). JSON columns are checked with `json_valid`.

use rusqlite::Connection;
use rusqlite_migration::{M, Migrations};

use crate::error::StoreError;

const V1: &str = "
CREATE TABLE events (
    seq          INTEGER PRIMARY KEY AUTOINCREMENT,
    ts           INTEGER NOT NULL,
    kind         TEXT NOT NULL,
    subject_type TEXT NOT NULL,
    subject_id   TEXT NOT NULL,
    principal    TEXT,
    data         TEXT NOT NULL CHECK (json_valid(data))
);
CREATE INDEX events_subject ON events (subject_type, subject_id, seq);

CREATE TABLE definitions (
    kind       TEXT NOT NULL,
    name       TEXT NOT NULL,
    version    INTEGER NOT NULL,
    hash       TEXT NOT NULL,
    body       TEXT NOT NULL CHECK (json_valid(body)),
    status     TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (kind, name, version)
) WITHOUT ROWID;

CREATE TABLE tasks (
    id          TEXT PRIMARY KEY,
    flow_run_id TEXT,
    node_id     TEXT,
    bot_id      TEXT,
    class       TEXT,
    goal        TEXT NOT NULL,
    spec        TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(spec)),
    status      TEXT NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'done', 'cancelled')),
    revision    INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL
);

CREATE TABLE runs (
    id               TEXT PRIMARY KEY,
    task_id          TEXT NOT NULL REFERENCES tasks (id),
    executor         TEXT NOT NULL,
    model            TEXT,
    status           TEXT NOT NULL DEFAULT 'queued' CHECK (status IN (
        'queued', 'leased', 'running', 'waiting_input', 'waiting_approval',
        'evaluating', 'succeeded', 'failed', 'interrupted', 'cancelled')),
    lease_owner      TEXT,
    lease_fence      INTEGER NOT NULL DEFAULT 0 CHECK (lease_fence >= 0),
    lease_expires_at INTEGER,
    session_ref      TEXT,
    wake_at          INTEGER,
    routing          TEXT CHECK (routing IS NULL OR json_valid(routing)),
    pid              INTEGER,
    pgid             INTEGER,
    created_at       INTEGER NOT NULL,
    started_at       INTEGER,
    ended_at         INTEGER
);
CREATE INDEX runs_task ON runs (task_id);
CREATE INDEX runs_status ON runs (status);
CREATE INDEX runs_claimable ON runs (created_at) WHERE status IN ('queued', 'leased');

CREATE TABLE attempts (
    run_id      TEXT NOT NULL REFERENCES runs (id),
    n           INTEGER NOT NULL,
    fence       INTEGER NOT NULL,
    status      TEXT NOT NULL CHECK (status IN (
        'queued', 'leased', 'running', 'waiting_input', 'waiting_approval',
        'evaluating', 'succeeded', 'failed', 'interrupted', 'cancelled')),
    stop_reason TEXT,
    diagnostic  TEXT,
    PRIMARY KEY (run_id, n)
) WITHOUT ROWID;
";

/// The ordered migration set. Append new migrations; never edit an old one.
pub(crate) fn migrations() -> Migrations<'static> {
    Migrations::new(vec![M::up(V1)])
}

/// Brings `conn` to the latest schema. A second call is a no-op.
pub(crate) fn run(conn: &mut Connection) -> Result<(), StoreError> {
    migrations()
        .to_latest(conn)
        .map_err(|e| StoreError::Migration(e.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_validate() {
        migrations().validate().unwrap();
    }

    fn schema(conn: &Connection) -> String {
        let mut stmt = conn
            .prepare("SELECT sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY name")
            .unwrap();
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        rows.join(";\n\n")
    }

    #[test]
    fn schema_snapshot() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn).unwrap();
        insta::assert_snapshot!(schema(&conn));
    }

    #[test]
    fn second_run_is_a_no_op() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn).unwrap();
        let before = schema(&conn);
        run(&mut conn).unwrap();
        assert_eq!(schema(&conn), before);
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 1);
    }
}
