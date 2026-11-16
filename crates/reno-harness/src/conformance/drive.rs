//! Driving one run of one scenario, and what it left behind.

use std::time::{Duration, Instant};

use futures::StreamExt;
use reno_proto::{ApprovalSubject, ExecEvent, ExecSpec, Question, RunId, StopReason};
use tokio::sync::mpsc;

use super::{DENY_MESSAGE, Expect, STEER_TEXT, Scenario, Target};
use crate::ProcessRecord;
use crate::exec::{Decision, ExecControls, ExecError, ExecHandle, InputAnswer};

/// What the driver does when the run asks or waits.
#[derive(Debug, Clone)]
pub(crate) enum Behaviour {
    /// Allow every approval, answer every question with its last option, do nothing else.
    Passive,
    /// Decide every approval this way, with this message.
    Decide(Decision, Option<String>),
    /// Send [`STEER_TEXT`] after the first `Text`.
    Steer,
    /// Cancel as soon as the run has started.
    CancelOnStart,
}

impl Behaviour {
    pub(crate) fn deny(with_message: bool) -> Self {
        Self::Decide(
            Decision::Deny,
            with_message.then(|| DENY_MESSAGE.to_owned()),
        )
    }
}

/// Everything one run showed.
#[derive(Debug, Default)]
pub(crate) struct RunLog {
    pub(crate) events: Vec<ExecEvent>,
    pub(crate) approvals: Vec<ApprovalSubject>,
    pub(crate) questions: Vec<Vec<Question>>,
    pub(crate) records: Vec<ProcessRecord>,
    pub(crate) steered: bool,
    /// How long the run took to end after the cancel, when it was cancelled.
    pub(crate) cancel_took: Option<Duration>,
}

impl RunLog {
    pub(crate) fn done(&self) -> Option<(StopReason, Option<&str>, Option<&str>)> {
        match self.events.last() {
            Some(ExecEvent::Done {
                stop,
                result,
                diagnostic,
            }) => Some((*stop, result.as_deref(), diagnostic.as_deref())),
            _ => None,
        }
    }

    pub(crate) fn done_count(&self) -> usize {
        self.events
            .iter()
            .filter(|e| matches!(e, ExecEvent::Done { .. }))
            .count()
    }

    pub(crate) fn text(&self) -> String {
        self.events
            .iter()
            .filter_map(|e| match e {
                ExecEvent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    pub(crate) fn started(&self) -> Vec<&reno_proto::SessionRef> {
        self.events
            .iter()
            .filter_map(|e| match e {
                ExecEvent::Started { session, .. } => Some(session),
                _ => None,
            })
            .collect()
    }

    /// Check one expectation against this run.
    pub(crate) fn check(&self, expect: &Expect) -> Result<(), String> {
        match expect {
            Expect::ResultContains(want) => {
                let result = self.done().and_then(|(_, result, _)| result).unwrap_or("");
                if result.contains(want.as_str()) {
                    Ok(())
                } else {
                    Err(format!("Done.result {result:?} does not contain {want:?}"))
                }
            }
            Expect::TextContains(want) => {
                let text = self.text();
                let result = self.done().and_then(|(_, result, _)| result).unwrap_or("");
                if text.contains(want.as_str()) || result.contains(want.as_str()) {
                    Ok(())
                } else {
                    Err(format!(
                        "no text or result contains {want:?} (text {text:?})"
                    ))
                }
            }
            Expect::SessionIs(want) => match self.started().first() {
                Some(session) if session.0 == *want => Ok(()),
                other => Err(format!("Started.session is {other:?}, expected {want:?}")),
            },
            Expect::Tool { name, class } => {
                let named: Vec<_> = self
                    .events
                    .iter()
                    .filter_map(|e| match e {
                        ExecEvent::Tool { call, .. } if call.name.contains(name.as_str()) => {
                            Some(call.class)
                        }
                        _ => None,
                    })
                    .collect();
                if named.is_empty() {
                    Err(format!("no tool call named like {name:?}"))
                } else if named.contains(class) {
                    Ok(())
                } else {
                    Err(format!(
                        "tool {name:?} carries class {named:?}, expected {class:?}"
                    ))
                }
            }
            Expect::ApprovalClass(class) => {
                if self.approvals.iter().any(|s| s.action == *class) {
                    Ok(())
                } else {
                    let seen: Vec<_> = self.approvals.iter().map(|s| s.action).collect();
                    Err(format!(
                        "approval classes {seen:?} do not include {class:?}"
                    ))
                }
            }
        }
    }

    pub(crate) fn check_all<'a>(
        &self,
        expects: impl IntoIterator<Item = &'a Expect>,
    ) -> Result<(), String> {
        for expect in expects {
            self.check(expect)?;
        }
        Ok(())
    }
}

/// Cancels the run when dropped, so a case that times out never leaves its run going.
struct CancelOnDrop(ExecHandle);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}

impl Target {
    /// The spec a scenario describes.
    pub(crate) fn spec(&self, scenario: &Scenario) -> Result<ExecSpec, String> {
        let run_id = RunId::from_suffix("01HZXC").map_err(|e| format!("run id: {e}"))?;
        Ok(ExecSpec {
            run_id,
            prompt: scenario.prompt.clone(),
            cwd: self.cwd.clone(),
            model: scenario.model.clone(),
            effort: None,
            access: scenario.access,
            sandbox: scenario.sandbox,
            resume: scenario.resume.clone(),
            mcp: scenario.mcp.clone(),
            env: scenario.env.clone(),
            output_schema: None,
            instructions: None,
        })
    }

