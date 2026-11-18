//! Installs and updates: the job registry, what it shows, the router's refusal while a job runs,
//! the queued update, the settings, and the daily auto-update. Every command goes through a
//! scripted runner that panics on one it was not given, so nothing real is ever spawned.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use reno_harness::HarnessError;
use reno_harness::exec::maintenance::{
    CheckDecl, CheckParse, InstalledVia, MaintenanceDecl, UpdaterDecl,
};
use reno_harness::exec::{ExecControls, ExecError, Executor, MockExecutor, MockStep};
use reno_harness::proc::maintain::{Beat, ScriptedRunner};
use reno_harness::proc::self_update::{SelfUpdateControl, SelfUpdateError, SelfUpdateSwitch};
use reno_proto::{
    ExecEvent, ExecSpec, ExecutorDescriptor, ExecutorInfo, HealthProbe, InstallStatus, JobFailure,
    JobKind, JobPhase, ModelCatalog, NotInstalledReason, RejectReason, RunStatus, SetupRecipe,
    SetupStep, SignedIn, StepRunner, VersionGate, VersionRange,
};

use super::{Fx, config, id, until};
use crate::{ExecutorRegistry, KernelError, Limits, MaintenancePorts, TaskRequest};

const DAY: i64 = 24 * 60 * 60 * 1000;

fn found(version: &str) -> HealthProbe {
    HealthProbe {
        installed: InstallStatus::Installed {
            path: "/bin/tool".into(),
            version: Some(version.into()),
        },
        version_gate: VersionGate::Ok,
        signed_in: SignedIn::Yes { account: None },
        models: ModelCatalog::curated(vec![]),
    }
}

fn missing() -> HealthProbe {
    HealthProbe::not_installed(
        NotInstalledReason::NotFound {
            hint: "tool is not installed".into(),
        },
        ModelCatalog::curated(vec![]),
    )
}

fn needs_install() -> HealthProbe {
    HealthProbe::not_installed(
        NotInstalledReason::NeedsInstall {
            what: "tool's adapter".into(),
        },
        ModelCatalog::curated(vec![]),
    )
}

const PLAIN: UpdaterDecl = UpdaterDecl {
    via: InstalledVia::Any,
    command: "tool update",
    steps: &["tool update"],
    pinned: Some("tool install {version}"),
    check: Some(CheckDecl {
        command: "tool latest",
        parse: CheckParse::PlainVersion,
    }),
    self_updating: false,
    switch: None,
};

const SELF: UpdaterDecl = UpdaterDecl {
    self_updating: true,
    switch: Some(SelfUpdateSwitch::ClaudeSettings),
    ..PLAIN
};

fn decl(updater: Option<UpdaterDecl>) -> MaintenanceDecl {
    MaintenanceDecl {
        setup: Some(SetupRecipe {
            source: "Homebrew cask tool".into(),
            steps: vec![
                SetupStep {
                    label: "Install".into(),
                    command: "brew install --cask tool".into(),
                    runner: StepRunner::Reno,
                },
                SetupStep {
                    label: "Sign in".into(),
                    command: "tool login".into(),
                    runner: StepRunner::Person,
                },
            ],
        }),
        sign_in: None,
        updaters: updater.into_iter().collect(),
        managed: false,
    }
}

/// An executor with a declaration, a probe the test sets, and a managed install.
#[derive(Clone)]
struct Tool {
    inner: MockExecutor,
    says: Arc<Mutex<HealthProbe>>,
    decl: Option<MaintenanceDecl>,
    installs: Arc<AtomicUsize>,
}

impl Tool {
    fn new(says: HealthProbe, decl: Option<MaintenanceDecl>) -> Self {
        Self {
            inner: MockExecutor::new(id("tool")).with_steps([
                MockStep::text("working"),
                MockStep::AwaitSteer,
                MockStep::done(reno_proto::StopReason::Completed),
            ]),
            says: Arc::new(Mutex::new(says)),
            decl,
            installs: Arc::default(),
        }
    }

    fn say(&self, says: HealthProbe) {
        *self.says.lock().unwrap() = says;
    }

    fn registry(&self) -> ExecutorRegistry {
        ExecutorRegistry::new().with(Arc::new(self.clone()))
    }
}

