//! Golden JSON of the routing types (T1.5): additive wire shapes.

#![allow(clippy::unwrap_used)]

use std::fmt::Debug;

use reno_proto::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

fn golden<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T, wire: Value) {
    assert_eq!(serde_json::to_value(value).unwrap(), wire, "serialise");
    let parsed: T = serde_json::from_str(&wire.to_string()).unwrap();
    assert_eq!(&parsed, value, "deserialise");
}

#[test]
fn a_routing_warning_is_on_the_wire_and_absent_when_there_is_none() {
    let mut decision: RoutingDecision = serde_json::from_value(json!({
        "chosen": "claude-code", "ranked": ["claude-code"], "rejected": [],
        "ruleMatched": {"type": "default"}, "atMs": 1
    }))
    .unwrap();
    assert!(decision.warnings.is_empty());
    assert!(
        serde_json::to_value(&decision)
            .unwrap()
            .get("warnings")
            .is_none()
    );
    decision.warnings = vec!["model \"x\" is not in claude-code's model list".into()];
    assert_eq!(
        serde_json::to_value(&decision).unwrap()["warnings"],
        json!(["model \"x\" is not in claude-code's model list"])
    );
}

#[test]
fn task_run_in_place_is_opt_in_and_omitted_when_off() {
    golden(
        &TaskRunParams {
            thread_id: None,
            model_options: Default::default(),
            effort: None,
            prompt: "go".into(),
            executor: None,
            bot: None,
            cwd: None,
            model: None,
            in_place: true,
            attachments: Vec::new(),
        },
        json!({"prompt": "go", "inPlace": true}),
    );
}

#[test]
fn routing_decision_wire_shape() {
    let claude: ExecutorId = "claude-code".parse().unwrap();
    let codex: ExecutorId = "codex".parse().unwrap();
    let decision = RoutingDecision {
        chosen: Some(codex.clone()),
        ranked: vec![codex.clone()],
        rejected: vec![Rejection {
            also: Vec::new(),
            executor: claude.clone(),
            reason: RejectReason::StartFailed {
                failure: StartFailureKind::NotSignedIn,
                detail: "run `claude login`".into(),
            },
        }],
        rule_matched: RuleMatched::BotPreference {
            bot: "keeper".into(),
        },
        fallbacks: vec![FallbackStep {
            from: claude,
            reason: RejectReason::SignedOut {
                hint: "run `claude login`".into(),
            },
            to: Some(codex),
            at_ms: 11,
        }],
        at_ms: 10,
        warnings: Vec::new(),
    };
    golden(
        &decision,
        json!({
            "chosen": "codex",
            "ranked": ["codex"],
            "rejected": [{"executor": "claude-code",
                          "reason": {"type": "start_failed", "failure": "not_signed_in",
                                     "detail": "run `claude login`"}}],
            "ruleMatched": {"type": "bot_preference", "bot": "keeper"},
            "fallbacks": [{"from": "claude-code",
                           "reason": {"type": "signed_out", "hint": "run `claude login`"},
                           "to": "codex", "atMs": 11}],
            "atMs": 10
        }),
    );
    // No eligible executor: `chosen` and the empty walk are omitted.
    golden(
        &RoutingDecision {
            chosen: None,
            ranked: vec![],
            rejected: vec![],
            rule_matched: RuleMatched::Default,
            fallbacks: vec![],
            warnings: Vec::new(),
            at_ms: 1,
        },
        json!({"ranked": [], "rejected": [], "ruleMatched": {"type": "default"}, "atMs": 1}),
    );
}

#[test]
fn every_reject_reason_has_a_wire_name() {
    let cases = [
        (
            RejectReason::NotInstalled { detail: "x".into() },
            json!({"type": "not_installed", "detail": "x"}),
        ),
        (
            RejectReason::TooOld {
                found: Some("1.0.0".into()),
                min: "2.0.0".into(),
            },
            json!({"type": "too_old", "found": "1.0.0", "min": "2.0.0"}),
        ),
        (
            RejectReason::AccessNotOffered {
                requested: AccessMode::Supervised,
                offered: vec![AccessMode::FullAccess],
            },
            json!({"type": "access_not_offered", "requested": "supervised",
                   "offered": ["full_access"]}),
        ),
        (
            RejectReason::SandboxNotEnforced {
                requested: SandboxLevel::ReadOnly,
                offered: vec![SandboxLevel::Unrestricted],
            },
            json!({"type": "sandbox_not_enforced", "requested": "read_only",
                   "offered": ["unrestricted"]}),
        ),
        (
            RejectReason::Excluded { why: "w".into() },
            json!({"type": "excluded", "why": "w"}),
        ),
        (
            RejectReason::NotRegistered,
            json!({"type": "not_registered"}),
        ),
    ];
    for (reason, wire) in cases {
        golden(&reason, wire);
    }
}

