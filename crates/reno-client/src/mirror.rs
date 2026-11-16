//! The [`Mirror`] driver: owns the connection, the subscriptions, and reconnecting.
//!
//! Updates leave on one bounded `futures` channel, so the app can read them on GPUI's executor
//! and feed them to [`State::apply`](crate::State::apply). Nothing here touches a `State`.
//!
//! # Sessions and the supervisor
//!
//! A [`Mirror`] runs one *supervisor* task. The supervisor asks a [`Connector`] for a [`Client`],
//! runs a *session* on it (bootstrap, one task per watch, one per followed run), and waits until
//! the session says the connection is lost. It then stops the session's tasks, tells the app it
//! is offline ([`Update::Offline`]), sleeps a capped exponential [`Backoff`] (a deadline, not a
//! poll: ARCH-10) and connects again. [`Mirror::retry_now`] cuts the sleep short. The update
//! channel is the same across sessions, and every new session starts with full snapshots, so the
//! [`State`](crate::State) converges without the app doing anything.
//!
//! Followed runs survive a reconnect: the mirror remembers the next seq it delivered for each
//! and follows them again from there.
//!
//! [`Mirror::start`] is the one-connection form: no reconnecting; the supervisor ends when the
//! connection does.
//!
//! # Capacity and overflow (ARCH-11, ARCH-12)
//!
//! The update channel holds [`UPDATE_CAPACITY`] (256) updates, plus one slot per producer task
//! (the `futures` channel's rule). Producers are the supervisor and a session's tasks (bootstrap,
//! status refresh, the watches, one per followed run), so the total is bounded by
//! `256 + 7 + followed runs`. The full channel is handled differently per kind of update:
//!
//! - **List snapshots** (the watches) are latest-value. The watch task keeps at most one pending
//!   snapshot; while the channel is full, a newer snapshot replaces it, and the task keeps
//!   draining its subscription so the daemon never sees it as slow. Older lists are dropped,
//!   which is lossless because each one is the whole list.
//! - **Run events** are not collapsible, so a full channel makes the run's task wait. It stops
//!   reading its `run.events` subscription; the bounded queues behind it fill, the subscription
//!   is cut off with a gap, and [`Client::run_events`] resumes from the first undelivered seq.
//!   Memory stays bounded and no event is lost.
//! - **Control updates** (connection, daemon info, feed states) wait for room like run events;
//!   they are rare.
//!
//! # Lifecycle (ARCH-11)
//!
//! Every task is spawned on a [`TaskTracker`] and stops when its [`CancellationToken`] fires.
//! [`Mirror::shutdown`] cancels and waits; dropping a `Mirror` cancels without waiting.

use std::collections::HashMap;
use std::future::{Future, poll_fn};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::StreamExt;
use futures::channel::mpsc;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use reno_proto::{RunId, Seq};
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::client::Client;
use crate::error::ClientError;
use crate::state::{ConnectionStatus, DaemonInfo, Feed, FeedState, Offline, OfflineKind, Update};

/// Capacity of the update channel. See the module docs for what happens when it is full.
pub const UPDATE_CAPACITY: usize = 256;

/// The receiving end of the update channel.
pub type Updates = mpsc::Receiver<Update>;

/// Opens a connection. Called again after every loss; it must be cheap to call and must not
/// block (it is awaited by the supervisor only).
pub type Connector = Arc<dyn Fn() -> BoxFuture<'static, Result<Client, ClientError>> + Send + Sync>;

/// The wait between connection attempts: doubles from `initial` up to `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// The wait after the first failure.
    pub initial: Duration,
    /// The longest wait.
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(500),
            max: Duration::from_secs(30),
        }
    }
}

impl Backoff {
    /// The wait after `failures` failures in a row (0 and 1 both mean `initial`).
    pub fn delay(&self, failures: u32) -> Duration {
        let doublings = failures.saturating_sub(1).min(20);
        self.initial.saturating_mul(1u32 << doublings).min(self.max)
    }
}

/// Tuning for a [`Mirror`].
#[derive(Debug, Clone)]
pub struct MirrorConfig {
    /// Capacity of the update channel. Default [`UPDATE_CAPACITY`].
    pub update_capacity: usize,
    /// The wait between connection attempts.
    pub backoff: Backoff,
    /// A connection that lasts this long counts as good: the next loss starts the backoff over.
    pub stable_after: Duration,
}

