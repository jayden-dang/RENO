//! Shared helpers: a daemon served in-process on a temp socket, and executors with scripts the
//! tests drive.
//!
//! Tests run on tokio's multi-thread runtime, like the real daemon. Nothing here sleeps to wait
//! for a condition: tests follow the daemon's own streams, and the timeouts are only there to
//! turn a hang into a failure.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used)]

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use reno_client::Client;
use reno_harness::exec::{MockExecutor, MockStep};
use reno_kernel::{ExecutorRegistry, KernelConfig};
use reno_proto::{
    ActionClass, ApprovalSubject, ExecEvent, ExecutorId, RunEventItem, RunId, RunStatus, Seq,
    StopReason, TaskRunParams,
};
use renod::executors::{mock_executor, mock_script};
use renod::{Daemon, DaemonConfig, ExecutorSet, ExecutorSource, GitOutcome, Paths, Stopped};
use tempfile::TempDir;

/// A failsafe, not a measurement: no test waits on it for a result.
pub(crate) const FAILSAFE: Duration = Duration::from_secs(60);

/// Runs `fut`, failing the test if it hangs.
pub(crate) async fn within<T>(fut: impl Future<Output = T>) -> T {
    tokio::time::timeout(FAILSAFE, fut)
        .await
        .expect("timed out: the daemon stopped making progress")
}

pub(crate) fn id(name: &str) -> ExecutorId {
    name.parse().expect("executor id")
}

/// The approval the `mock-approval` executor asks for.
pub(crate) fn approval_subject() -> ApprovalSubject {
    ApprovalSubject {
        action: ActionClass::Command,
        tool: "bash".into(),
        command: Some("git push origin main".into()),
        paths: Vec::new(),
        raw: serde_json::Value::Null,
    }
}

/// The executors of the in-process tests.
pub(crate) struct Executors {
    /// `mock`: the canned script the real daemon registers.
    pub(crate) mock: MockExecutor,
    /// `mock-hold`: stops after its first text until steered or cancelled.
    pub(crate) hold: MockExecutor,
    /// `mock-approval`: asks to run `git push origin main` (which the default policy leaves to a human) and goes on once it is answered.
    pub(crate) approval: MockExecutor,
    /// `mock-burst`: 400 text events, to overrun a small subscriber queue.
    pub(crate) burst: MockExecutor,
    /// `mock-danger`: asks to run `rm -rf build`, which the default policy denies outright.
    pub(crate) danger: MockExecutor,
}

impl Executors {
    pub(crate) fn new() -> Self {
        let named = |name: &str| MockExecutor::new(id(name));
        let mut burst: Vec<MockStep> = vec![MockStep::started("burst", "mock-1")];
        burst.extend((0..400).map(|n| MockStep::text(format!("{n} "))));
        burst.push(MockStep::done(StopReason::Completed));
        Self {
            mock: mock_executor(false).expect("mock"),
            hold: named("mock-hold").with_steps(mock_script(true)),
            approval: named("mock-approval").with_steps([
                MockStep::started("approval", "mock-1"),
                MockStep::text("about to ask. "),
                MockStep::Emit(ExecEvent::NeedsApproval {
                    request_id: "req-1".into(),
                    subject: approval_subject(),
                }),
                MockStep::AwaitAnswer {
                    request_id: "req-1".into(),
                },
                MockStep::text("answered."),
                MockStep::done(StopReason::Completed),
            ]),
            burst: named("mock-burst").with_steps(burst),
            danger: named("mock-danger").with_steps([
                MockStep::started("danger", "mock-1"),
                MockStep::Emit(ExecEvent::NeedsApproval {
                    request_id: "req-1".into(),
                    subject: ApprovalSubject {
                        action: ActionClass::Command,
                        tool: "bash".into(),
                        command: Some("rm -rf build".into()),
                        paths: Vec::new(),
                        raw: serde_json::Value::Null,
                    },
                }),
                MockStep::AwaitAnswer {
                    request_id: "req-1".into(),
                },
                MockStep::text("answered."),
                MockStep::done(StopReason::Completed),
            ]),
        }
    }

    pub(crate) fn set(&self) -> ExecutorSet {
        let registry = [
            &self.mock,
            &self.hold,
            &self.approval,
            &self.burst,
            &self.danger,
        ]
        .into_iter()
        .fold(ExecutorRegistry::new(), |registry, mock| {
            registry.with(Arc::new(mock.clone()))
        });
        ExecutorSet::from_registry(registry)
    }
}

/// A daemon on a temp data dir, and a client connected to it.
pub(crate) struct Fixture {
    pub(crate) dir: TempDir,
    pub(crate) paths: Paths,
    pub(crate) executors: Executors,
    pub(crate) daemon: Daemon,
    pub(crate) client: Client,
}

