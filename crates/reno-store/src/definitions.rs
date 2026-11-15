//! Versioned definitions: the `definitions` table of ARCHITECTURE section 9.
//!
//! A definition is a bot, a trigger or the policy, identified by `(kind, name)`. Each distinct
//! content, identified by its hash, is one **version**, counted from 1 per name. Versions are
//! immutable: the body, hash and creation time of a stored version never change and the row is
//! never deleted (migration 4 makes the database refuse both). Only `status` moves:
//!
//! - `active`: the newest version of a name that is part of the current set;
//! - `superseded`: an older version, replaced when a newer one was stored;
//! - `retired`: the newest version of a name whose file is gone.
//!
//! Storing is idempotent on the hash of the **newest** version: the same hash again is no new
//! version, a different hash is the next version, and going back to an older content (A, B, A)
//! is version 3. A retired name whose file returns with the same content is made active again,
//! and no version is added.
//!
//! Every change of state, a new version, a retirement or a restoration, commits in the same
//! transaction as its event (ARCH-8) and runs with `synchronous=FULL` (ARCHITECTURE 17.4: a
//! definition change must survive power loss).

use std::collections::BTreeSet;

use reno_proto::defs::{DefKind, canonical_json, content_hash, is_valid_name};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::{Value, json};

use crate::db::Store;
use crate::error::{DbResult, Result, StoreError};
use crate::events;
use crate::writer::Durability;

/// The most definitions `current_definitions` returns: more than any loader reads.
const CURRENT_LIMIT: i64 = 10_000;

/// Where a stored version stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefStatus {
    /// The newest version of a name in the current set.
    Active,
    /// An older version.
    Superseded,
    /// The newest version of a name that has been removed.
    Retired,
}

impl DefStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Superseded => "superseded",
            Self::Retired => "retired",
        }
    }

    fn parse(s: &str) -> Result<Self> {
        match s {
            "active" => Ok(Self::Active),
            "superseded" => Ok(Self::Superseded),
            "retired" => Ok(Self::Retired),
            other => Err(StoreError::Corrupt(format!(
                "unknown definition status {other:?}"
            ))),
        }
    }
}

/// One stored version.
#[derive(Debug, Clone, PartialEq)]
pub struct DefinitionRecord {
    /// The kind.
    pub kind: DefKind,
    /// The name; `policy` for the policy.
    pub name: String,
    /// The version, from 1.
    pub version: u32,
    /// The content hash, 64 lowercase hex digits.
    pub hash: String,
    /// The definition as JSON.
    pub body: Value,
    /// Where the version stands.
    pub status: DefStatus,
    /// When it was first stored, in milliseconds since the epoch.
    pub created_at: i64,
}

/// A definition to store.
#[derive(Debug, Clone, PartialEq)]
pub struct NewDefinition {
    /// The kind.
    pub kind: DefKind,
    /// The name.
    pub name: String,
    /// The content hash of `body`; the store checks it.
    pub hash: String,
    /// The definition as JSON.
    pub body: Value,
}

/// What storing one definition did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PutOutcome {
    /// The kind.
    pub kind: DefKind,
    /// The name.
    pub name: String,
    /// The version the content is stored as.
    pub version: u32,
    /// Whether this call added the version. False when the content was already the newest.
    pub created: bool,
}

/// What [`Store::apply_definitions`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Applied {
    /// One outcome per definition passed in, in the order passed.
    pub puts: Vec<PutOutcome>,
    /// The names that were active and are not in the new set, now retired.
    pub retired: Vec<(DefKind, String)>,
}

fn key(kind: DefKind, name: &str) -> String {
    format!("{kind}/{name}")
}

fn parse_kind(s: &str) -> Result<DefKind> {
    s.parse()
        .map_err(|_| StoreError::Corrupt(format!("unknown definition kind {s:?}")))
}

fn to_u32(v: i64, what: &str) -> Result<u32> {
    u32::try_from(v).map_err(|_| StoreError::Corrupt(format!("bad {what} {v}")))
}

const COLS: &str = "kind, name, version, hash, body, status, created_at";

fn record_from_row(row: &Row<'_>) -> rusqlite::Result<Result<DefinitionRecord>> {
    let kind: String = row.get(0)?;
    let name: String = row.get(1)?;
    let version: i64 = row.get(2)?;
    let hash: String = row.get(3)?;
    let body: String = row.get(4)?;
    let status: String = row.get(5)?;
    let created_at: i64 = row.get(6)?;
    Ok((|| {
        Ok(DefinitionRecord {
            kind: parse_kind(&kind)?,
            name,
            version: to_u32(version, "definition version")?,
            hash,
            body: serde_json::from_str(&body)
                .map_err(|e| StoreError::Corrupt(format!("definition body: {e}")))?,
            status: DefStatus::parse(&status)?,
            created_at,
        })
    })())
}

