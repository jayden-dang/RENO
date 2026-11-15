//! [`Client`]: a typed facade over [`reno_rpc::Client`].
//!
//! It adds three things to the raw RPC client: a checked `daemon.hello` handshake, one error
//! type, and subscriptions that hide `sub.gap` (ARCH-12): [`Client::run_events`] resumes by
//! sequence number until the run is done, so callers see an ordered, gap-free stream or an error.
//!
//! The client spawns no tasks of its own; the connection's tasks belong to the underlying
//! [`reno_rpc::Client`], which has a tracker and a cancellation token (ARCH-11). Dropping the last
//! clone of a [`Client`], and of every stream taken from it, closes the connection.

use std::any::Any;
use std::io::ErrorKind;
use std::path::Path;
use std::sync::Arc;

use futures::stream::{self, BoxStream};
use futures::{Stream, StreamExt};
use reno_proto::{
    Approval, ApprovalDecide, ApprovalDecideParams, ApprovalDecision, ApprovalList,
    ApprovalsSnapshot, ApprovalsWatch, ApprovalsWatchParams, DaemonHello, DaemonShutdown,
    DaemonStatus, DaemonStatusInfo, FEATURES, HelloParams, HelloResult, NoParams, PROTOCOL_VERSION,
    RunCancel, RunCancelParams, RunEventItem, RunEvents, RunEventsParams, RunGet, RunGetParams,
    RunId, RunList, RunListParams, RunListResult, RunStatus, RunSteer, RunSteerParams, RunSummary,
    RunsSnapshot, RunsWatch, RunsWatchParams, Seq, TaskRun, TaskRunParams,
};
use reno_rpc::{RpcError, SubEvent, SubStream};
use serde::de::DeserializeOwned;
use tokio_util::sync::CancellationToken;

use crate::error::ClientError;

/// How many times in a row a `run.events` subscription may end in a gap without delivering a
/// single new event before the stream gives up with [`ClientError::Protocol`].
const MAX_STALLED_GAPS: u32 = 8;

/// Client name sent in `daemon.hello`.
const CLIENT_NAME: &str = "reno-client";

/// A connection to the daemon. Cheap to clone; clones share one connection.
#[derive(Clone)]
pub struct Client {
    rpc: reno_rpc::Client,
    hello: Arc<HelloResult>,
    /// Keeps whatever serves this connection alive (the fake daemon), until the last clone drops.
    _keepalive: Option<Arc<dyn Any + Send + Sync>>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("daemon", &self.hello)
            .finish_non_exhaustive()
    }
}

/// Sends `daemon.hello` and checks the answer.
async fn handshake(rpc: &reno_rpc::Client) -> Result<HelloResult, ClientError> {
    let hello = rpc
        .call::<DaemonHello>(HelloParams {
            client_name: CLIENT_NAME.to_owned(),
            protocol_version: PROTOCOL_VERSION,
            features: FEATURES.iter().map(|f| (*f).to_owned()).collect(),
        })
        .await?;
    if hello.protocol_version != PROTOCOL_VERSION {
        return Err(ClientError::Incompatible {
            client: PROTOCOL_VERSION,
            server: Some(hello.protocol_version),
        });
    }
    Ok(hello)
}

impl Client {
    /// Wraps a connected RPC client and performs the `daemon.hello` handshake.
    ///
    /// Fails with [`ClientError::Incompatible`] when the daemon does not speak this client's
    /// protocol version.
    pub async fn connect(rpc: reno_rpc::Client) -> Result<Self, ClientError> {
        let hello = handshake(&rpc).await?;
        Ok(Self {
            rpc,
            hello: Arc::new(hello),
            _keepalive: None,
        })
    }

    /// Connects to the daemon's Unix socket at `path` and performs the `daemon.hello` handshake.
    ///
    /// Fails with [`ClientError::NotRunning`] when the socket file is missing or refuses the
    /// connection. The connection closes when the last clone of the client is dropped.
    pub async fn connect_unix(path: impl AsRef<Path>) -> Result<Self, ClientError> {
        let path = path.as_ref();
        let rpc = reno_rpc::connect_unix(path, CancellationToken::new())
            .await
            .map_err(|e| match e {
                RpcError::Io(io)
                    if matches!(
                        io.kind(),
                        ErrorKind::NotFound | ErrorKind::ConnectionRefused
                    ) =>
                {
                    ClientError::NotRunning(path.to_path_buf())
                }
                other => ClientError::from(other),
            })?;
        Self::connect(rpc).await
    }

    /// Attaches a value that is dropped together with the last clone of this client.
    #[must_use]
    pub fn with_keepalive(mut self, keepalive: Arc<dyn Any + Send + Sync>) -> Self {
        self._keepalive = Some(keepalive);
        self
    }

    /// What the daemon said in the handshake done by [`Client::connect`].
    pub fn daemon(&self) -> &HelloResult {
        &self.hello
    }

