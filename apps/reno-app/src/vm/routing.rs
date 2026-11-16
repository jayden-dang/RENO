//! Routing in words: the task a composer sends, the class chips, and a [`RoutingDecision`] as the
//! lines the Home preview and the Run screen show ("Routed to Codex: Claude Code was signed out").
//!
//! Pure functions of the daemon's answer, so every state is a unit test. Executor names come
//! from [`summary::executor_name`]; the id stays next to them as secondary text.

use reno_client::State;
use reno_proto::defs::DefKind;
use reno_proto::{
    AccessMode, Attachment, ExecutorId, FEATURE_ATTACHMENTS, FEATURE_DEFS, FEATURE_ROUTING,
    RejectReason, RoutingDecision, RuleMatched, SandboxLevel, StartFailureKind, TaskRunParams,
};

use crate::summary;

/// The class chips on the composer. A class is a bot: the router follows the bot's list of
/// executors, in its order of preference.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) enum Class {
    #[default]
    Implement,
    Review,
    Research,
    Triage,
}

impl Class {
    pub(crate) const ALL: [Self; 4] = [Self::Implement, Self::Review, Self::Research, Self::Triage];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Implement => "implement",
            Self::Review => "review",
            Self::Research => "research",
            Self::Triage => "triage",
        }
    }

    /// The bot a task of this class runs as.
    pub(crate) const fn bot(self) -> &'static str {
        match self {
            Self::Implement => "builder",
            Self::Review => "reviewer",
            Self::Research => "scout",
            Self::Triage => "keeper",
        }
    }
}

/// The task a composer sends, and the one it previews: the same, so what is shown is what runs.
///
/// The executor is left to the router. The class travels as its bot when the daemon has a bot of
/// that name; with none, the chip is only a label and the router uses its default order. A
/// daemon that predates routing cannot choose, so the first executor that looks usable is named.
/// Attachments go only to a daemon that takes them.
pub(crate) fn task_params(
    prompt: &str,
    class: Class,
    attachments: &[Attachment],
    state: &State,
) -> TaskRunParams {
    let has_bot = state.supports(FEATURE_DEFS)
        && state
            .defs()
            .defs
            .iter()
            .any(|d| d.kind == DefKind::Bot && d.name == class.bot());
    let routing = state.supports(FEATURE_ROUTING);
    TaskRunParams {
        prompt: prompt.to_owned(),
        executor: if routing {
            None
        } else {
            fallback_executor(state)
        },
        bot: has_bot.then(|| class.bot().to_owned()),
        attachments: if state.supports(FEATURE_ATTACHMENTS) {
            attachments.to_vec()
        } else {
            Vec::new()
        },
        ..TaskRunParams::default()
    }
}

/// The bot of `class` and its version, when the daemon has it and its definition is not read yet.
pub(crate) fn bot_to_fetch(state: &State, class: Class) -> Option<(String, u32)> {
    let bot = state
        .defs()
        .defs
        .iter()
        .find(|d| d.kind == DefKind::Bot && d.name == class.bot())?;
    state
        .def_record(DefKind::Bot, &bot.name, bot.version)
        .is_none()
        .then(|| (bot.name.clone(), bot.version))
}

/// The access mode the class's bot grants, in words, once its definition is read.
pub(crate) fn class_access(state: &State, class: Class) -> Option<&'static str> {
    let bot = state
        .defs()
        .defs
        .iter()
        .find(|d| d.kind == DefKind::Bot && d.name == class.bot())?;
    let record = state.def_record(DefKind::Bot, &bot.name, bot.version)?;
    crate::vm::runs::access_words(record.body.get("access")?.as_str()?)
}

/// What a daemon without a router is asked to run on: the first executor whose last probe found
/// it usable, else Claude Code.
fn fallback_executor(state: &State) -> Option<ExecutorId> {
    state
        .executors()
        .iter()
        .find(|e| e.health.as_ref().is_some_and(|h| h.ineligible().is_none()))
        .map(|e| e.descriptor.id.clone())
        .or_else(|| "claude-code".parse().ok())
}

/// What the Home preview knows.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum Preview {
    /// Nothing asked yet.
    #[default]
    Idle,
    /// The first answer has not come.
    Loading,
    Ready(RoutingDecision),
    /// The daemon could not say: no connection, an older daemon, a failed call.
    Unavailable(String),
}

/// An executor by name, with its id as the secondary text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Named {
    pub(crate) name: String,
    pub(crate) id: String,
}

impl Named {
    pub(crate) fn new(id: &ExecutorId) -> Self {
        Self {
            name: summary::executor_name(id.as_str()),
            id: id.as_str().to_owned(),
        }
    }

