//! Standing rules (T1.6b): decisions a person kept beyond one action.
//!
//! # Life of a row
//!
//! A rule is created with the approval decision that made it, in one transaction
//! ([`Store::resolve_approval_remembering`](crate::Store::resolve_approval_remembering)), or on
//! its own ([`Store::create_rule`]). It is then only ever read, or revoked once
//! ([`Store::revoke_rule`]); a trigger refuses any other change and any delete.
//!
//! # Events
//!
//! `rule.created` and `rule.revoked` are written in the same transaction as the change (ARCH-8),
//! with the principal `human`. The payload carries a digest of the matcher and never the matcher,
//! which holds a command line that may hold a secret.
//!
//! # Time
//!
//! A rule's `expires_at` is compared with the `now` the caller passes. Nothing closes an expired
//! rule; it simply stops being returned by [`Store::active_rules`].

use reno_proto::defs::content_hash;
use reno_proto::{ApprovalId, RuleId, RuleMatcher, StandingEffect, StandingRule, StandingScope};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::json;

use crate::db::Store;
use crate::error::{DbResult, Result, StoreError, is_unique_violation};
use crate::events;
use crate::writer::Durability;

/// A rule to store.
#[derive(Debug, Clone, PartialEq)]
pub struct NewRule {
    /// Its id.
    pub id: RuleId,
    /// Where it applies.
    pub scope: StandingScope,
    /// What it does.
    pub effect: StandingEffect,
    /// What it matches.
    pub matcher: RuleMatcher,
    /// When it stops applying, in milliseconds since the epoch.
    pub expires_at: Option<i64>,
    /// The approval it came from. [`Store::resolve_approval_remembering`] fills this in.
    pub source_approval: Option<ApprovalId>,
}

const COLUMNS: &str = "id, scope, scope_run, scope_bot, effect, matcher, created_by, created_at, \
     expires_at, revoked_at, source_approval";

fn effect_str(effect: StandingEffect) -> &'static str {
    match effect {
        StandingEffect::Allow => "allow",
        StandingEffect::Deny => "deny",
    }
}

fn corrupt(what: &str, value: &str) -> StoreError {
    StoreError::Corrupt(format!("standing rule {what} {value:?}"))
}

fn ms(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<Result<StandingRule>> {
    let id: String = row.get(0)?;
    let scope: String = row.get(1)?;
    let scope_run: Option<String> = row.get(2)?;
    let scope_bot: Option<String> = row.get(3)?;
    let effect: String = row.get(4)?;
    let matcher: String = row.get(5)?;
    let created_by: String = row.get(6)?;
    let created_at: i64 = row.get(7)?;
    let expires_at: Option<i64> = row.get(8)?;
    let revoked_at: Option<i64> = row.get(9)?;
    let source: Option<String> = row.get(10)?;
    Ok((|| {
        let scope = match (scope.as_str(), scope_run, scope_bot) {
            ("run", Some(run), _) => StandingScope::Run {
                run: run.parse().map_err(|_| corrupt("run", &run))?,
            },
            ("bot", _, Some(bot)) => StandingScope::Bot { bot },
            ("global", _, _) => StandingScope::Always,
            _ => return Err(corrupt("scope", &scope)),
        };
        Ok(StandingRule {
            id: id.parse().map_err(|_| corrupt("id", &id))?,
            scope,
            effect: match effect.as_str() {
                "allow" => StandingEffect::Allow,
                "deny" => StandingEffect::Deny,
                other => return Err(corrupt("effect", other)),
            },
            matcher: serde_json::from_str(&matcher)
                .map_err(|e| StoreError::Corrupt(format!("standing rule matcher: {e}")))?,
            created_by,
            created_at_ms: ms(created_at),
            expires_at_ms: expires_at.map(ms),
            revoked_at_ms: revoked_at.map(ms),
            source_approval: source
                .map(|s| s.parse().map_err(|_| corrupt("source approval", &s)))
                .transpose()?,
        })
    })())
}

fn query(conn: &Connection, sql: &str, args: &[&dyn rusqlite::ToSql]) -> Result<Vec<StandingRule>> {
    let mut stmt = conn.prepare_cached(sql).db()?;
    let rows = stmt.query_map(args, from_row).db()?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row.db()??);
    }
    Ok(out)
}

