//! Process records and the orphan sweep (ARCHITECTURE section 17.3).
//!
//! A [`ProcessRecord`] is what the daemon stores for each child it spawns: pid, process group and
//! start time. After a crash the daemon reads the records of the previous run and calls
//! [`sweep_orphans`], which ends the groups that are still running and leaves alone any pid that
//! has since been reused by an unrelated process.
//!
//! The start time comes from the one process-table reader this crate already has
//! (`process::start_time`: `proc_pidinfo` on macOS, `/proc/<pid>/stat` on Linux). The few `unsafe`
//! calls here are `getpgid`, `getpgrp` and `kill`, each wrapped once below with its safety
//! argument.

#![expect(
    unsafe_code,
    reason = "liveness checks and group kills for orphan sweeping need libc"
)]
#![deny(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use super::process::start_time;
use crate::{Signal, send_signal};

/// How long [`sweep_orphans`] gives a group to exit after SIGTERM before SIGKILL.
pub const DEFAULT_SWEEP_GRACE: Duration = Duration::from_secs(3);

/// How long a sweep waits for a SIGKILLed group to disappear before calling it a survivor.
const KILL_SETTLE: Duration = Duration::from_millis(500);

const POLL: Duration = Duration::from_millis(20);

/// Identity of one spawned child: enough to find it again, and to tell it from a stranger that
/// later inherited its pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ProcessRecord {
    /// The child's pid.
    pub pid: i32,
    /// Its process group. Equal to `pid` for a child spawned as its own group leader.
    pub pgid: i32,
    /// Opaque start time of the process. Two readings are the same process only if equal; the
    /// unit differs per platform (microseconds since the epoch on macOS, clock ticks since boot
    /// on Linux) and must not be compared across machines or reboots.
    pub start_time: u64,
}

impl ProcessRecord {
    /// Read the record of a live process. `None` when the process does not exist or its start
    /// time cannot be read.
    pub fn of_pid(pid: u32) -> Option<Self> {
        let pid = i32::try_from(pid).ok()?;
        // Read the start time first and the group second: a pid reused in between yields a
        // mismatched pair that the sweep will then refuse to touch.
        let start_time = start_time(pid)?;
        let pgid = process_group(pid)?;
        Some(Self {
            pid,
            pgid,
            start_time,
        })
    }
}

/// Why [`sweep_orphans`] left a record alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// The pid is alive but its start time differs: it is an unrelated process that inherited the
    /// pid.
    Reused,
    /// The recorded process is not the leader of the recorded group, so its group cannot be
    /// vouched for.
    NotLeader,
    /// The group is the sweeping process's own.
    OwnGroup,
    /// The pid or group id is not one a sweep may signal (zero, negative or 1).
    Invalid,
}

/// What [`sweep_orphans`] did, per record.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Groups that were running, were signalled, and no longer exist.
    pub killed: Vec<ProcessRecord>,
    /// Records whose process no longer exists: nothing to do.
    pub gone: Vec<ProcessRecord>,
    /// Records deliberately left alone, with the reason.
    pub skipped: Vec<(ProcessRecord, SkipReason)>,
    /// Groups still present after SIGKILL and a short wait (an unkillable or zombie member).
    pub survivors: Vec<ProcessRecord>,
}

/// End the process groups of `records` whose leader still exists with the recorded start time,
/// using [`DEFAULT_SWEEP_GRACE`]. See [`sweep_orphans_with_grace`].
pub async fn sweep_orphans(records: &[ProcessRecord]) -> SweepReport {
    sweep_orphans_with_grace(records, DEFAULT_SWEEP_GRACE).await
}

/// End the process groups of `records` whose leader still exists **and** whose start time
/// matches, so a pid reused by an unrelated process is never touched.
///
/// Each qualifying group gets SIGTERM, then SIGKILL after `grace` if it is still there. A group
/// whose leader already died is not swept: the leader is the only member whose identity the
/// record can vouch for.
///
/// The check is repeated right before every signal. It cannot be atomic with the signal, so a
/// pid reused in that instant is still possible; the window is microseconds, against the minutes
/// to days the check guards.
pub async fn sweep_orphans_with_grace(records: &[ProcessRecord], grace: Duration) -> SweepReport {
    let mut report = SweepReport::default();
    let mut remaining = Vec::new();
    for &record in records {
        match classify(record) {
            Verdict::Gone => report.gone.push(record),
            Verdict::Skip(reason) => report.skipped.push((record, reason)),
            Verdict::Sweep => remaining.push(record),
        }
    }

    // Ask politely.
    remaining.retain(|&record| match recheck(record) {
        Recheck::Leader => {
            send_signal(&-record.pgid, Signal::Term);
            true
        }
        Recheck::Vanished => {
            report.gone.push(record);
            false
        }
        Recheck::Reused => {
            report.skipped.push((record, SkipReason::Reused));
            false
        }
    });
    settle(&mut remaining, &mut report.killed, grace).await;

    // Then insist, on the groups still there. A group seen absent was dropped from `remaining`
    // and is never signalled again: its id may belong to someone else by now.
    remaining.retain(|&record| match recheck(record) {
        // The leader exiting during the grace leaves its members; the group stays ours while it
        // has any, because the kernel never hands out the id of a live group.
        Recheck::Leader | Recheck::Vanished => {
            send_signal(&-record.pgid, Signal::Kill);
            true
        }
        Recheck::Reused => {
            report.skipped.push((record, SkipReason::Reused));
            false
        }
    });
    settle(&mut remaining, &mut report.killed, KILL_SETTLE).await;
    report.survivors = remaining;
    report
}

