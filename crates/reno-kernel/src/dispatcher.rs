//! The dispatcher actor: claims queued runs while a slot is free and starts them.
//!
//! It waits on exactly two things (ARCH-10): a `Notify` that `submit` and every run's end ring,
//! and the next lease-expiry deadline of a run that someone else leased, because that run
//! becomes claimable when its lease runs out. With nothing queued and nothing leased it sleeps
//! until woken and makes no store call. The only other timer is the retry after a store
//! failure, which exists only while the store is failing.
//!
//! Per claim it moves the run `leased` to `running`, starts the executor with `ExecControls`
//! whose cancel token is a child of the root token, and hands the stream to a run supervisor.
//! A run it cannot start (unknown executor, `ExecError`, a task with no working directory)
//! fails: `running`, `evaluating`, `failed`, and then a `Done { Error, diagnostic }` in the
//! journal, after the state has committed.
//!
//! # Launch settings
//!
//! A run for a bot starts with the `access` and `sandbox` of the bot **version it is pinned to**
//! (read from the store, so a later edit of the bot changes nothing for a run already queued);
//! any other run starts with the kernel's configured defaults. Before the executor is started its
//! descriptor is checked: an executor that does not declare the requested access mode is not
//! eligible, and the run fails with a `NoEligibleExecutor` diagnostic. The run is never started
//! with more access than it asked for (ARCH-15), and never with less access silently either.
//! The sandbox level is passed through but not checked: no executor declares its sandbox
//! levels yet, and refusing every run for that would make the field unusable.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use reno_harness::exec::ExecControls;
use reno_proto::defs::{BotDef, DefKind};
use reno_proto::{
    AccessMode, ExecEvent, ExecSpec, ExecutorDescriptor, RunId, RunStatus, SandboxLevel, StopReason,
};
use reno_store::{Claim, RunRecord, StoreError};
use tokio::sync::mpsc;
use tokio::time::sleep;

use crate::lifecycle;
use crate::shared::{COMMAND_QUEUE, CONTROL_QUEUE, LiveRun, Shared, Slot};
use crate::supervisor::{RunInfo, Supervisor};

pub(crate) async fn run(shared: Arc<Shared>) {
    loop {
        if shared.root.is_cancelled() {
            break;
        }
        let wait = match fill(&shared).await {
            Ok(()) => next_lease_expiry(&shared).await,
            Err(error) => {
                tracing::error!(%error, "dispatcher could not claim; retrying");
                Some(shared.config.store_retry)
            }
        };
        tokio::select! {
            biased;
            () = shared.root.cancelled() => break,
            () = shared.wake.notified() => {}
            () = sleep_for(wait) => {}
        }
    }
}

async fn sleep_for(wait: Option<Duration>) {
    match wait {
        Some(wait) => sleep(wait).await,
        None => std::future::pending().await,
    }
}

/// Claim runs until every slot is taken or nothing is claimable.
async fn fill(shared: &Arc<Shared>) -> Result<(), StoreError> {
    let lease_ms = shared.config.lease_ms();
    while shared.live_len() < shared.config.max_concurrent && !shared.root.is_cancelled() {
        let Some(claim) = shared.db.claim_next(&shared.config.owner, lease_ms).await? else {
            break;
        };
        launch(shared, claim).await;
    }
    Ok(())
}

/// How long until the earliest lease held by someone else runs out. Only asked while a slot is
/// free: a full dispatcher has nothing to claim with.
async fn next_lease_expiry(shared: &Arc<Shared>) -> Option<Duration> {
    if shared.live_len() >= shared.config.max_concurrent {
        return None;
    }
    let leased = shared.db.list_runs(Some(RunStatus::Leased)).await.ok()?;
    let now = shared.db.now_ms();
    let expiry = leased
        .iter()
        .filter(|run| !shared.is_live(&run.id))
        .filter_map(|run| run.lease_expires_at)
        .min()?;
    let wait = u64::try_from(expiry.saturating_sub(now))
        .unwrap_or(0)
        .max(1);
    Some(Duration::from_millis(wait))
}

/// The access and sandbox a run asks for: its pinned bot version's, else the kernel's defaults.
async fn launch_settings(
    shared: &Shared,
    row: &RunRecord,
) -> Result<(AccessMode, SandboxLevel), String> {
    let Some(pin) = &row.bot else {
        return Ok((shared.config.access, shared.config.sandbox));
    };
    let stored = shared
        .db
        .get_definition(DefKind::Bot, &pin.name, pin.version)
        .await
        .map_err(|error| format!("cannot read bot {} v{}: {error}", pin.name, pin.version))?
        .ok_or_else(|| {
            format!(
                "the pinned definition bot {} v{} is gone",
                pin.name, pin.version
            )
        })?;
    let bot: BotDef = serde_json::from_value(stored.body).map_err(|error| {
        format!(
            "the pinned definition bot {} v{} is unreadable: {error}",
            pin.name, pin.version
        )
    })?;
    Ok((bot.access, bot.sandbox))
}

