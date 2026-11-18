//! `executor.list`, `executor.refresh` and `executors.watch` on a daemon served in-process, over
//! executors whose probes the test controls. No network, no real harness (ARCH-16).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{Fixture, id, within};
use futures::StreamExt;
use futures::stream::BoxStream;
use reno_harness::exec::{ExecControls, ExecError, Executor, MockExecutor};
use reno_kernel::ExecutorRegistry;
use reno_proto::{
    ErrorCode, ExecEvent, ExecSpec, ExecutorDescriptor, ExecutorInfo, HealthProbe, InstallStatus,
    ModelCatalog, NotInstalledReason, RunStatus, SignedIn, TaskRunParams, VersionGate,
};
use renod::{ExecutorSet, ExecutorSource};

/// An executor whose probe says what the test last set.
#[derive(Clone)]
struct Scripted {
    inner: MockExecutor,
    says: Arc<Mutex<HealthProbe>>,
    /// Never answer a probe.
    silent: bool,
    /// What the wire layer says it did not recognise.
    frames: Arc<Mutex<std::collections::BTreeMap<String, u64>>>,
}

impl Scripted {
    fn new(name: &str, says: HealthProbe) -> Self {
        Self {
            inner: MockExecutor::new(id(name)),
            says: Arc::new(Mutex::new(says)),
            silent: false,
            frames: Arc::default(),
        }
    }

    fn with_usage(mut self, source: Arc<dyn reno_harness::usage::UsageSource>) -> Self {
        self.inner = self.inner.with_usage_source(source);
        self
    }

    fn silent(mut self) -> Self {
        self.silent = true;
        self
    }

    fn say(&self, says: HealthProbe) {
        *self.says.lock().unwrap() = says;
    }
}

#[async_trait]
impl Executor for Scripted {
    fn descriptor(&self) -> ExecutorDescriptor {
        self.inner.descriptor()
    }

    fn unknown_frames(&self) -> std::collections::BTreeMap<String, u64> {
        self.frames.lock().unwrap().clone()
    }

    async fn plan_usage(&self) -> Option<reno_harness::usage::UsageReading> {
        self.inner.plan_usage().await
    }

    async fn probe(&self) -> HealthProbe {
        if self.silent {
            std::future::pending::<()>().await;
        }
        self.says.lock().unwrap().clone()
    }

    async fn start(
        &self,
        spec: ExecSpec,
        controls: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError> {
        self.inner.start(spec, controls).await
    }
}

fn healthy() -> HealthProbe {
    HealthProbe {
        installed: InstallStatus::Installed {
            path: "/bin/x".into(),
            version: Some("1.0.0".into()),
        },
        version_gate: VersionGate::Ok,
        signed_in: SignedIn::Yes { account: None },
        models: ModelCatalog::curated(vec![]),
    }
}

fn missing() -> HealthProbe {
    HealthProbe::not_installed(
        NotInstalledReason::NotFound {
            hint: "install it".into(),
        },
        ModelCatalog::curated(vec![]),
    )
}

fn health_of<'a>(list: &'a [ExecutorInfo], name: &str) -> Option<&'a reno_proto::ExecutorHealth> {
    list.iter()
        .find(|e| e.descriptor.id.as_str() == name)
        .and_then(|e| e.health.as_ref())
}

fn set(executors: &[&Scripted]) -> ExecutorSource {
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
    async fn the_daemon_probes_at_start_and_clients_see_every_executor_with_its_health() {
        let good = Scripted::new("mock", healthy());
        let gone = Scripted::new("mock-gone", missing());
        let fx = Fixture::with(|c| c.executors = set(&[&good, &gone])).await;

        // The start-up probe lands without anyone asking: follow the watch until it has.
        let mut watch = within(fx.client.watch_executors()).await.unwrap();
        let landed = within(async {
            while let Some(snapshot) = watch.next().await {
                let snapshot = snapshot.unwrap();
                if snapshot.executors.iter().all(|e| e.health.is_some()) {
                    return snapshot.executors;
                }
            }
            panic!("the watch ended");
        })
        .await;
        assert_eq!(landed.len(), 2);
        assert_eq!(landed, within(fx.client.list_executors()).await.unwrap());
        assert_eq!(health_of(&landed, "mock").unwrap().ineligible(), None);
        assert!(health_of(&landed, "mock-gone").unwrap().ineligible().is_some());

        // `daemon.status` names the executors found installed, not the missing one.
        let status = within(fx.client.status()).await.unwrap();
        let names: Vec<&str> = status.executors.iter().map(|e| e.as_str()).collect();
        assert_eq!(names, ["mock"]);
        fx.stop().await;
    }
}

