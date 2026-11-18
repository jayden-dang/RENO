//! Golden JSON: literal wire text must parse to the value and the value must serialise back to
//! the same JSON.

#![allow(clippy::unwrap_used)]

use std::fmt::Debug;

use reno_proto::defs::*;
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
                call: ToolCall::new("bash", json!({"cmd": "ls"}), ActionClass::Command),
            },
            json!({"type": "tool", "id": "t1", "call": {"name": "bash", "input": {"cmd": "ls"}, "class": "command"}}),
        ),
        (
            ExecEvent::ToolResult {
                id: "t1".into(),
                ok: true,
                summary: Some("2 files".into()),
                output: None,
            },
            json!({"type": "tool_result", "id": "t1", "ok": true, "summary": "2 files"}),
        ),
        (
            ExecEvent::ToolResult {
                id: "t2".into(),
                ok: false,
                summary: None,
                output: None,
            },
            json!({"type": "tool_result", "id": "t2", "ok": false}),
        ),
        (
            ExecEvent::Usage {
                input_tokens: 10,
                output_tokens: 20,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            json!({"type": "usage", "inputTokens": 10, "outputTokens": 20}),
        ),
        (
            ExecEvent::Usage {
                input_tokens: 16,
                output_tokens: 9120,
                cache_read_tokens: 1_270_554,
                cache_write_tokens: 48_213,
            },
            json!({"type": "usage", "inputTokens": 16, "outputTokens": 9120,
                   "cacheReadTokens": 1_270_554, "cacheWriteTokens": 48_213}),
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
            ExecEvent::AgentReport {
                status: reno_proto::ReportStatus::Blocked,
                summary: "need a token".into(),
                details: None,
            },
            json!({"type": "agent_report", "status": "blocked", "summary": "need a token"}),
        ),
        (
            ExecEvent::UserMessage {
                text: "use the other branch".into(),
            },
            json!({"type": "user_message", "text": "use the other branch"}),
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
            ExecEvent::ApprovalResolved {
                request_id: "r1".into(),
                decision: ApprovalOutcome::Approved,
                by: ResolvedBy::Rule {
                    name: "cargo in the worktree".into(),
                },
                message: None,
            },
            json!({"type": "approval_resolved", "requestId": "r1", "decision": "approved",
                   "by": {"type": "rule", "name": "cargo in the worktree"}}),
        ),
        (
            ExecEvent::ApprovalResolved {
                request_id: "r2".into(),
                decision: ApprovalOutcome::Denied,
                by: ResolvedBy::Rule {
                    name: "force push".into(),
                },
                message: Some("denied by policy: force push".into()),
            },
            json!({"type": "approval_resolved", "requestId": "r2", "decision": "denied",
                   "by": {"type": "rule", "name": "force push"},
                   "message": "denied by policy: force push"}),
        ),
        (
            ExecEvent::ApprovalResolved {
                request_id: "r3".into(),
                decision: ApprovalOutcome::Approved,
                by: ResolvedBy::Human,
                message: None,
            },
            json!({"type": "approval_resolved", "requestId": "r3", "decision": "approved",
                   "by": {"type": "human"}}),
        ),
        (
            ExecEvent::ApprovalResolved {
                request_id: "r4".into(),
                decision: ApprovalOutcome::Denied,
                by: ResolvedBy::Expired,
                message: None,
            },
            json!({"type": "approval_resolved", "requestId": "r4", "decision": "denied",
                   "by": {"type": "expired"}}),
        ),
        (
            ExecEvent::ApprovalResolved {
                request_id: "r5".into(),
                decision: ApprovalOutcome::Denied,
                by: ResolvedBy::RepeatDenial,
                message: None,
            },
            json!({"type": "approval_resolved", "requestId": "r5", "decision": "denied",
                   "by": {"type": "repeat_denial"}}),
        ),
        (
            ExecEvent::Done {
                stop: StopReason::MaxTokens,
                result: Some("partial".into()),
                diagnostic: None,
                reason: None,
            },
            json!({"type": "done", "stop": "max_tokens", "result": "partial"}),
        ),
        (
            ExecEvent::Done {
                stop: StopReason::Error,
                result: None,
                diagnostic: Some("boom".into()),
                reason: None,
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
        model_options: Default::default(),
        run_id: run_id(),
        inherit_user_config: true,
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
        instructions: Some("be brief".into()),
    };
    golden(
        &full,
        json!({
            "runId": "run_01HZX9", "prompt": "fix it", "cwd": "/work/tree", "model": "opus", "instructions": "be brief",
            "effort": "high", "access": "auto_accept_edits", "sandbox": "workspace_write",
            "resume": "sess-1",
            "mcp": [{"name": "reno", "command": "reno", "args": ["mcp"], "env": {"K": "V"}}],
            "env": {"PATH": "/usr/bin"}, "outputSchema": {"type": "object"},
            "inheritUserConfig": true
        }),
    );

    let minimal = ExecSpec {
        model_options: Default::default(),
        run_id: run_id(),
        inherit_user_config: false,
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
        instructions: None,
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
               "features": ["run.events.resume", "runs.watch", "sub.gap", "approvals",
                            "approval.history", "run.steer", "daemon.shutdown", "defs", "triggers", "workspaces", "executors",
                            "executor.install", "approval.message", "routing", "attachments", "budget", "agent",
                            "run.resume", "run.stop_detail", "secrets", "executor.frames", "rules", "rules.project", "run.model", "run.effort", "run.context", "run.configure", "executor.maintain", "run.changes", "executor.usage", "threads"],
               "pid": 4242}),
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

fn approval() -> Approval {
    Approval {
        id: "appr_01K1".parse().unwrap(),
        run_id: run_id(),
        subject: ApprovalSubject {
            action: ActionClass::Command,
            tool: "bash".into(),
            command: Some("git push -u origin reno/keeper/deps-1115".into()),
            paths: vec![],
            raw: json!({}),
        },
        requested_at_ms: 1_794_700_000_000,
    }
}

fn approval_wire() -> Value {
    json!({
        "id": "appr_01K1",
        "runId": "run_01HZX9",
        "subject": {
            "action": "command",
            "tool": "bash",
            "command": "git push -u origin reno/keeper/deps-1115",
            "paths": [],
            "raw": {}
        },
        "requestedAtMs": 1_794_700_000_000u64
    })
}

#[test]
fn approval_types() {
    golden(&approval(), approval_wire());
    golden(
        &ApprovalListResult {
            approvals: vec![approval()],
        },
        json!({"approvals": [approval_wire()]}),
    );
    golden(
        &ApprovalsSnapshot { approvals: vec![] },
        json!({"approvals": []}),
    );
    golden(&ApprovalsWatchParams {}, json!({}));
    golden(&ApprovalDecideResult {}, json!({}));
}

#[test]
fn approval_decide() {
    golden(
        &ApprovalDecideParams {
            approval_id: "appr_01K1".parse().unwrap(),
            decision: ApprovalDecision::ApproveOnce,
            message: None,
            remember: None,
        },
        json!({"approvalId": "appr_01K1", "decision": "approve_once"}),
    );
    golden(
        &ApprovalDecideParams {
            approval_id: "appr_01K1".parse().unwrap(),
            decision: ApprovalDecision::Deny,
            message: None,
            remember: None,
        },
        json!({"approvalId": "appr_01K1", "decision": "deny"}),
    );
    golden(
        &ApprovalDecideParams {
            approval_id: "appr_01K1".parse().unwrap(),
            decision: ApprovalDecision::Deny,
            message: Some("use the staging bucket".into()),
            remember: None,
        },
        json!({"approvalId": "appr_01K1", "decision": "deny",
               "message": "use the staging bucket"}),
    );
    golden(
        &ApprovalDecideParams {
            approval_id: "appr_01K1".parse().unwrap(),
            decision: ApprovalDecision::ApproveOnce,
            message: None,
            remember: Some(Remember {
                scope: RememberScope::Bot,
                expires_in_secs: Some(3600),
            }),
        },
        json!({"approvalId": "appr_01K1", "decision": "approve_once",
               "remember": {"scope": "bot", "expiresInSecs": 3600}}),
    );
    golden(
        &ApprovalDecideParams {
            approval_id: "appr_01K1".parse().unwrap(),
            decision: ApprovalDecision::Deny,
            message: None,
            remember: Some(Remember {
                scope: RememberScope::Always,
                expires_in_secs: None,
            }),
        },
        json!({"approvalId": "appr_01K1", "decision": "deny",
               "remember": {"scope": "always"}}),
    );
    assert!(
        serde_json::from_value::<ApprovalDecision>(json!("approve_always")).is_err(),
        "unknown decisions are rejected, not guessed"
    );
}

#[test]
fn approval_method_names_and_feature() {
    assert_eq!(<ApprovalList as Method>::NAME, "approval.list");
    assert_eq!(<ApprovalDecide as Method>::NAME, "approval.decide");
    assert_eq!(<ApprovalsWatch as Subscription>::NAME, "approvals.watch");
    assert_eq!(FEATURE_APPROVALS, "approvals");
    assert!(FEATURES.contains(&FEATURE_APPROVALS));
}

#[test]
fn approval_history() {
    let decided = ApprovalRecord {
        id: "appr_01K1".parse().unwrap(),
        run_id: run_id(),
        request_id: "r1".into(),
        subject: approval().subject,
        subject_digest: None,
        status: ApprovalStatus::Approved,
        decided_by: Some(ResolvedBy::Human),
        created_at_ms: 1_794_700_000_000,
        decided_at_ms: Some(1_794_700_060_000),
        expires_at_ms: 1_794_701_800_000,
        message: None,
    };
    let wire = json!({
        "id": "appr_01K1",
        "runId": "run_01HZX9",
        "requestId": "r1",
        "subject": approval_wire()["subject"].clone(),
        "status": "approved",
        "decidedBy": {"type": "human"},
        "createdAtMs": 1_794_700_000_000u64,
        "decidedAtMs": 1_794_700_060_000u64,
        "expiresAtMs": 1_794_701_800_000u64
    });
    golden(&decided, wire.clone());
    let denied = ApprovalRecord {
        status: ApprovalStatus::Denied,
        message: Some("not on main".into()),
        ..decided.clone()
    };
    let value = serde_json::to_value(&denied).unwrap();
    assert_eq!(value["message"], "not on main");
    let pending = ApprovalRecord {
        status: ApprovalStatus::Pending,
        decided_by: None,
        decided_at_ms: None,
        ..decided.clone()
    };
    let value = serde_json::to_value(&pending).unwrap();
    assert_eq!(value["status"], "pending");
    assert!(value.get("decidedBy").is_none() && value.get("decidedAtMs").is_none());
    for (status, name) in [
        (ApprovalStatus::Denied, "denied"),
        (ApprovalStatus::Expired, "expired"),
    ] {
        assert_eq!(serde_json::to_value(status).unwrap(), json!(name));
    }
    golden(
        &ApprovalHistoryResult {
            approvals: vec![decided],
        },
        json!({"approvals": [wire]}),
    );
    golden(&ApprovalHistoryParams::default(), json!({}));
    golden(
        &ApprovalHistoryParams {
            run: Some(run_id()),
            limit: Some(10),
        },
        json!({"run": "run_01HZX9", "limit": 10}),
    );
    assert_eq!(<ApprovalHistory as Method>::NAME, "approval.history");
    assert_eq!(FEATURE_APPROVAL_HISTORY, "approval.history");
    assert!(FEATURES.contains(&FEATURE_APPROVAL_HISTORY));
}

#[test]
fn an_approval_subject_digest_ignores_the_raw_request_and_tells_subjects_apart() {
    let base = approval().subject;
    assert_eq!(base.digest().len(), 64);
    let with_other_raw = ApprovalSubject {
        raw: json!({"toolUseId": "toolu_2"}),
        ..base.clone()
    };
    assert_eq!(
        base.digest(),
        with_other_raw.digest(),
        "raw is not part of identity"
    );
    for other in [
        ApprovalSubject {
            command: Some("git push origin main".into()),
            ..base.clone()
        },
        ApprovalSubject {
            tool: "sh".into(),
            ..base.clone()
        },
        ApprovalSubject {
            action: ActionClass::Network,
            ..base.clone()
        },
        ApprovalSubject {
            paths: vec!["/x".into()],
            ..base.clone()
        },
    ] {
        assert_ne!(base.digest(), other.digest(), "{other:?}");
    }
}

#[test]
fn run_steer() {
    golden(
        &RunSteerParams {
            run_id: run_id(),
            prompt: "prefer small commits".into(),
        },
        json!({"runId": "run_01HZX9", "prompt": "prefer small commits"}),
    );
    golden(&RunSteerResult {}, json!({}));
    assert_eq!(<RunSteer as Method>::NAME, "run.steer");
    assert_eq!(FEATURE_RUN_STEER, "run.steer");
    assert!(FEATURES.contains(&FEATURE_RUN_STEER));
}

#[test]
fn daemon_shutdown() {
    golden(&NoParams {}, json!({}));
    golden(&DaemonShutdownResult {}, json!({}));
    assert_eq!(<DaemonShutdown as Method>::NAME, "daemon.shutdown");
    assert_eq!(FEATURE_DAEMON_SHUTDOWN, "daemon.shutdown");
    assert!(FEATURES.contains(&FEATURE_DAEMON_SHUTDOWN));
}

fn run_summary(prompt: Option<&str>, model: Option<&str>) -> RunSummary {
    RunSummary {
        model_options: Default::default(),
        effort: None,
        run_id: run_id(),
        task_id: "task_01HZX8".parse().unwrap(),
        executor: "mock".parse().unwrap(),
        status: RunStatus::Running,
        attempt: 1,
        title: "first line".into(),
        prompt: prompt.map(str::to_owned),
        model: model.map(str::to_owned),
        stop_reason: None,
        bot: None,
        cwd: None,
        created_at_ms: 10,
        updated_at_ms: 20,
        stop: None,
        not_resumed: None,
        session: None,
        report: None,
    }
}

#[test]
fn run_summary_carries_prompt_and_model() {
    golden(
        &run_summary(Some("first line\nsecond line"), Some("mock-1")),
        json!({"runId": "run_01HZX9", "taskId": "task_01HZX8", "executor": "mock",
               "status": "running", "attempt": 1, "title": "first line",
               "prompt": "first line\nsecond line", "model": "mock-1",
               "createdAtMs": 10, "updatedAtMs": 20}),
    );
}

#[test]
fn run_list_paging() {
    golden(&RunListParams::default(), json!({}));
    golden(
        &RunListParams {
            status: Some(vec![RunStatus::Succeeded]),
            limit: Some(20),
            before: Some(run_id()),
        },
        json!({"status": ["succeeded"], "limit": 20, "before": "run_01HZX9"}),
    );
    golden(
        &RunListResult {
            runs: vec![],
            next: Some(run_id()),
        },
        json!({"runs": [], "next": "run_01HZX9"}),
    );
    // The last page, and a daemon that predates paging, send no `next`.
    golden(
        &RunListResult {
            runs: vec![],
            next: None,
        },
        json!({"runs": []}),
    );
    assert_eq!((RUN_LIST_DEFAULT_LIMIT, RUN_LIST_MAX_LIMIT), (100, 500));
    assert_eq!(PROMPT_SUMMARY_CHARS, 280);
}

#[test]
fn run_summary_carries_the_agents_report_beside_the_status() {
    let mut summary = run_summary(None, None);
    summary.status = RunStatus::Succeeded;
    summary.report = Some(RunReport {
        status: ReportStatus::Failed,
        summary: "tests failed, Cargo.lock reverted".into(),
    });
    golden(
        &summary,
        json!({"runId": "run_01HZX9", "taskId": "task_01HZX8", "executor": "mock",
               "status": "succeeded", "attempt": 1, "title": "first line",
               "report": {"status": "failed", "summary": "tests failed, Cargo.lock reverted"},
               "createdAtMs": 10, "updatedAtMs": 20}),
    );
}

#[test]
fn run_summary_without_prompt_still_parses() {
    // A daemon that predates `prompt` sends no such key; both ends must cope.
    golden(
        &run_summary(None, None),
        json!({"runId": "run_01HZX9", "taskId": "task_01HZX8", "executor": "mock",
               "status": "running", "attempt": 1, "title": "first line",
               "createdAtMs": 10, "updatedAtMs": 20}),
    );
}

#[test]
fn a_run_pinned_to_a_bot_carries_the_pin() {
    let mut summary = run_summary(None, None);
    summary.bot = Some(BotPin {
        name: "keeper".into(),
        version: 3,
    });
    golden(
        &summary,
        json!({"runId": "run_01HZX9", "taskId": "task_01HZX8", "executor": "mock",
               "status": "running", "attempt": 1, "title": "first line",
               "bot": {"name": "keeper", "version": 3},
               "createdAtMs": 10, "updatedAtMs": 20}),
    );
}

#[test]
fn task_run_params_old_and_new_shapes() {
    // A client that predates `bot` sends exactly this, and it still parses.
    golden(
        &TaskRunParams {
            thread_id: None,
            model_options: Default::default(),
            effort: None,
            prompt: "go".into(),
            executor: Some("mock".parse().unwrap()),
            bot: None,
            cwd: None,
            model: None,
            in_place: false,
            attachments: Vec::new(),
        },
        json!({"prompt": "go", "executor": "mock"}),
    );
    golden(
        &TaskRunParams {
            thread_id: None,
            model_options: Default::default(),
            effort: None,
            prompt: "go".into(),
            executor: None,
            bot: Some("keeper".into()),
            cwd: None,
            model: None,
            in_place: false,
            attachments: Vec::new(),
        },
        json!({"prompt": "go", "bot": "keeper"}),
    );
}

#[test]
fn definitions_methods() {
    assert_eq!(<DefList as Method>::NAME, "def.list");
    assert_eq!(<DefGet as Method>::NAME, "def.get");
    assert_eq!(<DefValidate as Method>::NAME, "def.validate");
    assert_eq!(<DefsWatch as Subscription>::NAME, "defs.watch");
    assert_eq!(FEATURE_DEFS, "defs");
    assert!(FEATURES.contains(&FEATURE_DEFS));

    golden(&DefListParams::default(), json!({}));
    golden(
        &DefListParams {
            kind: Some(DefKind::Trigger),
        },
        json!({"kind": "trigger"}),
    );
    let summary = DefSummary {
        kind: DefKind::Bot,
        name: "keeper".into(),
        version: 2,
        hash: "ab".repeat(32),
        created_at_ms: 99,
    };
    golden(
        &summary,
        json!({"kind": "bot", "name": "keeper", "version": 2, "hash": "ab".repeat(32),
               "createdAtMs": 99}),
    );
    golden(
        &DefsSnapshot {
            defs: vec![summary],
            errors: vec![DefError::new(
                "bots/scout.toml",
                "executors[0]",
                "unknown executor \"x\"",
            )],
        },
        json!({"defs": [{"kind": "bot", "name": "keeper", "version": 2,
                         "hash": "ab".repeat(32), "createdAtMs": 99}],
               "errors": [{"file": "bots/scout.toml", "path": "executors[0]",
                           "message": "unknown executor \"x\""}]}),
    );
    // A daemon that has no errors to report may leave the key out.
    let snapshot: DefsSnapshot = serde_json::from_value(json!({"defs": []})).unwrap();
    assert_eq!(snapshot, DefsSnapshot::default());
}

#[test]
fn definition_get_and_validate() {
    golden(
        &DefGetParams {
            kind: DefKind::Policy,
            name: "policy".into(),
            version: None,
        },
        json!({"kind": "policy", "name": "policy"}),
    );
    golden(
        &DefGetParams {
            kind: DefKind::Bot,
            name: "keeper".into(),
            version: Some(2),
        },
        json!({"kind": "bot", "name": "keeper", "version": 2}),
    );
    golden(
        &DefRecord {
            kind: DefKind::Bot,
            name: "keeper".into(),
            version: 2,
            hash: "cd".repeat(32),
            body: json!({"name": "keeper", "executors": ["codex"]}),
            created_at_ms: 5,
            latest: true,
        },
        json!({"kind": "bot", "name": "keeper", "version": 2, "hash": "cd".repeat(32),
               "body": {"name": "keeper", "executors": ["codex"]},
               "createdAtMs": 5, "latest": true}),
    );
    golden(
        &DefValidateParams {
            kind: DefKind::Trigger,
            name: "nightly".into(),
            body: json!({"name": "nightly"}),
        },
        json!({"kind": "trigger", "name": "nightly", "body": {"name": "nightly"}}),
    );
    golden(
        &DefValidateResult {
            valid: false,
            errors: vec![DefError::new("triggers/nightly.toml", "", "boom")],
            hash: None,
        },
        json!({"valid": false, "errors": [{"file": "triggers/nightly.toml", "message": "boom"}]}),
    );
    golden(
        &DefValidateResult {
            valid: true,
            errors: vec![],
            hash: Some("ef".repeat(32)),
        },
        json!({"valid": true, "errors": [], "hash": "ef".repeat(32)}),
    );
}

/// The file shape of each definition type is a contract too: `def.get` returns it and the files
/// are written in it.
#[test]
fn definition_bodies_in_file_shape() {
    let bot: BotDef = serde_json::from_value(json!({
        "name": "keeper", "instructions": "x", "executors": ["codex", "claude-code"],
        "access": "auto_accept_edits", "sandbox": "read_only", "team": "core",
        "triggers": ["nightly"],
        "budget": {"max_concurrent_runs": 2, "max_wall_time": "30m", "daily_token_ceiling": 5}
    }))
    .unwrap();
    assert_eq!(
        serde_json::to_value(&bot).unwrap()["budget"]["daily_token_ceiling"],
        5
    );

    let trigger: TriggerDef = serde_json::from_value(json!({
        "name": "nightly", "when": {"kind": "cron", "cron": "0 2 * * *", "timezone": "UTC"},
        "target": {"bot": "keeper", "prompt": "go"}
    }))
    .unwrap();
    assert!(trigger.enabled);
    assert_eq!(
        serde_json::to_value(&trigger.when).unwrap(),
        json!({"kind": "cron", "cron": "0 2 * * *", "timezone": "UTC"})
    );
    assert_eq!(
        serde_json::to_value(TriggerKind::Manual {}).unwrap(),
        json!({"kind": "manual"})
    );

    let policy: PolicyDef = serde_json::from_value(json!({
        "rules": [
            {"effect": "deny", "match": {"action": "command", "command": "rm -rf *"}},
            {"effect": "allow", "match": {"tool": "read*", "scope": ["keeper"]}}
        ]
    }))
    .unwrap();
    assert_eq!(policy.default, PolicyEffect::Ask);
    assert_eq!(policy.rules[0].matches.scope, RuleScope::All);
    let back = serde_json::to_value(&policy).unwrap();
    assert_eq!(back["rules"][0]["match"]["scope"], "*");
    assert_eq!(back["rules"][1]["match"]["scope"], json!(["keeper"]));

    // Unknown fields are refused at every level.
    for bad in [
        json!({"name": "a", "instructions": "x", "executors": [], "mood": 1}),
        json!({"name": "a", "instructions": "x", "executors": [], "budget": {"cpu": 1}}),
    ] {
        assert!(serde_json::from_value::<BotDef>(bad).is_err());
    }
    assert!(
        serde_json::from_value::<PolicyDef>(
            json!({"rules": [{"effect": "ask", "match": {"x": 1}}]})
        )
        .is_err()
    );
    assert!(serde_json::from_value::<RuleScope>(json!("keeper")).is_err());
}

#[test]
fn trigger_state_on_the_wire() {
    assert_eq!(<TriggerList as Method>::NAME, "trigger.list");
    assert_eq!(<TriggerFire as Method>::NAME, "trigger.fire");
    assert_eq!(<TriggersWatch as Subscription>::NAME, "triggers.watch");

    golden(
        &TriggerInfo {
            name: "nightly".into(),
            kind: "cron".into(),
            bot: "keeper".into(),
            version: 2,
            status: TriggerStatus::BackedOff,
            next_due_at_ms: Some(1_800_000_000_000),
            last_fired_at_ms: Some(1_799_999_000_000),
            last_run_id: Some(run_id()),
            failures: 3,
            backoff_until_ms: Some(1_800_000_600_000),
            error: None,
            cwd: None,
        },
        json!({
            "name": "nightly", "kind": "cron", "bot": "keeper", "version": 2,
            "status": "backed_off", "nextDueAtMs": 1_800_000_000_000_u64,
            "lastFiredAtMs": 1_799_999_000_000_u64, "lastRunId": "run_01HZX9",
            "failures": 3, "backoffUntilMs": 1_800_000_600_000_u64
        }),
    );
    // A trigger that has never fired carries only what is known.
    golden(
        &TriggerInfo {
            name: "poke".into(),
            kind: "manual".into(),
            bot: "keeper".into(),
            version: 1,
            status: TriggerStatus::Manual,
            next_due_at_ms: None,
            last_fired_at_ms: None,
            last_run_id: None,
            failures: 0,
            backoff_until_ms: None,
            error: None,
            cwd: None,
        },
        json!({"name": "poke", "kind": "manual", "bot": "keeper", "version": 1,
               "status": "manual", "failures": 0}),
    );
    // A trigger that names a directory shows it, as written.
    golden(
        &TriggerInfo {
            name: "nightly".into(),
            kind: "cron".into(),
            bot: "keeper".into(),
            version: 1,
            status: TriggerStatus::Scheduled,
            next_due_at_ms: None,
            last_fired_at_ms: None,
            last_run_id: None,
            failures: 0,
            backoff_until_ms: None,
            error: None,
            cwd: Some("~/code/app".into()),
        },
        json!({"name": "nightly", "kind": "cron", "bot": "keeper", "version": 1,
               "status": "scheduled", "failures": 0, "cwd": "~/code/app"}),
    );
    golden(
        &TriggerFireParams {
            name: "poke".into(),
        },
        json!({"name": "poke"}),
    );
    golden(
        &TriggerFireResult {
            outcome: FiringOutcome::Coalesced,
            run_id: Some(run_id()),
            scheduled_at_ms: 5,
        },
        json!({"outcome": "coalesced", "runId": "run_01HZX9", "scheduledAtMs": 5}),
    );
    golden(
        &TriggersSnapshot { triggers: vec![] },
        json!({"triggers": []}),
    );
    for (outcome, wire) in [
        (FiringOutcome::Fired, "fired"),
        (FiringOutcome::Coalesced, "coalesced"),
        (FiringOutcome::Duplicate, "duplicate"),
        (FiringOutcome::BackedOff, "backed_off"),
    ] {
        golden(&outcome, json!(wire));
    }
    for (status, wire) in [
        (TriggerStatus::Scheduled, "scheduled"),
        (TriggerStatus::Watching, "watching"),
        (TriggerStatus::Manual, "manual"),
        (TriggerStatus::Done, "done"),
        (TriggerStatus::BackedOff, "backed_off"),
        (TriggerStatus::Disabled, "disabled"),
        (TriggerStatus::Failed, "failed"),
    ] {
        golden(&status, json!(wire));
    }
}

#[test]
fn workspace_types_on_the_wire() {
    use reno_proto::{
        CleanItem, CleanOutcome, CleanTarget, WorkspaceChanges, WorkspaceCleanParams,
        WorkspaceInfo, WorkspaceStatus,
    };
    let info = WorkspaceInfo {
        run_id: run_id(),
        repo: "/r".into(),
        repo_key: "r-0123456789ab".into(),
        path: "/d/worktrees/r-0123456789ab/run_a".into(),
        subdir: "crates/x".into(),
        branch: "reno/run_a".into(),
        base_sha: "abc".into(),
        status: WorkspaceStatus::Kept,
        changes: Some(WorkspaceChanges {
            commits: 2,
            dirty: true,
        }),
        created_at_ms: 1,
        updated_at_ms: 2,
    };
    golden(
        &info,
        json!({
            "runId": run_id(),
            "repo": "/r",
            "repoKey": "r-0123456789ab",
            "path": "/d/worktrees/r-0123456789ab/run_a",
            "subdir": "crates/x",
            "branch": "reno/run_a",
            "baseSha": "abc",
            "status": "kept",
            "changes": {"commits": 2, "dirty": true},
            "createdAtMs": 1,
            "updatedAtMs": 2,
        }),
    );
    assert_eq!(
        info.cwd(),
        std::path::PathBuf::from("/d/worktrees/r-0123456789ab/run_a/crates/x")
    );
    golden(
        &WorkspaceCleanParams {
            target: CleanTarget::Merged,
            force: false,
        },
        json!({"target": {"type": "merged"}, "force": false}),
    );
    golden(
        &WorkspaceCleanParams {
            target: CleanTarget::Run { run_id: run_id() },
            force: true,
        },
        json!({"target": {"type": "run", "runId": run_id()}, "force": true}),
    );
    golden(
        &CleanItem {
            run_id: run_id(),
            outcome: CleanOutcome::Kept,
            reason: Some("unmerged commits".into()),
        },
        json!({"runId": run_id(), "outcome": "kept", "reason": "unmerged commits"}),
    );
}

#[test]
fn executor_health_on_the_wire() {
    assert_eq!(<ExecutorList as Method>::NAME, "executor.list");
    assert_eq!(<ExecutorRefresh as Method>::NAME, "executor.refresh");
    assert_eq!(<ExecutorInstall as Method>::NAME, "executor.install");
    assert_eq!(FEATURE_EXECUTOR_INSTALL, "executor.install");
    golden(
        &ExecutorInstallParams {
            id: "acp.hermes".parse().unwrap(),
        },
        json!({"id": "acp.hermes"}),
    );
    assert_eq!(<ExecutorsWatch as Subscription>::NAME, "executors.watch");

    assert_eq!(<ExecutorFrames as Method>::NAME, "executor.frames");
    assert_eq!(FEATURE_EXECUTOR_FRAMES, "executor.frames");
    golden(&ExecutorFramesParams { id: None }, json!({}));
    golden(
        &ExecutorFramesResult {
            executors: vec![ExecutorFrameCounts {
                id: "codex".parse().unwrap(),
                cli_version: Some("0.170.0".into()),
                frames: vec![UnknownFrameKind {
                    kind: "method:thread/new".into(),
                    count: 3,
                    first_seen_ms: 1_800_000_000_000,
                }],
            }],
        },
        json!({"executors": [{
            "id": "codex",
            "cliVersion": "0.170.0",
            "frames": [{"kind": "method:thread/new", "count": 3, "firstSeenMs": 1_800_000_000_000_u64}]
        }]}),
    );

    golden(
        &ExecutorHealth {
            installed: InstallStatus::Installed {
                path: "/usr/local/bin/codex".into(),
                version: Some("0.169.0".into()),
            },
            version_gate: VersionGate::Untested {
                verified: "0.169.0".into(),
            },
            signed_in: SignedIn::Yes {
                account: Some("me@example.com".into()),
            },
            models: ModelCatalog::live(vec![ModelInfo {
                options: Vec::new(),
                default_effort: None,
                efforts: vec![],
                id: "gpt-6".into(),
                display_name: Some("GPT-6".into()),
                description: None,
            }]),
            checked_at_ms: 1_800_000_000_000,
            plan_usage: None,
        },
        json!({
            "installed": {"type": "installed", "path": "/usr/local/bin/codex", "version": "0.169.0"},
            "versionGate": {"type": "untested", "verified": "0.169.0"},
            "signedIn": {"type": "yes", "account": "me@example.com"},
            "models": {"source": "live", "models": [{"id": "gpt-6", "displayName": "GPT-6"}]},
            "checkedAtMs": 1_800_000_000_000_u64
        }),
    );
    golden(
        &ExecutorHealth {
            installed: InstallStatus::NotInstalled {
                why: NotInstalledReason::NeedsInstall {
                    what: "the Hermes ACP adapter (npm)".into(),
                },
            },
            version_gate: VersionGate::Unchecked,
            signed_in: SignedIn::Unknown,
            models: ModelCatalog::curated(vec![]),
            checked_at_ms: 5,
            plan_usage: None,
        },
        json!({
            "installed": {"type": "not_installed", "why": {"type": "needs_install", "what": "the Hermes ACP adapter (npm)"}},
            "versionGate": {"type": "unchecked"},
            "signedIn": {"type": "unknown"},
            "models": {"source": "curated", "models": []},
            "checkedAtMs": 5
        }),
    );
    for (value, wire) in [
        (
            NotInstalledReason::NotFound { hint: "x".into() },
            json!({"type": "not_found", "hint": "x"}),
        ),
        (
            NotInstalledReason::Unusable { detail: "d".into() },
            json!({"type": "unusable", "detail": "d"}),
        ),
        (
            NotInstalledReason::ProbeFailed { detail: "d".into() },
            json!({"type": "probe_failed", "detail": "d"}),
        ),
    ] {
        golden(&value, wire);
    }
    golden(&VersionGate::Ok, json!({"type": "ok"}));
    golden(
        &VersionGate::TooOld {
            min: "2.0.0".into(),
        },
        json!({"type": "too_old", "min": "2.0.0"}),
    );
    golden(&SignedIn::Yes { account: None }, json!({"type": "yes"}));
    golden(
        &SignedIn::No {
            hint: "run `claude auth login`".into(),
        },
        json!({"type": "no", "hint": "run `claude auth login`"}),
    );
    // Plan usage rides on the health as `planUsage`; a window's reset is optional.
    golden(
        &ExecutorHealth {
            installed: InstallStatus::Installed {
                path: "/usr/local/bin/claude".into(),
                version: Some("2.2.0".into()),
            },
            version_gate: VersionGate::Ok,
            signed_in: SignedIn::Unknown,
            models: ModelCatalog::curated(vec![]),
            checked_at_ms: 5,
            plan_usage: Some(PlanUsage {
                plan_label: Some("ChatGPT Plus".into()),
                windows: vec![
                    UsageWindow::new("Session", 0.54, Some(1_800_007_200_000)),
                    UsageWindow::new("Week", 0.21, None),
                ],
                fetched_at_ms: 1_800_000_000_000,
            }),
        },
        json!({
            "installed": {"type": "installed", "path": "/usr/local/bin/claude", "version": "2.2.0"},
            "versionGate": {"type": "ok"},
            "signedIn": {"type": "unknown"},
            "models": {"source": "curated", "models": []},
            "checkedAtMs": 5,
            "planUsage": {
                "planLabel": "ChatGPT Plus",
                "windows": [
                    {"label": "Session", "usedPermille": 540, "resetsAtMs": 1_800_007_200_000_u64},
                    {"label": "Week", "usedPermille": 210}
                ],
                "fetchedAtMs": 1_800_000_000_000_u64
            }
        }),
    );
    golden(
        &ExecutorRefreshParams {
            id: None,
            usage_only: true,
        },
        json!({"usageOnly": true}),
    );
    golden(&ModelSource::Curated, json!("curated"));
    golden(&ModelSource::Live, json!("live"));
    golden(
        &ExecutorRefreshParams {
            id: None,
            usage_only: false,
        },
        json!({}),
    );
    golden(
        &ExecutorRefreshParams {
            id: Some("codex".parse().unwrap()),
            usage_only: false,
        },
        json!({"id": "codex"}),
    );
    golden(
        &ExecutorsSnapshot { executors: vec![] },
        json!({"executors": []}),
    );
    golden(
        &ExecutorListResult { executors: vec![] },
        json!({"executors": []}),
    );
    // Health a daemon has not produced yet is simply absent.
    let info: ExecutorInfo = serde_json::from_value(json!({
        "descriptor": {
            "id": "mock", "contractVersion": 1, "adapterVersion": "x",
            "cliVerified": {"min": "0", "verified": "1"},
            "caps": {"steering": "none", "resume": false, "accessModes": ["supervised"],
                     "sandboxLevels": ["unrestricted"], "mcpInjection": false,
                     "structuredOutput": false, "deterministicTurnEnd": false,
                     "planMode": false, "models": []}
        }
    }))
    .unwrap();
    assert!(info.health.is_none());
    // Caps from a daemon that predates user-config isolation claim none.
    assert!(!info.descriptor.caps.isolates_user_config);
    assert!(serde_json::to_value(&info).unwrap().get("health").is_none());
}

#[test]
fn task_run_attachments() {
    assert_eq!(FEATURE_ATTACHMENTS, "attachments");
    assert!(FEATURES.contains(&FEATURE_ATTACHMENTS));
    assert_eq!(ATTACHMENTS_MAX_COUNT, 10);
    assert_eq!(ATTACHMENT_MAX_BYTES, 25 * 1024 * 1024);

    let attachments = vec![
        Attachment {
            path: "/tmp/shot.png".into(),
            name: "shot.png".into(),
            mime: Some("image/png".into()),
        },
        Attachment {
            path: "/tmp/notes.txt".into(),
            name: "notes".into(),
            mime: None,
        },
    ];
    golden(
        &TaskRunParams {
            prompt: "look".into(),
            attachments: attachments.clone(),
            ..TaskRunParams::default()
        },
        json!({"prompt": "look", "attachments": [
            {"path": "/tmp/shot.png", "name": "shot.png", "mime": "image/png"},
            {"path": "/tmp/notes.txt", "name": "notes"},
        ]}),
    );
    assert_eq!(
        Attachment::prompt_suffix(&attachments),
        "\n\nAttached files:\n- /tmp/shot.png (shot.png, image/png)\n- /tmp/notes.txt (notes)\n"
    );
    assert_eq!(Attachment::prompt_suffix(&[]), "");
}

#[test]
fn secrets_on_the_wire() {
    assert_eq!(<SecretList as Method>::NAME, "secret.list");
    assert_eq!(<SecretSet as Method>::NAME, "secret.set");
    assert_eq!(<SecretDelete as Method>::NAME, "secret.delete");
    assert!(FEATURES.contains(&FEATURE_SECRETS));
    golden(
        &SecretListResult {
            secrets: vec![
                SecretInfo {
                    name: "GITHUB_TOKEN".into(),
                    set: true,
                    used_by: vec!["keeper".into()],
                },
                SecretInfo {
                    name: "NPM_TOKEN".into(),
                    set: false,
                    used_by: Vec::new(),
                },
            ],
        },
        json!({"secrets": [
            {"name": "GITHUB_TOKEN", "set": true, "usedBy": ["keeper"]},
            {"name": "NPM_TOKEN", "set": false},
        ]}),
    );
    golden(
        &SecretSetParams {
            name: "GITHUB_TOKEN".into(),
            value: "ghp_example".into(),
        },
        json!({"name": "GITHUB_TOKEN", "value": "ghp_example"}),
    );
    golden(
        &SecretSetResult { replaced: true },
        json!({"replaced": true}),
    );
    golden(
        &SecretDeleteParams {
            name: "NPM_TOKEN".into(),
        },
        json!({"name": "NPM_TOKEN"}),
    );
    golden(
        &SecretDeleteResult { deleted: false },
        json!({"deleted": false}),
    );
    golden(
        &SecretsMissing {
            bot: "keeper".into(),
            missing: vec!["GITHUB_TOKEN".into()],
        },
        json!({"bot": "keeper", "missing": ["GITHUB_TOKEN"]}),
    );
    golden(&StartRefusal::SecretMissing, json!("secret_missing"));
}

#[test]
fn a_bot_lists_secrets_by_name_only_when_it_has_some() {
    let bot: BotDef = serde_json::from_value(json!({
        "name": "keeper", "instructions": "x", "executors": ["codex"],
        "secrets": ["GITHUB_TOKEN"],
    }))
    .unwrap();
    assert_eq!(bot.secrets, ["GITHUB_TOKEN"]);
    assert_eq!(
        serde_json::to_value(&bot).unwrap()["secrets"],
        json!(["GITHUB_TOKEN"])
    );
    let plain: BotDef = serde_json::from_value(json!({
        "name": "keeper", "instructions": "x", "executors": ["codex"],
    }))
    .unwrap();
    assert!(
        serde_json::to_value(&plain)
            .unwrap()
            .get("secrets")
            .is_none()
    );
}

#[test]
fn an_exec_spec_never_debug_prints_its_environment_values() {
    let spec: ExecSpec = serde_json::from_value(json!({
        "runId": "run_01HZX9", "prompt": "go", "cwd": "/w",
        "access": "supervised", "sandbox": "unrestricted",
        "env": {"GITHUB_TOKEN": "ghp_supersecretvalue"},
    }))
    .unwrap();
    let shown = format!("{spec:?}");
    assert!(shown.contains("GITHUB_TOKEN"), "{shown}");
    assert!(!shown.contains("ghp_supersecretvalue"), "{shown}");
}

// --- resume and the typed stop reason (T1.10) ---------------------------------------------------

#[test]
fn done_carries_a_typed_reason_beside_its_diagnostic() {
    let reasons = [
        (
            StopDetail::BudgetExceeded {
                kind: BudgetKind::WallTime,
                limit: 60_000,
                used: 61_000,
            },
            json!({"type": "budget_exceeded", "kind": "wall_time", "limit": 60000, "used": 61000}),
        ),
        (
            StopDetail::StartRefused {
                failure: StartRefusal::NotSignedIn,
                detail: "executor codex is not signed in".into(),
            },
            json!({"type": "start_refused", "failure": "not_signed_in",
                   "detail": "executor codex is not signed in"}),
        ),
        (
            StopDetail::Cancelled {
                by: CancelledBy::User,
            },
            json!({"type": "cancelled", "by": "user"}),
        ),
        (StopDetail::Shutdown, json!({"type": "shutdown"})),
        (StopDetail::Crashed, json!({"type": "crashed"})),
        (
            StopDetail::ExecutorStopped,
            json!({"type": "executor_stopped"}),
        ),
        (
            StopDetail::ProtectedPathWrite {
                paths: vec![".claude/settings.json".into()],
                reverted: true,
            },
            json!({"type": "protected_path_write", "paths": [".claude/settings.json"],
                   "reverted": true}),
        ),
    ];
    for (reason, wire) in reasons {
        golden(&reason, wire.clone());
        golden(
            &ExecEvent::Done {
                stop: StopReason::Interrupted,
                result: None,
                diagnostic: Some("words".into()),
                reason: Some(reason),
            },
            json!({"type": "done", "stop": "interrupted", "diagnostic": "words", "reason": wire}),
        );
    }
}

#[test]
fn a_done_without_a_reason_still_parses() {
    // An older daemon, and every executor's own `Done`, send none.
    let event: ExecEvent =
        serde_json::from_value(json!({"type": "done", "stop": "completed"})).unwrap();
    assert!(matches!(event, ExecEvent::Done { reason: None, .. }));
}

#[test]
fn a_resume_is_journaled_with_the_attempt_it_continues() {
    golden(
        &ExecEvent::Resumed {
            from_attempt: 1,
            session: SessionRef("sess-1".into()),
        },
        json!({"type": "resumed", "fromAttempt": 1, "session": "sess-1"}),
    );
}

#[test]
fn run_resume_wire_shapes() {
    assert_eq!(<RunResume as Method>::NAME, "run.resume");
    assert_eq!(FEATURE_RUN_RESUME, "run.resume");
    assert_eq!(FEATURE_STOP_DETAIL, "run.stop_detail");
    assert!(FEATURES.contains(&FEATURE_RUN_RESUME) && FEATURES.contains(&FEATURE_STOP_DETAIL));
    golden(
        &RunResumeParams {
            run_id: run_id(),
            prompt: None,
        },
        json!({"runId": "run_01HZX9"}),
    );
    golden(
        &RunResumeParams {
            run_id: run_id(),
            prompt: Some("go on".into()),
        },
        json!({"runId": "run_01HZX9", "prompt": "go on"}),
    );
    golden(
        &RunResumeResult {
            status: RunStatus::Queued,
            attempt: 2,
        },
        json!({"status": "queued", "attempt": 2}),
    );
}

#[test]
fn why_a_run_was_not_resumed_is_typed() {
    let cases = [
        (NotResumed::Cancelled, json!({"type": "cancelled"})),
        (NotResumed::BudgetStopped, json!({"type": "budget_stopped"})),
        (NotResumed::NoSession, json!({"type": "no_session"})),
        (
            NotResumed::ExecutorCannotResume {
                executor: "codex".parse().unwrap(),
            },
            json!({"type": "executor_cannot_resume", "executor": "codex"}),
        ),
        (
            NotResumed::BotForbids {
                bot: "keeper".into(),
            },
            json!({"type": "bot_forbids", "bot": "keeper"}),
        ),
        (
            NotResumed::AttemptsExhausted {
                attempts: 3,
                max: 3,
            },
            json!({"type": "attempts_exhausted", "attempts": 3, "max": 3}),
        ),
        (
            NotResumed::WallTimeSpent { limit_ms: 60_000 },
            json!({"type": "wall_time_spent", "limitMs": 60000}),
        ),
        (
            NotResumed::WrongStatus {
                status: RunStatus::Running,
            },
            json!({"type": "wrong_status", "status": "running"}),
        ),
    ];
    for (why, wire) in cases {
        golden(&why, wire.clone());
        golden(
            &NotResumable {
                run_id: run_id(),
                why,
            },
            json!({"runId": "run_01HZX9", "why": wire}),
        );
    }
}

#[test]
fn a_summary_carries_its_stop_and_session() {
    let mut summary = run_summary(None, None);
    summary.status = RunStatus::Interrupted;
    summary.stop = Some(StopDetail::Shutdown);
    summary.not_resumed = Some(NotResumed::AttemptsExhausted {
        attempts: 3,
        max: 3,
    });
    summary.session = Some(SessionRef("sess-1".into()));
    golden(
        &summary,
        json!({"runId": "run_01HZX9", "taskId": "task_01HZX8", "executor": "mock",
               "status": "interrupted", "attempt": 1, "title": "first line",
               "stop": {"type": "shutdown"},
               "notResumed": {"type": "attempts_exhausted", "attempts": 3, "max": 3},
               "session": "sess-1",
               "createdAtMs": 10, "updatedAtMs": 20}),
    );
}

#[test]
fn a_bot_without_a_resume_setting_keeps_its_hash() {
    use reno_proto::defs::{BotDef, ResumePolicy, content_hash};
    let toml = "name = \"k\"\ninstructions = \"x\"\nexecutors = [\"mock\"]\n";
    let plain: BotDef = toml::from_str(toml).unwrap();
    assert_eq!(plain.resume, ResumePolicy::Auto);
    let explicit: BotDef = toml::from_str(&format!("{toml}resume = \"auto\"\n")).unwrap();
    let never: BotDef = toml::from_str(&format!("{toml}resume = \"never\"\n")).unwrap();
    let hash = |bot: &BotDef| content_hash(&serde_json::to_value(bot).unwrap());
    assert_eq!(
        hash(&plain),
        hash(&explicit),
        "auto is the default and is never written"
    );
    assert_ne!(hash(&plain), hash(&never));
    assert!(
        serde_json::to_value(&plain)
            .unwrap()
            .get("resume")
            .is_none()
    );
    assert_eq!(serde_json::to_value(&never).unwrap()["resume"], "never");
}

#[test]
fn standing_rules() {
    assert_eq!(<RuleList as Method>::NAME, "rule.list");
    assert_eq!(<RuleRevoke as Method>::NAME, "rule.revoke");
    assert!(FEATURES.contains(&FEATURE_RULES));
    golden(
        &RuleListParams::default(),
        json!({"includeInactive": false}),
    );
    golden(&RuleRevokeResult {}, json!({}));
    golden(
        &RuleListParams {
            include_inactive: false,
            project: Some("/Users/x/porch".into()),
        },
        json!({"includeInactive": false, "project": "/Users/x/porch"}),
    );
    golden(
        &RuleRevokeParams {
            id: "rule_01K1".parse().unwrap(),
        },
        json!({"id": "rule_01K1"}),
    );
    let rule = StandingRule {
        id: "rule_01K1".parse().unwrap(),
        scope: StandingScope::Bot {
            bot: "scout".into(),
        },
        effect: StandingEffect::Allow,
        matcher: RuleMatcher {
            action: ActionClass::Command,
            tool: "bash".into(),
            command: Some(CommandPattern::Exact {
                command: "git push".into(),
            }),
            paths: vec![],
        },
        created_by: "human".into(),
        created_at_ms: 1_794_700_000_000,
        expires_at_ms: None,
        revoked_at_ms: None,
        source_approval: Some("appr_01K1".parse().unwrap()),
    };
    let wire = json!({
        "id": "rule_01K1",
        "scope": {"type": "bot", "bot": "scout"},
        "effect": "allow",
        "matcher": {
            "action": "command",
            "tool": "bash",
            "command": {"type": "exact", "command": "git push"},
            "paths": []
        },
        "createdBy": "human",
        "createdAtMs": 1_794_700_000_000u64,
        "sourceApproval": "appr_01K1"
    });
    golden(&rule, wire.clone());
    golden(
        &RuleListResult { rules: vec![rule] },
        json!({"rules": [wire]}),
    );
    golden(&StandingScope::Always, json!({"type": "always"}));
}

#[test]
fn project_rules() {
    assert_eq!(<RuleAdd as Method>::NAME, "rule.add");
    assert_eq!(FEATURE_PROJECT_RULES, "rules.project");
    assert!(FEATURES.contains(&FEATURE_PROJECT_RULES));
    assert_eq!(
        serde_json::to_value(RememberScope::Project).unwrap(),
        json!("project")
    );
    golden(
        &Remember {
            scope: RememberScope::Project,
            expires_in_secs: None,
        },
        json!({"scope": "project"}),
    );
    golden(
        &StandingScope::Project {
            root: "/Users/x/porch".into(),
        },
        json!({"type": "project", "root": "/Users/x/porch"}),
    );
    golden(
        &RuleAddParams {
            project: "/Users/x/porch/src".into(),
            effect: StandingEffect::Allow,
            command: "cargo test".into(),
            tool: None,
            expires_in_secs: None,
        },
        json!({"project": "/Users/x/porch/src", "effect": "allow", "command": "cargo test"}),
    );
    golden(
        &RuleAddParams {
            project: "/Users/x/porch".into(),
            effect: StandingEffect::Deny,
            command: "git push --force".into(),
            tool: Some("exec".into()),
            expires_in_secs: Some(60),
        },
        json!({"project": "/Users/x/porch", "effect": "deny", "command": "git push --force",
               "tool": "exec", "expiresInSecs": 60}),
    );
}

#[test]
fn run_context() {
    golden(
        &ExecEvent::ContextUsage {
            tokens: Some(84_213),
            window: Some(200_000),
        },
        json!({"type": "context_usage", "tokens": 84_213, "window": 200_000}),
    );
    golden(
        &ExecEvent::ContextUsage {
            tokens: None,
            window: Some(1_000_000),
        },
        json!({"type": "context_usage", "window": 1_000_000}),
    );
    golden(
        &ContextUsage {
            tokens: Some(1),
            window: None,
        },
        json!({"tokens": 1, "window": null}),
    );
    golden(
        &ModelInfo {
            id: "claude-opus-5-5".into(),
            display_name: None,
            description: None,
            efforts: vec![],
            default_effort: None,
            options: vec![ModelOption {
                settable: true,
                id: MODEL_OPTION_CONTEXT_WINDOW.into(),
                label: "Context window".into(),
                choices: vec![
                    ModelOptionChoice {
                        id: CONTEXT_WINDOW_200K.into(),
                        label: "200K".into(),
                    },
                    ModelOptionChoice {
                        id: CONTEXT_WINDOW_1M.into(),
                        label: "1M".into(),
                    },
                ],
                default_choice: CONTEXT_WINDOW_1M.into(),
            }],
        },
        json!({"id": "claude-opus-5-5", "options": [{
            "id": "contextWindow", "label": "Context window",
            "choices": [{"id": "200k", "label": "200K"}, {"id": "1m", "label": "1M"}],
            "defaultChoice": "1m"}]}),
    );
    golden(
        &TaskRunParams {
            prompt: "p".into(),
            model_options: [("contextWindow".to_owned(), "200k".to_owned())].into(),
            ..TaskRunParams::default()
        },
        json!({"prompt": "p", "modelOptions": {"contextWindow": "200k"}}),
    );
    // A read-only option says so; settable is the default and stays off the wire.
    golden(
        &ModelOption {
            id: MODEL_OPTION_CONTEXT_WINDOW.into(),
            label: "Context window".into(),
            choices: vec![ModelOptionChoice {
                id: "256k".into(),
                label: "256K".into(),
            }],
            default_choice: "256k".into(),
            settable: false,
        },
        json!({"id": "contextWindow", "label": "Context window",
               "choices": [{"id": "256k", "label": "256K"}],
               "defaultChoice": "256k", "settable": false}),
    );
    assert_eq!(FEATURE_RUN_CONTEXT, "run.context");
    assert!(FEATURES.contains(&FEATURE_RUN_CONTEXT));
}

#[test]
fn run_configure() {
    golden(
        &RunConfigureParams {
            run_id: run_id(),
            effort: Some(EffortChange::Set {
                effort: Effort::High,
            }),
            model_options: [("contextWindow".to_owned(), "1m".to_owned())].into(),
        },
        json!({"runId": "run_01HZX9", "effort": {"type": "set", "effort": "high"},
               "modelOptions": {"contextWindow": "1m"}}),
    );
    golden(
        &RunConfigureParams {
            run_id: run_id(),
            effort: Some(EffortChange::Clear),
            model_options: Default::default(),
        },
        json!({"runId": "run_01HZX9", "effort": {"type": "clear"}}),
    );
    // Absent means unchanged, which is not the same as clear.
    golden(
        &RunConfigureParams {
            run_id: run_id(),
            effort: None,
            model_options: Default::default(),
        },
        json!({"runId": "run_01HZX9"}),
    );
    golden(
        &RunConfigureResult {
            effort: None,
            model_options: Default::default(),
            effort_applies: Some(ConfigTiming::NextTurn),
            options_apply: Some(ConfigTiming::AfterRestart),
        },
        json!({"effortApplies": "next_turn", "optionsApply": "after_restart"}),
    );
    golden(
        &ExecEvent::ConfigChanged {
            effort: Some(Effort::Low),
            model_options: [("contextWindow".to_owned(), "200k".to_owned())].into(),
        },
        json!({"type": "config_changed", "effort": "low",
               "modelOptions": {"contextWindow": "200k"}}),
    );
    golden(
        &ExecEvent::ConfigChanged {
            effort: None,
            model_options: Default::default(),
        },
        json!({"type": "config_changed"}),
    );
    assert_eq!(<RunConfigure as Method>::NAME, "run.configure");
    assert_eq!(FEATURE_RUN_CONFIGURE, "run.configure");
    assert!(FEATURES.contains(&FEATURE_RUN_CONFIGURE));
}
