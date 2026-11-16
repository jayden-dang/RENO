#![cfg(test)]

//! Routing (T1.5): the router's decision is stored with the run and shown, a run whose chosen
//! executor will not start falls back to the next candidate, a Supervised task never reaches a
//! full-access-only executor, a bot's instructions reach the executor, and a bot-less task
//! writes in place only when it opts in.
//!
//! Fake executors only: nothing here spawns a real harness.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use futures::stream::BoxStream;
use reno_harness::exec::{ExecControls, ExecError, Executor, MockExecutor, MockStep};
use reno_proto::defs::DefKind;
use reno_proto::{
    AccessMode, ExecEvent, ExecSpec, ExecutorDescriptor, HealthProbe, InstallStatus, ModelCatalog,
    RejectReason, RoutingDecision, RuleMatched, RunId, RunStatus, SignedIn, StartFailureKind,
    StopReason, VersionGate,
};

use super::defs::{FakePort, file, setup};
use super::*;
use crate::{FakeWorkspace, TaskRequest};

/// An executor with a settable health, an optional refusal to start, and a count of starts.
#[derive(Clone)]
struct Fake {
    inner: MockExecutor,
    health: Arc<Mutex<HealthProbe>>,
    refuse: Arc<Mutex<Option<Refusal>>>,
    starts: Arc<AtomicUsize>,
}

#[derive(Clone, Copy)]
enum Refusal {
    NotSignedIn,
    NotInstalled,
}

fn healthy() -> HealthProbe {
    HealthProbe {
        installed: InstallStatus::Installed {
            path: "/bin/fake".into(),
            version: Some("1.0.0".into()),
        },
        version_gate: VersionGate::Ok,
        signed_in: SignedIn::Yes { account: None },
        models: ModelCatalog::curated(vec![]),
    }
}

fn signed_out() -> HealthProbe {
    HealthProbe {
        signed_in: SignedIn::No {
            hint: "run `claude login`".into(),
        },
        ..healthy()
    }
}

impl Fake {
    fn new(name: &str) -> Self {
        Self {
            inner: MockExecutor::new(id(name)).with_steps([MockStep::text("hi")]),
            health: Arc::new(Mutex::new(healthy())),
            refuse: Arc::default(),
            starts: Arc::default(),
        }
    }

    fn access(mut self, modes: &[AccessMode]) -> Self {
        let mut descriptor = self.inner.descriptor();
        descriptor.caps.access_modes = modes.to_vec();
        self.inner = self.inner.with_descriptor(descriptor);
        self
    }

    fn aliases(mut self, aliases: &[&str]) -> Self {
        let mut descriptor = self.inner.descriptor();
        descriptor.caps.model_aliases = aliases.iter().map(|a| (*a).to_owned()).collect();
        self.inner = self.inner.with_descriptor(descriptor);
        self
    }

    fn says(self, health: HealthProbe) -> Self {
        *self.health.lock().unwrap() = health;
        self
    }

    fn refusing(self, refusal: Refusal) -> Self {
        *self.refuse.lock().unwrap() = Some(refusal);
        self
    }

