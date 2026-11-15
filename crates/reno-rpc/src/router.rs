//! The server-side method table: typed registration, type-erased dispatch.
//!
//! `router.method::<M>(handler)` captures `M::Params` and `M::Result` in a closure that works on
//! `serde_json::Value`, so the dispatch loop in `server` never sees a generic parameter.

use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;

use futures::{Stream, StreamExt};
use reno_proto::{ErrorCode, ErrorObject, Method, Notification, Seq, Subscription};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// A type-erased stream of serialised subscription items.
pub(crate) type ErasedStream = Pin<Box<dyn Stream<Item = ErasedItem> + Send>>;

/// One serialised subscription item.
#[derive(Debug)]
pub(crate) struct ErasedItem {
    pub(crate) seq: Option<Seq>,
    pub(crate) value: Value,
}

type MethodFn = dyn Fn(Value, Context) -> BoxFuture<Result<Value, ErrorObject>> + Send + Sync;
type SubscribeFn =
    dyn Fn(Value, Context) -> BoxFuture<Result<ErasedStream, ErrorObject>> + Send + Sync;
type NotifyFn = dyn Fn(Value, Context) -> BoxFuture<()> + Send + Sync;

/// What kind of transport a connection arrived on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerKind {
    /// A Unix domain socket.
    Unix,
    /// An in-memory duplex pair.
    InMemory,
}

/// Who is on the other end of a connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerInfo {
    /// The transport.
    pub kind: PeerKind,
    /// Peer uid, when the transport can tell (Unix sockets).
    pub uid: Option<u32>,
    /// Peer pid, when the transport can tell.
    pub pid: Option<i32>,
}

impl PeerInfo {
    /// Peer info for an in-memory connection.
    pub const fn in_memory() -> Self {
        Self {
            kind: PeerKind::InMemory,
            uid: None,
            pid: None,
        }
    }
}

/// Per-connection context handed to every handler.
#[derive(Debug, Clone)]
pub struct Context {
    conn_id: u64,
    peer: PeerInfo,
    cancel: CancellationToken,
}

impl Context {
    pub(crate) fn new(conn_id: u64, peer: PeerInfo, cancel: CancellationToken) -> Self {
        Self {
            conn_id,
            peer,
            cancel,
        }
    }

    /// Server-unique id of this connection.
    pub fn conn_id(&self) -> u64 {
        self.conn_id
    }

    /// Who is on the other end.
    pub fn peer(&self) -> &PeerInfo {
        &self.peer
    }

    /// Cancelled when the connection closes or the server shuts down. Handlers are dropped at
    /// that point anyway; this lets them release resources early.
    pub fn cancelled(&self) -> &CancellationToken {
        &self.cancel
    }
}

/// One item a subscription handler yields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubItem<T> {
    /// Position in the source journal. With a `Seq`, a slow consumer is cut off with `sub.gap`
    /// and resumes from it. Without one the subscription is latest-value: a full queue coalesces.
    pub seq: Option<Seq>,
    /// The payload.
    pub item: T,
}

impl<T> SubItem<T> {
    /// An item with a sequence number (event subscription).
    pub fn seq(seq: Seq, item: T) -> Self {
        Self {
            seq: Some(seq),
            item,
        }
    }

    /// An item without one (latest-value subscription).
    pub fn latest(item: T) -> Self {
        Self { seq: None, item }
    }
}

pub(crate) enum Handler {
    Method(Arc<MethodFn>),
    Subscribe(Arc<SubscribeFn>),
}

/// The table of methods, subscriptions and notifications a server answers.
///
/// Registering a name twice replaces the earlier handler. Handlers return
/// `Result<_, ErrorObject>`; params that fail to decode are answered with
/// [`ErrorCode::InvalidParams`] before the handler runs.
#[derive(Default)]
pub struct Router {
    pub(crate) handlers: HashMap<&'static str, Handler>,
    pub(crate) notifications: HashMap<&'static str, Arc<NotifyFn>>,
}

