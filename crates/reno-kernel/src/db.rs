//! The kernel's view of the store: every write goes through here, so one place stamps the time
//! (from the `Clock`, ARCH-7) and one place counts the writes.
//!
//! The count exists for the idle test (ARCH-10): with nothing to do, the kernel must make no
//! store write at all, not even an empty claim, and a counter is the only thing that can see
//! the writes that leave no event row (heartbeats, empty claims).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use reno_proto::defs::{BotPin, DefKind};
use reno_proto::{
    ApprovalId, ApprovalOutcome, ExecutorHealth, ExecutorId, LocalZone, NotResumed, ResolvedBy,
    RoutingDecision, RuleId, RunId, RunStatus, SessionRef, StandingRule, TaskId, WorkspaceChanges, WorkspaceInfo,
    WorkspaceStatus,
};
use reno_store::{
    Applied, BoardRow, BoardWrite, Claim, ClaimFilter, DefinitionRecord, ExpireScope, Expired,
    FireResult, Journals, NewApproval, NewDefinition, NewFiring, NewRule, NewRunToken, NewTask,
    Note,
    Occupancy, Resumed, RunRecord, RunToken, Store, StoreError, StoredApproval, Tokens, TriggerRow,
};

use crate::clock::Clock;

/// A store handle plus the clock that timestamps its writes.
#[derive(Clone)]
pub(crate) struct Db {
    store: Store,
    clock: Arc<dyn Clock>,
    writes: Arc<AtomicU64>,
    /// Deadline timers armed so far, by kind. A test that advances paused time must first know
    /// the timer it means to fire exists: the store's writer is a real thread, so a task can
    /// still be waiting for a reply when its caller has run out of yields.
    #[cfg(test)]
    armed: Arc<[AtomicU64; 2]>,
}

