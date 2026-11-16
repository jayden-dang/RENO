//! The mode runs of the suite (T1.14): what an access mode or a sandbox level really does, shown
//! by a run, tested on an executor that does it right and on two that do it wrong.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use reno_harness::conformance::{
    ApprovalAnswer, ApprovalPlan, CaseId, Expect, Limits, ModeRun, Plan, Profile, Proof, Scenario,
    Status, Target, TargetReport, run_suite,
};
use reno_harness::exec::{Answer, Decision, ExecControls, ExecError, Executor, MockExecutor};
use reno_proto::{
    AccessMode, ActionClass, ApprovalSubject, ExecEvent, ExecSpec, ExecutorDescriptor, HealthProbe,
    SandboxLevel, SessionRef, StopReason, ToolCall,
};
use tokio::sync::mpsc;

/// An executor that follows its access mode and sandbox the way the real CLIs are meant to, and
/// can be told to get one of them wrong.
struct ModeFake {
    descriptor: ExecutorDescriptor,
    /// Writes land whatever the sandbox says.
    leaky_sandbox: bool,
    /// An edit asks for approval under `AutoAcceptEdits`.
    asks_for_edits: bool,
}

impl ModeFake {
    fn new(leaky_sandbox: bool, asks_for_edits: bool) -> Self {
        let mut descriptor = MockExecutor::default_descriptor("modes".parse().unwrap());
        descriptor.caps.access_modes = vec![
            AccessMode::Supervised,
            AccessMode::AutoAcceptEdits,
            AccessMode::FullAccess,
        ];
        descriptor.caps.sandbox_levels = vec![
            SandboxLevel::ReadOnly,
            SandboxLevel::WorkspaceWrite,
            SandboxLevel::Unrestricted,
        ];
        descriptor.caps.steering = reno_proto::Steering::None;
        descriptor.caps.resume = false;
        descriptor.caps.mcp_injection = false;
        Self {
            descriptor,
            leaky_sandbox,
            asks_for_edits,
        }
    }
}

fn subject(action: ActionClass, tool: &str, what: &str) -> ApprovalSubject {
    ApprovalSubject {
        action,
        tool: tool.into(),
        command: Some(what.into()),
        paths: Vec::new(),
        raw: serde_json::json!({ "what": what }),
    }
}

async fn tool(tx: &mpsc::Sender<ExecEvent>, name: &str, class: ActionClass) {
    let _ = tx
        .send(ExecEvent::Tool {
            id: "t".into(),
            call: ToolCall::new(name, serde_json::json!({}), class),
        })
        .await;
}

/// Asks for approval and waits for the answer.
async fn ask(
    tx: &mpsc::Sender<ExecEvent>,
    controls: &mut ExecControls,
    subject: ApprovalSubject,
) -> bool {
    let _ = tx
        .send(ExecEvent::NeedsApproval {
            request_id: "r1".into(),
            subject,
        })
        .await;
    matches!(
        controls.answers.recv().await.map(|a| a.answer),
        Some(Answer::Decision(Decision::Allow, _))
    )
}

async fn play(
    spec: ExecSpec,
    mut controls: ExecControls,
    tx: mpsc::Sender<ExecEvent>,
    leaky: bool,
    asks_for_edits: bool,
) {
    let _ = tx
        .send(ExecEvent::Started {
            session: SessionRef("s".into()),
            model: "m".into(),
        })
        .await;
    let _ = tx
        .send(ExecEvent::Text {
            text: "hello".into(),
        })
        .await;
    let (kind, arg) = spec.prompt.split_once(':').unwrap_or((&spec.prompt, ""));
    let mut stop = StopReason::Completed;
    match kind {
        "wait" => {
            controls.cancel.cancelled().await;
            stop = StopReason::Interrupted;
        }
        "edit" => {
            tool(&tx, "Edit", ActionClass::Edit).await;
            let asks = spec.access == AccessMode::Supervised
                || (asks_for_edits && spec.access == AccessMode::AutoAcceptEdits);
            if !asks || ask(&tx, &mut controls, subject(ActionClass::Edit, "Edit", arg)).await {
                let _ = std::fs::write(spec.cwd.join(arg), "edited\n");
            }
        }
        "shell" => {
            tool(&tx, "Bash", ActionClass::Command).await;
            let asks = spec.access != AccessMode::FullAccess;
            if !asks
                || ask(
                    &tx,
                    &mut controls,
                    subject(ActionClass::Command, "Bash", arg),
                )
                .await
            {
                let _ = std::fs::write(spec.cwd.join(arg), "ran\n");
            }
        }
        "sandbox" => {
            tool(&tx, "Bash", ActionClass::Command).await;
            for target in arg.split('|') {
                let path = spec.cwd.join(target);
                let inside = path.starts_with(&spec.cwd);
                let lands = leaky
                    || match spec.sandbox {
                        SandboxLevel::ReadOnly => false,
                        SandboxLevel::WorkspaceWrite => inside,
                        SandboxLevel::Unrestricted => true,
                    };
                if lands {
                    let _ = std::fs::write(path, "wrote\n");
                }
            }
        }
        _ => {
            tool(&tx, "Bash", ActionClass::Command).await;
            let _ = tx
                .send(ExecEvent::ToolResult {
                    id: "t".into(),
                    ok: true,
                    summary: None,
                })
                .await;
        }
    }
    let _ = tx
        .send(ExecEvent::Done {
            stop,
            result: Some("ok".into()),
            diagnostic: None,
            reason: None,
        })
        .await;
}