/// Wait up to `limit` for the groups of `remaining` to disappear, moving each to `killed` the
/// first time it is seen absent.
async fn settle(
    remaining: &mut Vec<ProcessRecord>,
    killed: &mut Vec<ProcessRecord>,
    limit: Duration,
) {
    let deadline = tokio::time::Instant::now() + limit;
    loop {
        remaining.retain(|&record| {
            let present = group_exists(record.pgid);
            if !present {
                killed.push(record);
            }
            present
        });
        if remaining.is_empty() || tokio::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(POLL).await;
    }
}

/// What the recorded leader is now, checked right before a signal.
enum Recheck {
    /// Still the recorded process.
    Leader,
    /// No longer exists.
    Vanished,
    /// The pid belongs to another process now.
    Reused,
}

fn recheck(record: ProcessRecord) -> Recheck {
    match start_time(record.pid) {
        Some(start) if start == record.start_time => Recheck::Leader,
        Some(_) => Recheck::Reused,
        None => Recheck::Vanished,
    }
}

enum Verdict {
    Gone,
    Skip(SkipReason),
    Sweep,
}

fn classify(record: ProcessRecord) -> Verdict {
    // Signalling `-1`, `0` or init's group would reach far more than a child.
    if record.pid <= 1 || record.pgid <= 1 {
        return Verdict::Skip(SkipReason::Invalid);
    }
    let Some(now) = start_time(record.pid) else {
        return Verdict::Gone;
    };
    if now != record.start_time {
        return Verdict::Skip(SkipReason::Reused);
    }
    if record.pid != record.pgid || process_group(record.pid) != Some(record.pgid) {
        return Verdict::Skip(SkipReason::NotLeader);
    }
    if record.pgid == own_process_group() {
        return Verdict::Skip(SkipReason::OwnGroup);
    }
    Verdict::Sweep
}

fn process_group(pid: i32) -> Option<i32> {
    // SAFETY: getpgid only inspects; an unknown pid returns -1.
    let group = unsafe { libc::getpgid(pid) };
    (group > 0).then_some(group)
}

fn own_process_group() -> i32 {
    // SAFETY: getpgrp has no failure mode and touches nothing.
    unsafe { libc::getpgrp() }
}

