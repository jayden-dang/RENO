//! An in-process fake daemon, behind the cargo feature `fake`.
//!
//! [`FakeDaemon`] serves every Phase 0 method and the approval methods from a
//! [`reno_rpc::Router`], over the in-memory transport, so the app and the tests exercise the real
//! framing, subscriptions and gap handling. It does no real I/O, opens no network connection and
//! reads no wall clock: time in its data is a logical counter, and pacing uses tokio time, which
//! tests pause with `#[tokio::test(start_paused = true)]`.
//!
//! # Demo state
//!
//! See [`data`]: bots keeper, scout, builder, reviewer and architect; repositories atlas-api,
//! web-console and reno; six runs in six states; two held approvals (keeper's
//! `git push -u origin reno/keeper/deps-1115` and scout's network fetch).
//!
//! # Scripts
//!
//! `task.run` creates a queued run and starts a script, one step every [`FakeConfig::step`]:
//! `leased`, `running` (+ `Started`), text, a `bash ls` tool call and its result, usage, then by
//! prompt:
//!
//! - default: text, `evaluating`, `Done(completed)`, `succeeded`;
//! - containing `push`: a `git push -u origin HEAD` tool call, then `NeedsApproval` and
//!   `waiting_approval`, with a pending approval;
//! - containing `fetch` or `network`: the same with a network fetch;
//! - `/burst N`: N text events with no pause between them, then the default ending. This is how
//!   tests overflow a subscription.
//!
//! `approval.decide` removes the approval and resumes the run: approve ends `succeeded`, deny ends
//! `failed` with `Done(error, "approval denied")`. Held demo runs resume the same way.
//! `run.steer` on a live run journals a `UserMessage` at once, then a short scripted reply one
//! step later; on a finished run it is a conflict. `run.cancel` ends a run `cancelled` with `Done(interrupted)`.
//!
//! # Lifecycle (ARCH-11)
//!
//! Every task the fake spawns (scripts, and the server's connections) is owned by a
//! [`TaskTracker`] and stops on a [`CancellationToken`]. Dropping the [`FakeDaemon`], or the last
//! clone of a client made by [`FakeDaemon::spawn`], cancels them; [`FakeDaemon::shutdown`] also
//! waits.

mod activity;
mod changes;
pub mod data;
mod extras;
pub mod files;
mod history;
mod jobs;
mod questions;
mod registry;
mod script;
mod signin;
pub mod terminals;
mod world;

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::{Stream, stream};
use reno_proto::defs::{DefKind, DefValidateResult, DefsSnapshot};
use reno_proto::{
    Approval, ApprovalDecide, ApprovalDecideResult, ApprovalList, ApprovalListResult,
    ApprovalsSnapshot, ApprovalsWatch, DaemonHello, DaemonShutdown, DaemonShutdownResult,
    DaemonStatus, DaemonStatusInfo, ErrorCode, ErrorObject, FEATURES, HelloResult, NoParams,
    PROTOCOL_VERSION, RunCancel, RunCancelResult, RunEventItem, RunEvents, RunGet, RunId, RunList,
    RunListResult, RunSteer, RunSteerResult, RunSummary, RunsSnapshot, RunsWatch, SubGap,
    SubGapParams, TaskRun, TaskRunResult,
};
use reno_rpc::{Router, Server, ServerConfig, ServerStats, SubItem, connect_in_memory_with_buffer};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::{Client, ClientError};
use world::World;

/// Knobs of the fake daemon. The defaults suit most tests.
#[derive(Debug, Clone)]
pub struct FakeConfig {
    /// Pause between script steps, on tokio time. Default 50 ms.
    pub step: Duration,
    /// Capacities of the RPC server, for tests that provoke overflow. Default: the real defaults.
    pub server: ServerConfig,
    /// Buffer of the in-memory pipe, per direction, in bytes. Default 64 KiB.
    pub pipe_buffer: usize,
    /// The protocol version `daemon.hello` answers with. Default [`PROTOCOL_VERSION`]; set it
    /// to anything else to see a client report `Incompatible`.
    pub protocol_version: u32,
    /// Features the daemon does not negotiate even when the client offers them, for example
    /// [`reno_proto::FEATURE_DEFS`]. Default none.
    pub without_features: Vec<String>,
    /// Start with no runs, approvals, triggers, definitions or history. Default false.
    pub empty: bool,
    /// How long a simulated executor install or update takes, on tokio time. Default 3 s.
    pub job_duration: Duration,
    /// Start with agents added from the ACP registry (see the `registry` module for each one's
    /// state): the demo `--fake` runs with them, tests that count executors do not. The registry
    /// index itself is always served. Default false.
    pub registry_agents: bool,
    /// Add a run waiting on a question from its agent (`WaitingInput`) to the demo world, so the
    /// "Needs input" badge and the All runs lane's "asks" show in `--fake`; tests that count runs
    /// do not have it. Default false.
    pub question_run: bool,
    /// With `question_run`, also add the question-panel demo runs: a Grok run whose question
    /// takes only its options, and a Claude Code run asking two questions (one multi-select).
    /// Off for tests that count the waiting threads. Default false.
    pub question_demo_runs: bool,
    /// Add the two run-activity demo runs (`plan1` busy, `plan0` empty) and their mocked
    /// plan, background tasks and subagents (see [`crate::activity`]). Off for tests that count
    /// runs. Default false.
    pub activity_runs: bool,
}

impl Default for FakeConfig {
    fn default() -> Self {
        Self {
            step: Duration::from_millis(50),
            server: ServerConfig::default(),
            pipe_buffer: 64 * 1024,
            protocol_version: PROTOCOL_VERSION,
            without_features: Vec::new(),
            empty: false,
            job_duration: Duration::from_secs(3),
            registry_agents: false,
            question_run: false,
            question_demo_runs: false,
            activity_runs: false,
        }
    }
}

