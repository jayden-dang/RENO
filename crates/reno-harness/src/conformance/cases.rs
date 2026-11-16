//! The cases. Each one owns its assertions; the [`Target`] only supplies data.

use std::collections::BTreeMap;
use std::time::Instant;

use reno_proto::{
    AccessMode, ExecEvent, ExecutorCaps, ExecutorDescriptor, HealthProbe, InstallStatus,
    SandboxLevel, SignedIn, Steering, StopReason, VersionGate,
};

use super::drive::{Behaviour, RunLog};
use super::report::{CaseResult, Status, TargetReport};
use super::{CaseId, Expect, Scenario, Target};
use crate::exec::{Decision, ExecError};

/// Why a case did not pass.
enum Failure {
    /// The target has no scenario for something the case needs. A waiver can excuse it.
    Unplanned(String),
    /// Something did not hold.
    Broken(String),
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::Broken(message)
    }
}

impl From<&str> for Failure {
    fn from(message: &str) -> Self {
        Self::Broken(message.to_owned())
    }
}

/// How a case that did not fail ended.
enum Outcome {
    Pass,
    /// Passed in part: the rest could not be exercised on this target.
    Waived(String),
}

type CaseResultOf = Result<Outcome, Failure>;

/// Run every case against `target` and report. Never panics and never hangs: each case is
/// bounded by the target's limits, and the whole run by its total.
pub async fn run_suite(target: &Target) -> TargetReport {
    let mut cx = Cx {
        target,
        descriptor: target.executor.descriptor(),
        statuses: BTreeMap::new(),
        lifecycle_log: None,
        lifecycle_unknown: None,
    };
    let started = Instant::now();
    let mut results = Vec::with_capacity(CaseId::ALL.len());
    for case in CaseId::ALL {
        let begun = Instant::now();
        let status = if started.elapsed() > target.limits.total {
            Status::Fail(format!(
                "not run: the total budget of {:?} was spent",
                target.limits.total
            ))
        } else {
            cx.run_case(case).await
        };
        cx.statuses.insert(case, status.clone());
        results.push(CaseResult {
            case,
            status,
            took: begun.elapsed(),
        });
    }
    TargetReport {
        target: target.name.clone(),
        results,
        unknown_frames: target.executor.unknown_frames(),
    }
}

struct Cx<'a> {
    target: &'a Target,
    descriptor: ExecutorDescriptor,
    statuses: BTreeMap<CaseId, Status>,
    /// The plain run of the lifecycle case, reused by the cases that only look at it.
    lifecycle_log: Option<RunLog>,
    /// The counters the plain run raised.
    lifecycle_unknown: Option<BTreeMap<String, u64>>,
}