impl Default for MirrorConfig {
    fn default() -> Self {
        Self {
            update_capacity: UPDATE_CAPACITY,
            backoff: Backoff::default(),
            stable_after: Duration::from_secs(5),
        }
    }
}

/// A run the mirror was asked to follow.
#[derive(Debug, Clone)]
struct Follow {
    /// The next seq to ask for; `None` is the start of the journal.
    next: Option<Seq>,
    /// Whether a task is following it now.
    active: bool,
}

/// What the supervisor and the handle share.
struct Shared {
    tx: mpsc::Sender<Update>,
    cancel: CancellationToken,
    tracker: TaskTracker,
    /// The current session, when connected.
    session: Mutex<Option<Session>>,
    follows: Mutex<HashMap<RunId, Follow>>,
    /// Cuts a backoff sleep short.
    retry: Notify,
    /// Asks the session to refetch `daemon.status`.
    status: Notify,
}

impl Shared {
    fn session(&self) -> std::sync::MutexGuard<'_, Option<Session>> {
        self.session.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn follows(&self) -> std::sync::MutexGuard<'_, HashMap<RunId, Follow>> {
        self.follows.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// One connection's tasks.
#[derive(Clone)]
struct Session {
    client: Client,
    cancel: CancellationToken,
    /// Fired by any task that finds the connection gone.
    lost: CancellationToken,
    tracker: TaskTracker,
    tx: mpsc::Sender<Update>,
}

/// Drives the subscriptions that keep a [`State`](crate::State) current.
pub struct Mirror {
    shared: Arc<Shared>,
}

impl std::fmt::Debug for Mirror {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Mirror").finish_non_exhaustive()
    }
}

impl Mirror {
    /// Mirrors one connected `client`: announces the connection and daemon info, then watches
    /// runs, approvals, triggers and definitions. When the connection ends the state goes
    /// offline and the mirror stops; use [`Mirror::supervise`] to reconnect. Must be called
    /// inside a tokio runtime. Follow a run's transcript with [`Mirror::follow_run`].
    pub fn start(client: Client, config: MirrorConfig) -> (Self, Updates) {
        let slot = Arc::new(Mutex::new(Some(client)));
        let connector: Connector = Arc::new(move || {
            let slot = Arc::clone(&slot);
            Box::pin(async move {
                slot.lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .take()
                    .ok_or(ClientError::Disconnected)
            })
        });
        Self::spawn(connector, config, false)
    }

    /// Connects through `connector` and keeps reconnecting, with the config's backoff, whenever
    /// the connection is lost or cannot be made. Must be called inside a tokio runtime.
    pub fn supervise(connector: Connector, config: MirrorConfig) -> (Self, Updates) {
        Self::spawn(connector, config, true)
    }

    fn spawn(connector: Connector, config: MirrorConfig, reconnect: bool) -> (Self, Updates) {
        let (tx, rx) = mpsc::channel(config.update_capacity.max(1));
        let shared = Arc::new(Shared {
            tx,
            cancel: CancellationToken::new(),
            tracker: TaskTracker::new(),
            session: Mutex::new(None),
            follows: Mutex::new(HashMap::new()),
            retry: Notify::new(),
            status: Notify::new(),
        });
        shared
            .tracker
            .spawn(supervise(Arc::clone(&shared), connector, config, reconnect));
        (Self { shared }, rx)
    }

    /// The current client, for actions such as `decide` and `run_task`. `None` while there is
    /// no connection.
    pub fn client(&self) -> Option<Client> {
        self.shared.session().as_ref().map(|s| s.client.clone())
    }

    /// Follows one run's transcript from `from` (inclusive; the start when `None`) until the run
    /// is done. Pass `Transcript::resume_from()` to continue a transcript the state already
    /// holds. The follow survives reconnects. Returns false, doing nothing, if the run is
    /// already followed.
    pub fn follow_run(&self, run: RunId, from: Option<Seq>) -> bool {
        let session = {
            let mut follows = self.shared.follows();
            if follows.contains_key(&run) {
                return false;
            }
            let session = self.shared.session().clone();
            follows.insert(
                run.clone(),
                Follow {
                    next: from,
                    active: session.is_some(),
                },
            );
            session
        };
        // Offline: the next session picks it up from `follows`.
        if let Some(session) = session {
            spawn_follow(&self.shared, &session, run, from);
        }
        true
    }

