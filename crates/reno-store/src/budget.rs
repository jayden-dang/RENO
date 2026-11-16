//! Budgets (T1.7): token accounting per bot per local day, per run, and the pauses of the kill
//! switch. The limits themselves are not here: they are the bot definition's `budget`, read out
//! of the stored body of the version a run is pinned to (see [`ClaimFilter`]).
//!
//! # Tokens
//!
//! [`Store::record_usage`] adds the tokens of one `Usage` event to the run's total and to its
//! bot's total for the day it is given. The day is a label (`YYYY-MM-DD` in the machine's zone,
//! worked out by the kernel from its clock: the store never reads the clock or a zone). A day's
//! row is never reset: a new day is a new row, so the counts of past days stay as history and the
//! day boundary is only ever a change of label. Each call appends a `run.usage` event in the same
//! transaction (ARCH-8).
//!
//! # Pauses
//!
//! One row per paused scope: [`GLOBAL`] for everything, otherwise a bot's name. A pause outlives
//! the daemon. Setting and clearing one append a `budget.paused` / `budget.resumed` event with the
//! row (ARCH-8).

use std::collections::BTreeMap;

use reno_proto::RunId;
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use crate::db::Store;
use crate::error::{DbResult, Result, StoreError};
use crate::events;
use crate::runs::load_fenced;
use crate::writer::Durability;

/// The pause scope of everything. Bot names are `[a-z0-9_-]`, so this cannot be one.
pub const GLOBAL: &str = "*";

/// Tokens used, input and output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Tokens {
    /// Input tokens.
    pub input: u64,
    /// Output tokens.
    pub output: u64,
}

impl Tokens {
    /// Input and output together: what a ceiling counts.
    pub fn total(self) -> u64 {
        self.input.saturating_add(self.output)
    }
}

/// What a claim may not take, beyond a run being queued.
///
/// A queued run of a bot is passed over when
/// - the bot is in `paused_bots`, or
/// - the bot already has as many runs holding a slot as `budget.max_concurrent_runs` of the
///   definition version the run is pinned to (a `leased` run whose lease has run out holds
///   none), or
/// - the bot has used at least `budget.daily_token_ceiling` of that version on `day`.
///
/// The check is part of the claim's `SELECT`, so a bot at its limit never blocks the runs behind
/// it, and two claims cannot both take the last slot.
#[derive(Debug, Clone, Default)]
pub struct ClaimFilter {
    /// The local day tokens are counted for, `YYYY-MM-DD`.
    pub day: String,
    /// Bots that are paused by themselves.
    pub paused_bots: Vec<String>,
}

/// A bot's live and queued runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Occupancy {
    /// Runs that hold a slot: claimed and not ended.
    pub live: u32,
    /// Runs waiting to be claimed.
    pub queued: u32,
}

/// The statuses in which a run holds a slot.
pub(crate) const SLOT_STATUSES: &str =
    "'leased', 'running', 'waiting_input', 'waiting_approval', 'evaluating'";

/// The limit that stands for none in a claim's comparison.
const NO_LIMIT: i64 = i64::MAX;

/// The pinned definition's `budget.<key>`: its number, `default` when the version is stored
/// without the key, and `NO_LIMIT` when the version is not stored at all. `r` is the run.
fn budget_limit(key: &str, default: &str) -> String {
    format!(
        "COALESCE((SELECT COALESCE(json_extract(d.body, '$.budget.{key}'), {default})
                   FROM definitions d
                   WHERE d.kind = 'bot' AND d.name = r.bot_name AND d.version = r.bot_version),
                  {NO_LIMIT})"
    )
}

/// The `WHERE` terms that apply [`ClaimFilter`] to the run `r`. `?1` is now, `?2` the paused
/// bots as a JSON array and `?3` the day.
pub(crate) fn claim_terms() -> String {
    let concurrent = budget_limit("max_concurrent_runs", "1");
    let ceiling = budget_limit("daily_token_ceiling", &NO_LIMIT.to_string());
    format!(
        "(r.bot_name IS NULL OR (
            r.bot_name NOT IN (SELECT value FROM json_each(?2))
            AND (SELECT COUNT(*) FROM runs l
                 WHERE l.bot_name = r.bot_name AND l.id <> r.id
                   AND l.status IN ({SLOT_STATUSES})
                   AND NOT (l.status = 'leased' AND l.lease_expires_at <= ?1)) < {concurrent}
            AND COALESCE((SELECT u.input_tokens + u.output_tokens FROM bot_usage u
                          WHERE u.bot = r.bot_name AND u.day = ?3), 0) < {ceiling}
        ))"
    )
}

/// The claim filter as the parameters `claim_terms` takes after `now`.
pub(crate) fn filter_params(filter: &ClaimFilter) -> Result<(String, String)> {
    Ok((
        serde_json::to_string(&filter.paused_bots)?,
        filter.day.clone(),
    ))
}

fn to_i64(v: u64, what: &str) -> Result<i64> {
    i64::try_from(v).map_err(|_| StoreError::InvalidArgument(format!("{what} out of range")))
}

