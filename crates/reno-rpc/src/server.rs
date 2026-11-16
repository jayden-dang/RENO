//! The server: accepts connections, frames ndjson, dispatches through the [`Router`].
//!
//! # Lifecycle (ARCH-11)
//!
//! Every task goes through a [`TaskTracker`] and stops on a [`CancellationToken`]. Each
//! connection has its own tracker for request, notification and subscription tasks; the
//! connection task, its writer and the accept loop live in the server's tracker.
//! [`Server::serve`] returns once the token is cancelled and every connection has stopped
//! reading, dropped its in-flight handlers, flushed queued frames and closed.
//! [`Server::serve_until`] splits that token in two: one stops the accept loop, the other stops
//! the connections.
//!
//! A connection also ends on EOF, an I/O error or an oversized line. In-flight handlers are
//! dropped without a reply at that point: nobody is left to read it.
//!
//! # Capacities (all bounded)
//!
//! | Queue | Capacity |
//! |---|---|
//! | Outbound frames per connection | 64 |
//! | Items per subscription ([`ServerConfig::sub_queue_capacity`]) | 256 |
//! | Concurrent requests per connection ([`ServerConfig::max_in_flight`]) | 64 |
//! | Subscriptions per connection ([`ServerConfig::max_subscriptions`]) | 64 |
//! | Connections ([`ServerConfig::max_connections`]) | 64 |
//! | Longest line | 8 MiB |
//!
//! # Frame errors
//!
//! Malformed JSON is `-32700`, a batch or non-object is `-32600`, an unknown method is `-32601`
//! and params of the wrong shape are `-32602`; the connection stays open for all of these. A
//! line over 8 MiB is answered with `-32600` (null id) and then the connection is closed: the
//! rest of an oversized frame cannot be trusted to be framed.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::StreamExt;
use reno_proto::{ErrorCode, ErrorObject, SubscriptionId};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::codec::{FramedRead, LinesCodecError};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::error::RpcError;
use crate::frame::{
    Inbound, MAX_LINE, OUT_CAP, SUB_CANCEL, SubRef, codec, error_line, parse_inbound, result_line,
    spawn_writer, to_value,
};
use crate::pump::{Pump, ServerStats};
use crate::router::{Context, Handler, PeerInfo, Router};
use crate::transport::UnixServer;

/// Capacity knobs. The defaults are the documented ones.
#[derive(Debug, Clone)]
pub struct ServerConfig {
    /// Items queued per subscription before the overflow policy applies. Default 256.
    pub sub_queue_capacity: usize,
    /// Requests a connection may run at once; the read loop waits for a slot. Default 64.
    pub max_in_flight: usize,
    /// Subscriptions per connection; more are refused with `Unavailable`. Default 64.
    pub max_subscriptions: usize,
    /// Simultaneous connections; `accept` waits for a slot. Default 64.
    pub max_connections: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            sub_queue_capacity: 256,
            max_in_flight: 64,
            max_subscriptions: 64,
            max_connections: 64,
        }
    }
}

/// A JSON-RPC server over any byte stream.
#[derive(Debug)]
pub struct Server {
    router: Arc<Router>,
    config: ServerConfig,
    tracker: TaskTracker,
    stats: Arc<ServerStats>,
    next_conn: Arc<AtomicU64>,
}

impl Server {
    /// A server with default capacities.
    pub fn new(router: Router) -> Self {
        Self::with_config(router, ServerConfig::default())
    }

    /// A server with explicit capacities.
    pub fn with_config(router: Router, config: ServerConfig) -> Self {
        Self {
            router: Arc::new(router),
            config,
            tracker: TaskTracker::new(),
            stats: Arc::new(ServerStats::default()),
            next_conn: Arc::new(AtomicU64::new(1)),
        }
    }

    /// The tracker that owns every task of this server.
    pub fn tracker(&self) -> &TaskTracker {
        &self.tracker
    }

    /// Queue statistics, for tests and diagnostics.
    pub fn stats(&self) -> &Arc<ServerStats> {
        &self.stats
    }

    /// Accepts connections on `listener` until `cancel` fires, then waits for all of them.
    pub async fn serve(
        &self,
        listener: UnixServer,
        cancel: CancellationToken,
    ) -> Result<(), RpcError> {
        self.serve_until(listener, cancel.clone(), cancel).await
    }