    fn starts(&self) -> usize {
        self.starts.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Executor for Fake {
    fn descriptor(&self) -> ExecutorDescriptor {
        self.inner.descriptor()
    }

    async fn probe(&self) -> HealthProbe {
        self.health.lock().unwrap().clone()
    }

    async fn start(
        &self,
        spec: ExecSpec,
        controls: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        let refusal = *self.refuse.lock().unwrap();
        match refusal {
            Some(Refusal::NotSignedIn) => Err(ExecError::NotSignedIn("run `claude login`".into())),
            Some(Refusal::NotInstalled) => Err(ExecError::NotInstalled("install it".into())),
            None => self.inner.start(spec, controls).await,
        }
    }
}

fn with_defaults(names: &[&str]) -> KernelConfig {
    KernelConfig {
        default_executors: names.iter().map(|n| id(n)).collect(),
        ..config(1)
    }
}

fn registry_of(fakes: &[&Fake]) -> ExecutorRegistry {
    fakes
        .iter()
        .fold(ExecutorRegistry::new(), |registry, fake| {
            registry.with(Arc::new((*fake).clone()))
        })
}

fn bot_text(name: &str, executors: &[&str], extra: &str) -> String {
    let list: Vec<String> = executors.iter().map(|e| format!("\"{e}\"")).collect();
    format!(
        "name = \"{name}\"\ninstructions = \"Be terse.\"\nexecutors = [{}]\n{extra}",
        list.join(", ")
    )
}

async fn fx_with_bot(fakes: &[&Fake], bot: &str, executors: &[&str], extra: &str) -> Fx {
    let port = FakePort::with(vec![file(
        DefKind::Bot,
        bot,
        &bot_text(bot, executors, extra),
    )]);
    Fx::with_definitions(config(2), registry_of(fakes), setup(&port)).await
}

async fn diagnostic_of(fx: &Fx, run: &RunId) -> String {
    match fx.journal_with_done(run).await.last().unwrap().1.clone() {
        ExecEvent::Done { diagnostic, .. } => diagnostic.unwrap_or_default(),
        other => panic!("not a Done: {other:?}"),
    }
}

async fn routing_of(fx: &Fx, run: &RunId) -> RoutingDecision {
    fx.kernel
        .run_routing(run)
        .await
        .unwrap()
        .expect("a decision")
}

/// ROADMAP T1.5, acceptance 1, as the board knows it: with Claude Code signed out, a task
/// routed to it goes to Codex, and the stored decision says why.
#[tokio::test(start_paused = true)]
async fn with_claude_code_signed_out_a_routed_task_goes_to_codex_and_the_decision_explains_why() {
    let claude = Fake::new("claude-code").says(signed_out());
    let codex = Fake::new("codex");
    let fx = fx_with_bot(&[&claude, &codex], "keeper", &["claude-code", "codex"], "").await;
    fx.kernel.refresh_executors(None).await.unwrap();

    let run = fx
        .kernel
        .submit_for_bot("keeper", "go", None, PathBuf::from("/work"), None)
        .await
        .unwrap();
    fx.wait_status(&run, RunStatus::Succeeded).await;

    assert_eq!(fx.kernel.get_run(&run).await.unwrap().executor, id("codex"));
    assert_eq!((claude.starts(), codex.starts()), (0, 1));
    let decision = routing_of(&fx, &run).await;
    assert_eq!(decision.chosen, Some(id("codex")));
    assert_eq!(decision.ranked, [id("codex")]);
    assert_eq!(
        decision.rule_matched,
        RuleMatched::BotPreference {
            bot: "keeper".into()
        }
    );
    assert_eq!(decision.rejected.len(), 1);
    assert_eq!(decision.rejected[0].executor, id("claude-code"));
    assert_eq!(
        decision.rejected[0].reason,
        RejectReason::SignedOut {
            hint: "run `claude login`".into()
        }
    );
    assert!(decision.explain().contains("not claude-code: signed out"));
}

/// The same acceptance when the board did not know yet: Claude Code is chosen, will not start
/// because it is signed out, and the run falls back to Codex within the same attempt.
#[tokio::test(start_paused = true)]
async fn a_chosen_executor_that_will_not_start_signed_out_falls_back_and_the_walk_is_recorded() {
    let claude = Fake::new("claude-code").refusing(Refusal::NotSignedIn);
    let codex = Fake::new("codex");
    let fx = fx_with_bot(&[&claude, &codex], "keeper", &["claude-code", "codex"], "").await;

    let run = fx
        .kernel
        .submit_for_bot("keeper", "go", None, PathBuf::from("/work"), None)
        .await
        .unwrap();
    let at_submit = routing_of(&fx, &run).await;
    assert_eq!(at_submit.chosen, Some(id("claude-code")));
    assert!(at_submit.fallbacks.is_empty());

    fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!((claude.starts(), codex.starts()), (1, 1));
    assert_eq!(fx.kernel.get_run(&run).await.unwrap().executor, id("codex"));
    let decision = routing_of(&fx, &run).await;
    assert_eq!(decision.chosen, Some(id("codex")));
    assert_eq!(decision.ranked, [id("codex")]);
    assert_eq!(decision.fallbacks.len(), 1);
    assert_eq!(decision.fallbacks[0].from, id("claude-code"));
    assert_eq!(decision.fallbacks[0].to, Some(id("codex")));
    assert_eq!(
        decision.fallbacks[0].reason,
        RejectReason::SignedOut {
            hint: "run `claude login`".into()
        }
    );
    assert!(matches!(
        decision.rejected.as_slice(),
        [reno_proto::Rejection {
            reason: RejectReason::StartFailed {
                failure: StartFailureKind::NotSignedIn,
                ..
            },
            ..
        }]
    ));
    // The change is an event, so a client can show it.
    let routed = fx
        .store
        .events_since(reno_proto::Seq(0), 10_000)
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "run.routed")
        .count();
    assert_eq!(routed, 2, "once when queued, once for the fallback");
}

#[tokio::test(start_paused = true)]
async fn the_walk_is_bounded_by_the_candidates_and_ends_with_no_eligible_executor() {
    let a = Fake::new("a-first").refusing(Refusal::NotSignedIn);
    let b = Fake::new("b-second").refusing(Refusal::NotInstalled);
    let c = Fake::new("c-third").refusing(Refusal::NotSignedIn);
    let fx = Fx::new(
        with_defaults(&["a-first", "b-second", "c-third"]),
        registry_of(&[&a, &b, &c]),
    )
    .await;

    let (run, decision) = fx
        .kernel
        .submit_task(TaskRequest::new("go", PathBuf::from("/work")))
        .await
        .unwrap();
    assert_eq!(decision.rule_matched, RuleMatched::Default);
    assert_eq!(
        decision.ranked,
        [id("a-first"), id("b-second"), id("c-third")]
    );
    fx.wait_status(&run, RunStatus::Failed).await;

    assert_eq!((a.starts(), b.starts(), c.starts()), (1, 1, 1));
    let why = diagnostic_of(&fx, &run).await;
    assert!(why.starts_with("NoEligibleExecutor:"), "{why}");
    let stored = routing_of(&fx, &run).await;
    assert_eq!(stored.chosen, None);
    assert!(stored.ranked.is_empty());
    assert_eq!(stored.fallbacks.len(), 3);
    assert_eq!(stored.fallbacks[2].to, None);
}

#[tokio::test(start_paused = true)]
async fn a_failure_after_the_executor_started_is_not_a_fallback_case() {
    let first = Fake::new("a-first");
    let first = Fake {
        inner: first
            .inner
            .with_steps([MockStep::text("working"), MockStep::done(StopReason::Error)]),
        ..first
    };
    let second = Fake::new("b-second");
    let fx = Fx::new(
        with_defaults(&["a-first", "b-second"]),
        registry_of(&[&first, &second]),
    )
    .await;

    let run = fx
        .kernel
        .submit_task(TaskRequest::new("go", PathBuf::from("/work")))
        .await
        .unwrap()
        .0;
    fx.wait_status(&run, RunStatus::Failed).await;
    assert_eq!((first.starts(), second.starts()), (1, 0));
    let decision = routing_of(&fx, &run).await;
    assert!(decision.fallbacks.is_empty());
    assert_eq!(decision.chosen, Some(id("a-first")));
}

/// ROADMAP T1.5, acceptance 2: a Supervised task is never routed to a full-access-only
/// executor, and with no eligible executor it fails with `NoEligibleExecutor`.
#[tokio::test(start_paused = true)]
async fn a_supervised_task_never_reaches_a_full_access_only_executor() {
    let yolo = Fake::new("yolo").access(&[AccessMode::FullAccess]);
    let fx = Fx::new(with_defaults(&["yolo"]), registry_of(&[&yolo])).await;

    // The router refuses it, by name or by default.
    for request in [
        TaskRequest::new("go", PathBuf::from("/work")),
        TaskRequest::new("go", PathBuf::from("/work")).executor(id("yolo")),
    ] {
        let preview = fx.kernel.preview_route(&request).unwrap();
        assert_eq!(preview.chosen, None);
        assert_eq!(
            preview.rejected[0].reason,
            RejectReason::AccessNotOffered {
                requested: AccessMode::Supervised,
                offered: vec![AccessMode::FullAccess]
            }
        );
    }

    let (run, decision) = fx
        .kernel
        .submit_task(TaskRequest::new("go", PathBuf::from("/work")))
        .await
        .unwrap();
    assert_eq!(decision.chosen, None);
    fx.wait_status(&run, RunStatus::Failed).await;
    let why = diagnostic_of(&fx, &run).await;
    assert!(why.starts_with("NoEligibleExecutor:"), "{why}");
    assert!(why.contains("Supervised"), "{why}");
    assert_eq!(yolo.starts(), 0, "nothing was spawned");

    // A full-access executor beside it changes nothing for a Supervised task: it goes to the
    // one that offers Supervised.
    let safe = Fake::new("safe");
    let fx = Fx::new(
        with_defaults(&["yolo", "safe"]),
        registry_of(&[&yolo, &safe]),
    )
    .await;
    let (run, decision) = fx
        .kernel
        .submit_task(TaskRequest::new("go", PathBuf::from("/work")))
        .await
        .unwrap();
    assert_eq!(decision.chosen, Some(id("safe")));
    fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!((yolo.starts(), safe.starts()), (0, 1));
}

#[tokio::test(start_paused = true)]
async fn an_executor_named_by_the_task_is_the_only_candidate_and_no_fallback_beyond_it() {
    let claude = Fake::new("claude-code").refusing(Refusal::NotSignedIn);
    let codex = Fake::new("codex");
    let fx = Fx::new(config(1), registry_of(&[&claude, &codex])).await;

    let (run, decision) = fx
        .kernel
        .submit_task(TaskRequest::new("go", PathBuf::from("/work")).executor(id("claude-code")))
        .await
        .unwrap();
    assert_eq!(decision.rule_matched, RuleMatched::ExplicitExecutor);
    assert_eq!(decision.ranked, [id("claude-code")]);
    fx.wait_status(&run, RunStatus::Failed).await;
    assert_eq!(codex.starts(), 0);
    assert!(
        diagnostic_of(&fx, &run).await.contains("signed in"),
        "the executor's own refusal is the diagnostic"
    );
}

#[tokio::test(start_paused = true)]
async fn a_preview_decides_like_a_submit_and_creates_nothing() {
    let claude = Fake::new("claude-code").says(signed_out());
    let codex = Fake::new("codex");
    let fx = fx_with_bot(&[&claude, &codex], "keeper", &["claude-code", "codex"], "").await;
    fx.kernel.refresh_executors(None).await.unwrap();

    let request = TaskRequest::new("go", PathBuf::from("/work")).bot("keeper");
    let preview = fx.kernel.preview_route(&request).unwrap();
    assert_eq!(fx.kernel.count_runs(&RunStatus::ALL).await.unwrap(), 0);
    let (_, submitted) = fx.kernel.submit_task(request).await.unwrap();
    assert_eq!(preview, submitted);
    assert_eq!(preview.chosen, Some(id("codex")));
}

#[tokio::test(start_paused = true)]
async fn the_pinned_bot_versions_instructions_reach_the_executor() {
    let codex = Fake::new("codex");
    let fx = fx_with_bot(&[&codex], "keeper", &["codex"], "").await;
    let plain = fx
        .kernel
        .submit("go", id("codex"), PathBuf::from("/work"), None)
        .await
        .unwrap();
    let bot_run = fx
        .kernel
        .submit_for_bot("keeper", "go", None, PathBuf::from("/work"), None)
        .await
        .unwrap();
    fx.wait_status(&plain, RunStatus::Succeeded).await;
    fx.wait_status(&bot_run, RunStatus::Succeeded).await;

    let specs = codex.inner.log().specs;
    let by_run = |run: &RunId| specs.iter().find(|s| &s.run_id == run).unwrap();
    assert_eq!(by_run(&bot_run).instructions.as_deref(), Some("Be terse."));
    assert_eq!(by_run(&plain).instructions, None);
}

#[tokio::test(start_paused = true)]
async fn a_bot_less_task_writes_in_place_only_when_it_opts_in_and_never_in_a_repository() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let m = mock([]);
    let fx = Fx::with_workspace(
        config(2),
        registry(std::slice::from_ref(&m)),
        None,
        ws.clone(),
    )
    .await;
    let go = |cwd: &'static str, in_place: bool| {
        let kernel = fx.kernel.clone();
        async move {
            kernel
                .submit_task(
                    TaskRequest::new("go", PathBuf::from(cwd))
                        .executor(id("mock"))
                        .in_place(in_place),
                )
                .await
                .unwrap()
                .0
        }
    };

    // Refused with no opt-in, and the diagnostic says how to opt in.
    let refused = go("/plain", false).await;
    fx.wait_status(&refused, RunStatus::Failed).await;
    let why = diagnostic_of(&fx, &refused).await;
    assert!(why.contains("WorkspaceRefused"), "{why}");

    // Opted in: works where it is.
    let opted = go("/plain", true).await;
    fx.wait_status(&opted, RunStatus::Succeeded).await;
    assert_eq!(m.log().specs[0].cwd, PathBuf::from("/plain"));

    // Opting in never lets a repository be written in place.
    let in_repo = go("/repo", true).await;
    fx.wait_status(&in_repo, RunStatus::Succeeded).await;
    assert_ne!(m.log().specs[1].cwd, PathBuf::from("/repo"));
}

