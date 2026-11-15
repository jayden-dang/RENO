//! The client: typed calls, notifications and resumable subscriptions over one connection.
//!
//! # Design
//!
//! Requests carry a numeric id. An id-to-`oneshot` map matches replies; when the connection
//! drops, the reader task empties the map, so every pending call fails with
//! [`RpcError::Disconnected`] and later calls fail at once.
//!
//! A subscription is registered by the reader task itself when the reply to the starting request
//! arrives, so no `sub.item` can race ahead of its registration.
//!
//! # Capacities (all bounded)
//!
//! - outbound frames: 64;
//! - per-subscription queue between the reader and the consumer: 256 events. If the consumer
//!   falls that far behind, the client drops the subscription locally and tells the server to
//!   stop it. The stream then yields [`SubEvent::Gap`] (resume from the next unseen seq) when
//!   items carried seqs, and simply ends otherwise.

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};

use futures::{Stream, StreamExt};
use reno_proto::{Method, Notification, Seq, SubGapParams, Subscription, SubscriptionId};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};
use tokio_util::codec::{FramedRead, LinesCodecError};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::error::RpcError;
use crate::frame::{
    Inbound, MAX_LINE, OUT_CAP, SUB_CANCEL, SUB_DONE, SUB_GAP, SUB_ITEM, SubItemParams, SubRef,
    codec, notification_line, parse_inbound, request_line, spawn_writer, to_value,
};

/// Capacity of the queue between a connection's reader and one subscription's consumer.
const SUB_QUEUE: usize = 256;

/// One event of a subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubEvent<T> {
    /// The next item.
    Item(T),
    /// The consumer was too slow and the subscription ended. Subscribe again with the resume
    /// point set to `resume_from` to continue without loss.
    Gap {
        /// The first sequence number that was not delivered.
        resume_from: Seq,
    },
    /// The source finished.
    Done,
}

enum Raw {
    Item(Option<Seq>, Value),
    Gap(Seq),
    Done,
}

struct SubSlot {
    tx: mpsc::Sender<Raw>,
    overflow: Arc<AtomicBool>,
}

enum Pending {
    Call(oneshot::Sender<Result<Value, RpcError>>),
    Subscribe {
        slot: SubSlot,
        ack: oneshot::Sender<Result<SubscriptionId, RpcError>>,
    },
}

#[derive(Default)]
struct State {
    closed: bool,
    calls: HashMap<u64, Pending>,
    subs: HashMap<SubscriptionId, SubSlot>,
}

struct Inner {
    out: mpsc::Sender<String>,
    state: Mutex<State>,
    next_id: AtomicU64,
    cancel: CancellationToken,
    tracker: TaskTracker,
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// Removes a pending call if its future is dropped before the reply arrives.
struct PendingGuard<'a> {
    inner: &'a Inner,
    id: u64,
}

impl Drop for PendingGuard<'_> {
    fn drop(&mut self) {
        self.inner.state().calls.remove(&self.id);
    }
}

