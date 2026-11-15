//! Schema migrations (`user_version` based, via `rusqlite_migration`).
//!
//! Migration 1 creates the Phase 0 tables of ARCHITECTURE section 9; migration 2 adds the
//! process start time to `runs`; migration 5 adds the durable approval records. Status columns carry
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

/// Migration 2: the start time of the recorded child, so the orphan sweep can refuse a pid that
/// was reused (ARCHITECTURE 17.3).
const V2: &str = "ALTER TABLE runs ADD COLUMN process_start INTEGER;";

/// Migration 3: run listings page by `(created_at, rowid)`, newest first, so the order has an
/// index and a page reads only its own rows.
const V3: &str = "CREATE INDEX runs_created ON runs (created_at);";

/// Migration 4 (T1.4, definitions): a run submitted for a bot records the bot and the version of
/// its definition, for life. A run for no bot has both NULL; one without the other is refused.
/// Versions of a definition are immutable: only `status` may change, and rows are never deleted.
const V4: &str = "
ALTER TABLE runs ADD COLUMN bot_name TEXT;
ALTER TABLE runs ADD COLUMN bot_version INTEGER;
CREATE INDEX runs_bot ON runs (bot_name, bot_version) WHERE bot_name IS NOT NULL;

CREATE TRIGGER runs_bot_pin_is_whole_insert BEFORE INSERT ON runs
WHEN (NEW.bot_name IS NULL) <> (NEW.bot_version IS NULL)
BEGIN SELECT RAISE(ABORT, 'a run pins both a bot name and a version, or neither'); END;

CREATE TRIGGER runs_bot_pin_is_whole_update BEFORE UPDATE OF bot_name, bot_version ON runs
WHEN (NEW.bot_name IS NULL) <> (NEW.bot_version IS NULL)
  OR NEW.bot_name IS NOT OLD.bot_name OR NEW.bot_version IS NOT OLD.bot_version
BEGIN SELECT RAISE(ABORT, 'the bot pin of a run never changes'); END;

CREATE TRIGGER definitions_are_immutable BEFORE UPDATE OF kind, name, version, hash, body, created_at
ON definitions
BEGIN SELECT RAISE(ABORT, 'a stored definition version is immutable'); END;

CREATE TRIGGER definitions_are_kept BEFORE DELETE ON definitions
BEGIN SELECT RAISE(ABORT, 'a stored definition version is never deleted'); END;
";

/// Migration 5 (T1.6, approvals): one row per approval request, kept for life. A request is
/// `pending` until it is decided once; after that the row never changes, which is what lets the
/// repeat-denial rule and the history trust it. `decided_by` is `human`, `rule` (named in
/// `rule`), `expired` or `repeat_denial`; an `expired` row was refused because nobody answered
/// in time, the run ended, or the daemon restarted.
const V5: &str = "
CREATE TABLE approvals (
    id             TEXT PRIMARY KEY,
    run_id         TEXT NOT NULL REFERENCES runs (id),
    request_id     TEXT NOT NULL,
    subject        TEXT NOT NULL CHECK (json_valid(subject)),
    subject_digest TEXT NOT NULL,
    status         TEXT NOT NULL DEFAULT 'pending'
        CHECK (status IN ('pending', 'approved', 'denied', 'expired')),
    decided_by     TEXT CHECK (decided_by IS NULL
        OR decided_by IN ('human', 'rule', 'expired', 'repeat_denial')),
    rule           TEXT,
    created_at     INTEGER NOT NULL,
    decided_at     INTEGER,
    expires_at     INTEGER NOT NULL,
    CHECK ((status = 'pending') = (decided_by IS NULL)),
    CHECK ((status = 'pending') = (decided_at IS NULL)),
    CHECK (rule IS NULL OR decided_by = 'rule'),
    CHECK (decided_by IS NOT 'rule' OR rule IS NOT NULL),
    CHECK (status <> 'expired' OR decided_by = 'expired'),
    CHECK (decided_by IS NOT 'expired' OR status = 'expired')
);
CREATE INDEX approvals_pending ON approvals (created_at) WHERE status = 'pending';
CREATE INDEX approvals_denied ON approvals (run_id, subject_digest) WHERE status = 'denied';
CREATE INDEX approvals_decided ON approvals (decided_at) WHERE status <> 'pending';
CREATE INDEX approvals_run ON approvals (run_id, created_at);

