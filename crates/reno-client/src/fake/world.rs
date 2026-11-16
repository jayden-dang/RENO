//! The fake daemon's state: runs with their journals, and pending approvals. Plain data and
//! synchronous methods; no I/O, no clock (time is a logical counter), no async.

use std::sync::Arc;

use reno_proto::defs::{DefKind, DefRecord, DefsSnapshot};
use reno_proto::{
    Approval, ApprovalId, ExecEvent, ExecutorId, IdError, RunId, RunStatus, RunSummary, Seq,
    StopReason, TaskId,
};
use reno_proto::{ApprovalDecision, ApprovalRecord, ApprovalStatus, ResolvedBy, TriggerInfo};
use tokio::sync::{Notify, watch};
use tokio_util::sync::CancellationToken;

use super::data::{self, Seed};

/// Logical milliseconds added to the clock by every change.
const TICK_MS: u64 = 1_000;

/// One run, its journal, and what stops its script.
pub(super) struct RunRecord {
    pub(super) summary: RunSummary,
    pub(super) journal: Vec<ExecEvent>,
    /// Carries the journal length; subscribers wake on change.
    pub(super) journal_tx: watch::Sender<usize>,
    /// Cancelled when the run is cancelled or the daemon shuts down.
    pub(super) cancel: CancellationToken,
    /// Notified by `run.steer`; a `/long` script waits on it.
    pub(super) steered: Arc<Notify>,
    /// The routing decision the run was made with.
    pub(super) routing: Option<reno_proto::RoutingDecision>,
}

impl RunRecord {
    /// Whether the journal ended: its last event is `Done`.
    pub(super) fn finished(&self) -> bool {
        matches!(self.journal.last(), Some(ExecEvent::Done { .. }))
    }

    /// The id of the most recent tool call, which an approval's result answers.
    pub(super) fn last_tool_id(&self) -> String {
        self.journal
            .iter()
            .rev()
            .find_map(|e| match e {
                ExecEvent::Tool { id, .. } => Some(id.clone()),
                _ => None,
            })
            .unwrap_or_default()
    }
}

/// Everything the fake daemon serves.
pub(super) struct World {
    runs: Vec<RunRecord>,
    approvals: Vec<Approval>,
    next_id: u64,
    now_ms: u64,
    started_ms: u64,
    pub(super) executors: Vec<reno_proto::ExecutorInfo>,
    pub(super) triggers: Vec<TriggerInfo>,
    pub(super) defs: DefsSnapshot,
    records: Vec<DefRecord>,
    history: Vec<ApprovalRecord>,
}

/// Why the fake daemon refuses an install.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum InstallRefused {
    /// No such executor.
    NotFound,
    /// Its health does not say `needs_install`.
    NothingToInstall,
}

impl World {
    pub(super) fn from_seed(seed: Seed, root: &CancellationToken) -> Self {
        let runs = seed
            .runs
            .into_iter()
            .map(|r| RunRecord {
                journal_tx: watch::channel(r.journal.len()).0,
                summary: r.summary,
                journal: r.journal,
                cancel: root.child_token(),
                steered: Arc::new(Notify::new()),
                routing: None,
            })
            .collect();
        Self {
            runs,
            approvals: seed.approvals,
            next_id: 1,
            now_ms: data::NOW_MS,
            started_ms: data::NOW_MS,
            executors: seed.extras.executors,
            triggers: seed.extras.triggers,
            defs: seed.extras.defs,
            records: seed.extras.records,
            history: seed.extras.history,
        }
    }

    /// Probes again: stamps the health of `id` (every executor when `None`) with a new time.
    /// `Err` when `id` is not an executor here.
    pub(super) fn refresh_executors(
        &mut self,
        id: Option<&reno_proto::ExecutorId>,
    ) -> Result<(), ()> {
        if id.is_some_and(|id| !self.executors.iter().any(|e| &e.descriptor.id == id)) {
            return Err(());
        }
        let now = self.tick();
        for info in &mut self.executors {
            if id.is_none_or(|id| id == &info.descriptor.id)
                && let Some(health) = &mut info.health
            {
                health.checked_at_ms = now;
            }
        }
        Ok(())
    }

