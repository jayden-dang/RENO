//! Kernel tunables. Every default is a number from ARCHITECTURE 4.1 and 17.2.

use std::path::PathBuf;
use std::time::Duration;

use reno_proto::{AccessMode, SandboxLevel};

/// How the kernel runs. `Default` is what the daemon uses.
#[derive(Debug, Clone)]
pub struct KernelConfig {
    /// Runs the dispatcher keeps live at once. At least 1.
    pub max_concurrent: usize,
    /// The name this kernel claims runs under (`runs.lease_owner`).
    pub owner: String,
    /// How long a claim holds a run. A live run is heartbeated every third of it (20 s of 60 s).
    pub lease: Duration,
    /// How long a cancelled run's executor has to end its stream before the supervisor drops the
    /// stream and ends the run itself, `interrupted`.
    pub cancel_grace: Duration,
    /// How long `shutdown` waits for runs to end before it drops them where they stand.
    pub shutdown_deadline: Duration,
    /// How long the start-up orphan sweep waits after SIGTERM before SIGKILL.
    pub sweep_grace: Duration,
    /// How long the dispatcher waits before it retries after the store failed it. A deadline that
    /// exists only while the store is failing.
    pub store_retry: Duration,
    /// Events one `run_events` subscriber may have queued; past that it gets a gap error
    /// (ARCH-12). At least 1.
    pub subscriber_queue: usize,
    /// Most non-terminal runs `watch_runs` carries, a bound on a long queue.
    pub watch_limit: u32,
    /// Terminal runs `watch_runs` carries besides the non-terminal ones: the newest this many.
    pub recent_runs: u32,
    /// The access mode a run with no bot asks its executor for; the strictest by default
    /// (ARCH-15: an executor that cannot honour it is refused). A run for a bot asks for its
    /// pinned bot's `access` instead.
    pub access: AccessMode,
    /// The sandbox ceiling a run with no bot asks for; `Unrestricted` (the default) adds none
    /// beyond the access mode. A run for a bot asks for its bot's `sandbox`.
    pub sandbox: SandboxLevel,
    /// How long an approval that asks a human waits before it is denied as expired, when the
    /// policy sets no `approval_ttl` of its own. A deadline, not a poll (ARCH-10).
    pub approval_ttl: Duration,
    /// The working directory of the runs triggers start. The daemon points it at its workspace.
    pub trigger_cwd: PathBuf,
    /// Reno's own scratch directory, the working directory of a run that names none. A run whose
    /// directory is under it works there in place, with no worktree: it is not the user's
    /// checkout. `None` for a kernel with no scratch directory.
    pub scratch_root: Option<PathBuf>,
    /// The executors a task with neither a bot nor a named executor may go to, in order of
    /// preference. The router's `Default` rule considers only these; any other executor is
    /// reached by naming it or through a bot. Empty (the default) means no task goes to an
    /// executor unasked.
    pub default_executors: Vec<reno_proto::ExecutorId>,
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 4,
            owner: "reno-kernel".to_owned(),
            lease: Duration::from_secs(60),
            cancel_grace: Duration::from_secs(10),
            shutdown_deadline: Duration::from_secs(15),
            sweep_grace: Duration::from_secs(3),
            store_retry: Duration::from_secs(1),
            subscriber_queue: 256,
            watch_limit: 500,
            recent_runs: 50,
            access: AccessMode::Supervised,
            sandbox: SandboxLevel::Unrestricted,
            approval_ttl: Duration::from_secs(30 * 60),
            trigger_cwd: PathBuf::from("."),
            scratch_root: None,
            default_executors: Vec::new(),
        }
    }
}

impl KernelConfig {
    /// The lease in milliseconds, as the store takes it.
    pub(crate) fn lease_ms(&self) -> u64 {
        u64::try_from(self.lease.as_millis()).unwrap_or(u64::MAX)
    }

    /// The heartbeat period: a third of the lease, never zero.
    pub(crate) fn heartbeat_period(&self) -> Duration {
        (self.lease / 3).max(Duration::from_millis(1))
    }
}