impl Cx<'_> {
    fn caps(&self) -> &ExecutorCaps {
        &self.descriptor.caps
    }

    /// Why `case` does not apply to this target, if it does not.
    fn skip_reason(&self, case: CaseId) -> Option<&'static str> {
        let caps = self.caps();
        let asks = caps.access_modes.contains(&AccessMode::Supervised);
        match case {
            CaseId::ProcessRecords if !self.target.profile.spawns_process => {
                Some("the executor spawns no process")
            }
            CaseId::ApprovalAllow | CaseId::ApprovalDeny if !asks => {
                Some("does not claim Supervised access, so it never asks")
            }
            CaseId::DenyMessage if !asks => Some("does not claim Supervised access"),
            CaseId::DenyMessage if !self.target.profile.deny_message_on_wire => {
                Some("its wire has no place for a deny message")
            }
            CaseId::Steer if caps.steering == Steering::None => Some("claims no steering"),
            CaseId::Resume if !caps.resume => Some("claims no resume"),
            CaseId::Mcp if !caps.mcp_injection => Some("claims no MCP injection"),
            CaseId::PlanMode if !caps.plan_mode => Some("claims no plan mode"),
            CaseId::UnknownFrames if !self.target.profile.counts_unknown_frames => {
                Some("does not count unrecognised frames")
            }
            _ => None,
        }
    }

    async fn run_case(&mut self, case: CaseId) -> Status {
        if let Some(why) = self.skip_reason(case) {
            return Status::Skip(why.to_owned());
        }
        let bound = self.target.limits.case;
        let ran = tokio::time::timeout(bound, self.dispatch(case)).await;
        match ran {
            Err(_) => Status::Fail(format!("did not finish within {bound:?}")),
            Ok(Ok(Outcome::Pass)) => Status::Pass,
            Ok(Ok(Outcome::Waived(why))) => Status::Waived(why),
            Ok(Err(Failure::Broken(why))) => Status::Fail(why),
            Ok(Err(Failure::Unplanned(why))) => match self.target.waiver(case) {
                Some(waiver) => Status::Waived(waiver.to_owned()),
                None => Status::Fail(why),
            },
        }
    }

    async fn dispatch(&mut self, case: CaseId) -> CaseResultOf {
        match case {
            CaseId::Probe => self.probe().await,
            CaseId::Lifecycle => self.lifecycle().await,
            CaseId::ToolClasses => self.tool_classes().await,
            CaseId::ProcessRecords => self.process_records().await,
            CaseId::ApprovalAllow => self.approval(Decision::Allow, false).await,
            CaseId::ApprovalDeny => self.approval(Decision::Deny, false).await,
            CaseId::DenyMessage => self.approval(Decision::Deny, true).await,
            CaseId::Input => self.input().await,
            CaseId::Steer => self.steer().await,
            CaseId::Cancel => self.cancel().await,
            CaseId::Crash => self.crash().await,
            CaseId::Resume => self.resume().await,
            CaseId::Mcp => self.mcp().await,
            CaseId::AccessModes => self.access_modes().await,
            CaseId::SandboxLevels => self.sandbox_levels().await,
            CaseId::OutputSchema => self.output_schema().await,
            CaseId::PlanMode => Err(Failure::Broken(
                "claims a plan mode, but ExecSpec has no way to request one, so nothing can \
                 exercise it: never declare what a caller cannot use (ARCH-15)"
                    .into(),
            )),
            CaseId::UnknownFrames => self.unknown_frames().await,
            CaseId::Claims => self.claims(),
        }
    }

    /// A scenario the target must supply for `what`.
    fn need<'t, T>(&self, plan: &'t Option<T>, what: &str) -> Result<&'t T, Failure> {
        plan.as_ref().ok_or_else(|| {
            Failure::Unplanned(format!(
                "no {what} scenario: the target cannot exercise it, so what the descriptor claims about it is unproven (ARCH-15)"
            ))
        })
    }

    /// Run `scenario` as `behaviour` says and require it to complete.
    async fn run_completed(
        &self,
        scenario: &Scenario,
        behaviour: Behaviour,
    ) -> Result<RunLog, Failure> {
        let log = self
            .target
            .drive(self.target.spec(scenario)?, behaviour)
            .await?;
        require_one_done(&log)?;
        match log.done() {
            Some((StopReason::Completed, _, _)) => Ok(log),
            other => Err(Failure::Broken(format!(
                "the run should complete, but ended {other:?}"
            ))),
        }
    }

    // -- probe ---------------------------------------------------------------------------

    async fn probe(&mut self) -> CaseResultOf {
        let descriptor = &self.descriptor;
        if descriptor.id.as_str().is_empty() || descriptor.contract_version == 0 {
            return Err("the descriptor has no id or contract version".into());
        }
        let caps = &descriptor.caps;
        if caps.access_modes.is_empty() || caps.sandbox_levels.is_empty() {
            return Err("the descriptor declares no access mode or no sandbox level".into());
        }
        unique_models(caps.models.iter().map(|m| m.id.as_str()), "descriptor")?;
        if let (Ok(min), Ok(verified)) = (
            semver::Version::parse(&descriptor.cli_verified.min),
            semver::Version::parse(&descriptor.cli_verified.verified),
        ) && min > verified
        {
            return Err("cli_verified.min is newer than cli_verified.verified".into());
        }
        let probe = self.target.executor.probe().await;
        check_probe(&probe, &descriptor.cli_verified)?;
        let again = self.target.executor.probe().await;
        if again.installed.version() != probe.installed.version() {
            return Err("two probes in a row disagree about the version".into());
        }
        Ok(Outcome::Pass)
    }

    // -- lifecycle -----------------------------------------------------------------------

    async fn lifecycle(&mut self) -> CaseResultOf {
        let scenario = self.need(&self.target.plan.lifecycle, "lifecycle")?;
        let before = self.target.executor.unknown_frames();
        let log = self.run_completed(scenario, Behaviour::Passive).await?;
        self.lifecycle_unknown = Some(counter_diff(
            &before,
            &self.target.executor.unknown_frames(),
        ));
        check_started_first(&log)?;
        if !log
            .events
            .iter()
            .any(|e| matches!(e, ExecEvent::Text { .. }))
        {
            return Err("the run emitted no Text".into());
        }
        log.check_all(
            scenario
                .expect
                .iter()
                .filter(|e| !matches!(e, Expect::Tool { .. })),
        )?;
        self.lifecycle_log = Some(log);
        Ok(Outcome::Pass)
    }

    /// The plain run: the lifecycle case's, or a fresh one when that case did not leave one.
    async fn plain_run(&mut self) -> Result<(), Failure> {
        if self.lifecycle_log.is_some() {
            return Ok(());
        }
        let scenario = self.need(&self.target.plan.lifecycle, "lifecycle")?;
        let log = self.run_completed(scenario, Behaviour::Passive).await?;
        self.lifecycle_log = Some(log);
        Ok(())
    }

    async fn tool_classes(&mut self) -> CaseResultOf {
        self.plain_run().await?;
        let scenario = self.need(&self.target.plan.lifecycle, "lifecycle")?;
        let Some(log) = &self.lifecycle_log else {
            return Err("no run to read".into());
        };
        let mut open = Vec::new();
        let mut tools = 0;
        for event in &log.events {
            match event {
                ExecEvent::Tool { id, call } => {
                    tools += 1;
                    if call.name.is_empty() {
                        return Err(format!("tool call {id} has no name").into());
                    }
                    open.push(id.as_str());
                }
                ExecEvent::ToolResult { id, .. } if !open.contains(&id.as_str()) => {
                    return Err(format!("ToolResult {id} answers no earlier Tool").into());
                }
                _ => {}
            }
        }
        if tools == 0 {
            return Err("the run made no tool call".into());
        }
        let expected: Vec<&Expect> = scenario
            .expect
            .iter()
            .filter(|e| matches!(e, Expect::Tool { .. }))
            .collect();
        if expected.is_empty() {
            return Err(Failure::Unplanned(
                "the lifecycle scenario declares no expected tool class".into(),
            ));
        }
        log.check_all(expected)?;
        Ok(Outcome::Pass)
    }

    async fn process_records(&mut self) -> CaseResultOf {
        self.plain_run().await?;
        let Some(log) = &self.lifecycle_log else {
            return Err("no run to read".into());
        };
        if log.records.is_empty() {
            return Err("the run spawned a child but reported no ProcessRecord".into());
        }
        for record in &log.records {
            if record.pid <= 1 || record.pgid != record.pid {
                return Err(format!(
                    "record {record:?}: a child is reported as the leader of its own process group"
                )
                .into());
            }
        }
        Target::processes_gone(&log.records, self.target.limits.cancel).await?;
        Ok(Outcome::Pass)
    }

    // -- asking --------------------------------------------------------------------------

    async fn approval(&mut self, decision: Decision, with_message: bool) -> CaseResultOf {
        let plan = self.need(&self.target.plan.approval, "approval")?;
        let behaviour = match decision {
            Decision::Allow => Behaviour::Decide(Decision::Allow, None),
            Decision::Deny => Behaviour::deny(with_message),
        };
        let log = self.run_completed(&plan.scenario, behaviour).await?;
        if log.approvals.is_empty() {
            return Err("the run never asked for an approval".into());
        }
        for subject in &log.approvals {
            if subject.tool.is_empty() {
                return Err("an approval subject names no tool".into());
            }
            if subject.command.is_none() && subject.paths.is_empty() && subject.raw.is_null() {
                return Err(format!(
                    "approval subject {:?} carries no command, no paths and no raw request",
                    subject.tool
                )
                .into());
            }
        }
        let expect = match (decision, with_message) {
            (Decision::Allow, _) => &plan.on_allow,
            (Decision::Deny, false) => &plan.on_deny,
            (Decision::Deny, true) => {
                if plan.on_deny_message.is_empty() {
                    return Err(Failure::Unplanned(
                        "no expectation shows the deny message reaching the agent".into(),
                    ));
                }
                &plan.on_deny_message
            }
        };
        if expect.is_empty() {
            return Err(Failure::Unplanned(
                "the approval scenario declares no expectation of the decision".into(),
            ));
        }
        log.check_all(expect.iter().chain(&plan.scenario.expect))?;
        Ok(Outcome::Pass)
    }

    async fn input(&mut self) -> CaseResultOf {
        let scenario = self.need(&self.target.plan.input, "input")?;
        let log = self.run_completed(scenario, Behaviour::Passive).await?;
        if log.questions.is_empty() {
            return Err("the run never asked a question".into());
        }
        for question in log.questions.iter().flatten() {
            if question.id.is_empty() || question.prompt.is_empty() {
                return Err(format!("question {question:?} has no id or no text").into());
            }
        }
        require_expectations(&log, scenario)
    }

    async fn steer(&mut self) -> CaseResultOf {
        let scenario = self.need(&self.target.plan.steer, "steer")?;
        let log = self.run_completed(scenario, Behaviour::Steer).await?;
        if !log.steered {
            return Err("the run emitted no Text to steer after".into());
        }
        require_expectations(&log, scenario)
    }

    // -- ending --------------------------------------------------------------------------

    async fn cancel(&mut self) -> CaseResultOf {
        let scenario = self.need(&self.target.plan.cancel, "cancel")?;
        let log = self
            .target
            .drive(self.target.spec(scenario)?, Behaviour::CancelOnStart)
            .await?;
        require_one_done(&log)?;
        match log.done() {
            Some((StopReason::Interrupted, _, _)) => {}
            other => {
                return Err(
                    format!("a cancelled run should end Interrupted, got {other:?}").into(),
                );
            }
        }
        if self.target.profile.spawns_process && log.records.is_empty() {
            return Err("the run reported no ProcessRecord, so its cleanup cannot be shown".into());
        }
        Target::processes_gone(&log.records, self.target.limits.cancel).await?;
        Ok(Outcome::Pass)
    }

    async fn crash(&mut self) -> CaseResultOf {
        let scenario = self.need(&self.target.plan.crash, "crash")?;
        let log = self
            .target
            .drive(self.target.spec(scenario)?, Behaviour::Passive)
            .await?;
        require_one_done(&log)?;
        match log.done() {
            Some((StopReason::Error, _, Some(diagnostic))) if !diagnostic.trim().is_empty() => {}
            other => {
                return Err(format!(
                    "a crashing harness should end Done {{ Error }} with a diagnostic, got {other:?}"
                )
                .into());
            }
        }
        Target::processes_gone(&log.records, self.target.limits.cancel).await?;
        Ok(Outcome::Pass)
    }

    // -- claims with their own run -------------------------------------------------------

    async fn resume(&mut self) -> CaseResultOf {
        let scenario = self.need(&self.target.plan.resume, "resume")?;
        if scenario.resume.is_none() {
            return Err("the resume scenario carries no session to resume".into());
        }
        let log = self.run_completed(scenario, Behaviour::Passive).await?;
        if log.started().is_empty() {
            return Err("the resumed run never emitted Started".into());
        }
        require_expectations(&log, scenario)
    }

    async fn mcp(&mut self) -> CaseResultOf {
        let scenario = self.need(&self.target.plan.mcp, "mcp")?;
        if scenario.mcp.is_empty() {
            return Err("the mcp scenario injects no server".into());
        }
        let log = self.run_completed(scenario, Behaviour::Passive).await?;
        require_expectations(&log, scenario)
    }

    // -- refusals ------------------------------------------------------------------------

    async fn access_modes(&mut self) -> CaseResultOf {
        let scenario = self.need(&self.target.plan.lifecycle, "lifecycle")?;
        let mut skipped = Vec::new();
        for mode in [
            AccessMode::Supervised,
            AccessMode::AutoAcceptEdits,
            AccessMode::FullAccess,
        ] {
            let mut spec = self.target.spec(scenario)?;
            spec.access = mode;
            if self.caps().access_modes.contains(&mode) {
                if mode != scenario.access && !self.target.exhaustive {
                    skipped.push(format!("{mode:?}"));
                    continue;
                }
                let log = self.target.drive(spec, Behaviour::Passive).await?;
                require_one_done(&log)?;
                match log.done() {
                    Some((StopReason::Completed, _, _)) => {}
                    other => {
                        return Err(format!("claimed access mode {mode:?} ended {other:?}").into());
                    }
                }
            } else {
                match self.target.expect_refusal(spec).await? {
                    ExecError::UnsupportedAccess { requested, .. } if requested == mode => {}
                    other => {
                        return Err(format!(
                            "unclaimed access mode {mode:?} was refused with {other}, not UnsupportedAccess"
                        )
                        .into());
                    }
                }
            }
        }
        waived_if(skipped, "access modes claimed but not run on this target")
    }

    async fn sandbox_levels(&mut self) -> CaseResultOf {
        let scenario = self.need(&self.target.plan.lifecycle, "lifecycle")?;
        let mut skipped = Vec::new();
        for level in [
            SandboxLevel::ReadOnly,
            SandboxLevel::WorkspaceWrite,
            SandboxLevel::Unrestricted,
        ] {
            let mut spec = self.target.spec(scenario)?;
            spec.sandbox = level;
            if level == SandboxLevel::Unrestricted || self.caps().sandbox_levels.contains(&level) {
                if level != scenario.sandbox && !self.target.exhaustive {
                    skipped.push(format!("{level:?}"));
                    continue;
                }
                let log = self.target.drive(spec, Behaviour::Passive).await?;
                require_one_done(&log)?;
                match log.done() {
                    Some((StopReason::Completed, _, _)) => {}
                    other => {
                        return Err(
                            format!("claimed sandbox level {level:?} ended {other:?}").into()
                        );
                    }
                }
            } else {
                match self.target.expect_refusal(spec).await? {
                    ExecError::InvalidSpec(why) if why.contains(&format!("{level:?}")) => {}
                    other => {
                        return Err(format!(
                            "unclaimed sandbox level {level:?} was refused with {other}, not a typed refusal naming it"
                        )
                        .into());
                    }
                }
            }
        }
        waived_if(skipped, "sandbox levels claimed but not run on this target")
    }

    async fn output_schema(&mut self) -> CaseResultOf {
        if self.caps().structured_output {
            let scenario = self.need(&self.target.plan.output_schema, "output-schema")?;
            let mut spec = self.target.spec(scenario)?;
            spec.output_schema = Some(serde_json::json!({"type": "object"}));
            let log = self.target.drive(spec, Behaviour::Passive).await?;
            require_one_done(&log)?;
            let Some((StopReason::Completed, Some(result), _)) = log.done() else {
                return Err("a structured run should complete with a result".into());
            };
            return match serde_json::from_str::<serde_json::Value>(result) {
                Ok(value) if value.is_object() => Ok(Outcome::Pass),
                _ => Err(format!(
                    "the result {result:?} is not the JSON object the schema asks for"
                )
                .into()),
            };
        }
        let scenario = self.need(&self.target.plan.lifecycle, "lifecycle")?;
        let mut spec = self.target.spec(scenario)?;
        spec.output_schema = Some(serde_json::json!({"type": "object"}));
        match self.target.expect_refusal(spec).await? {
            ExecError::UnsupportedOutputSchema => Ok(Outcome::Pass),
            other => Err(format!(
                "an output schema was refused with {other}, not UnsupportedOutputSchema"
            )
            .into()),
        }
    }

    // -- counters ------------------------------------------------------------------------

    async fn unknown_frames(&mut self) -> CaseResultOf {
        self.plain_run().await?;
        match &self.lifecycle_unknown {
            Some(diff) if !diff.is_empty() => {
                return Err(format!("a clean run counted unrecognised frames: {diff:?}").into());
            }
            _ => {}
        }
        let plan = self.need(&self.target.plan.unknown_frames, "unknown-frame")?;
        let before = self.target.executor.unknown_frames();
        self.run_completed(&plan.scenario, Behaviour::Passive)
            .await?;
        let diff = counter_diff(&before, &self.target.executor.unknown_frames());
        for key in &plan.keys {
            if !diff.keys().any(|seen| seen.contains(key.as_str())) {
                return Err(format!("no counter named like {key:?} rose; saw {diff:?}").into());
            }
        }
        Ok(Outcome::Pass)
    }

    // -- the descriptor against the outcomes ---------------------------------------------

    fn claims(&self) -> CaseResultOf {
        let caps = self.caps();
        let mut claims: Vec<(String, CaseId)> = Vec::new();
        if caps.steering != Steering::None {
            claims.push((format!("steering {:?}", caps.steering), CaseId::Steer));
        }
        if caps.resume {
            claims.push(("resume".into(), CaseId::Resume));
        }
        if caps.mcp_injection {
            claims.push(("mcp injection".into(), CaseId::Mcp));
        }
        if caps.structured_output {
            claims.push(("structured output".into(), CaseId::OutputSchema));
        }
        if caps.plan_mode {
            claims.push(("plan mode".into(), CaseId::PlanMode));
        }
        if caps.deterministic_turn_end {
            claims.push(("deterministic turn end".into(), CaseId::Lifecycle));
        }
        for mode in &caps.access_modes {
            claims.push((format!("access mode {mode:?}"), CaseId::AccessModes));
        }
        for level in &caps.sandbox_levels {
            claims.push((format!("sandbox level {level:?}"), CaseId::SandboxLevels));
        }
        if caps.access_modes.contains(&AccessMode::Supervised) {
            claims.push(("approvals (allow)".into(), CaseId::ApprovalAllow));
            claims.push(("approvals (deny)".into(), CaseId::ApprovalDeny));
        }
        if !caps.models.is_empty() {
            claims.push(("models".into(), CaseId::Probe));
        }
        let mut broken = Vec::new();
        let mut unverified = Vec::new();
        for (claim, case) in claims {
            match self.statuses.get(&case) {
                Some(Status::Pass) => {}
                Some(Status::Waived(_)) => unverified.push(claim),
                other => broken.push(format!(
                    "{claim} (case {} is {})",
                    case.name(),
                    other.map_or("missing", Status::label)
                )),
            }
        }
        if !broken.is_empty() {
            return Err(format!("claimed but not proven: {}", broken.join("; ")).into());
        }
        waived_if(unverified, "claimed but unverified (waived on this target)")
    }
}