    /// Whether the daemon negotiated `feature` in the handshake.
    pub fn supports(&self, feature: &str) -> bool {
        self.hello.features.iter().any(|f| f == feature)
    }

    /// Repeats `daemon.hello` and checks the answer again.
    pub async fn hello(&self) -> Result<HelloResult, ClientError> {
        handshake(&self.rpc).await
    }

    /// Closes the connection and waits for its tasks to stop.
    pub async fn close(&self) {
        self.rpc.close().await;
    }

    /// `daemon.status`: daemon health and load.
    pub async fn status(&self) -> Result<DaemonStatusInfo, ClientError> {
        Ok(self.rpc.call::<DaemonStatus>(NoParams {}).await?)
    }

    /// `daemon.shutdown`: asks the daemon to stop. Needs feature `daemon.shutdown`.
    ///
    /// The daemon answers once shutdown has begun, then closes its connections. To know it has
    /// stopped, wait for its socket to disappear; do not wait for this connection to stay open.
    pub async fn shutdown_daemon(&self) -> Result<(), ClientError> {
        self.require(reno_proto::FEATURE_DAEMON_SHUTDOWN)?;
        self.rpc.call::<DaemonShutdown>(NoParams {}).await?;
        Ok(())
    }

    /// `task.run`: creates a task and its first run.
    pub async fn run_task(&self, params: TaskRunParams) -> Result<RunId, ClientError> {
        Ok(self.rpc.call::<TaskRun>(params).await?.run_id)
    }

    /// `run.get`: one run's summary.
    pub async fn get_run(&self, run_id: &RunId) -> Result<RunSummary, ClientError> {
        let params = RunGetParams {
            run_id: run_id.clone(),
        };
        Ok(self.rpc.call::<RunGet>(params).await?)
    }

    /// `run.list`: one page of runs, newest first (the daemon's default page size when `limit`
    /// is absent). Use [`Client::list_runs_page`] to page on.
    pub async fn list_runs(&self, params: RunListParams) -> Result<Vec<RunSummary>, ClientError> {
        Ok(self.list_runs_page(params).await?.runs)
    }

    /// `run.list` with its cursor: pass `next` as the following call's `before` until it is
    /// `None`.
    pub async fn list_runs_page(
        &self,
        params: RunListParams,
    ) -> Result<RunListResult, ClientError> {
        Ok(self.rpc.call::<RunList>(params).await?)
    }

    /// `run.cancel`: asks a run to stop. Returns the status once the cancel was accepted.
    pub async fn cancel_run(
        &self,
        run_id: &RunId,
        reason: Option<String>,
    ) -> Result<RunStatus, ClientError> {
        let params = RunCancelParams {
            run_id: run_id.clone(),
            reason,
        };
        Ok(self.rpc.call::<RunCancel>(params).await?.status)
    }

    /// `run.steer`: sends a steering message to a live run. Needs feature `run.steer`.
    pub async fn steer(&self, run_id: &RunId, prompt: String) -> Result<(), ClientError> {
        self.require(reno_proto::FEATURE_RUN_STEER)?;
        let params = RunSteerParams {
            run_id: run_id.clone(),
            prompt,
        };
        self.rpc.call::<RunSteer>(params).await?;
        Ok(())
    }

    /// `approval.list`: the pending approvals, oldest first.
    pub async fn list_approvals(&self) -> Result<Vec<Approval>, ClientError> {
        self.require(reno_proto::FEATURE_APPROVALS)?;
        Ok(self.rpc.call::<ApprovalList>(NoParams {}).await?.approvals)
    }

    /// `approval.decide`: answers one pending approval.
    pub async fn decide(
        &self,
        approval_id: &reno_proto::ApprovalId,
        decision: ApprovalDecision,
    ) -> Result<(), ClientError> {
        self.require(reno_proto::FEATURE_APPROVALS)?;
        let params = ApprovalDecideParams {
            approval_id: approval_id.clone(),
            decision,
        };
        self.rpc.call::<ApprovalDecide>(params).await?;
        Ok(())
    }