/// A task with no bot and no named executor goes only to the configured defaults, in order:
/// never to an `acp.*` agent or the mock, however they sort.
#[tokio::test(start_paused = true)]
async fn a_task_naming_nothing_goes_to_claude_code_then_codex_and_never_to_acp_or_mock() {
    let claude = Fake::new("claude-code");
    let codex = Fake::new("codex");
    let acp = Fake::new("acp.antigravity");
    let mock = Fake::new("mock");
    let all = [&claude, &codex, &acp, &mock];
    let fx = Fx::new(with_defaults(&["claude-code", "codex"]), registry_of(&all)).await;
    let request = || TaskRequest::new("go", PathBuf::from("/work"));

    let (run, decision) = fx.kernel.submit_task(request()).await.unwrap();
    assert_eq!(decision.rule_matched, RuleMatched::Default);
    assert_eq!(decision.chosen, Some(id("claude-code")));
    assert_eq!(decision.ranked, [id("claude-code"), id("codex")]);
    let left_out: Vec<&str> = decision
        .rejected
        .iter()
        .map(|r| r.executor.as_str())
        .collect();
    assert_eq!(left_out, ["acp.antigravity", "mock"]);
    assert!(
        decision
            .rejected
            .iter()
            .all(|r| matches!(r.reason, RejectReason::Excluded { .. }))
    );
    fx.wait_status(&run, RunStatus::Succeeded).await;

    // Signed out, Claude Code falls to Codex.
    *claude.health.lock().unwrap() = signed_out();
    fx.kernel.refresh_executors(None).await.unwrap();
    let (run, decision) = fx.kernel.submit_task(request()).await.unwrap();
    assert_eq!(decision.chosen, Some(id("codex")));
    fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!((acp.starts(), mock.starts()), (0, 0));

    // With both signed out nothing runs, and the diagnostic says what to do.
    *codex.health.lock().unwrap() = signed_out();
    fx.kernel.refresh_executors(None).await.unwrap();
    let (run, decision) = fx.kernel.submit_task(request()).await.unwrap();
    assert_eq!(decision.chosen, None);
    fx.wait_status(&run, RunStatus::Failed).await;
    let why = diagnostic_of(&fx, &run).await;
    assert!(why.starts_with("NoEligibleExecutor:"), "{why}");
    assert!(why.contains("name an executor"), "{why}");
    assert_eq!((acp.starts(), mock.starts()), (0, 0));
}

