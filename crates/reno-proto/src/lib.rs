//! The vocabulary every other crate shares: ids, domain types, `ExecEvent` and the typed
//! protocol (method, notification and subscription traits, `PROTOCOL_VERSION`, error codes).
//!
//! Owns ARCH-1: it sits at the bottom of the dependency graph and depends on no other Reno crate.
//! It does no I/O and has no async runtime. Owns ARCH-13: protocol types generate the schema that
//! CI compares with the committed snapshot, and changes within a version are additive only.
//!
//! # Modules
//!
//! - [`ids`]: id newtypes and their string format.
//! - [`run`]: `RunStatus` and the lifecycle transition table, `StopReason`.
//! - [`exec`]: `ExecSpec`, `ExecEvent` and executor capability types.
//! - [`protocol`]: `Method`, `Notification` and `Subscription`, the Phase 0 surface, and
//!   [`protocol_schema`].
//! - [`defs`]: bots, triggers and policy as TOML-backed definitions: parsing, validation and
//!   content hashing, all pure. Its wire types are in the protocol.
//! - [`health`]: what a probe found about an executor (installed, version gate, signed in, models)
//!   and the wire types of `executor.list`, `executor.refresh` and `executors.watch`.
//! - [`routing`]: the routing decision (which executor, and why), `route.preview`, `run.routing`.
//! - [`triggers`]: the state of triggers on the wire (`trigger.list`, `trigger.fire`).
//! - [`workspace`]: the worktree a write run works in (`workspace.list`, `workspace.clean`).
//! - [`rules`]: standing rules a person makes from an approval, and the matcher that holds them to
//!   the exact action (`rule.list`, `rule.revoke`).
//! - [`secrets`]: named secrets (`secret.list`, `secret.set`, `secret.delete`) and the rule for a
//!   secret's name.
//! - [`agent`]: the `agent.*` methods a run's own `reno mcp` process calls, and the run token digest.
//! - [`files`]: the read-only Files tab (`fs.*`, `git.status`, `fs.watch`).
//! - [`error`]: stable wire error codes.
//!
//! # Serde conventions
//!
//! One convention applies to every wire type:
//!
//! - Struct fields are `camelCase` (`#[serde(rename_all = "camelCase")]`).
//! - Enums with no data are plain `snake_case` strings (`"waiting_input"`).
//! - Enums with data are internally tagged: `#[serde(tag = "type", rename_all = "snake_case")]`,
//!   with `rename_all_fields = "camelCase"`, so every data variant is a struct variant
//!   (`{"type":"text","text":"hi"}`), never a newtype or tuple.
//! - `Option` fields are omitted when `None` and accepted when missing. Collections that may be
//!   empty accept a missing field as empty. This keeps additive changes compatible.
//! - Ids are strings, [`Seq`] is a number, [`ErrorCode`] is an integer.
//!
//! # The schema snapshot
//!
//! `tests/schema.rs` compares [`protocol_schema`] with the accepted snapshot in
//! `tests/snapshots/`. A change to any protocol type fails that test until the snapshot is
//! updated, which is the point: review the diff, and make sure it is additive within the current
//! [`PROTOCOL_VERSION`]. To update it:
//!
//! - `cargo insta review` (with `cargo-insta` installed), or
//! - `INSTA_UPDATE=always cargo test -p reno-proto --test schema`, then read the diff of the
//!   `.snap` file before committing.
//!
//! A new method, notification or subscription must also be added to [`protocol_schema`].

pub mod agent;
pub mod budget;
pub mod defs;
pub mod error;
pub mod exec;
pub mod files;
pub mod health;
pub mod ids;
pub mod protocol;
pub mod routing;
pub mod rules;
pub mod run;
pub mod secrets;
pub mod term;
pub mod thread;
pub mod triggers;
pub mod usage;
pub mod workspace;

pub use agent::*;
pub use budget::*;
pub use error::*;
pub use exec::*;
pub use files::*;
pub use health::*;
pub use ids::*;
pub use protocol::*;
pub use routing::*;
pub use rules::*;
pub use run::*;
pub use secrets::*;
pub use term::*;
pub use thread::*;
pub use triggers::*;
pub use usage::*;
pub use workspace::*;
