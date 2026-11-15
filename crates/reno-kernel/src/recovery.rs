//! Recovery at start (ARCHITECTURE 4.1 and 17.4). It runs to the end before the dispatcher
//! claims anything.
//!
//! It decides from run rows and never from journals (ARCH-8). In order:
//!
//! 1. **Sweep.** Every non-terminal run that is not `queued` and recorded a process (pid,
//!    process group and start time) has that group ended, unless the pid now belongs to someone
//!    else. Done first, so nothing is still writing when the state is repaired.
//! 2. **State.** `leased` goes back to `queued` (the claimer died before starting). `running`,
//!    `waiting_input`, `waiting_approval` and `evaluating` become `interrupted`: whether work
//!    with side effects may be redone is a decision, not a default. Each is a fenced transition
//!    at the row's own fence. `queued` and `interrupted` rows stay as they are.
//! 3. **Journals, state to journal.** A run that is terminal, or interrupted, gets a `Done` in
//!    its journal if the journal does not already end in one, with the stop reason its status
//!    implies.

use reno_proto::{ExecEvent, RunId, RunStatus, StopReason};
use reno_store::RunRecord;

use crate::error::Result;
use crate::procs;
use crate::shared::Shared;

/// What recovery did, for the log and for tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Recovered {
    pub(crate) requeued: Vec<RunId>,
    pub(crate) interrupted: Vec<RunId>,
    pub(crate) swept: usize,
    pub(crate) journals_repaired: usize,
}

pub(crate) async fn recover(shared: &Shared) -> Result<Recovered> {
    let mut report = Recovered::default();
    let rows = shared.db.nonterminal_runs().await?;

    let recorded: Vec<RunRecord> = rows
        .iter()
        .filter(|row| row.status != RunStatus::Queued)
        .cloned()
        .collect();
    report.swept = procs::sweep(&recorded, shared.config.sweep_grace).await;

    for row in &rows {
        match row.status {
            RunStatus::Leased => {
                shared
                    .db
                    .transition(&row.id, row.lease_fence, RunStatus::Queued)
                    .await?;
                report.requeued.push(row.id.clone());
            }
            RunStatus::Running
            | RunStatus::WaitingInput
            | RunStatus::WaitingApproval
            | RunStatus::Evaluating => {
                shared
                    .db
                    .transition(&row.id, row.lease_fence, RunStatus::Interrupted)
                    .await?;
                report.interrupted.push(row.id.clone());
            }
            RunStatus::Queued
            | RunStatus::Interrupted
            | RunStatus::Succeeded
            | RunStatus::Failed
            | RunStatus::Cancelled => {}
        }
    }

    let ended: Vec<(RunId, ExecEvent)> = shared
        .db
        .list_runs(None)
        .await?
        .iter()
        .filter(|row| row.status.is_terminal() || row.status == RunStatus::Interrupted)
        .map(|row| (row.id.clone(), repair_done(row.status)))
        .collect();
    let journals = shared.db.journals();
    // Blocking file I/O: scan each journal's tail, append where `Done` is missing.
    report.journals_repaired = tokio::task::spawn_blocking(move || {
        let mut repaired = 0;
        for (run, done) in &ended {
            match journals.ensure_done(run, done) {
                Ok(true) => repaired += 1,
                Ok(false) => {}
                Err(error) => tracing::error!(%run, %error, "could not repair the journal"),
            }
        }
        repaired
    })
    .await
    .unwrap_or_else(|error| {
        tracing::error!(%error, "journal repair task failed");
        0
    });

    if report != Recovered::default() {
        tracing::info!(?report, "recovery finished");
    }
    Ok(report)
}

/// The `Done` a journal gets when state says the run ended and the journal never said so.
fn repair_done(status: RunStatus) -> ExecEvent {
    let (stop, diagnostic) = match status {
        RunStatus::Succeeded => (StopReason::Completed, None),
        RunStatus::Failed => (
            StopReason::Error,
            Some("the journal ended before the run's Done; repaired from run state"),
        ),
        _ => (
            StopReason::Interrupted,
            Some("the journal ended before the run's Done; repaired from run state"),
        ),
    };
    ExecEvent::Done {
        stop,
        result: None,
        diagnostic: diagnostic.map(str::to_owned),
    }
}