daemon_test! {
    async fn refresh_probes_again_answers_with_the_list_and_publishes_it() {
        let flaky = Scripted::new("mock", missing());
        let fx = Fixture::with(|c| c.executors = set(&[&flaky])).await;
        let mut watch = within(fx.client.watch_executors()).await.unwrap();
        let _first = within(watch.next()).await;

        // The harness is installed after the daemon started; nobody polls, a person says so.
        flaky.say(healthy());
        let refreshed = within(fx.client.refresh_executors(Some(id("mock")))).await.unwrap();
        assert_eq!(health_of(&refreshed, "mock").unwrap().ineligible(), None);
        let published = within(async {
            while let Some(snapshot) = watch.next().await {
                let snapshot = snapshot.unwrap();
                if health_of(&snapshot.executors, "mock").is_some_and(|h| h.ineligible().is_none()) {
                    return snapshot.executors;
                }
            }
            panic!("the watch ended");
        })
        .await;
        assert_eq!(published, refreshed);

        // A refresh of everything works too, and an id nobody has is not found.
        let all = within(fx.client.refresh_executors(None)).await.unwrap();
        assert_eq!(all.len(), 1);
        let error = within(fx.client.refresh_executors(Some(id("nowhere")))).await.unwrap_err();
        assert_eq!(error.code(), Some(ErrorCode::NotFound));
        fx.stop().await;
    }
}

/// A usage source that says one Session meter, and how many times it was asked.
#[derive(Debug, Default)]
struct Meter {
    reads: std::sync::atomic::AtomicUsize,
}

#[async_trait]
impl reno_harness::usage::UsageSource for Meter {
    async fn read(&self) -> Option<reno_harness::usage::UsageReading> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Some(reno_harness::usage::UsageReading {
            plan_label: None,
            windows: vec![reno_proto::UsageWindow::new("Session", 0.54, None)],
        })
    }
}

daemon_test! {
    async fn plan_usage_rides_on_the_health_and_a_usage_only_refresh_is_throttled() {
        let meter = Arc::new(Meter::default());
        let claude = Scripted::new("mock", healthy()).with_usage(meter.clone());
        let fx = Fixture::with(|c| c.executors = set(&[&claude])).await;
        assert!(fx.client.supports(reno_proto::FEATURE_EXECUTOR_USAGE));

        // The start-up probe reads the usage once; follow the watch until it shows.
        let mut watch = within(fx.client.watch_executors()).await.unwrap();
        let landed = within(async {
            while let Some(snapshot) = watch.next().await {
                let snapshot = snapshot.unwrap();
                if health_of(&snapshot.executors, "mock").is_some_and(|h| h.plan_usage.is_some()) {
                    return snapshot.executors;
                }
            }
            panic!("the watch ended");
        })
        .await;
        let usage = health_of(&landed, "mock").unwrap().plan_usage.clone().unwrap();
        assert_eq!(usage.windows[0].label, "Session");
        assert_eq!(usage.windows[0].used_permille, 540);

        // Asking again inside 30 s answers from what is held: the source is not asked again.
        let again = within(fx.client.refresh_executor_usage(Some(id("mock")))).await.unwrap();
        assert_eq!(health_of(&again, "mock").unwrap().plan_usage, Some(usage));
        within(fx.client.refresh_executors(None)).await.unwrap();
        assert_eq!(meter.reads.load(std::sync::atomic::Ordering::SeqCst), 1);
        let error = within(fx.client.refresh_executor_usage(Some(id("nowhere")))).await.unwrap_err();
        assert_eq!(error.code(), Some(ErrorCode::NotFound));
        fx.stop().await;
    }
}

