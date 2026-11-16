//! `executor.install` on a daemon served in-process, over executors whose probe and install the
//! test controls. Nothing is fetched, nothing leaves the machine (ARCH-16).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{Fixture, id, within};
use futures::stream::BoxStream;
use reno_harness::HarnessError;
use reno_harness::exec::{ExecControls, ExecError, Executor, MockExecutor};
use reno_kernel::ExecutorRegistry;
use reno_proto::{
    ErrorCode, ExecEvent, ExecSpec, ExecutorDescriptor, HealthProbe, InstallStatus, ModelCatalog,
    NotInstalledReason, SignedIn, VersionGate,
};
use renod::{ExecutorSet, ExecutorSource};
use tokio::sync::Notify;

/// What an install does when called.
#[derive(Clone)]
enum Install {
    /// Succeeds, and the probe says installed from then on.
    Succeeds,
    /// Fails like a fetch that could not reach the registry.
    Fails,
    /// Waits for the test, then succeeds.
    Waits(Arc<Notify>, Arc<Notify>),
}

/// An executor that needs an install until `install()` is called.
#[derive(Clone)]
struct NeedsAdapter {
    inner: MockExecutor,
    installed: Arc<Mutex<bool>>,
    calls: Arc<AtomicU32>,
    how: Install,
    /// What the probe says before an install.
    before: NotInstalledReason,
}

impl NeedsAdapter {
    fn new(name: &str, how: Install) -> Self {
        Self {
            inner: MockExecutor::new(id(name)),
            installed: Arc::new(Mutex::new(false)),
            calls: Arc::new(AtomicU32::new(0)),
            how,
            before: NotInstalledReason::NeedsInstall {
                what: "the adapter (npm)".into(),
            },
        }
    }

    fn missing_for_another_reason(mut self) -> Self {
        self.before = NotInstalledReason::NotFound {
            hint: "install it yourself".into(),
        };
        self
    }

