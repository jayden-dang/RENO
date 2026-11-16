//! The dispatcher actor: claims queued runs while a slot is free and starts them.
//!
//! It waits on exactly these things (ARCH-10): a `Notify` that `submit`, `resume` and every run's
//! end ring (the end of a run is also what frees a bot's concurrency slot), the next lease-expiry
//! deadline of a run that someone else leased, because that run becomes claimable when its lease
//! runs out, and, only while a bot has queued runs held back by its daily token ceiling, the next
//! local midnight. With nothing queued and nothing leased it sleeps until woken and makes no
//! store call; while everything is paused it makes no claim at all. The only other timer is the
//! retry after a store failure, which exists only while the store is failing.
//!
//! Budgets (T1.7) are applied in the claim (`reno_store::ClaimFilter`), so a bot at its limit or
//! paused never hides the runs behind it. See `budget` for what each limit does.
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
//! The sandbox level is checked the same way: an executor must declare the level the run asks
//! for in `sandbox_levels`, else the run fails with `NoEligibleExecutor` before anything is
//! spawned. An executor with no sandbox of its own declares only `Unrestricted`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::BoxStream;
use reno_harness::ValueRedactor;
use reno_harness::exec::{ExecControls, ExecError, ExecHandle};
use reno_proto::defs::{BotDef, DefKind};
use reno_proto::{
    AccessMode, ExecEvent, ExecSpec, ExecutorDescriptor, ExecutorHealth, ExecutorId, RejectReason,
    RoutingDecision, RuleMatched, RunId, RunStatus, SandboxLevel, SecretsMissing, SessionRef,
    StartFailureKind, StartRefusal, StopDetail, StopReason,
};
use reno_store::{Claim, ClaimFilter, RunRecord, StoreError};
use tokio::sync::mpsc;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use crate::agent;
use crate::budget;
use crate::lifecycle;
use crate::router;
use crate::scheduler::{SchedCmd, StartFailure};
use crate::secrets::{SecretValue, SecretsError};
use crate::shared::{COMMAND_QUEUE, CONTROL_QUEUE, LiveRun, Shared, Slot, StopCause};
use crate::supervisor::{RunInfo, Supervisor};
use crate::workspaces;

pub(crate) async fn run(shared: Arc<Shared>) {
    loop {
        if shared.root.is_cancelled() {
            break;
        }
        let wait = match fill(&shared).await {
            Ok(()) => next_deadline(&shared).await,
            Err(error) => {
                tracing::error!(%error, "dispatcher could not claim; retrying");
                Some(shared.config.store_retry)
            }
        };
        tokio::select! {
            biased;
            () = shared.root.cancelled() => break,
            () = shared.wake.notified() => {}
            () = sleep_for(&shared, wait) => {}
        }
    }
}

async fn sleep_for(shared: &Shared, wait: Option<Duration>) {
    match wait {
        Some(wait) => {
            let timer = sleep(wait);
            #[cfg(test)]
            shared.db.armed(crate::db::Armed::Dispatcher);
            #[cfg(not(test))]
            let _ = shared;
            timer.await;
        }
        None => std::future::pending().await,
    }
}

/// Claim runs until every slot is taken or nothing is claimable. Nothing is claimed while
/// everything is paused (and no store call is made). A paused bot's runs, a bot's runs past its
/// concurrency limit and a bot's runs past its token ceiling are passed over in the claim itself,
/// so they never hold up the runs behind them (see `budget`).
async fn fill(shared: &Arc<Shared>) -> Result<(), StoreError> {
    let lease_ms = shared.config.lease_ms();
    while shared.live_len() < shared.config.max_concurrent && !shared.root.is_cancelled() {
        let pauses = shared.pauses_tx.borrow().clone();
        if pauses.global {
            break;
        }
        let filter = ClaimFilter {
            day: shared.db.today(),
            paused_bots: pauses.bots.iter().cloned().collect(),
        };
        let Some(claim) = shared
            .db
            .claim_next_filtered(&shared.config.owner, lease_ms, &filter)
            .await?
        else {
            break;
        };
        launch(shared, claim).await;
    }
    Ok(())
}