/// Whether any process is in group `pgid`.
fn group_exists(pgid: i32) -> bool {
    // SAFETY: signal 0 only checks for existence. `pgid` was validated as greater than 1 by
    // `classify`, so the target `-pgid` names one group and never "every process".
    unsafe { libc::kill(-pgid, 0) == 0 }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::time::Instant;

    use super::*;
    use crate::process::owned::{self, Child};
    use crate::process::{Command, Stdio};

    const GRACE: Duration = Duration::from_millis(300);

    fn sleeper() -> Child {
        let mut command = Command::new("sleep");
        command.arg("60").stdin(Stdio::null());
        owned::spawn_grouped(&mut command).unwrap()
    }

    fn alive(pid: i32) -> bool {
        // SAFETY: signal 0 only checks for existence.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Reap `child` in the background, as the daemon's supervisor would, and hand back its exit.
    fn reap(mut child: Child) -> tokio::task::JoinHandle<std::process::ExitStatus> {
        tokio::spawn(async move { child.wait().await.unwrap() })
    }

    #[tokio::test]
    async fn the_record_of_a_live_child_names_its_group_and_start() {
        let child = sleeper();
        let pid = child.id().unwrap();
        let record = child.record().unwrap();
        assert_eq!(record.pid, pid as i32);
        assert_eq!(record.pgid, pid as i32, "spawned as its own group leader");
        assert_eq!(record, ProcessRecord::of_pid(pid).unwrap());
        assert_eq!(record.start_time, start_time(record.pid).unwrap());
        assert!(ProcessRecord::of_pid(u32::MAX).is_none());
        let mut child = child;
        child.shutdown(GRACE).await;
        assert!(child.record().is_none(), "no record once reaped");
    }

    #[tokio::test]
    async fn sweeping_a_live_record_kills_its_group() {
        let child = sleeper();
        let record = child.record().unwrap();
        let reaped = reap(child);
        let began = Instant::now();
        let report = sweep_orphans_with_grace(&[record], GRACE).await;
        assert_eq!(report.killed, vec![record], "{report:?}");
        assert!(report.gone.is_empty() && report.skipped.is_empty() && report.survivors.is_empty());
        assert!(began.elapsed() < GRACE + Duration::from_secs(2));
        let status = tokio::time::timeout(Duration::from_secs(5), reaped)
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success());
        assert!(!alive(record.pid));
    }

    #[tokio::test]
    async fn one_sweep_handles_live_reused_and_dead_records_together() {
        let live = sleeper();
        let live_record = live.record().unwrap();
        let reaped = reap(live);
        let mut dead = sleeper();
        let dead_record = dead.record().unwrap();
        dead.shutdown(GRACE).await;
        let mut stranger = sleeper();
        let real = stranger.record().unwrap();
        let reused = ProcessRecord {
            start_time: real.start_time + 1,
            ..real
        };
        let report = sweep_orphans_with_grace(&[dead_record, reused, live_record], GRACE).await;
        assert_eq!(report.killed, vec![live_record], "{report:?}");
        assert_eq!(report.gone, vec![dead_record]);
        assert_eq!(report.skipped, vec![(reused, SkipReason::Reused)]);
        assert!(stranger.try_wait().unwrap().is_none() && alive(real.pid));
        stranger.shutdown(GRACE).await;
        tokio::time::timeout(Duration::from_secs(5), reaped)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn a_record_with_the_wrong_start_time_is_left_alone() {
        let mut child = sleeper();
        let real = child.record().unwrap();
        let impostor = ProcessRecord {
            start_time: real.start_time + 1,
            ..real
        };
        let report = sweep_orphans_with_grace(&[impostor], GRACE).await;
        assert_eq!(report.skipped, vec![(impostor, SkipReason::Reused)]);
        assert!(report.killed.is_empty());
        assert!(
            child.try_wait().unwrap().is_none() && alive(real.pid),
            "a reused pid was signalled"
        );
        child.shutdown(GRACE).await;
        assert!(!alive(real.pid));
    }

    #[tokio::test]
    async fn a_record_for_a_dead_pid_is_reported_gone() {
        let mut child = sleeper();
        let record = child.record().unwrap();
        child.shutdown(GRACE).await;
        let report = sweep_orphans_with_grace(&[record], GRACE).await;
        assert_eq!(report.gone, vec![record]);
        assert!(report.killed.is_empty() && report.skipped.is_empty());
    }

    #[tokio::test]
    async fn records_that_could_reach_beyond_a_child_are_refused() {
        let own = ProcessRecord::of_pid(std::process::id()).unwrap();
        let init = ProcessRecord {
            pid: 1,
            pgid: 1,
            start_time: 0,
        };
        let everyone = ProcessRecord {
            pid: 5,
            pgid: 0,
            start_time: 0,
        };
        let negative = ProcessRecord {
            pid: 5,
            pgid: -1,
            start_time: 0,
        };
        let report = sweep_orphans_with_grace(&[own, init, everyone, negative], GRACE).await;
        assert!(
            report.killed.is_empty() && report.survivors.is_empty(),
            "{report:?}"
        );
        assert_eq!(report.skipped.len(), 4, "{report:?}");
        assert_eq!(report.skipped[1].1, SkipReason::Invalid);
        assert_eq!(report.skipped[2].1, SkipReason::Invalid);
        assert_eq!(report.skipped[3].1, SkipReason::Invalid);
        // The test process itself, in its own group or not, is never signalled.
        assert!(matches!(
            report.skipped[0].1,
            SkipReason::NotLeader | SkipReason::OwnGroup
        ));
    }

    #[tokio::test]
    async fn a_group_that_ignores_sigterm_is_killed_after_the_grace() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "trap '' TERM; sleep 60"])
            .stdin(Stdio::null());
        let child = owned::spawn_grouped(&mut command).unwrap();
        // Let the shell install its trap before the sweep's SIGTERM arrives.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let record = child.record().unwrap();
        let reaped = reap(child);
        let began = Instant::now();
        let report = sweep_orphans_with_grace(&[record], GRACE).await;
        assert_eq!(report.killed, vec![record], "{report:?}");
        assert!(began.elapsed() >= GRACE, "SIGKILL waits out the grace");
        let status = tokio::time::timeout(Duration::from_secs(5), reaped)
            .await
            .unwrap()
            .unwrap();
        assert!(!status.success());
    }
}
