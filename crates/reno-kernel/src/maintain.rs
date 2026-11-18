//! Installs and updates of executors (EX-1 to EX-12): the job registry, the rule that an executor
//! with a job takes no run, the queued update, and the daily auto-update.
//!
//! # What these jobs are
//!
//! A job runs commands the adapters compile in ([`reno_harness::exec::maintenance`]) through the
//! [`CommandRunner`] port, or the adapter's own `Executor::install`. **They run outside the
//! policy engine.** They are not agent actions: a person pressed Install or Update, or turned
//! auto-update on. An RPC names an executor and a kind, never a command, so nothing a client
//! sends reaches a shell. The runner gives them no stdin and no sudo (see the runner's docs).
//!
//! # Where the state lives
//!
//! The kernel does no I/O of its own (ARCH-7): the runner and the self-updater switch are ports
//! ([`MaintenancePorts`]), and the daemon gives the real ones, tests give fakes that panic on a
//! real spawn. Jobs, their output tails and the stored settings are in memory, loaded at attach
//! and published through the executors watch; the settings are written to SQLite first (ARCH-8).
//! A job itself does not survive a restart (its processes do not either: they die with the
//! daemon's process group).
//!
//! # Nothing polls (ARCH-10)
//!
//! One task waits for: a run or a job ending, a setting changing, the system waking, and the next
//! daily deadline (the earliest `last check + 1 day` of the executors that auto-update). It
//! re-reads the clock on each wake. The first daily check of a daemon that has no stored check is
//! a day after it first saw the executor, never at start-up: an update is never a surprise.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use reno_harness::HarnessError;
use reno_harness::exec::Executor;
use reno_harness::exec::maintenance::{MaintenanceDecl, UpdaterDecl, is_newer};
use reno_harness::proc::maintain::{LogTail, Outcome, Progress, run_capture, run_steps};
use reno_harness::proc::self_update::SelfUpdateSwitch;
use reno_proto::{
    AutoUpdate, ExecutorHealth, ExecutorId, ExecutorInfo, ExecutorJob, InstallStatus, JobFailure,
    JobKind, JobPhase, Maintenance, NotInstalledReason, UpdaterInfo,
};
use reno_store::ExecutorSetting;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub use reno_harness::proc::maintain::{CommandRunner, Limits};
pub use reno_harness::proc::self_update::SelfUpdateControl;

use crate::error::{KernelError, Result};
use crate::health::Probing;
use crate::shared::Shared;
use crate::wake::system_woke;

/// The auto-update check of an executor runs this often.
const DAY_MS: i64 = 24 * 60 * 60 * 1000;
/// An update check (`brew outdated`, `npm view`) is given this long.
const CHECK_BUDGET: Duration = Duration::from_secs(60);
/// Lines of output are published at most this often: a noisy installer is one update per gap.
const PUBLISH_GAP: Duration = Duration::from_millis(100);

/// What the kernel needs from the world to install and update (ARCH-7).
#[derive(Debug, Clone)]
pub struct MaintenancePorts {
    /// Runs a command line as the user.
    pub runner: Arc<dyn CommandRunner>,
    /// Moves a self-updating CLI's own updater switch.
    pub self_update: Arc<dyn SelfUpdateControl>,
    /// The quiet notice and the ceiling of a job.
    pub limits: Limits,
}

/// What a started job does.
#[derive(Debug, Clone)]
enum Work {
    /// Shell steps, in order.
    Steps(Vec<String>),
    /// The adapter's own `Executor::install`.
    Managed,
}

impl Work {
    fn steps(&self) -> u32 {
        match self {
            Self::Steps(steps) => u32::try_from(steps.len()).unwrap_or(u32::MAX),
            Self::Managed => 1,
        }
    }
}

struct JobState {
    id: String,
    kind: JobKind,
    phase: JobPhase,
    started_at_ms: u64,
    cancel: CancellationToken,
    work: Work,
    /// When the last line was printed, for `quiet_since_ms`.
    last_output_ms: u64,
}

impl JobState {
    fn live(&self) -> bool {
        !matches!(self.phase, JobPhase::Failed { .. })
    }

    fn wire(&self) -> ExecutorJob {
        ExecutorJob {
            id: self.id.clone(),
            kind: self.kind,
            phase: self.phase.clone(),
            started_at_ms: self.started_at_ms,
        }
    }
}

/// An adapter's declaration resolved against where its binary was found.
struct Resolved {
    decl: MaintenanceDecl,
    /// Index into `decl.updaters`.
    updater: Option<usize>,
}

impl Resolved {
    fn updater(&self) -> Option<&UpdaterDecl> {
        self.updater.and_then(|at| self.decl.updaters.get(at))
    }
}

struct Latest {
    version: String,
    at_ms: u64,
}

