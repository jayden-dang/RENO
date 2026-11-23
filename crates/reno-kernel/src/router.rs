//! The router: which executor a task goes to, and why (ARCHITECTURE 5.2). Pure: it reads no
//! clock, store or process. Callers hand it the candidates with their health, and the time.
//!
//! # Stages
//!
//! 1. **Filter.** Every candidate that cannot take the task is dropped with a typed
//!    [`RejectReason`]: left out by the task (excluded), not installed, too old, signed out, an
//!    access mode it does not offer, a sandbox level it does not enforce. The checks are the
//!    dispatcher's own ([`ineligibility`]), so a task is never routed to an executor the
//!    dispatcher would refuse. In particular a `Supervised` task never reaches an executor that
//!    offers only `FullAccess`.
//! 2. **Rules**, in order, the first that applies wins:
//!    1. the task names its executor: only that one is a candidate (no fallback beyond it);
//!    2. the task runs as a bot: the bot's `executors` list, in its order, is the preference and
//!       the whole candidate set;
//!    3. otherwise only the daemon's default executors, in their order (`Default`); any other
//!       executor is rejected as not a default, reachable by naming it or through a bot.
//!
//!    Rules from definitions (a `[routing]` section) and scoring by past results are not part of
//!    this change; the rule list is where they go.
//!
//! The output is a [`RoutingDecision`]: the chosen executor (the first of `ranked`), the ranked
//! list, everything rejected with its reason, the rule that matched, and the time. The
//! dispatcher continues the same decision when the chosen executor will not start
//! ([`fall_back`]).

use std::collections::HashSet;

use reno_proto::{
    AccessMode, ExecutorDescriptor, ExecutorHealth, ExecutorId, FallbackStep, Ineligible,
    NotInstalledReason, RejectReason, Rejection, RoutingDecision, RuleMatched, SandboxLevel,
    StartFailureKind,
};

/// A registered executor as the router sees it.
#[derive(Debug, Clone)]
pub struct Candidate {
    /// What it can do.
    pub descriptor: ExecutorDescriptor,
    /// What this process last found out about it, when it has probed it. `None` is eligible: an
    /// executor nobody has probed is let through and says so itself if it cannot run.
    pub health: Option<ExecutorHealth>,
    /// Why no run may start on it now: an install or update of it is running (EX-4). Not a
    /// verdict on the executor, so it comes after every other reason.
    pub busy: Option<String>,
}

/// The bot a task runs as, as far as routing is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BotPreference {
    /// The bot's name.
    pub name: String,
    /// Its executors, in order of preference.
    pub executors: Vec<ExecutorId>,
}

/// What the router is asked.
#[derive(Debug, Clone)]
pub struct RouteRequest {
    /// The access mode the task asks for.
    pub access: AccessMode,
    /// The sandbox level the task asks for.
    pub sandbox: SandboxLevel,
    /// The executor the task names, if it does.
    pub explicit: Option<ExecutorId>,
    /// The bot the task runs as, if any.
    pub bot: Option<BotPreference>,
    /// Executors the task must not go to.
    pub exclude: Vec<ExecutorId>,
    /// The executors a task with neither a bot nor a named executor may go to, in order.
    pub defaults: Vec<ExecutorId>,
    /// Executors whose models never take the effort the task asks for, with what to say. Only
    /// filled when the task names no executor.
    pub unfit_effort: Vec<(ExecutorId, String)>,
    /// Executors the task's bot's settings rule out, with what to say. Applies to a named
    /// executor too.
    pub unfit_config: Vec<(ExecutorId, String)>,
}

/// Why `descriptor` cannot take a task that asks for `access` and `sandbox`, given what its last
/// probe found; `None` when it can. When several things are wrong this is the first, in the
/// dispatcher's order: access, sandbox, health. [`ineligibilities`] has all of them.
pub fn ineligibility(
    descriptor: &ExecutorDescriptor,
    health: Option<&ExecutorHealth>,
    access: AccessMode,
    sandbox: SandboxLevel,
) -> Option<RejectReason> {
    ineligibilities(descriptor, health, access, sandbox)
        .into_iter()
        .next()
}