#[async_trait]
impl Executor for Tool {
    fn descriptor(&self) -> ExecutorDescriptor {
        let mut descriptor = self.inner.descriptor();
        descriptor.cli_verified = VersionRange {
            min: "0.1.0".into(),
            verified: "0.9.0".into(),
        };
        descriptor
    }

    async fn probe(&self) -> HealthProbe {
        self.says.lock().unwrap().clone()
    }

    async fn install(&self) -> Result<(), HarnessError> {
        self.installs.fetch_add(1, Ordering::SeqCst);
        self.say(found("1.0.0"));
        Ok(())
    }

    fn maintenance(&self) -> Option<MaintenanceDecl> {
        self.decl.clone()
    }

    async fn start(
        &self,
        spec: ExecSpec,
        controls: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError> {
        self.inner.start(spec, controls).await
    }
}

/// A self-updater switch that records what it was asked and touches no file.
#[derive(Debug, Default, Clone)]
struct Switch {
    calls: Arc<Mutex<Vec<String>>>,
}

impl Switch {
    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl SelfUpdateControl for Switch {
    fn previous(&self, _: SelfUpdateSwitch) -> Result<String, SelfUpdateError> {
        self.calls.lock().unwrap().push("previous".into());
        Ok(r#"{"was":"0"}"#.into())
    }

    fn turn_off(&self, _: SelfUpdateSwitch) -> Result<String, SelfUpdateError> {
        self.calls.lock().unwrap().push("off".into());
        Ok(r#"{"was":"0"}"#.into())
    }

    fn restore(&self, _: SelfUpdateSwitch, previous: &str) -> Result<(), SelfUpdateError> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("restore {previous}"));
        Ok(())
    }
}

struct World {
    fx: Fx,
    tool: Tool,
    runner: ScriptedRunner,
    switch: Switch,
}

impl World {
    async fn new(tool: Tool, runner: ScriptedRunner) -> Self {
        Self::with_limits(tool, runner, Limits::default()).await
    }

    async fn with_limits(tool: Tool, runner: ScriptedRunner, limits: Limits) -> Self {
        let mut config = config(2);
        // A run held across a day of virtual time must not end by its wall-time limit.
        config.default_wall_time = Duration::from_secs(30 * 24 * 60 * 60);
        let fx = Fx::new(config, tool.registry()).await;
        let switch = Switch::default();
        fx.kernel
            .attach_maintenance(MaintenancePorts {
                runner: Arc::new(runner.clone()),
                self_update: Arc::new(switch.clone()),
                limits,
            })
            .await
            .unwrap();
        fx.kernel.refresh_executors(None).await.unwrap();
        Self {
            fx,
            tool,
            runner,
            switch,
        }
    }

    fn info(&self) -> ExecutorInfo {
        self.fx.kernel.executors().remove(0)
    }

    async fn until_job(&self, pred: impl Fn(&Option<reno_proto::ExecutorJob>) -> bool) {
        until(async || pred(&self.info().job).then_some(())).await;
    }

    async fn hold_a_run(&self) -> reno_proto::RunId {
        let run = self
            .fx
            .kernel
            .submit("go", id("tool"), PathBuf::from("/work"), None)
            .await
            .unwrap();
        self.fx.wait_status(&run, RunStatus::Running).await;
        run
    }

