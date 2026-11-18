//! Routing (T1.5) through the daemon, served in-process over fake executors: `route.preview`,
//! the decision stored with each run and shown by `run.routing`, the fallback walk when the
//! chosen executor will not start, a Supervised task that no executor may take, and the bot's
//! instructions reaching the executor. No network, no real harness (ARCH-16).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use common::{Fixture, id, within};
use futures::StreamExt;
use futures::stream::BoxStream;
use reno_harness::exec::{ExecControls, ExecError, Executor, MockExecutor};
use reno_kernel::ExecutorRegistry;
use reno_proto::defs::DefKind;
use reno_proto::{
    AccessMode, ErrorCode, ExecEvent, ExecSpec, ExecutorDescriptor, HealthProbe, InstallStatus,
    ModelCatalog, RejectReason, RoutingDecision, RuleMatched, RunId, RunStatus, SignedIn,
    TaskRunParams, VersionGate,
};
use renod::{ExecutorSet, ExecutorSource};

/// An executor with a settable probe, an optional refusal to start as a signed-out harness
/// does, and a mock script underneath.
#[derive(Clone)]
struct Scripted {
    inner: MockExecutor,
    says: Arc<Mutex<HealthProbe>>,
    refuse_start: Arc<AtomicBool>,
}

impl Scripted {
    fn new(name: &str) -> Self {
        Self {
            inner: MockExecutor::new(id(name)),
            says: Arc::new(Mutex::new(probe(SignedIn::Yes { account: None }))),
            refuse_start: Arc::default(),
        }
    }

    fn signed_out(self) -> Self {
        *self.says.lock().unwrap() = probe(SignedIn::No {
            hint: "run `claude login`".into(),
        });
        self
    }

    /// Probes fine, then refuses to start: the board did not know it was signed out.
    fn refusing_to_start(self) -> Self {
        self.refuse_start.store(true, Ordering::SeqCst);
        self
    }

    fn full_access_only(mut self) -> Self {
        let mut descriptor = self.inner.descriptor();
        descriptor.caps.access_modes = vec![AccessMode::FullAccess];
        self.inner = self.inner.with_descriptor(descriptor);
        self
    }
}

fn probe(signed_in: SignedIn) -> HealthProbe {
    HealthProbe {
        installed: InstallStatus::Installed {
            path: "/bin/x".into(),
            version: Some("1.0.0".into()),
        },
        version_gate: VersionGate::Ok,
        signed_in,
        models: ModelCatalog::curated(vec![]),
    }
}

#[async_trait]
impl Executor for Scripted {
    fn descriptor(&self) -> ExecutorDescriptor {
        self.inner.descriptor()
    }

    async fn probe(&self) -> HealthProbe {
        self.says.lock().unwrap().clone()
    }

