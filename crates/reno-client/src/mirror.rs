//! The [`Mirror`] driver: owns the subscriptions and turns what they yield into [`Update`]s.
//!
//! Updates leave on one bounded `futures` channel, so the app can read them on GPUI's executor
//! and feed them to [`State::apply`](crate::State::apply). Nothing here touches a `State`.
//!
//! # Capacity and overflow (ARCH-11, ARCH-12)
//!
//! The update channel holds [`UPDATE_CAPACITY`] (256) updates, plus one slot per producer task
//! (the `futures` channel's rule). Producers are the bootstrap task, the two watch tasks and one
//! task per followed run, so the total is bounded by `256 + 3 + followed runs`. The full channel
//! is handled differently per kind of update:
//!
//! - **List snapshots** (`runs.watch`, `approvals.watch`) are latest-value. The watch task keeps
//!   at most one pending snapshot; while the channel is full, a newer snapshot replaces it, and
//!   the task keeps draining its subscription so the daemon never sees it as slow. Older lists are
//!   dropped, which is lossless because each one is the whole list.
//! - **Run events** are not collapsible, so a full channel makes the run's task wait. It stops
//!   reading its `run.events` subscription; the bounded queues behind it fill, the subscription
//!   is cut off with a gap, and [`Client::run_events`] resumes from the first undelivered seq.
//!   Memory stays bounded and no event is lost.
//! - **Control updates** (connection, daemon info) wait for room like run events; they are rare.
//!
//! # Lifecycle (ARCH-11)
//!
//! Every task is spawned on the mirror's [`TaskTracker`] and stops when its
//! [`CancellationToken`] fires. [`Mirror::shutdown`] cancels and waits; dropping a `Mirror`
//! cancels without waiting.
//!
//! # Disconnects
//!
//! Reconnect is out of scope for Phase 0. When a subscription ends because the connection is
//! gone, the driver sends [`ConnectionStatus::Disconnected`] and stops that task.

use std::collections::HashSet;
use std::future::poll_fn;
use std::sync::{Arc, Mutex, PoisonError};

use futures::StreamExt;
use futures::channel::mpsc;
use futures::stream::BoxStream;
use reno_proto::{RunId, Seq};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::client::Client;
use crate::error::ClientError;
use crate::state::{ConnectionStatus, DaemonInfo, Update};

/// Capacity of the update channel. See the module docs for what happens when it is full.
pub const UPDATE_CAPACITY: usize = 256;

/// The receiving end of the update channel.
pub type Updates = mpsc::Receiver<Update>;

/// Tuning for a [`Mirror`].
#[derive(Debug, Clone)]
pub struct MirrorConfig {
    /// Capacity of the update channel. Default [`UPDATE_CAPACITY`].
    pub update_capacity: usize,
}

impl Default for MirrorConfig {
    fn default() -> Self {
        Self {
            update_capacity: UPDATE_CAPACITY,
        }
    }
}

/// Drives the subscriptions that keep a [`State`](crate::State) current.
#[derive(Debug)]
pub struct Mirror {
    client: Client,
    cancel: CancellationToken,
    tracker: TaskTracker,
    tx: mpsc::Sender<Update>,
    following: Arc<Mutex<HashSet<RunId>>>,
}

impl Mirror {
    /// Starts mirroring `client`: announces the connection and daemon info, then watches runs
    /// and approvals. Must be called inside a tokio runtime. Follow a run's transcript with
    /// [`Mirror::follow_run`].
    pub fn start(client: Client, config: MirrorConfig) -> (Self, Updates) {
        let (tx, rx) = mpsc::channel(config.update_capacity.max(1));
        let mirror = Self {
            client,
            cancel: CancellationToken::new(),
            tracker: TaskTracker::new(),
            tx,
            following: Arc::default(),
        };
        mirror.spawn_bootstrap();
        mirror.spawn_watch_runs();
        mirror.spawn_watch_approvals();
        (mirror, rx)
    }

    /// The client, for actions such as `decide` and `run_task`.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// Follows one run's transcript from `from` (inclusive; the start when `None`) until the run
    /// is done or the connection drops. Pass `Transcript::resume_from()` to continue a transcript
    /// the state already holds. Returns false, doing nothing, if the run is already followed.
    pub fn follow_run(&self, run: RunId, from: Option<Seq>) -> bool {
        if !self.following().insert(run.clone()) {
            return false;
        }
        let events = self.client.run_events(run.clone(), from).boxed();
        let task = FollowRun {
            run,
            events,
            tx: self.tx.clone(),
            cancel: self.cancel.child_token(),
            following: Arc::clone(&self.following),
        };
        self.tracker.spawn(task.run());
        true
    }

    /// Whether every task of this mirror has stopped.
    pub fn is_idle(&self) -> bool {
        self.tracker.is_empty()
    }

