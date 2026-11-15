//! The only crate that knows SQL: the SQLite bindings, migrations, the writer thread, the read
//! pool, run journals and artifacts.
//!
//! Owns ARCH-5: no other crate depends on the SQLite bindings, and no SQLite type appears in
//! this crate's public API. Owns ARCH-8: SQLite is the only source of truth, and every state
//! change commits in the same transaction as the event that records it. Owns ARCH-9: claims
//! return fences, and fenced writes are checked here. Owns ARCH-10 for the WAL: the checkpoint
//! is a deadline after the last write, never an interval.
//!
//! # Modules
//!
//! - `db`: [`Store`], opening, PRAGMAs, the read connections.
//! - `writer`: the writer thread, group commit, durable writes, idle checkpoint
//!   ([`StoreConfig`] holds the tunables).
//! - `migrations`: the schema, versioned with `user_version`.
//! - `runs`: tasks and runs, claims, fences and transitions.
//! - `events`: the global event log.
//! - `journal`: per-run JSONL journals and their repair.
//! - `error`: [`StoreError`].
//!
//! # Time
//!
//! The store never reads the clock. Every call that depends on time takes `now`, in milliseconds
//! since the epoch.

pub mod db;
pub mod error;
pub mod events;
pub mod journal;
mod migrations;
pub mod runs;
mod writer;

pub use db::Store;
pub use error::{Result, StoreError};
pub use events::EventRecord;
pub use journal::{JournalWriter, Journals, Replay};
pub use runs::{Claim, NewTask, RunRecord};
pub use writer::StoreConfig;

#[cfg(test)]
mod tests;
