//! Schema migrations (`user_version` based, via `rusqlite_migration`).
//!
//! Migration 1 creates the Phase 0 tables of ARCHITECTURE section 9; migration 2 adds the
//! process start time to `runs`; migration 5 adds the durable approval records; migration 6 adds
//! the scheduler's trigger state and firings; migration 7 adds the workspaces of write runs; migration 8 adds the latest executor health; migration 9 adds the routing decision of each run; migration 10 adds token accounting and the pauses of the kill switch; migration 11 adds why a run stopped and whether it was resumed. Status columns carry
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
CREATE INDEX approvals_denied ON approvals (run_id, subject_digest)
    WHERE status IN ('denied', 'expired');
CREATE INDEX approvals_decided ON approvals (decided_at) WHERE status <> 'pending';
CREATE INDEX approvals_run ON approvals (run_id, created_at);

CREATE TRIGGER approvals_decided_are_final BEFORE UPDATE ON approvals
WHEN OLD.status <> 'pending'
BEGIN SELECT RAISE(ABORT, 'a decided approval is final'); END;

CREATE TRIGGER approvals_are_kept BEFORE DELETE ON approvals
BEGIN SELECT RAISE(ABORT, 'an approval record is never deleted'); END;
";

/// Migration 6 (T1.8, triggers): the scheduler's state and the record of what each occurrence
/// became.
///
/// `triggers` holds one row per trigger in force: the definition version it was planned against,
/// the `when` it was planned for (a changed `when` is a new plan; an unchanged one keeps its
/// due time across a restart), when it is next due, when it last fired, the run it last started
/// and its backoff. A row is replaced on a new plan and deleted when the trigger leaves the
/// definitions.
///
/// `firings` has one row per occurrence that was acted on, under a unique `nonce`
/// (`<kind>:<trigger>:<scheduled_at>`), so firing the same occurrence twice is refused by the
/// database, not by a check that could race. A refused duplicate leaves no row. `run_id` is the
/// run the occurrence started, or for `coalesced` the run it was merged into. A row is never
/// deleted, and its only change is `fired` to `backed_off`, when the run it started could not
/// start for an account-level reason.
const V6: &str = "
CREATE TABLE triggers (
    name          TEXT PRIMARY KEY,
    def_version   INTEGER NOT NULL CHECK (def_version >= 1),
    schedule      TEXT NOT NULL CHECK (json_valid(schedule)),
    enabled       INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    next_due_at   INTEGER,
    last_fired_at INTEGER,
    last_run_id   TEXT REFERENCES runs (id),
    failures      INTEGER NOT NULL DEFAULT 0 CHECK (failures >= 0),
    backoff_until INTEGER,
    CHECK ((failures = 0) = (backoff_until IS NULL))
) WITHOUT ROWID;

CREATE TABLE firings (
    id           INTEGER PRIMARY KEY AUTOINCREMENT,
    nonce        TEXT NOT NULL UNIQUE,
    trigger_name TEXT NOT NULL,
    scheduled_at INTEGER NOT NULL,
    fired_at     INTEGER NOT NULL,
    run_id       TEXT NOT NULL REFERENCES runs (id),
    outcome      TEXT NOT NULL CHECK (outcome IN ('fired', 'coalesced', 'backed_off')),
    missed       INTEGER NOT NULL DEFAULT 1 CHECK (missed >= 1)
);
CREATE INDEX firings_trigger ON firings (trigger_name, id);
CREATE INDEX firings_run ON firings (run_id);

CREATE TRIGGER firings_only_back_off BEFORE UPDATE ON firings
WHEN NOT (OLD.outcome = 'fired' AND NEW.outcome = 'backed_off'
    AND NEW.id = OLD.id AND NEW.nonce = OLD.nonce AND NEW.trigger_name = OLD.trigger_name
    AND NEW.scheduled_at = OLD.scheduled_at AND NEW.fired_at = OLD.fired_at
    AND NEW.run_id = OLD.run_id AND NEW.missed = OLD.missed)
BEGIN SELECT RAISE(ABORT, 'a firing changes only from fired to backed_off'); END;

CREATE TRIGGER firings_are_kept BEFORE DELETE ON firings
BEGIN SELECT RAISE(ABORT, 'a firing record is never deleted'); END;
";