impl fmt::Debug for Router {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Router")
            .field("handlers", &self.handlers.keys().collect::<Vec<_>>())
            .field(
                "notifications",
                &self.notifications.keys().collect::<Vec<_>>(),
            )
            .finish()
    }
}

fn invalid_params(e: serde_json::Error) -> ErrorObject {
    ErrorObject::new(ErrorCode::InvalidParams, format!("invalid params: {e}"))
}

fn internal(e: serde_json::Error) -> ErrorObject {
    ErrorObject::new(ErrorCode::InternalError, format!("encoding failed: {e}"))
}

impl Router {
    /// An empty router.
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a request handler for `M`.
    #[must_use]
    pub fn method<M, F, Fut>(mut self, handler: F) -> Self
    where
        M: Method + 'static,
        M::Params: Send + 'static,
        M::Result: Send + 'static,
        F: Fn(M::Params, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<M::Result, ErrorObject>> + Send + 'static,
    {
        let handler = Arc::new(handler);
        let erased: Arc<MethodFn> = Arc::new(move |params, ctx| {
            let handler = Arc::clone(&handler);
            Box::pin(async move {
                let params = serde_json::from_value(params).map_err(invalid_params)?;
                let result = handler(params, ctx).await?;
                serde_json::to_value(result).map_err(internal)
            })
        });
        self.handlers.insert(M::NAME, Handler::Method(erased));
        self
    }

    /// Registers a subscription handler for `S`. The handler returns the stream of items; the
    /// server assigns the subscription id and owns the bounded queue (see the `server` docs).
    #[must_use]
    pub fn subscription<S, F, Fut, St>(mut self, handler: F) -> Self
    where
        S: Subscription + 'static,
        S::Params: Send + 'static,
        S::Item: Send + 'static,
        F: Fn(S::Params, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<St, ErrorObject>> + Send + 'static,
        St: Stream<Item = SubItem<S::Item>> + Send + 'static,
    {
        let handler = Arc::new(handler);
        let erased: Arc<SubscribeFn> = Arc::new(move |params, ctx| {
            let handler = Arc::clone(&handler);
            Box::pin(async move {
                let params = serde_json::from_value(params).map_err(invalid_params)?;
                let stream = handler(params, ctx).await?;
                let stream: ErasedStream = Box::pin(stream.filter_map(|it| async move {
                    match serde_json::to_value(&it.item) {
                        Ok(value) => Some(ErasedItem { seq: it.seq, value }),
                        Err(e) => {
                            tracing::warn!(error = %e, "dropping subscription item that does not serialise");
                            None
                        }
                    }
                }));
                Ok(stream)
            })
        });
        self.handlers.insert(S::NAME, Handler::Subscribe(erased));
        self
    }

    /// Registers a handler for the one-way notification `N`. Notifications get no reply, so
    /// params that fail to decode are logged and dropped.
    #[must_use]
    pub fn notification<N, F, Fut>(mut self, handler: F) -> Self
    where
        N: Notification + 'static,
        N::Params: Send + 'static,
        F: Fn(N::Params, Context) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let handler = Arc::new(handler);
        let erased: Arc<NotifyFn> = Arc::new(move |params, ctx| {
            let handler = Arc::clone(&handler);
            Box::pin(async move {
                match serde_json::from_value(params) {
                    Ok(params) => handler(params, ctx).await,
                    Err(e) => {
                        tracing::warn!(method = N::NAME, error = %e, "bad notification params")
                    }
                }
            })
        });
        self.notifications.insert(N::NAME, erased);
        self
    }

    pub(crate) fn handler(&self, name: &str) -> Option<&Handler> {
        self.handlers.get(name)
    }

    pub(crate) fn notification_handler(&self, name: &str) -> Option<Arc<NotifyFn>> {
        self.notifications.get(name).cloned()
    }
}
