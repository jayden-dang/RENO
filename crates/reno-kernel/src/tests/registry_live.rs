//! Agents from the ACP registry added and removed while the daemon runs: the set takes inserts and
//! removes live and publishes them, an add never replaces a registered executor, a remove never
//! orphans a run, and an add is a person's install job that ends with the full probe.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use reno_harness::HarnessError;
use reno_harness::exec::{ExecControls, ExecError, Executor, MockExecutor, MockStep, ProbeDepth};
use reno_harness::proc::maintain::ScriptedRunner;
use reno_proto::{
    ExecEvent, ExecSpec, ExecutorDescriptor, ExecutorSource, HealthProbe, InstallStatus,
    JobFailure, JobKind, JobPhase, ModelCatalog, NotInstalledReason, RunStatus, SignedIn,
    VersionGate,
};

mod defs_live;

use super::{Fx, config, id, until};
use crate::{ExecutorRegistry, InstallWork, KernelError, Limits, MaintenancePorts, RegistryPort};

#[derive(Debug)]
struct NoSwitch;

impl reno_harness::proc::self_update::SelfUpdateControl for NoSwitch {
    fn previous(
        &self,
        _: reno_harness::proc::self_update::SelfUpdateSwitch,
    ) -> Result<String, reno_harness::proc::self_update::SelfUpdateError> {
        unreachable!("no self-updater here")
    }
    fn turn_off(
        &self,
        _: reno_harness::proc::self_update::SelfUpdateSwitch,
    ) -> Result<String, reno_harness::proc::self_update::SelfUpdateError> {
        unreachable!("no self-updater here")
    }
    fn restore(
        &self,
        _: reno_harness::proc::self_update::SelfUpdateSwitch,
        _: &str,
    ) -> Result<(), reno_harness::proc::self_update::SelfUpdateError> {
        unreachable!("no self-updater here")
    }
}

/// What the "installed" files of the fake agents say: id to version.
type Disk = Arc<Mutex<HashMap<String, String>>>;

/// A registry agent: installed when `disk` says so, and never started for real.
#[derive(Clone)]
struct Agent {
    inner: MockExecutor,
    name: &'static str,
    disk: Disk,
    depths: Arc<Mutex<Vec<ProbeDepth>>>,
    source: ExecutorSource,
}

impl Agent {
    fn new(name: &'static str, disk: &Disk) -> Self {
        Self {
            inner: MockExecutor::new(id(name)).with_steps([
                MockStep::text("working"),
                MockStep::AwaitSteer,
                MockStep::done(reno_proto::StopReason::Completed),
            ]),
            name,
            disk: disk.clone(),
            depths: Arc::default(),
            source: ExecutorSource::Registry,
        }
    }

    fn builtin(mut self) -> Self {
        self.source = ExecutorSource::Builtin;
        self
    }
}

#[async_trait]
impl Executor for Agent {
    fn descriptor(&self) -> ExecutorDescriptor {
        self.inner.descriptor()
    }

    fn source(&self) -> ExecutorSource {
        self.source
    }

    async fn probe_at(&self, depth: ProbeDepth) -> HealthProbe {
        self.depths.lock().unwrap().push(depth);
        self.probe().await
    }

    async fn probe(&self) -> HealthProbe {
        let rid = self
            .name
            .strip_prefix("acp.")
            .unwrap_or(self.name)
            .to_owned();
        match self.disk.lock().unwrap().get(&rid) {
            Some(version) => HealthProbe {
                installed: InstallStatus::Installed {
                    path: "/agents/x".into(),
                    version: Some(version.clone()),
                },
                version_gate: VersionGate::Ok,
                signed_in: SignedIn::Unknown,
                models: ModelCatalog::curated(vec![]),
            },
            None => HealthProbe::not_installed(
                NotInstalledReason::NotFound {
                    hint: "not added".into(),
                },
                ModelCatalog::curated(vec![]),
            ),
        }
    }

    async fn start(
        &self,
        spec: ExecSpec,
        controls: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError> {
        self.inner.start(spec, controls).await
    }
}

/// The index: the version it lists per id, and how many installs it was asked for.
#[derive(Debug, Clone)]
struct Index {
    listed: Arc<Mutex<HashMap<String, String>>>,
    disk: Disk,
    installs: Arc<AtomicUsize>,
    fail: Arc<Mutex<Option<String>>>,
}

impl Index {
    fn new(disk: &Disk) -> Self {
        Self {
            listed: Arc::default(),
            disk: disk.clone(),
            installs: Arc::default(),
            fail: Arc::default(),
        }
    }