fn waived_if(items: Vec<String>, what: &str) -> CaseResultOf {
    if items.is_empty() {
        Ok(Outcome::Pass)
    } else {
        Ok(Outcome::Waived(format!("{what}: {}", items.join(", "))))
    }
}

fn require_expectations(log: &RunLog, scenario: &Scenario) -> CaseResultOf {
    if scenario.expect.is_empty() {
        return Err(Failure::Unplanned(
            "the scenario declares no expectation, so nothing shows the behaviour reached the agent"
                .into(),
        ));
    }
    log.check_all(&scenario.expect)?;
    Ok(Outcome::Pass)
}

/// Exactly one `Done`, and it is the last event.
fn require_one_done(log: &RunLog) -> Result<(), String> {
    match log.done_count() {
        1 if log.done().is_some() => Ok(()),
        0 => Err("the stream ended without a Done".into()),
        1 => Err("events follow the Done".into()),
        n => Err(format!("the stream carried {n} Done events")),
    }
}

/// One `Started`, ahead of every event that needs a session.
fn check_started_first(log: &RunLog) -> Result<(), String> {
    let started = log.started();
    if started.len() != 1 {
        return Err(format!("expected one Started, saw {}", started.len()));
    }
    if started[0].0.is_empty() {
        return Err("Started carries an empty session".into());
    }
    for event in &log.events {
        match event {
            ExecEvent::Started { .. } => return Ok(()),
            ExecEvent::Usage { .. } => {}
            other => return Err(format!("{other:?} came before Started")),
        }
    }
    Ok(())
}