/// Most watchers one test process holds at once. The machine has only a few file-watch streams
/// (about sixteen, for every process on it), and a test binary runs its tests in parallel, each
/// with a daemon. Past the limit a start is refused like the platform refuses it, and the daemon
/// retries (that is the behaviour under test in `watch_streams.rs`), so a daemon that must watch
/// waits for a turn instead of failing.
const WATCHERS_PER_PROCESS: usize = 4;

static HELD: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The real platform watcher, at most [`WATCHERS_PER_PROCESS`] at a time.
#[derive(Debug)]
struct Budgeted;

struct Permit;

impl Drop for Permit {
    fn drop(&mut self) {
        HELD.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// A started watcher with its permit; the fields drop in order, so the stream stops before the
/// permit goes back.
struct Held {
    _guard: renod::watcher::WatcherGuard,
    _permit: Permit,
}

impl renod::WatcherFactory for Budgeted {
    fn start(
        &self,
        roots: &[std::path::PathBuf],
        handler: renod::watcher::Handler,
    ) -> Result<renod::watcher::WatcherGuard, String> {
        use std::sync::atomic::Ordering::SeqCst;
        let taken = HELD.fetch_update(SeqCst, SeqCst, |n| {
            (n < WATCHERS_PER_PROCESS).then_some(n + 1)
        });
        if taken.is_err() {
            return Err(
                "unable to start FSEvent stream (this test process holds its share)".into(),
            );
        }
        let permit = Permit;
        let guard = renod::watcher::NotifyWatchers.start(roots, handler)?;
        Ok(renod::watcher::WatcherGuard::new(Held {
            _guard: guard,
            _permit: permit,
        }))
    }
}

pub(crate) fn config(dir: &TempDir, executors: &Executors) -> DaemonConfig {
    let mut config = DaemonConfig::new(
        Paths::new(dir.path().join("data"), None),
        ExecutorSource::Given(executors.set()),
    );
    // The definitions are in the test's own directory (`Paths::new`), edits are noticed quickly,
    // and no test runs `git`.
    config.defs_debounce = Duration::from_millis(50);
    config.git_init = |_| Box::pin(async { GitOutcome::Initialized });
    config.watchers = Arc::new(Budgeted);
    // Short, so a test that does stall on shutdown fails instead of waiting a quarter minute.
    config.kernel = KernelConfig {
        shutdown_deadline: Duration::from_secs(10),
        // A watch that fails to start is tried again quickly.
        watch_retry: reno_kernel::WatchBackoff {
            initial: Duration::from_millis(20),
            max: Duration::from_millis(200),
        },
        ..KernelConfig::default()
    };
    config
}

impl Fixture {
    pub(crate) async fn start() -> Self {
        Self::with(|_| {}).await
    }

    /// Starts a daemon after `tweak` has adjusted its config.
    pub(crate) async fn with(tweak: impl FnOnce(&mut DaemonConfig)) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let executors = Executors::new();
        let mut config = config(&dir, &executors);
        tweak(&mut config);
        let paths = config.paths.clone();
        let daemon = within(Daemon::start(config)).await.expect("start");
        let client = within(Client::connect_unix(&paths.socket))
            .await
            .expect("connect");
        Self {
            dir,
            paths,
            executors,
            daemon,
            client,
        }
    }

    /// Asks the daemon to stop and waits for the sequence to finish.
    pub(crate) async fn stop(self) -> (TempDir, Paths, Stopped) {
        let Self {
            dir,
            paths,
            daemon,
            client,
            ..
        } = self;
        daemon.request_shutdown();
        let stopped = within(daemon.wait()).await.expect("stop");
        drop(client);
        (dir, paths, stopped)
    }

    pub(crate) async fn run(&self, executor: &str, prompt: &str) -> RunId {
        within(self.client.run_task(TaskRunParams {
            prompt: prompt.into(),
            executor: Some(id(executor)),
            bot: None,
            cwd: None,
            model: None,
            in_place: false,
            attachments: Vec::new(),
        }))
        .await
        .expect("task.run")
    }

    /// Every event of `run`, from `from`, until the stream ends.
    pub(crate) async fn events(&self, run: &RunId, from: Option<Seq>) -> Vec<RunEventItem> {
        within(
            self.client
                .run_events(run.clone(), from)
                .map(|item| item.expect("event"))
                .collect::<Vec<_>>(),
        )
        .await
    }

    pub(crate) async fn status(&self, run: &RunId) -> RunStatus {
        within(self.client.get_run(run))
            .await
            .expect("run.get")
            .status
    }
}

pub(crate) fn seqs(events: &[RunEventItem]) -> Vec<u64> {
    events.iter().map(|e| e.seq.0).collect()
}

pub(crate) fn is_done(event: &ExecEvent, stop: StopReason) -> bool {
    matches!(event, ExecEvent::Done { stop: s, .. } if *s == stop)
}
