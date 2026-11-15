//! The wire types of triggers: what `trigger.list`, `trigger.fire` and `triggers.watch` carry.
//!
//! The definitions (`TriggerDef`) say when a trigger should fire; these types say where it stands:
//! what it will do next, what it did last, and whether it is backed off. All times are
//! milliseconds since the Unix epoch.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::RunId;

/// What became of one occurrence of a trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FiringOutcome {
    /// A run was started for it.
    Fired,
    /// The trigger's previous run is still going, so no run was started: the occurrence is
    /// merged into that run.
    Coalesced,
    /// This occurrence had already been fired (its nonce exists), so nothing was done. Returned
    /// by `trigger.fire`; never stored, because the nonce is unique.
    Duplicate,
    /// The run it started could not start for an account-level reason (not installed, not
    /// signed in, no eligible executor), and the trigger was backed off.
    BackedOff,
}

/// Where a trigger stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TriggerStatus {
    /// Waiting for its next due time (`nextDueAtMs`).
    Scheduled,
    /// Watching files (`fs`).
    Watching,
    /// Fires only when asked (`manual`).
    Manual,
    /// A one-shot (`at`, `after`) that has fired.
    Done,
    /// Backed off after repeated failures to start; fires again at `backoffUntilMs`.
    BackedOff,
    /// Disabled in its definition.
    Disabled,
    /// Cannot run: see `error`.
    Failed,
}

/// One trigger and its state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TriggerInfo {
    /// The trigger's name.
    pub name: String,
    /// The `kind` of its `when`: `at`, `after`, `every`, `cron`, `fs` or `manual`.
    pub kind: String,
    /// The bot it starts.
    pub bot: String,
    /// The version of the definition in force.
    pub version: u32,
    /// Where it stands.
    pub status: TriggerStatus,
    /// When it fires next. Absent for `fs`, `manual`, a finished one-shot and a disabled trigger.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_due_at_ms: Option<u64>,
    /// When it last started a run (or merged into one).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at_ms: Option<u64>,
    /// The run it last started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run_id: Option<RunId>,
    /// Consecutive account-level failures to start; `0` when healthy.
    pub failures: u32,
    /// Until when it is backed off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backoff_until_ms: Option<u64>,
    /// Why it cannot run, when `status` is `failed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Result of `trigger.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TriggerListResult {
    /// Every trigger in the definitions in force, by name.
    pub triggers: Vec<TriggerInfo>,
}

/// Params of `trigger.fire`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TriggerFireParams {
    /// The trigger to fire now. A `manual` trigger or any enabled one.
    pub name: String,
}

/// Result of `trigger.fire`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TriggerFireResult {
    /// What became of it.
    pub outcome: FiringOutcome,
    /// The run started, or, for `coalesced`, the run it was merged into.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<RunId>,
    /// The occurrence it fired for.
    pub scheduled_at_ms: u64,
}

/// Params of the `triggers.watch` subscription.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TriggersWatchParams {}

/// One item of the `triggers.watch` subscription: every trigger and its state (latest value
/// wins). The first item is the list as it stands when the subscription starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TriggersSnapshot {
    /// Every trigger in the definitions in force, by name.
    pub triggers: Vec<TriggerInfo>,
}
