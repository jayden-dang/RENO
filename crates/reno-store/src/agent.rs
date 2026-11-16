//! Run tokens and the private boards (T1.9).
//!
//! # Tokens
//!
//! A run token is a secret the daemon hands to the `reno mcp` process of one run. Only its SHA-256
//! digest is stored (`run_tokens`), together with the run, the bot, the board scopes the token may
//! touch and an expiry. `revoked_at` is set when the run ends (and for every token still open when
//! the daemon starts, since the run that held it did not survive). A token is **valid** when a
//! row with its digest and run exists, is not revoked and has not expired. The digest is never
//! written to an event, and nothing here ever sees the token itself.
//!
//! # Boards
//!
//! `board_entries` is a small key/value table per scope. A write may carry the version the writer
//! last saw (compare-and-set): a stale write changes nothing and returns the current entry. The
//! scope is chosen by the caller (the kernel derives it from the token); this module only
//! enforces the size limits, which are the protocol's. Each write adds a `board.written` event
//! carrying the key, the new version, the size and the value's digest, never the value.
//!
//! The store never reads the clock: every call takes `now`.

use reno_proto::{
    BOARD_KEY_MAX_BYTES, BOARD_MAX_KEYS, BOARD_VALUE_MAX_BYTES, RunId, token_digest,
};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::json;

use crate::db::Store;
use crate::error::{DbResult, Result, StoreError};
use crate::events;
use crate::writer::Durability;

/// A token to record. `token_hash` is the digest of the token ([`token_digest`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRunToken {
    /// The digest of the token.
    pub token_hash: String,
    /// The run it speaks for.
    pub run_id: RunId,
    /// The run's bot.
    pub bot: Option<String>,
    /// The board scopes it may read and write; the first is the run's own.
    pub scopes: Vec<String>,
    /// When it stops being valid, in milliseconds since the epoch.
    pub expires_at: i64,
}

/// A stored token, as authentication reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunToken {
    /// The run it speaks for.
    pub run_id: RunId,
    /// The run's bot.
    pub bot: Option<String>,
    /// The board scopes it may read and write.
    pub scopes: Vec<String>,
    /// When it stops being valid.
    pub expires_at: i64,
    /// When it was revoked, if it was.
    pub revoked_at: Option<i64>,
}

/// One board entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardRow {
    /// The key.
    pub key: String,
    /// The value.
    pub value: String,
    /// Writes of the key so far, from 1.
    pub version: u64,
    /// Bytes in the value.
    pub size: u64,
    /// When it was last written.
    pub updated_at: i64,
}

/// What a board write came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoardWrite {
    /// The value is stored, at this version.
    Written {
        /// The key's version now.
        version: u64,
    },
    /// The write named a version that is not the current one; nothing changed.
    Stale {
        /// The entry as it stands, or `None` when the key does not exist.
        current: Option<BoardRow>,
    },
    /// The scope already holds [`BOARD_MAX_KEYS`] keys and this one is new; nothing changed.
    Full,
}

fn board_row(row: &Row<'_>) -> rusqlite::Result<BoardRow> {
    let version: i64 = row.get(2)?;
    let size: i64 = row.get(3)?;
    Ok(BoardRow {
        key: row.get(0)?,
        value: row.get(1)?,
        version: u64::try_from(version).unwrap_or(0),
        size: u64::try_from(size).unwrap_or(0),
        updated_at: row.get(4)?,
    })
}

const BOARD_COLS: &str = "key, value, version, size, updated_at";

fn load_entry(conn: &Connection, scope: &str, key: &str) -> Result<Option<BoardRow>> {
    conn.prepare_cached(&format!(
        "SELECT {BOARD_COLS} FROM board_entries WHERE scope = ?1 AND key = ?2"
    ))
    .db()?
    .query_row(params![scope, key], board_row)
    .optional()
    .db()
}

fn check_key(key: &str) -> Result<()> {
    if key.is_empty() || key.len() > BOARD_KEY_MAX_BYTES {
        return Err(StoreError::InvalidArgument(format!(
            "a board key is 1 to {BOARD_KEY_MAX_BYTES} bytes"
        )));
    }
    Ok(())
}

