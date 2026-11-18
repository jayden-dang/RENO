//! Threads (RP0.5): the conversations that own a worktree.
//!
//! A thread is a row in `threads`; its runs point at it (`runs.thread_id`), and its workspace is
//! the `workspaces` row with its id. A run that names no thread starts one, `thr_` plus the run's
//! own suffix, in the transaction that queues the run. A run that names a thread joins it, as its
//! next run, in that same transaction.
//!
//! Whether a thread is `active`, `idle` or `closed` is read off the rows, not stored: closed
//! when `closed_at` is set, active while a run is queued or live, idle otherwise. So the status
//! cannot disagree with the runs. `updated_at` is when a run was added or the last one ended; the
//! idle limit runs from it.
//!
//! The store never reads the clock: every call takes `now`.

use reno_proto::{RunId, ThreadId, ThreadInfo, ThreadRun, ThreadStatus, WorkspaceInfo};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use crate::db::Store;
use crate::error::{DbResult, Result, StoreError, is_unique_violation};
use crate::events;
use crate::writer::Durability;

/// The statuses in which a run is queued or live: it holds its thread.
pub(crate) const LIVE: &str =
    "('queued', 'leased', 'running', 'waiting_input', 'waiting_approval', 'evaluating')";

/// The id of the thread a run starts: `thr_` plus the run's suffix.
pub(crate) fn thread_of_new_run(run: &RunId) -> Result<ThreadId> {
    let suffix = run.as_str().strip_prefix("run_").unwrap_or(run.as_str());
    ThreadId::from_suffix(suffix)
        .map_err(|_| StoreError::Corrupt(format!("run id {run} has no thread suffix")))
}