/// Migration 7 (T1.3, workspaces): one row per run that works in a Reno-managed git worktree: the
/// repository, the worktree's path, the branch `reno/<run id>`, the commit it was cut from, and
/// where it stands. The row is written (`preparing`) before the worktree is made, so a crash
/// leaves a handle to clean up by; `commits` and `dirty` are what the worktree held when its run
/// last ended, and are both set or both NULL. A path is used by one live workspace at a time.
const V7: &str = "
CREATE TABLE workspaces (
    run_id     TEXT PRIMARY KEY REFERENCES runs (id),
    repo       TEXT NOT NULL,
    repo_key   TEXT NOT NULL,
    path       TEXT NOT NULL,
    subdir     TEXT NOT NULL DEFAULT '',
    branch     TEXT NOT NULL,
    base_sha   TEXT NOT NULL,
    status     TEXT NOT NULL CHECK (status IN ('preparing', 'active', 'kept', 'removed', 'lost')),
    commits    INTEGER CHECK (commits IS NULL OR commits >= 0),
    dirty      INTEGER CHECK (dirty IS NULL OR dirty IN (0, 1)),
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK ((commits IS NULL) = (dirty IS NULL))
);
CREATE INDEX workspaces_open ON workspaces (status) WHERE status IN ('preparing', 'active', 'kept');
CREATE UNIQUE INDEX workspaces_live_path ON workspaces (path)
    WHERE status IN ('preparing', 'active', 'kept');
";

/// Migration 8 (T1.1, executor health): the latest probe of each executor, as the wire type's
/// JSON, so a restarted daemon shows what it last found before it probes again. One row per
/// executor, replaced by each probe; the history is in the event log.
const V8: &str = "
CREATE TABLE executor_health (
    executor   TEXT PRIMARY KEY,
    health     TEXT NOT NULL CHECK (json_valid(health)),
    checked_at INTEGER NOT NULL
) WITHOUT ROWID;
";

/// Migration 9 (T1.5, routing): the routing decision of each run as the wire type's JSON: the
/// executor the router chose, the ones it passed over and why, and the fallback steps. One row per
/// run, written with the run and replaced when a fallback moves it to another executor.
const V9: &str = "
CREATE TABLE run_routing (
    run_id     TEXT PRIMARY KEY REFERENCES runs (id),
    decision   TEXT NOT NULL CHECK (json_valid(decision)),
    updated_at INTEGER NOT NULL
) WITHOUT ROWID;
";

/// Migration 10 (T1.7, budgets): token accounting and the kill switch.
///
/// `bot_usage` holds what each bot's runs used on each local day (`day` is `YYYY-MM-DD`, a label
/// the kernel computes in the machine's zone); a new day is a new row, never a reset. `run_usage`
/// holds each run's own total. `pauses` holds one row per paused scope: `*` for everything,
/// otherwise a bot's name; it is what makes a pause outlive the daemon. `runs_bot_slots` serves
/// the per-bot concurrency count of the claim.
const V10: &str = "
CREATE TABLE bot_usage (
    bot           TEXT NOT NULL,
    day           TEXT NOT NULL,
    input_tokens  INTEGER NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens INTEGER NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    PRIMARY KEY (bot, day)
) WITHOUT ROWID;

