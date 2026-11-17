//! A one-time backfill of `runs.report` for runs that ended before schema V15.
//!
//! V15 added the column and the supervisor fills it as the agent reports, so a run that ended
//! earlier has `report = NULL` although its journal holds the `AgentReport` event. This reads the
//! last such event from each ended run's journal and stores it.
//!
//! This is the one place a journal is read to write state, and only once: it fills a column that
//! did not exist when the run ran, and it never decides anything from a journal (ARCH-8). The
//! pass is recorded as a `store.reports_backfilled` row in `events`, written in the same
//! transaction as the reports, so it runs once and a crash mid-way just repeats it.

use reno_proto::{ExecEvent, RunId, RunReport};
use rusqlite::params;
use serde_json::json;

use crate::db::Store;
use crate::error::{DbResult, Result, StoreError};
use crate::journal::Journals;

const MARKER_KIND: &str = "store.reports_backfilled";

/// What a backfill pass did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backfill {
    /// An earlier pass already ran; nothing was read.
    AlreadyDone,
    /// This pass looked at `scanned` ended runs and stored a report on `filled` of them.
    Ran {
        /// Runs whose journal was read.
        scanned: usize,
        /// Runs that gained a report.
        filled: usize,
    },
}

/// The last `AgentReport` in `run`'s journal, if any.
fn last_report(journals: &Journals, run: &RunId) -> Option<RunReport> {
    let mut last = None;
    for item in journals.replay(run, reno_proto::Seq(0)).ok()? {
        // A read error ends the scan: what was read so far is the best answer.
        let Ok((_, event)) = item else { break };
        if let ExecEvent::AgentReport {
            status, summary, ..
        } = event
        {
            last = Some(RunReport { status, summary });
        }
    }
    last
}

