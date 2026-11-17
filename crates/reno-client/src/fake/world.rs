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
    /// The standing rules, oldest first.
    rules: Vec<reno_proto::StandingRule>,
    /// Everything is paused (`budget.pause` with no bot).
    paused: bool,
    /// Bots paused by themselves.
    paused_bots: std::collections::BTreeSet<String>,
    /// The names of the secrets that are set. The fake never keeps a value: nothing reads one.
    secrets: std::collections::BTreeSet<String>,
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
        let mut world = Self {
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
            rules: seed.extras.rules,
            paused: false,
            paused_bots: std::collections::BTreeSet::new(),
            secrets: std::collections::BTreeSet::new(),
        };
        for at in 0..world.runs.len() {
            let routing = world.seed_routing(at);
            world.runs[at].routing = Some(routing);
        }
        world
    }

    /// The decision a seeded run is shown with: the demo router's answer for its bot, bent to
    /// the executor the run really has. `builder0` also shows a fallback walk: its first choice
    /// would not start.
    fn seed_routing(&self, at: usize) -> reno_proto::RoutingDecision {
        use reno_proto::{FallbackStep, RejectReason, Rejection, StartFailureKind};
        let summary = &self.runs[at].summary;
        let mut decision = self.route(&reno_proto::TaskRunParams {
            bot: summary.bot.as_ref().map(|b| b.name.clone()),
            ..reno_proto::TaskRunParams::default()
        });
        decision.at_ms = summary.created_at_ms;
        if decision.chosen.as_ref() != Some(&summary.executor) {
            decision.chosen = Some(summary.executor.clone());
            decision.ranked = vec![summary.executor.clone()];
            decision.rejected.clear();
        }
        if summary.run_id.to_string().ends_with("builder0")
            && let Ok(grok) = "acp.grok".parse::<ExecutorId>()
        {
            let reason = RejectReason::StartFailed {
                failure: StartFailureKind::NoEligibleExecutor,
                detail: "grok 0.4.0 is older than 0.9.0".to_owned(),
            };
            decision.rejected = vec![Rejection {
                executor: grok.clone(),
                reason: reason.clone(),
            }];
            decision.fallbacks = vec![FallbackStep {
                from: grok,
                reason,
                to: Some(summary.executor.clone()),
                at_ms: summary.created_at_ms + 2_000,
            }];
        }
        decision
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

    /// A simplified router: the named executor, else the bot's executors in its order of
    /// preference (the demo bots), else the two default executors; each is dropped for what its
    /// health or access modes say. The real daemon's router is in `reno-kernel`.
    pub(super) fn route(&self, params: &reno_proto::TaskRunParams) -> reno_proto::RoutingDecision {
        use reno_proto::{Ineligible, RejectReason, Rejection, RuleMatched};
        let (rule_matched, order) = match &params.executor {
            Some(executor) => (RuleMatched::ExplicitExecutor, vec![executor.clone()]),
            None => match params
                .bot
                .as_deref()
                .and_then(|b| data::bot_preference(b).map(|order| (b, order)))
            {
                Some((bot, order)) => (
                    RuleMatched::BotPreference {
                        bot: bot.to_owned(),
                    },
                    order.iter().filter_map(|id| id.parse().ok()).collect(),
                ),
                None => (
                    RuleMatched::Default,
                    data::DEFAULT_EXECUTORS
                        .iter()
                        .filter_map(|id| id.parse().ok())
                        .collect(),
                ),
            },
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
            warnings: Vec::new(),
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
                bot: params.bot.as_ref().map(|name| reno_proto::defs::BotPin {
                    name: name.clone(),
                    version: 1,
                }),
                cwd: None,
                created_at_ms: now,
                updated_at_ms: now,
                stop: None,
                not_resumed: None,
                session: None,
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
        if let ExecEvent::Done { stop, reason, .. } = &event {
            run.summary.stop_reason = Some(*stop);
            run.summary.stop.clone_from(reason);
        }
        if let ExecEvent::Started { session, .. } = &event {
            run.summary.session = Some(session.clone());
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
    ///
    /// With `remember` the answer also becomes a standing rule (the fake never judges anything
    /// by it; the list is for clients to show and revoke). `Err(Some(why))` for a request the
    /// real daemon refuses, `Err(None)` for an approval that is not pending.
    pub(super) fn take_approval(
        &mut self,
        id: &ApprovalId,
        decision: ApprovalDecision,
        message: Option<String>,
        remember: Option<reno_proto::Remember>,
    ) -> Result<Approval, Option<String>> {
        use reno_proto::{
            RULE_MAX_EXPIRY_SECS, RememberScope, RuleId, RuleMatcher, StandingEffect, StandingRule,
            StandingScope,
        };
        let at = self
            .approvals
            .iter()
            .position(|a| &a.id == id)
            .ok_or(None)?;
        let rule_scope = match &remember {
            None => None,
            Some(remember) => {
                if remember
                    .expires_in_secs
                    .is_some_and(|secs| !(1..=RULE_MAX_EXPIRY_SECS).contains(&secs))
                {
                    return Err(Some("a rule's expiry is out of range".to_owned()));
                }
                Some(match remember.scope {
                    RememberScope::Run => StandingScope::Run {
                        run: self.approvals[at].run_id.clone(),
                    },
                    RememberScope::Bot => StandingScope::Bot {
                        bot: self
                            .run(&self.approvals[at].run_id)
                            .and_then(|r| r.summary.bot.as_ref().map(|b| b.name.clone()))
                            .ok_or_else(|| Some("this run has no bot".to_owned()))?,
                    },
                    RememberScope::Always => StandingScope::Always,
                })
            }
        };
        let approval = self.approvals.remove(at);
        let now = self.tick();
        if let (Some(scope), Some(remember)) = (rule_scope, &remember) {
            let n = self.next_id;
            self.next_id += 1;
            if let Ok(rule_id) = RuleId::from_suffix(&format!("fake{n}")) {
                self.rules.push(StandingRule {
                    id: rule_id,
                    scope,
                    effect: match decision {
                        ApprovalDecision::ApproveOnce => StandingEffect::Allow,
                        ApprovalDecision::Deny => StandingEffect::Deny,
                    },
                    matcher: RuleMatcher::derive(&approval.subject, std::path::Path::new("/work")),
                    created_by: "human".to_owned(),
                    created_at_ms: now,
                    expires_at_ms: remember.expires_in_secs.map(|s| now + s * 1_000),
                    revoked_at_ms: None,
                    source_approval: Some(approval.id.clone()),
                });
            }
        }
        self.history.push(ApprovalRecord {
            id: approval.id.clone(),
            run_id: approval.run_id.clone(),
            request_id: format!("req-{}", approval.id),
            subject_digest: Some(approval.subject.digest()),
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
        Ok(approval)
    }

    /// `rule.list`: newest first; only the ones in force unless `all`.
    pub(super) fn rules(&self, all: bool) -> Vec<reno_proto::StandingRule> {
        let now = self.now_ms;
        let mut rules: Vec<_> = self
            .rules
            .iter()
            .filter(|r| all || r.is_active(now))
            .cloned()
            .collect();
        rules.reverse();
        rules
    }

    /// `rule.revoke`. `false` when there is no such rule.
    pub(super) fn revoke_rule(&mut self, id: &reno_proto::RuleId) -> bool {
        let now = self.tick();
        match self.rules.iter_mut().find(|r| &r.id == id) {
            Some(rule) => {
                rule.revoked_at_ms.get_or_insert(now);
                true
            }
            None => false,
        }
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
            attachments: Vec::new(),
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
        let done = data::done_because(
            StopReason::Interrupted,
            None,
            reason,
            Some(reno_proto::StopDetail::Cancelled {
                by: reno_proto::CancelledBy::User,
            }),
        );
        self.push(id, done);
        Some(RunStatus::Cancelled)
    }

    /// `run.resume`: queues an interrupted (or failed) run that has a session again, journals
    /// the `Resumed` and returns the attempt it will be.
    pub(super) fn resume_run(&mut self, id: &RunId) -> Result<u32, reno_proto::NotResumed> {
        let run = self.run(id).ok_or(reno_proto::NotResumed::NoSession)?;
        let status = run.summary.status;
        if !matches!(status, RunStatus::Interrupted | RunStatus::Failed) {
            return Err(reno_proto::NotResumed::WrongStatus { status });
        }
        let Some(session) = run.summary.session.clone() else {
            return Err(reno_proto::NotResumed::NoSession);
        };
        let attempt = run.summary.attempt;
        self.set_status(id, RunStatus::Queued);
        if let Some(run) = self.run_mut(id) {
            run.summary.stop = None;
            run.summary.not_resumed = None;
            run.summary.attempt = attempt + 1;
        }
        self.push(
            id,
            ExecEvent::Resumed {
                from_attempt: attempt,
                session,
            },
        );
        Ok(attempt + 1)
    }

    /// `budget.status`: each demo bot with its live and queued runs and its pause. The fake
    /// counts no tokens (every bot shows none spent) and enforces nothing: the pause flags and
    /// `stop_live` are all it models.
    pub(super) fn budget_status(&self) -> reno_proto::BudgetStatusResult {
        let bots = data::BOTS
            .iter()
            .map(|name| {
                let of_bot =
                    |r: &&RunRecord| r.summary.bot.as_ref().is_some_and(|pin| pin.name == *name);
                let live = self
                    .runs
                    .iter()
                    .filter(of_bot)
                    .filter(|r| {
                        let s = r.summary.status;
                        s != RunStatus::Queued && !s.is_terminal() && s != RunStatus::Interrupted
                    })
                    .count();
                let queued = self
                    .runs
                    .iter()
                    .filter(of_bot)
                    .filter(|r| r.summary.status == RunStatus::Queued)
                    .count();
                reno_proto::BotBudgetStatus {
                    bot: (*name).to_owned(),
                    live_runs: u32::try_from(live).unwrap_or(u32::MAX),
                    max_concurrent_runs: 1,
                    queued_runs: u32::try_from(queued).unwrap_or(u32::MAX),
                    tokens_today: 0,
                    cache_read_tokens_today: 0,
                    cache_write_tokens_today: 0,
                    daily_token_ceiling: None,
                    max_wall_time_ms: 45 * 60_000,
                    paused: self.paused_bots.contains(*name),
                }
            })
            .collect();
        reno_proto::BudgetStatusResult {
            paused: self.paused,
            day: "2026-11-16".to_owned(),
            day_ends_at_ms: self.now_ms + 24 * 3_600_000,
            default_wall_time_ms: 4 * 3_600_000,
            cache_read_weight_permille: 100,
            bots,
        }
    }

    /// `budget.pause`. `Err` for a bot the fake does not have.
    pub(super) fn pause(
        &mut self,
        bot: Option<&str>,
        stop_live: bool,
    ) -> Result<reno_proto::BudgetPauseResult, String> {
        let changed = match bot {
            Some(name) if data::BOTS.contains(&name) => self.paused_bots.insert(name.to_owned()),
            Some(name) => return Err(format!("no such bot {name:?}")),
            None => !std::mem::replace(&mut self.paused, true),
        };
        let mut stopped = Vec::new();
        if stop_live {
            let live: Vec<RunId> = self
                .runs
                .iter()
                .filter(|r| {
                    let s = r.summary.status;
                    s != RunStatus::Queued && !s.is_terminal() && s != RunStatus::Interrupted
                })
                .filter(|r| {
                    bot.is_none_or(|name| r.summary.bot.as_ref().is_some_and(|b| b.name == name))
                })
                .map(|r| r.summary.run_id.clone())
                .collect();
            for id in live {
                if self.cancel(&id, Some("paused")).is_some() {
                    stopped.push(id);
                }
            }
        }
        Ok(reno_proto::BudgetPauseResult { changed, stopped })
    }

    /// `budget.resume`.
    pub(super) fn resume(&mut self, bot: Option<&str>) -> reno_proto::BudgetResumeResult {
        let changed = match bot {
            Some(name) => self.paused_bots.remove(name),
            None => std::mem::replace(&mut self.paused, false),
        };
        reno_proto::BudgetResumeResult { changed }
    }

    /// `secret.list`: the names set, plus the secrets the demo bots ask for.
    pub(super) fn secret_list(&self) -> reno_proto::SecretListResult {
        let asked: Vec<(&str, &str)> = data::BOT_SECRETS.to_vec();
        let mut names: std::collections::BTreeSet<&str> =
            self.secrets.iter().map(String::as_str).collect();
        names.extend(asked.iter().map(|(_, secret)| *secret));
        let secrets = names
            .into_iter()
            .map(|name| reno_proto::SecretInfo {
                name: name.to_owned(),
                set: self.secrets.contains(name),
                used_by: asked
                    .iter()
                    .filter(|(_, secret)| *secret == name)
                    .map(|(bot, _)| (*bot).to_owned())
                    .collect(),
            })
            .collect();
        reno_proto::SecretListResult { secrets }
    }

    /// `secret.set`. Whether the name was already set.
    pub(super) fn secret_set(&mut self, name: &str) -> bool {
        !self.secrets.insert(name.to_owned())
    }

    /// `secret.delete`. Whether the name was set.
    pub(super) fn secret_delete(&mut self, name: &str) -> bool {
        self.secrets.remove(name)
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