#[test]
fn a_rejection_carries_its_other_reasons_only_when_it_has_some() {
    let devin: ExecutorId = "acp.devin".parse().unwrap();
    let access = RejectReason::AccessNotOffered {
        requested: AccessMode::Supervised,
        offered: vec![AccessMode::FullAccess],
    };
    // An older daemon sends none: the field is absent and reads as empty.
    let old: Rejection = serde_json::from_value(json!({
        "executor": "acp.devin",
        "reason": {"type": "access_not_offered", "requested": "supervised",
                   "offered": ["full_access"]}
    }))
    .unwrap();
    assert_eq!(old.also, Vec::new());
    assert_eq!(
        serde_json::to_value(&old).unwrap().get("also"),
        None,
        "nothing extra goes to an older client"
    );
    golden(
        &Rejection {
            executor: devin,
            reason: access,
            also: vec![RejectReason::NotInstalled {
                detail: "install it".into(),
            }],
        },
        json!({"executor": "acp.devin",
               "reason": {"type": "access_not_offered", "requested": "supervised",
                          "offered": ["full_access"]},
               "also": [{"type": "not_installed", "detail": "install it"}]}),
    );
}

#[test]
fn access_modes_and_sandbox_levels_read_in_words() {
    assert_eq!(AccessMode::FullAccess.to_string(), "Full access");
    assert_eq!(AccessMode::Supervised.to_string(), "Supervised");
    assert_eq!(AccessMode::AutoAcceptEdits.to_string(), "Auto-accept edits");
    assert_eq!(SandboxLevel::ReadOnly.to_string(), "Read-only");
    assert_eq!(SandboxLevel::WorkspaceWrite.to_string(), "Workspace write");
    assert_eq!(SandboxLevel::Unrestricted.to_string(), "Unrestricted");
    let access = RejectReason::AccessNotOffered {
        requested: AccessMode::Supervised,
        offered: vec![AccessMode::FullAccess, AccessMode::AutoAcceptEdits],
    };
    assert_eq!(
        access.to_string(),
        "does not offer access mode Supervised (offers Full access, Auto-accept edits)"
    );
    let sandbox = RejectReason::SandboxNotEnforced {
        requested: SandboxLevel::WorkspaceWrite,
        offered: vec![SandboxLevel::Unrestricted],
    };
    assert_eq!(
        sandbox.to_string(),
        "does not enforce sandbox Workspace write (enforces Unrestricted)"
    );
    let none = RejectReason::AccessNotOffered {
        requested: AccessMode::Supervised,
        offered: vec![],
    };
    assert_eq!(
        none.to_string(),
        "does not offer access mode Supervised (offers nothing)"
    );
    let failed = RejectReason::StartFailed {
        failure: StartFailureKind::NoEligibleExecutor,
        detail: "x".into(),
    };
    assert_eq!(
        failed.to_string(),
        "would not start (no eligible executor): x"
    );
}

#[test]
fn the_explanation_names_every_reason_an_executor_was_dropped_for() {
    let decision: RoutingDecision = serde_json::from_value(json!({
        "ranked": [],
        "rejected": [{"executor": "acp.devin",
            "reason": {"type": "access_not_offered", "requested": "supervised",
                       "offered": ["full_access"]},
            "also": [{"type": "not_installed", "detail": "install it"}]}],
        "ruleMatched": {"type": "default"}, "atMs": 1
    }))
    .unwrap();
    assert_eq!(
        decision.explain_verbose(),
        "no eligible executor (default order); not acp.devin: does not offer access mode \
         Supervised (offers Full access), and not installed (install it)"
    );
}

#[test]
fn routing_methods() {
    assert_eq!(<RoutePreview as Method>::NAME, "route.preview");
    assert_eq!(<RunRouting as Method>::NAME, "run.routing");
    assert!(FEATURES.contains(&FEATURE_ROUTING));
}
