//! The process-record queue between an executor and its supervisor, and the start-up sweep.
//!
//! `ProcessRecord` exists on unix only. On other platforms the queue never yields and the sweep
//! finds nothing, so the rest of the kernel carries no `cfg`.

use std::time::Duration;

use reno_harness::exec::ExecHandle;
use reno_store::RunRecord;

/// A process an executor reported: pid, process group and start time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Spawned {
    pub(crate) pid: u32,
    pub(crate) pgid: u32,
    pub(crate) start_time: u64,
}

#[cfg(unix)]
mod imp {
    use reno_harness::proc::{ProcessRecord, sweep_orphans_with_grace};
    use tokio::sync::mpsc;

    use super::*;

    /// The receiving end of a run's process-record queue.
    #[derive(Debug)]
    pub(crate) struct Records(Option<mpsc::Receiver<ProcessRecord>>);

    impl Records {
        pub(crate) fn take(handle: &ExecHandle) -> Self {
            Self(handle.take_process_records())
        }

        /// The next record; pending forever once the queue is closed or absent.
        pub(crate) async fn next(&mut self) -> Spawned {
            loop {
                let Some(rx) = self.0.as_mut() else {
                    return std::future::pending().await;
                };
                match rx.recv().await {
                    Some(record) => {
                        if let Some(spawned) = convert(record) {
                            return spawned;
                        }
                    }
                    None => self.0 = None,
                }
            }
        }

        /// A record that is already queued, without waiting.
        pub(crate) fn try_next(&mut self) -> Option<Spawned> {
            let rx = self.0.as_mut()?;
            while let Ok(record) = rx.try_recv() {
                if let Some(spawned) = convert(record) {
                    return Some(spawned);
                }
            }
            None
        }
    }

    fn convert(record: ProcessRecord) -> Option<Spawned> {
        Some(Spawned {
            pid: u32::try_from(record.pid).ok()?,
            pgid: u32::try_from(record.pgid).ok()?,
            start_time: record.start_time,
        })
    }

    /// Sweep the process groups the given runs recorded. Returns how many were killed.
    pub(crate) async fn sweep(runs: &[RunRecord], grace: Duration) -> usize {
        let records: Vec<ProcessRecord> = runs
            .iter()
            .filter_map(|run| {
                Some(ProcessRecord {
                    pid: i32::try_from(run.pid?).ok()?,
                    pgid: i32::try_from(run.pgid?).ok()?,
                    start_time: run.process_start?,
                })
            })
            .collect();
        if records.is_empty() {
            return 0;
        }
        let report = sweep_orphans_with_grace(&records, grace).await;
        for record in &report.survivors {
            tracing::warn!(
                pid = record.pid,
                pgid = record.pgid,
                "orphan survived the sweep"
            );
        }
        for (record, reason) in &report.skipped {
            tracing::warn!(pid = record.pid, ?reason, "orphan record skipped");
        }
        report.killed.len()
    }
}

#[cfg(not(unix))]
mod imp {
    use super::*;

    #[derive(Debug)]
    pub(crate) struct Records;

    impl Records {
        pub(crate) fn take(_: &ExecHandle) -> Self {
            Self
        }

        pub(crate) async fn next(&mut self) -> Spawned {
            std::future::pending().await
        }

        pub(crate) fn try_next(&mut self) -> Option<Spawned> {
            None
        }
    }

    pub(crate) async fn sweep(_: &[RunRecord], _: Duration) -> usize {
        0
    }
}

pub(crate) use imp::{Records, sweep};