fn from_i64(v: i64, what: &str) -> Result<u64> {
    u64::try_from(v).map_err(|_| StoreError::Corrupt(format!("negative {what}")))
}

fn tokens_from(input: i64, output: i64) -> Result<Tokens> {
    Ok(Tokens {
        input: from_i64(input, "input tokens")?,
        output: from_i64(output, "output tokens")?,
    })
}

fn tx_record_usage(
    conn: &Connection,
    run: &RunId,
    fence: u64,
    bot: Option<&str>,
    day: &str,
    add: Tokens,
    now: i64,
) -> Result<Tokens> {
    load_fenced(conn, run, fence)?;
    let (input, output) = (to_i64(add.input, "tokens")?, to_i64(add.output, "tokens")?);
    conn.prepare_cached(
        "INSERT INTO run_usage (run_id, input_tokens, output_tokens, updated_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (run_id) DO UPDATE SET
             input_tokens = input_tokens + excluded.input_tokens,
             output_tokens = output_tokens + excluded.output_tokens,
             updated_at = excluded.updated_at",
    )
    .db()?
    .execute(params![run.as_str(), input, output, now])
    .db()?;
    let total = match bot {
        Some(bot) => {
            conn.prepare_cached(
                "INSERT INTO bot_usage (bot, day, input_tokens, output_tokens)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (bot, day) DO UPDATE SET
                     input_tokens = input_tokens + excluded.input_tokens,
                     output_tokens = output_tokens + excluded.output_tokens",
            )
            .db()?
            .execute(params![bot, day, input, output])
            .db()?;
            bot_day(conn, bot, day)?
        }
        None => run_total(conn, run)?,
    };
    events::insert(
        conn,
        now,
        "run.usage",
        ("run", run.as_str()),
        None,
        &json!({
            "bot": bot,
            "day": day,
            "inputTokens": add.input,
            "outputTokens": add.output,
        }),
    )?;
    Ok(total)
}

