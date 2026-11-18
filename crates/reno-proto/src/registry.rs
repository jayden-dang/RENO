//! Agents from the ACP registry as a client sees them: browsing the index, adding an agent and
//! removing one (feature `executor.registry`).
//!
//! An added agent is an ordinary executor (`acp.<registry id>`) with [`ExecutorSource::Registry`];
//! the index only describes what could be added. Adding runs as a person's install job on that
//! executor, so it is followed through `executors.watch` like any other job.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::ExecutorId;

/// Feature: `executor.registry.list`, `executor.registry.add` and `executor.registry.remove`
/// exist, and `ExecutorInfo` may carry `source`.
pub const FEATURE_EXECUTOR_REGISTRY: &str = "executor.registry";

/// Where an executor comes from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorSource {
    /// Reno drives it itself (its own harness), or it is the mock.
    #[default]
    Builtin,
    /// Added from the ACP registry, through the generic ACP driver.
    Registry,
}

impl ExecutorSource {
    /// For `skip_serializing_if`: the default is left off the wire.
    pub fn is_builtin(&self) -> bool {
        matches!(self, Self::Builtin)
    }
}

/// How the registry's entry would be installed on this device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RegistryDistribution {
    /// A prebuilt archive for this platform.
    Binary,
    /// An npm package, run with Node.js.
    Npx,
    /// A Python package, installed with uv.
    Uvx,
}

/// What this device lacks to install an entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RegistryReadiness {
    /// Nothing: it can be added now.
    Ready,
    /// Node.js (npm) is not installed.
    NeedsNode,
    /// uv is not installed.
    NeedsUv,
    /// The entry has no build for this platform.
    NoBuild,
}

/// An entry that is added on this device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RegistryAdded {
    /// The version installed.
    pub version: String,
    /// The index lists a different (newer or not) version than the installed one.
    pub update_available: bool,
}

/// One agent as the list shows it: an agent of the registry's index, or one added on this device
/// that the index no longer lists (`delisted`) or that is the person's own (`own`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RegistryEntry {
    /// The registry's id (`gemini`, `goose`), or `own.<slug>` for a person's own agent. What
    /// `executor.registry.add` and `.remove` take.
    pub id: String,
    /// The executor the agent runs under once added (`acp.gemini`).
    pub executor: ExecutorId,
    /// Display name.
    pub name: String,
    /// The version the index lists (the installed one for a delisted or own agent).
    pub version: String,
    /// One paragraph, as the registry words it.
    #[serde(default)]
    pub description: String,
    /// Who makes it.
    #[serde(default)]
    pub authors: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository: Option<String>,
    /// The registry's icon, a sanitized one-ink SVG (cached with the index). Absent when it has
    /// none or it could not be fetched.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
    /// The build chosen for this device. Absent when there is none (`readiness: no_build`) and
    /// for an own agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<RegistryDistribution>,
    /// What this device lacks to add it.
    pub readiness: RegistryReadiness,
    /// What to do about `readiness`, when it is not `ready` ("brew install uv").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fix: Option<String>,
    /// Where a binary downloads from (its host name). Absent for a package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// For a binary: whether the registry publishes a sha256 the download is checked against
    /// (`false`: it installs unchecked). Absent for a package, which npm or PyPI checks.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<bool>,
    /// Set when Reno drives this agent itself: the built-in executor to use instead (`codex`).
    /// Such an entry is listed so a search finds it, and is never added from the registry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub built_in: Option<ExecutorId>,
    /// Set when the agent is added on this device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub added: Option<RegistryAdded>,
    /// An add or update of it is running.
    #[serde(default)]
    pub adding: bool,
    /// Added, but the index no longer lists it: it can be removed, not updated.
    #[serde(default)]
    pub delisted: bool,
    /// The person's own agent (`own.<slug>`): never installed or updated by Reno.
    #[serde(default)]
    pub own: bool,
}

/// Params of `executor.registry.list`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorRegistryListParams {
    /// Fetch the index now, even if the cached copy is fresh (it stays fresh for 5 minutes).
    #[serde(default)]
    pub refresh: bool,
}

/// Result of `executor.registry.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorRegistryListResult {
    /// Every agent the index lists, built-in ones included, then the agents added here that it
    /// does not list, sorted by name.
    pub entries: Vec<RegistryEntry>,
    /// When the index was fetched (ms since the Unix epoch).
    pub fetched_at_ms: u64,
    /// The index could not be fetched, so this is the cached copy of `fetched_at_ms`; `error`
    /// says why.
    #[serde(default)]
    pub offline: bool,
    /// Why the fetch failed, when `offline`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Params of `executor.registry.add`, `.remove`, `.env_get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorRegistryParams {
    /// The registry's id (`own.<slug>` for an own agent).
    pub id: String,
}

/// Result of `executor.registry.remove`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorRegistryRemoveResult {
    /// The executor that was removed.
    pub executor: ExecutorId,
}

/// Params of `executor.registry.add_own`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorRegistryAddOwnParams {
    /// What to call it; its id is `own.<slug of the name>`.
    pub name: String,
    /// The program and its arguments, as typed (`my-agent --acp`); quotes group words. Run as
    /// the agent, never installed or updated by Reno.
    pub command: String,
    /// The agent's own environment, as `KEY=VALUE` lines. Kept owner-only under Reno's data
    /// dir, never in a log.
    #[serde(default)]
    pub env: String,
}

/// Params of `executor.registry.env_set`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorRegistryEnvSetParams {
    /// The registry's id (or `own.<slug>`) of an added agent.
    pub id: String,
    /// `KEY=VALUE` lines (blank lines and `#` comments skipped); replaces the agent's own
    /// environment. Empty clears it.
    pub env: String,
}

/// Result of `executor.registry.env_get` and `.env_set`: the agent's own environment, which the
/// registry's own variables for it sit under (yours win).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorRegistryEnvResult {
    pub id: String,
    /// `KEY=VALUE` lines.
    pub env: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_builtin_source_is_left_off_the_wire_and_read_back_by_default() {
        assert!(ExecutorSource::default().is_builtin());
        assert_eq!(
            serde_json::to_value(ExecutorSource::Registry).unwrap(),
            "registry"
        );
    }

    #[test]
    fn an_entry_without_optional_fields_round_trips() {
        let entry: RegistryEntry = serde_json::from_value(serde_json::json!({
            "id": "goose", "executor": "acp.goose", "name": "goose", "version": "1.0.0",
            "readiness": "needs_uv"
        }))
        .unwrap();
        assert_eq!(entry.readiness, RegistryReadiness::NeedsUv);
        assert!(entry.added.is_none() && entry.built_in.is_none());
        let json = serde_json::to_value(&entry).unwrap();
        assert!(json.get("added").is_none() && json.get("icon").is_none());
    }
}