/// A JSON-RPC client. Cheap to clone; clones share one connection, which closes when the last
/// clone and the last [`SubStream`] are dropped.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("closed", &self.inner.state().closed)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Runs a client over any byte stream. Its tasks stop when `cancel` fires, the stream
    /// closes, or the last handle is dropped.
    pub fn from_io<IO>(io: IO, cancel: CancellationToken) -> Self
    where
        IO: AsyncRead + AsyncWrite + Send + 'static,
    {
        let (read, write) = tokio::io::split(io);
        let tracker = TaskTracker::new();
        let cancel = cancel.child_token();
        let (out, out_rx) = mpsc::channel(OUT_CAP);
        let inner = Arc::new(Inner {
            out,
            state: Mutex::new(State::default()),
            next_id: AtomicU64::new(1),
            cancel: cancel.clone(),
            tracker: tracker.clone(),
        });
        spawn_writer(&tracker, write, out_rx, cancel.clone());
        // The reader must not keep the connection alive, so it holds `Inner` weakly: it only
        // needs the shared state and the cancel token.
        let reader = Reader {
            inner: Arc::downgrade(&inner),
            out: inner.out.downgrade(),
        };
        tracker.spawn(reader.run(read, cancel));
        tracker.close();
        Self { inner }
    }

    /// Closes the connection and waits for the client's tasks to stop.
    pub async fn close(&self) {
        self.inner.cancel.cancel();
        self.inner.tracker.wait().await;
    }

    /// Calls `M` and waits for its reply.
    pub async fn call<M: Method>(&self, params: M::Params) -> Result<M::Result, RpcError> {
        let (tx, rx) = oneshot::channel();
        let params = serde_json::to_value(params)?;
        let id = self
            .send_request(M::NAME, &params, Pending::Call(tx))
            .await?;
        let _guard = PendingGuard {
            inner: &self.inner,
            id,
        };
        let value = rx.await.map_err(|_| RpcError::Disconnected)??;
        Ok(serde_json::from_value(value)?)
    }

    /// Sends the one-way notification `N`.
    pub async fn notify<N: Notification>(&self, params: N::Params) -> Result<(), RpcError> {
        let params = serde_json::to_value(params)?;
        let line = notification_line(N::NAME, &params);
        self.send_line(line).await
    }

    /// Starts subscription `S`. The stream yields [`SubEvent`]s; dropping it sends `sub.cancel`.
    pub async fn subscribe<S: Subscription>(
        &self,
        params: S::Params,
    ) -> Result<SubStream<S::Item>, RpcError> {
        let (ack_tx, ack_rx) = oneshot::channel();
        let (tx, rx) = mpsc::channel(SUB_QUEUE);
        let overflow = Arc::new(AtomicBool::new(false));
        let slot = SubSlot {
            tx,
            overflow: Arc::clone(&overflow),
        };
        let params = serde_json::to_value(params)?;
        let id = self
            .send_request(S::NAME, &params, Pending::Subscribe { slot, ack: ack_tx })
            .await?;
        let _guard = PendingGuard {
            inner: &self.inner,
            id,
        };
        let id = ack_rx.await.map_err(|_| RpcError::Disconnected)??;
        Ok(SubStream {
            id,
            rx,
            overflow,
            last_seq: None,
            finished: false,
            inner: Arc::clone(&self.inner),
            _item: std::marker::PhantomData,
        })
    }

    async fn send_request(
        &self,
        method: &str,
        params: &Value,
        pending: Pending,
    ) -> Result<u64, RpcError> {
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let line = request_line(id, method, params);
        check_len(&line)?;
        {
            let mut state = self.inner.state();
            if state.closed {
                return Err(RpcError::Disconnected);
            }
            state.calls.insert(id, pending);
        }
        if let Err(e) = self.send_line(line).await {
            self.inner.state().calls.remove(&id);
            return Err(e);
        }
        Ok(id)
    }

    async fn send_line(&self, line: String) -> Result<(), RpcError> {
        check_len(&line)?;
        self.inner
            .out
            .send(line)
            .await
            .map_err(|_| RpcError::Disconnected)
    }
}

fn check_len(line: &str) -> Result<(), RpcError> {
    if line.len() > MAX_LINE {
        return Err(RpcError::FrameTooLarge {
            len: line.len(),
            max: MAX_LINE,
        });
    }
    Ok(())
}

/// A live subscription. Yields [`SubEvent`]s; ends after `Done` or `Gap`, or when the connection
/// drops. Dropping it asks the server to stop the subscription.
pub struct SubStream<T> {
    id: SubscriptionId,
    rx: mpsc::Receiver<Raw>,
    overflow: Arc<AtomicBool>,
    last_seq: Option<Seq>,
    finished: bool,
    inner: Arc<Inner>,
    _item: std::marker::PhantomData<fn() -> T>,
}

impl<T> std::fmt::Debug for SubStream<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SubStream")
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

impl<T> SubStream<T> {
    /// The server-assigned id of this subscription.
    pub fn id(&self) -> &SubscriptionId {
        &self.id
    }
}

impl<T> Unpin for SubStream<T> {}

impl<T: DeserializeOwned> Stream for SubStream<T> {
    type Item = SubEvent<T>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if self.finished {
                return Poll::Ready(None);
            }
            match self.rx.poll_recv(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Raw::Item(seq, value))) => {
                    if seq.is_some() {
                        self.last_seq = seq;
                    }
                    match serde_json::from_value(value) {
                        Ok(item) => return Poll::Ready(Some(SubEvent::Item(item))),
                        Err(e) => {
                            tracing::warn!(error = %e, "dropping undecodable subscription item")
                        }
                    }
                }
                Poll::Ready(Some(Raw::Gap(resume_from))) => {
                    self.finished = true;
                    return Poll::Ready(Some(SubEvent::Gap { resume_from }));
                }
                Poll::Ready(Some(Raw::Done)) => {
                    self.finished = true;
                    return Poll::Ready(Some(SubEvent::Done));
                }
                Poll::Ready(None) => {
                    self.finished = true;
                    let resume = self
                        .overflow
                        .load(Ordering::Acquire)
                        .then(|| self.last_seq.and_then(Seq::next))
                        .flatten();
                    return Poll::Ready(resume.map(|resume_from| SubEvent::Gap { resume_from }));
                }
            }
        }
    }
}

impl<T> Drop for SubStream<T> {
    fn drop(&mut self) {
        if self.inner.state().subs.remove(&self.id).is_none() {
            return;
        }
        let id = self.inner.next_id.fetch_add(1, Ordering::Relaxed);
        let params = to_value(&SubRef {
            subscription: self.id.clone(),
        });
        // Best effort: if the outbound queue is full, the server notices on its own when the
        // connection closes or its own queue overflows.
        let _ = self
            .inner
            .out
            .try_send(request_line(id, SUB_CANCEL, &params));
    }
}

