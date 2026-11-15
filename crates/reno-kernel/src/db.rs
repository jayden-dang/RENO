//! The kernel's view of the store: every write goes through here, so one place stamps the time
//! (from the `Clock`, ARCH-7) and one place counts the writes.
//!
//! The count exists for the idle test (ARCH-10): with nothing to do, the kernel must make no
//! store write at all, not even an empty claim, and a counter is the only thing that can see
//! the writes that leave no event row (heartbeats, empty claims).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use reno_proto::defs::{BotPin, DefKind};
use reno_proto::{
    ApprovalId, ApprovalOutcome, ExecutorId, ResolvedBy, RunId, RunStatus, TaskId,
    WorkspaceChanges, WorkspaceInfo, WorkspaceStatus,
};
use reno_store::{
    Applied, Claim, DefinitionRecord, ExpireScope, Expired, FireResult, Journals, NewApproval,
    NewDefinition, NewFiring, NewTask, RunRecord, Store, StoreError, StoredApproval, TriggerRow,
};

use crate::clock::Clock;

/// A store handle plus the clock that timestamps its writes.
#[derive(Clone)]
pub(crate) struct Db {
    store: Store,
    clock: Arc<dyn Clock>,
    writes: Arc<AtomicU64>,
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
        }
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

    pub(crate) async fn enqueue_run(
        &self,
        task: &TaskId,
        run: &RunId,
        executor: &ExecutorId,
    ) -> Result<()> {
        self.wrote();
        self.store
            .enqueue_run(task, run, executor, self.now_ms())
            .await
    }

    pub(crate) async fn enqueue_pinned_run(
        &self,
        task: &TaskId,
        run: &RunId,
        executor: &ExecutorId,
        bot: &BotPin,
    ) -> Result<()> {
        self.wrote();
        self.store
            .enqueue_pinned_run(task, run, executor, bot, self.now_ms())
            .await
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

    pub(crate) async fn claim_next(&self, owner: &str, lease_ms: u64) -> Result<Option<Claim>> {
        self.wrote();
        self.store.claim_next(owner, self.now_ms(), lease_ms).await
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
        fence: u64,
    ) -> Result<()> {
        self.wrote();
        self.store
            .resolve_approval(id, outcome, by, fence, self.now_ms())
            .await
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

    pub(crate) async fn was_denied(&self, run: &RunId, digest: &str) -> Result<bool> {
        self.store.was_denied(run, digest).await
    }
}
