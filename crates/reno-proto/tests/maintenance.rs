//! The executor maintenance contract: wire shapes, compatibility with older payloads, the feature
//! and the new error codes.

#![allow(clippy::expect_used)]

use reno_proto::{
    AutoUpdate, ErrorCode, ExecutorInfo, ExecutorJob, ExecutorJobStartParams,
    FEATURE_EXECUTOR_MAINTAIN, FEATURES, JobFailure, JobKind, JobPhase, Maintenance, PersonStep,
    SetupRecipe, SetupStep, StepRunner, UpdaterInfo,
};
use serde_json::json;

fn maintenance() -> Maintenance {
    Maintenance {
        setup: Some(SetupRecipe {
            source: "Homebrew cask devin-cli".into(),
            steps: vec![SetupStep {
                label: "Install".into(),
                command: "brew install --cask devin-cli".into(),
                runner: StepRunner::Reno,
            }],
        }),
        sign_in: Some(PersonStep {
            label: "Sign in".into(),
            command: "devin auth login".into(),
        }),
        updater: Some(UpdaterInfo {
            command: "codex update".into(),
            self_updating: false,
            can_pin: false,
            latest: Some("0.171.0".into()),
            checked_at_ms: None,
        }),
        managed: false,
        auto_update: AutoUpdate {
            on: true,
            verified_only: false,
        },
    }
}

#[test]
fn the_feature_is_advertised() {
    assert_eq!(FEATURE_EXECUTOR_MAINTAIN, "executor.maintain");
    assert!(FEATURES.contains(&FEATURE_EXECUTOR_MAINTAIN));
}

#[test]
fn an_older_executor_payload_still_deserializes() {
    let old = json!({
        "descriptor": {
            "id": "mock",
            "contractVersion": 1,
            "adapterVersion": "x",
            "cliVerified": {"min": "0", "verified": "0"},
            "caps": {
                "steering": "none", "resume": false, "accessModes": [], "sandboxLevels": [],
                "mcpInjection": false, "structuredOutput": false,
                "deterministicTurnEnd": false, "planMode": false, "models": [],
                "modelSwitch": false, "isolatesUserConfig": false, "modelAliases": []
            }
        }
    });
    let info: ExecutorInfo = serde_json::from_value(old).expect("old payload");
    assert!(info.maintenance.is_none() && info.job.is_none());
    // And absent fields are not written back.
    let back = serde_json::to_value(&info).expect("serialize");
    assert!(back.get("maintenance").is_none() && back.get("job").is_none());
}

#[test]
fn maintenance_and_job_use_the_documented_wire_shapes() {
    let wire = serde_json::to_value(maintenance()).expect("serialize");
    assert_eq!(wire["signIn"]["command"], "devin auth login");
    assert_eq!(wire["setup"]["steps"][0]["runner"], "reno");
    assert_eq!(wire["updater"]["selfUpdating"], false);
    assert_eq!(wire["updater"]["canPin"], false);
    assert_eq!(
        wire["autoUpdate"],
        json!({"on": true, "verifiedOnly": false})
    );
    assert!(wire["updater"].get("checkedAtMs").is_none());
    let back: Maintenance = serde_json::from_value(wire).expect("round trip");
    assert_eq!(back, maintenance());

    let running = ExecutorJob {
        id: "j1".into(),
        kind: JobKind::Update,
        phase: JobPhase::Running {
            step: 1,
            steps: 2,
            last_line: Some("fetching".into()),
            quiet_since_ms: None,
        },
        started_at_ms: 5,
    };
    assert_eq!(
        serde_json::to_value(&running).expect("serialize"),
        json!({"id": "j1", "kind": "update", "startedAtMs": 5, "phase": {
            "type": "running", "step": 1, "steps": 2, "lastLine": "fetching"}})
    );
    let failed = JobPhase::Failed {
        failure: JobFailure::NeedsPassword,
        detail: "sudo".into(),
        finished_at_ms: 9,
    };
    assert_eq!(
        serde_json::to_value(&failed).expect("serialize"),
        json!({"type": "failed", "failure": "needs_password", "detail": "sudo",
               "finishedAtMs": 9})
    );
    assert_eq!(
        serde_json::to_value(JobPhase::Queued { waiting_runs: 1 }).expect("serialize"),
        json!({"type": "queued", "waitingRuns": 1})
    );
}

#[test]
fn start_params_default_force_off() {
    let p: ExecutorJobStartParams =
        serde_json::from_value(json!({"id": "codex", "kind": "update"})).expect("params");
    assert!(!p.force);
    let wire = serde_json::to_value(&p).expect("serialize");
    assert!(wire.get("force").is_none());
}

#[test]
fn the_new_error_codes_are_stable() {
    for (code, n) in [
        (ErrorCode::JobRunning, -32010),
        (ErrorCode::NothingToDo, -32011),
        (ErrorCode::NoUpdater, -32012),
    ] {
        assert_eq!(code.code(), n);
        assert_eq!(ErrorCode::from_code(n), code);
    }
}