/// The diagnostic of a run that no executor can honour, or `None` when `descriptor` can.
pub(crate) fn no_eligible_executor(
    descriptor: &ExecutorDescriptor,
    access: AccessMode,
) -> Option<String> {
    if descriptor.caps.access_modes.contains(&access) {
        return None;
    }
    Some(format!(
        "NoEligibleExecutor: the run asks for access mode {access:?}, and executor {} supports \
         only {:?}; a run is never started with a different access mode than it asked for",
        descriptor.id, descriptor.caps.access_modes
    ))
}

/// The task's prompt, working directory and model, as `submit` stored them.
fn exec_spec(
    row: &RunRecord,
    access: AccessMode,
    sandbox: SandboxLevel,
) -> Result<ExecSpec, String> {
    let cwd = row
        .spec
        .get("cwd")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| "the task has no working directory".to_owned())?;
    let model = row
        .spec
        .get("model")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .or_else(|| row.model.clone());
    Ok(ExecSpec {
        run_id: row.id.clone(),
        prompt: row.goal.clone(),
        cwd: PathBuf::from(cwd),
        model,
        effort: None,
        access,
        sandbox,
        resume: None,
        mcp: Vec::new(),
        env: Default::default(),
        output_schema: None,
    })
}

async fn launch(shared: &Arc<Shared>, claim: Claim) {
    let Claim { run_id, fence } = claim;
    let row = match shared.db.get_run(&run_id).await {
        Ok(Some(row)) => row,
        other => {
            tracing::error!(run = %run_id, ?other, "claimed run cannot be read");
            return;
        }
    };

    // The run holds a slot, and can be cancelled, from here on.
    let cancel = shared.root.child_token();
    let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_QUEUE);
    let slot = Slot::take(
        shared,
        run_id.clone(),
        LiveRun {
            cancel: cancel.clone(),
            cmd: cmd_tx,
        },
    );
    if let Err(error) = shared
        .db
        .transition(&run_id, fence, RunStatus::Running)
        .await
    {
        // The run stays leased; its lease runs out and it is claimed again.
        tracing::error!(run = %run_id, %error, "could not start the claimed run");
        return;
    }
    shared.mark_runs_dirty();

    let spec = match launch_settings(shared, &row)
        .await
        .and_then(|(access, sandbox)| exec_spec(&row, access, sandbox))
    {
        Ok(spec) => spec,
        Err(why) => return fail_start(shared, &run_id, fence, why, cmd_rx, slot).await,
    };
    let Some(executor) = shared.executors.get(&row.executor).cloned() else {
        let why = format!("unknown executor {}", row.executor);
        return fail_start(shared, &run_id, fence, why, cmd_rx, slot).await;
    };
    if let Some(why) = no_eligible_executor(&executor.descriptor(), spec.access) {
        return fail_start(shared, &run_id, fence, why, cmd_rx, slot).await;
    }
    let info = RunInfo {
        cwd: spec.cwd.clone(),
        bot: row.bot.as_ref().map(|pin| pin.name.clone()),
    };

    let (controls, handle) = ExecControls::with_cancel(CONTROL_QUEUE, cancel.clone());
    let started = tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        started = executor.start(spec, controls) => Some(started),
    };
    match started {
        Some(Ok(stream)) => {
            let supervisor = Supervisor::new(
                Arc::clone(shared),
                run_id,
                fence,
                info,
                stream,
                handle,
                cancel,
                cmd_rx,
                slot,
            );
            shared.spawn(supervisor.run());
        }
        Some(Err(error)) => {
            let why = format!("the executor refused the run: {error}");
            fail_start(shared, &run_id, fence, why, cmd_rx, slot).await;
        }
        None => {
            // Cancelled before the executor answered: nothing ran, so it is `interrupted`.
            let why = "cancelled while the executor was starting".to_owned();
            conclude(
                shared,
                &run_id,
                fence,
                StopReason::Interrupted,
                why,
                cmd_rx,
                slot,
            )
            .await;
        }
    }
}

async fn fail_start(
    shared: &Arc<Shared>,
    run: &RunId,
    fence: u64,
    why: String,
    cmd_rx: mpsc::Receiver<crate::shared::SupCmd>,
    slot: Slot,
) {
    tracing::warn!(%run, %why, "run failed to start");
    conclude(shared, run, fence, StopReason::Error, why, cmd_rx, slot).await;
}

/// End a run that has no supervisor: commit its state, then journal its `Done`, then free the
/// slot (ARCH-8). `cmd_rx` stays open until the `Done` is written, so a subscriber that
/// registered in the meantime finds the finished journal when its request is dropped.
async fn conclude(
    shared: &Arc<Shared>,
    run: &RunId,
    fence: u64,
    stop: StopReason,
    diagnostic: String,
    cmd_rx: mpsc::Receiver<crate::shared::SupCmd>,
    slot: Slot,
) {
    match lifecycle::end_run(&shared.db, run, fence, RunStatus::Running, stop).await {
        Ok(_) => {
            let done = ExecEvent::Done {
                stop,
                result: None,
                diagnostic: Some(diagnostic),
            };
            lifecycle::append_done(&shared.db, run, done).await;
        }
        Err(error) => tracing::error!(%run, %error, "could not commit the end of the run"),
    }
    drop(slot);
    drop(cmd_rx);
}
