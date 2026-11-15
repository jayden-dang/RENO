//! Workspaces: where a write run works (T1.3), kept for life so recovery can find and clean
//! them and clients can show them.
//!
//! One row per run, keyed by run id. The row is written with status `preparing` **before** the
//! worktree is made (ARCH-9: an external effect records its intent before it acts, so that write
//! is durable), and a crash at any point leaves a row that names the path and branch to
//! reconcile. Every change writes a `workspace.<status>` event in the same transaction (ARCH-8).
//! A run that is claimed again (retried or resumed) replaces its row.
//!
//! The store never reads the clock: every call takes `now`.

use reno_proto::{RunId, WorkspaceChanges, WorkspaceInfo, WorkspaceStatus};
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde_json::json;

use crate::db::Store;
use crate::error::{DbResult, Result, StoreError};
use crate::events;
use crate::writer::Durability;

/// The storage name of a status; matches the `CHECK` constraint and the serde names.
pub(crate) fn status_str(status: WorkspaceStatus) -> &'static str {
    match status {
        WorkspaceStatus::Preparing => "preparing",
        WorkspaceStatus::Active => "active",
        WorkspaceStatus::Kept => "kept",
        WorkspaceStatus::Removed => "removed",
        WorkspaceStatus::Lost => "lost",
    }
}

fn parse_status(s: &str) -> Result<WorkspaceStatus> {
    WorkspaceStatus::ALL
        .into_iter()
        .find(|&c| status_str(c) == s)
        .ok_or_else(|| StoreError::Corrupt(format!("unknown workspace status {s:?}")))
}

const COLS: &str = "run_id, repo, repo_key, path, subdir, branch, base_sha, status, commits, \
                    dirty, created_at, updated_at";

fn from_row(row: &Row<'_>) -> rusqlite::Result<Result<WorkspaceInfo>> {
    let run_id: String = row.get(0)?;
    let repo: String = row.get(1)?;
    let repo_key = row.get(2)?;
    let path: String = row.get(3)?;
    let subdir = row.get(4)?;
    let branch = row.get(5)?;
    let base_sha = row.get(6)?;
    let status: String = row.get(7)?;
    let commits: Option<i64> = row.get(8)?;
    let dirty: Option<bool> = row.get(9)?;
    let created: i64 = row.get(10)?;
    let updated: i64 = row.get(11)?;
    Ok((|| {
        let changes = match (commits, dirty) {
            (Some(commits), Some(dirty)) => Some(WorkspaceChanges {
                commits: u32::try_from(commits)
                    .map_err(|_| StoreError::Corrupt(format!("bad commit count {commits}")))?,
                dirty,
            }),
            _ => None,
        };
        Ok(WorkspaceInfo {
            run_id: run_id
                .parse()
                .map_err(|_| StoreError::Corrupt(format!("bad run id {run_id:?}")))?,
            repo: repo.into(),
            repo_key,
            path: path.into(),
            subdir,
            branch,
            base_sha,
            status: parse_status(&status)?,
            changes,
            created_at_ms: u64::try_from(created).unwrap_or(0),
            updated_at_ms: u64::try_from(updated).unwrap_or(0),
        })
    })())
}

fn text(path: &std::path::Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| StoreError::InvalidArgument("a workspace path is not UTF-8".into()))
}

fn load(conn: &Connection, run: &RunId) -> Result<Option<WorkspaceInfo>> {
    conn.prepare_cached(&format!("SELECT {COLS} FROM workspaces WHERE run_id = ?1"))
        .db()?
        .query_row(params![run.as_str()], from_row)
        .optional()
        .db()?
        .transpose()
}

fn event_data(info: &WorkspaceInfo) -> serde_json::Value {
    json!({
        "path": info.path,
        "repo": info.repo,
        "branch": info.branch,
        "baseSha": info.base_sha,
        "subdir": info.subdir,
        "commits": info.changes.map(|c| c.commits),
        "dirty": info.changes.map(|c| c.dirty),
    })
}

