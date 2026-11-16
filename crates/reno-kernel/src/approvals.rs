//! The kernel's view of approvals: the pending list that `approval.list` and `approvals.watch`
//! show, backed by the store.
//!
//! The store is the truth (`reno_store::approvals`): a pending approval is a durable row, so a
//! request survives in the record whatever happens to the process. This module holds only the
//! `watch` that mirrors the pending rows for subscribers. Whoever changes the rows calls
//! [`Approvals::refresh`] afterwards, which reads them back and publishes them, so a subscriber
//! sees what the store holds and nothing else. Refreshes are serialised, so a later read is never
//! published before an earlier one and the last value published is the newest.
//!
//! What the supervisors do with a request (policy, the wait, the deadline) is in `supervisor`;
//! this module is the read side.

use reno_proto::{Approval, ApprovalRecord};
use reno_store::StoredApproval;
use tokio::sync::{Mutex, watch};

use crate::db::Db;
use crate::error::Result;

/// The pending approvals, as subscribers see them.
#[derive(Debug)]
pub(crate) struct Approvals {
    tx: watch::Sender<Vec<Approval>>,
    /// Held across a read and its publish, so refreshes cannot cross.
    refreshing: Mutex<()>,
}

impl Approvals {
    pub(crate) fn new() -> Self {
        Self {
            tx: watch::channel(Vec::new()).0,
            refreshing: Mutex::new(()),
        }
    }

    /// Read the pending rows from the store and publish them if they changed.
    pub(crate) async fn refresh(&self, db: &Db) -> Result<()> {
        let _turn = self.refreshing.lock().await;
        let pending: Vec<Approval> = db.pending_approvals().await?.iter().map(approval).collect();
        self.tx.send_if_modified(|current| {
            if *current == pending {
                return false;
            }
            *current = pending;
            true
        });
        Ok(())
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Vec<Approval>> {
        self.tx.subscribe()
    }
}

/// A pending row as the wire's [`Approval`].
pub(crate) fn approval(row: &StoredApproval) -> Approval {
    Approval {
        id: row.id.clone(),
        run_id: row.run_id.clone(),
        subject: row.subject.clone(),
        requested_at_ms: to_ms(row.created_at),
    }
}

/// A row, pending or decided, as the wire's [`ApprovalRecord`].
pub(crate) fn record(row: &StoredApproval) -> ApprovalRecord {
    ApprovalRecord {
        id: row.id.clone(),
        run_id: row.run_id.clone(),
        request_id: row.request_id.clone(),
        subject: row.subject.clone(),
        status: row.status,
        decided_by: row.decided_by.clone(),
        created_at_ms: to_ms(row.created_at),
        decided_at_ms: row.decided_at.map(to_ms),
        expires_at_ms: to_ms(row.expires_at),
        message: row.message.clone(),
    }
}

fn to_ms(at: i64) -> u64 {
    u64::try_from(at).unwrap_or(0)
}
