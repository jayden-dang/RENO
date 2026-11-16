//! How a run's `Done` becomes state: the path through ARCHITECTURE 4.1 that each stop reason
//! takes. Every step is one fenced store transition, so each is atomic with its event row
//! (ARCH-8) and each is checked against `RunStatus::can_transition_to`.

use reno_proto::{ExecEvent, RunId, RunStatus, StopDetail, StopReason};
use reno_store::{Note, StoreError};

use crate::db::Db;

/// The terminal status a stop reason leads to, once evaluation has run.
fn evaluated(stop: StopReason) -> RunStatus {
    match stop {
        StopReason::Completed => RunStatus::Succeeded,
        StopReason::Error | StopReason::Refusal | StopReason::MaxTokens => RunStatus::Failed,
        StopReason::Interrupted => RunStatus::Interrupted,
    }
}

/// Move a run that is `from` (running, or waiting on a human) to the status its `stop` decides,
/// and return that status:
///
/// - `Interrupted` goes straight to `interrupted`, the one edge every live state has.
/// - Every other stop reason passes through `evaluating`; there is no edge from a waiting state
///   to `evaluating`, so a waiting run first goes back to `running`. Phase 0 has no evaluators,
///   so evaluation passes at once: `Completed` becomes `succeeded`, the rest `failed`.
///
/// `reason` is the typed reason the run stopped. It is stored with the last transition, in the
/// same transaction, and an interrupted run whose reason forbids resuming it also stores why.
pub(crate) async fn end_run(
    db: &Db,
    run: &RunId,
    fence: u64,
    from: RunStatus,
    stop: StopReason,
    reason: Option<&StopDetail>,
) -> Result<RunStatus, StoreError> {
    let end = evaluated(stop);
    let note = Note {
        stop: reason.cloned(),
        not_resumed: reason
            .filter(|_| end == RunStatus::Interrupted)
            .and_then(StopDetail::forbids_auto_resume),
    };
    if end == RunStatus::Interrupted {
        db.transition_noted(run, fence, RunStatus::Interrupted, note)
            .await?;
        return Ok(end);
    }
    if from != RunStatus::Running {
        db.transition(run, fence, RunStatus::Running).await?;
    }
    db.transition(run, fence, RunStatus::Evaluating).await?;
    db.transition_noted(run, fence, end, note).await?;
    Ok(end)
}

/// Append `done` to `run`'s journal, for a run that ended with no supervisor (it never started).
/// Call it only after the run's end state has committed (ARCH-8).
pub(crate) async fn append_done(db: &Db, run: &RunId, done: ExecEvent) {
    let journals = db.journals();
    let target = run.clone();
    let written = tokio::task::spawn_blocking(move || {
        let mut writer = journals.writer(&target)?;
        writer.append(&done)?;
        writer.flush()
    })
    .await;
    match written {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::error!(%run, %error, "could not journal the end of the run"),
        Err(error) => tracing::error!(%run, %error, "journal task failed"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_stop_reason_has_a_legal_path_from_every_live_state() {
        for from in [
            RunStatus::Running,
            RunStatus::WaitingInput,
            RunStatus::WaitingApproval,
        ] {
            for stop in [
                StopReason::Completed,
                StopReason::Interrupted,
                StopReason::Error,
                StopReason::Refusal,
                StopReason::MaxTokens,
            ] {
                let end = evaluated(stop);
                let mut at = from;
                let mut path = Vec::new();
                if end != RunStatus::Interrupted {
                    if at != RunStatus::Running {
                        path.push(RunStatus::Running);
                    }
                    path.push(RunStatus::Evaluating);
                }
                path.push(end);
                for next in path {
                    assert!(
                        at.can_transition_to(next),
                        "{from:?}/{stop:?}: {at:?} -> {next:?}"
                    );
                    at = next;
                }
            }
        }
    }
}