CREATE TRIGGER approvals_decided_are_final BEFORE UPDATE ON approvals
WHEN OLD.status <> 'pending'
BEGIN SELECT RAISE(ABORT, 'a decided approval is final'); END;

CREATE TRIGGER approvals_are_kept BEFORE DELETE ON approvals
BEGIN SELECT RAISE(ABORT, 'an approval record is never deleted'); END;
";

/// The ordered migration set. Append new migrations; never edit an old one.
pub(crate) fn migrations() -> Migrations<'static> {
    Migrations::new(vec![M::up(V1), M::up(V2), M::up(V3), M::up(V4), M::up(V5)])
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
        assert_eq!(version, 5);
    }

    /// The upgrade path of ARCHITECTURE 17.4: a database written by the previous phase (schema
    /// 1) keeps its rows and gains the new column, empty.
    #[test]
    fn upgrade_from_a_version_one_database_keeps_its_rows() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations().to_version(&mut conn, 1).unwrap();
        conn.execute_batch(
            "INSERT INTO tasks (id, goal, created_at) VALUES ('task_a', 'g', 1);
             INSERT INTO runs (id, task_id, executor, status, pid, pgid, created_at)
             VALUES ('run_a', 'task_a', 'mock', 'running', 7, 7, 2);",
        )
        .unwrap();
        run(&mut conn).unwrap();
        let (pid, start): (i64, Option<i64>) = conn
            .query_row(
                "SELECT pid, process_start FROM runs WHERE id = 'run_a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((pid, start), (7, None));
    }
    /// Upgrading a database written before approvals were stored (schema 4): its runs keep their
    /// rows, and the new table starts empty and works.
    #[test]
    fn upgrade_from_a_version_four_database_adds_an_empty_approvals_table() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations().to_version(&mut conn, 4).unwrap();
        conn.execute_batch(
            "INSERT INTO tasks (id, goal, created_at) VALUES ('task_a', 'g', 1);
             INSERT INTO runs (id, task_id, executor, status, created_at, bot_name, bot_version)
             VALUES ('run_a', 'task_a', 'mock', 'running', 2, NULL, NULL);",
        )
        .unwrap();
        assert!(
            conn.prepare("SELECT 1 FROM approvals").is_err(),
            "schema 4 has no approvals table"
        );
        run(&mut conn).unwrap();
        let (runs, approvals): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT count(*) FROM runs), (SELECT count(*) FROM approvals)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((runs, approvals), (1, 0));
        conn.execute(
            "INSERT INTO approvals (id, run_id, request_id, subject, subject_digest, created_at, expires_at)
             VALUES ('appr_a', 'run_a', 'q', '{}', 'd', 3, 4)",
            [],
        )
        .unwrap();
    }

    /// Upgrading a database written before definitions existed (schema 3): its runs keep their
    /// rows and carry no bot pin.
    #[test]
    fn upgrade_from_a_version_three_database_adds_an_empty_pin() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations().to_version(&mut conn, 3).unwrap();
        conn.execute_batch(
            "INSERT INTO tasks (id, goal, created_at) VALUES ('task_a', 'g', 1);
             INSERT INTO runs (id, task_id, executor, status, created_at)
             VALUES ('run_a', 'task_a', 'mock', 'queued', 2);
             INSERT INTO definitions (kind, name, version, hash, body, status, created_at)
             VALUES ('bot', 'old', 1, 'h', '{}', 'active', 3);",
        )
        .unwrap();
        run(&mut conn).unwrap();
        let (name, version): (Option<String>, Option<i64>) = conn
            .query_row(
                "SELECT bot_name, bot_version FROM runs WHERE id = 'run_a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((name, version), (None, None));
        // The existing definition row is untouched, and is now immutable.
        let kept: i64 = conn
            .query_row("SELECT count(*) FROM definitions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(kept, 1);
        assert!(conn.execute("DELETE FROM definitions", []).is_err());
    }
}
