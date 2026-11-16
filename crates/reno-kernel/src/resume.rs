//! Resume (T1.10): when an interrupted run continues its harness session, and how.
//!
//! # Policy
//!
//! The daemon resumes a run on its own only at start-up, in recovery, run by run (nothing
//! polls). A run is resumed when it is `interrupted` and:
//!
//! | Check | Fails with |
//! |---|---|
//! | its last stop was not a person's cancel, the kill switch, a budget or the executor ending it by itself | [`NotResumed::Cancelled`], `Paused`, `BudgetStopped`, `ExecutorStopped` |
//! | its harness reported a session | `NoSession` |
//! | its executor is registered, and declares resume | `ExecutorMissing`, `ExecutorCannotResume` |
//! | its pinned bot does not say `resume = "never"` | `BotForbids` |
//! | it has made fewer than `KernelConfig::max_attempts` attempts | `AttemptsExhausted` |
//! | it has not already spent its wall-time budget | `WallTimeSpent` |
//!
//! The causes that do resume are the ones where the run was not asked to stop: the daemon died
//! with it live (`Crashed`, found by recovery, which includes a sleeping Mac that never woke the
//! daemon again) and a drain at shutdown (`Shutdown`). A run that fails a check stays
//! `interrupted` with the reason stored, and a person can still resume it (`run.resume`).
//!
//! A person's resume ([`manual`]) skips the policy: it asks only for a status that has a way
//! back to `queued`, a session, and an executor that can continue it. The attempt cap and the
//! bot's setting govern what the daemon does unasked, not what a person chooses.
//!
//! # How
//!
//! The run goes `interrupted` (or `failed`) to `queued` with its id, its session and its first
//! start time kept; the next claim takes a new fence, which is the next attempt number, and the
//! dispatcher starts the executor with `ExecSpec::resume` set. The first event the new attempt's
//! supervisor journals is a `Resumed`. The run's worktree is whatever the previous attempt
//! left: placement finds the run's open workspace and reuses it, and makes a clean one again at
//! the same path when the release at the end of the attempt had removed it.

use reno_proto::defs::{BotDef, DefKind, ResumePolicy};
use reno_proto::{NotResumed, RunId, RunStatus, StopDetail};
use reno_store::RunRecord;

use crate::budget;
use crate::error::{KernelError, Result};
use crate::shared::Shared;

/// What the daemon decides for one interrupted run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Queue it again.
    Resume,
    /// Leave it interrupted, for this reason.
    Keep(NotResumed),
}

/// Whether `row`'s run has a session and an executor that can continue it: the part of the
/// policy a person's resume keeps too.
pub(crate) fn can_continue(
    shared: &Shared,
    row: &RunRecord,
) -> std::result::Result<(), NotResumed> {
    if row.session_ref.is_none() {
        return Err(NotResumed::NoSession);
    }
    let Some(executor) = shared.executors.get(&row.executor) else {
        return Err(NotResumed::ExecutorMissing {
            executor: row.executor.clone(),
        });
    };
    if !executor.descriptor().caps.resume {
        return Err(NotResumed::ExecutorCannotResume {
            executor: row.executor.clone(),
        });
    }
    Ok(())
}

/// The bot definition `row` is pinned to, when it has one that can be read.
async fn pinned_bot(shared: &Shared, row: &RunRecord) -> Option<BotDef> {
    let pin = row.bot.as_ref()?;
    let stored = shared
        .db
        .get_definition(DefKind::Bot, &pin.name, pin.version)
        .await
        .ok()??;
    serde_json::from_value(stored.body).ok()
}

/// The daemon's own decision for an interrupted run. `active_ms` is how long the run has been at
/// it: from its first start to its last event before the stop.
pub(crate) async fn judge_auto(shared: &Shared, row: &RunRecord, active_ms: i64) -> Verdict {
    if let Some(why) = row.stop.as_ref().and_then(StopDetail::forbids_auto_resume) {
        return Verdict::Keep(why);
    }
    if let Err(why) = can_continue(shared, row) {
        return Verdict::Keep(why);
    }
    // An unreadable definition changes nothing here: the launch will say so, with the cause.
    let bot = pinned_bot(shared, row).await;
    if let (Some(pin), Some(bot)) = (&row.bot, &bot)
        && bot.resume == ResumePolicy::Never
    {
        return Verdict::Keep(NotResumed::BotForbids {
            bot: pin.name.clone(),
        });
    }
    let attempts = u32::try_from(row.lease_fence).unwrap_or(u32::MAX);
    let max = shared.config.max_attempts;
    if attempts >= max {
        return Verdict::Keep(NotResumed::AttemptsExhausted { attempts, max });
    }
    let wall = bot.map_or(shared.config.default_wall_time, |bot| {
        budget::wall_time(&bot.budget, shared.config.default_wall_time)
    });
    let limit_ms = u64::try_from(wall.as_millis()).unwrap_or(u64::MAX);
    if u64::try_from(active_ms).unwrap_or(0) >= limit_ms {
        return Verdict::Keep(NotResumed::WallTimeSpent { limit_ms });
    }
    Verdict::Resume
}

/// How long `row` has been at its work, in milliseconds: its first start to its newest event.
pub(crate) fn active_ms(row: &RunRecord) -> i64 {
    row.updated_at
        .saturating_sub(row.started_at.unwrap_or(row.created_at))
        .max(0)
}

/// Queues `row`'s run again. Returns the attempt it will be. The `Resumed` event is journaled by
/// the supervisor of that attempt, as the first event it writes, so there is only ever one
/// writer on the journal.
pub(crate) async fn requeue(
    shared: &Shared,
    row: &RunRecord,
    prompt: Option<String>,
) -> Result<u32> {
    let resumed = shared
        .db
        .resume_run(&row.id, row.lease_fence, prompt)
        .await?;
    let from_attempt = u32::try_from(resumed.from_attempt).unwrap_or(u32::MAX);
    shared.mark_runs_dirty();
    Ok(from_attempt.saturating_add(1))
}

/// A person's resume (`run.resume`): returns the attempt the run will be.
pub(crate) async fn manual(shared: &Shared, run: &RunId, prompt: Option<String>) -> Result<u32> {
    if shared.root.is_cancelled() {
        return Err(KernelError::ShuttingDown);
    }
    // A run whose state has committed may still hold its slot for an instant (its `Done` is
    // being journaled); give it a moment rather than racing it for the run id.
    for _ in 0..3 {
        if !shared.is_live(run) {
            break;
        }
        tokio::task::yield_now().await;
    }
    let row = shared.run_record(run).await?;
    let refuse = |why| KernelError::NotResumable {
        run: run.clone(),
        why,
    };
    if shared.is_live(run) {
        return Err(refuse(NotResumed::StillStopping));
    }
    if !matches!(row.status, RunStatus::Interrupted | RunStatus::Failed) {
        return Err(refuse(NotResumed::WrongStatus { status: row.status }));
    }
    can_continue(shared, &row).map_err(refuse)?;
    let attempt = requeue(shared, &row, prompt).await?;
    shared.notify_status_changed();
    shared.wake.notify_one();
    Ok(attempt)
}
