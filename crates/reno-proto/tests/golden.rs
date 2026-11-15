//! Golden JSON: literal wire text must parse to the value and the value must serialise back to
//! the same JSON.

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

fn run_id() -> RunId {
    "run_01HZX9".parse().unwrap()
}

#[test]
fn exec_event_every_variant() {
    let cases: Vec<(ExecEvent, Value)> = vec![
        (
            ExecEvent::Started {
                session: SessionRef("sess-1".into()),
                model: "opus".into(),
            },
            json!({"type": "started", "session": "sess-1", "model": "opus"}),
        ),
        (
            ExecEvent::Text { text: "hi".into() },
            json!({"type": "text", "text": "hi"}),
        ),
        (
            ExecEvent::Reasoning { text: "hm".into() },
            json!({"type": "reasoning", "text": "hm"}),
        ),
        (
            ExecEvent::Tool {
                id: "t1".into(),
                call: ToolCall {
                    name: "bash".into(),
                    input: json!({"cmd": "ls"}),
                },
            },
            json!({"type": "tool", "id": "t1", "call": {"name": "bash", "input": {"cmd": "ls"}}}),
        ),
        (
            ExecEvent::ToolResult {
                id: "t1".into(),
                ok: true,
                summary: Some("2 files".into()),
            },
            json!({"type": "tool_result", "id": "t1", "ok": true, "summary": "2 files"}),
        ),
        (
            ExecEvent::ToolResult {
                id: "t2".into(),
                ok: false,
                summary: None,
            },
            json!({"type": "tool_result", "id": "t2", "ok": false}),
        ),
        (
            ExecEvent::Usage {
                input_tokens: 10,
                output_tokens: 20,
            },
            json!({"type": "usage", "inputTokens": 10, "outputTokens": 20}),
        ),
        (
            ExecEvent::NeedsInput {
                request_id: "q1".into(),
                questions: vec![Question {
                    id: "a".into(),
                    prompt: "Which?".into(),
                    options: vec!["x".into(), "y".into()],
                    multi_select: false,
                }],
            },
            json!({"type": "needs_input", "requestId": "q1", "questions": [
                {"id": "a", "prompt": "Which?", "options": ["x", "y"], "multiSelect": false}
            ]}),
        ),
        (
            ExecEvent::NeedsApproval {
                request_id: "r1".into(),
                subject: ApprovalSubject {
                    action: ActionClass::Command,
                    tool: "bash".into(),
                    command: Some("rm -rf target".into()),
                    paths: vec!["/work/target".into()],
                    raw: json!({"k": 1}),
                },
            },
            json!({"type": "needs_approval", "requestId": "r1", "subject": {
                "action": "command", "tool": "bash", "command": "rm -rf target",
                "paths": ["/work/target"], "raw": {"k": 1}
            }}),
        ),
        (
            ExecEvent::Done {
                stop: StopReason::MaxTokens,
                result: Some("partial".into()),
                diagnostic: None,
            },
            json!({"type": "done", "stop": "max_tokens", "result": "partial"}),
        ),
        (
            ExecEvent::Done {
                stop: StopReason::Error,
                result: None,
                diagnostic: Some("boom".into()),
            },
            json!({"type": "done", "stop": "error", "diagnostic": "boom"}),
        ),
    ];
    for (event, wire) in &cases {
        golden(event, wire.clone());
    }
}

#[test]
fn exec_spec() {
    let full = ExecSpec {
        run_id: run_id(),
        prompt: "fix it".into(),
        cwd: "/work/tree".into(),
        model: Some("opus".into()),
        effort: Some(Effort::High),
        access: AccessMode::AutoAcceptEdits,
        sandbox: SandboxLevel::WorkspaceWrite,
        resume: Some(SessionRef("sess-1".into())),
        mcp: vec![McpServerSpec {
            name: "reno".into(),
            command: "reno".into(),
            args: vec!["mcp".into()],
            env: [("K".to_owned(), "V".to_owned())].into(),
        }],
        env: [("PATH".to_owned(), "/usr/bin".to_owned())].into(),
        output_schema: Some(json!({"type": "object"})),
    };
    golden(
        &full,
        json!({
            "runId": "run_01HZX9", "prompt": "fix it", "cwd": "/work/tree", "model": "opus",
            "effort": "high", "access": "auto_accept_edits", "sandbox": "workspace_write",
            "resume": "sess-1",
            "mcp": [{"name": "reno", "command": "reno", "args": ["mcp"], "env": {"K": "V"}}],
            "env": {"PATH": "/usr/bin"}, "outputSchema": {"type": "object"}
        }),
    );

    let minimal = ExecSpec {
        run_id: run_id(),
        prompt: "p".into(),
        cwd: "/w".into(),
        model: None,
        effort: None,
        access: AccessMode::Supervised,
        sandbox: SandboxLevel::ReadOnly,
        resume: None,
        mcp: vec![],
        env: Default::default(),
        output_schema: None,
    };
    golden(
        &minimal,
        json!({"runId": "run_01HZX9", "prompt": "p", "cwd": "/w", "access": "supervised",
               "sandbox": "read_only", "mcp": [], "env": {}}),
    );
    // Additive evolution: optional and collection fields may be missing on input.
    let sparse: ExecSpec = serde_json::from_value(json!({
        "runId": "run_01HZX9", "prompt": "p", "cwd": "/w",
        "access": "supervised", "sandbox": "read_only"
    }))
    .unwrap();
    assert_eq!(sparse, minimal);
}

#[test]
fn error_object() {
    golden(
        &ErrorObject::new(ErrorCode::StaleFence, "stale fence"),
        json!({"code": -32003, "message": "stale fence"}),
    );
    golden(
        &ErrorObject {
            code: ErrorCode::NotFound,
            message: "no such run".into(),
            data: Some(json!({"runId": "run_01HZX9"})),
        },
        json!({"code": -32001, "message": "no such run", "data": {"runId": "run_01HZX9"}}),
    );
    golden(
        &ErrorObject::new(ErrorCode::Other(-1), "future"),
        json!({"code": -1, "message": "future"}),
    );
}

#[test]
fn daemon_hello() {
    golden(
        &HelloParams {
            client_name: "reno-cli".into(),
            protocol_version: PROTOCOL_VERSION,
            features: vec![FEATURE_RUNS_WATCH.into()],
        },
        json!({"clientName": "reno-cli", "protocolVersion": 1, "features": ["runs.watch"]}),
    );
    golden(
        &HelloResult {
            server_version: "0.0.0".into(),
            protocol_version: PROTOCOL_VERSION,
            features: FEATURES.iter().map(|f| (*f).to_owned()).collect(),
            pid: 4242,
        },
        json!({"serverVersion": "0.0.0", "protocolVersion": 1,
               "features": ["run.events.resume", "runs.watch", "sub.gap"], "pid": 4242}),
    );
}

#[test]
fn subscription_and_gap_payloads() {
    golden(
        &RunEventsParams {
            run_id: run_id(),
            from_seq: Some(Seq(5)),
        },
        json!({"runId": "run_01HZX9", "fromSeq": 5}),
    );
    golden(
        &RunEventItem {
            seq: Seq(6),
            event: ExecEvent::Text { text: "x".into() },
        },
        json!({"seq": 6, "event": {"type": "text", "text": "x"}}),
    );
    golden(
        &SubGapParams {
            subscription: "sub_a1".parse().unwrap(),
            resume_from: Seq(41),
        },
        json!({"subscription": "sub_a1", "resumeFrom": 41}),
    );
}