    /// Installs the adapter of `id`, which must be an executor whose health says `needs_install`:
    /// nothing is fetched. The health becomes installed, signed in unknown. `Err` says why not.
    pub(super) fn install_executor(
        &mut self,
        id: &reno_proto::ExecutorId,
    ) -> Result<(), InstallRefused> {
        let now = self.tick();
        let info = self
            .executors
            .iter_mut()
            .find(|e| &e.descriptor.id == id)
            .ok_or(InstallRefused::NotFound)?;
        let health = info
            .health
            .as_mut()
            .ok_or(InstallRefused::NothingToInstall)?;
        if !matches!(
            health.installed,
            reno_proto::InstallStatus::NotInstalled {
                why: reno_proto::NotInstalledReason::NeedsInstall { .. }
            }
        ) {
            return Err(InstallRefused::NothingToInstall);
        }
        health.installed = reno_proto::InstallStatus::Installed {
            path: format!("/fake/adapters/{id}/bin").into(),
            version: Some("1.0.0".to_owned()),
        };
        health.version_gate = reno_proto::VersionGate::Ok;
        health.signed_in = reno_proto::SignedIn::Unknown;
        health.checked_at_ms = now;
        Ok(())
    }

    fn tick(&mut self) -> u64 {
        self.now_ms += TICK_MS;
        self.now_ms
    }

    pub(super) fn uptime_secs(&self) -> u64 {
        (self.now_ms - self.started_ms) / 1_000
    }

    pub(super) fn run(&self, id: &RunId) -> Option<&RunRecord> {
        self.runs.iter().find(|r| &r.summary.run_id == id)
    }

    fn run_mut(&mut self, id: &RunId) -> Option<&mut RunRecord> {
        self.runs.iter_mut().find(|r| &r.summary.run_id == id)
    }

    /// Every run, newest first.
    pub(super) fn summaries(&self) -> Vec<RunSummary> {
        let mut all: Vec<RunSummary> = self.runs.iter().map(|r| r.summary.clone()).collect();
        all.sort_by(|a, b| {
            b.created_at_ms
                .cmp(&a.created_at_ms)
                .then_with(|| a.run_id.cmp(&b.run_id))
        });
        all
    }

    /// Pending approvals, oldest first.
    pub(super) fn pending(&self) -> Vec<Approval> {
        let mut all = self.approvals.clone();
        all.sort_by(|a, b| (a.requested_at_ms, &a.id).cmp(&(b.requested_at_ms, &b.id)));
        all
    }

    /// A simplified router: the named executor, else every executor by id; each is dropped for
    /// what its health or access modes say. The real daemon's router is in `reno-kernel`.
    pub(super) fn route(&self, params: &reno_proto::TaskRunParams) -> reno_proto::RoutingDecision {
        use reno_proto::{Ineligible, RejectReason, Rejection, RuleMatched};
        let (rule_matched, order) = match &params.executor {
            Some(executor) => (RuleMatched::ExplicitExecutor, vec![executor.clone()]),
            None => {
                let mut ids: Vec<ExecutorId> = self
                    .executors
                    .iter()
                    .map(|e| e.descriptor.id.clone())
                    .collect();
                ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
                (RuleMatched::Default, ids)
            }
        };
        let mut ranked = Vec::new();
        let mut rejected = Vec::new();
        for id in order {
            let reason = match self.executors.iter().find(|e| e.descriptor.id == id) {
                None => Some(RejectReason::NotRegistered),
                Some(info)
                    if !info
                        .descriptor
                        .caps
                        .access_modes
                        .contains(&reno_proto::AccessMode::Supervised) =>
                {
                    Some(RejectReason::AccessNotOffered {
                        requested: reno_proto::AccessMode::Supervised,
                        offered: info.descriptor.caps.access_modes.clone(),
                    })
                }
                Some(info) => {
                    info.health
                        .as_ref()
                        .and_then(|h| h.ineligible())
                        .map(|why| match why {
                            Ineligible::NotInstalled(why) => RejectReason::NotInstalled {
                                detail: format!("{why:?}"),
                            },
                            Ineligible::TooOld { found, min } => {
                                RejectReason::TooOld { found, min }
                            }
                            Ineligible::SignedOut(hint) => RejectReason::SignedOut { hint },
                        })
                }
            };
            match reason {
                None => ranked.push(id),
                Some(reason) => rejected.push(Rejection {
                    executor: id,
                    reason,
                }),
            }
        }
        reno_proto::RoutingDecision {
            chosen: ranked.first().cloned(),
            ranked,
            rejected,
            rule_matched,
            fallbacks: Vec::new(),
            at_ms: self.now_ms,
        }
    }

    /// Creates a queued run and returns its id.
    pub(super) fn create_run(
        &mut self,
        params: &reno_proto::TaskRunParams,
        executor: ExecutorId,
        routing: reno_proto::RoutingDecision,
        root: &CancellationToken,
    ) -> Result<RunId, IdError> {
        let n = self.next_id;
        self.next_id += 1;
        let run_id = RunId::from_suffix(&format!("fake{n}"))?;
        let now = self.tick();
        let title = params
            .prompt
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .take(80)
            .collect();
        self.runs.push(RunRecord {
            summary: RunSummary {
                run_id: run_id.clone(),
                task_id: TaskId::from_suffix(&format!("fake{n}"))?,
                executor,
                status: RunStatus::Queued,
                attempt: 1,
                title,
                prompt: Some(params.prompt.clone()),
                model: params.model.clone(),
                stop_reason: None,
                bot: None,
                created_at_ms: now,
                updated_at_ms: now,
            },
            journal: vec![],
            journal_tx: watch::channel(0).0,
            cancel: root.child_token(),
            steered: Arc::new(Notify::new()),
            routing: Some(routing),
        });
        Ok(run_id)
    }