#[tokio::test(start_paused = true)]
async fn with_no_default_executors_a_task_naming_nothing_fails_and_says_to_name_one() {
    let acp = Fake::new("acp.antigravity");
    let fx = Fx::new(config(1), registry_of(&[&acp])).await;
    let (run, decision) = fx
        .kernel
        .submit_task(TaskRequest::new("go", PathBuf::from("/work")))
        .await
        .unwrap();
    assert_eq!(decision.chosen, None);
    fx.wait_status(&run, RunStatus::Failed).await;
    let why = diagnostic_of(&fx, &run).await;
    assert!(
        why.starts_with("NoEligibleExecutor:") && why.contains("use a bot"),
        "{why}"
    );
    assert_eq!(acp.starts(), 0);
}

// ---- a bot's model --------------------------------------------------------------------------

/// A run submitted as `keeper` with this task model, and what the executor was asked for.
async fn model_asked(fx: &Fx, fake: &Fake, task_model: Option<&str>) -> (RunId, Option<String>) {
    let before = fake.inner.log().specs.len();
    let run = fx
        .kernel
        .submit_for_bot(
            "keeper",
            "go",
            None,
            PathBuf::from("/work"),
            task_model.map(str::to_owned),
        )
        .await
        .unwrap();
    fx.wait_status(&run, RunStatus::Succeeded).await;
    let specs = fake.inner.log().specs;
    assert_eq!(specs.len(), before + 1);
    (run, specs[before].model.clone())
}