    /// Start `spec` and drive it to its end as `behaviour` says. The caller bounds the time.
    pub(crate) async fn drive(
        &self,
        spec: ExecSpec,
        behaviour: Behaviour,
    ) -> Result<RunLog, String> {
        let (controls, handle) = ExecControls::new(8);
        let mut records = handle
            .take_process_records()
            .ok_or("process records were already taken")?;
        let guard = CancelOnDrop(handle.clone());
        let mut stream = self
            .executor
            .start(spec, controls)
            .await
            .map_err(|e| format!("start refused the run: {e}"))?;
        let mut log = RunLog::default();
        let mut cancelled_at: Option<Instant> = None;
        let mut deadline: Option<Instant> = None;
        loop {
            let next = match deadline {
                Some(at) => {
                    match tokio::time::timeout(
                        at.saturating_duration_since(Instant::now()),
                        stream.next(),
                    )
                    .await
                    {
                        Ok(next) => next,
                        Err(_) => {
                            return Err(format!(
                                "the run did not end within {:?} of the cancel",
                                self.limits.cancel
                            ));
                        }
                    }
                }
                None => stream.next().await,
            };
            let Some(event) = next else { break };
            while let Ok(record) = records.try_recv() {
                log.records.push(record);
            }
            match &event {
                ExecEvent::NeedsApproval {
                    request_id,
                    subject,
                } => {
                    log.approvals.push(subject.clone());
                    let (decision, message) = match &behaviour {
                        Behaviour::Decide(decision, message) => (*decision, message.clone()),
                        _ => (Decision::Allow, None),
                    };
                    handle
                        .decide_with_message(request_id.clone(), decision, message)
                        .await
                        .map_err(|_| "the run closed before the decision".to_owned())?;
                }
                ExecEvent::NeedsInput {
                    request_id,
                    questions,
                } => {
                    log.questions.push(questions.clone());
                    let answers = questions
                        .iter()
                        .map(|q| InputAnswer {
                            question_id: q.id.clone(),
                            labels: vec![q.options.last().cloned().unwrap_or_else(|| "A".into())],
                        })
                        .collect();
                    handle
                        .answer_input(request_id.clone(), answers)
                        .await
                        .map_err(|_| "the run closed before the answer".to_owned())?;
                }
                ExecEvent::Text { .. } if matches!(behaviour, Behaviour::Steer) && !log.steered => {
                    log.steered = true;
                    handle
                        .steer(STEER_TEXT)
                        .await
                        .map_err(|_| "the run closed before the steer".to_owned())?;
                }
                ExecEvent::Started { .. }
                    if matches!(behaviour, Behaviour::CancelOnStart) && cancelled_at.is_none() =>
                {
                    handle.cancel();
                    let now = Instant::now();
                    cancelled_at = Some(now);
                    deadline = Some(now + self.limits.cancel);
                }
                _ => {}
            }
            let done = matches!(event, ExecEvent::Done { .. });
            log.events.push(event);
            if done {
                // The stream ends with the Done; anything after it is a contract breach the
                // lifecycle case reports.
                while let Some(extra) = stream.next().await {
                    log.events.push(extra);
                }
                break;
            }
        }
        while let Ok(record) = records.try_recv() {
            log.records.push(record);
        }
        log.cancel_took = cancelled_at.map(|at| at.elapsed());
        drop(guard);
        Ok(log)
    }

    /// Start `spec` expecting a refusal: returns the error, and fails when the run began or a
    /// child process was reported.
    pub(crate) async fn expect_refusal(&self, spec: ExecSpec) -> Result<ExecError, String> {
        let (controls, handle) = ExecControls::new(2);
        let mut records = handle
            .take_process_records()
            .ok_or("process records were already taken")?;
        let _guard = CancelOnDrop(handle);
        match self.executor.start(spec, controls).await {
            Ok(_stream) => {
                Err("the run started although the spec asks for more than declared".into())
            }
            Err(error) => match records.try_recv() {
                Ok(record) => Err(format!(
                    "refused with {error}, but a process ({}) was spawned first",
                    record.pid
                )),
                Err(mpsc::error::TryRecvError::Empty | mpsc::error::TryRecvError::Disconnected) => {
                    Ok(error)
                }
            },
        }
    }

    /// Wait until no process of any record survives, up to `bound`.
    pub(crate) async fn processes_gone(
        records: &[ProcessRecord],
        bound: Duration,
    ) -> Result<(), String> {
        let until = Instant::now() + bound;
        loop {
            let alive: Vec<i32> = records
                .iter()
                .filter(|r| r.group_alive())
                .map(|r| r.pgid)
                .collect();
            if alive.is_empty() {
                return Ok(());
            }
            if Instant::now() >= until {
                return Err(format!(
                    "process groups {alive:?} still alive {bound:?} after the run"
                ));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}
