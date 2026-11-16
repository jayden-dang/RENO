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
//! - `agent`: run tokens (stored as digests) and the private boards of `reno mcp`.
//! - `approvals`: durable approval records, decided with `synchronous=FULL`.
//! - `runs`: tasks and runs, claims, fences and transitions.
//! - `definitions`: versioned bots, triggers and policy, idempotent on content hash.
//! - `triggers`: the scheduler's state and firings, fired atomically with the run they start.
//! - `executor_health`: the latest probe of each executor.
//! - `routing`: the routing decision of each run, replaced by a fenced fallback.
//! - `rules`: standing rules, created with the decision that made them and only ever revoked.
//! - `workspaces`: the worktree of each write run, recorded before it is made.
//! - `events`: the global event log.
//! - `journal`: per-run JSONL journals and their repair.
//! - `error`: [`StoreError`].
//!
//! # Time
//!
//! The store never reads the clock. Every call that depends on time takes `now`, in milliseconds
//! since the epoch.

pub mod agent;
pub mod approvals;
pub mod budget;
pub mod db;
pub mod definitions;
pub mod error;
pub mod events;
mod executor_health;
pub mod journal;
mod migrations;
mod routing;
pub mod rules;
pub mod runs;
pub mod triggers;
mod workspaces;
mod writer;

pub use agent::{BoardRow, BoardWrite, NewRunToken, RunToken};
pub use approvals::{ExpireScope, Expired, NewApproval, StoredApproval};
pub use budget::{ClaimFilter, GLOBAL as GLOBAL_PAUSE, Occupancy, Tokens};
pub use db::Store;
pub use definitions::{Applied, DefStatus, DefinitionRecord, NewDefinition, PutOutcome};
pub use error::{Result, StoreError};
pub use events::EventRecord;
pub use journal::{JournalWriter, Journals, Replay};
pub use rules::NewRule;
pub use runs::{Claim, NewTask, Note, Resumed, RunRecord};
pub use triggers::{FireResult, FiringRecord, FiringRun, NewFiring, TriggerRow, firing_nonce};
pub use writer::StoreConfig;

#[cfg(test)]
mod agent_tests;
#[cfg(test)]
mod approval_tests;
#[cfg(test)]
mod budget_tests;
#[cfg(test)]
mod definition_tests;
#[cfg(test)]
mod executor_health_tests;
#[cfg(test)]
mod resume_tests;
#[cfg(test)]
mod rule_tests;
#[cfg(test)]
mod routing_tests;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod trigger_tests;
#[cfg(test)]
mod workspace_tests;