#[derive(Default)]
struct State {
    settings: HashMap<ExecutorId, ExecutorSetting>,
    resolved: HashMap<ExecutorId, (Option<PathBuf>, Option<Arc<Resolved>>)>,
    latest: HashMap<ExecutorId, Latest>,
    jobs: HashMap<ExecutorId, JobState>,
    logs: HashMap<ExecutorId, LogTail>,
    /// Runs standing on each executor.
    runs: HashMap<ExecutorId, u32>,
    /// Executors whose daily check is running.
    checking: std::collections::HashSet<ExecutorId>,
    next_job: u64,
}

/// The job registry. One per kernel; see the module docs.
pub(crate) struct Maintainer {
    ports: MaintenancePorts,
    state: Mutex<State>,
    /// Something the maintenance task should look at again.
    changed: Notify,
    /// Output lines are waiting to be published.
    dirty: Notify,
}

impl std::fmt::Debug for Maintainer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Maintainer").finish_non_exhaustive()
    }
}

/// A run's place on an executor. While it is held an update of the executor waits (or is
/// forced), and dropping it tells the maintenance task.
#[derive(Debug)]
pub(crate) struct RunClaim {
    maintainer: Arc<Maintainer>,
    executor: ExecutorId,
}

impl Drop for RunClaim {
    fn drop(&mut self) {
        {
            let mut state = self.maintainer.state();
            if let Some(runs) = state.runs.get_mut(&self.executor) {
                *runs = runs.saturating_sub(1);
            }
        }
        self.maintainer.changed.notify_one();
    }
}

fn now_ms(shared: &Shared) -> u64 {
    u64::try_from(shared.db.now_ms()).unwrap_or(0)
}

impl Shared {
    /// Why no run may start on `id` now: an install or update of it is running. `None` otherwise.
    pub(crate) fn busy_reason(&self, id: &ExecutorId) -> Option<String> {
        self.maint.get()?.busy_reason(id)
    }

    /// Takes a run's place on `id`. `Err` with the reason when a job of it is running (EX-4);
    /// the check and the claim are one step, so a job cannot start between them.
    pub(crate) fn claim_run(
        &self,
        id: &ExecutorId,
    ) -> std::result::Result<Option<RunClaim>, String> {
        let Some(maintainer) = self.maint.get() else {
            return Ok(None);
        };
        let mut state = maintainer.state();
        if let Some(why) = busy(&state, id) {
            return Err(why);
        }
        *state.runs.entry(id.clone()).or_default() += 1;
        Ok(Some(RunClaim {
            maintainer: Arc::clone(maintainer),
            executor: id.clone(),
        }))
    }
}

fn busy(state: &State, id: &ExecutorId) -> Option<String> {
    let job = state.jobs.get(id)?;
    matches!(job.phase, JobPhase::Running { .. }).then(|| {
        let what = match job.kind {
            JobKind::Install => "install",
            JobKind::Update => "update",
        };
        format!("an {what} of {id} is running; try again when it ends")
    })
}

impl Maintainer {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Tells the maintenance task that something it looks at changed.
    pub(crate) fn wake(&self) {
        self.changed.notify_one();
    }

    fn busy_reason(&self, id: &ExecutorId) -> Option<String> {
        busy(&self.state(), id)
    }

    fn setting(state: &State, id: &ExecutorId) -> ExecutorSetting {
        state
            .settings
            .get(id)
            .cloned()
            .unwrap_or_else(|| ExecutorSetting::defaults(id.clone()))
    }

    /// The declaration of `id` resolved against the path its probe found, cached by that path.
    fn resolved(
        &self,
        shared: &Shared,
        id: &ExecutorId,
        health: Option<&ExecutorHealth>,
    ) -> Option<Arc<Resolved>> {
        let path = match health.map(|h| &h.installed) {
            Some(InstallStatus::Installed { path, .. }) => Some(path.clone()),
            _ => None,
        };
        let mut state = self.state();
        if let Some((cached, resolved)) = state.resolved.get(id)
            && *cached == path
        {
            return resolved.clone();
        }
        let resolved = shared.executors.get(id).and_then(|executor| {
            let decl = executor.maintenance()?;
            let updater = decl
                .updater_for(path.as_deref())
                .and_then(|found| decl.updaters.iter().position(|u| u == found));
            Some(Arc::new(Resolved { decl, updater }))
        });
        state.resolved.insert(id.clone(), (path, resolved.clone()));
        resolved
    }