/// Reads frames and completes calls and subscriptions.
struct Reader {
    inner: std::sync::Weak<Inner>,
    out: mpsc::WeakSender<String>,
}

impl Reader {
    async fn run<R: AsyncRead + Unpin>(self, read: R, cancel: CancellationToken) {
        let mut lines = FramedRead::new(read, codec());
        loop {
            let next = tokio::select! {
                biased;
                () = cancel.cancelled() => break,
                next = lines.next() => next,
            };
            match next {
                Some(Ok(line)) => {
                    let Some(inner) = self.inner.upgrade() else {
                        break;
                    };
                    self.handle(&inner, &line);
                }
                Some(Err(LinesCodecError::MaxLineLengthExceeded)) => {
                    tracing::warn!("server sent an oversized line; closing");
                    break;
                }
                Some(Err(LinesCodecError::Io(e))) => {
                    tracing::debug!(error = %e, "read failed");
                    break;
                }
                None => break,
            }
        }
        if let Some(inner) = self.inner.upgrade() {
            let (calls, subs) = {
                let mut state = inner.state();
                state.closed = true;
                (
                    std::mem::take(&mut state.calls),
                    std::mem::take(&mut state.subs),
                )
            };
            // Dropping the senders fails every waiter with `Disconnected` and ends every stream.
            drop((calls, subs));
            inner.cancel.cancel();
        }
    }

    fn handle(&self, inner: &Inner, line: &str) {
        match parse_inbound(line) {
            Err(bad) => {
                let (_, err) = *bad;
                tracing::warn!(code = err.code.code(), message = %err.message, "bad frame from server")
            }
            Ok(Inbound::Response { id, outcome }) => self.response(inner, &id, outcome),
            Ok(Inbound::Notification { method, params }) => {
                self.notification(inner, &method, params)
            }
            Ok(Inbound::Request { method, .. }) => {
                tracing::debug!(method, "ignoring a request from the server");
            }
        }
    }

    fn response(&self, inner: &Inner, id: &Value, outcome: Result<Value, reno_proto::ErrorObject>) {
        let Some(id) = id.as_u64() else {
            tracing::warn!(?id, "response with an id this client never sent");
            return;
        };
        let mut state = inner.state();
        let Some(pending) = state.calls.remove(&id) else {
            tracing::debug!(id, "response for a call nobody waits for");
            return;
        };
        match pending {
            Pending::Call(tx) => {
                let _ = tx.send(outcome.map_err(RpcError::from));
            }
            Pending::Subscribe { slot, ack } => {
                let sub = outcome.map_err(RpcError::from).and_then(|v| {
                    serde_json::from_value::<SubRef>(v)
                        .map(|r| r.subscription)
                        .map_err(RpcError::from)
                });
                if let Ok(sub) = &sub {
                    state.subs.insert(sub.clone(), slot);
                }
                let _ = ack.send(sub);
            }
        }
    }

    fn notification(&self, inner: &Inner, method: &str, params: Value) {
        let (sub, raw) = match method {
            SUB_ITEM => match serde_json::from_value::<SubItemParams>(params) {
                Ok(p) => (p.subscription, Raw::Item(p.seq, p.item)),
                Err(e) => return tracing::warn!(error = %e, "bad sub.item"),
            },
            SUB_DONE => match serde_json::from_value::<SubRef>(params) {
                Ok(p) => (p.subscription, Raw::Done),
                Err(e) => return tracing::warn!(error = %e, "bad sub.done"),
            },
            SUB_GAP => match serde_json::from_value::<SubGapParams>(params) {
                Ok(p) => (p.subscription, Raw::Gap(p.resume_from)),
                Err(e) => return tracing::warn!(error = %e, "bad sub.gap"),
            },
            other => return tracing::debug!(method = other, "ignoring server notification"),
        };
        let terminal = !matches!(raw, Raw::Item(..));
        let mut state = inner.state();
        let Some(slot) = state.subs.get(&sub) else {
            tracing::debug!(%sub, "frame for an unknown subscription");
            return;
        };
        match slot.tx.try_send(raw) {
            Ok(()) if terminal => {
                state.subs.remove(&sub);
            }
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                // The consumer is 256 events behind: cut it off locally and stop the server.
                slot.overflow.store(true, Ordering::Release);
                state.subs.remove(&sub);
                drop(state);
                self.cancel_remote(inner, &sub);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                state.subs.remove(&sub);
            }
        }
    }

    fn cancel_remote(&self, inner: &Inner, sub: &SubscriptionId) {
        let Some(out) = self.out.upgrade() else {
            return;
        };
        let id = inner.next_id.fetch_add(1, Ordering::Relaxed);
        let params = to_value(&SubRef {
            subscription: sub.clone(),
        });
        let _ = out.try_send(request_line(id, SUB_CANCEL, &params));
    }
}
