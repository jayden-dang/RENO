//! `executor.job.*` and `executor.auto_update.set` on a daemon served in-process. Every command
//! goes through a scripted runner that panics on one it was not given, and the self-updater
//! files are never touched (ARCH-16).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{Fixture, id, within};
use futures::stream::BoxStream;
use reno_harness::exec::maintenance::MaintenanceDecl;
use reno_harness::exec::{ExecControls, ExecError, Executor, MockExecutor};
use reno_harness::proc::maintain::{Beat, ScriptedRunner};
use reno_harness::proc::self_update::{SelfUpdateControl, SelfUpdateError, SelfUpdateSwitch};
use reno_kernel::{ExecutorRegistry, Limits, MaintenancePorts};
use reno_proto::{
    ErrorCode, ExecEvent, ExecSpec, ExecutorDescriptor, HealthProbe, InstallStatus, JobKind,
    ModelCatalog, NotInstalledReason, SetupRecipe, SetupStep, SignedIn, StepRunner, VersionGate,
};
use renod::{ExecutorSet, ExecutorSource};

#[derive(Clone)]
struct Recipe {
    inner: MockExecutor,
    found: Arc<Mutex<bool>>,
}

#[async_trait]
impl Executor for Recipe {
    fn descriptor(&self) -> ExecutorDescriptor {
        self.inner.descriptor()
    }

    async fn probe(&self) -> HealthProbe {
        if *self.found.lock().unwrap() {
            HealthProbe {
                installed: InstallStatus::Installed {
                    path: "/bin/recipe".into(),
                    version: Some("1.0.0".into()),
                },
                version_gate: VersionGate::Ok,
                signed_in: SignedIn::Unknown,
                models: ModelCatalog::curated(vec![]),
            }
        } else {
            HealthProbe::not_installed(
                NotInstalledReason::NotFound { hint: "x".into() },
                ModelCatalog::curated(vec![]),
            )
        }
    }

    fn maintenance(&self) -> Option<MaintenanceDecl> {
        Some(MaintenanceDecl {
            setup: Some(SetupRecipe {
                source: "test".into(),
                steps: vec![SetupStep {
                    label: "Install".into(),
                    command: "install-it".into(),
                    runner: StepRunner::Reno,
                }],
            }),
            sign_in: None,
            updaters: Vec::new(),
            managed: false,
        })
    }

    async fn start(
        &self,
        spec: ExecSpec,
        controls: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError> {
        self.inner.start(spec, controls).await
    }
}

#[derive(Debug)]
struct NoFiles;

impl SelfUpdateControl for NoFiles {
    fn previous(&self, _: SelfUpdateSwitch) -> Result<String, SelfUpdateError> {
        panic!("a test touched a settings file")
    }
    fn turn_off(&self, _: SelfUpdateSwitch) -> Result<String, SelfUpdateError> {
        panic!("a test touched a settings file")
    }
    fn restore(&self, _: SelfUpdateSwitch, _: &str) -> Result<(), SelfUpdateError> {
        panic!("a test touched a settings file")
    }
}

fn source(recipe: &Recipe) -> ExecutorSource {
    ExecutorSource::Given(ExecutorSet::from_registry(
        ExecutorRegistry::new().with(Arc::new(recipe.clone())),
    ))
}

fn recipe() -> Recipe {
    Recipe {
        inner: MockExecutor::new(id("recipe")),
        found: Arc::default(),
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_without_the_ports_does_not_offer_the_feature() {
    let recipe = recipe();
    let fx = Fixture::with(|c| c.executors = source(&recipe)).await;
    assert!(!fx.client.supports(reno_proto::FEATURE_EXECUTOR_MAINTAIN));
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_install_job_runs_the_recipe_and_the_errors_have_their_stable_codes() {
    let recipe = recipe();
    let runner = ScriptedRunner::new().script(
        "install-it",
        vec![Beat::Line("==> done".into()), Beat::Exit(0)],
    );
    let ports = MaintenancePorts {
        runner: Arc::new(runner.clone()),
        self_update: Arc::new(NoFiles),
        limits: Limits::default(),
    };
    let found = recipe.found.clone();
    let fx = Fixture::with(|c| {
        c.executors = source(&recipe);
        c.maintenance = Some(ports);
    })
    .await;
    assert!(fx.client.supports(reno_proto::FEATURE_EXECUTOR_MAINTAIN));
    within(fx.client.refresh_executors(None)).await.unwrap();

    // No updater and not installed: an update is nothing to do; auto-update has no updater.
    let nothing = within(
        fx.client
            .start_executor_job(&id("recipe"), JobKind::Update, false),
    )
    .await
    .unwrap_err();
    assert_eq!(nothing.code(), Some(ErrorCode::NothingToDo));
    let none = within(
        fx.client
            .set_executor_auto_update(&id("recipe"), false, false),
    )
    .await
    .unwrap_err();
    assert_eq!(none.code(), Some(ErrorCode::NoUpdater));

    *found.lock().unwrap() = true;
    // The tool arrives while the recipe runs, so the probe after it finds it.
    within(
        fx.client
            .start_executor_job(&id("recipe"), JobKind::Install, false),
    )
    .await
    .unwrap();
    within(async {
        loop {
            let list = fx.client.list_executors().await.unwrap();
            if list[0].job.is_none() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let log = within(fx.client.executor_job_log(&id("recipe")))
        .await
        .unwrap();
    assert!(log.lines.contains(&"$ install-it".to_owned()), "{log:?}");
    assert_eq!(runner.started(), ["install-it"]);
    let ghost = within(fx.client.cancel_executor_job(&id("ghost")))
        .await
        .unwrap_err();
    assert_eq!(ghost.code(), Some(ErrorCode::NotFound));
    let idle = within(fx.client.cancel_executor_job(&id("recipe")))
        .await
        .unwrap_err();
    assert_eq!(idle.code(), Some(ErrorCode::NothingToDo));
    fx.stop().await;
}
