//! Executor health: the latest probe of each executor, kept so a restarted daemon can show what
//! it last found before it has probed again (T1.1).
//!
//! One row per executor, replaced by every probe. The health is the wire type
//! ([`ExecutorHealth`]) as JSON, so what is stored is what clients are shown. Each write also
//! appends an `executor.health` event in the same transaction (ARCH-8), so the history of what
//! was found is in the event log and the row only ever holds the latest.
//!
//! The store never reads the clock: every call takes `now`.

use reno_proto::{ExecutorHealth, ExecutorId};
use rusqlite::params;
use serde_json::json;

use crate::db::Store;
use crate::error::{DbResult, Result, StoreError};
use crate::events;
use crate::writer::Durability;

impl Store {
    /// Stores `health` as the latest probe of `executor`, replacing the previous one, and
    /// writes an `executor.health` event.
    pub async fn put_executor_health(
        &self,
        executor: &ExecutorId,
        health: &ExecutorHealth,
        now: i64,
    ) -> Result<()> {
        let executor = executor.clone();
        let body = serde_json::to_string(health)?;
        let checked_at = i64::try_from(health.checked_at_ms).unwrap_or(i64::MAX);
        let summary = json!({
            "installed": health.installed,
            "versionGate": health.version_gate,
            "signedIn": health.signed_in,
            "models": { "source": health.models.source, "count": health.models.models.len() },
        });
        self.write(Durability::Normal, move |c| {
            c.prepare_cached(
                "INSERT INTO executor_health (executor, health, checked_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT (executor) DO UPDATE SET
                     health = excluded.health, checked_at = excluded.checked_at",
            )
            .db()?
            .execute(params![executor.as_str(), body, checked_at])
            .db()?;
            events::insert(
                c,
                now,
                "executor.health",
                ("executor", executor.as_str()),
                None,
                &summary,
            )
        })
        .await
    }

    /// The latest stored health of every executor that has one, by executor id.
    ///
    /// A row that no longer reads as the current wire type (written by a build with a different
    /// shape) is skipped: the next probe replaces it.
    pub async fn executor_health(&self) -> Result<Vec<(ExecutorId, ExecutorHealth)>> {
        self.read(|c| {
            let mut stmt = c
                .prepare_cached("SELECT executor, health FROM executor_health ORDER BY executor")
                .db()?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })
                .db()?;
            let mut out = Vec::new();
            for row in rows {
                let (executor, body) = row.db()?;
                let Ok(executor) = executor.parse::<ExecutorId>() else {
                    return Err(StoreError::Corrupt(format!(
                        "bad executor id {executor:?} in executor_health"
                    )));
                };
                if let Ok(health) = serde_json::from_str(&body) {
                    out.push((executor, health));
                }
            }
            Ok(out)
        })
        .await
    }
}
