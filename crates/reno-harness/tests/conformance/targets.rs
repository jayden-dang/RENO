//! The targets the conformance suite runs on in CI: the mock and every driver over its fake CLI.
//!
//! Each target is data: the executor, what the descriptor cannot say about it, and a plan that
//! maps every case to a scripted scenario of its fake. The cases own all assertions.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use reno_harness::conformance::{
    ApprovalPlan, DENY_MESSAGE, Expect, Limits, Plan, Profile, Scenario, Target, UnknownFramePlan,
};
use reno_harness::exec::{ExecControls, ExecError, Executor, MockExecutor, MockStep};
use reno_harness::{AcpExecutor, ClaudeExecutor, CodexExecutor, RegistryAgent};
use reno_proto::{
    AccessMode, ActionClass, ApprovalSubject, ExecEvent, ExecSpec, ExecutorDescriptor, ExecutorId,
    HealthProbe, McpServerSpec, Question, SessionRef, StopReason, ToolCall,
};

pub(crate) fn fixture(name: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name);
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    path
}

fn cwd() -> PathBuf {
    std::env::temp_dir()
}

// -- the mock ------------------------------------------------------------------------------

/// A mock that plays the script its prompt names (`mock:<case>`), so one executor serves every
/// case. It wraps [`MockExecutor`], which owns the playing and the log.
#[derive(Clone)]
pub(crate) struct ScriptedMock {
    pub(crate) base: MockExecutor,
}

impl ScriptedMock {
    pub(crate) fn new() -> Self {
        Self::with_descriptor(None)
    }

    pub(crate) fn with_descriptor(descriptor: Option<ExecutorDescriptor>) -> Self {
        let id: ExecutorId = "mock".parse().expect("id");
        let mut base = MockExecutor::new(id);
        if let Some(descriptor) = descriptor {
            base = base.with_descriptor(descriptor);
        }
        Self { base }
    }
}

fn started() -> MockStep {
    MockStep::started("mock-session", "mock-1")
}

fn subject(action: ActionClass, tool: &str, command: &str) -> ApprovalSubject {
    ApprovalSubject {
        action,
        tool: tool.into(),
        command: Some(command.into()),
        paths: Vec::new(),
        raw: serde_json::json!({ "command": command }),
    }
}

fn script(prompt: &str) -> Vec<MockStep> {
    let tool = |id: &str| {
        MockStep::Emit(ExecEvent::Tool {
            id: id.into(),
            call: ToolCall::new(
                "Bash",
                serde_json::json!({ "command": "echo hi" }),
                ActionClass::Command,
            ),
        })
    };
    match prompt {
        "mock:lifecycle" => vec![
            started(),
            MockStep::text("conf-hello"),
            tool("t1"),
            MockStep::Emit(ExecEvent::ToolResult {
                id: "t1".into(),
                ok: true,
                summary: None,
            }),
            MockStep::Emit(ExecEvent::Done {
                stop: StopReason::Completed,
                result: Some("conf-done".into()),
                diagnostic: None,
            }),
        ],
        "mock:approval" => vec![
            started(),
            MockStep::Emit(ExecEvent::NeedsApproval {
                request_id: "a1".into(),
                subject: subject(ActionClass::Command, "Bash", "rm -rf build"),
            }),
            MockStep::AwaitAnswer {
                request_id: "a1".into(),
            },
            MockStep::text("conf-answered"),
            MockStep::done(StopReason::Completed),
        ],
        "mock:input" => vec![
            started(),
            MockStep::Emit(ExecEvent::NeedsInput {
                request_id: "q1".into(),
                questions: vec![Question {
                    id: "pick".into(),
                    prompt: "Pick one".into(),
                    options: vec!["A".into(), "B".into()],
                    multi_select: false,
                }],
            }),
            MockStep::AwaitAnswer {
                request_id: "q1".into(),
            },
            MockStep::text("conf-input-answered"),
            MockStep::done(StopReason::Completed),
        ],
        "mock:steer" => vec![
            started(),
            MockStep::text("first"),
            MockStep::AwaitSteer,
            MockStep::text("conf-steered"),
            MockStep::done(StopReason::Completed),
        ],
        "mock:cancel" => vec![
            started(),
            MockStep::text("working"),
            MockStep::Sleep(std::time::Duration::from_secs(3600)),
        ],
        "mock:crash" => vec![
            started(),
            MockStep::Emit(ExecEvent::Done {
                stop: StopReason::Error,
                result: None,
                diagnostic: Some("the scripted harness crashed".into()),
            }),
        ],
        "mock:resume" => vec![
            MockStep::started("mock-resumed", "mock-1"),
            MockStep::text("conf-resumed"),
            MockStep::done(StopReason::Completed),
        ],
        "mock:mcp" => vec![
            started(),
            MockStep::text("conf-mcp"),
            MockStep::done(StopReason::Completed),
        ],
        _ => vec![MockStep::done(StopReason::Error)],
    }
}

