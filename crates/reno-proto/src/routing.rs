//! Routing on the wire: which executor a task goes to, and why (`route.preview`, `run.routing`).
//!
//! A [`RoutingDecision`] is what the kernel's router decided, kept for the run it was made for.
//! It answers "why this executor": which candidates passed the filter and in what order, which
//! were dropped and for what typed reason, which rule put them in that order, and what happened
//! when the first choice would not start.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::exec::{AccessMode, SandboxLevel};
use crate::ids::{ExecutorId, RunId};
use crate::protocol::TaskRunParams;

/// Why an executor was not chosen. The first six are the stage 1 filter; the last two are what
/// the dispatcher found when it tried.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum RejectReason {
    /// The harness is not installed, or cannot be launched.
    NotInstalled {
        /// What the last probe said, for a person.
        detail: String,
    },
    /// The installed version is older than the adapter supports.
    TooOld {
        /// The version found, when it could be read.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        found: Option<String>,
        /// The oldest supported version.
        min: String,
    },
    /// The harness has no login.
    SignedOut {
        /// What to do about it.
        hint: String,
    },
    /// The executor does not offer the access mode the task asks for. A task is never run with
    /// a different one.
    AccessNotOffered {
        /// What the task asked for.
        requested: AccessMode,
        /// What the executor offers.
        offered: Vec<AccessMode>,
    },
    /// The executor does not enforce the sandbox level the task asks for.
    SandboxNotEnforced {
        /// What the task asked for.
        requested: SandboxLevel,
        /// What the executor enforces.
        offered: Vec<SandboxLevel>,
    },
    /// The task leaves this executor out: the task pins another executor, its bot does not list
    /// this one, or the request excludes it.
    Excluded {
        /// Which of those.
        why: String,
    },
    /// The daemon has no executor with this id.
    NotRegistered,
    /// The executor was chosen and then would not start, for an account or install reason. Added
    /// by the fallback walk.
    StartFailed {
        /// What the dispatcher found.
        failure: StartFailureKind,
        /// The refusal, for a person.
        detail: String,
    },
}

/// The reasons a start can fail that make the dispatcher try the next candidate. A failure after
/// the executor started is not one of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StartFailureKind {
    /// Not installed.
    NotInstalled,
    /// Signed out.
    NotSignedIn,
    /// Not eligible for the task: too old, or an access mode or sandbox it does not offer.
    NoEligibleExecutor,
}

/// An executor that was dropped, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Rejection {
    /// The executor.
    pub executor: ExecutorId,
    /// Why.
    pub reason: RejectReason,
}

/// The stage 2 rule that ordered the candidates. The first rule that matches wins.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum RuleMatched {
    /// The task named its executor.
    ExplicitExecutor,
    /// The task's bot lists its executors in order of preference.
    BotPreference {
        /// The bot.
        bot: String,
    },
    /// No rule applied: the eligible executors in id order.
    Default,
}

/// One step of the fallback walk: the executor that would not start, and what came next.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FallbackStep {
    /// The executor that would not start.
    pub from: ExecutorId,
    /// Why.
    pub reason: RejectReason,
    /// The next candidate tried; absent when there was none left.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<ExecutorId>,
    /// When, in milliseconds since the Unix epoch.
    pub at_ms: u64,
}

/// Where a task goes and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RoutingDecision {
    /// The executor the run starts on. Absent when no executor is eligible: the run then fails
    /// with `NoEligibleExecutor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chosen: Option<ExecutorId>,
    /// The candidates that passed the filter, best first. `chosen` is the first of them until a
    /// fallback moves on.
    #[serde(default)]
    pub ranked: Vec<ExecutorId>,
    /// The candidates the filter dropped, and the executors the fallback walk gave up on.
    #[serde(default)]
    pub rejected: Vec<Rejection>,
    /// The rule that ordered `ranked`.
    pub rule_matched: RuleMatched,
    /// The fallback walk, in order. Empty when the first choice started.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fallbacks: Vec<FallbackStep>,
    /// When the decision was made, in milliseconds since the Unix epoch.
    pub at_ms: u64,
}

impl RoutingDecision {
    /// One line a person can read: who was chosen, by which rule, and who was passed over.
    pub fn explain(&self) -> String {
        let rule = match &self.rule_matched {
            RuleMatched::ExplicitExecutor => "the task named it".to_owned(),
            RuleMatched::BotPreference { bot } => format!("bot {bot} prefers it"),
            RuleMatched::Default => "default order".to_owned(),
        };
        let mut text = match &self.chosen {
            Some(chosen) => format!("{chosen} ({rule})"),
            None => format!("no eligible executor ({rule})"),
        };
        for Rejection { executor, reason } in &self.rejected {
            text.push_str(&format!("; not {executor}: {reason}"));
        }
        text
    }
}

impl std::fmt::Display for RejectReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInstalled { detail } => write!(f, "not installed ({detail})"),
            Self::TooOld { found, min } => match found {
                Some(found) => write!(f, "too old ({found}, needs {min} or newer)"),
                None => write!(f, "too old (needs {min} or newer)"),
            },
            Self::SignedOut { hint } => write!(f, "signed out ({hint})"),
            Self::AccessNotOffered { requested, offered } => write!(
                f,
                "does not offer access mode {requested:?} (offers {offered:?})"
            ),
            Self::SandboxNotEnforced { requested, offered } => write!(
                f,
                "does not enforce sandbox {requested:?} (enforces {offered:?})"
            ),
            Self::Excluded { why } => write!(f, "excluded ({why})"),
            Self::NotRegistered => write!(f, "not an executor of this daemon"),
            Self::StartFailed { failure, detail } => {
                write!(f, "would not start ({failure:?}): {detail}")
            }
        }
    }
}

/// Params of `route.preview`: the decision a task would get, without starting anything.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RoutePreviewParams {
    /// The task, as `task.run` takes it. Nothing is created.
    pub task: TaskRunParams,
}

/// Result of `route.preview`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RoutePreviewResult {
    /// What the router decides now.
    pub decision: RoutingDecision,
}

/// Params of `run.routing`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunRoutingParams {
    /// The run.
    pub run_id: RunId,
}

/// Result of `run.routing`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunRoutingResult {
    /// The run.
    pub run_id: RunId,
    /// Its routing decision, with every fallback step so far. Absent for a run made before
    /// routing existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<RoutingDecision>,
}