/// State shared by the handlers and the scripts.
pub(crate) struct Shared {
    world: Mutex<World>,
    runs_tx: watch::Sender<Vec<RunSummary>>,
    approvals_tx: watch::Sender<Vec<Approval>>,
    triggers_tx: watch::Sender<Vec<reno_proto::TriggerInfo>>,
    executors_tx: watch::Sender<Vec<reno_proto::ExecutorInfo>>,
    defs_tx: watch::Sender<DefsSnapshot>,
    /// `fs.watch` changes pushed by a test, with the run each is for.
    fs_tx: tokio::sync::broadcast::Sender<(RunId, reno_proto::FsChange)>,
    /// Open `fs.watch` subscriptions.
    fs_watches: AtomicUsize,
    /// The terminals of the Terminal tab.
    terms: Mutex<HashMap<reno_proto::TermId, terminals::FakeTerm>>,
    /// Numbers the terminals.
    term_counter: std::sync::atomic::AtomicU64,
    /// Errors a method answers with, until cleared.
    injected: Mutex<HashMap<String, ErrorObject>>,
    /// One token per open connection, so a test can cut them all.
    connections: Mutex<Vec<CancellationToken>>,
    /// The tasks `route.preview` was asked about, oldest first.
    previews: Mutex<Vec<reno_proto::TaskRunParams>>,
    config: FakeConfig,
    tracker: TaskTracker,
    cancel: CancellationToken,
    event_subscriptions: AtomicUsize,
}

impl Shared {
    fn lock(&self) -> std::sync::MutexGuard<'_, World> {
        self.world.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Reads the world.
    fn read<R>(&self, f: impl FnOnce(&World) -> R) -> R {
        f(&self.lock())
    }

    /// Changes the world, then publishes the run list and the approval list to their watchers
    /// (latest-value: only when they differ).
    fn with<R>(&self, f: impl FnOnce(&mut World) -> R) -> R {
        let mut world = self.lock();
        let result = f(&mut world);
        let runs = world.summaries();
        self.runs_tx.send_if_modified(|current| {
            let changed = *current != runs;
            if changed {
                *current = runs;
            }
            changed
        });
        let approvals = world.pending();
        self.approvals_tx.send_if_modified(|current| {
            let changed = *current != approvals;
            if changed {
                *current = approvals;
            }
            changed
        });
        let triggers = world.triggers.clone();
        self.triggers_tx.send_if_modified(|current| {
            let changed = *current != triggers;
            if changed {
                *current = triggers;
            }
            changed
        });
        let executors = world.executors.clone();
        self.executors_tx.send_if_modified(|current| {
            let changed = *current != executors;
            if changed {
                *current = executors;
            }
            changed
        });
        let defs = world.defs.clone();
        self.defs_tx.send_if_modified(|current| {
            let changed = *current != defs;
            if changed {
                *current = defs;
            }
            changed
        });
        result
    }
}

/// A fake daemon. Dropping it cancels everything it runs.
#[derive(Debug)]
pub struct FakeDaemon {
    shared: Arc<Shared>,
    server: Arc<Server>,
    /// The mocked run-activity simulations (see [`activity`]).
    activity: activity::Sims,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared").finish_non_exhaustive()
    }
}

impl FakeDaemon {
    /// Starts a fake daemon with the default config and returns a connected, handshaken client.
    /// The daemon lives as long as the client or any of its clones.
    pub async fn spawn() -> Result<Client, ClientError> {
        Self::spawn_with(FakeConfig::default()).await
    }

    /// Like [`FakeDaemon::spawn`], with a config.
    pub async fn spawn_with(config: FakeConfig) -> Result<Client, ClientError> {
        let daemon = Self::start(config)?;
        let client = daemon.connect().await?;
        Ok(client.with_keepalive(Arc::new(daemon)))
    }

    /// Starts a fake daemon with no client. Connect with [`FakeDaemon::connect`].
    pub fn start(config: FakeConfig) -> Result<Self, ClientError> {
        let cancel = CancellationToken::new();
        let mut seed = if config.empty {
            data::empty_seed()
        } else {
            data::seed().map_err(|e| ClientError::Protocol(e.to_string()))?
        };
        if config.question_run && !config.empty {
            let proto = |e: reno_proto::IdError| ClientError::Protocol(e.to_string());
            seed.runs.push(data::question_run().map_err(proto)?);
            if config.question_demo_runs {
                seed.runs.push(data::grok_question_run().map_err(proto)?);
                seed.runs.push(data::paged_question_run().map_err(proto)?);
            }
        }
        if config.activity_runs && !config.empty {
            let proto = |e: reno_proto::IdError| ClientError::Protocol(e.to_string());
            seed.runs.extend(data::activity_runs().map_err(proto)?);
        }
        let config_activity = config.activity_runs && !config.empty;
        let mut world = World::from_seed(seed, &cancel);
        if config.registry_agents {
            world
                .executors
                .extend(registry::seed().map_err(|e| ClientError::Protocol(e.to_string()))?);
        }
        let (runs_tx, _) = watch::channel(world.summaries());
        let (approvals_tx, _) = watch::channel(world.pending());
        let (triggers_tx, _) = watch::channel(world.triggers.clone());
        let (executors_tx, _) = watch::channel(world.executors.clone());
        let (defs_tx, _) = watch::channel(world.defs.clone());
        let shared = Arc::new(Shared {
            world: Mutex::new(world),
            runs_tx,
            approvals_tx,
            triggers_tx,
            executors_tx,
            defs_tx,
            fs_tx: tokio::sync::broadcast::channel(64).0,
            fs_watches: AtomicUsize::new(0),
            terms: Mutex::new(HashMap::new()),
            term_counter: std::sync::atomic::AtomicU64::new(0),
            injected: Mutex::new(HashMap::new()),
            connections: Mutex::new(Vec::new()),
            previews: Mutex::new(Vec::new()),
            tracker: TaskTracker::new(),
            cancel,
            event_subscriptions: AtomicUsize::new(0),
            config: config.clone(),
        });
        let server = Arc::new(Server::with_config(router(&shared), config.server));
        Ok(Self {
            shared,
            server,
            activity: activity::seed(config_activity),
        })
    }

    /// Opens a new connection to this daemon and performs the handshake.
    pub async fn connect(&self) -> Result<Client, ClientError> {
        let token = self.shared.cancel.child_token();
        self.shared
            .connections
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(token.clone());
        let rpc =
            connect_in_memory_with_buffer(&self.server, token, self.shared.config.pipe_buffer);
        Client::connect(rpc).await
    }

    /// Cuts every open connection, as if the daemon had died. The daemon keeps its state and
    /// accepts new connections, like a daemon that was restarted with its data intact.
    pub fn sever(&self) {
        let tokens: Vec<_> = self
            .shared
            .connections
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .drain(..)
            .collect();
        for token in tokens {
            token.cancel();
        }
    }

