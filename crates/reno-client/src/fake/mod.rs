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
mod script;
mod world;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::{Stream, stream};
use reno_proto::{
    Approval, ApprovalDecide, ApprovalDecideResult, ApprovalList, ApprovalListResult,
    ApprovalsSnapshot, ApprovalsWatch, DaemonHello, DaemonStatus, DaemonStatusInfo, ErrorCode,
    ErrorObject, FEATURES, HelloResult, NoParams, PROTOCOL_VERSION, RunCancel, RunCancelResult,
    RunEventItem, RunEvents, RunGet, RunId, RunList, RunListResult, RunSteer, RunSteerResult,
    RunSummary, RunsSnapshot, RunsWatch, SubGap, SubGapParams, TaskRun, TaskRunResult,
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
}

impl Default for FakeConfig {
    fn default() -> Self {
        Self {
            step: Duration::from_millis(50),
            server: ServerConfig::default(),
            pipe_buffer: 64 * 1024,
            protocol_version: PROTOCOL_VERSION,
        }
    }
}

/// State shared by the handlers and the scripts.
pub(crate) struct Shared {
    world: Mutex<World>,
    runs_tx: watch::Sender<Vec<RunSummary>>,
    approvals_tx: watch::Sender<Vec<Approval>>,
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
        let seed = data::seed().map_err(|e| ClientError::Protocol(e.to_string()))?;
        let world = World::from_seed(seed, &cancel);
        let (runs_tx, _) = watch::channel(world.summaries());
        let (approvals_tx, _) = watch::channel(world.pending());
        let shared = Arc::new(Shared {
            world: Mutex::new(world),
            runs_tx,
            approvals_tx,
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
        let rpc = connect_in_memory_with_buffer(
            &self.server,
            self.shared.cancel.child_token(),
            self.shared.config.pipe_buffer,
        );
        Client::connect(rpc).await
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
            let run_id = s.with(|w| w.create_run(&p, &s.cancel)).map_err(internal)?;
            s.tracker
                .spawn(script::run_new(Arc::clone(&s), run_id.clone(), p.prompt));
            Ok(TaskRunResult { run_id })
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
    let run_list = move |p: reno_proto::RunListParams, _| {
        let s = Arc::clone(&s);
        async move {
            let mut runs = s.read(World::summaries);
            if let Some(only) = &p.status {
                runs.retain(|r| only.contains(&r.status));
            }
            if let Some(limit) = p.limit {
                runs.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
            }
            Ok(RunListResult { runs })
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
            let approval = s
                .with(|w| w.take_approval(&p.approval_id))
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
            let from = p.from_seq.map_or(0, |seq| seq.0);
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

    Router::new()
        .method::<DaemonHello, _, _>(hello)
        .method::<DaemonStatus, _, _>(status)
        .method::<TaskRun, _, _>(task_run)
        .method::<RunGet, _, _>(run_get)
        .method::<RunList, _, _>(run_list)
        .method::<RunCancel, _, _>(run_cancel)
        .method::<RunSteer, _, _>(run_steer)
        .method::<ApprovalList, _, _>(approval_list)
        .method::<ApprovalDecide, _, _>(approval_decide)
        .subscription::<RunEvents, _, _, _>(run_events)
        .subscription::<RunsWatch, _, _, _>(runs_watch)
        .subscription::<ApprovalsWatch, _, _, _>(approvals_watch)
        .notification::<SubGap, _, _>(|_: SubGapParams, _| async {})
}