#[tokio::test(start_paused = true)]
async fn a_runs_model_is_the_tasks_then_the_bots_then_the_executors_default() {
    let fake = Fake::new("mock");
    let pinned = fx_with_bot(&[&fake], "keeper", &["mock"], "model = \"mock-1\"").await;

    // The bot's model, when the task names none; the run shows it as its own.
    let (run, asked) = model_asked(&pinned, &fake, None).await;
    assert_eq!(asked.as_deref(), Some("mock-1"));
    assert_eq!(
        pinned.kernel.get_run(&run).await.unwrap().model.as_deref(),
        Some("mock-1")
    );
    // The task's own model wins over the bot's.
    let (run, asked) = model_asked(&pinned, &fake, Some("mock-2")).await;
    assert_eq!(asked.as_deref(), Some("mock-2"));
    assert_eq!(
        pinned.kernel.get_run(&run).await.unwrap().model.as_deref(),
        Some("mock-2")
    );

    // A bot with no model leaves the executor's default alone.
    let plain_fake = Fake::new("mock");
    let plain = fx_with_bot(&[&plain_fake], "keeper", &["mock"], "").await;
    let (run, asked) = model_asked(&plain, &plain_fake, None).await;
    assert_eq!(asked, None);
    assert_eq!(plain.kernel.get_run(&run).await.unwrap().model, None);
}

