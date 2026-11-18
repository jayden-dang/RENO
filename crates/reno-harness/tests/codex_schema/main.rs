//! The Codex driver's wire against the schema the real CLI emits, with no model call.
//!
//! The driver runs against the fake app-server wrapped in `record-wire.sh`, which records every
//! frame both ways. Each request and notification the driver sent, each approval or question
//! reply, and each server frame the fake wrote is validated against the schema of
//! `codex app-server generate-json-schema` (see `schema.rs` for which directory). The captured
//! and hand-written frames under `tests/fixtures/codex` are validated the same way.
//!
//! What this does not cover, and so what stays unverified against the real CLI:
//!
//! - The fake's *responses* to the driver's requests are stubs (`{"thread":{"id":"th-1"}}`), so
//!   response shapes are not checked, only the reply shapes the driver sends.
//! - Frames the driver *reads* (server notifications and requests) are checked for the types,
//!   enums and variants of the fields they carry, not for completeness (the fakes trim them to
//!   what the driver reads) and not for unknown field names: a field Codex renamed that the
//!   driver reads, in a frame the fake writes under the old name, passes. (Closing objects to
//!   unlisted fields was tried and flagged only noise: Codex flattens tagged requests.)
//! - Fields the driver sends under `experimentalApi` and that a plain `generate-json-schema`
//!   leaves out (`EXPERIMENTAL_FIELDS`) are unchecked.
//! - The 0.146.1 live capture predates the snapshot's 0.169: it is checked only for shape.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod schema;

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use reno_harness::CodexExecutor;
use reno_harness::exec::{Decision, ExecControls, Executor, InputAnswer};
use reno_proto::{
    AccessMode, Effort, ExecEvent, ExecSpec, McpServerSpec, RunId, SandboxLevel, SessionRef,
};
use schema::{Schemas, Strictness, Union, schema_dir};
use serde_json::Value;

/// Frames the fakes write on purpose although 0.169 has no such method.
const OFF_SCHEMA: &[(&str, &str)] = &[
    (
        "some/unknownNotification",
        "made up: the driver must tolerate unknown methods",
    ),
    (
        "zz/futureNotification",
        "made up: the unrecognised-frame counter's probe",
    ),
    (
        "thread/somethingBrandNew",
        "made up: the driver must tolerate unknown methods",
    ),
    (
        "brand/newRequest",
        "made up: the driver must refuse unknown requests",
    ),
    (
        "turn/failed",
        "legacy settle method: 0.169 reports failure in turn/completed",
    ),
    (
        "turn/aborted",
        "legacy settle method: 0.169 reports interruption in turn/completed",
    ),
];

/// Request fields the driver sends under `experimentalApi` that the non-experimental schema
/// (`generate-json-schema` without `--experimental`) does not list: unverified here.
const EXPERIMENTAL_FIELDS: &[(&str, &str, &str)] = &[(
    "turn/start",
    "collaborationMode",
    "leaving plan mode on a resumed thread; its shape is not in the 0.169 schema",
)];