    /// Replaces the triggers and publishes them to `triggers.watch`.
    pub fn set_triggers(&self, triggers: Vec<reno_proto::TriggerInfo>) {
        self.shared.with(|w| w.triggers = triggers);
    }

    /// Replaces the executors and their health and publishes them to `executors.watch`.
    pub fn set_executors(&self, executors: Vec<reno_proto::ExecutorInfo>) {
        self.shared.with(|w| w.executors = executors);
    }

    /// Replaces the definitions snapshot (and its file problems) and publishes it to
    /// `defs.watch`.
    pub fn set_defs(&self, defs: DefsSnapshot) {
        self.shared.with(|w| w.defs = defs);
    }

    /// The next executor job to start ends `failed` with `failure` and `detail`, once its
    /// steps have run.
    pub fn fail_next_job(&self, failure: reno_proto::JobFailure, detail: &str) {
        self.shared.with(|w| {
            w.jobs.fail_next = Some((failure, detail.to_owned()));
        });
    }

    /// The next sign-in of `executor` says `events`, in order, instead of the default flow. When
    /// the last is a `done` it is the end; otherwise the sign-in stays open until
    /// [`FakeDaemon::finish_signin`], a cancel, or the client dropping the subscription.
    pub fn script_signin(
        &self,
        executor: &reno_proto::ExecutorId,
        events: Vec<reno_proto::SigninEvent>,
    ) {
        self.shared.with(|w| w.signins.script(executor, events));
    }

    /// Ends the running sign-in of `executor` that has no scripted end: `done`, signed in or not,
    /// with `message`. `false` when none is waiting. A signed-in end turns the executor's health
    /// signed in.
    pub fn finish_signin(
        &self,
        executor: &reno_proto::ExecutorId,
        signed_in: bool,
        message: Option<&str>,
    ) -> bool {
        self.shared.with(|w| {
            w.signins
                .finish(executor, signed_in, message.map(str::to_owned))
        })
    }

    /// The sign-ins the daemon was asked to start, oldest first: the executor and the method.
    pub fn signins_started(&self) -> Vec<(reno_proto::ExecutorId, Option<String>)> {
        self.shared.read(|w| w.signins.started())
    }

    /// Serves the registry index from its cache, as when the device is offline.
    pub fn set_registry_offline(&self, offline: bool) {
        self.shared.with(|w| w.registry.offline = offline);
    }

    /// Sets how many runs `id` has in flight. An update waits (`queued`) while it is above zero,
    /// unless forced, and starts when it drops to zero. The demo has one run on `codex`.
    pub fn set_active_runs(&self, id: &reno_proto::ExecutorId, runs: u32) {
        let plan = self
            .shared
            .with(|w| w.set_active_runs(id, runs, &self.shared.cancel));
        if let Some(plan) = plan {
            jobs::spawn(&self.shared, plan, self.shared.config.job_duration);
        }
    }

    /// Makes every call of `method` (for example `trigger.fire`) answer with `error`, until
    /// [`FakeDaemon::clear_failures`].
    pub fn fail(&self, method: &str, error: ErrorObject) {
        self.shared
            .injected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(method.to_owned(), error);
    }