/// Every reason `descriptor` cannot take the task, in the order of [`ineligibility`]; empty when
/// it can. An executor that does not offer the access mode and is not installed has both, and a
/// person has to put both right. Eligibility is the same as for [`ineligibility`].
pub fn ineligibilities(
    descriptor: &ExecutorDescriptor,
    health: Option<&ExecutorHealth>,
    access: AccessMode,
    sandbox: SandboxLevel,
) -> Vec<RejectReason> {
    let mut found = Vec::new();
    if !descriptor.caps.access_modes.contains(&access) {
        found.push(RejectReason::AccessNotOffered {
            requested: access,
            offered: descriptor.caps.access_modes.clone(),
        });
    }
    // `Unrestricted` asks for no ceiling beyond the access mode, so it needs no declaration.
    if sandbox != SandboxLevel::Unrestricted && !descriptor.caps.sandbox_levels.contains(&sandbox) {
        found.push(RejectReason::SandboxNotEnforced {
            requested: sandbox,
            offered: descriptor.caps.sandbox_levels.clone(),
        });
    }
    found.extend(
        health
            .and_then(ExecutorHealth::ineligible)
            .map(|why| match why {
                Ineligible::NotInstalled(why) => RejectReason::NotInstalled {
                    detail: match why {
                        NotInstalledReason::NotFound { hint } => hint,
                        NotInstalledReason::NeedsInstall { what } => {
                            format!("{what} needs to be installed")
                        }
                        NotInstalledReason::Unusable { detail } => detail,
                        NotInstalledReason::ProbeFailed { detail } => {
                            format!("the probe failed: {detail}")
                        }
                    },
                },
                Ineligible::TooOld { found, min } => RejectReason::TooOld { found, min },
                Ineligible::SignedOut(hint) => RejectReason::SignedOut { hint },
            }),
    );
    found
}

/// The reason an executor with a running install or update is passed over. It reuses
/// `StartFailed` (an older client reads it) rather than a variant of its own: a variant a client
/// does not know would make its whole routing decision unreadable.
pub(crate) fn busy_reason(why: &str) -> RejectReason {
    RejectReason::StartFailed {
        failure: StartFailureKind::NoEligibleExecutor,
        detail: why.to_owned(),
    }
}

/// Decides where a task goes. `candidates` are the registered executors; `at_ms` is the time to
/// stamp on the decision.
pub fn route(request: &RouteRequest, candidates: &[Candidate], at_ms: u64) -> RoutingDecision {
    let (rule_matched, preferred): (RuleMatched, Vec<ExecutorId>) =
        match (&request.explicit, &request.bot) {
            (Some(explicit), _) => (RuleMatched::ExplicitExecutor, vec![explicit.clone()]),
            (None, Some(bot)) => (
                RuleMatched::BotPreference {
                    bot: bot.name.clone(),
                },
                bot.executors.clone(),
            ),
            (None, None) => (
                RuleMatched::Default,
                // A default this daemon does not have is not worth a line: it is not a candidate.
                request
                    .defaults
                    .iter()
                    .filter(|id| candidates.iter().any(|c| &c.descriptor.id == *id))
                    .cloned()
                    .collect(),
            ),
        };
    // Preferred executors first, in their order; then every other candidate by id.
    let mut rest: Vec<&Candidate> = candidates.iter().collect();
    rest.sort_by(|a, b| a.descriptor.id.as_str().cmp(b.descriptor.id.as_str()));
    let mut seen = HashSet::new();
    let order: Vec<ExecutorId> = preferred
        .iter()
        .cloned()
        .chain(rest.iter().map(|c| c.descriptor.id.clone()))
        .filter(|id| seen.insert(id.clone()))
        .collect();

    let mut ranked = Vec::new();
    let mut rejected = Vec::new();
    for id in order {
        // The first reason is the verdict; an executor the task leaves out has only that one.
        let mut reasons = match candidates.iter().find(|c| c.descriptor.id == id) {
            None => vec![RejectReason::NotRegistered],
            Some(candidate) => match excluded(request, &id) {
                Some(left_out) => vec![left_out],
                None => {
                    let mut found = ineligibilities(
                        &candidate.descriptor,
                        candidate.health.as_ref(),
                        request.access,
                        request.sandbox,
                    );
                    found.extend(candidate.busy.as_deref().map(busy_reason));
                    found
                }
            },
        }
        .into_iter();
        match reasons.next() {
            None => ranked.push(id),
            Some(reason) => rejected.push(Rejection {
                executor: id,
                reason,
                also: reasons.collect(),
            }),
        }
    }
    RoutingDecision {
        chosen: ranked.first().cloned(),
        ranked,
        rejected,
        rule_matched,
        fallbacks: Vec::new(),
        at_ms,
        warnings: Vec::new(),
    }
}

