//! The brain of the daemon: dispatcher, leases, run supervisors, recovery and the façade over
//! them. Routing, triggers, flows, boards, policy and evaluation join it in later phases.
//!
//! Owns ARCH-7: the kernel does no I/O of its own. It reaches the world only through `reno-store`
//! and its ports (`Clock`, `Executor`, `Workspace`, `Watch`, `Definitions`, `Secrets`), which are
//! implemented outside it and faked in tests. `cargo xtask check` scans these sources for file,
//! process and socket use and for clock reads. Owns ARCH-10 and ARCH-11: nothing polls, every
//! queue is bounded and every task is owned.
//!
//! # Modules
//!
//! - `clock`: the `Clock` port, `SystemClock` (the one reader of the system clock), `ManualClock`.
//! - `registry`: executors by id.
//! - `kernel`: [`Kernel`], the façade, and the layout of tasks and channels.
//! - `dispatcher`: claims runs and starts them.
//! - `supervisor`: one task per live run; owns its stream, journal, subscribers and heartbeat.
//! - `events`: `run_events`, journal replay joined to the live tail.
//! - `recovery`: what start-up repairs.
//! - `lifecycle`: how a `Done` becomes state.
//! - `approvals`, `procs`, `db`, `shared`, `ids`, `config`, `error`: support.
//!
//! # Phase 0 limits
//!
//! - Approvals are in memory; recovery interrupts runs that were waiting on one.
//! - A run in `waiting_input` has no client path to an answer.
//! - A run holds one process record; a second report replaces the first.
//! - `RunSummary::stop_reason` is derived from the status (the store keeps no stop reason), and
//!   `model` stays empty: no event updates the row's model.

mod approvals;
mod clock;
mod config;
mod db;
mod dispatcher;
mod error;
mod events;
mod ids;
mod kernel;
mod lifecycle;
mod procs;
mod recovery;
mod registry;
mod shared;
mod supervisor;

pub use clock::{Clock, ManualClock, SystemClock};
pub use config::KernelConfig;
pub use error::{KernelError, Result};
pub use kernel::{Kernel, ShutdownReport};
pub use registry::ExecutorRegistry;

#[cfg(test)]
mod tests;