    fn calls(&self) -> u32 {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Executor for NeedsAdapter {
    fn descriptor(&self) -> ExecutorDescriptor {
        self.inner.descriptor()
    }

    async fn probe(&self) -> HealthProbe {
        if *self.installed.lock().unwrap() {
            HealthProbe {
                installed: InstallStatus::Installed {
                    path: "/managed/adapter".into(),
                    version: Some("1.0.0".into()),
                },
                version_gate: VersionGate::Ok,
                signed_in: SignedIn::Unknown,
                models: ModelCatalog::curated(vec![]),
            }
        } else {
            HealthProbe::not_installed(self.before.clone(), ModelCatalog::curated(vec![]))
        }
    }

    async fn install(&self) -> Result<(), HarnessError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        match &self.how {
            Install::Succeeds => {}
            Install::Fails => {
                return Err(HarnessError::Install("npm: registry unreachable".into()));
            }
            Install::Waits(started, release) => {
                started.notify_one();
                release.notified().await;
            }
        }
        *self.installed.lock().unwrap() = true;
        Ok(())
    }

    async fn start(
        &self,
        spec: ExecSpec,
        controls: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError> {
        self.inner.start(spec, controls).await
    }
}

fn set(executors: &[&NeedsAdapter]) -> ExecutorSource {
    let registry = executors
        .iter()
        .fold(ExecutorRegistry::new(), |registry, e| {
            registry.with(Arc::new((*e).clone()))
        });
    ExecutorSource::Given(ExecutorSet::from_registry(registry))
}

macro_rules! daemon_test {
    ($(#[$meta:meta])* async fn $name:ident() $body:block) => {
        $(#[$meta])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() $body
    };
}

daemon_test! {
    async fn an_install_fetches_the_adapter_probes_again_and_publishes_the_result() {
        let hermes = NeedsAdapter::new("mock-hermes", Install::Succeeds);
        let fx = Fixture::with(|c| c.executors = set(&[&hermes])).await;
        let mut watch = within(fx.client.watch_executors()).await.unwrap();
        let _first = within(futures::StreamExt::next(&mut watch)).await;
        assert!(fx.client.supports(reno_proto::FEATURE_EXECUTOR_INSTALL));

        let installed = within(fx.client.install_executor(&id("mock-hermes"))).await.unwrap();
        let health = installed.health.expect("probed after the install");
        assert!(matches!(health.installed, InstallStatus::Installed { .. }), "{health:?}");
        assert_eq!(health.ineligible(), None);
        assert_eq!(hermes.calls(), 1);

        // The list a client watches shows it, and so does a plain list.
        let published = within(async {
            while let Some(snapshot) = futures::StreamExt::next(&mut watch).await {
                let snapshot = snapshot.unwrap();
                let now = snapshot.executors.first().and_then(|e| e.health.as_ref());
                if now.is_some_and(|h| matches!(h.installed, InstallStatus::Installed { .. })) {
                    return snapshot.executors;
                }
            }
            panic!("the watch ended");
        })
        .await;
        assert_eq!(published, within(fx.client.list_executors()).await.unwrap());

        // Nothing is left to install, so a second call says so and installs nothing.
        let again = within(fx.client.install_executor(&id("mock-hermes"))).await.unwrap_err();
        assert_eq!(again.code(), Some(ErrorCode::Conflict));
        assert_eq!(hermes.calls(), 1);
        fx.stop().await;
    }
}

daemon_test! {
    async fn an_executor_that_is_missing_for_another_reason_or_unknown_is_not_installed() {
        let manual = NeedsAdapter::new("mock-manual", Install::Succeeds).missing_for_another_reason();
        let fx = Fixture::with(|c| c.executors = set(&[&manual])).await;

        let refused = within(fx.client.install_executor(&id("mock-manual"))).await.unwrap_err();
        assert_eq!(refused.code(), Some(ErrorCode::Conflict));
        assert!(refused.to_string().contains("install it yourself"), "{refused}");
        assert_eq!(manual.calls(), 0, "no install was run");

        let nowhere = within(fx.client.install_executor(&id("nowhere"))).await.unwrap_err();
        assert_eq!(nowhere.code(), Some(ErrorCode::NotFound));
        fx.stop().await;
    }
}

daemon_test! {
    async fn a_failed_install_is_unavailable_with_the_reason_and_the_health_is_kept_current() {
        let hermes = NeedsAdapter::new("mock-hermes", Install::Fails);
        let fx = Fixture::with(|c| c.executors = set(&[&hermes])).await;

        let failed = within(fx.client.install_executor(&id("mock-hermes"))).await.unwrap_err();
        assert_eq!(failed.code(), Some(ErrorCode::Unavailable));
        assert!(failed.to_string().contains("registry unreachable"), "{failed}");
        let listed = within(fx.client.list_executors()).await.unwrap();
        let health = listed[0].health.as_ref().unwrap();
        assert!(matches!(
            health.installed,
            InstallStatus::NotInstalled { why: NotInstalledReason::NeedsInstall { .. } }
        ));
        // Trying again is allowed: the slot was released.
        let again = within(fx.client.install_executor(&id("mock-hermes"))).await.unwrap_err();
        assert_eq!(again.code(), Some(ErrorCode::Unavailable));
        assert_eq!(hermes.calls(), 2);
        fx.stop().await;
    }
}

daemon_test! {
    async fn a_second_install_of_the_same_executor_is_refused_while_the_first_runs() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let hermes = NeedsAdapter::new(
            "mock-hermes",
            Install::Waits(Arc::clone(&started), Arc::clone(&release)),
        );
        let fx = Fixture::with(|c| c.executors = set(&[&hermes])).await;

        let first = {
            let client = fx.client.clone();
            tokio::spawn(async move { client.install_executor(&id("mock-hermes")).await })
        };
        within(started.notified()).await;
        let second = within(fx.client.install_executor(&id("mock-hermes"))).await.unwrap_err();
        assert_eq!(second.code(), Some(ErrorCode::Conflict));
        assert!(second.to_string().contains("already running"), "{second}");

        release.notify_one();
        let done = within(first).await.unwrap().unwrap();
        assert!(matches!(done.health.unwrap().installed, InstallStatus::Installed { .. }));
        assert_eq!(hermes.calls(), 1);
        fx.stop().await;
    }
}

daemon_test! {
    async fn a_client_that_hangs_up_does_not_leave_the_install_unprobed() {
        let started = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let hermes = NeedsAdapter::new(
            "mock-hermes",
            Install::Waits(Arc::clone(&started), Arc::clone(&release)),
        );
        let fx = Fixture::with(|c| c.executors = set(&[&hermes])).await;
        let mut watch = within(fx.client.watch_executors()).await.unwrap();
        let _first = within(futures::StreamExt::next(&mut watch)).await;

        let caller = {
            let client = fx.client.clone();
            tokio::spawn(async move { client.install_executor(&id("mock-hermes")).await })
        };
        within(started.notified()).await;
        caller.abort();
        let _ = caller.await;

        release.notify_one();
        within(async {
            while let Some(snapshot) = futures::StreamExt::next(&mut watch).await {
                let snapshot = snapshot.unwrap();
                let now = snapshot.executors.first().and_then(|e| e.health.as_ref());
                if now.is_some_and(|h| matches!(h.installed, InstallStatus::Installed { .. })) {
                    return;
                }
            }
            panic!("the watch ended");
        })
        .await;
        fx.stop().await;
    }
}
