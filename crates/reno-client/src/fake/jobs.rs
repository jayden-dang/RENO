//! The fake daemon's executor jobs (`executor.job.*`, `executor.auto_update.set`).
//!
//! A job is simulated, never run: it walks its steps on tokio time (so tests pause the clock),
//! writes a few log lines per step and publishes each change through `executors.watch`. The
//! outcome is scripted: success updates the executor's health, and a failure a test queued with
//! [`super::FakeDaemon::fail_next_job`] ends it `failed`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use reno_proto::{
    ErrorCode, ErrorObject, ExecutorId, ExecutorJob, InstallStatus, JobFailure, JobKind, JobPhase,
    NotInstalledReason, SignedIn, StepRunner, version_gate,
};
use tokio_util::sync::CancellationToken;

use super::Shared;
use super::world::World;

/// The most lines of output kept per executor, as the real daemon keeps.
const LOG_LINES: usize = 500;
/// Lines each step writes: the command, then three of output.
const LINES_PER_STEP: u32 = 4;

/// What the fake remembers about jobs.
#[derive(Default)]
pub(super) struct Jobs {
    next: u64,
    /// Output of the current or last job of each executor.
    logs: HashMap<ExecutorId, Log>,
    /// Cancels the task of each running job.
    tokens: HashMap<ExecutorId, CancellationToken>,
    /// Runs in flight per executor, which make an update wait.
    pub(super) active_runs: HashMap<ExecutorId, u32>,
    /// The failure the next job ends with.
    pub(super) fail_next: Option<(JobFailure, String)>,
}

#[derive(Default)]
struct Log {
    lines: Vec<String>,
    truncated: bool,
}

/// What a job that is to run needs from its task.
pub(super) struct Plan {
    id: ExecutorId,
    job: String,
    commands: Vec<String>,
    token: CancellationToken,
    fail: Option<(JobFailure, String)>,
}

fn err(code: ErrorCode, message: impl Into<String>) -> ErrorObject {
    ErrorObject::new(code, message)
}

impl World {
    fn executor_index(&self, id: &ExecutorId) -> Result<usize, ErrorObject> {
        self.executors
            .iter()
            .position(|e| &e.descriptor.id == id)
            .ok_or_else(|| err(ErrorCode::NotFound, "no such executor"))
    }

    /// `executor.job.start`. Returns the plan of the task to spawn, or `None` when the job waits
    /// for the executor's runs.
    pub(super) fn start_job(
        &mut self,
        id: &ExecutorId,
        kind: JobKind,
        force: bool,
        root: &CancellationToken,
    ) -> Result<Option<Plan>, ErrorObject> {
        let at = self.executor_index(id)?;
        let info = &self.executors[at];
        if info
            .job
            .as_ref()
            .is_some_and(|j| !matches!(j.phase, JobPhase::Failed { .. }))
        {
            return Err(err(
                ErrorCode::JobRunning,
                format!("{id} already has a job"),
            ));
        }
        let installed = matches!(
            info.health.as_ref().map(|h| &h.installed),
            Some(InstallStatus::Installed { .. })
        );
        let maintenance = info.maintenance.as_ref();
        let (commands, waiting) = match kind {
            JobKind::Install => {
                if installed || info.health.is_none() {
                    return Err(err(ErrorCode::NothingToDo, format!("{id} is installed")));
                }
                let mut commands: Vec<String> = maintenance
                    .and_then(|m| m.setup.as_ref())
                    .map(|s| {
                        s.steps
                            .iter()
                            .filter(|s| s.runner == StepRunner::Reno)
                            .map(|s| s.command.clone())
                            .collect()
                    })
                    .unwrap_or_default();
                if commands.is_empty() {
                    let managed = maintenance.is_some_and(|m| m.managed)
                        || matches!(
                            info.health.as_ref().map(|h| &h.installed),
                            Some(InstallStatus::NotInstalled {
                                why: NotInstalledReason::NeedsInstall { .. }
                            })
                        );
                    if !managed {
                        return Err(err(
                            ErrorCode::NothingToDo,
                            format!("Reno has no way to install {id}"),
                        ));
                    }
                    commands.push(format!("reno managed install {id}"));
                }
                (commands, 0)
            }
            JobKind::Update => {
                if !installed {
                    return Err(err(
                        ErrorCode::NothingToDo,
                        format!("{id} is not installed"),
                    ));
                }
                let updater = maintenance
                    .and_then(|m| m.updater.as_ref())
                    .ok_or_else(|| err(ErrorCode::NoUpdater, format!("{id} has no updater")))?;
                let version = info
                    .health
                    .as_ref()
                    .and_then(|h| h.installed.version().map(str::to_owned));
                if updater.latest.is_some() && updater.latest == version {
                    return Err(err(ErrorCode::NothingToDo, format!("{id} is up to date")));
                }
                let waiting = self.jobs.active_runs.get(id).copied().unwrap_or(0);
                (
                    vec![updater.command.clone()],
                    if force { 0 } else { waiting },
                )
            }
        };
        self.jobs.next += 1;
        let job_id = format!("job-{}", self.jobs.next);
        let started_at_ms = self.tick();
        self.jobs.logs.insert(id.clone(), Log::default());
        let phase = if waiting > 0 {
            JobPhase::Queued {
                waiting_runs: waiting,
            }
        } else {
            running(1, &commands)
        };
        let queued = waiting > 0;
        self.executors[at].job = Some(ExecutorJob {
            id: job_id.clone(),
            kind,
            phase,
            started_at_ms,
        });
        if queued {
            // The commands are rebuilt when the runs end.
            return Ok(None);
        }
        Ok(Some(self.plan(id, &job_id, commands, root)))
    }