    fn started(&self) -> Vec<String> {
        self.runner.started()
    }
}

fn running(job: &Option<reno_proto::ExecutorJob>) -> bool {
    matches!(
        job.as_ref().map(|j| &j.phase),
        Some(JobPhase::Running { .. })
    )
}

fn failed(job: &Option<reno_proto::ExecutorJob>) -> Option<(JobFailure, String)> {
    match job.as_ref().map(|j| &j.phase) {
        Some(JobPhase::Failed {
            failure, detail, ..
        }) => Some((*failure, detail.clone())),
        _ => None,
    }
}

#[tokio::test(start_paused = true)]
async fn the_declaration_reaches_the_list_for_an_executor_that_has_one_and_not_for_one_that_does_not()
 {
    let with = World::new(
        Tool::new(missing(), Some(decl(None))),
        ScriptedRunner::new(),
    )
    .await;
    let shown = with.info().maintenance.expect("declared");
    assert_eq!(shown.setup.expect("not found: a recipe").steps.len(), 2);
    assert!(shown.updater.is_none());
    assert!(shown.auto_update.on && !shown.auto_update.verified_only);
    let without = World::new(Tool::new(found("1.0.0"), None), ScriptedRunner::new()).await;
    assert!(without.info().maintenance.is_none());
}

#[tokio::test(start_paused = true)]
async fn a_recipe_install_runs_only_reno_steps_shows_its_progress_and_probes_again() {
    let runner = ScriptedRunner::new().script(
        "brew install --cask tool",
        vec![
            Beat::Line("==> Downloading".into()),
            Beat::Wait(Duration::from_millis(20)),
            Beat::Exit(0),
        ],
    );
    let world = World::new(Tool::new(missing(), Some(decl(None))), runner).await;
    let started = world
        .fx
        .kernel
        .start_job(&id("tool"), JobKind::Install, false)
        .await
        .unwrap();
    // The answer comes at once, with the job running its one Reno step.
    assert!(matches!(
        started.job.as_ref().map(|j| (&j.kind, &j.phase)),
        Some((
            JobKind::Install,
            JobPhase::Running {
                step: 1,
                steps: 1,
                ..
            }
        ))
    ));
    world
        .until_job(|j| {
            matches!(
                j.as_ref().map(|j| &j.phase),
                Some(JobPhase::Running { last_line: Some(l), .. }) if l == "==> Downloading"
            )
        })
        .await;
    // The tool arrives while the job runs: the probe that follows finds it.
    world.tool.say(found("1.0.0"));
    world.until_job(Option::is_none).await;
    assert!(matches!(
        world.info().health.unwrap().installed,
        InstallStatus::Installed { .. }
    ));
    // Only the Reno step ran; the Person step is left to the person.
    assert_eq!(world.started(), ["brew install --cask tool"]);
    let (lines, truncated) = world.fx.kernel.job_log(&id("tool")).unwrap();
    assert_eq!(lines, ["$ brew install --cask tool", "==> Downloading"]);
    assert!(!truncated);
}

#[tokio::test(start_paused = true)]
async fn a_recipe_that_leaves_the_executor_missing_fails_as_a_probe_failure() {
    let runner = ScriptedRunner::new().script("brew install --cask tool", vec![Beat::Exit(0)]);
    let world = World::new(Tool::new(missing(), Some(decl(None))), runner).await;
    world
        .fx
        .kernel
        .start_job(&id("tool"), JobKind::Install, false)
        .await
        .unwrap();
    world.until_job(|j| failed(j).is_some()).await;
    assert_eq!(failed(&world.info().job).unwrap().0, JobFailure::Probe);
}

#[tokio::test(start_paused = true)]
async fn a_managed_install_runs_the_adapters_own_install_as_a_job() {
    let tool = Tool::new(needs_install(), None);
    let world = World::new(tool.clone(), ScriptedRunner::new()).await;
    world
        .fx
        .kernel
        .start_job(&id("tool"), JobKind::Install, false)
        .await
        .unwrap();
    world.until_job(Option::is_none).await;
    assert_eq!(tool.installs.load(Ordering::SeqCst), 1);
    assert!(matches!(
        world.info().health.unwrap().installed,
        InstallStatus::Installed { .. }
    ));
    assert!(world.started().is_empty());
}

#[tokio::test(start_paused = true)]
async fn what_cannot_be_done_is_refused_with_the_stable_reasons() {
    let world = World::new(
        Tool::new(found("1.0.0"), Some(decl(None))),
        ScriptedRunner::new(),
    )
    .await;
    let kernel = &world.fx.kernel;
    // Installed: nothing to install. No updater declared: no update.
    assert!(matches!(
        kernel.start_job(&id("tool"), JobKind::Install, false).await,
        Err(KernelError::NothingToDo { .. })
    ));
    assert!(matches!(
        kernel.start_job(&id("tool"), JobKind::Update, false).await,
        Err(KernelError::NoUpdater(_))
    ));
    assert!(matches!(
        kernel.set_auto_update(&id("tool"), false, false).await,
        Err(KernelError::NoUpdater(_))
    ));
    // Nothing to cancel; dismissing nothing is fine.
    assert!(matches!(
        kernel.cancel_job(&id("tool")).await,
        Err(KernelError::NothingToDo { .. })
    ));
    kernel.dismiss_job(&id("tool")).await.unwrap();
    assert!(matches!(
        kernel.start_job(&id("ghost"), JobKind::Update, false).await,
        Err(KernelError::ExecutorNotFound(_))
    ));
    // An executor that is not found and has no recipe cannot be installed by Reno.
    let none = World::new(Tool::new(missing(), None), ScriptedRunner::new()).await;
    assert!(matches!(
        none.fx
            .kernel
            .start_job(&id("tool"), JobKind::Install, false)
            .await,
        Err(KernelError::NothingToDo { .. })
    ));
}

#[tokio::test(start_paused = true)]
async fn a_failed_job_stays_until_dismissed_or_started_again_and_one_job_runs_at_a_time() {
    let runner = ScriptedRunner::new()
        .script(
            "tool update",
            vec![Beat::Line("boom".into()), Beat::Exit(2)],
        )
        .script("tool update", vec![Beat::Wait(Duration::from_secs(3600))]);
    let world = World::new(Tool::new(found("1.0.0"), Some(decl(Some(PLAIN)))), runner).await;
    let kernel = &world.fx.kernel;
    kernel
        .start_job(&id("tool"), JobKind::Update, false)
        .await
        .unwrap();
    world.until_job(|j| failed(j).is_some()).await;
    let (failure, detail) = failed(&world.info().job).unwrap();
    assert_eq!(failure, JobFailure::ExitCode);
    assert_eq!(detail, "`tool update` exited with status 2: boom");
    // The output is there to read.
    assert_eq!(
        kernel.job_log(&id("tool")).unwrap().0,
        [
            "$ tool update",
            "boom",
            "error: `tool update` exited with status 2: boom"
        ]
    );
    // Retry replaces the failure; a second start while it runs is refused; dismiss refuses too.
    kernel
        .start_job(&id("tool"), JobKind::Update, false)
        .await
        .unwrap();
    assert!(running(&world.info().job));
    until(async || (world.started().len() == 2).then_some(())).await;
    assert!(matches!(
        kernel.start_job(&id("tool"), JobKind::Update, false).await,
        Err(KernelError::JobRunning(_))
    ));
    assert!(matches!(
        kernel.dismiss_job(&id("tool")).await,
        Err(KernelError::JobRunning(_))
    ));
    let cancelled = kernel.cancel_job(&id("tool")).await.unwrap();
    assert_eq!(failed(&cancelled.job).unwrap().0, JobFailure::Cancelled);
    until(async || (world.runner.stopped() == ["tool update"]).then_some(())).await;
    assert!(kernel.dismiss_job(&id("tool")).await.unwrap().job.is_none());
}

#[tokio::test(start_paused = true)]
async fn a_password_prompt_fails_the_job_at_once() {
    let runner = ScriptedRunner::new().script(
        "tool update",
        vec![
            Beat::Line("Password:".into()),
            Beat::Wait(Duration::from_secs(60)),
        ],
    );
    let world = World::new(Tool::new(found("1.0.0"), Some(decl(Some(PLAIN)))), runner).await;
    world
        .fx
        .kernel
        .start_job(&id("tool"), JobKind::Update, false)
        .await
        .unwrap();
    world.until_job(|j| failed(j).is_some()).await;
    assert_eq!(
        failed(&world.info().job).unwrap().0,
        JobFailure::NeedsPassword
    );
}

#[tokio::test(start_paused = true)]
async fn quiet_and_the_ceiling_are_reported_and_enforced() {
    let limits = Limits {
        quiet: Duration::from_millis(30),
        ceiling: Duration::from_millis(200),
    };
    let runner = ScriptedRunner::new().script("tool update", vec![Beat::Line("hi".into())]);
    let world = World::with_limits(
        Tool::new(found("1.0.0"), Some(decl(Some(PLAIN)))),
        runner,
        limits,
    )
    .await;
    world
        .fx
        .kernel
        .start_job(&id("tool"), JobKind::Update, false)
        .await
        .unwrap();
    // Silence is said, in epoch time of the last output, and the job keeps running.
    world
        .until_job(|j| {
            matches!(
                j.as_ref().map(|j| &j.phase),
                Some(JobPhase::Running {
                    quiet_since_ms: Some(_),
                    ..
                })
            )
        })
        .await;
    world.until_job(|j| failed(j).is_some()).await;
    assert_eq!(failed(&world.info().job).unwrap().0, JobFailure::TimedOut);
}

#[tokio::test(start_paused = true)]
async fn an_executor_with_a_running_job_takes_no_run_and_takes_one_after() {
    let runner = ScriptedRunner::new().script(
        "tool update",
        vec![Beat::Wait(Duration::from_secs(3600)), Beat::Exit(0)],
    );
    let world = World::new(Tool::new(found("1.0.0"), Some(decl(Some(PLAIN)))), runner).await;
    world
        .fx
        .kernel
        .start_job(&id("tool"), JobKind::Update, false)
        .await
        .unwrap();
    // Routed while the job runs: the executor is passed over, with the reason.
    let (run, decision) = world
        .fx
        .kernel
        .submit_task(TaskRequest::new("go", PathBuf::from("/work")).executor(id("tool")))
        .await
        .unwrap();
    let rejection = decision
        .rejected
        .iter()
        .find(|r| r.executor == id("tool"))
        .expect("tool is passed over");
    assert!(
        matches!(&rejection.reason, RejectReason::StartFailed { detail, .. } if detail.contains("update of tool is running")),
        "{:?}",
        rejection.reason
    );
    world.fx.wait_status(&run, RunStatus::Failed).await;
    // After the job, a run starts.
    world.fx.kernel.cancel_job(&id("tool")).await.unwrap();
    world.fx.kernel.dismiss_job(&id("tool")).await.unwrap();
    let run = world.hold_a_run().await;
    world.fx.kernel.cancel(&run).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn an_update_waits_for_the_runs_and_starts_when_they_end_unless_forced() {
    let runner = ScriptedRunner::new()
        .script("tool update", vec![Beat::Exit(0)])
        .script("tool update", vec![Beat::Exit(0)]);
    let world = World::new(Tool::new(found("1.0.0"), Some(decl(Some(PLAIN)))), runner).await;
    let run = world.hold_a_run().await;
    let queued = world
        .fx
        .kernel
        .start_job(&id("tool"), JobKind::Update, false)
        .await
        .unwrap();
    assert_eq!(
        queued.job.unwrap().phase,
        JobPhase::Queued { waiting_runs: 1 }
    );
    assert!(world.started().is_empty());
    // The run ends: the update starts, runs, and the job goes away.
    world.fx.kernel.cancel(&run).await.unwrap();
    world.until_job(Option::is_none).await;
    assert_eq!(world.started(), ["tool update"]);

    // Forced: it does not wait.
    let _held = world.hold_a_run().await;
    let forced = world
        .fx
        .kernel
        .start_job(&id("tool"), JobKind::Update, true)
        .await
        .unwrap();
    assert!(running(&forced.job));
    world.until_job(Option::is_none).await;
    assert_eq!(world.started().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_queued_update_can_be_cancelled_and_never_runs() {
    let world = World::new(
        Tool::new(found("1.0.0"), Some(decl(Some(PLAIN)))),
        ScriptedRunner::new(),
    )
    .await;
    let run = world.hold_a_run().await;
    world
        .fx
        .kernel
        .start_job(&id("tool"), JobKind::Update, false)
        .await
        .unwrap();
    let cancelled = world.fx.kernel.cancel_job(&id("tool")).await.unwrap();
    assert_eq!(failed(&cancelled.job).unwrap().0, JobFailure::Cancelled);
    world.fx.kernel.cancel(&run).await.unwrap();
    world.fx.idle().await;
    // The scripted runner would have panicked on an unscripted `tool update`.
    assert!(world.started().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_update_to_a_version_that_is_not_newer_is_refused() {
    let runner = ScriptedRunner::new().script(
        "tool latest",
        vec![Beat::Line("1.0.0".into()), Beat::Exit(0)],
    );
    let world = World::new(Tool::new(found("1.0.0"), Some(decl(Some(PLAIN)))), runner).await;
    // The daily check learns the newest version one day on.
    world.fx.clock.advance(DAY);
    tokio::time::advance(Duration::from_millis(DAY as u64 + 1)).await;
    until(async || {
        world
            .info()
            .maintenance
            .and_then(|m| m.updater)
            .and_then(|u| u.latest)
    })
    .await;
    assert!(matches!(
        world
            .fx
            .kernel
            .start_job(&id("tool"), JobKind::Update, false)
            .await,
        Err(KernelError::NothingToDo { .. })
    ));
}

#[tokio::test(start_paused = true)]
async fn the_daily_check_updates_an_idle_executor_that_does_not_update_itself_and_never_at_start() {
    let runner = ScriptedRunner::new()
        .script(
            "tool latest",
            vec![Beat::Line("1.1.0".into()), Beat::Exit(0)],
        )
        .script("tool update", vec![Beat::Exit(0)]);
    let world = World::new(Tool::new(found("1.0.0"), Some(decl(Some(PLAIN)))), runner).await;
    // Nothing runs at start, however long the daemon idles inside the day.
    tokio::time::advance(Duration::from_secs(60 * 60)).await;
    world.fx.idle().await;
    assert!(world.started().is_empty());
    // A day later (the wall clock and the deadline agree) it checks, then updates.
    world.tool.say(found("1.0.0"));
    world.fx.clock.advance(DAY);
    tokio::time::advance(Duration::from_millis(DAY as u64)).await;
    until(async || (world.started() == ["tool latest", "tool update"]).then_some(())).await;
    world.until_job(Option::is_none).await;
    let latest = world.info().maintenance.unwrap().updater.unwrap();
    assert_eq!(latest.latest.as_deref(), Some("1.1.0"));
    // The next check is a day after this one, not at once.
    world.fx.idle().await;
    assert_eq!(world.started().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn the_daily_check_waits_for_an_idle_executor_and_leaves_a_self_updating_cli_alone() {
    // Busy: the check does not run until the last run ends.
    let runner = ScriptedRunner::new()
        .script(
            "tool latest",
            vec![Beat::Line("1.1.0".into()), Beat::Exit(0)],
        )
        .script("tool update", vec![Beat::Exit(0)]);
    let world = World::new(Tool::new(found("1.0.0"), Some(decl(Some(PLAIN)))), runner).await;
    let run = world.hold_a_run().await;
    world.fx.clock.advance(DAY);
    tokio::time::advance(Duration::from_millis(DAY as u64)).await;
    world.fx.idle().await;
    assert!(
        world.started().is_empty(),
        "never swap a binary under a run"
    );
    world.fx.kernel.cancel(&run).await.unwrap();
    until(async || (world.started() == ["tool latest", "tool update"]).then_some(())).await;

    // A self-updating CLI: only `latest` is refreshed.
    let runner = ScriptedRunner::new().script(
        "tool latest",
        vec![Beat::Line("1.1.0".into()), Beat::Exit(0)],
    );
    let world = World::new(Tool::new(found("1.0.0"), Some(decl(Some(SELF)))), runner).await;
    world.fx.clock.advance(DAY);
    tokio::time::advance(Duration::from_millis(DAY as u64)).await;
    until(async || {
        world
            .info()
            .maintenance
            .and_then(|m| m.updater)
            .and_then(|u| u.latest)
    })
    .await;
    world.fx.idle().await;
    assert_eq!(world.started(), ["tool latest"]);
}

#[tokio::test(start_paused = true)]
async fn auto_update_off_stops_the_daily_check_and_verified_only_pins() {
    // Off: no check.
    let world = World::new(
        Tool::new(found("1.0.0"), Some(decl(Some(PLAIN)))),
        ScriptedRunner::new(),
    )
    .await;
    let info = world
        .fx
        .kernel
        .set_auto_update(&id("tool"), false, false)
        .await
        .unwrap();
    assert!(!info.maintenance.unwrap().auto_update.on);
    world.fx.clock.advance(2 * DAY);
    tokio::time::advance(Duration::from_millis(2 * DAY as u64)).await;
    world.fx.idle().await;
    assert!(world.started().is_empty());
    // The choice is in the store.
    let rows = world.fx.store.executor_settings().await.unwrap();
    assert!(!rows[0].auto_update);

    // Verified only: the update is the pinned one, to the version the adapter was verified on.
    let tool = Tool::new(found("0.5.0"), Some(decl(Some(PLAIN))));
    let verified = tool.descriptor().cli_verified.verified;
    let runner =
        ScriptedRunner::new().script(&format!("tool install {verified}"), vec![Beat::Exit(0)]);
    let world = World::new(tool, runner).await;
    world
        .fx
        .kernel
        .set_auto_update(&id("tool"), true, true)
        .await
        .unwrap();
    world
        .fx
        .kernel
        .start_job(&id("tool"), JobKind::Update, false)
        .await
        .unwrap();
    world.until_job(Option::is_none).await;
    assert_eq!(world.started(), [format!("tool install {verified}")]);
}

#[tokio::test(start_paused = true)]
async fn turning_auto_update_off_turns_a_self_updating_clis_own_updater_off_and_on_restores_it() {
    let world = World::new(
        Tool::new(found("1.0.0"), Some(decl(Some(SELF)))),
        ScriptedRunner::new(),
    )
    .await;
    let kernel = &world.fx.kernel;
    kernel
        .set_auto_update(&id("tool"), false, false)
        .await
        .unwrap();
    // The previous value is stored before the file is changed.
    assert_eq!(world.switch.calls(), ["previous", "off"]);
    let row = &world.fx.store.executor_settings().await.unwrap()[0];
    assert_eq!(row.prev_self_update.as_deref(), Some(r#"{"was":"0"}"#));
    // Off again: nothing more happens, and the first previous value survives.
    kernel
        .set_auto_update(&id("tool"), false, true)
        .await
        .unwrap();
    assert_eq!(world.switch.calls(), ["previous", "off"]);
    // On: what was there is put back and forgotten.
    let info = kernel
        .set_auto_update(&id("tool"), true, true)
        .await
        .unwrap();
    assert_eq!(
        world.switch.calls(),
        ["previous", "off", r#"restore {"was":"0"}"#]
    );
    assert!(info.maintenance.unwrap().auto_update.on);
    let row = &world.fx.store.executor_settings().await.unwrap()[0];
    assert_eq!(row.prev_self_update, None);
    assert!(row.verified_only);
    // On again: nothing to restore.
    kernel
        .set_auto_update(&id("tool"), true, true)
        .await
        .unwrap();
    assert_eq!(world.switch.calls().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn the_settings_survive_a_restart() {
    let world = World::new(
        Tool::new(found("1.0.0"), Some(decl(Some(PLAIN)))),
        ScriptedRunner::new(),
    )
    .await;
    world
        .fx
        .kernel
        .set_auto_update(&id("tool"), false, true)
        .await
        .unwrap();
    let (dir, store) = world.fx.into_store();
    let tool = Tool::new(found("1.0.0"), Some(decl(Some(PLAIN))));
    let fx = Fx::over(dir, store, config(1), tool.registry()).await;
    fx.kernel
        .attach_maintenance(MaintenancePorts {
            runner: Arc::new(ScriptedRunner::new()),
            self_update: Arc::new(Switch::default()),
            limits: Limits::default(),
        })
        .await
        .unwrap();
    fx.kernel.refresh_executors(None).await.unwrap();
    let shown = fx
        .kernel
        .executors()
        .remove(0)
        .maintenance
        .unwrap()
        .auto_update;
    assert!(!shown.on && shown.verified_only);
}

#[tokio::test(start_paused = true)]
async fn without_the_ports_nothing_can_be_started() {
    let tool = Tool::new(found("1.0.0"), Some(decl(Some(PLAIN))));
    let fx = Fx::new(config(1), tool.registry()).await;
    assert!(matches!(
        fx.kernel
            .start_job(&id("tool"), JobKind::Update, false)
            .await,
        Err(KernelError::MaintenanceOff)
    ));
    // And the list carries no job or declaration: an older daemon looks like this.
    fx.kernel.refresh_executors(None).await.unwrap();
    assert!(fx.kernel.executors()[0].maintenance.is_none());
}

#[tokio::test(start_paused = true)]
async fn a_running_job_and_the_maintenance_tasks_end_with_the_kernel() {
    let runner = ScriptedRunner::new().script("tool update", vec![Beat::Line("x".into())]);
    let world = World::new(Tool::new(found("1.0.0"), Some(decl(Some(PLAIN)))), runner).await;
    world
        .fx
        .kernel
        .start_job(&id("tool"), JobKind::Update, false)
        .await
        .unwrap();
    until(async || (world.started().len() == 1).then_some(())).await;
    let report = world.fx.kernel.shutdown().await;
    assert!(!report.aborted, "no maintenance task outlives the shutdown");
    // The command was dropped with its job: what a real runner kills.
    assert_eq!(world.runner.stopped(), ["tool update"]);
    assert_eq!(world.fx.kernel.live_tasks(), 0);
}
