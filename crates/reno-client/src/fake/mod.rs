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

pub mod data;
mod extras;
mod script;
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
        let seed = if config.empty {
            data::empty_seed()
        } else {
            data::seed().map_err(|e| ClientError::Protocol(e.to_string()))?
        };
        let world = World::from_seed(seed, &cancel);
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
            injected: Mutex::new(HashMap::new()),
            connections: Mutex::new(Vec::new()),
            previews: Mutex::new(Vec::new()),
            tracker: TaskTracker::new(),
            cancel,
            event_subscriptions: AtomicUsize::new(0),
            config: config.clone(),
        });
        let server = Arc::new(Server::with_config(router(&shared), config.server));
        Ok(Self { shared, server })
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
            })
        }
    };

    let s = Arc::clone(shared);
    let task_run = move |p: reno_proto::TaskRunParams, _| {
        let s = Arc::clone(&s);
        async move {
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
            let run_id = s
                .with(|w| w.create_run(&p, executor, routing.clone(), &s.cancel))
                .map_err(internal)?;
            s.tracker
                .spawn(script::run_new(Arc::clone(&s), run_id.clone(), p.prompt));
            Ok(TaskRunResult {
                run_id,
                routing: Some(routing),
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
                .with(|w| w.take_approval(&p.approval_id, p.decision, p.message))
                .ok_or_else(|| not_found("approval"))?;
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
            s.with(|w| w.refresh_executors(p.id.as_ref()))
                .map_err(|()| not_found("executor"))?;
            Ok(reno_proto::ExecutorListResult {
                executors: s.read(|w| w.executors.clone()),
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

    Router::new()
        .method::<DaemonHello, _, _>(hello)
        .method::<DaemonStatus, _, _>(status)
        // The fake has no process to stop: it accepts the request and carries on.
        .method::<DaemonShutdown, _, _>(|_: NoParams, _| async { Ok(DaemonShutdownResult {}) })
        .method::<TaskRun, _, _>(task_run)
        .method::<RunGet, _, _>(run_get)
        .method::<reno_proto::RunRouting, _, _>(run_routing)
        .method::<reno_proto::RoutePreview, _, _>(route_preview)
        .method::<RunList, _, _>(run_list)
        .method::<RunCancel, _, _>(run_cancel)
        .method::<RunSteer, _, _>(run_steer)
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
        .subscription::<reno_proto::ExecutorsWatch, _, _, _>(executors_watch)
        .method::<reno_proto::DefList, _, _>(def_list)
        .method::<reno_proto::DefGet, _, _>(def_get)
        .method::<reno_proto::DefValidate, _, _>(def_validate)
        .subscription::<reno_proto::DefsWatch, _, _, _>(defs_watch)
        .notification::<SubGap, _, _>(|_: SubGapParams, _| async {})
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