/// Whether the task itself leaves `id` out.
fn excluded(request: &RouteRequest, id: &ExecutorId) -> Option<RejectReason> {
    let why = if request.exclude.contains(id) {
        "the request excludes it".to_owned()
    } else if let Some((_, why)) = request.unfit_config.iter().find(|(unfit, _)| unfit == id) {
        why.clone()
    } else if let Some((_, why)) = request.unfit_effort.iter().find(|(unfit, _)| unfit == id) {
        why.clone()
    } else if let Some(explicit) = &request.explicit {
        if explicit == id {
            return None;
        }
        format!("the task names {explicit}")
    } else {
        match request.bot.as_ref() {
            Some(bot) if bot.executors.contains(id) => return None,
            Some(bot) => return Some(RejectReason::unlisted_by_bot(&bot.name)),
            None if request.defaults.contains(id) => return None,
            None => "not a default executor: name it, or use a bot that lists it".to_owned(),
        }
    };
    Some(RejectReason::Excluded { why })
}

/// The kind of start failure a refusal is, for the walk.
pub(crate) fn failure_kind(reason: &RejectReason) -> StartFailureKind {
    match reason {
        RejectReason::NotInstalled { .. } => StartFailureKind::NotInstalled,
        RejectReason::SignedOut { .. } => StartFailureKind::NotSignedIn,
        _ => StartFailureKind::NoEligibleExecutor,
    }
}

/// One step of the fallback walk: `failed` would not start for `reason`. It leaves `ranked`
/// and joins `rejected` as a `StartFailed`, the step is recorded, and the next candidate in
/// `ranked` becomes `chosen` (`None` when there is none). Each step removes one candidate, so
/// the walk is bounded by the number of candidates and cannot loop.
pub fn fall_back(
    decision: &mut RoutingDecision,
    failed: &ExecutorId,
    reason: &RejectReason,
    detail: &str,
    at_ms: u64,
) {
    decision.ranked.retain(|id| id != failed);
    decision.rejected.push(Rejection {
        also: Vec::new(),
        executor: failed.clone(),
        reason: RejectReason::StartFailed {
            failure: failure_kind(reason),
            detail: detail.to_owned(),
        },
    });
    decision.chosen = decision.ranked.first().cloned();
    decision.fallbacks.push(FallbackStep {
        from: failed.clone(),
        reason: reason.clone(),
        to: decision.chosen.clone(),
        at_ms,
    });
}

#[cfg(test)]
mod tests {
    use reno_harness::exec::Executor;
    use reno_proto::{InstallStatus, ModelCatalog, SignedIn, VersionGate};

    use super::*;

    fn id(name: &str) -> ExecutorId {
        name.parse().unwrap()
    }

