//! What a person chose about auto-update for each executor (EX-10), and when the daily check last
//! ran.
//!
//! One row per executor, written when the person changes the switch or the check runs. A missing
//! row means the defaults ([`ExecutorSetting::defaults`]): auto-update on, verified only off.
//! Each change appends an `executor.setting` event in the same transaction (ARCH-8).
//!
//! The store never reads the clock: every call takes `now`.

use reno_proto::ExecutorId;
use rusqlite::params;
use serde_json::json;

use crate::db::Store;
use crate::error::{DbResult, Result, StoreError};
use crate::events;
use crate::writer::Durability;

/// The stored choices of one executor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutorSetting {
    /// The executor.
    pub executor: ExecutorId,
    /// Update it when it is idle.
    pub auto_update: bool,
    /// Only to the version the adapter was verified on.
    pub verified_only: bool,
    /// What the CLI's own updater switch held before Reno turned it off, as the harness wrote it
    /// down. `None` when Reno has not turned it off.
    pub prev_self_update: Option<String>,
    /// When the daily check last ran, in milliseconds since the Unix epoch.
    pub last_checked_ms: Option<i64>,
}

impl ExecutorSetting {
    /// What an executor with no row has: auto-update on, verified only off.
    pub fn defaults(executor: ExecutorId) -> Self {
        Self {
            executor,
            auto_update: true,
            verified_only: false,
            prev_self_update: None,
            last_checked_ms: None,
        }
    }
}

impl Store {
    /// Stores the choices in `setting` (not the time of the last check, which
    /// [`Store::put_executor_checked`] owns) and writes an `executor.setting` event.
    pub async fn put_executor_setting(&self, setting: &ExecutorSetting, now: i64) -> Result<()> {
        let setting = setting.clone();
        self.write(Durability::Normal, move |c| {
            c.prepare_cached(
                "INSERT INTO executor_settings
                     (executor, auto_update, verified_only, prev_self_update)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (executor) DO UPDATE SET
                     auto_update = excluded.auto_update,
                     verified_only = excluded.verified_only,
                     prev_self_update = excluded.prev_self_update",
            )
            .db()?
            .execute(params![
                setting.executor.as_str(),
                setting.auto_update,
                setting.verified_only,
                setting.prev_self_update,
            ])
            .db()?;
            events::insert(
                c,
                now,
                "executor.setting",
                ("executor", setting.executor.as_str()),
                None,
                &json!({
                    "autoUpdate": setting.auto_update,
                    "verifiedOnly": setting.verified_only,
                    "selfUpdateSwitched": setting.prev_self_update.is_some(),
                }),
            )
        })
        .await
    }

    /// Records that the daily check of `executor` ran at `at`, keeping what else its row holds
    /// (or the defaults, for an executor with no row).
    pub async fn put_executor_checked(&self, executor: &ExecutorId, at: i64) -> Result<()> {
        let executor = executor.clone();
        self.write(Durability::Normal, move |c| {
            c.prepare_cached(
                "INSERT INTO executor_settings (executor, last_checked_at) VALUES (?1, ?2)
                 ON CONFLICT (executor) DO UPDATE SET last_checked_at = excluded.last_checked_at",
            )
            .db()?
            .execute(params![executor.as_str(), at])
            .db()?;
            Ok(())
        })
        .await
    }

    /// The stored choices of every executor that has a row, by executor id.
    pub async fn executor_settings(&self) -> Result<Vec<ExecutorSetting>> {
        self.read(|c| {
            let mut stmt = c
                .prepare_cached(
                    "SELECT executor, auto_update, verified_only, prev_self_update, last_checked_at
                     FROM executor_settings ORDER BY executor",
                )
                .db()?;
            let rows = stmt
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, bool>(1)?,
                        row.get::<_, bool>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<i64>>(4)?,
                    ))
                })
                .db()?;
            let mut out = Vec::new();
            for row in rows {
                let (executor, auto_update, verified_only, prev_self_update, last_checked_ms) =
                    row.db()?;
                let Ok(executor) = executor.parse::<ExecutorId>() else {
                    return Err(StoreError::Corrupt(format!(
                        "bad executor id {executor:?} in executor_settings"
                    )));
                };
                out.push(ExecutorSetting {
                    executor,
                    auto_update,
                    verified_only,
                    prev_self_update,
                    last_checked_ms,
                });
            }
            Ok(out)
        })
        .await
    }
}