    /// The id, when it says something the name does not.
    pub(crate) fn secondary(&self) -> Option<&str> {
        (self.name != self.id).then_some(self.id.as_str())
    }
}

/// An executor the router passed over, and why, as a sentence.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Rejected {
    pub(crate) who: Named,
    pub(crate) reason: String,
}

/// One step of the fallback walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Step {
    pub(crate) from: Named,
    pub(crate) to: Option<Named>,
    pub(crate) reason: String,
}

/// A decision, as the screens draw it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RoutingView {
    pub(crate) chosen: Option<Named>,
    /// "Routed to Codex", or that nothing could take the task.
    pub(crate) headline: String,
    /// The one-line "because": "Claude Code was signed out". `None` when the first choice went.
    pub(crate) because: Option<String>,
    /// The rule that ordered the candidates.
    pub(crate) rule: String,
    /// The executors that passed the filter, best first.
    pub(crate) order: Vec<Named>,
    pub(crate) rejected: Vec<Rejected>,
    pub(crate) walk: Vec<Step>,
}

impl RoutingView {
    /// Whether there is anything to say under the headline.
    pub(crate) fn has_detail(&self) -> bool {
        !self.rejected.is_empty() || !self.walk.is_empty() || self.order.len() > 1
    }
}

pub(crate) fn view(decision: &RoutingDecision) -> RoutingView {
    let chosen = decision.chosen.as_ref().map(Named::new);
    let headline = match &chosen {
        Some(c) => format!("Routed to {}", c.name),
        None => "No executor can take this task".to_owned(),
    };
    let rejected: Vec<Rejected> = decision
        .rejected
        .iter()
        .map(|r| Rejected {
            who: Named::new(&r.executor),
            reason: reason_sentence(&r.reason),
        })
        .collect();
    let walk: Vec<Step> = decision
        .fallbacks
        .iter()
        .map(|step| Step {
            from: Named::new(&step.from),
            to: step.to.as_ref().map(Named::new),
            reason: reason_sentence(&step.reason),
        })
        .collect();

    // The "because" is what happened to the executors the task would otherwise have gone to:
    // the fallback walk when there was one, else the filter's rejections.
    let because = if decision.fallbacks.is_empty() {
        let parts: Vec<String> = decision
            .rejected
            .iter()
            .take(2)
            .map(|r| {
                format!(
                    "{} {}",
                    summary::executor_name(r.executor.as_str()),
                    happened(&r.reason)
                )
            })
            .collect();
        let more = decision.rejected.len().saturating_sub(2);
        (!parts.is_empty()).then(|| {
            let mut text = parts.join("; ");
            if more > 0 {
                text.push_str(&format!("; and {more} more"));
            }
            text
        })
    } else {
        let parts: Vec<String> = decision
            .fallbacks
            .iter()
            .map(|s| {
                format!(
                    "{} {}",
                    summary::executor_name(s.from.as_str()),
                    happened(&s.reason)
                )
            })
            .collect();
        Some(parts.join("; "))
    };

    RoutingView {
        chosen,
        headline,
        because,
        rule: rule_words(&decision.rule_matched),
        order: decision.ranked.iter().map(Named::new).collect(),
        rejected,
        walk,
    }
}

/// The rule that ordered the candidates, as a phrase.
pub(crate) fn rule_words(rule: &RuleMatched) -> String {
    match rule {
        RuleMatched::ExplicitExecutor => "The task named this executor".to_owned(),
        RuleMatched::BotPreference { bot } => format!("{bot}'s order of preference"),
        RuleMatched::Default => "Default order: no rule matched".to_owned(),
    }
}

/// What happened to an executor, to follow its name: "was signed out".
fn happened(reason: &RejectReason) -> &'static str {
    match reason {
        RejectReason::NotInstalled { .. } => "was not installed",
        RejectReason::TooOld { .. } => "was too old",
        RejectReason::SignedOut { .. } => "was signed out",
        RejectReason::AccessNotOffered { .. } => "does not offer that access",
        RejectReason::SandboxNotEnforced { .. } => "does not enforce that sandbox",
        RejectReason::Excluded { .. } => "was left out",
        RejectReason::NotRegistered => "is not set up here",
        RejectReason::StartFailed { .. } => "would not start",
    }
}

