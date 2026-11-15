//! The Reno daemon: composes the store, the executors and the kernel, serves the client protocol
//! on a Unix socket, and shuts down in order.
//!
//! Owns ARCH-4 (the protocol half): this is the only crate that maps protocol methods to kernel
//! calls. It is also the composition root, so it is the one crate besides the kernel that names
//! `reno-store` and `reno-harness`: it opens the store and builds the executors the kernel is
//! handed. It must not depend on GPUI or on the CLI, and must not link `loro`.
//!
//! # Modules
//!
//! - [`paths`]: the data directory layout and its defaults.
//! - [`lock`]: the exclusive lock on the data directory.
//! - [`executors`]: `mock`, and the harnesses that are installed.
//! - [`defs`]: the `Definitions` port over `~/.reno`: first-start files, reading, and the
//!   debounced `notify` watch.
//! - [`fswatch`]: the `Watch` port for `fs` triggers, over `notify`, debounced per definition.
//! - [`worktrees`]: the `Workspace` port over git worktrees in `<data-dir>/worktrees/`.
//! - `handlers`: methods and subscriptions to kernel calls.
//! - [`errors`]: kernel errors to stable wire codes (ARCH-14).
//! - [`daemon`]: [`Daemon`], start-up and the shutdown sequence.
//! - [`logging`]: JSON logs to the data directory; called by the binary only.
//!
//! The binary (`main.rs`) is arguments, logging, the runtime and signals.

pub mod daemon;
pub mod defs;
pub mod errors;
pub mod executors;
pub mod fswatch;
mod handlers;
pub mod lock;
pub mod logging;
pub mod paths;
pub mod worktrees;

pub use daemon::{
    CONNECTION_GRACE, Daemon, DaemonConfig, DaemonError, ExecutorSource, SERVER_DEADLINE, Stopped,
};
pub use defs::{Bootstrapped, FsDefinitions, GitInit, GitOutcome};
pub use executors::ExecutorSet;
pub use fswatch::FsWatch;
pub use paths::Paths;
pub use worktrees::GitWorkspace;
