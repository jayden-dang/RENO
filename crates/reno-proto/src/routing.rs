//! Routing on the wire: which executor a task goes to, and why (`route.preview`, `run.routing`).
//!
//! A [`RoutingDecision`] is what the kernel's router decided, kept for the run it was made for.
//! It answers "why this executor": which candidates passed the filter and in what order, which
//! were dropped and for what typed reason, which rule put them in that order, and what happened
//! when the first choice would not start.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::exec::{AccessMode, SandboxLevel, list_in_words};
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
    /// Why. When more than one thing is wrong this is the first the filter checks (access mode,
    /// then sandbox, then health), and what else is wrong is in `also`.
    pub reason: RejectReason,
    /// The other reasons that hold too, in the filter's order: an executor that does not offer
    /// the access mode and is not installed has the first as `reason` and the second here, so a
    /// person sees what they would have to change. Absent from older daemons, and when there is
    /// only one reason. Which executors are eligible does not depend on it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub also: Vec<RejectReason>,
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
    /// Things worth a person's attention that did not stop the run: the model the bot pins is
    /// not in the chosen executor's model list (a list that may be curated and incomplete, so
    /// the run still starts).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl RejectReason {
    /// The `Excluded` reason of an executor its bot does not list.
    pub fn unlisted_by_bot(bot: &str) -> Self {
        Self::Excluded {
            why: format!("bot {bot} {UNLISTED_TAIL}"),
        }
    }

    /// The bot, when this is the reason [`Self::unlisted_by_bot`] makes.
    pub fn unlisting_bot(&self) -> Option<&str> {
        match self {
            Self::Excluded { why } => why
                .strip_prefix("bot ")?
                .strip_suffix(UNLISTED_TAIL)?
                .strip_suffix(' '),
            _ => None,
        }
    }
}

/// What follows the bot's name in the reason an unlisted executor carries.
const UNLISTED_TAIL: &str = "does not list it";

impl RoutingDecision {
    /// One line a person can read: who was chosen, by which rule, which executor is next in line,
    /// and the executors passed over for a reason about the executor itself (not installed,
    /// signed out). Executors the task or its bot simply leaves out are not listed; use
    /// [`Self::explain_verbose`] or [`Self::left_out_lines`] for them.
    pub fn explain(&self) -> String {
        let mut text = self.headline(true);
        for rejection in &self.rejected {
            if !matches!(rejection.reason, RejectReason::Excluded { .. }) {
                text.push_str(&format!(
                    "; not {}: {}",
                    rejection.executor,
                    rejection.reasons()
                ));
            }
        }
        text
    }

    /// [`Self::explain`] with every passed-over executor and its reason.
    pub fn explain_verbose(&self) -> String {
        let mut text = self.headline(false);
        for rejection in &self.rejected {
            text.push_str(&format!(
                "; not {}: {}",
                rejection.executor,
                rejection.reasons()
            ));
        }
        text
    }

    /// The executors left out by the task or its bot, one line per reason: `6 others not listed
    /// by bot keeper`. Empty when none were.
    pub fn left_out_lines(&self) -> Vec<String> {
        let mut groups: Vec<(String, usize)> = Vec::new();
        for Rejection { reason, .. } in &self.rejected {
            let label = match reason {
                RejectReason::Excluded { why } => match reason.unlisting_bot() {
                    Some(bot) => format!("not listed by bot {bot}"),
                    None => format!("excluded ({why})"),
                },
                _ => continue,
            };
            match groups.iter_mut().find(|(known, _)| *known == label) {
                Some((_, count)) => *count += 1,
                None => groups.push((label, 1)),
            }
        }
        groups
            .into_iter()
            .map(|(label, count)| match count {
                1 => format!("1 other {label}"),
                n => format!("{n} others {label}"),
            })
            .collect()
    }

    /// `claude-code (bot keeper prefers it; fallback codex)`.
    fn headline(&self, with_fallback: bool) -> String {
        let rule = match &self.rule_matched {
            RuleMatched::ExplicitExecutor => "the task named it".to_owned(),
            RuleMatched::BotPreference { bot } => format!("bot {bot} prefers it"),
            RuleMatched::Default => "default order".to_owned(),
        };
        let Some(chosen) = &self.chosen else {
            return format!("no eligible executor ({rule})");
        };
        let next = self
            .ranked
            .iter()
            .position(|id| id == chosen)
            .and_then(|at| self.ranked.get(at + 1));
        match next {
            Some(next) if with_fallback => format!("{chosen} ({rule}; fallback {next})"),
            _ => format!("{chosen} ({rule})"),
        }
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
                "does not offer access mode {requested} (offers {})",
                list_in_words(offered)
            ),
            Self::SandboxNotEnforced { requested, offered } => write!(
                f,
                "does not enforce sandbox {requested} (enforces {})",
                list_in_words(offered)
            ),
            Self::Excluded { why } => write!(f, "excluded ({why})"),
            Self::NotRegistered => write!(f, "not an executor of this daemon"),
            Self::StartFailed { failure, detail } => {
                write!(f, "would not start ({failure}): {detail}")
            }
        }
    }
}

impl std::fmt::Display for StartFailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::NotInstalled => "not installed",
            Self::NotSignedIn => "not signed in",
            Self::NoEligibleExecutor => "no eligible executor",
        })
    }
}

impl Rejection {
    /// An executor dropped for one reason.
    pub fn new(executor: ExecutorId, reason: RejectReason) -> Self {
        Self {
            executor,
            reason,
            also: Vec::new(),
        }
    }

    /// Every reason, the first one first, as a person reads it: `does not offer access mode
    /// Supervised (offers Full access), and not installed (install it)`.
    pub fn reasons(&self) -> String {
        let mut text = self.reason.to_string();
        for also in &self.also {
            text.push_str(&format!(", and {also}"));
        }
        text
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
