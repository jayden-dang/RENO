//! The routing decision of each run (T1.5): which executor the router chose, which it passed
//! over and why, and the fallback steps taken when the first choice would not start.
//!
//! One row per run, written in the transaction that enqueues the run and replaced, fenced, when
//! a fallback moves the run to another executor. The decision is the wire type
//! ([`RoutingDecision`]) as JSON, so what is stored is what clients are shown. Every write
//! appends a `run.routed` event in the same transaction (ARCH-8). The store never reads the
//! clock: every call takes `now`.

use reno_proto::defs::BotPin;
use reno_proto::{ExecutorId, RoutingDecision, RunId, TaskId};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::json;

use crate::db::Store;
use crate::error::{DbResult, Result, StoreError};
use crate::events;
use crate::runs::{load_fenced, tx_enqueue_run};
use crate::writer::Durability;

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
        let (task, run, executor) = (task.clone(), run.clone(), executor.clone());
        let (bot, decision) = (bot.cloned(), decision.clone());
        self.write(Durability::Normal, move |c| {
            tx_enqueue_run(
                c,
                &task,
                &run,
                &executor,
                bot.as_ref(),
                Some(&decision),
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