    fn list(&self, rid: &str, version: &str) {
        self.listed
            .lock()
            .unwrap()
            .insert(rid.to_owned(), version.to_owned());
    }
}

impl RegistryPort for Index {
    fn latest<'a>(&'a self, rid: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move { self.listed.lock().unwrap().get(rid).cloned() })
    }

    fn install_work<'a>(&'a self, rid: &'a str) -> BoxFuture<'a, Option<InstallWork>> {
        Box::pin(async move {
            let version = self.listed.lock().unwrap().get(rid).cloned()?;
            let (disk, installs, fail, rid) = (
                self.disk.clone(),
                self.installs.clone(),
                self.fail.clone(),
                rid.to_owned(),
            );
            let work: InstallWork = Arc::new(move || {
                let (disk, installs, fail, rid, version) = (
                    disk.clone(),
                    installs.clone(),
                    fail.clone(),
                    rid.clone(),
                    version.clone(),
                );
                Box::pin(async move {
                    installs.fetch_add(1, Ordering::SeqCst);
                    if let Some(why) = fail.lock().unwrap().clone() {
                        return Err(HarnessError::Install(why));
                    }
                    disk.lock().unwrap().insert(rid, version);
                    Ok(())
                })
            });
            Some(work)
        })
    }
}

struct World {
    fx: Fx,
    disk: Disk,
    index: Index,
}

impl World {
    async fn new(concurrent: usize, built_in: Vec<Agent>) -> Self {
        Self::build(concurrent, built_in, None).await
    }

    /// A world whose kernel loads definitions through `port`.
    async fn with_defs(port: &Arc<super::defs::FakePort>) -> Self {
        Self::build(2, vec![], Some(port)).await
    }

    async fn build(
        concurrent: usize,
        built_in: Vec<Agent>,
        defs: Option<&Arc<super::defs::FakePort>>,
    ) -> Self {
        let disk = Disk::default();
        let mut registry = ExecutorRegistry::new();
        for agent in built_in {
            registry = registry.with(Arc::new(agent));
        }
        let fx = match defs {
            Some(port) => {
                Fx::with_definitions(config(concurrent), registry, super::defs::setup(port)).await
            }
            None => Fx::new(config(concurrent), registry).await,
        };
        fx.kernel
            .attach_maintenance(MaintenancePorts {
                runner: Arc::new(ScriptedRunner::default()),
                self_update: Arc::new(NoSwitch),
                limits: Limits::default(),
                check_at_start: false,
            })
            .await
            .unwrap();
        let index = Index::new(&disk);
        fx.kernel.attach_registry(Arc::new(index.clone())).unwrap();
        Self { fx, disk, index }
    }

    fn agent(&self, name: &'static str) -> Agent {
        Agent::new(name, &self.disk)
    }

    fn info(&self, name: &str) -> Option<reno_proto::ExecutorInfo> {
        self.fx
            .kernel
            .executors()
            .into_iter()
            .find(|i| i.descriptor.id.as_str() == name)
    }
}

#[tokio::test(start_paused = true)]
async fn an_added_agent_is_registered_live_installed_and_probed_fully() {
    let w = World::new(2, vec![]).await;
    w.index.list("demo", "1.0.0");
    let agent = w.agent("acp.demo");
    let depths = agent.depths.clone();
    let mut watch = w.fx.kernel.watch_executors();
    watch.borrow_and_update();

    let info =
        w.fx.kernel
            .add_registry_executor(Arc::new(agent))
            .await
            .unwrap();
    assert_eq!(info.descriptor.id.as_str(), "acp.demo");
    assert_eq!(info.source, ExecutorSource::Registry);
    assert!(
        info.job.is_some(),
        "an install job is shown at once: {info:?}"
    );
    // The watch carried the new executor.
    assert!(watch.has_changed().unwrap());

    until(async || {
        let i = w.info("acp.demo")?;
        (i.job.is_none()
            && i.health
                .is_some_and(|h| h.installed.version() == Some("1.0.0")))
        .then_some(())
    })
    .await;
    assert_eq!(w.index.installs.load(Ordering::SeqCst), 1);
    // A person's add ends with the full probe.
    assert_eq!(depths.lock().unwrap().last(), Some(&ProbeDepth::Full));
}