    /// Cancels every task and waits for them to stop. Calling it again is harmless.
    pub async fn shutdown(&self) {
        self.cancel.cancel();
        self.tracker.close();
        self.tracker.wait().await;
    }

    fn following(&self) -> std::sync::MutexGuard<'_, HashSet<RunId>> {
        self.following
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn spawn_bootstrap(&self) {
        let client = self.client.clone();
        let mut tx = self.tx.clone();
        let cancel = self.cancel.child_token();
        self.tracker.spawn(async move {
            let mut info = DaemonInfo {
                hello: client.daemon().clone(),
                status: None,
            };
            let first = [
                Update::Connection(ConnectionStatus::Connected),
                Update::Daemon(info.clone()),
            ];
            for update in first {
                if !send(&mut tx, &cancel, update).await {
                    return;
                }
            }
            let status = tokio::select! {
                () = cancel.cancelled() => return,
                status = client.status() => status,
            };
            match status {
                Ok(status) => {
                    info.status = Some(status);
                    send(&mut tx, &cancel, Update::Daemon(info)).await;
                }
                Err(e) => report(&mut tx, &cancel, "daemon.status", &e).await,
            }
        });
    }

    fn spawn_watch_runs(&self) {
        let client = self.client.clone();
        let mut tx = self.tx.clone();
        let cancel = self.cancel.child_token();
        self.tracker.spawn(async move {
            match client.watch_runs().await {
                Ok(stream) => {
                    let stream = stream.map(|r| r.map(|s| Update::Runs(s.runs))).boxed();
                    pump_latest("runs.watch", stream, tx, cancel).await;
                }
                Err(e) => report(&mut tx, &cancel, "runs.watch", &e).await,
            }
        });
    }

    fn spawn_watch_approvals(&self) {
        if !self.client.supports(reno_proto::FEATURE_APPROVALS) {
            return;
        }
        let client = self.client.clone();
        let mut tx = self.tx.clone();
        let cancel = self.cancel.child_token();
        self.tracker.spawn(async move {
            match client.watch_approvals().await {
                Ok(stream) => {
                    let stream = stream
                        .map(|r| r.map(|s| Update::Approvals(s.approvals)))
                        .boxed();
                    pump_latest("approvals.watch", stream, tx, cancel).await;
                }
                Err(e) => report(&mut tx, &cancel, "approvals.watch", &e).await,
            }
        });
    }
}

impl Drop for Mirror {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Sends one update, waiting for room. False when the receiver is gone or the mirror stopped.
async fn send(tx: &mut mpsc::Sender<Update>, cancel: &CancellationToken, update: Update) -> bool {
    tokio::select! {
        biased;
        () = cancel.cancelled() => false,
        sent = futures::SinkExt::send(tx, update) => sent.is_ok(),
    }
}

/// Logs a failure, and tells the app when the failure means the connection is gone.
async fn report(
    tx: &mut mpsc::Sender<Update>,
    cancel: &CancellationToken,
    what: &str,
    error: &ClientError,
) {
    if matches!(error, ClientError::Disconnected) {
        send(
            tx,
            cancel,
            Update::Connection(ConnectionStatus::Disconnected),
        )
        .await;
    } else {
        tracing::warn!(%what, %error, "mirror source failed");
    }
}

/// Forwards a latest-value stream, collapsing to the newest item while the channel is full.
async fn pump_latest(
    what: &'static str,
    mut stream: BoxStream<'static, Result<Update, ClientError>>,
    mut tx: mpsc::Sender<Update>,
    cancel: CancellationToken,
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
                Some(Ok(update)) => pending = Some(update),
                Some(Err(e)) => break Some(e),
                None => break None,
            },
        }
    };
    if let Some(update) = pending
        && !send(&mut tx, &cancel, update).await
    {
        return;
    }
    match ended {
        Some(e) => report(&mut tx, &cancel, what, &e).await,
        None => report(&mut tx, &cancel, what, &ClientError::Disconnected).await,
    }
}

/// One followed run.
struct FollowRun {
    run: RunId,
    events: BoxStream<'static, Result<reno_proto::RunEventItem, ClientError>>,
    tx: mpsc::Sender<Update>,
    cancel: CancellationToken,
    following: Arc<Mutex<HashSet<RunId>>>,
}

impl FollowRun {
    async fn run(mut self) {
        loop {
            let next = tokio::select! {
                biased;
                () = self.cancel.cancelled() => break,
                next = self.events.next() => next,
            };
            match next {
                Some(Ok(item)) => {
                    let update = Update::RunEvent {
                        run: self.run.clone(),
                        seq: item.seq,
                        event: item.event,
                    };
                    // Run events cannot be collapsed: wait for room (see module docs).
                    if !send(&mut self.tx, &self.cancel, update).await {
                        break;
                    }
                }
                Some(Err(e)) => {
                    report(&mut self.tx, &self.cancel, "run.events", &e).await;
                    break;
                }
                None => break,
            }
        }
        self.following
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.run);
    }
}