daemon_test! {
    async fn a_run_on_an_executor_that_is_not_installed_fails_with_the_reason_before_any_spawn() {
        let gone = Scripted::new("mock", missing());
        let fx = Fixture::with(|c| c.executors = set(&[&gone])).await;
        within(fx.client.refresh_executors(None)).await.unwrap();

        let run = within(fx.client.run_task(TaskRunParams {
            effort: None,
            prompt: "go".into(),
            executor: Some(id("mock")),
            bot: None,
            cwd: None,
            model: None,
            in_place: false,
            attachments: Vec::new(),
        }))
        .await
        .unwrap();
        let events = fx.events(&run, None).await;
        let Some(ExecEvent::Done { diagnostic: Some(text), .. }) = events.last().map(|e| &e.event) else {
            panic!("{events:?}");
        };
        assert!(!text.contains("NoEligibleExecutor"), "{text}");
        assert!(text.contains("not installed"), "{text}");
        let summary = within(fx.client.get_run(&run)).await.unwrap();
        assert_eq!(summary.status, RunStatus::Failed);
        fx.stop().await;
    }
}

daemon_test! {
    async fn the_health_a_daemon_found_is_shown_at_once_by_the_next_one() {
        let first = Scripted::new("mock", healthy());
        let fx = Fixture::with(|c| c.executors = set(&[&first])).await;
        within(fx.client.refresh_executors(None)).await.unwrap();
        let (dir, paths, _) = fx.stop().await;

        // The next daemon's executor cannot be probed in time (it never answers), so what the
        // client sees is what the first one stored.
        let silent = Scripted::new("mock", healthy()).silent();
        let mut config = common::config(&dir, &common::Executors::new());
        config.executors = set(&[&silent]);
        let daemon = within(renod::Daemon::start(config)).await.unwrap();
        let client = within(reno_client::Client::connect_unix(&paths.socket)).await.unwrap();
        let listed = within(client.list_executors()).await.unwrap();
        assert!(health_of(&listed, "mock").is_some(), "stored health is shown");
        daemon.request_shutdown();
        within(daemon.wait()).await.unwrap();
    }
}

daemon_test! {
    async fn frames_a_wire_layer_did_not_recognise_are_asked_for_by_kind() {
        let noisy = Scripted::new("mock", healthy());
        noisy.frames.lock().unwrap().insert("type:brand_new".into(), 4);
        let fx = Fixture::with(|c| c.executors = set(&[&noisy])).await;
        within(fx.client.refresh_executors(None)).await.unwrap();

        let all = within(fx.client.executor_frames(None)).await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, id("mock"));
        assert_eq!(all[0].cli_version.as_deref(), Some("1.0.0"));
        assert_eq!(all[0].frames.len(), 1);
        assert_eq!(all[0].frames[0].kind, "type:brand_new");
        assert_eq!(all[0].frames[0].count, 4);
        assert!(all[0].frames[0].first_seen_ms > 0);

        let one = within(fx.client.executor_frames(Some(id("mock")))).await.unwrap();
        assert_eq!(one, all);
        let error = within(fx.client.executor_frames(Some(id("nowhere")))).await.unwrap_err();
        assert_eq!(error.code(), Some(ErrorCode::NotFound));
        fx.stop().await;
    }
}

daemon_test! {
    async fn the_daemons_mock_keeps_no_record_of_the_runs_it_played() {
        // The daemon's mock plays every run for as long as the daemon lives; a log of what it
        // received grew by a few KiB per run (the bench's memory budget caught it).
        let fx = Fixture::start().await;
        for _ in 0..3 {
            let run = within(fx.client.run_task(TaskRunParams {
                effort: None,
                prompt: "go".into(),
                executor: Some(id("mock")),
                bot: None,
                cwd: None,
                model: None,
                in_place: false,
                attachments: Vec::new(),
            }))
            .await
            .unwrap();
            let events = fx.events(&run, None).await;
            assert!(matches!(events.last().map(|e| &e.event), Some(ExecEvent::Done { .. })));
        }
        assert_eq!(fx.executors.mock.log(), reno_harness::exec::MockLog::default());
        fx.stop().await;
    }
}