    /// Asks for a fresh `daemon.status`. Answered through [`Update::Daemon`].
    pub fn refresh_status(&self) {
        self.shared.status.notify_one();
    }

    /// Ends the wait before the next connection attempt. Does nothing while connected.
    pub fn retry_now(&self) {
        self.shared.retry.notify_one();
    }

    /// Whether every task of this mirror has stopped.
    pub fn is_idle(&self) -> bool {
        self.shared.tracker.is_empty()
    }

    /// Cancels every task and waits for them to stop. Calling it again is harmless.
    pub async fn shutdown(&self) {
        self.shared.cancel.cancel();
        self.shared.tracker.close();
        self.shared.tracker.wait().await;
    }
}

impl Drop for Mirror {
    fn drop(&mut self) {
        self.shared.cancel.cancel();
    }
}

/// Sends one update, waiting for room. False when the receiver is gone or `cancel` fired.
async fn send(tx: &mut mpsc::Sender<Update>, cancel: &CancellationToken, update: Update) -> bool {
    tokio::select! {
        biased;
        () = cancel.cancelled() => false,
        sent = futures::SinkExt::send(tx, update) => sent.is_ok(),
    }
}

fn offline_of(error: &ClientError, attempt: u32) -> Offline {
    let kind = match error {
        ClientError::NotRunning(_) => OfflineKind::NotRunning,
        ClientError::Incompatible { .. } => OfflineKind::Incompatible,
        ClientError::Disconnected => OfflineKind::Lost,
        ClientError::Starting => OfflineKind::Starting,
        ClientError::StartFailed(_) => OfflineKind::StartFailed,
        ClientError::Remote(_) | ClientError::Protocol(_) | ClientError::Transport(_) => {
            OfflineKind::Other
        }
    };
    Offline {
        kind,
        detail: error.to_string(),
        attempt,
    }
}

/// The supervisor: connect, run a session, wait for its loss, back off, repeat.
async fn supervise(
    shared: Arc<Shared>,
    connector: Connector,
    config: MirrorConfig,
    reconnect: bool,
) {
    let mut tx = shared.tx.clone();
    let mut failures: u32 = 0;
    loop {
        if !send(
            &mut tx,
            &shared.cancel,
            Update::Connection(ConnectionStatus::Connecting),
        )
        .await
        {
            return;
        }
        let connected = tokio::select! {
            biased;
            () = shared.cancel.cancelled() => return,
            connected = connector() => connected,
        };
        let offline = match connected {
            Ok(client) => {
                let started = Instant::now();
                let session = start_session(&shared, client);
                tokio::select! {
                    biased;
                    () = shared.cancel.cancelled() => {
                        stop_session(&shared, &session).await;
                        return;
                    }
                    () = session.lost.cancelled() => {}
                }
                stop_session(&shared, &session).await;
                if started.elapsed() >= config.stable_after {
                    failures = 0;
                } else {
                    failures += 1;
                }
                offline_of(&ClientError::Disconnected, failures)
            }
            Err(error) => {
                failures += 1;
                offline_of(&error, failures)
            }
        };
        if !send(&mut tx, &shared.cancel, Update::Offline(offline)).await || !reconnect {
            return;
        }
        let wait = config.backoff.delay(failures);
        tokio::select! {
            biased;
            () = shared.cancel.cancelled() => return,
            () = shared.retry.notified() => {}
            () = tokio::time::sleep(wait) => {}
        }
    }
}

/// Starts the tasks of one connection and publishes the session.
fn start_session(shared: &Arc<Shared>, client: Client) -> Session {
    let session = Session {
        client,
        cancel: shared.cancel.child_token(),
        lost: CancellationToken::new(),
        tracker: TaskTracker::new(),
        tx: shared.tx.clone(),
    };
    spawn_bootstrap(shared, &session);
    spawn_status(shared, &session);
    spawn_feeds(shared, &session);
    // Runs followed before the loss, or while offline, continue where they stopped.
    let pending: Vec<(RunId, Option<Seq>)> = {
        let mut follows = shared.follows();
        follows
            .iter_mut()
            .filter(|(_, f)| !f.active)
            .map(|(run, f)| {
                f.active = true;
                (run.clone(), f.next)
            })
            .collect()
    };
    for (run, from) in pending {
        spawn_follow(shared, &session, run, from);
    }
    *shared.session() = Some(session.clone());
    session
}