    /// Moves a run along an edge of the lifecycle diagram. False, changing nothing, when the run
    /// does not exist or the edge is not in the diagram.
    pub(super) fn set_status(&mut self, id: &RunId, status: RunStatus) -> bool {
        let now = self.tick();
        let Some(run) = self.run_mut(id) else {
            return false;
        };
        if !run.summary.status.can_transition_to(status) {
            return false;
        }
        run.summary.status = status;
        run.summary.updated_at_ms = now;
        true
    }

    /// Appends an event to a run's journal and wakes its subscribers. A `Done` also records the
    /// run's stop reason.
    pub(super) fn push(&mut self, id: &RunId, event: ExecEvent) {
        let now = self.tick();
        let Some(run) = self.run_mut(id) else {
            return;
        };
        if let ExecEvent::Done { stop, .. } = &event {
            run.summary.stop_reason = Some(*stop);
        }
        run.summary.updated_at_ms = now;
        run.journal.push(event);
        run.journal_tx.send_replace(run.journal.len());
    }

    /// Ends a run: the final status and the closing `Done` land together, so a subscriber that
    /// has seen `Done` always finds the run terminal.
    pub(super) fn finish(&mut self, id: &RunId, status: RunStatus, done: ExecEvent) -> bool {
        if !self.set_status(id, status) {
            return false;
        }
        self.push(id, done);
        true
    }

    /// Journals a steering message. Returns the run when it is live (not finished), else `None`.
    pub(super) fn steer(&mut self, id: &RunId, prompt: &str) -> Option<RunId> {
        if self.run(id)?.finished() {
            return None;
        }
        self.push(
            id,
            ExecEvent::UserMessage {
                text: prompt.to_owned(),
            },
        );
        Some(id.clone())
    }

    /// Adds a pending approval for `run`, returning it.
    pub(super) fn hold(
        &mut self,
        run: &RunId,
        subject: reno_proto::ApprovalSubject,
    ) -> Result<Approval, IdError> {
        let n = self.next_id;
        self.next_id += 1;
        let approval = Approval {
            id: ApprovalId::from_suffix(&format!("fake{n}"))?,
            run_id: run.clone(),
            subject,
            requested_at_ms: self.tick(),
        };
        self.approvals.push(approval.clone());
        Ok(approval)
    }

    /// Removes a pending approval and records how it was decided.
    pub(super) fn take_approval(
        &mut self,
        id: &ApprovalId,
        decision: ApprovalDecision,
        message: Option<String>,
    ) -> Option<Approval> {
        let at = self.approvals.iter().position(|a| &a.id == id)?;
        let approval = self.approvals.remove(at);
        let now = self.tick();
        self.history.push(ApprovalRecord {
            id: approval.id.clone(),
            run_id: approval.run_id.clone(),
            request_id: format!("req-{}", approval.id),
            subject: approval.subject.clone(),
            status: match decision {
                ApprovalDecision::ApproveOnce => ApprovalStatus::Approved,
                ApprovalDecision::Deny => ApprovalStatus::Denied,
            },
            decided_by: Some(ResolvedBy::Human),
            created_at_ms: approval.requested_at_ms,
            decided_at_ms: Some(now),
            expires_at_ms: approval.requested_at_ms + 30 * 60_000,
            message: message
                .map(|text| text.trim().to_owned())
                .filter(|text| !text.is_empty()),
        });
        Some(approval)
    }

    /// Decided approvals, newest decision first, optionally of one run.
    pub(super) fn history(&self, run: Option<&RunId>, limit: usize) -> Vec<ApprovalRecord> {
        let mut all: Vec<ApprovalRecord> = self
            .history
            .iter()
            .filter(|r| run.is_none_or(|run| &r.run_id == run))
            .cloned()
            .collect();
        all.sort_by(|a, b| {
            b.decided_at_ms
                .cmp(&a.decided_at_ms)
                .then_with(|| a.id.cmp(&b.id))
        });
        all.truncate(limit);
        all
    }