#[tokio::test(start_paused = true)]
async fn an_added_agent_never_replaces_a_registered_executor() {
    let disk = Disk::default();
    let builtin = Agent::new("acp.devin", &disk).builtin();
    let w = World::new(2, vec![builtin]).await;
    w.index.list("devin", "9.9.9");
    let before = w.info("acp.devin").unwrap();
    let err =
        w.fx.kernel
            .add_registry_executor(Arc::new(w.agent("acp.devin")))
            .await
            .unwrap_err();
    assert!(matches!(err, KernelError::NothingToDo { .. }), "{err}");
    assert_eq!(w.info("acp.devin").unwrap().source, before.source);
    assert_eq!(w.index.installs.load(Ordering::SeqCst), 0);
    assert_eq!(
        w.fx.kernel.executors().len(),
        1,
        "nothing was added beside it"
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_add_shows_its_failure_and_dismissing_it_unregisters_the_agent() {
    let w = World::new(2, vec![]).await;
    w.index.list("demo", "1.0.0");
    *w.index.fail.lock().unwrap() = Some("the download broke".into());
    w.fx.kernel
        .add_registry_executor(Arc::new(w.agent("acp.demo")))
        .await
        .unwrap();
    until(async || {
        matches!(
            w.info("acp.demo")?.job?.phase,
            JobPhase::Failed {
                failure: JobFailure::ExitCode,
                ..
            }
        )
        .then_some(())
    })
    .await;
    w.fx.kernel.dismiss_job(&id("acp.demo")).await.unwrap();
    assert!(
        w.info("acp.demo").is_none(),
        "nothing installed, nothing listed"
    );
}

#[tokio::test(start_paused = true)]
async fn an_agent_the_index_does_not_list_cannot_be_added() {
    let w = World::new(2, vec![]).await;
    let err =
        w.fx.kernel
            .add_registry_executor(Arc::new(w.agent("acp.ghost")))
            .await
            .unwrap_err();
    assert!(matches!(err, KernelError::NothingToDo { .. }), "{err}");
    assert!(w.info("acp.ghost").is_none(), "unregistered again");
}

#[tokio::test(start_paused = true)]
async fn a_remove_refuses_while_a_run_is_live_or_queued_and_then_unregisters() {
    let w = World::new(1, vec![]).await;
    w.index.list("demo", "1.0.0");
    w.fx.kernel
        .add_registry_executor(Arc::new(w.agent("acp.demo")))
        .await
        .unwrap();
    until(async || {
        let i = w.info("acp.demo")?;
        i.job.is_none().then_some(())
    })
    .await;
    let kernel = &w.fx.kernel;
    let live = kernel
        .submit("go", id("acp.demo"), PathBuf::from("/work"), None)
        .await
        .unwrap();
    w.fx.wait_status(&live, RunStatus::Running).await;
    // A second run waits in the queue (one slot).
    let queued = kernel
        .submit("go", id("acp.demo"), PathBuf::from("/work"), None)
        .await
        .unwrap();
    let err = kernel
        .remove_registry_executor(&id("acp.demo"))
        .await
        .unwrap_err();
    assert!(matches!(err, KernelError::ExecutorInUse { .. }), "{err}");
    assert!(w.info("acp.demo").is_some(), "still registered");

    kernel.cancel(&queued).await.unwrap();
    kernel.cancel(&live).await.unwrap();
    w.fx.wait_status(&live, RunStatus::Cancelled).await;
    until(async || kernel.remove_registry_executor(&id("acp.demo")).await.ok()).await;
    assert!(w.info("acp.demo").is_none());
    // Its stored health is forgotten: a run on it now finds no such executor.
    assert!(
        kernel
            .remove_registry_executor(&id("acp.demo"))
            .await
            .is_err()
    );
}

#[tokio::test(start_paused = true)]
async fn only_an_added_agent_can_be_removed() {
    let disk = Disk::default();
    let w = World::new(2, vec![Agent::new("acp.devin", &disk).builtin()]).await;
    let err =
        w.fx.kernel
            .remove_registry_executor(&id("acp.devin"))
            .await
            .unwrap_err();
    assert!(matches!(err, KernelError::ExecutorInUse { .. }), "{err}");
    assert!(w.info("acp.devin").is_some());
}

#[tokio::test(start_paused = true)]
async fn an_update_installs_the_index_version_and_a_delisted_agent_cannot_update() {
    let w = World::new(2, vec![]).await;
    w.index.list("demo", "1.0.0");
    w.fx.kernel
        .add_registry_executor(Arc::new(w.agent("acp.demo")))
        .await
        .unwrap();
    until(async || w.info("acp.demo")?.job.is_none().then_some(())).await;

    // Same version: nothing to do.
    let err =
        w.fx.kernel
            .start_job(&id("acp.demo"), JobKind::Update, false)
            .await
            .unwrap_err();
    assert!(matches!(err, KernelError::NothingToDo { .. }), "{err}");

    // A newer one in the index: an update is an add job.
    w.index.list("demo", "1.1.0");
    w.fx.kernel
        .start_job(&id("acp.demo"), JobKind::Update, false)
        .await
        .unwrap();
    until(async || {
        let i = w.info("acp.demo")?;
        (i.job.is_none()
            && i.health
                .is_some_and(|h| h.installed.version() == Some("1.1.0")))
        .then_some(())
    })
    .await;
    assert_eq!(w.index.installs.load(Ordering::SeqCst), 2);

    // Delisted: it can be removed, not updated.
    w.index.listed.lock().unwrap().clear();
    let err =
        w.fx.kernel
            .start_job(&id("acp.demo"), JobKind::Update, false)
            .await
            .unwrap_err();
    assert!(matches!(err, KernelError::NothingToDo { .. }), "{err}");
}

#[tokio::test(start_paused = true)]
async fn the_daily_check_updates_a_registry_agent_in_the_background_with_a_quiet_probe() {
    let w = World::new(2, vec![]).await;
    w.index.list("demo", "1.0.0");
    let agent = w.agent("acp.demo");
    let depths = agent.depths.clone();
    w.fx.kernel
        .add_registry_executor(Arc::new(agent))
        .await
        .unwrap();
    until(async || w.info("acp.demo")?.job.is_none().then_some(())).await;
    w.fx.idle().await;
    assert_eq!(depths.lock().unwrap().last(), Some(&ProbeDepth::Full));

    // The index moves on; a day later the check finds it and adds the new version.
    w.index.list("demo", "1.1.0");
    w.fx.clock.advance(DAY);
    tokio::time::advance(std::time::Duration::from_millis(DAY as u64 + 1)).await;
    until(async || {
        let i = w.info("acp.demo")?;
        (i.job.is_none()
            && i.health
                .is_some_and(|h| h.installed.version() == Some("1.1.0")))
        .then_some(())
    })
    .await;
    assert_eq!(w.index.installs.load(Ordering::SeqCst), 2);
    assert_eq!(
        depths.lock().unwrap().last(),
        Some(&ProbeDepth::Quiet),
        "a background update never starts the agent"
    );
}

#[tokio::test(start_paused = true)]
async fn with_auto_update_off_the_daily_check_leaves_a_registry_agent_alone() {
    let w = World::new(2, vec![]).await;
    w.index.list("demo", "1.0.0");
    w.fx.kernel
        .add_registry_executor(Arc::new(w.agent("acp.demo")))
        .await
        .unwrap();
    until(async || w.info("acp.demo")?.job.is_none().then_some(())).await;
    let info =
        w.fx.kernel
            .set_auto_update(&id("acp.demo"), false, false)
            .await
            .unwrap();
    assert!(!info.maintenance.unwrap().auto_update.on);
    w.index.list("demo", "1.1.0");
    w.fx.clock.advance(2 * DAY);
    tokio::time::advance(std::time::Duration::from_millis(2 * DAY as u64)).await;
    w.fx.idle().await;
    assert_eq!(w.index.installs.load(Ordering::SeqCst), 1);
}

const DAY: i64 = 24 * 60 * 60 * 1000;

/// A run that queued for an agent between the check of the store and the unregister: the agent is
/// gone when it dispatches, and the run ends terminal with the reason, never stuck.
#[tokio::test(start_paused = true)]
async fn a_run_queued_on_an_agent_that_goes_away_ends_terminal_with_a_reason() {
    let w = World::new(1, vec![]).await;
    w.index.list("demo", "1.0.0");
    w.fx.kernel
        .add_registry_executor(Arc::new(w.agent("acp.demo")))
        .await
        .unwrap();
    until(async || w.info("acp.demo")?.job.is_none().then_some(())).await;
    let kernel = &w.fx.kernel;
    let live = kernel
        .submit("go", id("acp.demo"), PathBuf::from("/work"), None)
        .await
        .unwrap();
    w.fx.wait_status(&live, RunStatus::Running).await;
    let queued = kernel
        .submit("go", id("acp.demo"), PathBuf::from("/work"), None)
        .await
        .unwrap();
    // The race, made certain: the executor leaves the set while both runs hold its id.
    kernel.forget(&id("acp.demo")).await;
    kernel.cancel(&live).await.unwrap();
    w.fx.wait_status(&queued, RunStatus::Failed).await;
    let events = w.fx.events(&queued, None).await;
    let done = events.last().unwrap().as_ref().unwrap().event.clone();
    assert!(
        matches!(&done, ExecEvent::Done { diagnostic: Some(why), .. } if why.contains("unknown executor acp.demo")),
        "{done:?}"
    );
}