    fn descriptor(
        name: &str,
        access: &[AccessMode],
        sandbox: &[SandboxLevel],
    ) -> ExecutorDescriptor {
        let mut descriptor = reno_harness::exec::MockExecutor::new(id(name)).descriptor();
        descriptor.caps.access_modes = access.to_vec();
        descriptor.caps.sandbox_levels = sandbox.to_vec();
        descriptor
    }

    fn health(installed: InstallStatus, gate: VersionGate, signed_in: SignedIn) -> ExecutorHealth {
        ExecutorHealth {
            installed,
            version_gate: gate,
            signed_in,
            models: ModelCatalog::curated(vec![]),
            checked_at_ms: 1,
            plan_usage: None,
        }
    }

    fn ok() -> ExecutorHealth {
        health(
            InstallStatus::Installed {
                path: "/bin/x".into(),
                version: Some("1.5.0".into()),
            },
            VersionGate::Ok,
            SignedIn::Yes { account: None },
        )
    }

    fn candidate(name: &str, health: Option<ExecutorHealth>) -> Candidate {
        Candidate {
            descriptor: descriptor(
                name,
                &[AccessMode::Supervised, AccessMode::FullAccess],
                &[SandboxLevel::Unrestricted],
            ),
            health,
            busy: None,
        }
    }

    fn request() -> RouteRequest {
        RouteRequest {
            access: AccessMode::Supervised,
            sandbox: SandboxLevel::Unrestricted,
            explicit: None,
            bot: None,
            exclude: vec![],
            unfit_effort: vec![],
            unfit_config: Vec::new(),
            defaults: [
                "claude-code",
                "codex",
                "a-missing",
                "b-old",
                "c-out",
                "d-unknown",
                "yolo",
            ]
            .map(id)
            .to_vec(),
        }
    }

    fn bot(names: &[&str]) -> Option<BotPreference> {
        Some(BotPreference {
            name: "keeper".into(),
            executors: names.iter().map(|n| id(n)).collect(),
        })
    }

    #[test]
    fn an_executor_that_fails_two_checks_is_dropped_for_both() {
        let missing = health(
            InstallStatus::NotInstalled {
                why: NotInstalledReason::NotFound {
                    hint: "install it".into(),
                },
            },
            VersionGate::Ok,
            SignedIn::Unknown,
        );
        // Offers only full access and is not installed; a supervised run needs both changed.
        let devin = Candidate {
            descriptor: descriptor(
                "acp.devin",
                &[AccessMode::FullAccess],
                &[SandboxLevel::Unrestricted],
            ),
            health: Some(missing),
            busy: None,
        };
        let named = RouteRequest {
            explicit: Some(id("acp.devin")),
            ..request()
        };
        let decision = route(&named, std::slice::from_ref(&devin), 1);
        assert!(decision.ranked.is_empty());
        let [rejection] = decision.rejected.as_slice() else {
            panic!("{decision:?}");
        };
        // The first reason is the one the dispatcher has always given.
        assert_eq!(
            rejection.reason,
            RejectReason::AccessNotOffered {
                requested: AccessMode::Supervised,
                offered: vec![AccessMode::FullAccess],
            }
        );
        assert_eq!(
            rejection.also,
            [RejectReason::NotInstalled {
                detail: "install it".into()
            }]
        );
        assert_eq!(
            ineligibility(
                &devin.descriptor,
                devin.health.as_ref(),
                AccessMode::Supervised,
                SandboxLevel::Unrestricted
            ),
            Some(rejection.reason.clone())
        );
        // With the access mode it offers, only the health is wrong: one reason, as before.
        let full = RouteRequest {
            access: AccessMode::FullAccess,
            ..named
        };
        let decision = route(&full, &[devin], 1);
        assert!(matches!(
            decision.rejected[0].reason,
            RejectReason::NotInstalled { .. }
        ));
        assert!(decision.rejected[0].also.is_empty());
    }