#[tokio::test(start_paused = true)]
async fn a_queued_run_keeps_the_model_of_the_bot_version_it_was_made_under() {
    let port = FakePort::with(vec![file(
        DefKind::Bot,
        "keeper",
        &bot_text("keeper", &["mock"], "model = \"mock-1\""),
    )]);
    // One slot, held by a run that does not end, so the second run waits in the queue.
    let held = Fake {
        inner: MockExecutor::new(id("mock"))
            .with_steps([MockStep::Sleep(std::time::Duration::from_secs(3600))]),
        ..Fake::new("mock")
    };
    let fx = Fx::with_definitions(config(1), registry_of(&[&held]), setup(&port)).await;
    let first = fx
        .kernel
        .submit_for_bot("keeper", "hold", None, PathBuf::from("/work"), None)
        .await
        .unwrap();
    fx.wait_status(&first, RunStatus::Running).await;
    let queued = fx
        .kernel
        .submit_for_bot("keeper", "wait", None, PathBuf::from("/work"), None)
        .await
        .unwrap();
    // The bot is edited afterwards; the queued run keeps the model it was made under.
    port.set(vec![file(
        DefKind::Bot,
        "keeper",
        &bot_text("keeper", &["mock"], "model = \"mock-2\""),
    )]);
    port.touch_and_wait().await;
    assert_eq!(
        fx.kernel.get_run(&queued).await.unwrap().model.as_deref(),
        Some("mock-1")
    );
}