impl Store {
    /// Records a token. Writes a `run.token_issued` event (without the digest).
    ///
    /// Errors: [`StoreError::NotFound`] when the run does not exist.
    pub async fn put_run_token(&self, token: NewRunToken, now: i64) -> Result<()> {
        self.write(Durability::Normal, move |c| {
            let known: bool = c
                .prepare_cached("SELECT 1 FROM runs WHERE id = ?1")
                .db()?
                .exists(params![token.run_id.as_str()])
                .db()?;
            if !known {
                return Err(StoreError::NotFound {
                    kind: "run",
                    id: token.run_id.to_string(),
                });
            }
            let scopes = serde_json::to_string(&token.scopes)?;
            c.prepare_cached(
                "INSERT INTO run_tokens (token_hash, run_id, bot, scopes, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            )
            .db()?
            .execute(params![
                token.token_hash,
                token.run_id.as_str(),
                token.bot,
                scopes,
                now,
                token.expires_at
            ])
            .db()?;
            events::insert(
                c,
                now,
                "run.token_issued",
                ("run", token.run_id.as_str()),
                None,
                &json!({ "bot": token.bot, "scopes": token.scopes, "expiresAt": token.expires_at }),
            )
        })
        .await
    }

    /// The token of `run` with this digest, valid or not. `None` when there is no such pair: a
    /// token minted for another run is not found under this one.
    pub async fn run_token(&self, run: &RunId, token_hash: &str) -> Result<Option<RunToken>> {
        let run = run.clone();
        let hash = token_hash.to_owned();
        self.read(move |c| {
            let row = c
                .prepare_cached(
                    "SELECT bot, scopes, expires_at, revoked_at FROM run_tokens
                     WHERE token_hash = ?1 AND run_id = ?2",
                )
                .db()?
                .query_row(params![hash, run.as_str()], |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Option<i64>>(3)?,
                    ))
                })
                .optional()
                .db()?;
            let Some((bot, scopes, expires_at, revoked_at)) = row else {
                return Ok(None);
            };
            let scopes: Vec<String> = serde_json::from_str(&scopes)
                .map_err(|e| StoreError::Corrupt(format!("bad token scopes: {e}")))?;
            Ok(Some(RunToken {
                run_id: run,
                bot,
                scopes,
                expires_at,
                revoked_at,
            }))
        })
        .await
    }

    /// Revokes every token of `run` that is not revoked yet. Returns how many it revoked, and
    /// writes a `run.token_revoked` event when that is more than none.
    pub async fn revoke_run_tokens(&self, run: &RunId, now: i64) -> Result<u64> {
        let run = run.clone();
        self.write(Durability::Normal, move |c| {
            let n = c
                .prepare_cached(
                    "UPDATE run_tokens SET revoked_at = ?2 WHERE run_id = ?1 AND revoked_at IS NULL",
                )
                .db()?
                .execute(params![run.as_str(), now])
                .db()?;
            if n > 0 {
                events::insert(
                    c,
                    now,
                    "run.token_revoked",
                    ("run", run.as_str()),
                    None,
                    &json!({ "count": n }),
                )?;
            }
            Ok(u64::try_from(n).unwrap_or(u64::MAX))
        })
        .await
    }

    /// Revokes every token that is still open, whatever its run: the daemon calls this when it
    /// starts, because a run that held a token did not survive the last daemon. Returns the runs
    /// whose tokens were revoked.
    pub async fn revoke_open_run_tokens(&self, now: i64) -> Result<Vec<RunId>> {
        self.write(Durability::Normal, move |c| {
            let mut stmt = c
                .prepare_cached(
                    "SELECT DISTINCT run_id FROM run_tokens WHERE revoked_at IS NULL ORDER BY run_id",
                )
                .db()?;
            let runs: Vec<String> = stmt
                .query_map([], |row| row.get(0))
                .db()?
                .collect::<rusqlite::Result<_>>()
                .db()?;
            c.prepare_cached("UPDATE run_tokens SET revoked_at = ?1 WHERE revoked_at IS NULL")
                .db()?
                .execute(params![now])
                .db()?;
            let mut out = Vec::with_capacity(runs.len());
            for run in runs {
                let id: RunId = run
                    .parse()
                    .map_err(|_| StoreError::Corrupt(format!("bad run id {run:?}")))?;
                events::insert(
                    c,
                    now,
                    "run.token_revoked",
                    ("run", id.as_str()),
                    None,
                    &json!({ "reason": "daemon_start" }),
                )?;
                out.push(id);
            }
            Ok(out)
        })
        .await
    }

    /// One entry of a board scope, with its value.
    pub async fn board_get(&self, scope: &str, key: &str) -> Result<Option<BoardRow>> {
        let (scope, key) = (scope.to_owned(), key.to_owned());
        self.read(move |c| load_entry(c, &scope, &key)).await
    }

    /// Every entry of a board scope, by key. The values are included: callers that list drop them.
    pub async fn board_list(&self, scope: &str) -> Result<Vec<BoardRow>> {
        let scope = scope.to_owned();
        self.read(move |c| {
            let mut stmt = c
                .prepare_cached(&format!(
                    "SELECT {BOARD_COLS} FROM board_entries WHERE scope = ?1 ORDER BY key LIMIT ?2"
                ))
                .db()?;
            let limit = i64::try_from(BOARD_MAX_KEYS).unwrap_or(i64::MAX);
            let rows = stmt.query_map(params![scope, limit], board_row).db()?;
            rows.collect::<rusqlite::Result<Vec<_>>>().db()
        })
        .await
    }

    /// Writes `value` under `key` in `scope`, on behalf of `run`.
    ///
    /// With `expected` the write happens only when the key's version is that number (0 meaning
    /// the key must not exist); otherwise it returns [`BoardWrite::Stale`] and changes nothing.
    ///
    /// Errors: [`StoreError::InvalidArgument`] for a key or value over the limits.
    pub async fn board_put(
        &self,
        scope: &str,
        key: &str,
        value: &str,
        expected: Option<u64>,
        run: &RunId,
        now: i64,
    ) -> Result<BoardWrite> {
        check_key(key)?;
        if value.len() > BOARD_VALUE_MAX_BYTES {
            return Err(StoreError::InvalidArgument(format!(
                "a board value is at most {BOARD_VALUE_MAX_BYTES} bytes"
            )));
        }
        let (scope, key, value, run) = (
            scope.to_owned(),
            key.to_owned(),
            value.to_owned(),
            run.clone(),
        );
        self.write(Durability::Normal, move |c| {
            let current = load_entry(c, &scope, &key)?;
            if let Some(expected) = expected
                && current.as_ref().map_or(0, |row| row.version) != expected
            {
                return Ok(BoardWrite::Stale { current });
            }
            if current.is_none() {
                let held: i64 = c
                    .prepare_cached("SELECT count(*) FROM board_entries WHERE scope = ?1")
                    .db()?
                    .query_row(params![scope], |row| row.get(0))
                    .db()?;
                if usize::try_from(held).unwrap_or(usize::MAX) >= BOARD_MAX_KEYS {
                    return Ok(BoardWrite::Full);
                }
            }
            let version = current.as_ref().map_or(1, |row| row.version + 1);
            let size = i64::try_from(value.len()).unwrap_or(i64::MAX);
            c.prepare_cached(
                "INSERT INTO board_entries (scope, key, value, version, size, updated_at, updated_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT (scope, key) DO UPDATE SET
                     value = excluded.value, version = excluded.version, size = excluded.size,
                     updated_at = excluded.updated_at, updated_by = excluded.updated_by",
            )
            .db()?
            .execute(params![
                scope,
                key,
                value,
                i64::try_from(version).unwrap_or(i64::MAX),
                size,
                now,
                run.as_str()
            ])
            .db()?;
            events::insert(
                c,
                now,
                "board.written",
                ("board", &scope),
                Some(run.as_str()),
                &json!({
                    "key": key,
                    "version": version,
                    "size": size,
                    "digest": token_digest(&value),
                }),
            )?;
            Ok(BoardWrite::Written { version })
        })
        .await
    }
}
