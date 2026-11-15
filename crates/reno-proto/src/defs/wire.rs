//! The request and reply types of `def.list`, `def.get`, `def.validate` and `defs.watch`.
//! They follow the protocol's `camelCase`; a `body` inside them is a definition in file shape
//! (see the module docs on wire case).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{DefError, DefKind};

/// The version of a bot's definition a run is pinned to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BotPin {
    /// The bot's name.
    pub name: String,
    /// The version of its definition when the run was submitted.
    pub version: u32,
}

/// One stored version of one definition, without its body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DefSummary {
    /// The kind.
    pub kind: DefKind,
    /// The name; `policy` for the policy.
    pub name: String,
    /// The version, counted from 1 per name.
    pub version: u32,
    /// The content hash of the version: 64 lowercase hex digits.
    pub hash: String,
    /// When this version was first stored, in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
}

/// The current definitions and the problems with the files, as the daemon holds them.
///
/// When `errors` is not empty the files on disk do not validate, and `defs` is the last set that
/// did: the daemon keeps running on it until the files are fixed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DefsSnapshot {
    /// The current version of every definition, by kind and then name.
    pub defs: Vec<DefSummary>,
    /// Every problem found in the last load; empty when the files are valid.
    #[serde(default)]
    pub errors: Vec<DefError>,
}

/// Params of `def.list`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DefListParams {
    /// Only this kind; all kinds when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<DefKind>,
}

/// Params of `def.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DefGetParams {
    /// The kind.
    pub kind: DefKind,
    /// The name; `policy` for the policy.
    pub name: String,
    /// The version; the newest when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
}

/// Result of `def.get`: one version with its body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DefRecord {
    /// The kind.
    pub kind: DefKind,
    /// The name.
    pub name: String,
    /// The version.
    pub version: u32,
    /// The content hash.
    pub hash: String,
    /// The definition, in the shape of its file.
    pub body: serde_json::Value,
    /// When this version was first stored, in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// Whether this is the newest stored version of the name.
    pub latest: bool,
}

/// Params of `def.validate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DefValidateParams {
    /// The kind.
    pub kind: DefKind,
    /// The name the body would be saved under: the file stem.
    pub name: String,
    /// The definition, in the shape of its file.
    pub body: serde_json::Value,
}

/// Result of `def.validate`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DefValidateResult {
    /// Whether the body passes every rule, in the context of the current definitions.
    pub valid: bool,
    /// Every problem found; empty when `valid`.
    pub errors: Vec<DefError>,
    /// The content hash the body would have, when valid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
}

/// Params of the `defs.watch` subscription: none.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DefsWatchParams {}