impl Store {
    /// Records that `info.run_id` is about to get the workspace `info`, replacing its previous
    /// row if it had one (a retried or resumed run). The stored status is `preparing`, and the
    /// timestamps are `now`, whatever `info` says. Writes `workspace.preparing`, durably.
    ///
    /// Errors: [`StoreError::NotFound`] when the run does not exist.
    pub async fn begin_workspace(&self, info: WorkspaceInfo, now: i64) -> Result<()> {
        self.write(Durability::Full, move |c| {
            let known: bool = c
                .prepare_cached("SELECT 1 FROM runs WHERE id = ?1")
                .db()?
                .exists(params![info.run_id.as_str()])
                .db()?;
            if !known {
                return Err(StoreError::NotFound {
                    kind: "run",
                    id: info.run_id.to_string(),
                });
            }
            c.prepare_cached(
                "INSERT INTO workspaces (run_id, repo, repo_key, path, subdir, branch, base_sha,
                                         status, commits, dirty, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'preparing', NULL, NULL, ?8, ?8)
                 ON CONFLICT (run_id) DO UPDATE SET
                     repo = excluded.repo, repo_key = excluded.repo_key, path = excluded.path,
                     subdir = excluded.subdir, branch = excluded.branch,
                     base_sha = excluded.base_sha, status = 'preparing', commits = NULL,
                     dirty = NULL, created_at = excluded.created_at,
                     updated_at = excluded.updated_at",
            )
            .db()?
            .execute(params![
                info.run_id.as_str(),
                text(&info.repo)?,
                info.repo_key,
                text(&info.path)?,
                info.subdir,
                info.branch,
                info.base_sha,
                now,
            ])
            .db()?;
            events::insert(
                c,
                now,
                "workspace.preparing",
                ("run", info.run_id.as_str()),
                None,
                &event_data(&info),
            )
        })
        .await
    }

    /// Moves a run's workspace to `status`, recording what it held when that is known (the last
    /// known changes are kept when `changes` is `None`). Writes `workspace.<status>`. Setting
    /// what is already stored writes nothing.
    ///
    /// Errors: [`StoreError::NotFound`] when the run has no workspace.
    pub async fn set_workspace_status(
        &self,
        run: &RunId,
        status: WorkspaceStatus,
        changes: Option<WorkspaceChanges>,
        now: i64,
    ) -> Result<WorkspaceInfo> {
        let run = run.clone();
        self.write(Durability::Full, move |c| {
            let Some(mut info) = load(c, &run)? else {
                return Err(StoreError::NotFound {
                    kind: "workspace",
                    id: run.to_string(),
                });
            };
            if info.status == status && (changes.is_none() || info.changes == changes) {
                return Ok(info);
            }
            let changes = changes.or(info.changes);
            c.prepare_cached(
                "UPDATE workspaces SET status = ?2, commits = ?3, dirty = ?4, updated_at = ?5
                 WHERE run_id = ?1",
            )
            .db()?
            .execute(params![
                run.as_str(),
                status_str(status),
                changes.map(|c| i64::from(c.commits)),
                changes.map(|c| c.dirty),
                now,
            ])
            .db()?;
            info.status = status;
            info.changes = changes;
            info.updated_at_ms = u64::try_from(now).unwrap_or(0);
            events::insert(
                c,
                now,
                &format!("workspace.{}", status_str(status)),
                ("run", run.as_str()),
                None,
                &event_data(&info),
            )?;
            Ok(info)
        })
        .await
    }

    /// Records that a run works in `cwd` itself, not in a worktree, and why (`reason` is a short
    /// word such as `read_only`). Writes one `workspace.in_place` event.
    pub async fn record_in_place(
        &self,
        run: &RunId,
        cwd: String,
        reason: &str,
        now: i64,
    ) -> Result<()> {
        let run = run.clone();
        let reason = reason.to_owned();
        self.write(Durability::Normal, move |c| {
            events::insert(
                c,
                now,
                "workspace.in_place",
                ("run", run.as_str()),
                None,
                &json!({ "cwd": cwd, "reason": reason }),
            )
        })
        .await
    }

    /// A run's workspace.
    pub async fn get_workspace(&self, run: &RunId) -> Result<Option<WorkspaceInfo>> {
        let run = run.clone();
        self.read(move |c| load(c, &run)).await
    }

    /// Workspaces in one of `statuses` (every one when empty), newest first.
    pub async fn list_workspaces(
        &self,
        statuses: &[WorkspaceStatus],
    ) -> Result<Vec<WorkspaceInfo>> {
        let wanted: Vec<&'static str> = statuses.iter().map(|s| status_str(*s)).collect();
        self.read(move |c| {
            let mut stmt = c
                .prepare_cached(&format!(
                    "SELECT {COLS} FROM workspaces ORDER BY created_at DESC, rowid DESC"
                ))
                .db()?;
            let rows = stmt.query_map([], from_row).db()?;
            let mut out = Vec::new();
            for row in rows {
                let info = row.db()??;
                if wanted.is_empty() || wanted.contains(&status_str(info.status)) {
                    out.push(info);
                }
            }
            Ok(out)
        })
        .await
    }
}