#[tokio::test(start_paused = true)]
async fn a_model_the_executor_does_not_list_is_a_warning_and_never_a_refusal() {
    let fake = Fake::new("mock");
    let fx = fx_with_bot(
        &[&fake],
        "keeper",
        &["mock"],
        "model = \"mock-not-in-the-list\"",
    )
    .await;
    // The run starts, and asks for the model as given.
    let (run, asked) = model_asked(&fx, &fake, None).await;
    assert_eq!(asked.as_deref(), Some("mock-not-in-the-list"));
    // The stored decision and the preview carry the warning.
    let decision = routing_of(&fx, &run).await;
    assert_eq!(decision.chosen, Some(id("mock")));
    assert_eq!(decision.warnings.len(), 1, "{:?}", decision.warnings);
    assert!(
        decision.warnings[0].contains("mock-not-in-the-list")
            && decision.warnings[0].contains("mock-1"),
        "{:?}",
        decision.warnings
    );
    let preview = fx
        .kernel
        .preview_route(&TaskRequest::new("go", PathBuf::from("/work")).bot("keeper"))
        .unwrap();
    assert_eq!(preview.warnings, decision.warnings);

    // A listed model warns of nothing; a task's own unlisted model does.
    let listed = Fake::new("mock");
    let fx = fx_with_bot(&[&listed], "keeper", &["mock"], "model = \"mock-1\"").await;
    let (run, _) = model_asked(&fx, &listed, None).await;
    assert!(routing_of(&fx, &run).await.warnings.is_empty());
    let (run, _) = model_asked(&fx, &listed, Some("mock-zzz")).await;
    assert_eq!(routing_of(&fx, &run).await.warnings.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_model_alias_the_executor_accepts_is_not_a_warning() {
    for model in ["sonnet", "opus[1m]", "mock-1[1m]"] {
        let fake = Fake::new("mock").aliases(&["sonnet", "opus", "opus[1m]"]);
        let fx = fx_with_bot(&[&fake], "keeper", &["mock"], &format!("model = {model:?}")).await;
        let (run, asked) = model_asked(&fx, &fake, None).await;
        assert_eq!(asked.as_deref(), Some(model));
        assert!(routing_of(&fx, &run).await.warnings.is_empty(), "{model}");
    }
    // Without aliases the same name warns, and so does a suffix the executor does not know.
    let plain = Fake::new("mock");
    let fx = fx_with_bot(&[&plain], "keeper", &["mock"], "model = \"sonnet\"").await;
    let (run, _) = model_asked(&fx, &plain, None).await;
    assert_eq!(routing_of(&fx, &run).await.warnings.len(), 1);
    let (run, _) = model_asked(&fx, &plain, Some("mock-1[1m]")).await;
    assert_eq!(routing_of(&fx, &run).await.warnings.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_trigger_fired_run_asks_for_its_bots_model() {
    let fake = Fake::new("mock");
    let port = FakePort::with(vec![
        file(
            DefKind::Bot,
            "keeper",
            &bot_text("keeper", &["mock"], "model = \"mock-1\""),
        ),
        file(
            DefKind::Trigger,
            "poke",
            "name = \"poke\"\n[when]\nkind = \"manual\"\n[target]\nbot = \"keeper\"\nprompt = \"go\"\n",
        ),
    ]);
    let fx = Fx::with_definitions(config(2), registry_of(&[&fake]), setup(&port)).await;
    let fired = fx.kernel.fire_trigger("poke").await.unwrap();
    let run = fired.run_id.expect("a run");
    fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(fake.inner.log().specs[0].model.as_deref(), Some("mock-1"));
    assert_eq!(
        fx.kernel.get_run(&run).await.unwrap().model.as_deref(),
        Some("mock-1")
    );
}