    /// Fills in the maintenance and the job of `info` (called as the list is published).
    pub(crate) fn decorate(&self, shared: &Shared, info: &mut ExecutorInfo) {
        let id = info.descriptor.id.clone();
        let resolved = self.resolved(shared, &id, info.health.as_ref());
        let state = self.state();
        info.job = state.jobs.get(&id).map(JobState::wire);
        let Some(resolved) = resolved else { return };
        let installed = matches!(
            info.health.as_ref().map(|h| &h.installed),
            Some(InstallStatus::Installed { .. })
        );
        let setting = Self::setting(&state, &id);
        let latest = state.latest.get(&id);
        info.maintenance = Some(Maintenance {
            setup: (!installed).then(|| resolved.decl.setup.clone()).flatten(),
            sign_in: resolved.decl.sign_in.clone(),
            updater: installed
                .then(|| resolved.updater())
                .flatten()
                .map(|updater| UpdaterInfo {
                    command: updater.command.to_owned(),
                    self_updating: updater.self_updating,
                    can_pin: updater.pinned.is_some(),
                    latest: latest.map(|l| l.version.clone()),
                    checked_at_ms: latest.map(|l| l.at_ms),
                }),
            managed: resolved.decl.managed,
            auto_update: AutoUpdate {
                on: setting.auto_update,
                verified_only: setting.verified_only,
            },
        });
    }
}

fn attached(shared: &Shared) -> Result<Arc<Maintainer>> {
    shared
        .maint
        .get()
        .cloned()
        .ok_or(KernelError::MaintenanceOff)
}

/// Starts the maintenance task and the publisher, and loads the stored settings.
pub(crate) async fn attach(shared: &Arc<Shared>, ports: MaintenancePorts) -> Result<()> {
    let stored = shared.db.executor_settings().await?;
    let maintainer = Arc::new(Maintainer {
        ports,
        state: Mutex::new(State {
            settings: stored
                .into_iter()
                .map(|s| (s.executor.clone(), s))
                .collect(),
            ..State::default()
        }),
        changed: Notify::new(),
        dirty: Notify::new(),
    });
    if shared.maint.set(Arc::clone(&maintainer)).is_err() {
        return Ok(());
    }
    // An executor Reno has no stored check for is first checked a day from now, not at start-up.
    let now = shared.db.now_ms();
    let ids: Vec<ExecutorId> = shared.executors.ids().cloned().collect();
    for id in ids {
        let Some(executor) = shared.executors.get(&id) else {
            continue;
        };
        if executor.maintenance().is_none_or(|d| d.updaters.is_empty()) {
            continue;
        }
        let missing = maintainer
            .state()
            .settings
            .get(&id)
            .is_none_or(|s| s.last_checked_ms.is_none());
        if missing {
            shared.db.put_executor_checked(&id, now).await?;
            let mut state = maintainer.state();
            let row = state
                .settings
                .entry(id.clone())
                .or_insert_with(|| ExecutorSetting::defaults(id.clone()));
            row.last_checked_ms = Some(now);
        }
    }
    shared.publish_executors();
    let publisher = Arc::clone(shared);
    shared.spawn(async move { publish_loop(publisher).await });
    let looped = Arc::clone(shared);
    shared.spawn(async move { maintenance_loop(looped).await });
    Ok(())
}

/// Publishes output lines, coalesced.
async fn publish_loop(shared: Arc<Shared>) {
    let Some(maintainer) = shared.maint.get().cloned() else {
        return;
    };
    loop {
        tokio::select! {
            () = shared.root.cancelled() => return,
            () = maintainer.dirty.notified() => {}
        }
        shared.publish_executors();
        tokio::select! {
            () = shared.root.cancelled() => return,
            () = tokio::time::sleep(PUBLISH_GAP) => {}
        }
    }
}

