//! The suite tested on executors that break the contract: each must fail the cases that prove
//! the contract, and only those. A suite that cannot fail proves nothing.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::{self, BoxStream};
use reno_harness::conformance::{
    CaseId, Expect, Limits, Plan, Profile, Scenario, Status, Target, TargetReport, run_suite,
};
use reno_harness::exec::{ExecControls, ExecError, Executor, MockExecutor};
use reno_proto::{
    AccessMode, ActionClass, ExecEvent, ExecSpec, ExecutorDescriptor, HealthProbe, SessionRef,
    StopReason, ToolCall,
};

use crate::targets::ScriptedMock;

/// What the broken executor does with a run.
#[derive(Clone, Copy)]
enum Fault {
    /// Started, text, then the stream just ends.
    NoDone,
    /// Started, text, then nothing, and the cancel token is never looked at.
    IgnoresCancel,
    /// Two Done events.
    TwoDones,
    /// A command tool call classified `Other`.
    WrongClass,
    /// Steering is claimed but the run never reacts to a steer.
    DropsSteer,
    /// Accepts every access mode and sandbox level whatever it declares.
    AcceptsEverything,
}

struct Broken {
    descriptor: ExecutorDescriptor,
    fault: Fault,
}

impl Broken {
    fn new(fault: Fault) -> Self {
        let mut descriptor = MockExecutor::default_descriptor("broken".parse().unwrap());
        // Claim little so only the fault is under test.
        descriptor.caps.access_modes = vec![AccessMode::FullAccess];
        descriptor.caps.sandbox_levels = vec![reno_proto::SandboxLevel::Unrestricted];
        descriptor.caps.resume = false;
        descriptor.caps.mcp_injection = false;
        Self { descriptor, fault }
    }
}

fn started() -> ExecEvent {
    ExecEvent::Started {
        session: SessionRef("s".into()),
        model: "m".into(),
    }
}

fn done(stop: StopReason) -> ExecEvent {
    ExecEvent::Done {
        stop,
        result: Some("ok".into()),
        diagnostic: None,
        reason: None,
    }
}

#[async_trait]
impl Executor for Broken {
    fn descriptor(&self) -> ExecutorDescriptor {
        self.descriptor.clone()
    }

    async fn probe(&self) -> HealthProbe {
        MockExecutor::new("broken".parse().unwrap()).probe().await
    }