/// Stops a session's tasks and waits for them. Followed runs that did not finish stay followed.
async fn stop_session(shared: &Arc<Shared>, session: &Session) {
    *shared.session() = None;
    session.cancel.cancel();
    session.tracker.close();
    session.tracker.wait().await;
}

fn spawn_bootstrap(shared: &Arc<Shared>, session: &Session) {
    let client = session.client.clone();
    let mut tx = session.tx.clone();
    let cancel = session.cancel.clone();
    let shared = Arc::clone(shared);
    session.tracker.spawn(async move {
        let info = DaemonInfo {
            hello: client.daemon().clone(),
            status: None,
        };
        let first = [
            Update::Connection(ConnectionStatus::Connected),
            Update::Daemon(info),
        ];
        for update in first {
            if !send(&mut tx, &cancel, update).await {
                return;
            }
        }
        for (feed, feature) in [
            (Feed::Approvals, reno_proto::FEATURE_APPROVALS),
            (Feed::Triggers, reno_proto::FEATURE_TRIGGERS),
            (Feed::Executors, reno_proto::FEATURE_EXECUTORS),
            (Feed::Defs, reno_proto::FEATURE_DEFS),
            (Feed::ApprovalHistory, reno_proto::FEATURE_APPROVAL_HISTORY),
        ] {
            if !client.supports(feature)
                && !send(&mut tx, &cancel, Update::Feed(feed, FeedState::Unavailable)).await
            {
                return;
            }
        }
        // The first `daemon.status`.
        shared.status.notify_one();
    });
}

/// Refetches `daemon.status` whenever asked ([`Mirror::refresh_status`], or a runs change).
fn spawn_status(shared: &Arc<Shared>, session: &Session) {
    let client = session.client.clone();
    let mut tx = session.tx.clone();
    let cancel = session.cancel.clone();
    let lost = session.lost.clone();
    let shared = Arc::clone(shared);
    session.tracker.spawn(async move {
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return,
                () = shared.status.notified() => {}
            }
            let status = tokio::select! {
                () = cancel.cancelled() => return,
                status = client.status() => status,
            };
            match status {
                Ok(status) => {
                    let info = DaemonInfo {
                        hello: client.daemon().clone(),
                        status: Some(status),
                    };
                    if !send(&mut tx, &cancel, Update::Daemon(info)).await {
                        return;
                    }
                }
                Err(ClientError::Disconnected) => {
                    lost.cancel();
                    return;
                }
                Err(e) => tracing::warn!(error = %e, "daemon.status failed"),
            }
        }
    });
}

fn spawn_feeds(shared: &Arc<Shared>, session: &Session) {
    let client = session.client.clone();
    feed(
        session,
        Feed::Runs,
        Some(Arc::clone(shared)),
        async move { client.watch_runs().await },
        |s| Update::Runs(s.runs),
    );
    if session.client.supports(reno_proto::FEATURE_APPROVALS) {
        let client = session.client.clone();
        feed(
            session,
            Feed::Approvals,
            Some(Arc::clone(shared)),
            async move { client.watch_approvals().await },
            |s| Update::Approvals(s.approvals),
        );
    }
    if session.client.supports(reno_proto::FEATURE_TRIGGERS) {
        let client = session.client.clone();
        feed(
            session,
            Feed::Triggers,
            None,
            async move { client.watch_triggers().await },
            |s| Update::Triggers(s.triggers),
        );
    }
    if session.client.supports(reno_proto::FEATURE_EXECUTORS) {
        let client = session.client.clone();
        feed(
            session,
            Feed::Executors,
            None,
            async move { client.watch_executors().await },
            |s| Update::Executors(s.executors),
        );
    }
    if session.client.supports(reno_proto::FEATURE_DEFS) {
        let client = session.client.clone();
        feed(
            session,
            Feed::Defs,
            None,
            async move { client.watch_defs().await },
            Update::Defs,
        );
    }
}