/// How long until the dispatcher must look again with nothing else having happened: the earliest
/// of a lease held by someone else running out, and the local midnight that gives a bot held back
/// by its token ceiling a new day. Both are deadlines (ARCH-10).
async fn next_deadline(shared: &Arc<Shared>) -> Option<Duration> {
    let lease = next_lease_expiry(shared).await;
    let day = next_day_start(shared).await;
    [lease, day].into_iter().flatten().min()
}

/// How long until the next local midnight, when a bot has queued runs that only its token ceiling
/// holds back. Only asked while a slot is free and nothing is paused globally (which holds
/// everything anyway).
async fn next_day_start(shared: &Arc<Shared>) -> Option<Duration> {
    if shared.live_len() >= shared.config.max_concurrent || shared.pauses_tx.borrow().global {
        return None;
    }
    let now = shared.db.now_ms();
    let zone = shared.db.zone();
    let blocked = shared.db.token_blocked_bots(&zone.day_of(now)).await.ok()?;
    if blocked.is_empty() {
        return None;
    }
    let wait = u64::try_from(zone.next_day_start_ms(now).saturating_sub(now))
        .unwrap_or(0)
        .max(1);
    Some(Duration::from_millis(wait))
}

/// How long until the earliest lease held by someone else runs out. Only asked while a slot is
/// free: a full dispatcher has nothing to claim with.
async fn next_lease_expiry(shared: &Arc<Shared>) -> Option<Duration> {
    if shared.live_len() >= shared.config.max_concurrent {
        return None;
    }
    let pauses = shared.pauses_tx.borrow().clone();
    if pauses.global {
        return None;
    }
    let now = shared.db.now_ms();
    let filter = ClaimFilter {
        day: shared.db.today(),
        paused_bots: pauses.bots.iter().cloned().collect(),
    };
    // Only leases a claim would take once they run out: a run the filter passes over is woken
    // by what lifts the filter (a resume, a run's end, midnight), not by its lease.
    let leased = shared.db.claimable_leases(&filter).await.ok()?;
    let expiry = leased
        .iter()
        .filter(|(run, _)| !shared.is_live(run))
        .map(|(_, at)| *at)
        .min()?;
    let wait = u64::try_from(expiry.saturating_sub(now))
        .unwrap_or(0)
        .max(1);
    Some(Duration::from_millis(wait))
}

/// What a run launches with, from its pinned bot version (else the kernel's defaults).
struct LaunchSettings {
    access: AccessMode,
    sandbox: SandboxLevel,
    /// The bot's standing instructions, handed to the executor with the run.
    instructions: Option<String>,
    /// Whether the bot lets a write run work in a directory that is not a git repository.
    in_place_writes: bool,
    /// The longest the run may take: its pinned bot version's `budget.max_wall_time`, else the
    /// kernel's default.
    wall_time: Duration,
    /// The daily token ceiling of the pinned bot version; none for a run with no bot.
    token_ceiling: Option<u64>,
    /// The bot's name, for the run's `RENO_BOT` and the secrets' refusal.
    bot: Option<String>,
    /// The secrets the pinned bot version lists: the names its run receives as environment
    /// variables.
    secrets: Vec<String>,
}