    /// `runs.watch`: the current run list, then a new full list on every change.
    ///
    /// Latest-value: a slow consumer sees fewer lists, never stale ones out of order. The stream
    /// ends when the daemon closes the subscription or the connection drops.
    pub async fn watch_runs(
        &self,
    ) -> Result<BoxStream<'static, Result<RunsSnapshot, ClientError>>, ClientError> {
        let sub = self
            .rpc
            .subscribe::<RunsWatch>(RunsWatchParams::default())
            .await?;
        Ok(watch_stream(sub))
    }

    /// `approvals.watch`: the pending approvals, then a new full list on every change.
    ///
    /// Latest-value, like [`Client::watch_runs`].
    pub async fn watch_approvals(
        &self,
    ) -> Result<BoxStream<'static, Result<ApprovalsSnapshot, ClientError>>, ClientError> {
        self.require(reno_proto::FEATURE_APPROVALS)?;
        let sub = self
            .rpc
            .subscribe::<ApprovalsWatch>(ApprovalsWatchParams::default())
            .await?;
        Ok(watch_stream(sub))
    }

    /// `run.events`: the run's journal from `from` (inclusive; the start when `None`), then
    /// live, until the daemon reports the run done.
    ///
    /// When the daemon cuts the subscription off with `sub.gap` (ARCH-12), the stream
    /// re-subscribes from `resume_from` and carries on. Items arrive in strictly increasing,
    /// consecutive sequence order; a repeated seq is skipped, and a hole ends the stream with
    /// [`ClientError::Protocol`]. The stream is lazy: nothing is sent until it is first polled.
    /// After an `Err` it ends.
    pub fn run_events(
        &self,
        run_id: RunId,
        from: Option<Seq>,
    ) -> impl Stream<Item = Result<RunEventItem, ClientError>> + Send + use<> {
        let state = EventsState {
            rpc: self.rpc.clone(),
            run_id,
            // A run's journal counts from 1, so an absent `from` and `Seq(0)` both mean the start.
            next: from.unwrap_or(Seq(1)).max(Seq(1)),
            sub: None,
            stalled: 0,
            finished: false,
        };
        stream::unfold(state, EventsState::step)
    }

    fn require(&self, feature: &str) -> Result<(), ClientError> {
        if self.supports(feature) {
            Ok(())
        } else {
            Err(ClientError::Protocol(format!(
                "the daemon did not negotiate feature `{feature}`"
            )))
        }
    }
}

/// Maps a latest-value subscription to a plain stream of values. A gap cannot happen on a
/// latest-value subscription; if one is reported anyway, it is an error and the stream ends.
fn watch_stream<T>(sub: SubStream<T>) -> BoxStream<'static, Result<T, ClientError>>
where
    T: DeserializeOwned + Send + 'static,
{
    stream::unfold(sub, |mut sub| async move {
        match sub.next().await {
            Some(SubEvent::Item(item)) => Some((Ok(item), sub)),
            Some(SubEvent::Gap { .. }) => Some((
                Err(ClientError::Protocol(
                    "gap on a latest-value subscription".to_owned(),
                )),
                sub,
            )),
            Some(SubEvent::Done) | None => None,
        }
    })
    .boxed()
}

struct EventsState {
    rpc: reno_rpc::Client,
    run_id: RunId,
    /// The next seq the caller should see.
    next: Seq,
    sub: Option<SubStream<RunEventItem>>,
    /// Gaps in a row with no new event in between.
    stalled: u32,
    finished: bool,
}

impl EventsState {
    async fn step(mut self) -> Option<(Result<RunEventItem, ClientError>, Self)> {
        match self.advance().await {
            Ok(Some(item)) => Some((Ok(item), self)),
            Ok(None) => None,
            Err(e) => {
                self.finished = true;
                Some((Err(e), self))
            }
        }
    }

    async fn advance(&mut self) -> Result<Option<RunEventItem>, ClientError> {
        loop {
            if self.finished {
                return Ok(None);
            }
            let sub = match self.sub.as_mut() {
                Some(sub) => sub,
                None => {
                    let params = RunEventsParams {
                        run_id: self.run_id.clone(),
                        from_seq: Some(self.next),
                    };
                    self.sub
                        .insert(self.rpc.subscribe::<RunEvents>(params).await?)
                }
            };
            match sub.next().await {
                Some(SubEvent::Item(item)) => {
                    if item.seq < self.next {
                        // A repeat after a resume: already delivered.
                        continue;
                    }
                    if item.seq > self.next {
                        return Err(ClientError::Protocol(format!(
                            "run {}: expected seq {}, daemon sent {}",
                            self.run_id, self.next, item.seq
                        )));
                    }
                    self.next = item.seq.next().ok_or_else(|| {
                        ClientError::Protocol("run sequence numbers overflowed".to_owned())
                    })?;
                    self.stalled = 0;
                    return Ok(Some(item));
                }
                Some(SubEvent::Gap { resume_from }) => {
                    self.sub = None;
                    self.stalled += 1;
                    if self.stalled > MAX_STALLED_GAPS {
                        return Err(ClientError::Protocol(format!(
                            "run {}: subscription keeps gapping without progress at seq {}",
                            self.run_id, self.next
                        )));
                    }
                    if resume_from > self.next {
                        return Err(ClientError::Protocol(format!(
                            "run {}: daemon resumes at {resume_from} but only {} was delivered",
                            self.run_id, self.next
                        )));
                    }
                    self.next = self.next.min(resume_from);
                }
                Some(SubEvent::Done) => {
                    self.finished = true;
                    return Ok(None);
                }
                None => return Err(ClientError::Disconnected),
            }
        }
    }
}

#[cfg(test)]
#[path = "client_tests.rs"]
mod tests;