/// The full sentence for the "why?" list: what is wrong and what to do about it.
pub(crate) fn reason_sentence(reason: &RejectReason) -> String {
    match reason {
        RejectReason::NotInstalled { .. } => "Not installed.".to_owned(),
        RejectReason::TooOld { found, min } => match found {
            Some(found) => format!("Its CLI is too old: found {found}, needs {min} or newer."),
            None => format!("Its CLI is too old: needs {min} or newer."),
        },
        RejectReason::SignedOut { hint } => {
            let hint = hint.trim().trim_end_matches('.');
            if hint.is_empty() {
                "Signed out.".to_owned()
            } else {
                format!("Signed out. {}.", capitalise(hint))
            }
        }
        RejectReason::AccessNotOffered { requested, .. } => {
            format!("Does not offer {}.", access_words(*requested))
        }
        RejectReason::SandboxNotEnforced { requested, .. } => {
            format!("Does not enforce a {} sandbox.", sandbox_words(*requested))
        }
        RejectReason::Excluded { why } => format!("Left out: {}.", why.trim_end_matches('.')),
        RejectReason::NotRegistered => "Not set up on this daemon.".to_owned(),
        RejectReason::StartFailed { failure, detail } => match failure {
            StartFailureKind::NotInstalled => "Would not start: not installed.".to_owned(),
            StartFailureKind::NotSignedIn => "Would not start: signed out.".to_owned(),
            StartFailureKind::NoEligibleExecutor => {
                format!("Would not start: {}.", detail.trim_end_matches('.'))
            }
        },
    }
}

fn access_words(mode: AccessMode) -> &'static str {
    match mode {
        AccessMode::Supervised => "supervised access",
        AccessMode::AutoAcceptEdits => "auto-accept edits",
        AccessMode::FullAccess => "full access",
    }
}

fn sandbox_words(level: SandboxLevel) -> &'static str {
    match level {
        SandboxLevel::ReadOnly => "read-only",
        SandboxLevel::WorkspaceWrite => "workspace-write",
        SandboxLevel::Unrestricted => "unrestricted",
    }
}

