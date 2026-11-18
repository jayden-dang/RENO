//! Executor health: what the kernel shows after a probe, what it persists, and how it keeps an
//! executor that is not installed, too old or signed out from being started.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use futures::stream::BoxStream;
use reno_harness::exec::{ExecControls, ExecError, Executor, MockExecutor, MockStep};
use reno_proto::{
    ExecEvent, ExecSpec, ExecutorDescriptor, HealthProbe, InstallStatus, ModelCatalog, ModelSource,
    NotInstalledReason, RunStatus, SignedIn, StopReason, VersionGate,
};

use super::{Fx, START_MS, config, id, last_done, until};
use crate::{ExecutorRegistry, KernelError};

fn installed(signed_in: SignedIn, gate: VersionGate) -> HealthProbe {
    HealthProbe {
        installed: InstallStatus::Installed {
            path: "/bin/mock".into(),
            version: Some("1.0.0".into()),
        },
        version_gate: gate,
        signed_in,
        models: ModelCatalog::curated(vec![]),
    }
}

fn signed_in() -> HealthProbe {
    installed(SignedIn::Yes { account: None }, VersionGate::Ok)
}

pub(super) fn signed_out() -> HealthProbe {
    installed(
        SignedIn::No {
            hint: "run `mock login`".into(),
        },
        VersionGate::Ok,
    )
}

/// An executor whose probe says what the test last set, counts its probes and starts, and can
/// refuse to start as a harness that is not signed in does.
#[derive(Clone)]
pub(super) struct Probed {
    inner: MockExecutor,
    says: Arc<Mutex<HealthProbe>>,
    probes: Arc<AtomicUsize>,
    starts: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
    overlapped: Arc<AtomicBool>,
    refuse_start: Arc<AtomicBool>,
    hang: Arc<AtomicBool>,
    /// What the wire layer says it has not recognised.
    frames: Arc<Mutex<std::collections::BTreeMap<String, u64>>>,
}

impl Probed {
    pub(super) fn new(says: HealthProbe) -> Self {
        Self::named("mock", says)
    }

    fn named(name: &str, says: HealthProbe) -> Self {
        Self {
            inner: MockExecutor::new(id(name)).with_steps([MockStep::text("hi")]),
            says: Arc::new(Mutex::new(says)),
            probes: Arc::default(),
            starts: Arc::default(),
            active: Arc::default(),
            overlapped: Arc::default(),
            refuse_start: Arc::default(),
            hang: Arc::default(),
            frames: Arc::default(),
        }
    }

    fn say(&self, says: HealthProbe) {
        *self.says.lock().unwrap() = says;
    }

    /// Reads the usage of this executor's account from `source`.
    pub(super) fn with_usage(mut self, source: Arc<dyn reno_harness::usage::UsageSource>) -> Self {
        self.inner = self.inner.with_usage_source(source);
        self
    }

    fn probes(&self) -> usize {
        self.probes.load(Ordering::SeqCst)
    }

    fn starts(&self) -> usize {
        self.starts.load(Ordering::SeqCst)
    }

    pub(super) fn registry(&self) -> ExecutorRegistry {
        ExecutorRegistry::new().with(Arc::new(self.clone()))
    }
}

#[async_trait]
impl Executor for Probed {
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
        if self.active.fetch_add(1, Ordering::SeqCst) > 0 {
            self.overlapped.store(true, Ordering::SeqCst);
        }
        self.probes.fetch_add(1, Ordering::SeqCst);
        if self.hang.load(Ordering::SeqCst) {
            std::future::pending::<()>().await;
        }
        // Yield, so overlapping probes would show.
        tokio::task::yield_now().await;
        let says = self.says.lock().unwrap().clone();
        self.active.fetch_sub(1, Ordering::SeqCst);
        says
    }

    async fn start(
        &self,
        spec: ExecSpec,
        controls: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        if self.refuse_start.load(Ordering::SeqCst) {
            return Err(ExecError::NotSignedIn("run `mock login`".into()));
        }
        self.inner.start(spec, controls).await
    }
}

