//! The kernel's view of the store: every write goes through here, so one place stamps the time
//! (from the `Clock`, ARCH-7) and one place counts the writes.
//!
//! The count exists for the idle test (ARCH-10): with nothing to do, the kernel must make no
//! store write at all, not even an empty claim, and a counter is the only thing that can see
//! the writes that leave no event row (heartbeats, empty claims).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use reno_proto::{ExecutorId, RunId, RunStatus, TaskId};
use reno_store::{Claim, Journals, NewTask, RunRecord, Store, StoreError};

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
}