/// The settings a run launches with: its pinned bot version's, else the kernel's defaults.
async fn launch_settings(shared: &Shared, row: &RunRecord) -> Result<LaunchSettings, String> {
    let Some(pin) = &row.bot else {
        return Ok(LaunchSettings {
            access: shared.config.access,
            sandbox: shared.config.sandbox,
            instructions: None,
            in_place_writes: false,
            wall_time: shared.config.default_wall_time,
            token_ceiling: None,
            bot: None,
            secrets: Vec::new(),
        });
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
    Ok(LaunchSettings {
        access: bot.access,
        sandbox: bot.sandbox,
        instructions: Some(bot.instructions).filter(|text| !text.trim().is_empty()),
        in_place_writes: bot.in_place_writes,
        wall_time: budget::wall_time(&bot.budget, shared.config.default_wall_time),
        token_ceiling: bot.budget.daily_token_ceiling,
        bot: Some(pin.name.clone()),
        secrets: bot.secrets,
    })
}

/// Whether the run may write in a directory that is not a git repository: its task opted in
/// (`task.run` with `inPlace`), or its pinned bot allows it. A git repository is never written
/// in place either way (see `workspaces::place`).
fn in_place_opt_in(row: &RunRecord, settings: &LaunchSettings) -> bool {
    settings.in_place_writes
        || row
            .spec
            .get("inPlace")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
}

/// Why an executor may not start a run, and how a trigger's backoff accounts for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    /// The run's diagnostic, starting `NoEligibleExecutor:`.
    pub(crate) why: String,
    /// The account-level failure the scheduler backs off on.
    pub(crate) account: StartFailure,
    /// The same finding, typed, for the routing decision.
    pub(crate) reason: RejectReason,
}

/// The refusal of a run that `descriptor` cannot honour, or `None` when it can: it must declare
/// both the access mode and the sandbox level the run asks for (ARCH-15), and the health of its
/// last probe, when this process has one, must not say it is not installed, too old or signed
/// out. Unknown sign-in and an untested version never refuse. The finding is the router's
/// ([`router::ineligibility`]), so what the router would not choose the dispatcher does not start.
pub(crate) fn no_eligible_executor(
    descriptor: &ExecutorDescriptor,
    health: Option<&ExecutorHealth>,
    access: AccessMode,
    sandbox: SandboxLevel,
) -> Option<Refusal> {
    let reason = router::ineligibility(descriptor, health, access, sandbox)?;
    let id = &descriptor.id;
    let why = match &reason {
        RejectReason::AccessNotOffered { requested, offered } => format!(
            "NoEligibleExecutor: the run asks for access mode {requested:?}, and executor {id} \
             supports only {offered:?}; a run is never started with a different access mode than \
             it asked for"
        ),
        RejectReason::SandboxNotEnforced { requested, offered } => format!(
            "NoEligibleExecutor: the run asks for sandbox level {requested:?}, and executor {id} \
             declares only {offered:?}; a run is never started with a different sandbox than it \
             asked for"
        ),
        other => {
            let finding = health
                .and_then(ExecutorHealth::ineligible)
                .map_or_else(|| other.to_string(), |found| found.to_string());
            format!("NoEligibleExecutor: executor {id} is not usable: {finding}")
        }
    };
    let account = match router::failure_kind(&reason) {
        StartFailureKind::NotInstalled => StartFailure::NotInstalled,
        StartFailureKind::NotSignedIn => StartFailure::NotSignedIn,
        StartFailureKind::NoEligibleExecutor => StartFailure::NoEligibleExecutor,
    };
    Some(Refusal {
        why,
        account,
        reason,
    })
}

/// What a resumed run is told when nobody gave it a prompt of its own.
pub(crate) const RESUME_PROMPT: &str = "Your previous attempt at this task was interrupted. \
    Continue from where you left off, and finish the task.";

/// The task's prompt, working directory and model, as `submit` stored them. A run that holds a
/// session continues it: its prompt is the one given at resume, else the standard continue
/// prompt.
fn exec_spec(row: &RunRecord, settings: &LaunchSettings) -> Result<ExecSpec, String> {
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
    let resume = row.session_ref.clone().map(SessionRef);
    let prompt = match &resume {
        Some(_) => row
            .resume_prompt
            .clone()
            .unwrap_or_else(|| RESUME_PROMPT.to_owned()),
        None => row.goal.clone(),
    };
    Ok(ExecSpec {
        run_id: row.id.clone(),
        prompt,
        cwd: PathBuf::from(cwd),
        model,
        instructions: settings.instructions.clone(),
        effort: None,
        access: settings.access,
        sandbox: settings.sandbox,
        resume,
        mcp: Vec::new(),
        // Reno's own variables for the run; the bot's secrets join them at launch.
        env: run_env(&row.id, settings.bot.as_deref()),
        output_schema: None,
    })
}