    /// One stored version of a definition; the newest when `version` is `None`.
    pub(super) fn def_record(
        &self,
        kind: DefKind,
        name: &str,
        version: Option<u32>,
    ) -> Option<DefRecord> {
        self.records
            .iter()
            .find(|r| r.kind == kind && r.name == name && version.is_none_or(|v| v == r.version))
            .cloned()
    }

    /// Fires a trigger. `Err` carries the reason it cannot fire.
    pub(super) fn fire_trigger(
        &mut self,
        name: &str,
        root: &CancellationToken,
    ) -> Result<(reno_proto::FiringOutcome, RunId, String, u64), &'static str> {
        use reno_proto::FiringOutcome;
        let at = self
            .triggers
            .iter()
            .position(|t| t.name == name)
            .ok_or("no such trigger")?;
        let status = self.triggers[at].status;
        if matches!(
            status,
            reno_proto::TriggerStatus::Disabled
                | reno_proto::TriggerStatus::Done
                | reno_proto::TriggerStatus::Failed
        ) {
            return Err("the trigger cannot fire in its state");
        }
        let scheduled = self.tick();
        if let Some(last) = self.triggers[at].last_run_id.clone()
            && self
                .run(&last)
                .is_some_and(|r| !r.summary.status.is_terminal())
        {
            self.triggers[at].last_fired_at_ms = Some(scheduled);
            return Ok((FiringOutcome::Coalesced, last, String::new(), scheduled));
        }
        let (bot, prompt) = super::extras::trigger_target(name).unwrap_or(("keeper", name));
        let params = reno_proto::TaskRunParams {
            prompt: prompt.to_owned(),
            executor: "claude-code".parse().ok(),
            bot: None,
            cwd: None,
            model: None,
            in_place: false,
        };
        let executor = params.executor.clone().ok_or("no executor")?;
        let routing = self.route(&params);
        let run_id = self
            .create_run(&params, executor, routing, root)
            .map_err(|_| "could not create the run")?;
        if let Some(run) = self.run_mut(&run_id) {
            run.summary.bot = Some(reno_proto::defs::BotPin {
                name: bot.to_owned(),
                version: 1,
            });
            run.summary.title = format!("{bot} \u{b7} atlas-api: {name} fired");
        }
        let trigger = &mut self.triggers[at];
        trigger.last_fired_at_ms = Some(scheduled);
        trigger.last_run_id = Some(run_id.clone());
        trigger.failures = 0;
        if trigger.status == reno_proto::TriggerStatus::BackedOff {
            trigger.status = reno_proto::TriggerStatus::Scheduled;
            trigger.backoff_until_ms = None;
        }
        Ok((FiringOutcome::Fired, run_id, prompt.to_owned(), scheduled))
    }

    /// Cancels a run by walking the lifecycle diagram to `cancelled`, closing its journal.
    /// Returns the final status, or `None` when the run is unknown or already ended.
    pub(super) fn cancel(&mut self, id: &RunId, reason: Option<&str>) -> Option<RunStatus> {
        let status = self.run(id)?.summary.status;
        let path: &[RunStatus] = match status {
            RunStatus::Queued => &[RunStatus::Cancelled],
            RunStatus::Leased => &[RunStatus::Queued, RunStatus::Cancelled],
            RunStatus::Running
            | RunStatus::WaitingInput
            | RunStatus::WaitingApproval
            | RunStatus::Evaluating => &[RunStatus::Interrupted, RunStatus::Cancelled],
            RunStatus::Interrupted => &[RunStatus::Cancelled],
            RunStatus::Succeeded | RunStatus::Failed | RunStatus::Cancelled => return None,
        };
        for &next in path {
            self.set_status(id, next);
        }
        self.approvals.retain(|a| &a.run_id != id);
        if let Some(run) = self.run_mut(id) {
            run.cancel.cancel();
        }
        let done = data::done(StopReason::Interrupted, None, reason);
        self.push(id, done);
        Some(RunStatus::Cancelled)
    }

    /// Counts for `daemon.status`: runs awake (not queued, not terminal) and runs queued.
    pub(super) fn counts(&self) -> (u32, u32) {
        let count = |pred: &dyn Fn(RunStatus) -> bool| {
            let n = self.runs.iter().filter(|r| pred(r.summary.status)).count();
            u32::try_from(n).unwrap_or(u32::MAX)
        };
        (
            count(&|s| s != RunStatus::Queued && !s.is_terminal()),
            count(&|s| s == RunStatus::Queued),
        )
    }

    /// The event at `seq` of a run, counted from 1 like a real journal.
    pub(super) fn event_at(&self, id: &RunId, seq: u64) -> Option<(Seq, ExecEvent)> {
        let run = self.run(id)?;
        let at = usize::try_from(seq.checked_sub(1)?).ok()?;
        run.journal.get(at).map(|e| (Seq(seq), e.clone()))
    }
}