CREATE TABLE run_usage (
    run_id        TEXT PRIMARY KEY REFERENCES runs (id),
    input_tokens  INTEGER NOT NULL DEFAULT 0 CHECK (input_tokens >= 0),
    output_tokens INTEGER NOT NULL DEFAULT 0 CHECK (output_tokens >= 0),
    updated_at    INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TABLE pauses (
    scope     TEXT PRIMARY KEY,
    paused_at INTEGER NOT NULL
) WITHOUT ROWID;

CREATE INDEX runs_bot_slots ON runs (bot_name, status) WHERE bot_name IS NOT NULL;
";

/// Migration 11 (T1.10, resume): why a run last stopped, why it was not resumed, and the prompt a
/// person gave to resume it.
///
/// `stop` is the typed `StopDetail` of the run's last end and `not_resumed` the typed `NotResumed`
/// recovery or the end of the run recorded for an interrupted run it left alone; both are cleared
/// when the run is queued again. `session_ref` already existed (migration 1) and is now written.
/// `resume_prompt` holds a manual resume's prompt until the next attempt ends.
const V11: &str = "
ALTER TABLE runs ADD COLUMN stop TEXT CHECK (stop IS NULL OR json_valid(stop));
ALTER TABLE runs ADD COLUMN not_resumed TEXT CHECK (not_resumed IS NULL OR json_valid(not_resumed));
ALTER TABLE runs ADD COLUMN resume_prompt TEXT;
";

/// The ordered migration set. Append new migrations; never edit an old one.
pub(crate) fn migrations() -> Migrations<'static> {
    Migrations::new(vec![
        M::up(V1),
        M::up(V2),
        M::up(V3),
        M::up(V4),
        M::up(V5),
        M::up(V6),
        M::up(V7),
        M::up(V8),
        M::up(V9),
        M::up(V10),
        M::up(V11),
    ])
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
        assert_eq!(version, 11);
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

    /// Upgrading a database written before triggers were scheduled (schema 5): its runs keep their
    /// rows, and the new tables start empty and work.
    #[test]
    fn upgrade_from_a_version_five_database_adds_empty_trigger_tables() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations().to_version(&mut conn, 5).unwrap();
        conn.execute_batch(
            "INSERT INTO tasks (id, goal, created_at) VALUES ('task_a', 'g', 1);
             INSERT INTO runs (id, task_id, executor, status, created_at)
             VALUES ('run_a', 'task_a', 'mock', 'running', 2);",
        )
        .unwrap();
        assert!(
            conn.prepare("SELECT 1 FROM firings").is_err(),
            "schema 5 has no firings table"
        );
        run(&mut conn).unwrap();
        let (runs, triggers, firings): (i64, i64, i64) = conn
            .query_row(
                "SELECT (SELECT count(*) FROM runs), (SELECT count(*) FROM triggers),
                        (SELECT count(*) FROM firings)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((runs, triggers, firings), (1, 0, 0));
    }

    /// Upgrading a database written before workspaces (schema 6): its runs keep their rows and
    /// the new table starts empty and works.
    #[test]
    fn upgrade_from_a_version_six_database_adds_an_empty_workspaces_table() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations().to_version(&mut conn, 6).unwrap();
        conn.execute_batch(
            "INSERT INTO tasks (id, goal, created_at) VALUES ('task_a', 'g', 1);
             INSERT INTO runs (id, task_id, executor, status, created_at)
             VALUES ('run_a', 'task_a', 'mock', 'running', 2);",
        )
        .unwrap();
        assert!(conn.prepare("SELECT 1 FROM workspaces").is_err());
        run(&mut conn).unwrap();
        let (runs, workspaces): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT count(*) FROM runs), (SELECT count(*) FROM workspaces)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((runs, workspaces), (1, 0));
    }

    /// Upgrading a database written before executor health was stored (schema 7): its runs keep
    /// their rows and the new table starts empty and works.
    #[test]
    fn upgrade_from_a_version_seven_database_adds_an_empty_executor_health_table() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations().to_version(&mut conn, 7).unwrap();
        conn.execute_batch(
            "INSERT INTO tasks (id, goal, created_at) VALUES ('task_a', 'g', 1);
             INSERT INTO runs (id, task_id, executor, status, created_at)
             VALUES ('run_a', 'task_a', 'mock', 'running', 2);",
        )
        .unwrap();
        assert!(conn.prepare("SELECT 1 FROM executor_health").is_err());
        run(&mut conn).unwrap();
        let (runs, health): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT count(*) FROM runs), (SELECT count(*) FROM executor_health)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((runs, health), (1, 0));
    }

    /// Upgrading a database written before budgets (schema 9): its runs and triggers keep their
    /// rows, the new tables start empty and work, and a bot-pinned run is counted by the index.
    #[test]
    fn upgrade_from_a_version_nine_database_adds_empty_budget_tables() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations().to_version(&mut conn, 9).unwrap();
        conn.execute_batch(
            "INSERT INTO tasks (id, goal, created_at) VALUES ('task_a', 'g', 1);
             INSERT INTO runs (id, task_id, executor, status, created_at)
             VALUES ('run_a', 'task_a', 'mock', 'running', 2);",
        )
        .unwrap();
        assert!(conn.prepare("SELECT 1 FROM bot_usage").is_err());
        run(&mut conn).unwrap();
        let (runs, usage, runs_usage, pauses): (i64, i64, i64, i64) = conn
            .query_row(
                "SELECT (SELECT count(*) FROM runs), (SELECT count(*) FROM bot_usage),
                        (SELECT count(*) FROM run_usage), (SELECT count(*) FROM pauses)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!((runs, usage, runs_usage, pauses), (1, 0, 0, 0));
        conn.execute_batch(
            "INSERT INTO run_usage (run_id, input_tokens, output_tokens, updated_at)
             VALUES ('run_a', 1, 2, 3);
             INSERT INTO pauses (scope, paused_at) VALUES ('*', 4);",
        )
        .unwrap();
    }

    /// A database written before resume keeps its runs and gains the empty reason columns; the
    /// `session_ref` column of migration 1 is untouched and writable.
    #[test]
    fn upgrade_from_a_version_ten_database_adds_empty_stop_columns() {
        let mut conn = Connection::open_in_memory().unwrap();
        migrations().to_version(&mut conn, 10).unwrap();
        conn.execute_batch(
            "INSERT INTO tasks (id, goal, created_at) VALUES ('task_a', 'g', 1);
             INSERT INTO runs (id, task_id, executor, status, session_ref, created_at)
             VALUES ('run_a', 'task_a', 'mock', 'interrupted', 'sess-1', 2);",
        )
        .unwrap();
        assert!(conn.prepare("SELECT stop FROM runs").is_err());
        run(&mut conn).unwrap();
        let (session, stop, not_resumed, prompt): (
            Option<String>,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "SELECT session_ref, stop, not_resumed, resume_prompt FROM runs WHERE id = 'run_a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(session.as_deref(), Some("sess-1"));
        assert_eq!((stop, not_resumed, prompt), (None, None, None));
        conn.execute_batch(
            "UPDATE runs SET stop = '{\"type\":\"crashed\"}', not_resumed = '{\"type\":\"no_session\"}'",
        )
        .unwrap();
        assert!(
            conn.execute("UPDATE runs SET stop = 'not json'", [])
                .is_err()
        );
    }
}