impl Store {
    /// Fills in missing reports of ended runs from their journals, once per database.
    ///
    /// Looks at no more than the newest `max_runs` ended runs without a report, so a huge store
    /// cannot make the pass unbounded; the pass is recorded as done either way. Blocking journal
    /// reads run on the blocking pool, so this is safe to await from the daemon's runtime.
    pub async fn backfill_reports(&self, now: i64, max_runs: usize) -> Result<Backfill> {
        let done = self
            .read(|c| {
                c.query_row(
                    "SELECT EXISTS (SELECT 1 FROM events WHERE kind = ?1)",
                    [MARKER_KIND],
                    |r| r.get::<_, bool>(0),
                )
                .db()
            })
            .await?;
        if done {
            return Ok(Backfill::AlreadyDone);
        }
        let limit = i64::try_from(max_runs).unwrap_or(i64::MAX);
        let ids: Vec<String> = self
            .read(move |c| {
                let mut stmt = c
                    .prepare(
                        "SELECT id FROM runs
                         WHERE report IS NULL AND status IN ('succeeded', 'failed', 'cancelled')
                         ORDER BY created_at DESC, rowid DESC LIMIT ?1",
                    )
                    .db()?;
                let rows = stmt.query_map([limit], |r| r.get(0)).db()?;
                rows.collect::<std::result::Result<_, _>>().db()
            })
            .await?;
        let scanned = ids.len();
        let journals = self.journals();
        let found: Vec<(String, String)> = tokio::task::spawn_blocking(move || {
            ids.into_iter()
                .filter_map(|id| {
                    let run = id.parse::<RunId>().ok()?;
                    let report = last_report(&journals, &run)?;
                    Some((id, serde_json::to_string(&report).ok()?))
                })
                .collect()
        })
        .await
        .map_err(|e| StoreError::Corrupt(format!("report backfill task: {e}")))?;
        let filled = self
            .write(crate::writer::Durability::Normal, move |c| {
                let mut filled = 0;
                for (id, report) in &found {
                    // A run that reported meanwhile (or was queued again) keeps what it has.
                    filled += c
                        .prepare_cached(
                            "UPDATE runs SET report = ?2
                             WHERE id = ?1 AND report IS NULL
                               AND status IN ('succeeded', 'failed', 'cancelled')",
                        )
                        .db()?
                        .execute(params![id, report])
                        .db()?;
                }
                crate::events::insert(
                    c,
                    now,
                    MARKER_KIND,
                    ("store", "reports"),
                    None,
                    &json!({ "scanned": scanned, "filled": filled }),
                )?;
                Ok(filled)
            })
            .await?;
        Ok(Backfill::Ran { scanned, filled })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use reno_proto::{ExecutorId, ReportStatus, RunStatus, TaskId};

    use super::*;
    use crate::NewTask;

    fn report_event(status: ReportStatus, summary: &str) -> ExecEvent {
        ExecEvent::AgentReport {
            status,
            summary: summary.into(),
            details: None,
        }
    }

    /// Makes run `name` and moves it to `status` (`None` leaves it queued).
    async fn make_run(store: &Store, name: &str, status: Option<RunStatus>) -> RunId {
        let task = TaskId::from_suffix(name).unwrap();
        let run = RunId::from_suffix(name).unwrap();
        store
            .create_task(
                NewTask {
                    id: task.clone(),
                    goal: "g".into(),
                    class: None,
                    spec: serde_json::json!({}),
                },
                0,
            )
            .await
            .unwrap();
        let executor: ExecutorId = "claude".parse().unwrap();
        store.enqueue_run(&task, &run, &executor, 0).await.unwrap();
        if let Some(status) = status {
            let claim = store.claim_next("a", 10, 100).await.unwrap().unwrap();
            assert_eq!(claim.run_id, run);
            store
                .transition(&run, claim.fence, RunStatus::Running, 20)
                .await
                .unwrap();
            if status != RunStatus::Cancelled {
                store
                    .transition(&run, claim.fence, RunStatus::Evaluating, 25)
                    .await
                    .unwrap();
            }
            store
                .transition(&run, claim.fence, status, 30)
                .await
                .unwrap();
        }
        run
    }

    fn journal(store: &Store, run: &RunId, events: &[ExecEvent]) {
        let mut writer = store.journals().writer(run).unwrap();
        for event in events {
            writer.append(event).unwrap();
        }
        writer.flush().unwrap();
    }

    async fn report_of(store: &Store, run: &RunId) -> Option<RunReport> {
        store.get_run(run).await.unwrap().unwrap().report
    }

    #[tokio::test]
    async fn an_ended_run_gains_the_last_report_in_its_journal_once() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("reno.db")).unwrap();
        let ended = make_run(&store, "a1", Some(RunStatus::Succeeded)).await;
        let silent = make_run(&store, "a2", Some(RunStatus::Succeeded)).await;
        let unjournaled = make_run(&store, "a3", Some(RunStatus::Failed)).await;
        let live = make_run(&store, "a4", None).await;
        journal(
            &store,
            &ended,
            &[
                report_event(ReportStatus::Progress, "halfway"),
                ExecEvent::Text { text: "x".into() },
                report_event(ReportStatus::Failed, "the registry is down"),
            ],
        );
        journal(&store, &silent, &[ExecEvent::Text { text: "x".into() }]);
        journal(&store, &live, &[report_event(ReportStatus::Done, "early")]);

        let first = store.backfill_reports(100, 1000).await.unwrap();
        assert_eq!(
            first,
            Backfill::Ran {
                scanned: 3,
                filled: 1
            }
        );
        let report = report_of(&store, &ended).await.unwrap();
        assert_eq!(report.status, ReportStatus::Failed);
        assert_eq!(report.summary, "the registry is down");
        assert_eq!(report_of(&store, &silent).await, None);
        assert_eq!(report_of(&store, &unjournaled).await, None);
        assert_eq!(report_of(&store, &live).await, None, "not ended");

        // Once only: a journal that gained a report later is not read again.
        journal(&store, &silent, &[report_event(ReportStatus::Done, "late")]);
        assert_eq!(
            store.backfill_reports(200, 1000).await.unwrap(),
            Backfill::AlreadyDone
        );
        assert_eq!(report_of(&store, &silent).await, None);
    }

    #[tokio::test]
    async fn the_pass_looks_only_at_the_newest_runs_up_to_its_bound() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("reno.db")).unwrap();
        let old = make_run(&store, "b1", Some(RunStatus::Succeeded)).await;
        journal(&store, &old, &[report_event(ReportStatus::Blocked, "old")]);
        let newer = make_run(&store, "b2", Some(RunStatus::Succeeded)).await;
        journal(&store, &newer, &[report_event(ReportStatus::Failed, "new")]);
        let outcome = store.backfill_reports(100, 1).await.unwrap();
        assert_eq!(
            outcome,
            Backfill::Ran {
                scanned: 1,
                filled: 1
            }
        );
        assert!(report_of(&store, &newer).await.is_some());
        assert_eq!(report_of(&store, &old).await, None);
    }
}