/// The one task that looks at the registry when something happened or a deadline passed.
async fn maintenance_loop(shared: Arc<Shared>) {
    let Some(maintainer) = shared.maint.get().cloned() else {
        return;
    };
    let mut woke = shared.system_wake.as_ref().map(|wake| wake.subscribe());
    loop {
        promote_queued(&shared, &maintainer);
        let next = daily_round(&shared, &maintainer);
        let deadline = async {
            match next {
                Some(wait) => tokio::time::sleep(wait).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            biased;
            () = shared.root.cancelled() => return,
            () = maintainer.changed.notified() => {}
            () = system_woke(&mut woke) => {}
            () = deadline => {}
        }
    }
}

/// What an update or install of `id` would run, decided from the health the board holds now.
struct Plan {
    work: Work,
    kind: JobKind,
}

enum Refused {
    NothingToDo(String),
    NoUpdater,
}

fn plan(
    kind: JobKind,
    resolved: Option<&Resolved>,
    health: &ExecutorHealth,
    setting: &ExecutorSetting,
    latest: Option<&str>,
    verified: &str,
    pin_wanted: bool,
) -> std::result::Result<Plan, Refused> {
    let version = health.installed.version().map(str::to_owned);
    match kind {
        JobKind::Install => match &health.installed {
            InstallStatus::Installed { .. } => {
                Err(Refused::NothingToDo("it is already installed".into()))
            }
            InstallStatus::NotInstalled {
                why: NotInstalledReason::NeedsInstall { .. },
            } => Ok(Plan {
                work: Work::Managed,
                kind,
            }),
            InstallStatus::NotInstalled {
                why: NotInstalledReason::NotFound { .. },
            } => {
                let steps: Vec<String> = resolved
                    .and_then(|r| r.decl.setup.as_ref())
                    .map(|recipe| {
                        recipe
                            .steps
                            .iter()
                            .filter(|s| s.runner == reno_proto::StepRunner::Reno)
                            .map(|s| s.command.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                if steps.is_empty() {
                    Err(Refused::NothingToDo(
                        "Reno has no way to install it; follow the hint".into(),
                    ))
                } else {
                    Ok(Plan {
                        work: Work::Steps(steps),
                        kind,
                    })
                }
            }
            InstallStatus::NotInstalled {
                why: NotInstalledReason::Unusable { detail },
            } => Err(Refused::NothingToDo(format!(
                "what is there cannot be launched: {detail}"
            ))),
            InstallStatus::NotInstalled {
                why: NotInstalledReason::ProbeFailed { detail },
            } => Err(Refused::NothingToDo(format!("its probe failed: {detail}"))),
        },
        JobKind::Update => {
            if !matches!(health.installed, InstallStatus::Installed { .. }) {
                return Err(Refused::NothingToDo("it is not installed".into()));
            }
            let updater = resolved
                .and_then(Resolved::updater)
                .ok_or(Refused::NoUpdater)?;
            let pin = if pin_wanted && setting.verified_only && updater.pinned.is_some() {
                // Verified only, and the updater can: go to the version the adapter was verified
                // on, and never past it (or back to it from a newer one).
                if !is_newer(verified, "0.0.0") {
                    return Err(Refused::NothingToDo(
                        "Reno has no verified version of it to pin to".into(),
                    ));
                }
                if version.as_deref().is_some_and(|v| !is_newer(verified, v)) {
                    return Err(Refused::NothingToDo(format!(
                        "it is already at or past {verified}, the newest version Reno has verified"
                    )));
                }
                Some(verified)
            } else {
                if let (Some(latest), Some(version)) = (latest, version.as_deref())
                    && !is_newer(latest, version)
                {
                    return Err(Refused::NothingToDo("it is up to date".into()));
                }
                None
            };
            let steps = updater.update_steps(pin).ok_or_else(|| {
                Refused::NothingToDo("its updater cannot pin that version".into())
            })?;
            Ok(Plan {
                work: Work::Steps(steps),
                kind,
            })
        }
    }
}

fn refusal(id: &ExecutorId, why: Refused) -> KernelError {
    match why {
        Refused::NothingToDo(why) => KernelError::NothingToDo {
            executor: id.clone(),
            why,
        },
        Refused::NoUpdater => KernelError::NoUpdater(id.clone()),
    }
}

/// How a job of `id` begins.
enum Begin {
    /// Running: the caller spawns its task.
    Run {
        job: String,
        cancel: CancellationToken,
    },
    /// Waiting for runs to end.
    Queued,
}

fn begin(
    shared: &Shared,
    maintainer: &Maintainer,
    id: &ExecutorId,
    plan: Plan,
    force: bool,
    idle_only: bool,
) -> Result<Begin> {
    let now = now_ms(shared);
    let mut state = maintainer.state();
    if state.jobs.get(id).is_some_and(JobState::live) {
        return Err(KernelError::JobRunning(id.clone()));
    }
    let runs = state.runs.get(id).copied().unwrap_or(0);
    if runs > 0 && idle_only {
        return Err(KernelError::NothingToDo {
            executor: id.clone(),
            why: "it has runs in flight".into(),
        });
    }
    let waiting = if force || plan.kind == JobKind::Install {
        0
    } else {
        runs
    };
    state.next_job += 1;
    let job_id = format!("job-{now}-{}", state.next_job);
    let steps = plan.work.steps();
    let phase = if waiting > 0 {
        JobPhase::Queued {
            waiting_runs: waiting,
        }
    } else {
        JobPhase::Running {
            step: 1,
            steps,
            last_line: None,
            quiet_since_ms: None,
        }
    };
    let cancel = shared.root.child_token();
    state.logs.insert(id.clone(), LogTail::default());
    state.jobs.insert(
        id.clone(),
        JobState {
            id: job_id.clone(),
            kind: plan.kind,
            phase,
            started_at_ms: now,
            cancel: cancel.clone(),
            work: plan.work,
            last_output_ms: now,
        },
    );
    Ok(if waiting > 0 {
        Begin::Queued
    } else {
        Begin::Run {
            job: job_id,
            cancel,
        }
    })
}

/// Starts or queues a job. `pin_wanted`: the update goes to the verified version when the
/// person turned "verified only" on.
async fn start(
    shared: &Arc<Shared>,
    id: &ExecutorId,
    kind: JobKind,
    force: bool,
    idle_only: bool,
) -> Result<ExecutorInfo> {
    let maintainer = attached(shared)?;
    let executor = shared
        .executors
        .get(id)
        .cloned()
        .ok_or_else(|| KernelError::ExecutorNotFound(id.clone()))?;
    let mut health = shared.any_health(id);
    if health.is_none() {
        shared.probe_executor(id, Probing::Shared).await?;
        health = shared.any_health(id);
    }
    let health = health.ok_or_else(|| KernelError::NothingToDo {
        executor: id.clone(),
        why: "it has not been probed yet".into(),
    })?;
    let resolved = maintainer.resolved(shared, id, Some(&health));
    let (setting, latest) = {
        let state = maintainer.state();
        (
            Maintainer::setting(&state, id),
            state.latest.get(id).map(|l| l.version.clone()),
        )
    };
    let verified = executor.descriptor().cli_verified.verified;
    let plan = plan(
        kind,
        resolved.as_deref(),
        &health,
        &setting,
        latest.as_deref(),
        &verified,
        true,
    )
    .map_err(|why| refusal(id, why))?;
    let started = begin(shared, &maintainer, id, plan, force, idle_only)?;
    if let Begin::Run { job, cancel } = started {
        spawn_job(shared, &maintainer, id, job, cancel);
    }
    shared.publish_executors();
    maintainer.changed.notify_one();
    shared
        .published(id)
        .ok_or_else(|| KernelError::ExecutorNotFound(id.clone()))
}

fn spawn_job(
    shared: &Arc<Shared>,
    maintainer: &Arc<Maintainer>,
    id: &ExecutorId,
    job: String,
    cancel: CancellationToken,
) {
    let shared = Arc::clone(shared);
    let maintainer = Arc::clone(maintainer);
    let id = id.clone();
    let task = Arc::clone(&shared);
    task.spawn(async move { run_job(shared, maintainer, id, job, cancel).await });
}

/// Moves queued updates whose executor has no runs left to running.
fn promote_queued(shared: &Arc<Shared>, maintainer: &Arc<Maintainer>) {
    let mut started = Vec::new();
    let mut changed = false;
    {
        let mut state = maintainer.state();
        let queued: Vec<ExecutorId> = state
            .jobs
            .iter()
            .filter(|(_, job)| matches!(job.phase, JobPhase::Queued { .. }))
            .map(|(id, _)| id.clone())
            .collect();
        for id in queued {
            let runs = state.runs.get(&id).copied().unwrap_or(0);
            let Some(job) = state.jobs.get_mut(&id) else {
                continue;
            };
            if runs > 0 {
                let now = JobPhase::Queued { waiting_runs: runs };
                if job.phase != now {
                    job.phase = now;
                    changed = true;
                }
                continue;
            }
            job.phase = JobPhase::Running {
                step: 1,
                steps: job.work.steps(),
                last_line: None,
                quiet_since_ms: None,
            };
            started.push((id, job.id.clone(), job.cancel.clone()));
            changed = true;
        }
    }
    for (id, job, cancel) in started {
        spawn_job(shared, maintainer, &id, job, cancel);
    }
    if changed {
        shared.publish_executors();
    }
}

/// Runs a job to its end, then probes the executor again.
async fn run_job(
    shared: Arc<Shared>,
    maintainer: Arc<Maintainer>,
    id: ExecutorId,
    job: String,
    cancel: CancellationToken,
) {
    let (work, kind) = {
        let state = maintainer.state();
        match state.jobs.get(&id) {
            Some(j) if j.id == job => (j.work.clone(), j.kind),
            _ => return,
        }
    };
    tracing::info!(executor = %id, ?kind, "starting an install or update job");
    let limits = maintainer.ports.limits;
    let outcome = match work {
        Work::Steps(steps) => {
            let on_progress = |progress: Progress| apply(&shared, &maintainer, &id, &job, progress);
            run_steps(
                maintainer.ports.runner.as_ref(),
                &steps,
                limits,
                &cancel,
                on_progress,
            )
            .await
        }
        Work::Managed => managed(&shared, &maintainer, &id, &job, &cancel, limits).await,
    };
    finish(&shared, &maintainer, &id, &job, kind, outcome).await;
}

async fn managed(
    shared: &Arc<Shared>,
    maintainer: &Arc<Maintainer>,
    id: &ExecutorId,
    job: &str,
    cancel: &CancellationToken,
    limits: Limits,
) -> Outcome {
    let Some(executor) = shared.executors.get(id).cloned() else {
        return Outcome::Failed {
            failure: JobFailure::Broken,
            detail: format!("executor {id} is not registered"),
        };
    };
    apply(
        shared,
        maintainer,
        id,
        job,
        Progress::Step {
            n: 1,
            of: 1,
            command: format!("install {id}'s adapter"),
        },
    );
    let installing = install_managed(executor);
    tokio::select! {
        biased;
        () = cancel.cancelled() => Outcome::Failed {
            failure: JobFailure::Cancelled,
            detail: "cancelled".into(),
        },
        done = tokio::time::timeout(limits.ceiling, installing) => match done {
            Err(_) => Outcome::Failed {
                failure: JobFailure::TimedOut,
                detail: format!("still running after {} min, so it was stopped", limits.ceiling.as_secs() / 60),
            },
            Ok(Ok(())) => Outcome::Done,
            Ok(Err(HarnessError::Install(detail) | HarnessError::Protocol(detail))) => {
                let failure = if detail.contains("stalled") {
                    JobFailure::Stalled
                } else {
                    JobFailure::ExitCode
                };
                Outcome::Failed { failure, detail }
            }
            Ok(Err(other)) => Outcome::Failed {
                failure: JobFailure::Broken,
                detail: other.to_string(),
            },
        },
    }
}

async fn install_managed(executor: Arc<dyn Executor>) -> std::result::Result<(), HarnessError> {
    executor.install().await
}

/// Applies a line of progress to the job, if it is still the one running.
fn apply(
    shared: &Arc<Shared>,
    maintainer: &Arc<Maintainer>,
    id: &ExecutorId,
    job: &str,
    progress: Progress,
) {
    let now = now_ms(shared);
    let immediate = !matches!(progress, Progress::Line(_));
    {
        let mut state = maintainer.state();
        let State { jobs, logs, .. } = &mut *state;
        let Some(current) = jobs.get_mut(id).filter(|j| j.id == job && j.live()) else {
            return;
        };
        let log = logs.entry(id.clone()).or_default();
        match progress {
            Progress::Step { n, of, command } => {
                log.push(format!("$ {command}"));
                current.last_output_ms = now;
                current.phase = JobPhase::Running {
                    step: n,
                    steps: of,
                    last_line: None,
                    quiet_since_ms: None,
                };
            }
            Progress::Line(line) => {
                log.push(line.clone());
                current.last_output_ms = now;
                if let JobPhase::Running {
                    last_line,
                    quiet_since_ms,
                    ..
                } = &mut current.phase
                {
                    *last_line = Some(line);
                    *quiet_since_ms = None;
                }
            }
            Progress::Quiet => {
                let since = current.last_output_ms;
                if let JobPhase::Running { quiet_since_ms, .. } = &mut current.phase {
                    *quiet_since_ms = Some(since);
                }
            }
        }
    }
    if immediate {
        shared.publish_executors();
    } else {
        maintainer.dirty.notify_one();
    }
}

/// Ends a job: records the failure (or removes the job), probes the executor again and tells the
/// maintenance task.
async fn finish(
    shared: &Arc<Shared>,
    maintainer: &Arc<Maintainer>,
    id: &ExecutorId,
    job: &str,
    kind: JobKind,
    outcome: Outcome,
) {
    let now = now_ms(shared);
    if let Outcome::Failed { failure, detail } = &outcome {
        let mut state = maintainer.state();
        let State { jobs, logs, .. } = &mut *state;
        if let Some(current) = jobs.get_mut(id).filter(|j| j.id == job && j.live()) {
            logs.entry(id.clone())
                .or_default()
                .push(format!("error: {detail}"));
            current.phase = JobPhase::Failed {
                failure: *failure,
                detail: detail.clone(),
                finished_at_ms: now,
            };
        }
    }
    shared.publish_executors();
    // Whatever happened, the health shown is what the executor is now.
    if let Err(error) = shared.probe_executor(id, Probing::Fresh).await {
        tracing::debug!(executor = %id, %error, "the probe after a job ended early");
    }
    {
        let health = shared.any_health(id);
        let mut state = maintainer.state();
        let still_ours = state.jobs.get(id).is_some_and(|j| j.id == job);
        if still_ours && matches!(outcome, Outcome::Done) {
            let found = health
                .as_ref()
                .is_some_and(|h| matches!(h.installed, InstallStatus::Installed { .. }));
            if kind == JobKind::Install && !found {
                let detail = format!("the steps finished, but {id} is still not found");
                if let Some(current) = state.jobs.get_mut(id) {
                    current.phase = JobPhase::Failed {
                        failure: JobFailure::Probe,
                        detail,
                        finished_at_ms: now_ms(shared),
                    };
                }
            } else {
                state.jobs.remove(id);
            }
        }
    }
    shared.publish_executors();
    maintainer.changed.notify_one();
}

/// The daily round: starts the check of every executor whose day is up and that is idle, and
/// returns how long until the next one is due.
fn daily_round(shared: &Arc<Shared>, maintainer: &Arc<Maintainer>) -> Option<Duration> {
    let now = shared.db.now_ms();
    let mut next: Option<i64> = None;
    let mut due: Vec<ExecutorId> = Vec::new();
    let mut ids: Vec<ExecutorId> = shared.executors.ids().cloned().collect();
    ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    for id in ids {
        let health = shared.any_health(&id);
        let resolved = maintainer.resolved(shared, &id, health.as_ref());
        let installed = matches!(
            health.as_ref().map(|h| &h.installed),
            Some(InstallStatus::Installed { .. })
        );
        if !installed || resolved.as_deref().and_then(Resolved::updater).is_none() {
            continue;
        }
        let state = maintainer.state();
        let setting = Maintainer::setting(&state, &id);
        if !setting.auto_update || state.checking.contains(&id) {
            continue;
        }
        let at = setting.last_checked_ms.unwrap_or(now) + DAY_MS;
        if at <= now {
            let idle = state.runs.get(&id).copied().unwrap_or(0) == 0
                && !state.jobs.get(&id).is_some_and(JobState::live);
            // Not idle: it is looked at again when a run or a job ends, not on a timer.
            if idle {
                due.push(id);
            }
        } else {
            next = Some(next.map_or(at, |n| n.min(at)));
        }
    }
    for id in due {
        maintainer.state().checking.insert(id.clone());
        let shared = Arc::clone(shared);
        let maintainer = Arc::clone(maintainer);
        let task = Arc::clone(&shared);
        task.spawn(async move {
            daily_one(&shared, &maintainer, &id).await;
            maintainer.state().checking.remove(&id);
            maintainer.changed.notify_one();
        });
    }
    next.map(|at| Duration::from_millis(u64::try_from(at - now).unwrap_or(0)))
}

/// The daily check of one executor: records that it ran, learns the newest version, and, for an
/// executor that does not update itself, starts the update if it is idle and there is one.
async fn daily_one(shared: &Arc<Shared>, maintainer: &Arc<Maintainer>, id: &ExecutorId) {
    let now = shared.db.now_ms();
    if let Err(error) = shared.db.put_executor_checked(id, now).await {
        tracing::warn!(executor = %id, %error, "could not record the daily check");
    }
    {
        let mut state = maintainer.state();
        state
            .settings
            .entry(id.clone())
            .or_insert_with(|| ExecutorSetting::defaults(id.clone()))
            .last_checked_ms = Some(now);
    }
    let Some(health) = shared.any_health(id) else {
        return;
    };
    let resolved = maintainer.resolved(shared, id, Some(&health));
    let Some(updater) = resolved.as_deref().and_then(Resolved::updater) else {
        return;
    };
    let self_updating = updater.self_updating;
    refresh_latest(shared, maintainer, id, updater, &health).await;
    if self_updating {
        // The CLI updates itself; Reno only keeps `latest` fresh.
        return;
    }
    let setting = Maintainer::setting(&maintainer.state(), id);
    let verified = shared
        .executors
        .get(id)
        .map(|e| e.descriptor().cli_verified.verified)
        .unwrap_or_default();
    let latest = maintainer.state().latest.get(id).map(|l| l.version.clone());
    let Some(installed) = health.installed.version().map(str::to_owned) else {
        return;
    };
    // The update, if there is one: to the verified version when asked, else to the newest.
    let target = if setting.verified_only {
        if updater.pinned.is_none() {
            tracing::info!(executor = %id, "auto-update is verified only and the updater cannot pin; skipped");
            return;
        }
        verified
    } else {
        match latest {
            Some(latest) => latest,
            None => return,
        }
    };
    if !is_newer(&target, &installed) {
        return;
    }
    match start(shared, id, JobKind::Update, false, true).await {
        Ok(_) => tracing::info!(executor = %id, "auto-update started"),
        Err(error) => tracing::info!(executor = %id, %error, "auto-update did not start"),
    }
}

/// Asks the executor's check command for the newest version and remembers the answer.
async fn refresh_latest(
    shared: &Arc<Shared>,
    maintainer: &Arc<Maintainer>,
    id: &ExecutorId,
    updater: &UpdaterDecl,
    health: &ExecutorHealth,
) {
    let Some(check) = updater.check else { return };
    let Some(lines) = run_capture(
        maintainer.ports.runner.as_ref(),
        check.command,
        CHECK_BUDGET,
    )
    .await
    else {
        tracing::info!(executor = %id, "the update check did not answer");
        return;
    };
    let installed = health.installed.version();
    let Some(version) = check.parse.latest(&lines.join("\n"), installed) else {
        tracing::info!(executor = %id, "the update check said nothing readable");
        return;
    };
    let at_ms = now_ms(shared);
    maintainer
        .state()
        .latest
        .insert(id.clone(), Latest { version, at_ms });
    shared.publish_executors();
}

// ---- what the kernel offers

impl crate::Kernel {
    /// Gives the kernel the means to install and update executors: the runner and the
    /// self-updater switch (ARCH-7). Loads the stored settings, then starts the task that
    /// promotes queued updates and runs the daily auto-update. Calling it again does nothing.
    pub async fn attach_maintenance(&self, ports: MaintenancePorts) -> Result<()> {
        attach(self.shared(), ports).await
    }

    /// `executor.job.start`: installs (an executor that is not found) or updates (an installed
    /// one). Answers once the job is queued or started, with the executor as it then is. An
    /// update waits for the executor's runs unless `force`.
    pub async fn start_job(
        &self,
        id: &ExecutorId,
        kind: JobKind,
        force: bool,
    ) -> Result<ExecutorInfo> {
        start(self.shared(), id, kind, force, false).await
    }

    /// `executor.job.cancel`: ends the job as cancelled and kills what it runs.
    pub async fn cancel_job(&self, id: &ExecutorId) -> Result<ExecutorInfo> {
        let shared = self.shared();
        let maintainer = attached(shared)?;
        self.known(id)?;
        let now = now_ms(shared);
        {
            let mut state = maintainer.state();
            let State { jobs, logs, .. } = &mut *state;
            let Some(job) = jobs.get_mut(id).filter(|j| j.live()) else {
                return Err(KernelError::NothingToDo {
                    executor: id.clone(),
                    why: "it has no install or update to cancel".into(),
                });
            };
            job.cancel.cancel();
            job.phase = JobPhase::Failed {
                failure: JobFailure::Cancelled,
                detail: "cancelled".into(),
                finished_at_ms: now,
            };
            logs.entry(id.clone()).or_default().push("cancelled".into());
        }
        shared.publish_executors();
        maintainer.changed.notify_one();
        self.published(id)
    }

    /// `executor.job.dismiss`: clears a failed job. A job that is not over stays, and is an error.
    pub async fn dismiss_job(&self, id: &ExecutorId) -> Result<ExecutorInfo> {
        let shared = self.shared();
        let maintainer = attached(shared)?;
        self.known(id)?;
        {
            let mut state = maintainer.state();
            if state.jobs.get(id).is_some_and(JobState::live) {
                return Err(KernelError::JobRunning(id.clone()));
            }
            state.jobs.remove(id);
        }
        shared.publish_executors();
        self.published(id)
    }

    /// `executor.job.log`: the last 500 lines of the current or last job, and whether older ones
    /// were dropped.
    pub fn job_log(&self, id: &ExecutorId) -> Result<(Vec<String>, bool)> {
        let maintainer = attached(self.shared())?;
        self.known(id)?;
        Ok(maintainer
            .state()
            .logs
            .get(id)
            .map(LogTail::snapshot)
            .unwrap_or_default())
    }

    /// `executor.auto_update.set`. Turning it off for a self-updating CLI turns the CLI's own
    /// updater off with it (EX-12); turning it on restores what was there.
    pub async fn set_auto_update(
        &self,
        id: &ExecutorId,
        on: bool,
        verified_only: bool,
    ) -> Result<ExecutorInfo> {
        let shared = self.shared();
        let maintainer = attached(shared)?;
        self.known(id)?;
        let health = shared.any_health(id);
        let resolved = maintainer.resolved(shared, id, health.as_ref());
        let decl = resolved
            .as_deref()
            .map(|r| &r.decl)
            .filter(|d| !d.updaters.is_empty())
            .ok_or_else(|| KernelError::NoUpdater(id.clone()))?;
        let switch: Option<SelfUpdateSwitch> = resolved
            .as_deref()
            .and_then(Resolved::updater)
            .or_else(|| decl.updaters.first())
            .filter(|u| u.self_updating)
            .and_then(|u| u.switch);
        let mut row = Maintainer::setting(&maintainer.state(), id);
        row.auto_update = on;
        row.verified_only = verified_only;
        let ports = maintainer.ports.self_update.clone();
        match (switch, on) {
            (Some(switch), false) if row.prev_self_update.is_none() => {
                // The intent first (ARCH-9): what the key holds is stored before it is changed.
                let record = blocking(move || ports.previous(switch)).await?;
                row.prev_self_update = Some(record);
                shared.db.put_executor_setting(&row).await?;
                let ports = maintainer.ports.self_update.clone();
                blocking(move || ports.turn_off(switch).map(drop)).await?;
            }
            (Some(switch), true) if row.prev_self_update.is_some() => {
                let record = row.prev_self_update.take().unwrap_or_default();
                blocking(move || ports.restore(switch, &record)).await?;
                shared.db.put_executor_setting(&row).await?;
            }
            _ => shared.db.put_executor_setting(&row).await?,
        }
        {
            let mut state = maintainer.state();
            let kept = state.settings.get(id).and_then(|s| s.last_checked_ms);
            row.last_checked_ms = kept;
            state.settings.insert(id.clone(), row);
        }
        shared.publish_executors();
        maintainer.changed.notify_one();
        self.published(id)
    }

    fn known(&self, id: &ExecutorId) -> Result<()> {
        self.shared()
            .executors
            .get(id)
            .map(|_| ())
            .ok_or_else(|| KernelError::ExecutorNotFound(id.clone()))
    }

    fn published(&self, id: &ExecutorId) -> Result<ExecutorInfo> {
        self.shared()
            .published(id)
            .ok_or_else(|| KernelError::ExecutorNotFound(id.clone()))
    }
}

/// Runs a blocking port call off the async threads.
async fn blocking<T: Send + 'static>(
    call: impl FnOnce() -> std::result::Result<T, reno_harness::proc::self_update::SelfUpdateError>
    + Send
    + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(call)
        .await
        .map_err(|e| KernelError::SelfUpdate(e.to_string()))?
        .map_err(|e| KernelError::SelfUpdate(e.to_string()))
}
