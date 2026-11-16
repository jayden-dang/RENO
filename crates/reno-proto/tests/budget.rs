//! Golden JSON of the budget types (T1.7): the typed stop a `Done` carries, `budget.status`,
//! `budget.pause` and `budget.resume`.

#![allow(clippy::unwrap_used)]

use std::fmt::Debug;

use reno_proto::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

fn golden<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T, wire: Value) {
    assert_eq!(serde_json::to_value(value).unwrap(), wire, "serialise");
    let text = wire.to_string();
    let parsed: T = serde_json::from_str(&text).unwrap();
    assert_eq!(&parsed, value, "deserialise");
}

#[test]
fn budget_types_on_the_wire() {
    golden(
        &BudgetStop {
            kind: BudgetKind::WallTime,
            limit: 2_700_000,
            used: 2_700_010,
        },
        json!({"kind": "wall_time", "limit": 2_700_000, "used": 2_700_010}),
    );
    golden(
        &BudgetStop {
            kind: BudgetKind::Tokens,
            limit: 100,
            used: 120,
        },
        json!({"kind": "tokens", "limit": 100, "used": 120}),
    );
    golden(&BudgetPauseParams::default(), json!({}));
    golden(
        &BudgetPauseParams {
            bot: Some("keeper".into()),
            stop_live: true,
        },
        json!({"bot": "keeper", "stopLive": true}),
    );
    golden(
        &BudgetPauseResult {
            changed: true,
            stopped: vec!["run_01HZX9".parse().unwrap()],
        },
        json!({"changed": true, "stopped": ["run_01HZX9"]}),
    );
    golden(&BudgetResumeParams::default(), json!({}));
    golden(
        &BudgetResumeResult { changed: false },
        json!({"changed": false}),
    );
    golden(
        &BudgetStatusResult {
            paused: true,
            day: "2026-11-16".into(),
            day_ends_at_ms: 1_700_000_000_000,
            default_wall_time_ms: 14_400_000,
            bots: vec![
                BotBudgetStatus {
                    bot: "keeper".into(),
                    live_runs: 1,
                    max_concurrent_runs: 2,
                    queued_runs: 3,
                    tokens_today: 120,
                    daily_token_ceiling: Some(1000),
                    max_wall_time_ms: 2_700_000,
                    paused: false,
                },
                BotBudgetStatus {
                    bot: "scribe".into(),
                    live_runs: 0,
                    max_concurrent_runs: 1,
                    queued_runs: 0,
                    tokens_today: 0,
                    daily_token_ceiling: None,
                    max_wall_time_ms: 60_000,
                    paused: true,
                },
            ],
        },
        json!({
            "paused": true, "day": "2026-11-16", "dayEndsAtMs": 1_700_000_000_000_u64,
            "defaultWallTimeMs": 14_400_000,
            "bots": [
                {"bot": "keeper", "liveRuns": 1, "maxConcurrentRuns": 2, "queuedRuns": 3,
                 "tokensToday": 120, "dailyTokenCeiling": 1000, "maxWallTimeMs": 2_700_000,
                 "paused": false},
                {"bot": "scribe", "liveRuns": 0, "maxConcurrentRuns": 1, "queuedRuns": 0,
                 "tokensToday": 0, "maxWallTimeMs": 60_000, "paused": true}
            ]
        }),
    );
}

#[test]
fn the_refusal_payload_on_the_wire() {
    golden(
        &BudgetRefusal {
            bot: "keeper".into(),
            stop: BudgetStop {
                kind: BudgetKind::Tokens,
                limit: 100,
                used: 120,
            },
        },
        json!({"bot": "keeper", "stop": {"kind": "tokens", "limit": 100, "used": 120}}),
    );
}

#[test]
fn a_done_diagnostic_carries_a_budget_stop() {
    let stop = BudgetStop {
        kind: BudgetKind::Tokens,
        limit: 5,
        used: 9,
    };
    let done = ExecEvent::Done {
        stop: StopReason::Interrupted,
        result: None,
        diagnostic: Some(stop.to_diagnostic()),
        reason: None,
    };
    let wire = serde_json::to_value(&done).unwrap();
    let diagnostic = wire["diagnostic"].as_str().unwrap();
    assert_eq!(BudgetStop::from_diagnostic(diagnostic), Some(stop));
}

#[test]
fn the_method_names_and_the_feature() {
    assert_eq!(<BudgetStatus as Method>::NAME, "budget.status");
    assert_eq!(<BudgetPause as Method>::NAME, "budget.pause");
    assert_eq!(<BudgetResume as Method>::NAME, "budget.resume");
    assert!(FEATURES.contains(&FEATURE_BUDGET));
    assert_eq!(ErrorCode::BudgetExceeded.code(), -32009);
}