fn to_ms(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

/// Puts `run` (about to be inserted) in a thread: its own new one, or `thread`, which it
/// continues. Returns the thread and the run it follows.
pub(crate) fn tx_attach(
    conn: &Connection,
    task: &reno_proto::TaskId,
    run: &RunId,
    thread: Option<&ThreadId>,
    now: i64,
) -> Result<(ThreadId, Option<RunId>)> {
    let Some(thread) = thread else {
        let id = thread_of_new_run(run)?;
        let cwd: Option<String> = conn
            .prepare_cached("SELECT json_extract(spec, '$.cwd') FROM tasks WHERE id = ?1")
            .db()?
            .query_row([task.as_str()], |r| r.get(0))
            .optional()
            .db()?
            .flatten();
        conn.prepare_cached(
            "INSERT INTO threads (id, cwd, closed_at, created_at, updated_at)
             VALUES (?1, ?2, NULL, ?3, ?3)",
        )
        .db()?
        .execute(params![id.as_str(), cwd, now])
        .map_err(|e| {
            if is_unique_violation(&e) {
                StoreError::AlreadyExists {
                    kind: "run",
                    id: run.to_string(),
                }
            } else {
                crate::error::db(e)
            }
        })?;
        // The thread's birth is recorded by the `run.enqueued` event that follows, which names it.
        return Ok((id, None));
    };

    let closed: Option<Option<i64>> = conn
        .prepare_cached("SELECT closed_at FROM threads WHERE id = ?1")
        .db()?
        .query_row([thread.as_str()], |r| r.get(0))
        .optional()
        .db()?;
    let Some(closed_at) = closed else {
        return Err(StoreError::NotFound {
            kind: "thread",
            id: thread.to_string(),
        });
    };
    let live: Option<String> = conn
        .prepare_cached(&format!(
            "SELECT id FROM runs WHERE thread_id = ?1 AND status IN {LIVE} LIMIT 1"
        ))
        .db()?
        .query_row([thread.as_str()], |r| r.get(0))
        .optional()
        .db()?;
    if let Some(live) = live {
        return Err(StoreError::ThreadBusy {
            thread: thread.clone(),
            run: live
                .parse()
                .map_err(|_| StoreError::Corrupt(format!("bad run id {live:?}")))?,
        });
    }
    let gone: bool = conn
        .prepare_cached(
            "SELECT 1 FROM workspaces WHERE thread_id = ?1 AND status IN ('removed', 'lost')",
        )
        .db()?
        .exists([thread.as_str()])
        .db()?;
    if gone {
        return Err(StoreError::ThreadClosed(thread.clone()));
    }
    let parent: Option<String> = conn
        .prepare_cached(
            "SELECT id FROM runs WHERE thread_id = ?1 ORDER BY created_at DESC, rowid DESC LIMIT 1",
        )
        .db()?
        .query_row([thread.as_str()], |r| r.get(0))
        .optional()
        .db()?;
    let parent: Option<RunId> = parent
        .map(|p| {
            p.parse()
                .map_err(|_| StoreError::Corrupt(format!("bad run id {p:?}")))
        })
        .transpose()?;
    conn.prepare_cached("UPDATE threads SET closed_at = NULL, updated_at = ?2 WHERE id = ?1")
        .db()?
        .execute(params![thread.as_str(), now])
        .db()?;
    events::insert(
        conn,
        now,
        if closed_at.is_some() {
            "thread.reopened"
        } else {
            "thread.continued"
        },
        ("thread", thread.as_str()),
        None,
        &json!({ "run": run, "parent": parent }),
    )?;
    Ok((thread.clone(), parent))
}

/// A run of `thread` is queued again (a resume): the thread must have no other run queued or
/// live ([`StoreError::ThreadBusy`]), and is open again if it had been closed, because a thread
/// with a live run is never closed.
pub(crate) fn tx_rejoin(conn: &Connection, thread: &ThreadId, run: &RunId, now: i64) -> Result<()> {
    let other: Option<String> = conn
        .prepare_cached(&format!(
            "SELECT id FROM runs WHERE thread_id = ?1 AND id != ?2 AND status IN {LIVE} LIMIT 1"
        ))
        .db()?
        .query_row(params![thread.as_str(), run.as_str()], |r| r.get(0))
        .optional()
        .db()?;
    if let Some(other) = other {
        return Err(StoreError::ThreadBusy {
            thread: thread.clone(),
            run: other
                .parse()
                .map_err(|_| StoreError::Corrupt(format!("bad run id {other:?}")))?,
        });
    }
    let reopened = conn
        .prepare_cached(
            "UPDATE threads SET closed_at = NULL, updated_at = ?2 WHERE id = ?1 AND closed_at IS NOT NULL",
        )
        .db()?
        .execute(params![thread.as_str(), now])
        .db()?;
    if reopened > 0 {
        events::insert(
            conn,
            now,
            "thread.reopened",
            ("thread", thread.as_str()),
            None,
            &json!({ "run": run, "resumed": true }),
        )?;
    }
    Ok(())
}

fn load_runs(conn: &Connection, thread: &ThreadId) -> Result<Vec<ThreadRun>> {
    let mut stmt = conn
        .prepare_cached(
            "SELECT id, parent_run, start_sha FROM runs WHERE thread_id = ?1
             ORDER BY created_at, rowid",
        )
        .db()?;
    let rows = stmt
        .query_map([thread.as_str()], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, Option<String>>(1)?,
                r.get::<_, Option<String>>(2)?,
            ))
        })
        .db()?;
    let mut out = Vec::new();
    for row in rows {
        let (id, parent, start_sha) = row.db()?;
        let parse = |s: &str| {
            s.parse::<RunId>()
                .map_err(|_| StoreError::Corrupt(format!("bad run id {s:?}")))
        };
        out.push(ThreadRun {
            run_id: parse(&id)?,
            parent_run: parent.as_deref().map(parse).transpose()?,
            start_sha,
        });
    }
    Ok(out)
}

/// One thread as the wire shows it, without the idle deadline (the kernel owns the limit).
pub(crate) fn load_info(conn: &Connection, thread: &ThreadId) -> Result<Option<ThreadInfo>> {
    let row: Option<(Option<String>, Option<i64>, i64, i64)> = conn
        .prepare_cached("SELECT cwd, closed_at, created_at, updated_at FROM threads WHERE id = ?1")
        .db()?
        .query_row([thread.as_str()], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })
        .optional()
        .db()?;
    let Some((cwd, closed_at, created, updated)) = row else {
        return Ok(None);
    };
    let live: bool = conn
        .prepare_cached(&format!(
            "SELECT 1 FROM runs WHERE thread_id = ?1 AND status IN {LIVE}"
        ))
        .db()?
        .exists([thread.as_str()])
        .db()?;
    let status = if closed_at.is_some() {
        ThreadStatus::Closed
    } else if live {
        ThreadStatus::Active
    } else {
        ThreadStatus::Idle
    };
    let workspace: Option<WorkspaceInfo> = crate::workspaces::load_for_thread(conn, thread)?;
    Ok(Some(ThreadInfo {
        thread_id: thread.clone(),
        status,
        cwd: cwd.map(Into::into),
        project: workspace.as_ref().map(|w| w.repo.clone()),
        worktree: workspace.as_ref().map(|w| w.path.clone()),
        branch: workspace.as_ref().map(|w| w.branch.clone()),
        base_sha: workspace.as_ref().map(|w| w.base_sha.clone()),
        workspace: workspace.as_ref().map(|w| w.status),
        runs: load_runs(conn, thread)?,
        idle_expires_at_ms: None,
        created_at_ms: to_ms(created),
        updated_at_ms: to_ms(updated),
    }))
}