fn check_new(new: &NewDefinition) -> Result<()> {
    if !is_valid_name(&new.name) {
        return Err(StoreError::InvalidArgument(format!(
            "{:?} is not a definition name",
            new.name
        )));
    }
    if new.hash != content_hash(&new.body) {
        return Err(StoreError::InvalidArgument(format!(
            "the hash of {} does not match its body",
            key(new.kind, &new.name)
        )));
    }
    Ok(())
}

/// Whether version `version` of the named definition is stored. Used by `runs` to refuse a pin
/// to a version that does not exist.
pub(crate) fn version_exists(
    conn: &Connection,
    kind: DefKind,
    name: &str,
    version: u32,
) -> Result<bool> {
    conn.prepare_cached("SELECT 1 FROM definitions WHERE kind = ?1 AND name = ?2 AND version = ?3")
        .db()?
        .exists(params![kind.as_str(), name, version])
        .db()
}

/// The newest version of a name: its version, hash and status.
fn newest(
    conn: &Connection,
    kind: DefKind,
    name: &str,
) -> Result<Option<(u32, String, DefStatus)>> {
    let row: Option<(i64, String, String)> = conn
        .prepare_cached(
            "SELECT version, hash, status FROM definitions
             WHERE kind = ?1 AND name = ?2 ORDER BY version DESC LIMIT 1",
        )
        .db()?
        .query_row(params![kind.as_str(), name], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })
        .optional()
        .db()?;
    row.map(|(version, hash, status)| {
        Ok((
            to_u32(version, "definition version")?,
            hash,
            DefStatus::parse(&status)?,
        ))
    })
    .transpose()
}

fn set_status(
    conn: &Connection,
    kind: DefKind,
    name: &str,
    version: u32,
    status: DefStatus,
) -> Result<()> {
    conn.prepare_cached(
        "UPDATE definitions SET status = ?4 WHERE kind = ?1 AND name = ?2 AND version = ?3",
    )
    .db()?
    .execute(params![kind.as_str(), name, version, status.as_str()])
    .db()?;
    Ok(())
}

pub(crate) fn tx_put(
    conn: &Connection,
    new: &NewDefinition,
    principal: Option<&str>,
    now: i64,
) -> Result<PutOutcome> {
    check_new(new)?;
    let outcome = |version, created| PutOutcome {
        kind: new.kind,
        name: new.name.clone(),
        version,
        created,
    };
    let subject = key(new.kind, &new.name);
    let newest = newest(conn, new.kind, &new.name)?;

    if let Some((version, hash, status)) = &newest
        && *hash == new.hash
    {
        if *status == DefStatus::Retired {
            set_status(conn, new.kind, &new.name, *version, DefStatus::Active)?;
            events::insert(
                conn,
                now,
                "definition.restored",
                ("definition", &subject),
                principal,
                &json!({ "version": version, "hash": hash }),
            )?;
        }
        return Ok(outcome(*version, false));
    }

    let version = match &newest {
        Some((version, ..)) => version
            .checked_add(1)
            .ok_or_else(|| StoreError::Corrupt("definition version overflow".into()))?,
        None => 1,
    };
    if let Some((previous, ..)) = &newest {
        set_status(conn, new.kind, &new.name, *previous, DefStatus::Superseded)?;
    }
    conn.prepare_cached(
        "INSERT INTO definitions (kind, name, version, hash, body, status, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6)",
    )
    .db()?
    .execute(params![
        new.kind.as_str(),
        new.name,
        version,
        new.hash,
        canonical_json(&new.body),
        now
    ])
    .db()?;
    events::insert(
        conn,
        now,
        "definition.created",
        ("definition", &subject),
        principal,
        &json!({
            "kind": new.kind,
            "name": new.name,
            "version": version,
            "hash": new.hash,
            "previous": newest.as_ref().map(|(v, ..)| v),
        }),
    )?;
    Ok(outcome(version, true))
}

