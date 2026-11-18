//! A signed-in account's plan usage ("Session 54%, Week 21%"), read from the provider's own
//! usage endpoint with the login the CLI already holds.
//!
//! Ported from zeron (`plan_usage.rs` and the usage probes of `agent_accounts.rs`, see
//! `docs/internal/UPSTREAM.md`), reduced to the one live login of each CLI: Claude Code and
//! Codex.
//!
//! # Read-only
//!
//! A credential is **read and never written**: no refresh, no rotation, no Keychain write. The
//! CLI that owns the login may hold its single-use refresh token, and a second refresher would
//! sign it out. A token the endpoint rejects, or that is not there, makes the usage
//! unavailable (`None`); it does not start a refresh.
//!
//! A token goes to exactly one place: the provider's usage endpoint, over HTTPS, with no
//! redirects followed. It is never logged and never part of an error. What this module says
//! about a failure is its class (`missing credentials`, `rejected`, `network`, `schema`).
//!
//! # Seams
//!
//! [`UsageSource`] is what an executor holds. Production wires [`ClaudeUsage::from_env`] and
//! [`CodexUsage::from_env`] (`renod` does; a driver built bare has none). Tests build them
//! over temp directories and a local endpoint, with no Keychain unless a stand-in `security`
//! program is named (ARCH-16).

#![deny(missing_docs, clippy::unwrap_used, clippy::expect_used)]

mod claude;
mod codex;
mod http;
#[cfg(unix)]
mod keychain;
pub mod parse;

use async_trait::async_trait;
use reno_proto::UsageWindow;

pub use claude::ClaudeUsage;
pub use codex::CodexUsage;

/// What a usage endpoint said, before the daemon stamps a time on it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageReading {
    /// The live plan name, when the provider says.
    pub plan_label: Option<String>,
    /// The meters, never empty.
    pub windows: Vec<UsageWindow>,
}

/// Where an executor's plan usage comes from.
#[async_trait]
pub trait UsageSource: std::fmt::Debug + Send + Sync {
    /// The account's usage now, or `None` when it is unavailable: no login with meters, a token
    /// the endpoint rejected, no answer, or a reply with no window. Bounded in time; never
    /// writes a credential.
    async fn read(&self) -> Option<UsageReading>;
}