/// Scenarios whose server frames deliberately keep an older app-server's shape, because the
/// driver still reads it. Their server frames are not held to 0.169; what the driver sends is.
const LEGACY_SCENARIOS: &[(&str, &str)] = &[
    (
        "scenario:happy",
        "both spellings of a delta, string file-change kinds and a todoList item: the tolerance test",
    ),
    (
        "scenario:decline",
        "the command is an argv array, as older app-servers sent",
    ),
    (
        "scenario:file-change-inline",
        "the changes ride in the approval request itself, as older app-servers sent",
    ),
];

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn make_executable(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// What a recorded run reacts to.
#[derive(Clone, Copy, PartialEq)]
enum React {
    Nothing,
    SteerAfterText,
    CancelAfterText,
}

struct Scenario {
    prompt: &'static str,
    access: AccessMode,
    mcp: Vec<McpServerSpec>,
    resume: Option<&'static str>,
    react: React,
    model: Option<&'static str>,
    effort: Option<Effort>,
    instructions: Option<&'static str>,
}

fn scenario(prompt: &'static str, access: AccessMode) -> Scenario {
    Scenario {
        prompt,
        access,
        mcp: Vec::new(),
        resume: None,
        react: React::Nothing,
        model: None,
        effort: None,
        instructions: None,
    }
}

fn scenarios() -> Vec<Scenario> {
    let mcp = vec![
        McpServerSpec {
            name: "reno".into(),
            command: "/opt/reno/reno".into(),
            args: vec!["mcp".into()],
            env: [("RENO_RUN".to_owned(), "r1".to_owned())].into(),
        },
        McpServerSpec {
            name: "plugin-focus".into(),
            command: "/opt/focus/server".into(),
            args: Vec::new(),
            env: Default::default(),
        },
    ];
    vec![
        scenario("scenario:conf-happy", AccessMode::FullAccess),
        scenario("scenario:conf-unknown", AccessMode::FullAccess),
        scenario("scenario:approve", AccessMode::Supervised),
        scenario("scenario:one-approval", AccessMode::Supervised),
        scenario("scenario:decline", AccessMode::Supervised),
        scenario("scenario:file-change-inline", AccessMode::Supervised),
        scenario("scenario:mcp-approval", AccessMode::Supervised),
        scenario("scenario:permissions", AccessMode::Supervised),
        scenario("scenario:user-input", AccessMode::Supervised),
        scenario("scenario:elicitation-form", AccessMode::Supervised),
        scenario("scenario:server-requests", AccessMode::Supervised),
        scenario("scenario:policy", AccessMode::AutoAcceptEdits),
        Scenario {
            mcp,
            ..scenario("scenario:mcp-config", AccessMode::Supervised)
        },
        Scenario {
            resume: Some("th-conf"),
            ..scenario("scenario:resumed", AccessMode::FullAccess)
        },
        // The launch options: model, effort and instructions ride on thread/start and turn/start.
        // `happy` checks them on the wire too.
        Scenario {
            model: Some("gpt-5.6-sol"),
            effort: Some(Effort::High),
            ..scenario("scenario:happy", AccessMode::FullAccess)
        },
        Scenario {
            model: Some("gpt-5.6-terra"),
            effort: Some(Effort::Low),
            instructions: Some("Answer briefly."),
            ..scenario("scenario:conf-happy", AccessMode::Supervised)
        },
        Scenario {
            effort: Some(Effort::Medium),
            instructions: Some("Answer briefly."),
            ..scenario("scenario:conf-happy", AccessMode::AutoAcceptEdits)
        },
        // The resume paths: a fresh start after a lost rollout, an archived thread, a thread
        // left in plan mode.
        Scenario {
            resume: Some("resume-fail"),
            ..scenario("scenario:resumed", AccessMode::FullAccess)
        },
        Scenario {
            resume: Some("resume-archived"),
            ..scenario("scenario:resumed", AccessMode::FullAccess)
        },
        Scenario {
            resume: Some("resume-in-plan"),
            ..scenario("scenario:leave-plan", AccessMode::FullAccess)
        },
        Scenario {
            react: React::SteerAfterText,
            ..scenario("scenario:steer", AccessMode::FullAccess)
        },
        Scenario {
            react: React::CancelAfterText,
            ..scenario("scenario:interrupt", AccessMode::FullAccess)
        },
    ]
}

/// Both directions of one recorded run.
struct Recorded {
    sent: Vec<Value>,
    received: Vec<Value>,
}

fn read_frames(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        // Plain-text output of a `--version` probe goes through the recorder too.
        .filter(|l| l.trim_start().starts_with('{'))
        .map(|l| serde_json::from_str(l).unwrap_or_else(|e| panic!("{path:?}: {l}: {e}")))
        .collect()
}

/// An executor whose app-server is the fake, behind the recorder, logging into `log`.
fn recording_executor(dir: &Path) -> (CodexExecutor, PathBuf) {
    let log = dir.join("wire");
    std::fs::create_dir_all(&log).unwrap();
    let fake = fixtures().join("fake-codex.sh");
    let recorder = fixtures().join("record-wire.sh");
    make_executable(&fake);
    make_executable(&recorder);
    let launcher = dir.join("codex");
    std::fs::write(
        &launcher,
        format!(
            "#!/bin/sh\nRENO_WIRE_TARGET='{}' RENO_WIRE_LOG='{}' exec '{}' \"$@\"\n",
            fake.display(),
            log.display(),
            recorder.display()
        ),
    )
    .unwrap();
    make_executable(&launcher);
    (CodexExecutor::new().with_executable(&launcher), log)
}

async fn record(s: &Scenario) -> Recorded {
    let dir = tempfile::tempdir().unwrap();
    let (executor, log) = recording_executor(dir.path());
    let spec = ExecSpec {
        model_options: Default::default(),
        run_id: RunId::from_suffix("01HZXD").unwrap(),
        prompt: s.prompt.into(),
        cwd: PathBuf::from("/tmp"),
        model: s.model.map(Into::into),
        effort: s.effort,
        access: s.access,
        sandbox: SandboxLevel::Unrestricted,
        resume: s.resume.map(|r| SessionRef(r.into())),
        mcp: s.mcp.clone(),
        env: Default::default(),
        output_schema: None,
        instructions: s.instructions.map(Into::into),
        inherit_user_config: false,
    };
    let (controls, handle) = ExecControls::new(8);
    let mut stream = executor.start(spec, controls).await.unwrap();
    let mut reacted = false;
    let run = async {
        while let Some(event) = stream.next().await {
            match event {
                ExecEvent::NeedsApproval { request_id, .. } => {
                    handle.decide(request_id, Decision::Allow).await.unwrap();
                }
                ExecEvent::NeedsInput {
                    request_id,
                    questions,
                } => {
                    let answers = questions
                        .iter()
                        .map(|q| InputAnswer {
                            question_id: q.id.clone(),
                            labels: vec![q.options.last().cloned().unwrap_or_else(|| "A".into())],
                        })
                        .collect();
                    handle.answer_input(request_id, answers).await.unwrap();
                }
                ExecEvent::Text { .. } if !reacted => {
                    reacted = true;
                    match s.react {
                        React::SteerAfterText => handle.steer("redirect please").await.unwrap(),
                        React::CancelAfterText => handle.cancel(),
                        React::Nothing => {}
                    }
                }
                _ => {}
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .unwrap_or_else(|_| panic!("{} did not finish", s.prompt));
    // The probe too: `account/read` and `model/list` are requests the driver makes.
    let _ = executor.probe().await;
    executor.shutdown().await;
    Recorded {
        sent: read_frames(&log.join("in.jsonl")),
        received: read_frames(&log.join("out.jsonl")),
    }
}

fn method(frame: &Value) -> Option<&str> {
    frame.get("method").and_then(Value::as_str)
}

fn is_off_schema(method: &str) -> bool {
    OFF_SCHEMA.iter().any(|(m, _)| *m == method)
}

/// Every problem in one recorded run.
fn check(schemas: &mut Schemas, label: &str, run: &Recorded) -> Vec<String> {
    let mut problems = Vec::new();
    let mut note = |what: String, why: Vec<String>| {
        for why in why {
            problems.push(format!("{label}: {what}: {why}"));
        }
    };
    for frame in &run.sent {
        match (method(frame), frame.get("id")) {
            (Some(m), Some(_)) => {
                let mut why = schemas.check(Union::ClientRequest, m, frame);
                // An experimental-API field is absent from the schema a plain generation emits, so
                // it cannot be checked; it is sent only because the driver opts in at `initialize`.
                why.retain(|w| {
                    !EXPERIMENTAL_FIELDS.iter().any(|(method, field, _)| {
                        *method == m && w.starts_with(&format!("params.{field} is not a field of"))
                    })
                });
                note(format!("request {m}"), why);
            }
            (Some(m), None) => {
                note(
                    format!("notification {m}"),
                    schemas.check(Union::ClientNotification, m, frame),
                );
            }
            (None, Some(id)) => {
                // A reply to a server request: its schema follows the request's method.
                let asked = run
                    .received
                    .iter()
                    .find(|r| r.get("id") == Some(id) && method(r).is_some())
                    .and_then(method);
                match (asked, frame.get("result")) {
                    (Some(m), Some(result)) => {
                        note(format!("reply to {m}"), schemas.check_response(m, result));
                    }
                    (Some(m), None) => {
                        // An error reply is how a request the driver refuses is answered.
                        assert!(
                            frame.get("error").is_some() && is_off_schema(m),
                            "{label}: reply to {m} has neither result nor an expected error: {frame}"
                        );
                    }
                    (None, _) => note("reply".into(), vec![format!("answers no request: {frame}")]),
                }
            }
            (None, None) => note("frame".into(), vec![format!("is not JSON-RPC: {frame}")]),
        }
    }
    for frame in &run.received {
        let Some(m) = method(frame) else { continue };
        if is_off_schema(m) {
            continue;
        }
        let union = if frame.get("id").is_some() {
            Union::ServerRequest
        } else {
            Union::ServerNotification
        };
        if LEGACY_SCENARIOS.iter().any(|(name, _)| *name == label) {
            continue;
        }
        note(
            format!("server {m}"),
            schemas.check_as(union, m, frame, Strictness::Shape),
        );
    }
    problems
}

#[tokio::test]
async fn what_the_driver_sends_and_the_fake_serves_matches_the_codex_schema() {
    let mut schemas = Schemas::open(&schema_dir());
    let mut problems = Vec::new();
    let mut sent_methods = std::collections::BTreeSet::new();
    for s in scenarios() {
        let run = record(&s).await;
        assert!(
            !run.sent.is_empty() && !run.received.is_empty(),
            "{}",
            s.prompt
        );
        sent_methods.extend(run.sent.iter().filter_map(method).map(str::to_owned));
        problems.extend(check(&mut schemas, s.prompt, &run));
    }
    // Every request the driver can make was made, and so checked. A new one (a fork, a
    // rollback) fails here until a scenario makes it.
    assert_eq!(
        sent_methods.into_iter().collect::<Vec<_>>(),
        [
            "account/read",
            "initialize",
            "initialized",
            "model/list",
            "thread/resume",
            "thread/start",
            "thread/unarchive",
            "turn/interrupt",
            "turn/start",
            "turn/steer",
        ]
    );
    problems.sort();
    problems.dedup();
    assert!(
        problems.is_empty(),
        "{} wire problems against {}:\n{}",
        problems.len(),
        schema_dir().display(),
        problems.join("\n")
    );
}

/// The captured and hand-written frames the driver parses, as the app-server would send them.
#[test]
fn the_fixture_frames_match_the_codex_schema() {
    let mut schemas = Schemas::open(&schema_dir());
    let mut problems = Vec::new();
    let mut frames = 0;
    for entry in std::fs::read_dir(fixtures().join("codex")).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let run = Recorded {
            sent: Vec::new(),
            received: read_frames(&path),
        };
        frames += run.received.len();
        problems.extend(check(&mut schemas, &name, &run));
    }
    assert!(frames > 0);
    problems.sort();
    problems.dedup();
    assert!(
        problems.is_empty(),
        "{} fixture problems against {}:\n{}",
        problems.len(),
        schema_dir().display(),
        problems.join("\n")
    );
}

/// The snapshot says which CLI it came from, and that is the version the driver claims to have
/// verified: refreshing one without the other fails here.
#[test]
fn the_snapshot_is_the_version_the_driver_declares_verified() {
    let verified = CodexExecutor::new().descriptor().cli_verified.verified;
    let snapshot = Schemas::open(&schema::snapshot_dir());
    assert_eq!(
        snapshot.recorded_version().as_deref(),
        Some(verified.as_str())
    );
}

/// The checker itself: a request with a missing required field, a wrong enum value and a field
/// the schema does not list must each be reported, or a green run proves nothing.
#[test]
fn the_checker_catches_a_wrong_request() {
    let mut schemas = Schemas::open(&schema::snapshot_dir());
    let bad = serde_json::json!({
        "id": 1,
        "method": "turn/start",
        "params": { "threadId": "t", "input": [], "approvalPolicy": "sometimes", "bogus": 1 }
    });
    let problems = schemas.check(Union::ClientRequest, "turn/start", &bad);
    let joined = problems.join("\n");
    assert!(joined.contains("approvalPolicy"), "{joined}");
    assert!(joined.contains("params.bogus"), "{joined}");
    let missing = serde_json::json!({"id": 1, "method": "turn/start", "params": {}});
    assert!(
        !schemas
            .check(Union::ClientRequest, "turn/start", &missing)
            .is_empty()
    );
    assert!(
        schemas
            .check(Union::ClientRequest, "no/such/method", &bad)
            .join("")
            .contains("not a ClientRequest.json method")
    );
}