fn tx_apply(
    conn: &Connection,
    desired: &[NewDefinition],
    principal: Option<&str>,
    now: i64,
) -> Result<Applied> {
    let mut puts = Vec::with_capacity(desired.len());
    for new in desired {
        puts.push(tx_put(conn, new, principal, now)?);
    }
    let wanted: BTreeSet<(DefKind, &str)> =
        desired.iter().map(|d| (d.kind, d.name.as_str())).collect();
    let active: Vec<(DefKind, String, u32)> = {
        let mut stmt = conn
            .prepare_cached("SELECT kind, name, version FROM definitions WHERE status = 'active'")
            .db()?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .db()?;
        let mut out = Vec::new();
        for row in rows {
            let (kind, name, version) = row.db()?;
            out.push((
                parse_kind(&kind)?,
                name,
                to_u32(version, "definition version")?,
            ));
        }
        out
    };
    let mut retired = Vec::new();
    for (kind, name, version) in active {
        if wanted.contains(&(kind, name.as_str())) {
            continue;
        }
        set_status(conn, kind, &name, version, DefStatus::Retired)?;
        events::insert(
            conn,
            now,
            "definition.retired",
            ("definition", &key(kind, &name)),
            principal,
            &json!({ "version": version }),
        )?;
        retired.push((kind, name));
    }
    Ok(Applied { puts, retired })
}

impl Store {
    /// Stores one definition: returns `(version, created)`.
    ///
    /// Idempotent on the hash of the newest version: the same content again returns that version
    /// with `created = false`; different content is stored as the next version and writes a
    /// `definition.created` event. `hash` must be the content hash of `body`
    /// ([`reno_proto::content_hash`]), and `body` is stored in canonical form. `principal` says
    /// who changed the definition, for the event.
    pub async fn put_definition(
        &self,
        kind: DefKind,
        name: &str,
        hash: &str,
        body: &Value,
        principal: Option<&str>,
        now: i64,
    ) -> Result<(u32, bool)> {
        let new = NewDefinition {
            kind,
            name: name.to_owned(),
            hash: hash.to_owned(),
            body: body.clone(),
        };
        let principal = principal.map(str::to_owned);
        self.write(Durability::Full, move |c| {
            tx_put(c, &new, principal.as_deref(), now)
        })
        .await
        .map(|o| (o.version, o.created))
    }

    /// Makes `desired` the active set, in one transaction: each definition is stored as by
    /// [`put_definition`](Self::put_definition), and every name that is active and not in
    /// `desired` is retired. Either all of it commits or none of it does, so a reader that sees
    /// a version also sees every other version of the same load.
    pub async fn apply_definitions(
        &self,
        desired: Vec<NewDefinition>,
        principal: Option<&str>,
        now: i64,
    ) -> Result<Applied> {
        let principal = principal.map(str::to_owned);
        self.write(Durability::Full, move |c| {
            tx_apply(c, &desired, principal.as_deref(), now)
        })
        .await
    }

    /// The active version of every definition of `kind` (of every kind when `None`), ordered by
    /// kind and name: the set as of the last [`apply_definitions`](Self::apply_definitions).
    pub async fn current_definitions(
        &self,
        kind: Option<DefKind>,
    ) -> Result<Vec<DefinitionRecord>> {
        self.read(move |conn| {
            let mut stmt = conn
                .prepare_cached(&format!(
                    "SELECT {COLS} FROM definitions
                     WHERE status = 'active' AND (?1 IS NULL OR kind = ?1)
                     ORDER BY kind, name LIMIT ?2"
                ))
                .db()?;
            let rows = stmt
                .query_map(
                    params![kind.map(DefKind::as_str), CURRENT_LIMIT],
                    record_from_row,
                )
                .db()?;
            let mut out = Vec::new();
            for row in rows {
                out.push(row.db()??);
            }
            Ok(out)
        })
        .await
    }

    /// One stored version, or `None` when there is none.
    pub async fn get_definition(
        &self,
        kind: DefKind,
        name: &str,
        version: u32,
    ) -> Result<Option<DefinitionRecord>> {
        let name = name.to_owned();
        self.read(move |conn| {
            conn.prepare_cached(&format!(
                "SELECT {COLS} FROM definitions WHERE kind = ?1 AND name = ?2 AND version = ?3"
            ))
            .db()?
            .query_row(params![kind.as_str(), name, version], record_from_row)
            .optional()
            .db()?
            .transpose()
        })
        .await
    }

    /// The newest stored version of a name, whatever its status, or `None`.
    pub async fn latest_definition(
        &self,
        kind: DefKind,
        name: &str,
    ) -> Result<Option<DefinitionRecord>> {
        let name = name.to_owned();
        self.read(move |conn| {
            conn.prepare_cached(&format!(
                "SELECT {COLS} FROM definitions WHERE kind = ?1 AND name = ?2
                 ORDER BY version DESC LIMIT 1"
            ))
            .db()?
            .query_row(params![kind.as_str(), name], record_from_row)
            .optional()
            .db()?
            .transpose()
        })
        .await
    }
}
