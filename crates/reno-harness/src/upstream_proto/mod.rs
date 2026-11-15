//! Wire types the imported harness drivers speak, copied from the upstream protocol crate.
//!
//! Private to `reno-harness` and replaced by `reno-proto` types in stage B.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub mod agent;
pub mod annotation;
pub mod attachment_mentions;
pub mod chat_mentions;
pub mod file_mentions;
pub mod invocation;
pub mod plan_usage;
pub mod redact;
pub mod run_errors;

pub use agent::*;

/// CLI plan rate-limit window (accounts settings meters), not app token accounting.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentUsageWindow {
    pub label: String,
    /// 0.0..=1.0
    pub used_fraction: f32,
    pub resets_at: Option<DateTime<Utc>>,
}

/// One piece of work a waiting agent will be woken by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitingOn {
    /// The runtime's id for it (a background process's `BackgroundTask::id`).
    pub id: String,
    pub kind: WaitingKind,
    /// What it is, as the person would read it: a command, or an agent's task.
    pub label: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WaitingKind {
    /// A background agent: it always reports back.
    Agent,
    /// A background process the agent said it waits on.
    Process,
    /// A kind from a newer peer.
    #[serde(other)]
    Other,
}

/// One background process of a chat's agent, as its runtime reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTask {
    /// The runtime's id for it: what `StopBackgroundTask` and
    /// `ReadBackgroundTaskOutput` address.
    pub id: String,
    /// The command line, or the agent's description of it when the runtime
    /// never said what it runs.
    pub command: String,
    /// The agent's one-line description, when it gave one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// When the host first saw it running.
    pub started_at: DateTime<Utc>,
}

/// The tail of a background process's output (`ReadBackgroundTaskOutput`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackgroundTaskOutput {
    /// Plain text (terminal escapes stripped), ending with the latest line.
    pub text: String,
    /// Earlier output was cut off.
    #[serde(default)]
    pub truncated: bool,
}
