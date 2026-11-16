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
fn task_run_in_place_is_opt_in_and_omitted_when_off() {
    golden(
        &TaskRunParams {
            prompt: "go".into(),
            executor: None,
            bot: None,
            cwd: None,
            model: None,
            in_place: true,
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
fn routing_methods() {
    assert_eq!(<RoutePreview as Method>::NAME, "route.preview");
    assert_eq!(<RunRouting as Method>::NAME, "run.routing");
    assert!(FEATURES.contains(&FEATURE_ROUTING));
}
