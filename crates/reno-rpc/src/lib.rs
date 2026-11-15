//! The only crate that knows JSON-RPC framing: ndjson over a Unix socket, the id-to-reply map,
//! subscriptions with resume, and an in-memory transport for tests.
//!
//! Owns ARCH-4 (framing half): no other crate parses or writes JSON-RPC frames. Owns ARCH-12: a
//! slow reader never costs unbounded memory, because subscriptions are bounded per client.
//!
//! # Modules
//!
//! - `frame`: ndjson lines (8 MiB cap) and the private JSON-RPC 2.0 wire structs. Batches are not
//!   supported and are answered with `InvalidRequest`.
//! - [`Router`]: typed registration of methods, subscriptions and notifications, erased to
//!   `serde_json::Value` for dispatch.
//! - [`Server`]: connection loop, per-subscription bounded queues, lifecycle.
//! - [`Client`]: typed `call`, `notify` and `subscribe`.
//! - transport: [`UnixServer`], [`connect_unix`] and the in-memory pair.
//! - [`RpcError`]: the one error type, mapped to stable [`reno_proto::ErrorCode`]s.
//!
//! # Subscription wire format
//!
//! A subscribing request is answered with `{"subscription": id}`. Then the server sends
//! `sub.item {subscription, seq?, item}`, and ends with `sub.done {subscription}` or, for a
//! consumer that fell behind a sequenced stream, `sub.gap {subscription, resumeFrom}`. The client
//! stops one with the request `sub.cancel {subscription}`; no `sub.done` follows a cancel.

mod client;
mod error;
mod frame;
mod pump;
mod router;
mod server;
mod transport;

pub use client::{Client, SubEvent, SubStream};
pub use error::RpcError;
pub use pump::ServerStats;
pub use router::{Context, PeerInfo, PeerKind, Router, SubItem};
pub use server::{Server, ServerConfig};
pub use transport::{
    UnixServer, connect_in_memory, connect_in_memory_with_buffer, connect_unix, in_memory_raw,
};

#[cfg(test)]
mod tests;