fn unique_models<'a>(ids: impl Iterator<Item = &'a str>, whose: &str) -> Result<(), String> {
    let mut seen = std::collections::BTreeSet::new();
    for id in ids {
        if id.is_empty() {
            return Err(format!("the {whose} lists a model with no id"));
        }
        if !seen.insert(id) {
            return Err(format!("the {whose} lists model {id:?} twice"));
        }
    }
    Ok(())
}

fn check_probe(probe: &HealthProbe, verified: &reno_proto::VersionRange) -> Result<(), String> {
    let InstallStatus::Installed { path, version } = &probe.installed else {
        return Err(format!("not installed: {:?}", probe.installed));
    };
    if path.as_os_str().is_empty() {
        return Err("Installed carries an empty path".into());
    }
    if let SignedIn::No { hint } = &probe.signed_in {
        return Err(format!("signed out: {hint}"));
    }
    unique_models(probe.models.models.iter().map(|m| m.id.as_str()), "probe")?;
    let Some(version) = version else {
        return match probe.version_gate {
            VersionGate::Unchecked => Ok(()),
            ref gate => Err(format!("no version was read, yet the gate says {gate:?}")),
        };
    };
    let parsed = semver::Version::parse(version.trim().trim_start_matches('v'));
    let (Ok(parsed), Ok(min), Ok(top)) = (
        parsed,
        semver::Version::parse(&verified.min),
        semver::Version::parse(&verified.verified),
    ) else {
        // A version that is not semver (the mock's) cannot be compared; the gate must not claim
        // it was.
        return match probe.version_gate {
            VersionGate::Unchecked | VersionGate::Ok => Ok(()),
            ref gate => Err(format!(
                "version {version:?} cannot be compared, yet the gate says {gate:?}"
            )),
        };
    };
    let expected = if parsed < min {
        "too_old"
    } else if parsed > top {
        "untested"
    } else {
        "ok"
    };
    let actual = match &probe.version_gate {
        VersionGate::TooOld { .. } => "too_old",
        VersionGate::Untested { .. } => "untested",
        VersionGate::Ok => "ok",
        VersionGate::Unchecked => "unchecked",
    };
    if actual == expected {
        Ok(())
    } else {
        Err(format!(
            "version {version} is {expected} against {}..{}, yet the gate says {actual}",
            verified.min, verified.verified
        ))
    }
}

/// What rose between two counter snapshots.
fn counter_diff(
    before: &BTreeMap<String, u64>,
    after: &BTreeMap<String, u64>,
) -> BTreeMap<String, u64> {
    after
        .iter()
        .filter_map(|(kind, now)| {
            let rose = now.saturating_sub(before.get(kind).copied().unwrap_or(0));
            (rose > 0).then(|| (kind.clone(), rose))
        })
        .collect()
}