impl Store {
    /// One thread, with its runs and its worktree. `idle_expires_at_ms` is left for the caller,
    /// which knows the idle limit.
    pub async fn get_thread(&self, thread: &ThreadId) -> Result<Option<ThreadInfo>> {
        let thread = thread.clone();
        self.read(move |c| load_info(c, &thread)).await
    }

    /// The worktree of `thread`: the row of the run that made it.
    pub async fn get_thread_workspace(&self, thread: &ThreadId) -> Result<Option<WorkspaceInfo>> {
        let thread = thread.clone();
        self.read(move |c| crate::workspaces::load_for_thread(c, &thread))
            .await
    }

    /// Threads in one of `statuses` (every one when empty), most recently changed first, at most
    /// `limit` of them. The status is decided in the query, so a long history of closed threads
    /// costs nothing to skip.
    pub async fn list_threads(
        &self,
        statuses: &[ThreadStatus],
        limit: u32,
    ) -> Result<Vec<ThreadInfo>> {
        let wanted: Vec<String> = statuses
            .iter()
            .map(|status| match status {
                ThreadStatus::Closed => "t.closed_at IS NOT NULL".to_owned(),
                ThreadStatus::Active => format!(
                    "(t.closed_at IS NULL AND EXISTS \
                     (SELECT 1 FROM runs r WHERE r.thread_id = t.id AND r.status IN {LIVE}))"
                ),
                ThreadStatus::Idle => format!(
                    "(t.closed_at IS NULL AND NOT EXISTS \
                     (SELECT 1 FROM runs r WHERE r.thread_id = t.id AND r.status IN {LIVE}))"
                ),
            })
            .collect();
        let filter = if wanted.is_empty() {
            "1 = 1".to_owned()
        } else {
            wanted.join(" OR ")
        };
        self.read(move |c| {
            let ids: Vec<String> = c
                .prepare(&format!(
                    "SELECT t.id FROM threads t WHERE {filter}
                     ORDER BY t.updated_at DESC, t.created_at DESC, t.id DESC LIMIT ?1"
                ))
                .db()?
                .query_map([limit], |r| r.get(0))
                .db()?
                .collect::<std::result::Result<_, _>>()
                .db()?;
            let mut out = Vec::new();
            for id in ids {
                let id: ThreadId = id
                    .parse()
                    .map_err(|_| StoreError::Corrupt(format!("bad thread id {id:?}")))?;
                if let Some(info) = load_info(c, &id)? {
                    out.push(info);
                }
            }
            Ok(out)
        })
        .await
    }

    /// Marks `thread` closed. A thread already closed is returned as it is. Writes
    /// `thread.closed`. Errors: [`StoreError::NotFound`]; [`StoreError::ThreadBusy`] while one of
    /// its runs is queued or live.
    pub async fn close_thread(&self, thread: &ThreadId, now: i64) -> Result<ThreadInfo> {
        let thread = thread.clone();
        self.write(Durability::Full, move |c| {
            let Some(info) = load_info(c, &thread)? else {
                return Err(StoreError::NotFound {
                    kind: "thread",
                    id: thread.to_string(),
                });
            };
            match info.status {
                ThreadStatus::Closed => return Ok(info),
                ThreadStatus::Active => {
                    let run =
                        info.runs.last().map(|r| r.run_id.clone()).ok_or_else(|| {
                            StoreError::Corrupt("an active thread has no run".into())
                        })?;
                    return Err(StoreError::ThreadBusy { thread, run });
                }
                ThreadStatus::Idle => {}
            }
            c.prepare_cached("UPDATE threads SET closed_at = ?2 WHERE id = ?1")
                .db()?
                .execute(params![thread.as_str(), now])
                .db()?;
            events::insert(
                c,
                now,
                "thread.closed",
                ("thread", thread.as_str()),
                None,
                &json!({ "runs": info.runs.len() }),
            )?;
            load_info(c, &thread)?.ok_or_else(|| StoreError::NotFound {
                kind: "thread",
                id: thread.to_string(),
            })
        })
        .await
    }