/// The deadline timers a test can wait for.
#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum Armed {
    /// A supervisor's `max_wall_time` deadline.
    WallTime = 0,
    /// A deadline of the dispatcher: a lease running out, or the next local midnight.
    Dispatcher = 1,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db")
            .field("writes", &self.writes.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

type Result<T> = std::result::Result<T, StoreError>;

impl Db {
    pub(crate) fn new(store: Store, clock: Arc<dyn Clock>) -> Self {
        Self {
            store,
            clock,
            writes: Arc::default(),
            #[cfg(test)]
            armed: Arc::default(),
        }
    }

    /// Records that a deadline timer of `kind` has just been armed (tests only).
    #[cfg(test)]
    pub(crate) fn armed(&self, kind: Armed) {
        self.armed[kind as usize].fetch_add(1, Ordering::SeqCst);
    }

    /// How many deadline timers of `kind` have been armed so far (tests only).
    #[cfg(test)]
    pub(crate) fn armed_count(&self, kind: Armed) -> u64 {
        self.armed[kind as usize].load(Ordering::SeqCst)
    }

    pub(crate) fn now_ms(&self) -> i64 {
        self.clock.now_ms()
    }

    /// Store write calls made so far, whether they succeeded or not.
    #[cfg(test)]
    pub(crate) fn writes(&self) -> u64 {
        self.writes.load(Ordering::Relaxed)
    }

    fn wrote(&self) {
        self.writes.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn journals(&self) -> Journals {
        self.store.journals()
    }

    pub(crate) async fn create_task(&self, task: NewTask) -> Result<()> {
        self.wrote();
        self.store.create_task(task, self.now_ms()).await
    }

    pub(crate) async fn enqueue_routed_run(
        &self,
        task: &TaskId,
        run: &RunId,
        executor: &ExecutorId,
        bot: Option<&BotPin>,
        decision: &RoutingDecision,
    ) -> Result<()> {
        self.wrote();
        self.store
            .enqueue_routed_run(task, run, executor, bot, decision, self.now_ms())
            .await
    }

    pub(crate) async fn reroute_run(
        &self,
        run: &RunId,
        fence: u64,
        executor: &ExecutorId,
        decision: &RoutingDecision,
    ) -> Result<()> {
        self.wrote();
        self.store
            .reroute_run(run, fence, executor, decision, self.now_ms())
            .await
    }

    pub(crate) async fn run_routing(&self, run: &RunId) -> Result<Option<RoutingDecision>> {
        self.store.run_routing(run).await
    }

    pub(crate) async fn apply_definitions(
        &self,
        desired: Vec<NewDefinition>,
        principal: &str,
    ) -> Result<Applied> {
        self.wrote();
        self.store
            .apply_definitions(desired, Some(principal), self.now_ms())
            .await
    }

    pub(crate) async fn current_definitions(&self) -> Result<Vec<DefinitionRecord>> {
        self.store.current_definitions(None).await
    }

    pub(crate) async fn get_definition(
        &self,
        kind: DefKind,
        name: &str,
        version: u32,
    ) -> Result<Option<DefinitionRecord>> {
        self.store.get_definition(kind, name, version).await
    }

    pub(crate) async fn latest_definition(
        &self,
        kind: DefKind,
        name: &str,
    ) -> Result<Option<DefinitionRecord>> {
        self.store.latest_definition(kind, name).await
    }

    /// The claim with the budget filter: paused bots, bots at their concurrency limit and bots
    /// past their daily tokens are passed over (`reno_store::ClaimFilter`).
    pub(crate) async fn claim_next_filtered(
        &self,
        owner: &str,
        lease_ms: u64,
        filter: &ClaimFilter,
    ) -> Result<Option<Claim>> {
        self.wrote();
        self.store
            .claim_next_filtered(owner, self.now_ms(), lease_ms, filter)
            .await
    }

    /// The zone of the budget day.
    pub(crate) fn zone(&self) -> LocalZone {
        self.clock.zone()
    }

    /// The budget day now, `YYYY-MM-DD`.
    pub(crate) fn today(&self) -> String {
        self.zone().day_of(self.now_ms())
    }

    pub(crate) async fn record_usage(
        &self,
        run: &RunId,
        fence: u64,
        bot: Option<&str>,
        add: Tokens,
    ) -> Result<Tokens> {
        self.wrote();
        let now = self.now_ms();
        let day = self.zone().day_of(now);
        self.store
            .record_usage(run, fence, bot, &day, add, now)
            .await
    }

    pub(crate) async fn bot_usage(&self, day: &str) -> Result<BTreeMap<String, Tokens>> {
        self.store.bot_usage(day).await
    }

    pub(crate) async fn claimable_leases(&self, filter: &ClaimFilter) -> Result<Vec<(RunId, i64)>> {
        self.store.claimable_leases(self.now_ms(), filter).await
    }

    pub(crate) async fn occupancy(&self) -> Result<BTreeMap<String, Occupancy>> {
        self.store.occupancy().await
    }

    pub(crate) async fn token_blocked_bots(&self, day: &str) -> Result<Vec<String>> {
        self.store.token_blocked_bots(day).await
    }

    pub(crate) async fn pause(&self, scope: &str) -> Result<bool> {
        self.wrote();
        self.store.pause(scope, self.now_ms()).await
    }

    pub(crate) async fn resume(&self, scope: &str) -> Result<bool> {
        self.wrote();
        self.store.resume(scope, self.now_ms()).await
    }

    pub(crate) async fn pauses(&self) -> Result<BTreeMap<String, i64>> {
        self.store.pauses().await
    }

    pub(crate) async fn skip_trigger_occurrence(
        &self,
        trigger: &str,
        scheduled_at: i64,
        missed: u32,
        next_due_at: Option<i64>,
        reason: &str,
    ) -> Result<()> {
        self.wrote();
        self.store
            .skip_trigger_occurrence(
                trigger,
                scheduled_at,
                missed,
                next_due_at,
                reason,
                self.now_ms(),
            )
            .await
    }

    pub(crate) async fn hold_trigger_occurrence(&self, trigger: &str, due_at: i64) -> Result<()> {
        self.wrote();
        self.store
            .hold_trigger_occurrence(trigger, due_at, self.now_ms())
            .await
    }

    pub(crate) async fn heartbeat(&self, run: &RunId, fence: u64, lease_ms: u64) -> Result<()> {
        self.wrote();
        self.store
            .heartbeat(run, fence, self.now_ms(), lease_ms)
            .await
    }

    pub(crate) async fn transition(&self, run: &RunId, fence: u64, to: RunStatus) -> Result<()> {
        self.wrote();
        self.store.transition(run, fence, to, self.now_ms()).await
    }

    /// A transition that also records why the run stopped (and will not be resumed).
    pub(crate) async fn transition_noted(
        &self,
        run: &RunId,
        fence: u64,
        to: RunStatus,
        note: Note,
    ) -> Result<()> {
        self.wrote();
        self.store
            .transition_noted(run, fence, to, note, self.now_ms())
            .await
    }

    pub(crate) async fn record_session(
        &self,
        run: &RunId,
        fence: u64,
        session: &SessionRef,
    ) -> Result<()> {
        self.wrote();
        self.store
            .record_session(run, fence, session, self.now_ms())
            .await
    }

    pub(crate) async fn resume_run(
        &self,
        run: &RunId,
        fence: u64,
        prompt: Option<String>,
    ) -> Result<Resumed> {
        self.wrote();
        self.store
            .resume_run(run, fence, prompt, self.now_ms())
            .await
    }

    pub(crate) async fn set_not_resumed(
        &self,
        run: &RunId,
        fence: u64,
        why: &NotResumed,
    ) -> Result<()> {
        self.wrote();
        self.store
            .set_not_resumed(run, fence, why, self.now_ms())
            .await
    }

    pub(crate) async fn record_process(
        &self,
        run: &RunId,
        fence: u64,
        pid: u32,
        pgid: u32,
        start_time: u64,
    ) -> Result<()> {
        self.wrote();
        self.store
            .record_process(run, fence, pid, pgid, start_time, self.now_ms())
            .await
    }

    pub(crate) async fn get_run(&self, run: &RunId) -> Result<Option<RunRecord>> {
        self.store.get_run(run).await
    }

    pub(crate) async fn list_runs(&self, status: Option<RunStatus>) -> Result<Vec<RunRecord>> {
        self.store.list_runs(status).await
    }

    pub(crate) async fn list_runs_page(
        &self,
        status: &[RunStatus],
        before: Option<&RunId>,
        limit: u32,
    ) -> Result<Vec<RunRecord>> {
        self.store.list_runs_page(status, before, limit).await
    }

    pub(crate) async fn count_runs(&self, status: &[RunStatus]) -> Result<u64> {
        self.store.count_runs(status).await
    }

    pub(crate) async fn nonterminal_runs(&self) -> Result<Vec<RunRecord>> {
        self.store.nonterminal_runs().await
    }

    pub(crate) async fn create_approval(&self, new: NewApproval, fence: u64) -> Result<()> {
        self.wrote();
        self.store.create_approval(new, fence, self.now_ms()).await
    }

    pub(crate) async fn record_approval(
        &self,
        new: NewApproval,
        outcome: ApprovalOutcome,
        by: ResolvedBy,
        fence: u64,
    ) -> Result<()> {
        self.wrote();
        self.store
            .record_approval(new, outcome, by, fence, self.now_ms())
            .await
    }

    pub(crate) async fn resolve_approval(
        &self,
        id: &ApprovalId,
        outcome: ApprovalOutcome,
        by: ResolvedBy,
        message: Option<String>,
        fence: u64,
    ) -> Result<()> {
        self.wrote();
        self.store
            .resolve_approval(id, outcome, by, message, fence, self.now_ms())
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn resolve_approval_remembering(
        &self,
        id: &ApprovalId,
        outcome: ApprovalOutcome,
        by: ResolvedBy,
        message: Option<String>,
        rule: NewRule,
        fence: u64,
    ) -> Result<()> {
        self.wrote();
        self.store
            .resolve_approval_remembering(id, outcome, by, message, rule, fence, self.now_ms())
            .await
    }

    pub(crate) async fn revoke_rule(&self, id: &RuleId) -> Result<()> {
        self.wrote();
        self.store.revoke_rule(id, self.now_ms()).await
    }

    pub(crate) async fn list_rules(&self, include_inactive: bool) -> Result<Vec<StandingRule>> {
        self.store.list_rules(include_inactive, self.now_ms()).await
    }

    /// The standing rules in force now that apply to `run`, a run of `bot`.
    pub(crate) async fn active_rules(
        &self,
        run: &RunId,
        bot: Option<&str>,
    ) -> Result<Vec<StandingRule>> {
        self.store.active_rules(run, bot, self.now_ms()).await
    }

    pub(crate) async fn expire_pending(&self, scope: ExpireScope) -> Result<Vec<Expired>> {
        self.wrote();
        self.store.expire_pending(scope, self.now_ms()).await
    }

    pub(crate) async fn pending_approvals(&self) -> Result<Vec<StoredApproval>> {
        self.store.pending_approvals().await
    }

    pub(crate) async fn get_approval(&self, id: &ApprovalId) -> Result<Option<StoredApproval>> {
        self.store.get_approval(id).await
    }

    pub(crate) async fn approval_history(
        &self,
        run: Option<&RunId>,
        limit: u32,
    ) -> Result<Vec<StoredApproval>> {
        self.store.approval_history(run, limit).await
    }

    pub(crate) async fn list_triggers(&self) -> Result<Vec<TriggerRow>> {
        self.store.list_triggers().await
    }

    pub(crate) async fn plan_triggers(
        &self,
        rows: Vec<TriggerRow>,
        removed: Vec<String>,
    ) -> Result<()> {
        self.wrote();
        self.store.plan_triggers(rows, removed, self.now_ms()).await
    }

    pub(crate) async fn fire_trigger(&self, fire: NewFiring) -> Result<FireResult> {
        self.wrote();
        self.store.fire_trigger(fire, self.now_ms()).await
    }

    pub(crate) async fn trigger_start_failed(
        &self,
        name: &str,
        run: Option<&RunId>,
        failures: u32,
        backoff_until: i64,
        reason: &str,
    ) -> Result<bool> {
        self.wrote();
        self.store
            .trigger_start_failed(name, run, failures, backoff_until, reason, self.now_ms())
            .await
    }

    pub(crate) async fn trigger_recovered(&self, name: &str, run: &RunId) -> Result<bool> {
        self.wrote();
        self.store.trigger_recovered(name, run, self.now_ms()).await
    }

    pub(crate) async fn begin_workspace(&self, info: WorkspaceInfo) -> Result<()> {
        self.wrote();
        self.store.begin_workspace(info, self.now_ms()).await
    }

    pub(crate) async fn set_workspace_status(
        &self,
        run: &RunId,
        status: WorkspaceStatus,
        changes: Option<WorkspaceChanges>,
    ) -> Result<WorkspaceInfo> {
        self.wrote();
        self.store
            .set_workspace_status(run, status, changes, self.now_ms())
            .await
    }

    pub(crate) async fn record_in_place(
        &self,
        run: &RunId,
        cwd: String,
        reason: &str,
    ) -> Result<()> {
        self.wrote();
        self.store
            .record_in_place(run, cwd, reason, self.now_ms())
            .await
    }

    pub(crate) async fn get_workspace(&self, run: &RunId) -> Result<Option<WorkspaceInfo>> {
        self.store.get_workspace(run).await
    }

    pub(crate) async fn list_workspaces(
        &self,
        statuses: &[WorkspaceStatus],
    ) -> Result<Vec<WorkspaceInfo>> {
        self.store.list_workspaces(statuses).await
    }

    pub(crate) async fn put_executor_health(
        &self,
        executor: &ExecutorId,
        health: &ExecutorHealth,
    ) -> Result<()> {
        self.wrote();
        self.store
            .put_executor_health(executor, health, self.now_ms())
            .await
    }

    pub(crate) async fn executor_health(&self) -> Result<Vec<(ExecutorId, ExecutorHealth)>> {
        self.store.executor_health().await
    }

    pub(crate) async fn was_denied(&self, run: &RunId, digest: &str) -> Result<bool> {
        self.store.was_denied(run, digest).await
    }

    pub(crate) async fn put_run_token(&self, token: NewRunToken) -> Result<()> {
        self.wrote();
        self.store.put_run_token(token, self.now_ms()).await
    }

    pub(crate) async fn run_token(
        &self,
        run: &RunId,
        token_hash: &str,
    ) -> Result<Option<RunToken>> {
        self.store.run_token(run, token_hash).await
    }

    pub(crate) async fn revoke_run_tokens(&self, run: &RunId) -> Result<u64> {
        self.wrote();
        self.store.revoke_run_tokens(run, self.now_ms()).await
    }

    pub(crate) async fn revoke_open_run_tokens(&self) -> Result<Vec<RunId>> {
        self.wrote();
        self.store.revoke_open_run_tokens(self.now_ms()).await
    }

    pub(crate) async fn board_get(&self, scope: &str, key: &str) -> Result<Option<BoardRow>> {
        self.store.board_get(scope, key).await
    }

    pub(crate) async fn board_list(&self, scope: &str) -> Result<Vec<BoardRow>> {
        self.store.board_list(scope).await
    }

    pub(crate) async fn board_put(
        &self,
        scope: &str,
        key: &str,
        value: &str,
        expected: Option<u64>,
        run: &RunId,
    ) -> Result<BoardWrite> {
        self.wrote();
        self.store
            .board_put(scope, key, value, expected, run, self.now_ms())
            .await
    }
}