    fn plan(
        &mut self,
        id: &ExecutorId,
        job: &str,
        commands: Vec<String>,
        root: &CancellationToken,
    ) -> Plan {
        let token = root.child_token();
        self.jobs.tokens.insert(id.clone(), token.clone());
        Plan {
            id: id.clone(),
            job: job.to_owned(),
            commands,
            token,
            fail: self.jobs.fail_next.take(),
        }
    }

    /// Sets how many runs `id` has in flight. A queued update starts when it reaches none.
    pub(super) fn set_active_runs(
        &mut self,
        id: &ExecutorId,
        runs: u32,
        root: &CancellationToken,
    ) -> Option<Plan> {
        self.jobs.active_runs.insert(id.clone(), runs);
        let info = self.executors.iter_mut().find(|e| &e.descriptor.id == id)?;
        let job = info.job.as_mut()?;
        if !matches!(job.phase, JobPhase::Queued { .. }) {
            return None;
        }
        if runs > 0 {
            job.phase = JobPhase::Queued { waiting_runs: runs };
            return None;
        }
        let command = info
            .maintenance
            .as_ref()
            .and_then(|m| m.updater.as_ref())
            .map(|u| u.command.clone())?;
        let commands = vec![command];
        job.phase = running(1, &commands);
        let job_id = job.id.clone();
        Some(self.plan(id, &job_id, commands, root))
    }

    /// `executor.job.cancel`.
    pub(super) fn cancel_job(&mut self, id: &ExecutorId) -> Result<(), ErrorObject> {
        let at = self.executor_index(id)?;
        let live = self.executors[at]
            .job
            .as_ref()
            .is_some_and(|j| !matches!(j.phase, JobPhase::Failed { .. }));
        if !live {
            return Err(err(
                ErrorCode::NothingToDo,
                format!("{id} has no job to cancel"),
            ));
        }
        if let Some(token) = self.jobs.tokens.remove(id) {
            token.cancel();
        }
        self.push_log(id, "cancelled".to_owned());
        self.fail_job(id, JobFailure::Cancelled, "cancelled");
        Ok(())
    }

    /// `executor.job.dismiss`: clears a failed job. A job that is still going stays.
    pub(super) fn dismiss_job(&mut self, id: &ExecutorId) -> Result<(), ErrorObject> {
        let at = self.executor_index(id)?;
        match &self.executors[at].job {
            Some(job) if !matches!(job.phase, JobPhase::Failed { .. }) => Err(err(
                ErrorCode::JobRunning,
                format!("{id} has a job that is not over"),
            )),
            _ => {
                self.executors[at].job = None;
                // An add that failed leaves an agent that was never installed: it goes.
                let info = &self.executors[at];
                if info.source == reno_proto::ExecutorSource::Registry
                    && !matches!(
                        info.health.as_ref().map(|h| &h.installed),
                        Some(InstallStatus::Installed { .. })
                    )
                {
                    self.executors.remove(at);
                }
                Ok(())
            }
        }
    }

    /// `executor.job.log`.
    pub(super) fn job_log(&self, id: &ExecutorId) -> Result<(Vec<String>, bool), ErrorObject> {
        self.executor_index(id)?;
        Ok(self
            .jobs
            .logs
            .get(id)
            .map(|l| (l.lines.clone(), l.truncated))
            .unwrap_or_default())
    }

    /// `executor.auto_update.set`.
    pub(super) fn set_auto_update(
        &mut self,
        id: &ExecutorId,
        on: bool,
        verified_only: bool,
    ) -> Result<(), ErrorObject> {
        let at = self.executor_index(id)?;
        let maintenance = self.executors[at]
            .maintenance
            .as_mut()
            .filter(|m| m.updater.is_some())
            .ok_or_else(|| err(ErrorCode::NoUpdater, format!("{id} has no updater")))?;
        maintenance.auto_update = reno_proto::AutoUpdate { on, verified_only };
        Ok(())
    }