/// The `RENO_*` variables every run's child gets.
fn run_env(run: &RunId, bot: Option<&str>) -> std::collections::BTreeMap<String, String> {
    let mut env = std::collections::BTreeMap::new();
    env.insert("RENO_RUN_ID".to_owned(), run.to_string());
    if let Some(bot) = bot {
        env.insert("RENO_BOT".to_owned(), bot.to_owned());
    }
    env
}

/// Why a run's secrets could not be resolved, and how a trigger's backoff counts it.
struct SecretsRefused {
    /// The run's diagnostic. It names secrets, never their values.
    why: String,
    /// The typed refusal the run's `Done` carries.
    refusal: StartRefusal,
    account: Option<StartFailure>,
}

/// The values of the secrets the run's bot lists, resolved through the `Secrets` port. A secret
/// that is not set refuses the run before anything is spawned, naming it (not its value). It
/// backs a trigger off like a signed-out account: a person has to act, and starting again would
/// fail the same way. A store that cannot be reached (a locked Keychain) is the same kind of
/// failure; any other store failure is not counted against the account.
async fn resolve_secrets(
    shared: &Shared,
    settings: &LaunchSettings,
) -> Result<std::collections::BTreeMap<String, SecretValue>, SecretsRefused> {
    if settings.secrets.is_empty() {
        return Ok(Default::default());
    }
    let bot = settings.bot.as_deref().unwrap_or("this run");
    let found = match &shared.secrets {
        Some(port) => match port.resolve(&settings.secrets).await {
            Ok(found) => found,
            Err(error) => {
                let unavailable = matches!(error, SecretsError::Unavailable(_));
                let account = unavailable.then_some(StartFailure::SecretMissing);
                let refusal = if unavailable {
                    StartRefusal::SecretMissing
                } else {
                    StartRefusal::Invalid
                };
                return Err(SecretsRefused {
                    refusal,
                    why: format!(
                        "SecretsUnavailable: the secrets of {bot} cannot be read: {error}"
                    ),
                    account,
                });
            }
        },
        None => Default::default(),
    };
    let missing: Vec<&str> = settings
        .secrets
        .iter()
        .map(String::as_str)
        .filter(|name| !found.contains_key(*name))
        .collect();
    if missing.is_empty() {
        return Ok(found);
    }
    let finding = SecretsMissing {
        bot: settings.bot.clone().unwrap_or_default(),
        missing: missing.iter().map(|name| (*name).to_owned()).collect(),
    };
    Err(SecretsRefused {
        why: finding.to_string(),
        refusal: StartRefusal::SecretMissing,
        account: Some(StartFailure::SecretMissing),
    })
}

/// What to say when a task that named no executor and no bot found none to go to.
const NAME_ONE: &str =
    "name an executor (task.run executor, `reno task run --executor`) or use a bot";

