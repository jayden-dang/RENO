//! The routing decision of each run (T1.5): which executor the router chose, which it passed
//! over and why, and the fallback steps taken when the first choice would not start.
//!
//! One row per run, written in the transaction that enqueues the run and replaced, fenced, when
//! a fallback moves the run to another executor. The decision is the wire type
//! ([`RoutingDecision`]) as JSON, so what is stored is what clients are shown. Every write
//! appends a `run.routed` event in the same transaction (ARCH-8). The store never reads the
//! clock: every call takes `now`.

use reno_proto::defs::BotPin;
use reno_proto::{ExecutorId, RoutingDecision, RunId, TaskId, ThreadId};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use crate::db::Store;
use crate::error::{DbResult, Result, StoreError};
use crate::events;
use crate::runs::{NewRun, load_fenced, tx_enqueue_run};
use crate::writer::Durability;

/// A routed run to queue: see [`Store::enqueue_routed`].
#[derive(Debug, Clone)]
pub struct RoutedRun {
    /// The task.
    pub task: TaskId,
    /// The new run.
    pub run: RunId,
    /// The executor the run is recorded against.
    pub executor: ExecutorId,
    /// The bot version it is pinned to.
    pub bot: Option<BotPin>,
    /// The routing decision to store with it.
    pub decision: RoutingDecision,
    /// The thread to continue (a follow-up); `None` starts a thread of its own.
    pub thread: Option<ThreadId>,
}

/// Stores `decision` as the routing of `run`, replacing an earlier one, and writes the
/// `run.routed` event.
pub(crate) fn tx_put_routing(
    conn: &Connection,
    run: &RunId,
    executor: &ExecutorId,
    decision: &RoutingDecision,
    now: i64,
) -> Result<()> {
    let body = serde_json::to_string(decision)?;
    conn.prepare_cached(
        "INSERT INTO run_routing (run_id, decision, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (run_id) DO UPDATE SET
             decision = excluded.decision, updated_at = excluded.updated_at",
    )
    .db()?
    .execute(params![run.as_str(), body, now])
    .db()?;
    events::insert(
        conn,
        now,
        "run.routed",
        ("run", run.as_str()),
        None,
        &json!({ "executor": executor, "decision": decision }),
    )
}

impl Store {
    /// Queues a new run of `task` on `executor` (and pins it to `bot` when given), and stores
    /// the routing decision that chose `executor`, in one transaction.
    pub async fn enqueue_routed_run(
        &self,
        task: &TaskId,
        run: &RunId,
        executor: &ExecutorId,
        bot: Option<&BotPin>,
        decision: &RoutingDecision,
        now: i64,
    ) -> Result<()> {
        let new = RoutedRun {
            task: task.clone(),
            run: run.clone(),
            executor: executor.clone(),
            bot: bot.cloned(),
            decision: decision.clone(),
            thread: None,
        };
        self.enqueue_routed(new, now).await
    }

    /// Queues `new` and stores its routing decision, in one transaction. A run for a thread
    /// (`new.thread`) is that thread's next: its `parent_run` is the thread's newest run, and a
    /// thread whose worktree was removed, or that has a run queued or live, refuses it
    /// ([`StoreError::ThreadClosed`], [`StoreError::ThreadBusy`]). A closed thread whose
    /// worktree was kept is reopened. A run with no thread starts one.
    pub async fn enqueue_routed(&self, new: RoutedRun, now: i64) -> Result<()> {
        self.write(Durability::Normal, move |c| {
            tx_enqueue_run(
                c,
                &NewRun {
                    task: &new.task,
                    run: &new.run,
                    executor: &new.executor,
                    bot: new.bot.as_ref(),
                    routing: Some(&new.decision),
                    thread: new.thread.as_ref(),
                },
                now,
            )
        })
        .await
    }

    /// Moves a claimed run to `executor` and stores the decision that says why: the fallback
    /// walk. Carries the run's fence; a stale one is [`StoreError::StaleFence`].
    pub async fn reroute_run(
        &self,
        run: &RunId,
        fence: u64,
        executor: &ExecutorId,
        decision: &RoutingDecision,
        now: i64,
    ) -> Result<()> {
        let (run, executor, decision) = (run.clone(), executor.clone(), decision.clone());
        self.write(Durability::Normal, move |c| {
            load_fenced(c, &run, fence)?;
            c.prepare_cached("UPDATE runs SET executor = ?2 WHERE id = ?1")
                .db()?
                .execute(params![run.as_str(), executor.as_str()])
                .db()?;
            tx_put_routing(c, &run, &executor, &decision, now)
        })
        .await
    }

    /// The routing decision stored for `run`, if it has one: runs made before routing existed
    /// have none.
    pub async fn run_routing(&self, run: &RunId) -> Result<Option<RoutingDecision>> {
        let run = run.clone();
        self.read(move |c| {
            let body: Option<String> = c
                .prepare_cached("SELECT decision FROM run_routing WHERE run_id = ?1")
                .db()?
                .query_row([run.as_str()], |row| row.get(0))
                .optional()
                .db()?;
            body.map(|body| {
                serde_json::from_str(&body)
                    .map_err(|e| StoreError::Corrupt(format!("routing of {run}: {e}")))
            })
            .transpose()
        })
        .await
    }
}