fn capitalise(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use reno_client::Update;
    use reno_proto::defs::{DefSummary, DefsSnapshot};
    use reno_proto::{FallbackStep, Rejection};

    use super::*;

    fn id(s: &str) -> ExecutorId {
        s.parse().unwrap()
    }

    fn decision(chosen: Option<&str>) -> RoutingDecision {
        RoutingDecision {
            chosen: chosen.map(id),
            ranked: chosen.map(id).into_iter().collect(),
            rejected: vec![],
            rule_matched: RuleMatched::Default,
            fallbacks: vec![],
            at_ms: 1,
        }
    }

    fn signed_out(who: &str, hint: &str) -> Rejection {
        Rejection {
            executor: id(who),
            reason: RejectReason::SignedOut { hint: hint.into() },
        }
    }

    #[test]
    fn a_plain_decision_names_the_executor_and_has_no_because() {
        let v = view(&decision(Some("claude-code")));
        assert_eq!(v.headline, "Routed to Claude Code");
        assert_eq!(v.because, None);
        assert_eq!(v.rule, "Default order: no rule matched");
        assert!(!v.has_detail());
        assert_eq!(v.chosen.unwrap().secondary(), Some("claude-code"));
    }

    #[test]
    fn a_signed_out_first_choice_says_so_and_how_to_fix_it() {
        let mut d = decision(Some("codex"));
        d.rule_matched = RuleMatched::BotPreference {
            bot: "keeper".into(),
        };
        d.rejected = vec![signed_out("claude-code", "run `claude login`")];
        let v = view(&d);
        assert_eq!(v.headline, "Routed to Codex");
        assert_eq!(
            v.because.as_deref(),
            Some("Claude Code was signed out"),
            "{v:?}"
        );
        assert_eq!(v.rule, "keeper's order of preference");
        assert_eq!(v.rejected.len(), 1);
        assert_eq!(v.rejected[0].who.name, "Claude Code");
        assert_eq!(v.rejected[0].reason, "Signed out. Run `claude login`.");
        assert!(v.has_detail());
    }

    #[test]
    fn many_rejections_are_summarised() {
        let mut d = decision(Some("codex"));
        d.rejected = vec![
            signed_out("claude-code", "run `claude login`"),
            Rejection {
                executor: id("acp.grok"),
                reason: RejectReason::TooOld {
                    found: Some("0.4.0".into()),
                    min: "0.9.0".into(),
                },
            },
            Rejection {
                executor: id("acp.hermes"),
                reason: RejectReason::NotInstalled { detail: "x".into() },
            },
        ];
        let v = view(&d);
        assert_eq!(
            v.because.as_deref(),
            Some("Claude Code was signed out; Grok was too old; and 1 more")
        );
        assert_eq!(
            v.rejected[1].reason,
            "Its CLI is too old: found 0.4.0, needs 0.9.0 or newer."
        );
        assert_eq!(v.rejected[1].who.secondary(), Some("acp.grok"));
    }

    #[test]
    fn the_fallback_walk_is_the_because_and_is_listed_in_order() {
        let mut d = decision(Some("codex"));
        d.fallbacks = vec![FallbackStep {
            from: id("claude-code"),
            reason: RejectReason::StartFailed {
                failure: StartFailureKind::NotSignedIn,
                detail: "run `claude login`".into(),
            },
            to: Some(id("codex")),
            at_ms: 5,
        }];
        let v = view(&d);
        assert_eq!(v.because.as_deref(), Some("Claude Code would not start"));
        assert_eq!(v.walk.len(), 1);
        assert_eq!(v.walk[0].to.as_ref().unwrap().name, "Codex");
        assert_eq!(v.walk[0].reason, "Would not start: signed out.");
    }

    #[test]
    fn a_decision_with_no_executor_says_so() {
        let mut d = decision(None);
        d.rejected = vec![signed_out("codex", "run `codex login`")];
        let v = view(&d);
        assert_eq!(v.headline, "No executor can take this task");
        assert_eq!(v.chosen, None);
        assert_eq!(v.because.as_deref(), Some("Codex was signed out"));
    }

    #[test]
    fn every_reject_reason_reads_as_a_sentence() {
        let cases = [
            (
                RejectReason::NotInstalled { detail: "x".into() },
                "Not installed.",
            ),
            (RejectReason::SignedOut { hint: "".into() }, "Signed out."),
            (
                RejectReason::AccessNotOffered {
                    requested: AccessMode::FullAccess,
                    offered: vec![],
                },
                "Does not offer full access.",
            ),
            (
                RejectReason::SandboxNotEnforced {
                    requested: SandboxLevel::ReadOnly,
                    offered: vec![],
                },
                "Does not enforce a read-only sandbox.",
            ),
            (
                RejectReason::Excluded {
                    why: "the bot does not list it".into(),
                },
                "Left out: the bot does not list it.",
            ),
            (RejectReason::NotRegistered, "Not set up on this daemon."),
            (
                RejectReason::StartFailed {
                    failure: StartFailureKind::NoEligibleExecutor,
                    detail: "too old.".into(),
                },
                "Would not start: too old.",
            ),
        ];
        for (reason, text) in cases {
            assert_eq!(reason_sentence(&reason), text);
        }
    }

    #[test]
    fn classes_are_bots_and_default_to_implement() {
        assert_eq!(Class::default(), Class::Implement);
        let labels: Vec<_> = Class::ALL.iter().map(|c| c.label()).collect();
        assert_eq!(labels, ["implement", "review", "research", "triage"]);
        let bots: Vec<_> = Class::ALL.iter().map(|c| c.bot()).collect();
        assert_eq!(bots, ["builder", "reviewer", "scout", "keeper"]);
    }

    fn state_with(features: &[&str], bots: &[&str]) -> State {
        let mut state = State::new();
        state.apply(Update::Daemon(reno_client::DaemonInfo {
            hello: reno_proto::HelloResult {
                server_version: "t".into(),
                protocol_version: reno_proto::PROTOCOL_VERSION,
                features: features.iter().map(|f| (*f).to_owned()).collect(),
                pid: 1,
            },
            status: None,
        }));
        state.apply(Update::Defs(DefsSnapshot {
            defs: bots
                .iter()
                .map(|b| DefSummary {
                    kind: DefKind::Bot,
                    name: (*b).to_owned(),
                    version: 1,
                    hash: "0".repeat(64),
                    created_at_ms: 1,
                })
                .collect(),
            errors: vec![],
        }));
        state
    }

    #[test]
    fn the_task_leaves_the_executor_to_the_router_and_sends_the_class_as_a_bot_it_has() {
        let all = [FEATURE_ROUTING, FEATURE_DEFS, FEATURE_ATTACHMENTS];
        let state = state_with(&all, &["reviewer"]);
        let file = Attachment {
            path: "/tmp/a.png".into(),
            name: "a.png".into(),
            mime: None,
        };
        let p = task_params("go", Class::Review, std::slice::from_ref(&file), &state);
        assert_eq!(p.executor, None);
        assert_eq!(p.bot.as_deref(), Some("reviewer"));
        assert_eq!(p.attachments, std::slice::from_ref(&file));
        // A class whose bot the daemon lacks is only a label.
        let p = task_params("go", Class::Research, &[], &state);
        assert_eq!(p.bot, None);
        // An older daemon takes no attachments and no bot.
        let old = state_with(&[FEATURE_ROUTING], &["reviewer"]);
        let p = task_params("go", Class::Review, &[file], &old);
        assert_eq!((p.bot, p.attachments.len()), (None, 0));
    }

    #[test]
    fn a_daemon_without_a_router_is_asked_for_an_executor_by_name() {
        let state = state_with(&[], &[]);
        let p = task_params("go", Class::Implement, &[], &state);
        assert_eq!(p.executor, Some(id("claude-code")));
    }
}