    /// Like [`serve`](Self::serve), with the two stops apart: when `stop_accepting` fires the
    /// listener is dropped (so the socket file goes and no new peer can connect) while the
    /// connections already open carry on; they stop, and this call returns, when `cancel` fires
    /// or each peer closes. A daemon uses this to finish its own shutdown work while clients can
    /// still read the replies they are owed.
    pub async fn serve_until(
        &self,
        listener: UnixServer,
        stop_accepting: CancellationToken,
        cancel: CancellationToken,
    ) -> Result<(), RpcError> {
        let slots = Arc::new(Semaphore::new(self.config.max_connections.max(1)));
        loop {
            let permit = tokio::select! {
                biased;
                () = stop_accepting.cancelled() => break,
                permit = Arc::clone(&slots).acquire_owned() => match permit {
                    Ok(permit) => permit,
                    Err(_) => break,
                },
            };
            let accepted = tokio::select! {
                biased;
                () = stop_accepting.cancelled() => break,
                accepted = listener.accept() => accepted,
            };
            match accepted {
                Ok((stream, peer)) => {
                    let (read, write) = tokio::io::split(stream);
                    self.spawn_connection(read, write, peer, cancel.child_token(), Some(permit));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "accept failed");
                    tokio::select! {
                        () = stop_accepting.cancelled() => break,
                        () = tokio::time::sleep(Duration::from_millis(100)) => {}
                    }
                }
            }
        }
        // Closing the listener removes the socket file: from here on nobody can connect.
        drop(listener);
        self.tracker.close();
        self.tracker.wait().await;
        self.tracker.reopen();
        Ok(())
    }

    /// Serves one already-connected stream (used by the in-memory transport). The connection
    /// ends when the stream closes or `cancel` fires.
    pub fn serve_io<IO>(&self, io: IO, peer: PeerInfo, cancel: CancellationToken)
    where
        IO: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (read, write) = tokio::io::split(io);
        self.spawn_connection(read, write, peer, cancel, None);
    }

    fn spawn_connection<R, W>(
        &self,
        read: R,
        write: W,
        peer: PeerInfo,
        cancel: CancellationToken,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Send + Unpin + 'static,
    {
        let conn_id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        let (out, out_rx) = mpsc::channel(OUT_CAP);
        // The writer stops only on server shutdown, so that it can flush a final error frame
        // after the connection's own token has been cancelled.
        let writer = spawn_writer(&self.tracker, write, out_rx, cancel.clone());
        let cancel = cancel.child_token();
        let conn = Arc::new(Conn {
            router: Arc::clone(&self.router),
            ctx: Context::new(conn_id, peer, cancel.clone()),
            out,
            subs: Mutex::new(HashMap::new()),
            next_sub: AtomicU64::new(1),
            tasks: TaskTracker::new(),
            cancel,
            in_flight: Arc::new(Semaphore::new(self.config.max_in_flight.max(1))),
            config: self.config.clone(),
            stats: Arc::clone(&self.stats),
        });
        self.tracker.spawn(async move {
            let _slot = permit;
            Arc::clone(&conn).read_loop(read).await;
            // Stop request and subscription tasks, then let the writer drain and close.
            conn.cancel.cancel();
            conn.tasks.close();
            conn.tasks.wait().await;
            // The last sender goes with the connection; the writer then flushes and closes.
            drop(conn);
            if writer.await.is_err() {
                tracing::debug!(conn_id, "writer task panicked");
            }
        });
    }
}

/// State of one connection.
struct Conn {
    router: Arc<Router>,
    ctx: Context,
    out: mpsc::Sender<String>,
    subs: Mutex<HashMap<SubscriptionId, CancellationToken>>,
    next_sub: AtomicU64,
    tasks: TaskTracker,
    cancel: CancellationToken,
    in_flight: Arc<Semaphore>,
    config: ServerConfig,
    stats: Arc<ServerStats>,
}

impl Conn {
    async fn read_loop<R: AsyncRead + Unpin>(self: Arc<Self>, read: R) {
        let mut lines = FramedRead::new(read, codec());
        loop {
            let next = tokio::select! {
                biased;
                () = self.cancel.cancelled() => return,
                next = lines.next() => next,
            };
            match next {
                None => return,
                Some(Ok(line)) => {
                    if !self.handle_line(&line).await {
                        return;
                    }
                }
                Some(Err(LinesCodecError::MaxLineLengthExceeded)) => {
                    let err = ErrorObject::new(
                        ErrorCode::InvalidRequest,
                        format!("frame exceeds the {MAX_LINE} byte limit"),
                    );
                    self.send(error_line(&Value::Null, &err)).await;
                    return;
                }
                Some(Err(LinesCodecError::Io(e))) => {
                    tracing::debug!(error = %e, "read failed");
                    return;
                }
            }
        }
    }