    /// Records that the last run of `run`'s thread ended at `now`: the idle limit runs from here.
    /// Does nothing for a thread that is closed or that another run holds. Writes
    /// `thread.idle`. This commits after the run's own end, so a crash in between leaves the
    /// older `updated_at`, which only shortens the idle time by the length of the run.
    pub async fn thread_went_idle(&self, run: &RunId, now: i64) -> Result<()> {
        let run = run.clone();
        self.write(Durability::Normal, move |c| {
            let thread: Option<String> = c
                .prepare_cached("SELECT thread_id FROM runs WHERE id = ?1")
                .db()?
                .query_row([run.as_str()], |r| r.get(0))
                .optional()
                .db()?
                .flatten();
            let Some(thread) = thread else {
                return Ok(());
            };
            let changed = c
                .prepare_cached(&format!(
                    "UPDATE threads SET updated_at = ?2
                     WHERE id = ?1 AND closed_at IS NULL
                       AND NOT EXISTS (SELECT 1 FROM runs WHERE thread_id = ?1 AND status IN {LIVE})"
                ))
                .db()?
                .execute(params![thread, now])
                .db()?;
            if changed == 0 {
                return Ok(());
            }
            events::insert(
                c,
                now,
                "thread.idle",
                ("thread", thread.as_str()),
                None,
                &json!({ "run": run }),
            )
        })
        .await
    }

    /// The `updated_at` of the thread that has been idle the longest (open, no run queued or
    /// live), or `None` when no thread is idle: the idle limit is that time plus the limit.
    pub async fn oldest_idle_since(&self) -> Result<Option<i64>> {
        self.read(|c| {
            c.prepare_cached(&format!(
                "SELECT MIN(updated_at) FROM threads t WHERE closed_at IS NULL
                 AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.thread_id = t.id AND r.status IN {LIVE})"
            ))
            .db()?
            .query_row([], |r| r.get(0))
            .db()
        })
        .await
    }

    /// The open threads with no run queued or live that last changed at or before `cutoff`.
    pub async fn idle_threads_since(&self, cutoff: i64) -> Result<Vec<ThreadId>> {
        self.read(move |c| {
            let ids: Vec<String> = c
                .prepare_cached(&format!(
                    "SELECT id FROM threads t WHERE closed_at IS NULL AND updated_at <= ?1
                     AND NOT EXISTS (SELECT 1 FROM runs r WHERE r.thread_id = t.id AND r.status IN {LIVE})
                     ORDER BY updated_at, id"
                ))
                .db()?
                .query_map([cutoff], |r| r.get(0))
                .db()?
                .collect::<std::result::Result<_, _>>()
                .db()?;
            ids.into_iter()
                .map(|id| {
                    id.parse()
                        .map_err(|_| StoreError::Corrupt(format!("bad thread id {id:?}")))
                })
                .collect()
        })
        .await
    }

    /// Records the commit the thread's branch stood at when `run` first started. Fenced. Kept
    /// when already set: a resumed attempt does not move the start of its run.
    pub async fn record_start_sha(
        &self,
        run: &RunId,
        fence: u64,
        sha: &str,
        now: i64,
    ) -> Result<()> {
        let (run, sha) = (run.clone(), sha.to_owned());
        self.write(Durability::Normal, move |c| {
            crate::runs::load_fenced(c, &run, fence)?;
            let changed = c
                .prepare_cached(
                    "UPDATE runs SET start_sha = ?2 WHERE id = ?1 AND start_sha IS NULL",
                )
                .db()?
                .execute(params![run.as_str(), sha])
                .db()?;
            if changed == 0 {
                return Ok(());
            }
            events::insert(
                c,
                now,
                "run.start_sha",
                ("run", run.as_str()),
                None,
                &json!({ "sha": sha }),
            )
        })
        .await
    }
}