fn bot_day(conn: &Connection, bot: &str, day: &str) -> Result<Tokens> {
    let found: Option<(i64, i64)> = conn
        .prepare_cached(
            "SELECT input_tokens, output_tokens FROM bot_usage WHERE bot = ?1 AND day = ?2",
        )
        .db()?
        .query_row(params![bot, day], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
        .db()?;
    found.map_or(Ok(Tokens::default()), |(i, o)| tokens_from(i, o))
}

fn run_total(conn: &Connection, run: &RunId) -> Result<Tokens> {
    let found: Option<(i64, i64)> = conn
        .prepare_cached("SELECT input_tokens, output_tokens FROM run_usage WHERE run_id = ?1")
        .db()?
        .query_row([run.as_str()], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()
        .db()?;
    found.map_or(Ok(Tokens::default()), |(i, o)| tokens_from(i, o))
}

impl Store {
    /// Adds `add` to the tokens of `run` and, when it is a bot's, to the bot's tokens of `day`,
    /// and returns the total that matters for a ceiling: the bot's for the day, or the run's own
    /// for a run with no bot. Carries the run's fence.
    pub async fn record_usage(
        &self,
        run: &RunId,
        fence: u64,
        bot: Option<&str>,
        day: &str,
        add: Tokens,
        now: i64,
    ) -> Result<Tokens> {
        let (run, bot, day) = (run.clone(), bot.map(str::to_owned), day.to_owned());
        self.write(Durability::Normal, move |c| {
            tx_record_usage(c, &run, fence, bot.as_deref(), &day, add, now)
        })
        .await
    }

    /// What `run` has used, in all.
    pub async fn run_usage(&self, run: &RunId) -> Result<Tokens> {
        let run = run.clone();
        self.read(move |c| run_total(c, &run)).await
    }

    /// What every bot has used on `day`, by bot.
    pub async fn bot_usage(&self, day: &str) -> Result<BTreeMap<String, Tokens>> {
        let day = day.to_owned();
        self.read(move |c| {
            let mut stmt = c
                .prepare_cached(
                    "SELECT bot, input_tokens, output_tokens FROM bot_usage WHERE day = ?1",
                )
                .db()?;
            let rows = stmt
                .query_map([day], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })
                .db()?;
            let mut out = BTreeMap::new();
            for row in rows {
                let (bot, input, output) = row.db()?;
                out.insert(bot, tokens_from(input, output)?);
            }
            Ok(out)
        })
        .await
    }

    /// Every bot with a run holding a slot or waiting, and how many of each.
    pub async fn occupancy(&self) -> Result<BTreeMap<String, Occupancy>> {
        self.read(move |c| {
            let mut stmt = c
                .prepare_cached(&format!(
                    "SELECT bot_name,
                            SUM(status IN ({SLOT_STATUSES})), SUM(status = 'queued')
                     FROM runs
                     WHERE bot_name IS NOT NULL
                       AND status IN ('queued', {SLOT_STATUSES})
                     GROUP BY bot_name"
                ))
                .db()?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                    ))
                })
                .db()?;
            let mut out = BTreeMap::new();
            for row in rows {
                let (bot, live, queued) = row.db()?;
                out.insert(
                    bot,
                    Occupancy {
                        live: u32::try_from(live).unwrap_or(u32::MAX),
                        queued: u32::try_from(queued).unwrap_or(u32::MAX),
                    },
                );
            }
            Ok(out)
        })
        .await
    }

    /// The bots with a queued run that [`ClaimFilter`] passes over only because the bot has used
    /// its token ceiling on `day`. The dispatcher arms its one day-boundary deadline from this.
    pub async fn token_blocked_bots(&self, day: &str) -> Result<Vec<String>> {
        let day = day.to_owned();
        self.read(move |c| {
            let ceiling = budget_limit("daily_token_ceiling", &NO_LIMIT.to_string());
            let mut stmt = c
                .prepare_cached(&format!(
                    "SELECT DISTINCT r.bot_name FROM runs r
                     WHERE r.status = 'queued' AND r.bot_name IS NOT NULL
                       AND COALESCE((SELECT u.input_tokens + u.output_tokens FROM bot_usage u
                                     WHERE u.bot = r.bot_name AND u.day = ?1), 0) >= {ceiling}
                     ORDER BY r.bot_name"
                ))
                .db()?;
            let rows = stmt.query_map([day], |r| r.get::<_, String>(0)).db()?;
            rows.map(|row| row.db()).collect()
        })
        .await
    }

    /// Pauses `scope` ([`GLOBAL`] or a bot's name). `false` when it was paused already.
    pub async fn pause(&self, scope: &str, now: i64) -> Result<bool> {
        let scope = scope.to_owned();
        self.write(Durability::Normal, move |c| {
            let added = c
                .prepare_cached(
                    "INSERT INTO pauses (scope, paused_at) VALUES (?1, ?2)
                     ON CONFLICT (scope) DO NOTHING",
                )
                .db()?
                .execute(params![scope, now])
                .db()?;
            if added > 0 {
                events::insert(
                    c,
                    now,
                    "budget.paused",
                    ("budget", &scope),
                    None,
                    &json!({ "scope": scope }),
                )?;
            }
            Ok(added > 0)
        })
        .await
    }

    /// Lifts the pause of `scope`. `false` when it was not paused.
    pub async fn resume(&self, scope: &str, now: i64) -> Result<bool> {
        let scope = scope.to_owned();
        self.write(Durability::Normal, move |c| {
            let removed = c
                .prepare_cached("DELETE FROM pauses WHERE scope = ?1")
                .db()?
                .execute([&scope])
                .db()?;
            if removed > 0 {
                events::insert(
                    c,
                    now,
                    "budget.resumed",
                    ("budget", &scope),
                    None,
                    &json!({ "scope": scope }),
                )?;
            }
            Ok(removed > 0)
        })
        .await
    }

    /// The paused scopes, with when each was paused.
    pub async fn pauses(&self) -> Result<BTreeMap<String, i64>> {
        self.read(move |c| {
            let mut stmt = c
                .prepare_cached("SELECT scope, paused_at FROM pauses")
                .db()?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
                .db()?;
            rows.map(|row| row.db()).collect()
        })
        .await
    }

    /// Records that an occurrence of `trigger` was passed over for `reason` (`budget_tokens`),
    /// and moves the trigger on: its next due time is `next_due_at`. One transaction (ARCH-8).
    /// The occurrence is not a firing: no run exists for it to point at.
    pub async fn skip_trigger_occurrence(
        &self,
        trigger: &str,
        scheduled_at: i64,
        missed: u32,
        next_due_at: Option<i64>,
        reason: &str,
        now: i64,
    ) -> Result<()> {
        let (trigger, reason) = (trigger.to_owned(), reason.to_owned());
        self.write(Durability::Normal, move |c| {
            let changed = c
                .prepare_cached("UPDATE triggers SET next_due_at = ?2 WHERE name = ?1")
                .db()?
                .execute(params![trigger, next_due_at])
                .db()?;
            if changed == 0 {
                return Err(StoreError::NotFound {
                    kind: "trigger",
                    id: trigger,
                });
            }
            events::insert(
                c,
                now,
                "trigger.skipped",
                ("trigger", &trigger),
                None,
                &json!({
                    "reason": reason,
                    "scheduledAt": scheduled_at,
                    "missed": missed.max(1),
                    "nextDueAt": next_due_at,
                }),
            )
        })
        .await
    }

    /// Records, once, that `trigger` has an occurrence due that a pause holds back.
    pub async fn hold_trigger_occurrence(
        &self,
        trigger: &str,
        due_at: i64,
        now: i64,
    ) -> Result<()> {
        let trigger = trigger.to_owned();
        self.write(Durability::Normal, move |c| {
            events::insert(
                c,
                now,
                "trigger.held",
                ("trigger", &trigger),
                None,
                &json!({ "reason": "paused", "dueAt": due_at }),
            )
        })
        .await
    }
}