    /// The tasks `route.preview` has been asked about so far, oldest first.
    pub fn route_previews(&self) -> Vec<reno_proto::TaskRunParams> {
        self.shared
            .previews
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Removes every injected error.
    pub fn clear_failures(&self) {
        self.shared
            .injected
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clear();
    }

    /// Queue statistics of the RPC server.
    pub fn stats(&self) -> &Arc<ServerStats> {
        self.server.stats()
    }

    /// How many `run.events` subscriptions have been started, resumes included.
    pub fn run_events_subscriptions(&self) -> usize {
        self.shared.event_subscriptions.load(Ordering::SeqCst)
    }

    /// Whether every task of this daemon (scripts and connections) has stopped.
    pub fn is_idle(&self) -> bool {
        self.shared.tracker.is_empty() && self.server.tracker().is_empty()
    }

    /// Cancels every task and waits for them to stop.
    pub async fn shutdown(&self) {
        self.shared.cancel.cancel();
        self.shared.tracker.close();
        self.server.tracker().close();
        self.shared.tracker.wait().await;
        self.server.tracker().wait().await;
    }
}

impl Drop for FakeDaemon {
    fn drop(&mut self) {
        self.shared.cancel.cancel();
    }
}

/// The injected error of `method`, if a test set one.
fn injected(shared: &Shared, method: &str) -> Result<(), ErrorObject> {
    match shared
        .injected
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(method)
    {
        Some(error) => Err(error.clone()),
        None => Ok(()),
    }
}

/// The executor `id` as it is now, as the answer to an `executor.job.*` call.
fn executor_result(
    shared: &Shared,
    id: &reno_proto::ExecutorId,
) -> Result<reno_proto::ExecutorJobResult, ErrorObject> {
    shared
        .read(|w| w.executors.iter().find(|e| &e.descriptor.id == id).cloned())
        .map(|executor| reno_proto::ExecutorJobResult { executor })
        .ok_or_else(|| not_found("executor"))
}

fn not_found(what: &str) -> ErrorObject {
    ErrorObject::new(ErrorCode::NotFound, format!("no such {what}"))
}

fn internal(e: impl std::fmt::Display) -> ErrorObject {
    ErrorObject::new(ErrorCode::Internal, e.to_string())
}

/// A latest-value stream over a watch channel: the current value first, then each change.
fn latest<T, U>(
    rx: watch::Receiver<T>,
    map: impl Fn(&T) -> U + Send + Sync + 'static,
) -> impl Stream<Item = SubItem<U>> + Send
where
    T: Send + Sync + 'static,
    U: Send + 'static,
{
    let map = Arc::new(map);
    stream::unfold((rx, true), move |(mut rx, first)| {
        let map = Arc::clone(&map);
        async move {
            if !first {
                rx.changed().await.ok()?;
            }
            let value = map(&rx.borrow_and_update());
            Some((SubItem::latest(value), (rx, false)))
        }
    })
}

/// A run's journal from `from`, then live, ending after the closing `Done`.
fn journal(
    shared: Arc<Shared>,
    run: RunId,
    from: u64,
    rx: watch::Receiver<usize>,
) -> impl Stream<Item = SubItem<RunEventItem>> + Send {
    stream::unfold(
        (shared, run, from, rx),
        |(shared, run, next, mut rx)| async move {
            loop {
                // Mark the current length seen first: a change after this wakes `changed()`.
                drop(rx.borrow_and_update());
                let (event, finished) = shared.read(|w| {
                    (
                        w.event_at(&run, next),
                        w.run(&run).map(world::RunRecord::finished),
                    )
                });
                match (event, finished) {
                    (Some((seq, event)), _) => {
                        let item = SubItem::seq(seq, RunEventItem { seq, event });
                        return Some((item, (shared, run, next + 1, rx)));
                    }
                    (None, Some(false)) => rx.changed().await.ok()?,
                    (None, _) => return None,
                }
            }
        },
    )
}

/// Builds the router that serves every Phase 0 method and the approval methods.
fn router(shared: &Arc<Shared>) -> Router {
    let s = Arc::clone(shared);
    let hello = move |p: reno_proto::HelloParams, _| {
        let s = Arc::clone(&s);
        async move {
            let features = FEATURES
                .iter()
                .filter(|f| !s.config.without_features.iter().any(|w| w == **f))
                .filter(|f| p.features.iter().any(|c| c == *f))
                .map(|f| (*f).to_owned())
                .collect();
            Ok(HelloResult {
                server_version: "0.0.0-fake".to_owned(),
                protocol_version: s.config.protocol_version,
                features,
                pid: 4242,
            })
        }
    };

    let s = Arc::clone(shared);
    let status = move |_: NoParams, _| {
        let s = Arc::clone(&s);
        async move {
            let (active_runs, queued_runs, uptime_secs) =
                s.read(|w| (w.counts().0, w.counts().1, w.uptime_secs()));
            Ok(DaemonStatusInfo {
                version: "0.0.0-fake".to_owned(),
                pid: 4242,
                uptime_secs,
                active_runs,
                queued_runs,
                executors: ["claude-code", "codex"]
                    .iter()
                    .filter_map(|e| e.parse().ok())
                    .collect(),
                definitions_watch: None,
            })
        }
    };

    let s = Arc::clone(shared);
    let task_run = move |p: reno_proto::TaskRunParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "task.run")?;
            // The fake knows the demo bots only.
            if p.bot
                .as_deref()
                .is_some_and(|bot| !data::BOTS.contains(&bot))
            {
                return Err(ErrorObject::new(
                    ErrorCode::InvalidParams,
                    "the fake daemon has no such bot",
                ));
            }
            if p.attachments.len() > reno_proto::ATTACHMENTS_MAX_COUNT
                || p.attachments.iter().any(|a| !a.path.is_absolute())
            {
                return Err(ErrorObject::new(
                    ErrorCode::InvalidParams,
                    "too many attachments, or one is not an absolute path",
                ));
            }
            let mut p = p;
            p.prompt
                .push_str(&reno_proto::Attachment::prompt_suffix(&p.attachments));
            let routing = s.read(|w| w.route(&p));
            let Some(executor) = routing.chosen.clone().or_else(|| p.executor.clone()) else {
                return Err(ErrorObject::new(
                    ErrorCode::InvalidParams,
                    "the fake daemon has no executor for this task",
                ));
            };
            let mut routed = p.clone();
            routed.executor = Some(executor.clone());
            if let Some(why) = s.read(|w| w.refuse_model_options(&routed)) {
                return Err(ErrorObject::new(ErrorCode::InvalidParams, why));
            }
            if let Some(thread) = &p.thread_id {
                s.read(|w| w.check_thread_follow_up(thread))?;
            }
            let run_id = s
                .with(|w| w.create_run(&p, executor, routing.clone(), &s.cancel))
                .map_err(internal)?;
            let thread_id = s.read(|w| w.run(&run_id).map(|r| r.thread.clone()));
            s.tracker
                .spawn(script::run_new(Arc::clone(&s), run_id.clone(), p.prompt));
            Ok(TaskRunResult {
                run_id,
                routing: Some(routing),
                thread_id,
            })
        }
    };

    let s = Arc::clone(shared);
    let run_get = move |p: reno_proto::RunGetParams, _| {
        let s = Arc::clone(&s);
        async move {
            s.read(|w| w.run(&p.run_id).map(|r| r.summary.clone()))
                .ok_or_else(|| not_found("run"))
        }
    };

    let s = Arc::clone(shared);
    let run_routing = move |p: reno_proto::RunRoutingParams, _| {
        let s = Arc::clone(&s);
        async move {
            s.read(|w| w.run(&p.run_id).map(|r| r.routing.clone()))
                .map(|decision| reno_proto::RunRoutingResult {
                    run_id: p.run_id,
                    decision,
                })
                .ok_or_else(|| not_found("run"))
        }
    };

    let s = Arc::clone(shared);
    let route_preview = move |p: reno_proto::RoutePreviewParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "route.preview")?;
            s.previews
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(p.task.clone());
            Ok(reno_proto::RoutePreviewResult {
                decision: s.read(|w| w.route(&p.task)),
            })
        }
    };

    let s = Arc::clone(shared);
    let rule_list = move |p: reno_proto::RuleListParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "rule.list")?;
            Ok(reno_proto::RuleListResult {
                rules: s.read(|w| w.rules(p.include_inactive, p.project.as_deref())),
            })
        }
    };

    let s = Arc::clone(shared);
    let rule_add = move |p: reno_proto::RuleAddParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "rule.add")?;
            s.with(|w| w.add_rule(&p))
                .map_err(|why| ErrorObject::new(ErrorCode::InvalidParams, why))
        }
    };

    let s = Arc::clone(shared);
    let rule_revoke = move |p: reno_proto::RuleRevokeParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "rule.revoke")?;
            if s.with(|w| w.revoke_rule(&p.id)) {
                Ok(reno_proto::RuleRevokeResult {})
            } else {
                Err(not_found("rule"))
            }
        }
    };

    let s = Arc::clone(shared);
    let secret_list = move |_: NoParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "secret.list")?;
            Ok(s.read(World::secret_list))
        }
    };

    let s = Arc::clone(shared);
    let secret_set = move |p: reno_proto::SecretSetParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "secret.set")?;
            if let Err(error) = reno_proto::check_secret_name(&p.name) {
                return Err(ErrorObject::new(
                    ErrorCode::InvalidParams,
                    error.to_string(),
                ));
            }
            if p.value.is_empty() {
                return Err(ErrorObject::new(
                    ErrorCode::InvalidParams,
                    "a secret value must not be empty",
                ));
            }
            // The fake keeps the name only: it never holds a value.
            let replaced = s.with(|w| w.secret_set(&p.name));
            Ok(reno_proto::SecretSetResult { replaced })
        }
    };

    let s = Arc::clone(shared);
    let secret_delete = move |p: reno_proto::SecretDeleteParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "secret.delete")?;
            Ok(reno_proto::SecretDeleteResult {
                deleted: s.with(|w| w.secret_delete(&p.name)),
            })
        }
    };

    let s = Arc::clone(shared);
    let budget_status = move |_: reno_proto::BudgetStatusParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "budget.status")?;
            Ok(s.read(World::budget_status))
        }
    };

    let s = Arc::clone(shared);
    let budget_pause = move |p: reno_proto::BudgetPauseParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "budget.pause")?;
            s.with(|w| w.pause(p.bot.as_deref(), p.stop_live))
                .map_err(|why| ErrorObject::new(ErrorCode::NotFound, why))
        }
    };

    let s = Arc::clone(shared);
    let budget_resume = move |p: reno_proto::BudgetResumeParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "budget.resume")?;
            Ok(s.with(|w| w.resume(p.bot.as_deref())))
        }
    };

    let s = Arc::clone(shared);
    let run_list = move |p: reno_proto::RunListParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "run.list")?;
            let mut runs = s.read(World::summaries);
            if let Some(only) = &p.status {
                runs.retain(|r| only.contains(&r.status));
            }
            if let Some(cursor) = &p.before {
                match runs.iter().position(|r| &r.run_id == cursor) {
                    Some(at) => runs.drain(..=at).for_each(drop),
                    None => return Err(not_found("run")),
                }
            }
            let limit = p
                .limit
                .unwrap_or(reno_proto::RUN_LIST_DEFAULT_LIMIT)
                .clamp(1, reno_proto::RUN_LIST_MAX_LIMIT);
            let limit = usize::try_from(limit).unwrap_or(usize::MAX);
            let next = (runs.len() > limit)
                .then(|| runs.get(limit - 1).map(|r| r.run_id.clone()))
                .flatten();
            runs.truncate(limit);
            Ok(RunListResult { runs, next })
        }
    };

    let s = Arc::clone(shared);
    let run_cancel = move |p: reno_proto::RunCancelParams, _| {
        let s = Arc::clone(&s);
        async move {
            if s.read(|w| w.run(&p.run_id).is_none()) {
                return Err(not_found("run"));
            }
            s.with(|w| w.cancel(&p.run_id, p.reason.as_deref()))
                .map(|status| RunCancelResult { status })
                .ok_or_else(|| ErrorObject::new(ErrorCode::Conflict, "the run has already ended"))
        }
    };

    let s = Arc::clone(shared);
    let run_resume = move |p: reno_proto::RunResumeParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "run.resume")?;
            if s.read(|w| w.run(&p.run_id).is_none()) {
                return Err(not_found("run"));
            }
            let attempt = s
                .with(|w| w.resume_run(&p.run_id))
                .map_err(|why| ErrorObject {
                    code: ErrorCode::Conflict,
                    message: format!("run {} cannot be resumed: {why}", p.run_id),
                    data: serde_json::to_value(reno_proto::NotResumable {
                        run_id: p.run_id.clone(),
                        why,
                    })
                    .ok(),
                })?;
            s.tracker
                .spawn(script::run_resumed(Arc::clone(&s), p.run_id.clone()));
            Ok(reno_proto::RunResumeResult {
                status: reno_proto::RunStatus::Queued,
                attempt,
            })
        }
    };

    let s = Arc::clone(shared);
    let run_steer = move |p: reno_proto::RunSteerParams, _| {
        let s = Arc::clone(&s);
        async move {
            if s.read(|w| w.run(&p.run_id).is_none()) {
                return Err(not_found("run"));
            }
            // The message is journalled at once; the reply follows on the script's pace.
            let live = s.with(|w| w.steer(&p.run_id, &p.prompt)).ok_or_else(|| {
                ErrorObject::new(ErrorCode::Conflict, "the run has already ended")
            })?;
            s.tracker
                .spawn(script::reply_to_steer(Arc::clone(&s), live));
            Ok(RunSteerResult {})
        }
    };

    let s = Arc::clone(shared);
    let run_set_model = move |p: reno_proto::RunSetModelParams, _| {
        let s = Arc::clone(&s);
        async move {
            let model = s.with(|w| w.set_model(&p.run_id, &p.model))?;
            Ok(reno_proto::RunSetModelResult { model })
        }
    };

    let s = Arc::clone(shared);
    let run_configure = move |p: reno_proto::RunConfigureParams, _| {
        let s = Arc::clone(&s);
        async move { s.with(|w| w.configure(&p)) }
    };

    let s = Arc::clone(shared);
    let approval_list = move |_: NoParams, _| {
        let s = Arc::clone(&s);
        async move {
            Ok(ApprovalListResult {
                approvals: s.read(World::pending),
            })
        }
    };

    let s = Arc::clone(shared);
    let approval_decide = move |p: reno_proto::ApprovalDecideParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "approval.decide")?;
            let approval = s
                .with(|w| w.take_approval(&p.approval_id, p.decision, p.message, p.remember))
                .map_err(|why| match why {
                    Some(why) => ErrorObject::new(ErrorCode::InvalidParams, why),
                    None => not_found("approval"),
                })?;
            s.tracker
                .spawn(script::resume(Arc::clone(&s), approval.run_id, p.decision));
            Ok(ApprovalDecideResult {})
        }
    };

    let s = Arc::clone(shared);
    let run_events = move |p: reno_proto::RunEventsParams, _| {
        let s = Arc::clone(&s);
        async move {
            let rx = s
                .read(|w| w.run(&p.run_id).map(|r| r.journal_tx.subscribe()))
                .ok_or_else(|| not_found("run"))?;
            s.event_subscriptions.fetch_add(1, Ordering::SeqCst);
            // Journals count from 1; `0` and an absent `fromSeq` both start at the beginning.
            let from = p.from_seq.map_or(1, |seq| seq.0.max(1));
            Ok(journal(Arc::clone(&s), p.run_id, from, rx))
        }
    };

    let s = Arc::clone(shared);
    let runs_watch = move |p: reno_proto::RunsWatchParams, _| {
        let s = Arc::clone(&s);
        async move {
            let only = p.status;
            Ok(latest(
                s.runs_tx.subscribe(),
                move |all: &Vec<RunSummary>| {
                    let mut runs = all.clone();
                    if let Some(only) = &only {
                        runs.retain(|r| only.contains(&r.status));
                    }
                    RunsSnapshot { runs }
                },
            ))
        }
    };

    let s = Arc::clone(shared);
    let approvals_watch = move |_: reno_proto::ApprovalsWatchParams, _| {
        let s = Arc::clone(&s);
        async move {
            Ok(latest(s.approvals_tx.subscribe(), |all: &Vec<Approval>| {
                ApprovalsSnapshot {
                    approvals: all.clone(),
                }
            }))
        }
    };

    let s = Arc::clone(shared);
    let approval_history = move |p: reno_proto::ApprovalHistoryParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "approval.history")?;
            let limit = p
                .limit
                .unwrap_or(reno_proto::APPROVAL_HISTORY_DEFAULT_LIMIT)
                .clamp(1, reno_proto::APPROVAL_HISTORY_MAX_LIMIT);
            let limit = usize::try_from(limit).unwrap_or(usize::MAX);
            Ok(reno_proto::ApprovalHistoryResult {
                approvals: s.read(|w| w.history(p.run.as_ref(), limit)),
            })
        }
    };

    let s = Arc::clone(shared);
    let trigger_list = move |_: NoParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "trigger.list")?;
            Ok(reno_proto::TriggerListResult {
                triggers: s.read(|w| w.triggers.clone()),
            })
        }
    };

    let s = Arc::clone(shared);
    let trigger_fire = move |p: reno_proto::TriggerFireParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "trigger.fire")?;
            if s.read(|w| !w.triggers.iter().any(|t| t.name == p.name)) {
                return Err(not_found("trigger"));
            }
            let (outcome, run_id, prompt, scheduled_at_ms) = s
                .with(|w| w.fire_trigger(&p.name, &s.cancel))
                .map_err(|why| ErrorObject::new(ErrorCode::Conflict, why))?;
            if outcome == reno_proto::FiringOutcome::Fired {
                s.tracker
                    .spawn(script::run_new(Arc::clone(&s), run_id.clone(), prompt));
            }
            Ok(reno_proto::TriggerFireResult {
                outcome,
                run_id: Some(run_id),
                scheduled_at_ms,
            })
        }
    };

    let s = Arc::clone(shared);
    let triggers_watch = move |_: reno_proto::TriggersWatchParams, _| {
        let s = Arc::clone(&s);
        async move {
            Ok(latest(
                s.triggers_tx.subscribe(),
                |all: &Vec<reno_proto::TriggerInfo>| reno_proto::TriggersSnapshot {
                    triggers: all.clone(),
                },
            ))
        }
    };

    let s = Arc::clone(shared);
    let executor_list = move |_: NoParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "executor.list")?;
            Ok(reno_proto::ExecutorListResult {
                executors: s.read(|w| w.executors.clone()),
            })
        }
    };

    let s = Arc::clone(shared);
    let executor_refresh = move |p: reno_proto::ExecutorRefreshParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "executor.refresh")?;
            s.with(|w| w.refresh_executors(p.id.as_ref(), p.usage_only))
                .map_err(|()| not_found("executor"))?;
            Ok(reno_proto::ExecutorListResult {
                executors: s.read(|w| w.executors.clone()),
            })
        }
    };

    let s = Arc::clone(shared);
    let executor_frames = move |p: reno_proto::ExecutorFramesParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "executor.frames")?;
            let known = s.read(|w| w.executors.clone());
            if let Some(id) = &p.id
                && !known.iter().any(|e| e.descriptor.id == *id)
            {
                return Err(not_found("executor"));
            }
            // The fake daemon runs no real harness, so nothing was ever unrecognised.
            Ok(reno_proto::ExecutorFramesResult {
                executors: known
                    .into_iter()
                    .filter(|e| p.id.as_ref().is_none_or(|id| e.descriptor.id == *id))
                    .map(|e| reno_proto::ExecutorFrameCounts {
                        id: e.descriptor.id,
                        cli_version: None,
                        frames: Vec::new(),
                    })
                    .collect(),
            })
        }
    };

    let s = Arc::clone(shared);
    let executor_install = move |p: reno_proto::ExecutorInstallParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "executor.install")?;
            s.with(|w| w.install_executor(&p.id))
                .map_err(|why| match why {
                    world::InstallRefused::NotFound => not_found("executor"),
                    world::InstallRefused::NothingToInstall => ErrorObject::new(
                        ErrorCode::Conflict,
                        format!("executor {} has nothing to install", p.id),
                    ),
                })?;
            let executor = s
                .read(|w| {
                    w.executors
                        .iter()
                        .find(|e| e.descriptor.id == p.id)
                        .cloned()
                })
                .ok_or_else(|| not_found("executor"))?;
            Ok(reno_proto::ExecutorInstallResult { executor })
        }
    };

    let s = Arc::clone(shared);
    let executor_job_start = move |p: reno_proto::ExecutorJobStartParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "executor.job.start")?;
            let plan = s.with(|w| w.start_job(&p.id, p.kind, p.force, &s.cancel))?;
            if let Some(plan) = plan {
                jobs::spawn(&s, plan, s.config.job_duration);
            }
            executor_result(&s, &p.id)
        }
    };

    let s = Arc::clone(shared);
    let executor_job_cancel = move |p: reno_proto::ExecutorJobParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "executor.job.cancel")?;
            s.with(|w| w.cancel_job(&p.id))?;
            executor_result(&s, &p.id)
        }
    };

    let s = Arc::clone(shared);
    let executor_job_dismiss = move |p: reno_proto::ExecutorJobParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "executor.job.dismiss")?;
            // (As the daemon answers: the agent as it was, which a failed add then leaves.)
            let before = executor_result(&s, &p.id)?;
            s.with(|w| w.dismiss_job(&p.id))?;
            Ok(executor_result(&s, &p.id).unwrap_or(before))
        }
    };

    let s = Arc::clone(shared);
    let executor_job_log = move |p: reno_proto::ExecutorJobParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "executor.job.log")?;
            let (lines, truncated) = s.read(|w| w.job_log(&p.id))?;
            Ok(reno_proto::ExecutorJobLogResult { lines, truncated })
        }
    };

    let s = Arc::clone(shared);
    let executor_auto_update_set = move |p: reno_proto::ExecutorAutoUpdateSetParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "executor.auto_update.set")?;
            s.with(|w| w.set_auto_update(&p.id, p.on, p.verified_only))?;
            executor_result(&s, &p.id)
        }
    };

    let s = Arc::clone(shared);
    let registry_list = move |_: reno_proto::ExecutorRegistryListParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "executor.registry.list")?;
            s.read(|w| w.registry_list())
        }
    };

    let s = Arc::clone(shared);
    let registry_add = move |p: reno_proto::ExecutorRegistryParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "executor.registry.add")?;
            let (id, plan) = s.with(|w| w.registry_add(&p.id, &s.cancel))?;
            if let Some(plan) = plan {
                jobs::spawn(&s, plan, s.config.job_duration);
            }
            executor_result(&s, &id)
        }
    };

    let s = Arc::clone(shared);
    let registry_remove = move |p: reno_proto::ExecutorRegistryParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "executor.registry.remove")?;
            let executor = s.with(|w| w.registry_remove(&p.id))?;
            Ok(reno_proto::ExecutorRegistryRemoveResult { executor })
        }
    };

    let s = Arc::clone(shared);
    let registry_add_own = move |p: reno_proto::ExecutorRegistryAddOwnParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "executor.registry.add_own")?;
            let id = s.with(|w| w.registry_add_own(&p.name, &p.command, &p.env))?;
            executor_result(&s, &id)
        }
    };

    let s = Arc::clone(shared);
    let registry_env_get = move |p: reno_proto::ExecutorRegistryParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "executor.registry.env_get")?;
            let env = s.read(|w| w.registry_env_get(&p.id))?;
            Ok(reno_proto::ExecutorRegistryEnvResult { id: p.id, env })
        }
    };

    let s = Arc::clone(shared);
    let registry_env_set = move |p: reno_proto::ExecutorRegistryEnvSetParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "executor.registry.env_set")?;
            let env = s.with(|w| w.registry_env_set(&p.id, &p.env))?;
            Ok(reno_proto::ExecutorRegistryEnvResult { id: p.id, env })
        }
    };

    let s = Arc::clone(shared);
    let signin_start = move |p: reno_proto::SigninStartParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "executor.signin.start")?;
            let plan = s.with(|w| w.signin_start(&p.executor, p.method, &s.cancel))?;
            Ok(signin::serve(
                &s,
                plan,
                s.config.step,
                s.config.job_duration,
            ))
        }
    };

    let s = Arc::clone(shared);
    let signin_cancel = move |p: reno_proto::SigninCancelParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "executor.signin.cancel")?;
            let cancelled = s.with(|w| w.signins.cancel(&p.executor));
            Ok(reno_proto::SigninCancelResult { cancelled })
        }
    };

    let s = Arc::clone(shared);
    let executors_watch = move |_: reno_proto::ExecutorsWatchParams, _| {
        let s = Arc::clone(&s);
        async move {
            Ok(latest(
                s.executors_tx.subscribe(),
                |all: &Vec<reno_proto::ExecutorInfo>| reno_proto::ExecutorsSnapshot {
                    executors: all.clone(),
                },
            ))
        }
    };

    let s = Arc::clone(shared);
    let def_list = move |p: reno_proto::defs::DefListParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "def.list")?;
            let mut snapshot = s.read(|w| w.defs.clone());
            if let Some(kind) = p.kind {
                snapshot.defs.retain(|d| d.kind == kind);
            }
            Ok(snapshot)
        }
    };

    let s = Arc::clone(shared);
    let def_get = move |p: reno_proto::defs::DefGetParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "def.get")?;
            s.read(|w| w.def_record(p.kind, &p.name, p.version))
                .ok_or_else(|| not_found("definition"))
        }
    };

    let s = Arc::clone(shared);
    let def_validate = move |p: reno_proto::defs::DefValidateParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "def.validate")?;
            Ok(validate(p.kind, &p.name, &p.body))
        }
    };

    let s = Arc::clone(shared);
    let defs_watch = move |_: reno_proto::defs::DefsWatchParams, _| {
        let s = Arc::clone(&s);
        async move {
            Ok(latest(s.defs_tx.subscribe(), |all: &DefsSnapshot| {
                all.clone()
            }))
        }
    };

    let s = Arc::clone(shared);
    let workspace_get = move |p: reno_proto::WorkspaceGetParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "workspace.get")?;
            s.read(|w| w.workspace_of(&p.run_id))
                .ok_or_else(|| not_found("workspace"))
        }
    };

    let s = Arc::clone(shared);
    let workspace_diff = move |p: reno_proto::WorkspaceDiffParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "workspace.diff")?;
            let scope = p.scope.clone().unwrap_or_default();
            s.read(|w| w.diff_for(&p.run_id, p.path.as_deref(), &scope))?
                .ok_or_else(|| not_found("workspace"))
        }
    };

    let s = Arc::clone(shared);
    let thread_list = move |p: reno_proto::ThreadListParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "thread.list")?;
            let wanted = p.status.unwrap_or_default();
            let threads = s.read(|w| w.thread_infos());
            let limit = p
                .limit
                .unwrap_or(reno_proto::THREAD_LIST_DEFAULT_LIMIT)
                .clamp(1, reno_proto::THREAD_LIST_MAX_LIMIT);
            Ok(reno_proto::ThreadListResult {
                threads: threads
                    .into_iter()
                    .filter(|t| wanted.is_empty() || wanted.contains(&t.status))
                    .take(usize::try_from(limit).unwrap_or(usize::MAX))
                    .collect(),
            })
        }
    };

    let s = Arc::clone(shared);
    let thread_get = move |p: reno_proto::ThreadGetParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "thread.get")?;
            s.read(|w| w.thread_info(&p.thread_id))
                .ok_or_else(|| not_found("thread"))
        }
    };

    let s = Arc::clone(shared);
    let thread_close = move |p: reno_proto::ThreadCloseParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "thread.close")?;
            s.with(|w| w.close_thread(&p.thread_id, p.force))
        }
    };

    let router = Router::new()
        .method::<DaemonHello, _, _>(hello)
        .method::<DaemonStatus, _, _>(status)
        // The fake has no process to stop: it accepts the request and carries on.
        .method::<DaemonShutdown, _, _>(|_: NoParams, _| async { Ok(DaemonShutdownResult {}) })
        .method::<TaskRun, _, _>(task_run)
        .method::<RunGet, _, _>(run_get)
        .method::<reno_proto::WorkspaceGet, _, _>(workspace_get)
        .method::<reno_proto::WorkspaceDiff, _, _>(workspace_diff)
        .method::<reno_proto::ThreadList, _, _>(thread_list)
        .method::<reno_proto::ThreadGet, _, _>(thread_get)
        .method::<reno_proto::ThreadClose, _, _>(thread_close)
        .method::<reno_proto::RunRouting, _, _>(run_routing)
        .method::<reno_proto::RoutePreview, _, _>(route_preview)
        .method::<reno_proto::RuleList, _, _>(rule_list)
        .method::<reno_proto::RuleAdd, _, _>(rule_add)
        .method::<reno_proto::RuleRevoke, _, _>(rule_revoke)
        .method::<reno_proto::SecretList, _, _>(secret_list)
        .method::<reno_proto::SecretSet, _, _>(secret_set)
        .method::<reno_proto::SecretDelete, _, _>(secret_delete)
        .method::<reno_proto::BudgetStatus, _, _>(budget_status)
        .method::<reno_proto::BudgetPause, _, _>(budget_pause)
        .method::<reno_proto::BudgetResume, _, _>(budget_resume)
        .method::<RunList, _, _>(run_list)
        .method::<RunCancel, _, _>(run_cancel)
        .method::<reno_proto::RunResume, _, _>(run_resume)
        .method::<RunSteer, _, _>(run_steer)
        .method::<reno_proto::RunSetModel, _, _>(run_set_model)
        .method::<reno_proto::RunConfigure, _, _>(run_configure)
        .method::<ApprovalList, _, _>(approval_list)
        .method::<ApprovalDecide, _, _>(approval_decide)
        .subscription::<RunEvents, _, _, _>(run_events)
        .subscription::<RunsWatch, _, _, _>(runs_watch)
        .subscription::<ApprovalsWatch, _, _, _>(approvals_watch)
        .method::<reno_proto::ApprovalHistory, _, _>(approval_history)
        .method::<reno_proto::TriggerList, _, _>(trigger_list)
        .method::<reno_proto::TriggerFire, _, _>(trigger_fire)
        .subscription::<reno_proto::TriggersWatch, _, _, _>(triggers_watch)
        .method::<reno_proto::ExecutorList, _, _>(executor_list)
        .method::<reno_proto::ExecutorRefresh, _, _>(executor_refresh)
        .method::<reno_proto::ExecutorInstall, _, _>(executor_install)
        .method::<reno_proto::ExecutorJobStart, _, _>(executor_job_start)
        .method::<reno_proto::ExecutorJobCancel, _, _>(executor_job_cancel)
        .method::<reno_proto::ExecutorJobDismiss, _, _>(executor_job_dismiss)
        .method::<reno_proto::ExecutorJobLog, _, _>(executor_job_log)
        .method::<reno_proto::ExecutorAutoUpdateSet, _, _>(executor_auto_update_set)
        .method::<reno_proto::ExecutorFrames, _, _>(executor_frames)
        .method::<reno_proto::ExecutorRegistryList, _, _>(registry_list)
        .method::<reno_proto::ExecutorRegistryAdd, _, _>(registry_add)
        .method::<reno_proto::ExecutorRegistryRemove, _, _>(registry_remove)
        .method::<reno_proto::ExecutorRegistryAddOwn, _, _>(registry_add_own)
        .method::<reno_proto::ExecutorRegistryEnvGet, _, _>(registry_env_get)
        .method::<reno_proto::ExecutorRegistryEnvSet, _, _>(registry_env_set)
        .subscription::<reno_proto::ExecutorsWatch, _, _, _>(executors_watch)
        .subscription::<reno_proto::ExecutorSigninStart, _, _, _>(signin_start)
        .method::<reno_proto::ExecutorSigninCancel, _, _>(signin_cancel)
        .method::<reno_proto::DefList, _, _>(def_list)
        .method::<reno_proto::DefGet, _, _>(def_get)
        .method::<reno_proto::DefValidate, _, _>(def_validate)
        .subscription::<reno_proto::DefsWatch, _, _, _>(defs_watch)
        .notification::<SubGap, _, _>(|_: SubGapParams, _| async {});
    history::add(terminals::add(files::add(router, shared), shared), shared)
}

/// A simplified `def.validate`: the body must be an object named like the file, and a bot needs
/// an executor. The real daemon checks far more; this is enough to show both outcomes.
fn validate(kind: DefKind, name: &str, body: &serde_json::Value) -> DefValidateResult {
    let file = kind.path_for(name);
    let mut errors = Vec::new();
    if !body.is_object() {
        errors.push(reno_proto::defs::DefError::new(
            &file,
            "",
            "a definition must be a table",
        ));
    } else {
        if kind != DefKind::Policy && body.get("name").and_then(|n| n.as_str()) != Some(name) {
            errors.push(reno_proto::defs::DefError::new(
                &file,
                "name",
                "must equal the file name",
            ));
        }
        let has_executor = body
            .get("executors")
            .and_then(|e| e.as_array())
            .is_some_and(|e| !e.is_empty());
        if kind == DefKind::Bot && !has_executor {
            errors.push(reno_proto::defs::DefError::new(
                &file,
                "executors",
                "at least one executor is required",
            ));
        }
    }
    let valid = errors.is_empty();
    DefValidateResult {
        valid,
        errors,
        hash: valid.then(|| format!("{:064x}", body.to_string().len())),
    }
}