    #[test]
    fn with_no_rule_the_eligible_executors_are_ranked_by_id() {
        let candidates = [
            candidate("codex", None),
            candidate("claude-code", Some(ok())),
        ];
        let decision = route(&request(), &candidates, 7);
        assert_eq!(decision.rule_matched, RuleMatched::Default);
        assert_eq!(decision.ranked, [id("claude-code"), id("codex")]);
        assert_eq!(decision.chosen, Some(id("claude-code")));
        assert!(decision.rejected.is_empty() && decision.fallbacks.is_empty());
        assert_eq!(decision.at_ms, 7);
    }

    #[test]
    fn a_bots_list_is_the_preference_and_the_whole_candidate_set() {
        let candidates = [
            candidate("claude-code", None),
            candidate("codex", None),
            candidate("gemini", None),
        ];
        let decision = route(
            &RouteRequest {
                bot: bot(&["codex", "claude-code", "ghost"]),
                ..request()
            },
            &candidates,
            1,
        );
        assert_eq!(
            decision.rule_matched,
            RuleMatched::BotPreference {
                bot: "keeper".into()
            }
        );
        assert_eq!(decision.ranked, [id("codex"), id("claude-code")]);
        let rejected: Vec<_> = decision
            .rejected
            .iter()
            .map(|r| (r.executor.as_str(), r.reason.clone()))
            .collect();
        assert_eq!(
            rejected,
            [
                ("ghost", RejectReason::NotRegistered),
                (
                    "gemini",
                    RejectReason::Excluded {
                        why: "bot keeper does not list it".into()
                    }
                ),
            ]
        );
    }

    #[test]
    fn an_explicit_executor_is_the_only_candidate_and_beats_the_bot() {
        let candidates = [candidate("claude-code", None), candidate("codex", None)];
        let decision = route(
            &RouteRequest {
                explicit: Some(id("codex")),
                bot: bot(&["claude-code"]),
                ..request()
            },
            &candidates,
            1,
        );
        assert_eq!(decision.rule_matched, RuleMatched::ExplicitExecutor);
        assert_eq!(decision.ranked, [id("codex")]);
        assert_eq!(
            decision.rejected,
            [Rejection {
                also: Vec::new(),
                executor: id("claude-code"),
                reason: RejectReason::Excluded {
                    why: "the task names codex".into()
                }
            }]
        );
    }

    #[test]
    fn an_explicit_executor_nobody_registered_leaves_nothing_chosen() {
        let decision = route(
            &RouteRequest {
                explicit: Some(id("ghost")),
                ..request()
            },
            &[candidate("codex", None)],
            1,
        );
        assert_eq!(decision.chosen, None);
        assert_eq!(decision.rejected[0].reason, RejectReason::NotRegistered);
    }

    #[test]
    fn the_request_can_exclude_an_executor() {
        let decision = route(
            &RouteRequest {
                exclude: vec![id("claude-code")],
                ..request()
            },
            &[candidate("claude-code", None), candidate("codex", None)],
            1,
        );
        assert_eq!(decision.ranked, [id("codex")]);
        assert_eq!(
            decision.rejected[0].reason,
            RejectReason::Excluded {
                why: "the request excludes it".into()
            }
        );
    }

