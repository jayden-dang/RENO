//! Pending approvals: in memory in Phase 0 (persistence is T1.6).
//!
//! A pending approval lives exactly as long as the run supervisor that raised it: the
//! supervisor adds it when the executor asks, removes it when it is decided, and removes every
//! approval of its run when the run ends. A daemon restart therefore forgets them, which is
//! sound: recovery interrupts every run that was waiting on one.
//!
//! The mutex guards a small table and is never held across an `.await`.

use std::sync::{Mutex, MutexGuard, PoisonError};

use reno_proto::{Approval, ApprovalId, RunId};
use tokio::sync::watch;

#[derive(Debug)]
struct Pending {
    approval: Approval,
}

/// The pending approvals and the `watch` that mirrors them.
#[derive(Debug)]
pub(crate) struct Approvals {
    pending: Mutex<Vec<Pending>>,
    tx: watch::Sender<Vec<Approval>>,
}

impl Approvals {
    pub(crate) fn new() -> Self {
        Self {
            pending: Mutex::new(Vec::new()),
            tx: watch::channel(Vec::new()).0,
        }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<Pending>> {
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Publish the table to watchers, still holding its lock so updates arrive in order.
    fn publish(&self, table: &[Pending]) {
        self.tx
            .send_replace(table.iter().map(|p| p.approval.clone()).collect());
    }

    pub(crate) fn add(&self, approval: Approval) {
        let mut table = self.lock();
        table.push(Pending { approval });
        self.publish(&table);
    }

    /// The run that raised `id`, if it is still pending.
    pub(crate) fn run_of(&self, id: &ApprovalId) -> Option<RunId> {
        self.lock()
            .iter()
            .find(|p| p.approval.id == *id)
            .map(|p| p.approval.run_id.clone())
    }

    /// Forget `id`. Returns whether it was pending.
    pub(crate) fn remove(&self, id: &ApprovalId) -> bool {
        let mut table = self.lock();
        let before = table.len();
        table.retain(|p| p.approval.id != *id);
        let removed = table.len() != before;
        if removed {
            self.publish(&table);
        }
        removed
    }

    /// Forget everything `run` is waiting on.
    pub(crate) fn remove_run(&self, run: &RunId) {
        let mut table = self.lock();
        let before = table.len();
        table.retain(|p| p.approval.run_id != *run);
        if table.len() != before {
            self.publish(&table);
        }
    }

    /// Forget everything (a hard shutdown dropped the supervisors without their cleanup).
    pub(crate) fn clear(&self) {
        let mut table = self.lock();
        if !table.is_empty() {
            table.clear();
            self.publish(&table);
        }
    }

    /// The pending approvals, oldest first.
    pub(crate) fn list(&self) -> Vec<Approval> {
        self.lock().iter().map(|p| p.approval.clone()).collect()
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Vec<Approval>> {
        self.tx.subscribe()
    }
}