#[async_trait]
impl Executor for ModeFake {
    fn descriptor(&self) -> ExecutorDescriptor {
        self.descriptor.clone()
    }

    async fn probe(&self) -> HealthProbe {
        MockExecutor::new("modes".parse().unwrap()).probe().await
    }

    async fn start(
        &self,
        spec: ExecSpec,
        controls: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError> {
        if !self.descriptor.caps.access_modes.contains(&spec.access) {
            return Err(ExecError::UnsupportedAccess {
                requested: spec.access,
                supported: self.descriptor.caps.access_modes.clone(),
            });
        }
        if spec.output_schema.is_some() {
            return Err(ExecError::UnsupportedOutputSchema);
        }
        let (tx, rx) = mpsc::channel(16);
        let (leaky, asks) = (self.leaky_sandbox, self.asks_for_edits);
        tokio::spawn(play(spec, controls, tx, leaky, asks));
        Ok(stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|e| (e, rx)) }).boxed())
    }
}

fn mode_runs(outside: &Path) -> Vec<ModeRun> {
    let outside = outside.join("out.txt");
    vec![
        ModeRun {
            proves: Proof::Access(AccessMode::AutoAcceptEdits),
            scenario: Scenario::new("edit:auto-edit.txt")
                .access(AccessMode::AutoAcceptEdits)
                .fresh("auto-edit.txt")
                .expect(Expect::Tool {
                    name: String::new(),
                    class: ActionClass::Edit,
                }),
            answer: ApprovalAnswer::Allow,
            expect: vec![
                Expect::NoApprovalOf(ActionClass::Edit),
                Expect::FileContains {
                    path: "auto-edit.txt".into(),
                    text: "edited".into(),
                },
            ],
        },
        ModeRun {
            proves: Proof::Access(AccessMode::AutoAcceptEdits),
            scenario: Scenario::new("shell:auto-shell.txt")
                .access(AccessMode::AutoAcceptEdits)
                .fresh("auto-shell.txt"),
            answer: ApprovalAnswer::Deny,
            expect: vec![
                Expect::ApprovalClass(ActionClass::Command),
                Expect::PathAbsent("auto-shell.txt".into()),
            ],
        },
        ModeRun {
            proves: Proof::Sandbox(SandboxLevel::ReadOnly),
            scenario: Scenario::new("sandbox:ro.txt")
                .fresh("ro.txt")
                .sandbox(SandboxLevel::ReadOnly),
            answer: ApprovalAnswer::Deny,
            expect: vec![Expect::PathAbsent("ro.txt".into())],
        },
        ModeRun {
            proves: Proof::Sandbox(SandboxLevel::WorkspaceWrite),
            scenario: Scenario::new(format!("sandbox:ws.txt|{}", outside.display()))
                .fresh("ws.txt")
                .sandbox(SandboxLevel::WorkspaceWrite),
            answer: ApprovalAnswer::Deny,
            expect: vec![
                Expect::PathExists("ws.txt".into()),
                Expect::PathAbsent(outside.display().to_string()),
            ],
        },
    ]
}

async fn run(fake: ModeFake, with_mode_runs: bool) -> TargetReport {
    let cwd = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let plan = Plan {
        lifecycle: Some(
            Scenario::new("plain:")
                .expect(Expect::TextContains("hello".into()))
                .expect(Expect::Tool {
                    name: "Bash".into(),
                    class: ActionClass::Command,
                }),
        ),
        approval: Some(ApprovalPlan {
            scenario: Scenario::new("shell:approval.txt")
                .access(AccessMode::Supervised)
                .fresh("approval.txt")
                .expect(Expect::ApprovalClass(ActionClass::Command)),
            on_allow: vec![Expect::PathExists("approval.txt".into())],
            on_deny: vec![Expect::PathAbsent("approval.txt".into())],
            on_deny_message: Vec::new(),
        }),
        cancel: Some(Scenario::new("wait:")),
        mode_runs: if with_mode_runs {
            mode_runs(outside.path())
        } else {
            Vec::new()
        },
        ..Plan::default()
    };
    let mut target = Target::new(
        "modes",
        Arc::new(fake),
        Profile::in_process(),
        cwd.path().to_path_buf(),
        Limits::fake(),
    )
    .with_plan(plan)
    .waive(CaseId::Input, "not driven here")
    .waive(CaseId::Crash, "not driven here");
    target.exhaustive = false;
    run_suite(&target).await
}

fn status(report: &TargetReport, case: CaseId) -> &Status {
    report.status(case).unwrap()
}

