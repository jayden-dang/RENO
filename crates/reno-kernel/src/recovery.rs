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
//! 3. **Approvals.** No supervisor exists at start, so every `pending` approval is orphaned:
//!    each becomes `expired` (a `synchronous=FULL` write), and its run's journal gets the
//!    `ApprovalResolved { denied, expired }` that its supervisor never wrote. The harness that
//!    asked is gone with its run, which is `interrupted` above.
//!    Run tokens (T1.9) go with them: every token still open is revoked, and the `RunLink` port
//!    clears its files.
//! 4. **Journals, state to journal.** A run that is terminal, or interrupted, gets a `Done` in
//!    its journal if the journal does not already end in one, with the stop reason its status
//!    implies.

use reno_proto::{ApprovalOutcome, ExecEvent, ResolvedBy, RunId, RunStatus, StopReason};
use reno_store::{ExpireScope, RunRecord};

use crate::error::Result;
use crate::procs;
use crate::shared::Shared;

/// What recovery did, for the log and for tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Recovered {
    pub(crate) requeued: Vec<RunId>,
    pub(crate) interrupted: Vec<RunId>,
    pub(crate) swept: usize,
    pub(crate) approvals_expired: usize,
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

    report.approvals_expired = expire_orphans(shared).await?;

    // No run survives a restart, so no run token does either (T1.9): the ones still open are
    // revoked, and the link's files are removed.
    crate::agent::revoke_orphans(shared).await?;

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

/// Expires every pending approval, for the moment no supervisor is left to answer them (start-up
/// recovery, and shutdown after the supervisors are gone or were dropped), and writes each one's
/// resolution into its run's journal. Returns how many there were.
///
/// State first, journal second (ARCH-8): the rows are expired in one durable transaction, and
/// only then are the journals appended to. A journal that cannot be appended to is logged and
/// left; the record is what counts.
pub(crate) async fn expire_orphans(shared: &Shared) -> Result<usize> {
    let expired = shared.db.expire_pending(ExpireScope::All).await?;
    if expired.is_empty() {
        return Ok(0);
    }
    let count = expired.len();
    let journals = shared.db.journals();
    tokio::task::spawn_blocking(move || {
        for closed in expired {
            let resolved = ExecEvent::ApprovalResolved {
                request_id: closed.request_id,
                decision: ApprovalOutcome::Denied,
                by: ResolvedBy::Expired,
                message: None,
            };
            let appended = journals
                .writer(&closed.run_id)
                .and_then(|mut writer| writer.append(&resolved).and_then(|_| writer.flush()));
            if let Err(error) = appended {
                tracing::error!(run = %closed.run_id, %error, "could not journal an expired approval");
            }
        }
    })
    .await
    .unwrap_or_else(|error| tracing::error!(%error, "journal task failed"));
    shared.approvals.refresh(&shared.db).await?;
    Ok(count)
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