    #[test]
    fn health_drops_what_is_not_installed_too_old_or_signed_out() {
        let not_installed = health(
            InstallStatus::NotInstalled {
                why: NotInstalledReason::NotFound {
                    hint: "install it".into(),
                },
            },
            VersionGate::Unchecked,
            SignedIn::Unknown,
        );
        let too_old = health(
            InstallStatus::Installed {
                path: "/bin/x".into(),
                version: Some("0.1.0".into()),
            },
            VersionGate::TooOld {
                min: "1.0.0".into(),
            },
            SignedIn::Unknown,
        );
        let signed_out = health(
            InstallStatus::Installed {
                path: "/bin/x".into(),
                version: Some("1.5.0".into()),
            },
            VersionGate::Ok,
            SignedIn::No {
                hint: "run `x login`".into(),
            },
        );
        let candidates = [
            candidate("a-missing", Some(not_installed)),
            candidate("b-old", Some(too_old)),
            candidate("c-out", Some(signed_out)),
            // Unknown sign-in and an untested version stay eligible.
            candidate(
                "d-unknown",
                Some(health(
                    InstallStatus::Installed {
                        path: "/bin/x".into(),
                        version: None,
                    },
                    VersionGate::Untested {
                        verified: "2.0.0".into(),
                    },
                    SignedIn::Unknown,
                )),
            ),
        ];
        let decision = route(&request(), &candidates, 1);
        assert_eq!(decision.ranked, [id("d-unknown")]);
        let reasons: Vec<_> = decision.rejected.iter().map(|r| r.reason.clone()).collect();
        assert_eq!(
            reasons,
            [
                RejectReason::NotInstalled {
                    detail: "install it".into()
                },
                RejectReason::TooOld {
                    found: Some("0.1.0".into()),
                    min: "1.0.0".into()
                },
                RejectReason::SignedOut {
                    hint: "run `x login`".into()
                },
            ]
        );
    }

    #[test]
    fn a_supervised_task_is_never_routed_to_a_full_access_only_executor() {
        let full_only = Candidate {
            descriptor: descriptor(
                "yolo",
                &[AccessMode::FullAccess],
                &[SandboxLevel::Unrestricted],
            ),
            health: None,
            busy: None,
        };
        let decision = route(&request(), std::slice::from_ref(&full_only), 1);
        assert_eq!(decision.chosen, None);
        assert_eq!(
            decision.rejected[0].reason,
            RejectReason::AccessNotOffered {
                requested: AccessMode::Supervised,
                offered: vec![AccessMode::FullAccess]
            }
        );
        // Not even when the task names it.
        let decision = route(
            &RouteRequest {
                explicit: Some(id("yolo")),
                ..request()
            },
            &[full_only],
            1,
        );
        assert_eq!(decision.chosen, None);
    }

    #[test]
    fn a_sandbox_level_the_executor_does_not_enforce_drops_it() {
        let decision = route(
            &RouteRequest {
                sandbox: SandboxLevel::WorkspaceWrite,
                ..request()
            },
            &[candidate("codex", None)],
            1,
        );
        assert_eq!(decision.chosen, None);
        assert_eq!(
            decision.rejected[0].reason,
            RejectReason::SandboxNotEnforced {
                requested: SandboxLevel::WorkspaceWrite,
                offered: vec![SandboxLevel::Unrestricted]
            }
        );
    }

    #[test]
    fn falling_back_moves_to_the_next_candidate_and_records_the_step() {
        let candidates = [candidate("claude-code", None), candidate("codex", None)];
        let mut decision = route(&request(), &candidates, 1);
        let reason = RejectReason::SignedOut {
            hint: "run `claude login`".into(),
        };
        fall_back(
            &mut decision,
            &id("claude-code"),
            &reason,
            "not signed in",
            9,
        );
        assert_eq!(decision.chosen, Some(id("codex")));
        assert_eq!(decision.ranked, [id("codex")]);
        assert_eq!(
            decision.rejected,
            [Rejection {
                also: Vec::new(),
                executor: id("claude-code"),
                reason: RejectReason::StartFailed {
                    failure: StartFailureKind::NotSignedIn,
                    detail: "not signed in".into()
                }
            }]
        );
        assert_eq!(decision.fallbacks.len(), 1);
        assert_eq!(decision.fallbacks[0].to, Some(id("codex")));

        // The walk ends with nothing chosen once the candidates are used up.
        fall_back(&mut decision, &id("codex"), &reason, "x", 10);
        assert_eq!(decision.chosen, None);
        assert_eq!(decision.fallbacks[1].to, None);
    }
}