    fn push_log(&mut self, id: &ExecutorId, line: String) {
        let log = self.jobs.logs.entry(id.clone()).or_default();
        log.lines.push(line);
        if log.lines.len() > LOG_LINES {
            log.lines.remove(0);
            log.truncated = true;
        }
    }

    /// One line of output of the running job `job`, which is on `step`.
    fn job_line(&mut self, plan: &Plan, step: u32, line: String) {
        if !self.is_job(&plan.id, &plan.job) {
            return;
        }
        self.push_log(&plan.id, line.clone());
        if let Some(job) = self.executors.iter_mut().find_map(|e| {
            (e.descriptor.id == plan.id)
                .then_some(e.job.as_mut())
                .flatten()
        }) {
            job.phase = JobPhase::Running {
                step,
                steps: plan.commands.len() as u32,
                last_line: Some(line),
                quiet_since_ms: None,
            };
        }
    }

    fn is_job(&self, id: &ExecutorId, job: &str) -> bool {
        self.executors
            .iter()
            .find(|e| &e.descriptor.id == id)
            .and_then(|e| e.job.as_ref())
            .is_some_and(|j| j.id == job && !matches!(j.phase, JobPhase::Failed { .. }))
    }

    fn fail_job(&mut self, id: &ExecutorId, failure: JobFailure, detail: &str) {
        let now = self.tick();
        if let Some(job) = self
            .executors
            .iter_mut()
            .find(|e| &e.descriptor.id == id)
            .and_then(|e| e.job.as_mut())
        {
            job.phase = JobPhase::Failed {
                failure,
                detail: detail.to_owned(),
                finished_at_ms: now,
            };
        }
    }

    /// The job ended as scripted: the health shows the result and the job goes away, or it
    /// stays `failed`.
    fn finish_job(&mut self, plan: &Plan) {
        if !self.is_job(&plan.id, &plan.job) {
            return;
        }
        self.jobs.tokens.remove(&plan.id);
        if let Some((failure, detail)) = &plan.fail {
            self.push_log(&plan.id, format!("error: {detail}"));
            self.fail_job(&plan.id, *failure, detail);
            return;
        }
        let now = self.tick();
        let index_version = self.index_version(&plan.id);
        let Some(info) = self
            .executors
            .iter_mut()
            .find(|e| e.descriptor.id == plan.id)
        else {
            return;
        };
        let kind = info.job.take().map(|j| j.kind);
        let range = info.descriptor.cli_verified.clone();
        let latest = info
            .maintenance
            .as_ref()
            .and_then(|m| m.updater.as_ref())
            .and_then(|u| u.latest.clone());
        if let Some(health) = &mut info.health {
            match kind {
                Some(JobKind::Install) => {
                    let version = index_version.unwrap_or_else(|| "1.0.0".to_owned());
                    health.version_gate = version_gate(Some(&version), &range);
                    health.installed = InstallStatus::Installed {
                        path: format!("/fake/bin/{}", plan.id).into(),
                        version: Some(version),
                    };
                    health.signed_in = SignedIn::Unknown;
                }
                Some(JobKind::Update) => {
                    if let InstallStatus::Installed { version, .. } = &mut health.installed
                        && let Some(latest) = latest
                    {
                        health.version_gate = version_gate(Some(&latest), &range);
                        *version = Some(latest);
                    }
                }
                None => {}
            }
            health.checked_at_ms = now;
        }
        if let Some(updater) = info.maintenance.as_mut().and_then(|m| m.updater.as_mut()) {
            updater.checked_at_ms = Some(now);
        }
    }
}

fn running(step: u32, commands: &[String]) -> JobPhase {
    JobPhase::Running {
        step,
        steps: commands.len() as u32,
        last_line: None,
        quiet_since_ms: None,
    }
}

/// Walks a planned job to its end, a line at a time, spread over `duration`.
pub(super) fn spawn(shared: &Arc<Shared>, plan: Plan, duration: Duration) {
    let shared = Arc::clone(shared);
    let tracker = shared.tracker.clone();
    tracker.spawn(async move {
        let lines = (plan.commands.len() as u32 * LINES_PER_STEP).max(1);
        let pause = duration / lines;
        for (at, command) in plan.commands.iter().enumerate() {
            let step = at as u32 + 1;
            for n in 0..LINES_PER_STEP {
                let line = match n {
                    0 => format!("$ {command}"),
                    1 => "resolving".to_owned(),
                    2 => "downloading".to_owned(),
                    _ => "done".to_owned(),
                };
                tokio::select! {
                    () = plan.token.cancelled() => return,
                    () = tokio::time::sleep(pause) => {}
                }
                shared.with(|w| w.job_line(&plan, step, line));
            }
        }
        if !plan.token.is_cancelled() {
            shared.with(|w| w.finish_job(&plan));
        }
    });
}
