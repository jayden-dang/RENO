//! The fake daemon's state: runs with their journals, and pending approvals. Plain data and
//! synchronous methods; no I/O, no clock (time is a logical counter), no async.

use reno_proto::{
    Approval, ApprovalId, ExecEvent, IdError, RunId, RunStatus, RunSummary, Seq, StopReason, TaskId,
};
use tokio::sync::watch;
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
            })
            .collect();
        Self {
            runs,
            approvals: seed.approvals,
            next_id: 1,
            now_ms: data::NOW_MS,
            started_ms: data::NOW_MS,
        }
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

    /// Creates a queued run and returns its id.
    pub(super) fn create_run(
        &mut self,
        params: &reno_proto::TaskRunParams,
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
                executor: params.executor.clone(),
                status: RunStatus::Queued,
                attempt: 1,
                title,
                model: params.model.clone(),
                stop_reason: None,
                created_at_ms: now,
                updated_at_ms: now,
            },
            journal: vec![],
            journal_tx: watch::channel(0).0,
            cancel: root.child_token(),
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

    /// Removes and returns a pending approval.
    pub(super) fn take_approval(&mut self, id: &ApprovalId) -> Option<Approval> {
        let at = self.approvals.iter().position(|a| &a.id == id)?;
        Some(self.approvals.remove(at))
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

    /// The event at `seq` of a run, with whether the journal has ended after it.
    pub(super) fn event_at(&self, id: &RunId, seq: u64) -> Option<(Seq, ExecEvent)> {
        let run = self.run(id)?;
        let at = usize::try_from(seq).ok()?;
        run.journal.get(at).map(|e| (Seq(seq), e.clone()))
    }
}