/// Inserts a rule and its `rule.created` event. Called inside the caller's transaction.
pub(crate) fn tx_create(conn: &Connection, new: &NewRule, now: i64) -> Result<()> {
    let (scope, run, bot) = match &new.scope {
        StandingScope::Run { run } => ("run", Some(run.as_str()), None),
        StandingScope::Bot { bot } => ("bot", None, Some(bot.as_str())),
        StandingScope::Always => ("global", None, None),
    };
    conn.prepare_cached(
        "INSERT INTO standing_rules
             (id, scope, scope_run, scope_bot, effect, matcher, created_by, created_at,
              expires_at, source_approval)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'human', ?7, ?8, ?9)",
    )
    .db()?
    .execute(params![
        new.id.as_str(),
        scope,
        run,
        bot,
        effect_str(new.effect),
        serde_json::to_string(&new.matcher)?,
        now,
        new.expires_at,
        new.source_approval.as_ref().map(ApprovalId::as_str),
    ])
    .map_err(|e| {
        if is_unique_violation(&e) {
            StoreError::AlreadyExists {
                kind: "standing rule",
                id: new.id.to_string(),
            }
        } else {
            crate::error::db(e)
        }
    })?;
    events::insert(
        conn,
        now,
        "rule.created",
        ("rule", new.id.as_str()),
        Some("human"),
        &json!({
            "scope": new.scope,
            "effect": new.effect,
            "matcherDigest": content_hash(&serde_json::to_value(&new.matcher)?),
            "expiresAt": new.expires_at,
            "sourceApproval": new.source_approval,
        }),
    )
}

fn tx_revoke(conn: &Connection, id: &RuleId, now: i64) -> Result<()> {
    let current: Option<Option<i64>> = conn
        .prepare_cached("SELECT revoked_at FROM standing_rules WHERE id = ?1")
        .db()?
        .query_row([id.as_str()], |r| r.get(0))
        .optional()
        .db()?;
    match current {
        None => Err(StoreError::NotFound {
            kind: "standing rule",
            id: id.to_string(),
        }),
        // Revoking twice changes nothing and says nothing new.
        Some(Some(_)) => Ok(()),
        Some(None) => {
            conn.prepare_cached("UPDATE standing_rules SET revoked_at = ?2 WHERE id = ?1")
                .db()?
                .execute(params![id.as_str(), now])
                .db()?;
            events::insert(
                conn,
                now,
                "rule.revoked",
                ("rule", id.as_str()),
                Some("human"),
                &json!({}),
            )
        }
    }
}

impl Store {
    /// Stores a rule on its own, with its `rule.created` event. A [`Durability::Full`] write.
    /// Fails with `AlreadyExists` for a used id.
    pub async fn create_rule(&self, new: NewRule, now: i64) -> Result<()> {
        self.write(Durability::Full, move |c| tx_create(c, &new, now))
            .await
    }

    /// Revokes a rule for good, with its `rule.revoked` event. `NotFound` for an unknown id;
    /// revoking a revoked rule is a no-op. A [`Durability::Full`] write.
    pub async fn revoke_rule(&self, id: &RuleId, now: i64) -> Result<()> {
        let id = id.clone();
        self.write(Durability::Full, move |c| tx_revoke(c, &id, now))
            .await
    }

    /// One rule, in force or not.
    pub async fn get_rule(&self, id: &RuleId) -> Result<Option<StandingRule>> {
        let id = id.clone();
        self.read(move |conn| {
            Ok(query(
                conn,
                &format!("SELECT {COLUMNS} FROM standing_rules WHERE id = ?1"),
                &[&id.as_str()],
            )?
            .into_iter()
            .next())
        })
        .await
    }

    /// The rules, newest first. Only those in force at `now` (not revoked, not expired) unless
    /// `include_inactive`.
    pub async fn list_rules(&self, include_inactive: bool, now: i64) -> Result<Vec<StandingRule>> {
        self.read(move |conn| {
            query(
                conn,
                &format!(
                    "SELECT {COLUMNS} FROM standing_rules
                     WHERE ?1 OR (revoked_at IS NULL AND (expires_at IS NULL OR expires_at > ?2))
                     ORDER BY created_at DESC, rowid DESC"
                ),
                &[&include_inactive, &now],
            )
        })
        .await
    }

    /// The rules in force at `now` that apply to `run`, a run of `bot` (`None` for a run with no
    /// bot): global ones, ones for that bot, and ones for that run. Oldest first.
    pub async fn active_rules(
        &self,
        run: &reno_proto::RunId,
        bot: Option<&str>,
        now: i64,
    ) -> Result<Vec<StandingRule>> {
        let run = run.clone();
        let bot = bot.map(str::to_owned);
        self.read(move |conn| {
            query(
                conn,
                &format!(
                    "SELECT {COLUMNS} FROM standing_rules
                     WHERE revoked_at IS NULL AND (expires_at IS NULL OR expires_at > ?3)
                       AND (scope = 'global' OR scope_run = ?1 OR (scope = 'bot' AND scope_bot = ?2))
                     ORDER BY created_at, rowid"
                ),
                &[&run.as_str(), &bot, &now],
            )
        })
        .await
    }
}