/// Spawns the task of one watch. `poke` asks for a `daemon.status` refresh per snapshot.
fn feed<T, Fut>(
    session: &Session,
    which: Feed,
    poke: Option<Arc<Shared>>,
    open: Fut,
    map: fn(T) -> Update,
) where
    T: Send + 'static,
    Fut: Future<Output = Result<BoxStream<'static, Result<T, ClientError>>, ClientError>>
        + Send
        + 'static,
{
    let mut tx = session.tx.clone();
    let cancel = session.cancel.clone();
    let lost = session.lost.clone();
    session.tracker.spawn(async move {
        let opened = tokio::select! {
            () = cancel.cancelled() => return,
            opened = open => opened,
        };
        match opened {
            Ok(stream) => {
                let stream = stream.map(move |r| r.map(map)).boxed();
                pump_latest(which, stream, tx, cancel, lost, poke).await;
            }
            Err(e) => report(&mut tx, &cancel, &lost, which, &e).await,
        }
    });
}

/// Logs a failure; a gone connection ends the session, anything else marks the feed failed.
async fn report(
    tx: &mut mpsc::Sender<Update>,
    cancel: &CancellationToken,
    lost: &CancellationToken,
    which: Feed,
    error: &ClientError,
) {
    if matches!(error, ClientError::Disconnected) {
        lost.cancel();
    } else {
        tracing::warn!(?which, %error, "mirror source failed");
        send(
            tx,
            cancel,
            Update::Feed(which, FeedState::Failed(error.to_string())),
        )
        .await;
    }
}

/// Forwards a latest-value stream, collapsing to the newest item while the channel is full.
async fn pump_latest(
    which: Feed,
    mut stream: BoxStream<'static, Result<Update, ClientError>>,
    mut tx: mpsc::Sender<Update>,
    cancel: CancellationToken,
    lost: CancellationToken,
    poke: Option<Arc<Shared>>,
) {
    let mut pending: Option<Update> = None;
    let ended = loop {
        tokio::select! {
            biased;
            () = cancel.cancelled() => return,
            ready = poll_fn(|cx| tx.poll_ready(cx)), if pending.is_some() => {
                if ready.is_err() {
                    return;
                }
                if let Some(update) = pending.take() && tx.start_send(update).is_err() {
                    return;
                }
            }
            next = stream.next() => match next {
                // Replaces an older, unsent snapshot: the newest list is the whole truth.
                Some(Ok(update)) => {
                    pending = Some(update);
                    if let Some(shared) = &poke {
                        shared.status.notify_one();
                    }
                }
                Some(Err(e)) => break e,
                None => break ClientError::Disconnected,
            },
        }
    };
    if let Some(update) = pending
        && !send(&mut tx, &cancel, update).await
    {
        return;
    }
    report(&mut tx, &cancel, &lost, which, &ended).await;
}

fn spawn_follow(shared: &Arc<Shared>, session: &Session, run: RunId, from: Option<Seq>) {
    let events = session.client.run_events(run.clone(), from).boxed();
    let task = FollowRun {
        run,
        events,
        tx: session.tx.clone(),
        cancel: session.cancel.clone(),
        lost: session.lost.clone(),
        shared: Arc::clone(shared),
    };
    session.tracker.spawn(task.run());
}

/// One followed run.
struct FollowRun {
    run: RunId,
    events: BoxStream<'static, Result<reno_proto::RunEventItem, ClientError>>,
    tx: mpsc::Sender<Update>,
    cancel: CancellationToken,
    lost: CancellationToken,
    shared: Arc<Shared>,
}

impl FollowRun {
    async fn run(mut self) {
        let mut finished = false;
        loop {
            let next = tokio::select! {
                biased;
                () = self.cancel.cancelled() => break,
                next = self.events.next() => next,
            };
            match next {
                Some(Ok(item)) => {
                    let after = item.seq.next();
                    let update = Update::RunEvent {
                        run: self.run.clone(),
                        seq: item.seq,
                        event: item.event,
                    };
                    // Run events cannot be collapsed: wait for room (see module docs).
                    if !send(&mut self.tx, &self.cancel, update).await {
                        break;
                    }
                    if let Some(follow) = self.shared.follows().get_mut(&self.run) {
                        follow.next = after;
                    }
                }
                Some(Err(ClientError::Disconnected)) => {
                    self.lost.cancel();
                    break;
                }
                Some(Err(e)) => {
                    tracing::warn!(run = %self.run, error = %e, "run.events failed");
                    finished = true;
                    break;
                }
                None => {
                    finished = true;
                    break;
                }
            }
        }
        let mut follows = self.shared.follows();
        if finished {
            follows.remove(&self.run);
        } else if let Some(follow) = follows.get_mut(&self.run) {
            // Stopped by a lost connection or a shutdown: the next session resumes from `next`.
            follow.active = false;
        }
    }
}