    async fn start(
        &self,
        spec: ExecSpec,
        controls: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError> {
        if !matches!(self.fault, Fault::AcceptsEverything) {
            if !self.descriptor.caps.access_modes.contains(&spec.access) {
                return Err(ExecError::UnsupportedAccess {
                    requested: spec.access,
                    supported: self.descriptor.caps.access_modes.clone(),
                });
            }
            if !self.descriptor.caps.sandbox_levels.contains(&spec.sandbox) {
                return Err(ExecError::InvalidSpec(format!(
                    "sandbox level {:?} is not supported",
                    spec.sandbox
                )));
            }
            if spec.output_schema.is_some() {
                return Err(ExecError::UnsupportedOutputSchema);
            }
        }
        let text = ExecEvent::Text { text: "hi".into() };
        let tool = |class| ExecEvent::Tool {
            id: "t".into(),
            call: ToolCall::new("Bash", serde_json::json!({}), class),
        };
        let events: Vec<ExecEvent> = match self.fault {
            Fault::NoDone => vec![started(), text],
            Fault::IgnoresCancel => {
                let head = stream::iter(vec![started(), text]);
                // Keep the controls alive and never look at them.
                let tail = stream::once(async move {
                    let _controls = controls;
                    futures::future::pending::<ExecEvent>().await
                });
                return Ok(head.chain(tail).boxed());
            }
            Fault::TwoDones => vec![
                started(),
                text,
                done(StopReason::Completed),
                done(StopReason::Completed),
            ],
            Fault::WrongClass => vec![
                started(),
                text,
                tool(ActionClass::Other),
                ExecEvent::ToolResult {
                    id: "t".into(),
                    ok: true,
                    summary: None,
                },
                done(StopReason::Completed),
            ],
            Fault::DropsSteer | Fault::AcceptsEverything => {
                vec![started(), text, done(StopReason::Completed)]
            }
        };
        Ok(stream::iter(events).boxed())
    }
}

fn limits() -> Limits {
    Limits {
        case: Duration::from_secs(2),
        cancel: Duration::from_millis(400),
        total: Duration::from_secs(60),
    }
}

fn target(fault: Fault) -> Target {
    let plan = Plan {
        lifecycle: Some(Scenario::new("x").expect(Expect::Tool {
            name: "Bash".into(),
            class: ActionClass::Command,
        })),
        cancel: Some(Scenario::new("x")),
        steer: Some(Scenario::new("x").expect(Expect::TextContains("steered".into()))),
        ..Plan::default()
    };
    Target::new(
        "broken",
        Arc::new(Broken::new(fault)),
        Profile::in_process(),
        std::env::temp_dir(),
        limits(),
    )
    .with_plan(plan)
}

fn status(report: &TargetReport, case: CaseId) -> &Status {
    report.status(case).unwrap()
}

fn failed(report: &TargetReport, case: CaseId) -> bool {
    matches!(status(report, case), Status::Fail(_))
}

#[tokio::test]
async fn a_stream_without_done_fails_lifecycle_and_cancel() {
    let report = run_suite(&target(Fault::NoDone)).await;
    assert!(
        failed(&report, CaseId::Lifecycle),
        "{:?}",
        report.failures()
    );
    assert!(failed(&report, CaseId::Cancel));
    assert!(
        failed(&report, CaseId::Claims),
        "a claim rests on a failed case"
    );
}

#[tokio::test]
async fn a_run_that_ignores_cancel_fails_within_the_bound() {
    let started = std::time::Instant::now();
    let report = run_suite(&target(Fault::IgnoresCancel)).await;
    assert!(failed(&report, CaseId::Cancel), "{:?}", report.failures());
    let Status::Fail(why) = status(&report, CaseId::Cancel) else {
        unreachable!()
    };
    assert!(why.contains("cancel"), "{why}");
    // Every case that waits on the run ends at its own bound, so the suite always ends.
    assert!(started.elapsed() < Duration::from_secs(60));
}

#[tokio::test]
async fn two_done_events_fail_lifecycle() {
    let report = run_suite(&target(Fault::TwoDones)).await;
    let Status::Fail(why) = status(&report, CaseId::Lifecycle) else {
        panic!("{:?}", report.labels());
    };
    assert!(why.contains("Done"), "{why}");
}

#[tokio::test]
async fn a_wrong_tool_class_fails_tool_classes_only() {
    let report = run_suite(&target(Fault::WrongClass)).await;
    assert!(
        failed(&report, CaseId::ToolClasses),
        "{:?}",
        report.failures()
    );
    assert!(matches!(status(&report, CaseId::Lifecycle), Status::Pass));
}

#[tokio::test]
async fn a_claimed_steer_that_never_lands_fails() {
    let report = run_suite(&target(Fault::DropsSteer)).await;
    assert!(failed(&report, CaseId::Steer), "{:?}", report.failures());
    assert!(failed(&report, CaseId::Claims));
}

#[tokio::test]
async fn accepting_what_the_descriptor_does_not_declare_fails_the_refusal_cases() {
    let report = run_suite(&target(Fault::AcceptsEverything)).await;
    assert!(
        failed(&report, CaseId::AccessModes),
        "{:?}",
        report.failures()
    );
    assert!(failed(&report, CaseId::SandboxLevels));
    assert!(failed(&report, CaseId::OutputSchema));
}

#[tokio::test]
async fn a_claim_with_no_scenario_is_a_failure_unless_waived() {
    // The mock claims steering; give the target no steer scenario.
    let mut mock = crate::targets::mock_target();
    mock.plan.steer = None;
    let report = run_suite(&mock).await;
    let Status::Fail(why) = status(&report, CaseId::Steer) else {
        panic!("{:?}", report.labels());
    };
    assert!(why.contains("ARCH-15"), "{why}");
    assert!(failed(&report, CaseId::Claims));

    // Waived is reported as waived, the claim as unverified: never as a pass.
    let mut mock = crate::targets::mock_target().waive(CaseId::Steer, "cannot be provoked");
    mock.plan.steer = None;
    let report = run_suite(&mock).await;
    assert_eq!(
        status(&report, CaseId::Steer),
        &Status::Waived("cannot be provoked".into())
    );
    assert!(matches!(status(&report, CaseId::Claims), Status::Waived(_)));
    assert!(report.is_green(), "{:?}", report.failures());
}

#[tokio::test]
async fn a_resume_that_starts_another_session_fails_the_resume_case() {
    let mut target = crate::targets::mock_target();
    target.plan.resume = Some(Scenario::new("mock:resume-forks").resume("mock-session"));
    let report = run_suite(&target).await;
    assert!(failed(&report, CaseId::Resume), "{:?}", report.labels());
    let Status::Fail(why) = status(&report, CaseId::Resume) else {
        unreachable!("failed above");
    };
    assert!(why.contains("must continue the session"), "{why}");
}

#[tokio::test]
async fn a_case_the_descriptor_does_not_claim_is_skipped_not_passed() {
    let mut descriptor = MockExecutor::default_descriptor("mock".parse().unwrap());
    descriptor.caps.steering = reno_proto::Steering::None;
    descriptor.caps.resume = false;
    let executor = ScriptedMock::with_descriptor(Some(descriptor));
    let report = run_suite(&crate::targets::mock_target_for(Arc::new(executor))).await;
    assert!(matches!(status(&report, CaseId::Steer), Status::Skip(_)));
    assert!(matches!(status(&report, CaseId::Resume), Status::Skip(_)));
    assert!(report.is_green(), "{:?}", report.failures());
}

#[tokio::test]
async fn a_claimed_plan_mode_is_a_failure_because_nothing_can_exercise_it() {
    let mut descriptor = MockExecutor::default_descriptor("mock".parse().unwrap());
    descriptor.caps.plan_mode = true;
    let executor = ScriptedMock::with_descriptor(Some(descriptor));
    let report = run_suite(&crate::targets::mock_target_for(Arc::new(executor))).await;
    assert!(failed(&report, CaseId::PlanMode), "{:?}", report.labels());
    assert!(failed(&report, CaseId::Claims));
}

#[tokio::test]
async fn the_mock_sees_the_decisions_the_suite_sent() {
    let mock = ScriptedMock::new();
    let log_of = mock.base.clone();
    let target = crate::targets::mock_target_for(Arc::new(mock));
    let report = run_suite(&target).await;
    assert!(report.is_green(), "{:?}", report.failures());
    let log = log_of.log();
    let decisions: Vec<_> = log
        .answers
        .iter()
        .filter_map(|a| match &a.answer {
            reno_harness::exec::Answer::Decision(decision, message) => {
                Some((*decision, message.clone()))
            }
            reno_harness::exec::Answer::Input(_) => None,
        })
        .collect();
    use reno_harness::exec::Decision;
    assert!(decisions.iter().any(|(d, _)| *d == Decision::Allow));
    assert!(
        decisions
            .iter()
            .any(|(d, m)| *d == Decision::Deny && m.is_none())
    );
    assert_eq!(
        log.steers,
        vec![reno_harness::conformance::STEER_TEXT.to_owned()]
    );
}