fn diagnostic(done: &ExecEvent) -> &str {
    match done {
        ExecEvent::Done {
            diagnostic: Some(text),
            ..
        } => text,
        other => panic!("not an error Done: {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn an_executor_nobody_probed_is_listed_without_health_and_stays_eligible() {
    let probed = Probed::new(signed_out());
    let fx = Fx::new(config(1), probed.registry()).await;
    let listed = fx.kernel.executors();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].descriptor.id, id("mock"));
    assert!(listed[0].health.is_none());

    // Nothing is known, so the run goes ahead (and would say so itself if it could not).
    let run = fx.submit("go").await;
    fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(probed.starts(), 1);
    assert_eq!(probed.probes(), 0, "a clean run probes nothing");
}

#[tokio::test(start_paused = true)]
async fn a_refresh_probes_stamps_stores_and_publishes() {
    let probed = Probed::new(signed_in());
    let fx = Fx::new(config(1), probed.registry()).await;
    let mut watch = fx.kernel.watch_executors();
    assert!(watch.borrow_and_update()[0].health.is_none());

    let listed = fx.kernel.refresh_executors(None).await.unwrap();
    let health = listed[0].health.clone().expect("probed");
    assert_eq!(health.checked_at_ms, START_MS as u64);
    assert_eq!(health.signed_in, SignedIn::Yes { account: None });
    assert!(watch.has_changed().unwrap());
    assert_eq!(watch.borrow_and_update()[0].health, Some(health.clone()));

    // The latest probe is what the store holds.
    let stored = fx.store.executor_health().await.unwrap();
    assert_eq!(stored, vec![(id("mock"), health)]);
}