#[tokio::test]
async fn mode_runs_turn_the_waived_modes_into_passes() {
    let report = run(ModeFake::new(false, false), true).await;
    assert!(report.is_green(), "{:?}", report.failures());
    assert_eq!(status(&report, CaseId::AccessModes), &Status::Pass);
    assert_eq!(status(&report, CaseId::SandboxLevels), &Status::Pass);
    assert_eq!(status(&report, CaseId::Claims), &Status::Pass);
}

#[tokio::test]
async fn without_mode_runs_the_modes_stay_waived_and_the_claim_unverified() {
    let report = run(ModeFake::new(false, false), false).await;
    assert!(report.is_green(), "{:?}", report.failures());
    let Status::Waived(access) = status(&report, CaseId::AccessModes) else {
        panic!("{:?}", status(&report, CaseId::AccessModes));
    };
    // Supervised is what the approval cases ran in, so only the other mode is waived.
    assert!(access.contains("AutoAcceptEdits"), "{access}");
    assert!(!access.contains("Supervised"), "{access}");
    let Status::Waived(sandbox) = status(&report, CaseId::SandboxLevels) else {
        panic!("{:?}", status(&report, CaseId::SandboxLevels));
    };
    assert!(
        sandbox.contains("ReadOnly") && sandbox.contains("WorkspaceWrite"),
        "{sandbox}"
    );
    assert!(matches!(status(&report, CaseId::Claims), Status::Waived(_)));
}

#[tokio::test]
async fn a_sandbox_that_lets_a_write_through_fails_the_case() {
    let report = run(ModeFake::new(true, false), true).await;
    let Status::Fail(why) = status(&report, CaseId::SandboxLevels) else {
        panic!("{:?}", status(&report, CaseId::SandboxLevels));
    };
    assert!(why.contains("ReadOnly") && why.contains("ro.txt"), "{why}");
    assert!(matches!(status(&report, CaseId::Claims), Status::Fail(_)));
}

#[tokio::test]
async fn an_edit_that_asks_under_auto_accept_edits_fails_the_case() {
    let report = run(ModeFake::new(false, true), true).await;
    let Status::Fail(why) = status(&report, CaseId::AccessModes) else {
        panic!("{:?}", status(&report, CaseId::AccessModes));
    };
    assert!(
        why.contains("AutoAcceptEdits") && why.contains("unasked"),
        "{why}"
    );
}

#[tokio::test]
async fn a_mode_run_that_runs_in_another_mode_is_a_broken_plan() {
    let cwd = tempfile::tempdir().unwrap();
    let plan = Plan {
        lifecycle: Some(Scenario::new("plain:")),
        mode_runs: vec![ModeRun {
            proves: Proof::Access(AccessMode::AutoAcceptEdits),
            scenario: Scenario::new("plain:").access(AccessMode::Supervised),
            answer: ApprovalAnswer::Allow,
            expect: vec![Expect::TextContains("hello".into())],
        }],
        ..Plan::default()
    };
    let mut target = Target::new(
        "modes",
        Arc::new(ModeFake::new(false, false)),
        Profile::in_process(),
        cwd.path().to_path_buf(),
        Limits::fake(),
    )
    .with_plan(plan);
    target.exhaustive = false;
    let report = run_suite(&target).await;
    let Status::Fail(why) = status(&report, CaseId::AccessModes) else {
        panic!("{:?}", status(&report, CaseId::AccessModes));
    };
    assert!(why.contains("proves"), "{why}");
}

#[test]
fn the_estimate_counts_each_scenario_once_and_the_approval_twice() {
    let plan = Plan {
        lifecycle: Some(Scenario::new("a")),
        approval: Some(ApprovalPlan {
            scenario: Scenario::new("b"),
            on_allow: Vec::new(),
            on_deny: Vec::new(),
            on_deny_message: Vec::new(),
        }),
        cancel: Some(Scenario::new("c")),
        mode_runs: vec![
            ModeRun {
                proves: Proof::Sandbox(SandboxLevel::ReadOnly),
                scenario: Scenario::new("d"),
                answer: ApprovalAnswer::Deny,
                expect: Vec::new(),
            };
            3
        ],
        ..Plan::default()
    };
    // lifecycle + cancel + 2 approvals + 3 mode runs + the two baselines of the mode cases.
    assert_eq!(plan.estimated_runs(), 1 + 1 + 2 + 3 + 2);
    assert_eq!(Plan::default().estimated_runs(), 0);
}

#[tokio::test]
async fn the_report_serialises_every_case_with_its_status_and_reason() {
    let report = run(ModeFake::new(false, false), false).await;
    let json = report.to_json();
    assert_eq!(json["target"], "modes");
    let cases = json["cases"].as_array().unwrap();
    assert_eq!(cases.len(), CaseId::ALL.len());
    let access = cases.iter().find(|c| c["case"] == "access-modes").unwrap();
    assert_eq!(access["status"], "waived");
    assert!(
        access["reason"]
            .as_str()
            .unwrap()
            .contains("AutoAcceptEdits")
    );
    let lifecycle = cases.iter().find(|c| c["case"] == "lifecycle").unwrap();
    assert_eq!(lifecycle["status"], "pass");
    assert!(lifecycle["reason"].is_null());
    assert!(json["unknownFrames"].as_object().unwrap().is_empty());
}