    /// Handles one line. Returns false when the connection should stop reading.
    async fn handle_line(self: &Arc<Self>, line: &str) -> bool {
        let inbound = match parse_inbound(line) {
            Ok(inbound) => inbound,
            Err(bad) => {
                let (id, err) = *bad;
                return self.send(error_line(&id, &err)).await;
            }
        };
        let permit = tokio::select! {
            biased;
            () = self.cancel.cancelled() => return false,
            permit = Arc::clone(&self.in_flight).acquire_owned() => match permit {
                Ok(permit) => permit,
                Err(_) => return false,
            },
        };
        let conn = Arc::clone(self);
        match inbound {
            Inbound::Request { id, method, params } => {
                self.tasks.spawn(async move {
                    let cancel = conn.cancel.clone();
                    let work = conn.request(id, method, params);
                    tokio::select! {
                        () = cancel.cancelled() => {}
                        () = work => {}
                    }
                    drop(permit);
                });
            }
            Inbound::Notification { method, params } => {
                if let Err(err) = self.router.check(&self.ctx, &method) {
                    tracing::debug!(method, error = %err.message, "a guarded notification is dropped");
                    return true;
                }
                let Some(handler) = self.router.notification_handler(&method) else {
                    tracing::debug!(method, "ignoring unknown notification");
                    return true;
                };
                let ctx = self.ctx.clone();
                self.tasks.spawn(async move {
                    tokio::select! {
                        () = conn.cancel.cancelled() => {}
                        () = handler(params, ctx) => {}
                    }
                    drop(permit);
                });
            }
            Inbound::Response { .. } => {
                tracing::debug!("ignoring a response: this server sends no requests");
            }
        }
        true
    }

    /// Queues a line for the writer. Returns false when the writer is gone.
    async fn send(&self, line: String) -> bool {
        self.out.send(line).await.is_ok()
    }

    async fn reply(&self, id: &Value, outcome: Result<Value, ErrorObject>) {
        let line = match outcome {
            Ok(result) => result_line(id, &result),
            Err(err) => error_line(id, &err),
        };
        self.send(line).await;
    }

    async fn request(self: Arc<Self>, id: Value, method: String, params: Value) {
        if method == SUB_CANCEL {
            let outcome = self.cancel_subscription(params);
            return self.reply(&id, outcome).await;
        }
        if let Err(err) = self.router.check(&self.ctx, &method) {
            return self.reply(&id, Err(err)).await;
        }
        match self.router.handler(&method) {
            None => {
                let err = ErrorObject::new(
                    ErrorCode::MethodNotFound,
                    format!("method not found: {method}"),
                );
                self.reply(&id, Err(err)).await;
            }
            Some(Handler::Method(handler)) => {
                let outcome = handler(params, self.ctx.clone()).await;
                self.reply(&id, outcome).await;
            }
            Some(Handler::Subscribe(handler)) => {
                let stream = match handler(params, self.ctx.clone()).await {
                    Ok(stream) => stream,
                    Err(err) => return self.reply(&id, Err(err)).await,
                };
                let (sub, token) = match self.register_subscription() {
                    Ok(registered) => registered,
                    Err(err) => return self.reply(&id, Err(err)).await,
                };
                let ack = to_value(&SubRef {
                    subscription: sub.clone(),
                });
                // The reply is queued before the pump exists, so it precedes every item.
                if !self.send(result_line(&id, &ack)).await {
                    self.forget_subscription(&sub);
                    return;
                }
                let pump = Pump {
                    sub: sub.clone(),
                    stream,
                    out: self.out.clone(),
                    capacity: self.config.sub_queue_capacity,
                    token,
                    stats: Arc::clone(&self.stats),
                };
                let conn = Arc::clone(&self);
                self.tasks.spawn(async move {
                    pump.run().await;
                    conn.forget_subscription(&sub);
                });
            }
        }
    }

    fn subs(&self) -> std::sync::MutexGuard<'_, HashMap<SubscriptionId, CancellationToken>> {
        self.subs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn register_subscription(&self) -> Result<(SubscriptionId, CancellationToken), ErrorObject> {
        let mut subs = self.subs();
        if subs.len() >= self.config.max_subscriptions {
            return Err(ErrorObject::new(
                ErrorCode::Unavailable,
                "too many subscriptions on this connection",
            ));
        }
        let n = self.next_sub.fetch_add(1, Ordering::Relaxed);
        let suffix = format!("c{}s{n}", self.ctx.conn_id());
        let sub = SubscriptionId::from_suffix(&suffix).map_err(|e| {
            ErrorObject::new(ErrorCode::InternalError, format!("subscription id: {e}"))
        })?;
        let token = self.cancel.child_token();
        subs.insert(sub.clone(), token.clone());
        Ok((sub, token))
    }

    fn forget_subscription(&self, sub: &SubscriptionId) {
        self.subs().remove(sub);
    }

    /// `sub.cancel`: stops the subscription. No `sub.done` follows a cancel.
    fn cancel_subscription(&self, params: Value) -> Result<Value, ErrorObject> {
        let SubRef { subscription } = serde_json::from_value(params).map_err(|e| {
            ErrorObject::new(ErrorCode::InvalidParams, format!("invalid params: {e}"))
        })?;
        match self.subs().remove(&subscription) {
            Some(token) => {
                token.cancel();
                Ok(Value::Object(serde_json::Map::new()))
            }
            None => Err(ErrorObject::new(
                ErrorCode::NotFound,
                format!("no such subscription: {subscription}"),
            )),
        }
    }
}