#[async_trait]
impl Executor for ScriptedMock {
    fn descriptor(&self) -> ExecutorDescriptor {
        self.base.descriptor()
    }

    async fn probe(&self) -> HealthProbe {
        self.base.probe().await
    }

    async fn start(
        &self,
        spec: ExecSpec,
        controls: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError> {
        let steps = script(&spec.prompt);
        self.base
            .clone()
            .with_steps(steps)
            .start(spec, controls)
            .await
    }
}

pub(crate) fn mock_target() -> Target {
    mock_target_for(Arc::new(ScriptedMock::new()))
}

pub(crate) fn mock_target_for(executor: Arc<dyn Executor>) -> Target {
    let plan = Plan {
        lifecycle: Some(
            Scenario::new("mock:lifecycle")
                .expect(Expect::TextContains("conf-hello".into()))
                .expect(Expect::ResultContains("conf-done".into()))
                .expect(Expect::Tool {
                    name: "Bash".into(),
                    class: ActionClass::Command,
                }),
        ),
        approval: Some(ApprovalPlan {
            scenario: Scenario::new("mock:approval")
                .access(AccessMode::Supervised)
                .expect(Expect::ApprovalClass(ActionClass::Command)),
            on_allow: vec![Expect::TextContains("conf-answered".into())],
            on_deny: vec![Expect::TextContains("conf-answered".into())],
            on_deny_message: Vec::new(),
        }),
        input: Some(
            Scenario::new("mock:input").expect(Expect::TextContains("conf-input-answered".into())),
        ),
        steer: Some(
            Scenario::new("mock:steer").expect(Expect::TextContains("conf-steered".into())),
        ),
        cancel: Some(Scenario::new("mock:cancel")),
        crash: Some(Scenario::new("mock:crash")),
        resume: Some(
            Scenario::new("mock:resume")
                .resume("mock-session")
                .expect(Expect::SessionIs("mock-resumed".into())),
        ),
        mcp: Some(
            Scenario::new("mock:mcp")
                .mcp(vec![McpServerSpec {
                    name: "conf-server".into(),
                    command: "/opt/conf/server".into(),
                    args: Vec::new(),
                    env: Default::default(),
                }])
                .expect(Expect::TextContains("conf-mcp".into())),
        ),
        output_schema: None,
        unknown_frames: None,
    };
    Target::new(
        "mock",
        executor,
        Profile::in_process(),
        cwd(),
        Limits::fake(),
    )
    .with_plan(plan)
}

// -- Claude Code -----------------------------------------------------------------------------

pub(crate) fn claude_target() -> Target {
    let executor = ClaudeExecutor::new().with_executable(fixture("fake-claude.sh"));
    let no_history = std::env::temp_dir().join("reno-conformance-no-claude-history");
    let plan = Plan {
        lifecycle: Some(
            Scenario::new("scenario:happy")
                .expect(Expect::TextContains("Hello".into()))
                .expect(Expect::ResultContains("done!".into()))
                .expect(Expect::Tool {
                    name: "Bash".into(),
                    class: ActionClass::Command,
                })
                .expect(Expect::Tool {
                    name: "mcp__linear__search".into(),
                    class: ActionClass::McpTool,
                }),
        ),
        approval: Some(ApprovalPlan {
            scenario: Scenario::new("scenario:approval-subjects")
                .access(AccessMode::Supervised)
                .expect(Expect::ApprovalClass(ActionClass::Command))
                .expect(Expect::ApprovalClass(ActionClass::Edit))
                .expect(Expect::ApprovalClass(ActionClass::McpTool))
                .expect(Expect::ApprovalClass(ActionClass::Network))
                .expect(Expect::ApprovalClass(ActionClass::Read)),
            on_allow: vec![Expect::ResultContains(
                "bash=allow edit=allow mcp=allow net=allow read=allow other=allow".into(),
            )],
            on_deny: vec![Expect::ResultContains(
                "bash=deny:The user did not allow this. edit=deny:The user did not allow this."
                    .into(),
            )],
            on_deny_message: vec![Expect::ResultContains(format!(
                "bash=deny:{DENY_MESSAGE} edit=deny:{DENY_MESSAGE}"
            ))],
        }),
        input: Some(
            Scenario::new("scenario:askuser")
                .access(AccessMode::Supervised)
                .expect(Expect::ResultContains("answered".into())),
        ),
        steer: Some(
            Scenario::new("scenario:steer")
                .expect(Expect::TextContains("steered:redirect please".into())),
        ),
        cancel: Some(Scenario::new("scenario:interrupt")),
        crash: Some(Scenario::new("scenario:crash-mid-turn")),
        resume: Some(
            Scenario::new("scenario:resume-check")
                .env("CLAUDE_CONFIG_DIR", no_history.display().to_string())
                .resume("conf-sess-1")
                .expect(Expect::SessionIs("conf-sess-1".into()))
                .expect(Expect::ResultContains("resumed=conf-sess-1".into())),
        ),
        mcp: Some(
            Scenario::new("scenario:conf-mcp")
                .mcp(vec![McpServerSpec {
                    name: "conf-server".into(),
                    command: "/opt/conf/server".into(),
                    args: vec!["mcp".into()],
                    env: Default::default(),
                }])
                .expect(Expect::ResultContains("mcp:conf-server".into())),
        ),
        output_schema: None,
        unknown_frames: Some(UnknownFramePlan {
            scenario: Scenario::new("scenario:conf-unknown")
                .expect(Expect::ResultContains("unknown-done".into())),
            keys: vec!["type:zz_future_frame".into()],
        }),
    };
    Target::new(
        "claude-code",
        Arc::new(executor),
        Profile {
            deny_message_on_wire: true,
            ..Profile::driver()
        },
        cwd(),
        Limits::fake(),
    )
    .with_plan(plan)
}

// -- Codex -----------------------------------------------------------------------------------

pub(crate) fn codex_target() -> Target {
    let executor = CodexExecutor::new().with_executable(fixture("fake-codex.sh"));
    let plan = Plan {
        lifecycle: Some(
            Scenario::new("scenario:conf-happy")
                .expect(Expect::TextContains("conf-hello".into()))
                .expect(Expect::ResultContains("conf-done".into()))
                .expect(Expect::Tool {
                    name: "exec".into(),
                    class: ActionClass::Command,
                })
                .expect(Expect::Tool {
                    name: "apply_patch".into(),
                    class: ActionClass::Edit,
                }),
        ),
        approval: Some(ApprovalPlan {
            scenario: Scenario::new("scenario:approve")
                .access(AccessMode::Supervised)
                .expect(Expect::ApprovalClass(ActionClass::Command))
                .expect(Expect::ApprovalClass(ActionClass::Edit)),
            on_allow: vec![Expect::ResultContains("command=accept file=accept".into())],
            on_deny: vec![Expect::ResultContains(
                "command=decline file=decline".into(),
            )],
            on_deny_message: Vec::new(),
        }),
        input: Some(
            Scenario::new("scenario:user-input").expect(Expect::ResultContains("mode=safe".into())),
        ),
        steer: Some(Scenario::new("scenario:steer").expect(Expect::TextContains("steered".into()))),
        cancel: Some(Scenario::new("scenario:interrupt")),
        crash: Some(Scenario::new("scenario:crash-after-error")),
        resume: Some(
            Scenario::new("scenario:resumed")
                .resume("th-conf")
                .expect(Expect::SessionIs("th-resumed".into())),
        ),
        mcp: Some(
            Scenario::new("scenario:mcp-config")
                .mcp(vec![
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
                ])
                .expect(Expect::ResultContains("configured".into())),
        ),
        output_schema: None,
        unknown_frames: Some(UnknownFramePlan {
            scenario: Scenario::new("scenario:conf-unknown")
                .expect(Expect::ResultContains("unknown-done".into())),
            keys: vec!["method:zz/futureNotification".into()],
        }),
    };
    Target::new(
        "codex",
        Arc::new(executor),
        Profile::driver(),
        cwd(),
        Limits::fake(),
    )
    .with_plan(plan)
}

// -- ACP -------------------------------------------------------------------------------------

fn acp_plan() -> Plan {
    Plan {
        lifecycle: Some(
            Scenario::new("scenario:conf-happy")
                .expect(Expect::TextContains("conf-hello".into()))
                .expect(Expect::Tool {
                    name: String::new(),
                    class: ActionClass::Command,
                }),
        ),
        approval: Some(ApprovalPlan {
            scenario: Scenario::new("scenario:permission")
                .access(AccessMode::Supervised)
                .expect(Expect::ApprovalClass(ActionClass::Command)),
            on_allow: vec![Expect::TextContains("picked:once".into())],
            on_deny: vec![Expect::TextContains("picked:no".into())],
            on_deny_message: vec![Expect::TextContains(format!("reason:{DENY_MESSAGE}"))],
        }),
        input: Some(
            Scenario::new("scenario:conf-question")
                .expect(Expect::TextContains("answered:opt-".into())),
        ),
        steer: Some(
            Scenario::new("scenario:steer-ext").expect(Expect::TextContains("steered".into())),
        ),
        cancel: Some(Scenario::new("scenario:interrupt")),
        crash: Some(Scenario::new("scenario:conf-crash")),
        resume: Some(
            Scenario::new("scenario:resumed")
                .resume("s-loaded")
                .expect(Expect::SessionIs("s-loaded".into()))
                .expect(Expect::TextContains("back again".into())),
        ),
        mcp: Some(
            Scenario::new("scenario:mcp")
                .mcp(vec![McpServerSpec {
                    name: "reno".into(),
                    command: "/path with spaces/reno".into(),
                    args: vec!["mcp".into()],
                    env: [
                        ("RENO_CHAT_ID".to_owned(), "origin-chat".to_owned()),
                        ("RENO_IPC_PORT".to_owned(), "27699".to_owned()),
                    ]
                    .into(),
                }])
                .expect(Expect::TextContains("mcp configured".into())),
        ),
        output_schema: None,
        unknown_frames: Some(UnknownFramePlan {
            scenario: Scenario::new("scenario:happy"),
            keys: vec![
                "update:totally_unknown_kind".into(),
                "method:some/unknownNotification".into(),
            ],
        }),
    }
}

fn with_model(mut plan: Plan) -> Plan {
    for scenario in [
        plan.lifecycle.as_mut(),
        plan.input.as_mut(),
        plan.steer.as_mut(),
        plan.cancel.as_mut(),
        plan.crash.as_mut(),
        plan.resume.as_mut(),
        plan.mcp.as_mut(),
        plan.approval.as_mut().map(|a| &mut a.scenario),
        plan.unknown_frames.as_mut().map(|u| &mut u.scenario),
    ]
    .into_iter()
    .flatten()
    {
        scenario.model = Some("grok-4.5".into());
    }
    plan
}

/// Grok over the fake agent: it honours full access only, so it never asks for approval.
pub(crate) fn grok_target() -> Target {
    let executor = AcpExecutor::grok().with_executable(fixture("fake-acp.sh"));
    let mut plan = with_model(acp_plan());
    plan.approval = None;
    Target::new(
        "acp.grok",
        Arc::new(executor),
        Profile {
            deny_message_on_wire: true,
            ..Profile::driver()
        },
        cwd(),
        Limits::fake(),
    )
    .with_plan(plan)
}

/// A registry agent over the fake agent: it honours `Supervised` and `FullAccess`.
pub(crate) fn registry_target() -> Target {
    let executor = AcpExecutor::registry(
        RegistryAgent::new("fake-agent").expect("agent id"),
        "Fake agent",
    )
    .expect("registry agent")
    .with_executable(fixture("fake-acp.sh"));
    Target::new(
        "acp.registry",
        Arc::new(executor),
        Profile {
            deny_message_on_wire: true,
            ..Profile::driver()
        },
        cwd(),
        Limits::fake(),
    )
    .with_plan(with_model(acp_plan()))
}

#[allow(dead_code)]
pub(crate) fn session(id: &str) -> SessionRef {
    SessionRef(id.into())
}
