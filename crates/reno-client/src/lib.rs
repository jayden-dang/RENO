//! The typed client over `reno-rpc`, plus a GPUI-free state mirror with a pure `apply(event)`.
//!
//! Owns ARCH-2: the app and the CLI know the daemon only through this crate, and never link the
//! kernel, the store or the harness layer. Every feature the app has, the CLI can reach through it.
//! Its consumers are the CLI, the MCP server and the app (ARCH-3).
//!
//! # Modules
//!
//! - `client`: [`Client`], typed calls and subscriptions, with the checked `daemon.hello`
//!   handshake and gap-free [`Client::run_events`].
//! - `state`: [`State`], [`Update`] and [`Changed`]. Plain data with one pure `apply`; no tokio.
//! - `paths`: the default data directory and socket path (`renod` resolves the same ones).
//! - `mirror`: [`Mirror`], the tokio driver that turns subscriptions into [`Update`]s on a
//!   bounded channel.
//! - `error`: [`ClientError`].
//! - `launcher`: starting the daemon detached or through launchd, `DaemonKeeper` (the app's
//!   connect-or-start), the restart backoff, the LaunchAgent plist and the daemon's log files.
//! - `fake` (cargo feature `fake`): an in-process fake daemon over the in-memory transport.

pub mod activity;
pub mod answers;
pub mod client;
pub mod completions;
pub mod error;
pub mod launcher;
pub mod mirror;
pub mod paths;
pub mod places;
pub mod queue;
pub mod state;

#[cfg(feature = "fake")]
pub mod fake;

pub use client::Client;
pub use error::ClientError;
pub use mirror::{Backoff, Connector, Mirror, MirrorConfig, UPDATE_CAPACITY, Updates};
pub use paths::{default_data_dir, default_defs_dir, default_socket_path};
pub use state::{
    Changed, ConnectionStatus, DaemonInfo, Feed, FeedState, Offline, OfflineKind, RunPage, State,
    Transcript, Update, filter_key,
};