#[tokio::test(start_paused = true)]
async fn refreshing_an_executor_that_is_not_registered_is_not_found() {
    let fx = Fx::new(config(1), Probed::new(signed_in()).registry()).await;
    let error = fx
        .kernel
        .refresh_executors(Some(&id("nowhere")))
        .await
        .unwrap_err();
    assert!(
        matches!(error, KernelError::ExecutorNotFound(_)),
        "{error:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_restarted_daemon_shows_the_stored_health_but_does_not_trust_it() {
    let before = Probed::new(signed_out());
    let fx = Fx::new(config(1), before.registry()).await;
    fx.kernel.refresh_executors(None).await.unwrap();
    let (dir, store) = fx.into_store();

    // The user signed in meanwhile. The new daemon has not probed yet.
    let after = Probed::new(signed_in());
    let fx = Fx::over(dir, store, config(1), after.registry()).await;
    let shown = fx.kernel.executors();
    assert!(matches!(
        shown[0].health.as_ref().map(|h| &h.signed_in),
        Some(SignedIn::No { .. })
    ));

    // The stored verdict shows, but a run is not refused by it.
    let run = fx.submit("go").await;
    fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(after.starts(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_signed_out_executor_is_refused_before_it_is_started_and_the_diagnostic_says_why() {
    let probed = Probed::new(signed_out());
    let fx = Fx::new(config(1), probed.registry()).await;
    fx.kernel.refresh_executors(None).await.unwrap();

    let run = fx.submit("go").await;
    fx.wait_status(&run, RunStatus::Failed).await;
    let journal = fx.journal_with_done(&run).await;
    let done = last_done(&journal);
    let text = diagnostic(done);
    assert!(!text.contains("NoEligibleExecutor"), "{text}");
    assert!(text.contains("not signed in"), "{text}");
    assert!(text.contains("run `mock login`"), "{text}");
    assert!(matches!(
        done,
        ExecEvent::Done {
            stop: StopReason::Error,
            ..
        }
    ));
    assert_eq!(probed.starts(), 0, "nothing was spawned");
}

#[tokio::test(start_paused = true)]
async fn not_installed_and_too_old_are_refused_and_unknown_and_untested_are_not() {
    let missing = HealthProbe::not_installed(
        NotInstalledReason::NotFound {
            hint: "install mock".into(),
        },
        ModelCatalog::curated(vec![]),
    );
    let too_old = installed(
        SignedIn::Unknown,
        VersionGate::TooOld {
            min: "2.0.0".into(),
        },
    );
    let untested = installed(
        SignedIn::Unknown,
        VersionGate::Untested {
            verified: "0.9.0".into(),
        },
    );
    for (says, refused, words) in [
        (missing, true, "not installed"),
        (too_old, true, "older than the 2.0.0"),
        (untested, false, ""),
    ] {
        let probed = Probed::new(says);
        let fx = Fx::new(config(1), probed.registry()).await;
        fx.kernel.refresh_executors(None).await.unwrap();
        let run = fx.submit("go").await;
        if refused {
            fx.wait_status(&run, RunStatus::Failed).await;
            let journal = fx.journal_with_done(&run).await;
            let text = diagnostic(last_done(&journal)).to_owned();
            assert!(text.contains(words), "{text}");
            assert_eq!(probed.starts(), 0);
        } else {
            fx.wait_status(&run, RunStatus::Succeeded).await;
            assert_eq!(probed.starts(), 1);
        }
    }
}

#[tokio::test(start_paused = true)]
async fn a_fresh_probe_confirms_an_ineligible_verdict_before_a_run_is_refused() {
    let probed = Probed::new(signed_out());
    let fx = Fx::new(config(1), probed.registry()).await;
    fx.kernel.refresh_executors(None).await.unwrap();
    assert_eq!(probed.probes(), 1);

    // Within the floor the stored verdict stands, with no new probe.
    let run = fx.submit("go").await;
    fx.wait_status(&run, RunStatus::Failed).await;
    assert_eq!(probed.probes(), 1);

    // Later the user has signed in: the run's own check notices, and the run goes ahead.
    fx.clock.advance(60_000);
    probed.say(signed_in());
    let run = fx.submit("again").await;
    fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(probed.probes(), 2);
    assert_eq!(probed.starts(), 1);
    let health = fx.kernel.executors()[0].health.clone().unwrap();
    assert_eq!(health.signed_in, SignedIn::Yes { account: None });
}

#[tokio::test(start_paused = true)]
async fn an_executor_that_refuses_as_not_signed_in_is_probed_again_without_a_timer() {
    let probed = Probed::new(signed_in());
    probed.refuse_start.store(true, Ordering::SeqCst);
    let fx = Fx::new(config(1), probed.registry()).await;
    assert_eq!(probed.probes(), 0);

    let run = fx.submit("go").await;
    fx.wait_status(&run, RunStatus::Failed).await;
    // The refusal was a definite signal: one probe, and it is shown.
    until(async || fx.kernel.executors()[0].health.is_some().then_some(())).await;
    assert_eq!(probed.probes(), 1);

    // Idle again: nothing probes on its own.
    fx.clock.advance(3_600_000);
    tokio::time::advance(Duration::from_secs(3600)).await;
    assert_eq!(probed.probes(), 1);
}

#[tokio::test(start_paused = true)]
async fn probes_of_one_executor_never_overlap_and_a_start_up_probe_runs_in_the_background() {
    let probed = Probed::new(signed_in());
    let fx = Fx::new(config(1), probed.registry()).await;
    fx.kernel.probe_executors_in_background();
    let mock = id("mock");
    let (a, b, c) = tokio::join!(
        fx.kernel.refresh_executors(None),
        fx.kernel.refresh_executors(Some(&mock)),
        fx.kernel.refresh_executors(None),
    );
    for listed in [a, b, c] {
        assert!(listed.unwrap()[0].health.is_some());
    }
    assert!(!probed.overlapped.load(Ordering::SeqCst));
    assert!(probed.probes() >= 3, "each refresh probes");
}

#[tokio::test(start_paused = true)]
async fn a_probe_that_never_finishes_is_given_up_on_and_does_not_block_a_run() {
    let probed = Probed::new(signed_in());
    probed.hang.store(true, Ordering::SeqCst);
    let fx = Fx::new(config(1), probed.registry()).await;
    let refreshing = {
        let kernel = fx.kernel.clone();
        tokio::spawn(async move { kernel.refresh_executors(None).await })
    };
    // Let the probe start, so its deadline is armed before time moves.
    until(async || (probed.probes() == 1).then_some(())).await;
    tokio::time::advance(Duration::from_secs(46)).await;
    let listed = refreshing.await.unwrap().unwrap();
    let health = listed[0].health.clone().expect("recorded");
    assert!(matches!(
        health.installed,
        InstallStatus::NotInstalled {
            why: NotInstalledReason::ProbeFailed { .. }
        }
    ));
    // A probe that could not tell is no reason to refuse a run.
    assert_eq!(health.ineligible(), None);
    let run = fx.submit("go").await;
    fx.wait_status(&run, RunStatus::Succeeded).await;
}

#[tokio::test(start_paused = true)]
async fn the_listed_models_are_the_ones_the_health_labels() {
    let live = ModelCatalog::live(vec![reno_proto::ModelInfo {
        options: Vec::new(),
        default_effort: None,
        efforts: vec![],
        id: "live-1".into(),
        display_name: None,
        description: None,
    }]);
    let probed = Probed::new(HealthProbe {
        models: live.clone(),
        ..signed_in()
    });
    let fx = Fx::new(config(1), probed.registry()).await;
    // Before a probe: the executor's own (curated) list.
    let before = fx.kernel.executors()[0].descriptor.caps.models.clone();
    assert!(before.iter().all(|m| m.id != "live-1"));

    let listed = fx.kernel.refresh_executors(None).await.unwrap();
    assert_eq!(listed[0].descriptor.caps.models, live.models);
    assert_eq!(
        listed[0].health.as_ref().unwrap().models.source,
        ModelSource::Live
    );
}

#[tokio::test(start_paused = true)]
async fn a_bot_goes_to_the_first_of_its_executors_that_is_not_known_to_be_unusable() {
    use reno_proto::defs::DefKind;

    use super::defs::{FakePort, file, setup};
    let bot = file(
        DefKind::Bot,
        "keeper",
        "name = \"keeper\"\ninstructions = \"x\"\nexecutors = [\"codex\", \"mock\"]\n",
    );
    let codex = Probed::named("codex", signed_out());
    let mock = Probed::new(signed_in());
    let registry = ExecutorRegistry::new()
        .with(Arc::new(codex.clone()))
        .with(Arc::new(mock.clone()));
    let port = FakePort::with(vec![bot]);
    let fx = Fx::with_definitions(config(1), registry, setup(&port)).await;

    // Unprobed, the bot's first executor is chosen.
    let first = fx
        .kernel
        .submit_for_bot("keeper", "go", None, "/work".into(), None)
        .await
        .unwrap();
    assert_eq!(
        fx.kernel.get_run(&first).await.unwrap().executor,
        id("codex")
    );
    fx.wait_status(&first, RunStatus::Succeeded).await;

    // Once codex is known to be signed out, the next run is not sent to it.
    fx.kernel.refresh_executors(None).await.unwrap();
    let second = fx
        .kernel
        .submit_for_bot("keeper", "go", None, "/work".into(), None)
        .await
        .unwrap();
    assert_eq!(
        fx.kernel.get_run(&second).await.unwrap().executor,
        id("mock")
    );
    fx.wait_status(&second, RunStatus::Succeeded).await;
}

#[tokio::test(start_paused = true)]
async fn unknown_frames_are_reported_per_executor_with_when_each_kind_was_first_seen() {
    let probed = Probed::new(signed_in());
    let fx = Fx::new(config(1), probed.registry()).await;
    fx.kernel.refresh_executors(None).await.unwrap();
    let before = fx.kernel.executor_frames(None).unwrap();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].id, id("mock"));
    assert_eq!(before[0].cli_version.as_deref(), Some("1.0.0"));
    assert!(before[0].frames.is_empty(), "a clean executor reports none");

    probed.frames.lock().unwrap().insert("type:new".into(), 2);
    let first = fx.kernel.executor_frames(Some(&id("mock"))).unwrap();
    assert_eq!(first[0].frames.len(), 1);
    assert_eq!(first[0].frames[0].kind, "type:new");
    assert_eq!(first[0].frames[0].count, 2);
    let seen_at = first[0].frames[0].first_seen_ms;

    // A run that ends notes the frames too, and a kind keeps the time it was first seen.
    fx.clock.advance(5_000);
    probed.frames.lock().unwrap().insert("type:new".into(), 5);
    probed.frames.lock().unwrap().insert("method:x".into(), 1);
    let run = fx.submit("go").await;
    fx.wait_status(&run, RunStatus::Succeeded).await;
    let later = fx.kernel.executor_frames(None).unwrap();
    let kinds: Vec<(&str, u64)> = later[0]
        .frames
        .iter()
        .map(|f| (f.kind.as_str(), f.count))
        .collect();
    assert_eq!(kinds, [("method:x", 1), ("type:new", 5)]);
    let new = later[0].frames.iter().find(|f| f.kind == "type:new");
    assert_eq!(new.map(|f| f.first_seen_ms), Some(seen_at));
    let x = later[0].frames.iter().find(|f| f.kind == "method:x");
    assert!(x.is_some_and(|f| f.first_seen_ms > seen_at));
}

#[tokio::test(start_paused = true)]
async fn a_new_cli_version_makes_every_kind_new_again() {
    let probed = Probed::new(signed_in());
    let fx = Fx::new(config(1), probed.registry()).await;
    fx.kernel.refresh_executors(None).await.unwrap();
    probed.frames.lock().unwrap().insert("type:a".into(), 1);
    let old = fx.kernel.executor_frames(None).unwrap()[0].frames[0].first_seen_ms;

    fx.clock.advance(60_000);
    let mut newer = signed_in();
    if let InstallStatus::Installed { version, .. } = &mut newer.installed {
        *version = Some("1.1.0".into());
    }
    probed.say(newer);
    fx.kernel.refresh_executors(None).await.unwrap();
    let now = fx.kernel.executor_frames(None).unwrap();
    assert_eq!(now[0].cli_version.as_deref(), Some("1.1.0"));
    assert!(
        now[0].frames.is_empty(),
        "what the old CLI sent is not the new one's: {:?}",
        now[0].frames
    );
    // Only what rises from here counts, and it is new under the new version.
    probed.frames.lock().unwrap().insert("type:a".into(), 3);
    probed.frames.lock().unwrap().insert("type:b".into(), 1);
    let rose = fx.kernel.executor_frames(None).unwrap();
    let seen: Vec<(&str, u64)> = rose[0]
        .frames
        .iter()
        .map(|f| (f.kind.as_str(), f.count))
        .collect();
    assert_eq!(seen, [("type:a", 2), ("type:b", 1)]);
    assert!(rose[0].frames.iter().all(|f| f.first_seen_ms > old));
}

#[tokio::test(start_paused = true)]
async fn frames_of_an_unknown_executor_are_not_found() {
    let fx = Fx::new(config(1), Probed::new(signed_in()).registry()).await;
    let error = fx.kernel.executor_frames(Some(&id("nowhere"))).unwrap_err();
    assert!(
        matches!(error, KernelError::ExecutorNotFound(_)),
        "{error:?}"
    );
}