/// What one try at starting a run on one executor came to.
enum Attempt {
    Started(BoxStream<'static, ExecEvent>, ExecHandle),
    /// Cancelled before the executor answered: nothing ran.
    Cancelled,
    /// The executor will not take the run for an account or install reason: the next candidate
    /// may. Never an executor that started: a run that ran is not a fallback case.
    Declined {
        why: String,
        reason: RejectReason,
        account: StartFailure,
    },
    /// The run cannot start, and no other executor would change that.
    Failed {
        why: String,
        refusal: StartRefusal,
        account: Option<StartFailure>,
    },
}

/// Tries to start `spec` on executor `id`: the eligibility check against fresh health, then the
/// executor's own start.
///
/// `scrub` is the run's secret redactor: the executor already holds the run's secrets, so what it
/// refuses with is cleaned before it is journaled, stored in the routing decision or logged.
async fn attempt(
    shared: &Arc<Shared>,
    cancel: &CancellationToken,
    id: &ExecutorId,
    spec: &ExecSpec,
    scrub: Option<&ValueRedactor>,
) -> Attempt {
    let clean = |text: String| match scrub {
        Some(redactor) => redactor.redact_str(&text).into_owned(),
        None => text,
    };
    let Some(executor) = shared.executors.get(id).cloned() else {
        return Attempt::Failed {
            why: format!("unknown executor {id}"),
            refusal: StartRefusal::Invalid,
            account: None,
        };
    };
    let descriptor = executor.descriptor();
    // An executor that cannot inject MCP servers gets none: Reno's own server is left out, and
    // the run can only work alone.
    let mut spec = spec.clone();
    if !descriptor.caps.mcp_injection && !spec.mcp.is_empty() {
        tracing::warn!(executor = %id, "the executor does not inject MCP servers; Reno's is left out");
        spec.mcp.clear();
    }
    let spec = &spec;
    if spec.resume.is_some() && !descriptor.caps.resume {
        // A session belongs to the executor that made it; there is no other to go to.
        return Attempt::Failed {
            why: format!(
                "NoEligibleExecutor: executor {id} cannot resume a session, and the run has one"
            ),
            refusal: StartRefusal::NoEligibleExecutor,
            account: None,
        };
    }
    let found = shared.health_for_dispatch(id).await;
    if let Some(refusal) =
        no_eligible_executor(&descriptor, found.as_ref(), spec.access, spec.sandbox)
    {
        return Attempt::Declined {
            why: refusal.why,
            reason: refusal.reason,
            account: refusal.account,
        };
    }
    let (controls, handle) = ExecControls::with_cancel(CONTROL_QUEUE, cancel.clone());
    let started = tokio::select! {
        biased;
        () = cancel.cancelled() => return Attempt::Cancelled,
        started = executor.start(spec.clone(), controls) => started,
    };
    match started {
        Ok(stream) => Attempt::Started(stream, handle),
        Err(error) => {
            let why = clean(format!("the executor refused the run: {error}"));
            match error {
                ExecError::NotInstalled(detail) => {
                    let detail = clean(detail);
                    // A definite signal that what the board holds may be out of date.
                    shared.probe_soon(id);
                    Attempt::Declined {
                        why,
                        reason: RejectReason::NotInstalled { detail },
                        account: StartFailure::NotInstalled,
                    }
                }
                ExecError::NotSignedIn(hint) => {
                    let hint = clean(hint);
                    shared.probe_soon(id);
                    Attempt::Declined {
                        why,
                        reason: RejectReason::SignedOut { hint },
                        account: StartFailure::NotSignedIn,
                    }
                }
                ExecError::UnsupportedAccess {
                    requested,
                    supported,
                } => Attempt::Declined {
                    why,
                    reason: RejectReason::AccessNotOffered {
                        requested,
                        offered: supported,
                    },
                    account: StartFailure::NoEligibleExecutor,
                },
                ExecError::UnsupportedOutputSchema | ExecError::InvalidSpec(_) => Attempt::Failed {
                    why,
                    refusal: StartRefusal::Invalid,
                    account: None,
                },
            }
        }
    }
}

/// The diagnostic of a run no executor would take: the single refusal as it was, or all of them.
fn refused_by_all(refusals: &[(ExecutorId, String)]) -> String {
    match refusals {
        [(_, only)] => only.clone(),
        many => {
            let tried: Vec<String> = many
                .iter()
                .map(|(id, why)| {
                    format!("{id}: {}", why.trim_start_matches("NoEligibleExecutor: "))
                })
                .collect();
            format!(
                "NoEligibleExecutor: no executor could take the run ({})",
                tried.join("; ")
            )
        }
    }
}

/// Records on the stored decision that `current` is the executor the run starts on, when the
/// decision had none: a probe since the task was routed made it eligible.
fn adopt(decision: &mut RoutingDecision, current: &ExecutorId) {
    decision.rejected.retain(|r| &r.executor != current);
    decision.ranked = vec![current.clone()];
    decision.chosen = Some(current.clone());
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
    let cause = StopCause::default();
    let (cmd_tx, cmd_rx) = mpsc::channel(COMMAND_QUEUE);
    let slot = Slot::take(
        shared,
        run_id.clone(),
        LiveRun {
            cancel: cancel.clone(),
            cause: cause.clone(),
            cmd: cmd_tx,
            bot: row.bot.as_ref().map(|pin| pin.name.clone()),
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

    let settings = match launch_settings(shared, &row).await {
        Ok(settings) => settings,
        Err(why) => {
            let refusal = StartRefusal::Invalid;
            return fail_start(shared, &run_id, fence, why, refusal, None, cmd_rx, slot).await;
        }
    };
    let mut spec = match exec_spec(&row, &settings) {
        Ok(spec) => spec,
        Err(why) => {
            let refusal = StartRefusal::Invalid;
            return fail_start(shared, &run_id, fence, why, refusal, None, cmd_rx, slot).await;
        }
    };
    // The bot's secrets, before anything else is prepared: a missing one refuses the run here.
    let mut redactor = None;
    match resolve_secrets(shared, &settings).await {
        Ok(found) if found.is_empty() => {}
        Ok(found) => {
            redactor = Some(Arc::new(ValueRedactor::new(
                found
                    .iter()
                    .map(|(name, value)| (name.as_str(), value.expose())),
            )));
            spec.env.extend(
                found
                    .into_iter()
                    .map(|(name, value)| (name, value.expose().to_owned())),
            );
        }
        Err(refused) => {
            let why = refused.why;
            let refusal = refused.refusal;
            return fail_start(
                shared,
                &run_id,
                fence,
                why,
                refusal,
                refused.account,
                cmd_rx,
                slot,
            )
            .await;
        }
    }
    let resuming = spec.resume.is_some();
    // Where the run works: its own worktree for a write run in a git repository (T1.3).
    let opted_in = in_place_opt_in(&row, &settings);
    let placed = tokio::select! {
        biased;
        () = cancel.cancelled() => None,
        placed = workspaces::place(shared, &row, spec.cwd.clone(), spec.sandbox, opted_in) => {
            Some(placed)
        }
    };
    match placed {
        None => {
            let why = "cancelled while the workspace was being prepared".to_owned();
            let stop = StopReason::Interrupted;
            let reason = cancelled_for(shared, &cause);
            return conclude(shared, &run_id, fence, stop, why, reason, cmd_rx, slot).await;
        }
        Some(Err(error)) => {
            let why = format!("WorkspaceRefused: {error}");
            let refusal = StartRefusal::WorkspaceRefused;
            // A directory that is gone needs a person, like a signed-out account: the trigger
            // backs off. The other refusals are about this run and do not.
            let account = matches!(error, crate::WorkspaceError::DirMissing { .. })
                .then_some(StartFailure::WorkingDirMissing);
            return fail_start(shared, &run_id, fence, why, refusal, account, cmd_rx, slot).await;
        }
        Some(Ok(placement)) => spec.cwd = placement.cwd(),
    }
    // Reno's own MCP server, with a token for this run (T1.9).
    if let Err(why) = agent::provision(shared, &row, settings.wall_time, &mut spec).await {
        let refusal = StartRefusal::Invalid;
        return fail_start(shared, &run_id, fence, why, refusal, None, cmd_rx, slot).await;
    }
    let mut info = RunInfo {
        executor: row.executor.clone(),
        cwd: spec.cwd.clone(),
        bot: row.bot.as_ref().map(|pin| pin.name.clone()),
        wall_time: settings.wall_time,
        token_ceiling: settings.token_ceiling,
        redactor: redactor.clone(),
        // This claim is the attempt after the one that ended.
        resumed: spec.resume.clone().map(|session| {
            (
                u32::try_from(fence.saturating_sub(1)).unwrap_or(u32::MAX),
                session,
            )
        }),
    };

    // The fallback walk (ARCHITECTURE 5.2): try the executor the router chose; when it will not
    // start for an account or install reason, mark it rejected in the stored decision and try
    // the next ranked candidate, within this attempt. Each step removes one candidate from the
    // ranking, so the walk ends after at most as many steps as there were candidates.
    // A resumed run goes back to the executor whose session it holds: there is no fallback walk.
    let mut decision = shared
        .db
        .run_routing(&run_id)
        .await
        .ok()
        .flatten()
        .filter(|_| !resuming);
    let mut current = row.executor.clone();
    let mut refusals: Vec<(ExecutorId, String)> = Vec::new();
    // A task that named nothing and has no default to go to (the router left every executor out
    // as not a default): nothing is started, whatever the executor it is recorded against.
    if let Some(left_out) = decision.as_ref().filter(|d| {
        d.chosen.is_none()
            && d.rejected
                .iter()
                .any(|r| r.executor == current && matches!(r.reason, RejectReason::Excluded { .. }))
    }) {
        let why = format!(
            "NoEligibleExecutor: no default executor can take the task ({}); {NAME_ONE}",
            left_out.explain_verbose()
        );
        let account = Some(StartFailure::NoEligibleExecutor);
        let refusal = StartRefusal::NoEligibleExecutor;
        return fail_start(shared, &run_id, fence, why, refusal, account, cmd_rx, slot).await;
    }
    let (stream, handle) = loop {
        match attempt(shared, &cancel, &current, &spec, redactor.as_deref()).await {
            Attempt::Started(stream, handle) => {
                if let Some(decision) = decision.as_mut()
                    && decision.chosen.is_none()
                {
                    adopt(decision, &current);
                    reroute(shared, &run_id, fence, &current, decision).await;
                }
                break (stream, handle);
            }
            Attempt::Cancelled => {
                // Nothing ran, so it is `interrupted`.
                let why = "cancelled while the executor was starting".to_owned();
                let stop = StopReason::Interrupted;
                let reason = cancelled_for(shared, &cause);
                return conclude(shared, &run_id, fence, stop, why, reason, cmd_rx, slot).await;
            }
            Attempt::Failed {
                why,
                refusal,
                account,
            } => {
                return fail_start(shared, &run_id, fence, why, refusal, account, cmd_rx, slot)
                    .await;
            }
            Attempt::Declined {
                why,
                reason,
                account,
            } => {
                tracing::warn!(run = %run_id, executor = %current, %why, "executor declined the run");
                refusals.push((current.clone(), why.clone()));
                let next = match decision.as_mut() {
                    Some(decision) if decision.chosen.as_ref() == Some(&current) => {
                        let at = u64::try_from(shared.db.now_ms()).unwrap_or(0);
                        router::fall_back(decision, &current, &reason, &why, at);
                        let next = decision.chosen.clone();
                        reroute(
                            shared,
                            &run_id,
                            fence,
                            next.as_ref().unwrap_or(&current),
                            decision,
                        )
                        .await;
                        next
                    }
                    _ => None,
                };
                match next {
                    Some(next) => current = next,
                    None => {
                        let mut why = refused_by_all(&refusals);
                        if decision
                            .as_ref()
                            .is_some_and(|d| d.rule_matched == RuleMatched::Default)
                        {
                            why = format!("{why}; {NAME_ONE}");
                        }
                        let refusal = refusal_of(account);
                        let account = Some(account);
                        return fail_start(
                            shared, &run_id, fence, why, refusal, account, cmd_rx, slot,
                        )
                        .await;
                    }
                }
            }
        }
    };
    shared
        .signal_scheduler(SchedCmd::RunStarted {
            run: run_id.clone(),
        })
        .await;
    info.executor = current;
    let supervisor = Supervisor::new(
        Arc::clone(shared),
        run_id,
        fence,
        info,
        stream,
        handle,
        cancel,
        cause,
        cmd_rx,
        slot,
    );
    shared.spawn(supervisor.run());
}

/// Stores `decision` and the executor the run now stands on. A failure is logged: the walk
/// goes on, and the diagnostic of the run still says what happened.
async fn reroute(
    shared: &Shared,
    run: &RunId,
    fence: u64,
    executor: &ExecutorId,
    decision: &RoutingDecision,
) {
    if let Err(error) = shared.db.reroute_run(run, fence, executor, decision).await {
        tracing::error!(%run, %error, "could not store the routing decision");
    }
}

/// The typed refusal an account-level start failure is.
fn refusal_of(failure: StartFailure) -> StartRefusal {
    match failure {
        StartFailure::NotInstalled => StartRefusal::NotInstalled,
        StartFailure::NotSignedIn => StartRefusal::NotSignedIn,
        StartFailure::NoEligibleExecutor => StartRefusal::NoEligibleExecutor,
        StartFailure::SecretMissing => StartRefusal::SecretMissing,
        StartFailure::WorkingDirMissing => StartRefusal::WorkspaceRefused,
    }
}

/// Why a run that was cancelled before its executor ran ended: the reason its canceller gave, or
/// the shutdown, or (nobody said) a person's cancel.
fn cancelled_for(shared: &Shared, cause: &StopCause) -> StopDetail {
    cause.get().unwrap_or_else(|| {
        if shared.root.is_cancelled() {
            StopDetail::Shutdown
        } else {
            StopDetail::Cancelled {
                by: reno_proto::CancelledBy::User,
            }
        }
    })
}

#[allow(clippy::too_many_arguments)]
async fn fail_start(
    shared: &Arc<Shared>,
    run: &RunId,
    fence: u64,
    why: String,
    refusal: StartRefusal,
    account: Option<StartFailure>,
    cmd_rx: mpsc::Receiver<crate::shared::SupCmd>,
    slot: Slot,
) {
    tracing::warn!(%run, %why, "run failed to start");
    let reason = StopDetail::StartRefused {
        failure: refusal,
        detail: why.clone(),
    };
    conclude(
        shared,
        run,
        fence,
        StopReason::Error,
        why,
        reason,
        cmd_rx,
        slot,
    )
    .await;
    // After the run's end has committed, so the trigger's next occurrence finds it finished.
    if let Some(why) = account {
        shared
            .signal_scheduler(SchedCmd::StartFailed {
                run: run.clone(),
                why,
            })
            .await;
    }
}

/// End a run that has no supervisor: commit its state, then journal its `Done`, then free the
/// slot (ARCH-8). `cmd_rx` stays open until the `Done` is written, so a subscriber that
/// registered in the meantime finds the finished journal when its request is dropped.
#[allow(clippy::too_many_arguments)]
async fn conclude(
    shared: &Arc<Shared>,
    run: &RunId,
    fence: u64,
    stop: StopReason,
    diagnostic: String,
    reason: StopDetail,
    cmd_rx: mpsc::Receiver<crate::shared::SupCmd>,
    slot: Slot,
) {
    // The run has no executor to speak for it any more: its token goes first.
    agent::revoke(shared, run).await;
    match lifecycle::end_run(
        &shared.db,
        run,
        fence,
        RunStatus::Running,
        stop,
        Some(&reason),
    )
    .await
    {
        Ok(_) => {
            workspaces::release_after_run(shared, run).await;
            let done = ExecEvent::Done {
                stop,
                result: None,
                diagnostic: Some(diagnostic),
                reason: Some(reason),
            };
            lifecycle::append_done(&shared.db, run, done).await;
        }
        Err(error) => tracing::error!(%run, %error, "could not commit the end of the run"),
    }
    drop(slot);
    drop(cmd_rx);
}