    async fn start(
        &self,
        spec: ExecSpec,
        controls: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError> {
        if self.refuse_start.load(Ordering::SeqCst) {
            return Err(ExecError::NotSignedIn("run `claude login`".into()));
        }
        self.inner.start(spec, controls).await
    }
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

fn task(bot: Option<&str>) -> TaskRunParams {
    TaskRunParams {
        model_options: Default::default(),
        effort: None,
        prompt: "go".into(),
        executor: None,
        bot: bot.map(str::to_owned),
        cwd: None,
        model: None,
        in_place: false,
        attachments: Vec::new(),
    }
}

async fn write_bot(fx: &Fixture, text: &str) {
    let mut watch = within(fx.client.watch_defs()).await.unwrap();
    let path = fx.paths.defs_dir.join("bots/keeper.toml");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    within(async {
        while let Some(snapshot) = watch.next().await {
            let snapshot = snapshot.unwrap();
            if snapshot
                .defs
                .iter()
                .any(|d| d.kind == DefKind::Bot && d.name == "keeper")
            {
                return;
            }
        }
        panic!("the watch ended");
    })
    .await;
}

const KEEPER: &str = "name = \"keeper\"\ninstructions = \"Be terse.\"\n\
                      executors = [\"claude-code\", \"codex\"]\n";

async fn finished(fx: &Fixture, run: &RunId) -> RunStatus {
    fx.events(run, None).await;
    fx.status(run).await
}

daemon_test! {
    /// ROADMAP T1.5: with Claude Code signed out, a task routed to it goes to Codex, and the
    /// stored decision explains why. The preview said the same before anything started.
    async fn with_claude_code_signed_out_a_bots_task_goes_to_codex_and_the_decision_says_why() {
        let claude = Scripted::new("claude-code").signed_out();
        let codex = Scripted::new("codex");
        let fx = Fixture::with(|c| c.executors = set(&[&claude, &codex])).await;
        within(fx.client.refresh_executors(None)).await.unwrap();
        write_bot(&fx, KEEPER).await;

        let preview = within(fx.client.route_preview(task(Some("keeper")))).await.unwrap();
        assert_eq!(preview.chosen, Some(id("codex")));
        assert!(within(fx.client.list_runs(Default::default())).await.unwrap().is_empty());

        let submitted = within(fx.client.submit_task(task(Some("keeper")))).await.unwrap();
        let decision = submitted.routing.expect("task.run answers with the decision");
        assert_eq!(
            (decision.chosen.clone(), decision.ranked.clone()),
            (preview.chosen.clone(), preview.ranked.clone())
        );
        assert_eq!(finished(&fx, &submitted.run_id).await, RunStatus::Succeeded);

        let run = within(fx.client.get_run(&submitted.run_id)).await.unwrap();
        assert_eq!(run.executor, id("codex"));
        let stored: RoutingDecision = within(fx.client.run_routing(&submitted.run_id))
            .await
            .unwrap()
            .expect("a stored decision");
        assert_eq!(stored.chosen, Some(id("codex")));
        assert_eq!(
            stored.rule_matched,
            RuleMatched::BotPreference { bot: "keeper".into() }
        );
        assert_eq!(stored.rejected.len(), 1);
        assert_eq!(stored.rejected[0].executor, id("claude-code"));
        assert!(matches!(stored.rejected[0].reason, RejectReason::SignedOut { .. }));

        // The bot's instructions reached the executor that ran.
        let specs = codex.inner.log().specs;
        assert_eq!(specs[0].instructions.as_deref(), Some("Be terse."));
        fx.stop().await;
    }
}

daemon_test! {
    async fn a_chosen_executor_that_refuses_to_start_falls_back_and_the_walk_is_stored() {
        let claude = Scripted::new("claude-code").refusing_to_start();
        let codex = Scripted::new("codex");
        let fx = Fixture::with(|c| c.executors = set(&[&claude, &codex])).await;
        within(fx.client.refresh_executors(None)).await.unwrap();
        write_bot(&fx, KEEPER).await;

        let submitted = within(fx.client.submit_task(task(Some("keeper")))).await.unwrap();
        assert_eq!(
            submitted.routing.unwrap().chosen,
            Some(id("claude-code")),
            "it looked fine when it was routed"
        );
        assert_eq!(finished(&fx, &submitted.run_id).await, RunStatus::Succeeded);

        let stored = within(fx.client.run_routing(&submitted.run_id)).await.unwrap().unwrap();
        assert_eq!(stored.chosen, Some(id("codex")));
        assert_eq!(stored.fallbacks.len(), 1);
        assert_eq!(stored.fallbacks[0].from, id("claude-code"));
        assert_eq!(stored.fallbacks[0].to, Some(id("codex")));
        assert!(matches!(stored.fallbacks[0].reason, RejectReason::SignedOut { .. }));
        let run = within(fx.client.get_run(&submitted.run_id)).await.unwrap();
        assert_eq!(run.executor, id("codex"));
        fx.stop().await;
    }
}

daemon_test! {
    /// ROADMAP T1.5: a Supervised task is never routed to a full-access-only executor, and with
    /// no eligible executor it fails with `NoEligibleExecutor`.
    async fn a_supervised_task_with_only_a_full_access_executor_fails_with_no_eligible_executor() {
        let yolo = Scripted::new("claude-code").full_access_only();
        let fx = Fixture::with(|c| c.executors = set(&[&yolo])).await;
        within(fx.client.refresh_executors(None)).await.unwrap();

        let preview = within(fx.client.route_preview(task(None))).await.unwrap();
        assert_eq!(preview.chosen, None);
        assert!(matches!(
            preview.rejected[0].reason,
            RejectReason::AccessNotOffered { requested: AccessMode::Supervised, .. }
        ));

        let submitted = within(fx.client.submit_task(task(None))).await.unwrap();
        assert_eq!(finished(&fx, &submitted.run_id).await, RunStatus::Failed);
        let events = fx.events(&submitted.run_id, None).await;
        let Some(ExecEvent::Done { diagnostic: Some(text), .. }) = events.last().map(|e| &e.event) else {
            panic!("{events:?}");
        };
        assert!(!text.contains("NoEligibleExecutor"), "{text}");
        assert!(yolo.inner.log().specs.is_empty(), "nothing was started");
        let stored = within(fx.client.run_routing(&submitted.run_id)).await.unwrap().unwrap();
        assert_eq!(stored.chosen, None);
        fx.stop().await;
    }
}

daemon_test! {
    /// A task naming nothing goes to Claude Code, then Codex, and never to an `acp.*` agent or
    /// the mock, whatever their ids sort like.
    async fn a_task_naming_nothing_goes_to_claude_code_then_codex_and_never_to_acp_or_mock() {
        let claude = Scripted::new("claude-code");
        let codex = Scripted::new("codex");
        let acp = Scripted::new("acp.antigravity");
        let mock = Scripted::new("mock");
        let fx = Fixture::with(|c| c.executors = set(&[&claude, &codex, &acp, &mock])).await;
        within(fx.client.refresh_executors(None)).await.unwrap();

        let first = within(fx.client.submit_task(task(None))).await.unwrap();
        let decision = first.routing.unwrap();
        assert_eq!(decision.chosen, Some(id("claude-code")));
        assert_eq!(decision.ranked, [id("claude-code"), id("codex")]);
        assert_eq!(finished(&fx, &first.run_id).await, RunStatus::Succeeded);

        // Claude Code signed out: Codex.
        *claude.says.lock().unwrap() = probe(SignedIn::No { hint: "login".into() });
        within(fx.client.refresh_executors(None)).await.unwrap();
        let second = within(fx.client.submit_task(task(None))).await.unwrap();
        assert_eq!(second.routing.unwrap().chosen, Some(id("codex")));
        assert_eq!(finished(&fx, &second.run_id).await, RunStatus::Succeeded);
        assert!(acp.inner.log().specs.is_empty() && mock.inner.log().specs.is_empty());
        fx.stop().await;
    }
}

daemon_test! {
    async fn naming_an_executor_pins_it_and_an_unknown_run_has_no_routing() {
        let claude = Scripted::new("claude-code");
        let codex = Scripted::new("codex");
        let fx = Fixture::with(|c| c.executors = set(&[&claude, &codex])).await;
        within(fx.client.refresh_executors(None)).await.unwrap();

        let mut pinned = task(None);
        pinned.executor = Some(id("codex"));
        let preview = within(fx.client.route_preview(pinned.clone())).await.unwrap();
        assert_eq!(preview.rule_matched, RuleMatched::ExplicitExecutor);
        assert_eq!(preview.ranked, [id("codex")]);
        let submitted = within(fx.client.submit_task(pinned)).await.unwrap();
        assert_eq!(finished(&fx, &submitted.run_id).await, RunStatus::Succeeded);
        assert_eq!(codex.inner.log().specs.len(), 1);
        assert!(claude.inner.log().specs.is_empty());

        // With nothing named and no bot, the router takes the first eligible one by name.
        let unnamed = within(fx.client.submit_task(task(None))).await.unwrap();
        assert_eq!(unnamed.routing.unwrap().rule_matched, RuleMatched::Default);

        let ghost: RunId = "run_nothere".parse().unwrap();
        let error = within(fx.client.run_routing(&ghost)).await.unwrap_err();
        assert_eq!(error.code(), Some(ErrorCode::NotFound));
        fx.stop().await;
    }
}
