#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The kernel, end to end, over the native `ClaudeExecutor` and a fake `claude` binary (the
//! harness crate's `tests/fixtures/fake-claude.sh`, which speaks stream-json and the control
//! protocol). No network and no real harness (ARCH-16). Real time, because a real child process
//! runs; nothing here sleeps to wait, and the timeout is only a failsafe against a hang.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use reno_harness::ClaudeExecutor;
use reno_kernel::{ExecutorRegistry, Kernel, KernelConfig, SystemClock};
use reno_proto::{
    ActionClass, ApprovalDecision, ExecEvent, ExecutorId, RunId, RunStatus, Seq, StopReason,
};
use reno_store::Store;

const FAILSAFE: Duration = Duration::from_secs(60);

async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    tokio::time::timeout(FAILSAFE, fut)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

fn fake_claude() -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("reno-harness")
        .join("tests")
        .join("fixtures")
        .join("fake-claude.sh");
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    path
}

struct Fx {
    _dir: tempfile::TempDir,
    store: Store,
    kernel: Kernel,
}

impl Fx {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("reno.db")).unwrap();
        let registry = ExecutorRegistry::new().with(Arc::new(
            ClaudeExecutor::new().with_executable(fake_claude()),
        ));
        // The default access mode is Supervised: the strictest.
        let kernel = Kernel::start(
            KernelConfig::default(),
            store.clone(),
            registry,
            Arc::new(SystemClock),
        )
        .await
        .unwrap();
        Self {
            _dir: dir,
            store,
            kernel,
        }
    }

    async fn submit(&self, prompt: &str) -> RunId {
        let claude: ExecutorId = "claude-code".parse().unwrap();
        self.kernel
            .submit(prompt, claude, std::env::temp_dir(), None)
            .await
            .unwrap()
    }

    /// Wait until `run` has `status`, following the kernel's own watch (no polling).
    async fn status(&self, run: &RunId, want: RunStatus) {
        let mut watch = self.kernel.watch_runs();
        within(&format!("{run} to be {want:?}"), async {
            loop {
                if watch
                    .borrow_and_update()
                    .iter()
                    .any(|r| r.run_id == *run && r.status == want)
                {
                    return;
                }
                watch.changed().await.unwrap();
            }
        })
        .await;
    }

    async fn pending_approval(&self) -> reno_proto::Approval {
        let mut watch = self.kernel.watch_approvals();
        within("an approval", async {
            loop {
                if let Some(first) = watch.borrow_and_update().first() {
                    return first.clone();
                }
                watch.changed().await.unwrap();
            }
        })
        .await
    }

    fn journal(&self, run: &RunId) -> Vec<ExecEvent> {
        self.store
            .journals()
            .replay(run, Seq(0))
            .unwrap()
            .map(|entry| entry.unwrap().1)
            .collect()
    }
}

/// A `Supervised` run hits `NeedsApproval` with a structured subject; the kernel decides
/// approve, the CLI is told to allow, and the run succeeds.
#[tokio::test]
async fn a_supervised_run_asks_the_kernel_decides_approve_and_the_run_succeeds() {
    let fx = Fx::new().await;
    let run = fx.submit("scenario:one-approval").await;

    let approval = fx.pending_approval().await;
    assert_eq!(approval.run_id, run);
    assert_eq!(approval.subject.action, ActionClass::Command);
    assert_eq!(approval.subject.tool, "Bash");
    assert_eq!(approval.subject.command.as_deref(), Some("rm -rf build"));
    fx.status(&run, RunStatus::WaitingApproval).await;

    fx.kernel
        .decide(&approval.id, ApprovalDecision::ApproveOnce)
        .await
        .unwrap();
    fx.status(&run, RunStatus::Succeeded).await;

    let journal = fx.journal(&run);
    assert!(
        journal
            .iter()
            .any(|e| matches!(e, ExecEvent::NeedsApproval { subject, .. } if subject.action == ActionClass::Command)),
        "{journal:?}"
    );
    // The fake CLI says what it was answered: `ran` means it saw an allow.
    assert!(
        matches!(
            journal.last(),
            Some(ExecEvent::Done { stop: StopReason::Completed, result: Some(r), .. }) if r == "ran"
        ),
        "{journal:?}"
    );
    assert!(fx.kernel.pending_approvals().is_empty());
    fx.kernel.shutdown().await;
}

#[tokio::test]
async fn a_denied_approval_reaches_the_cli_as_a_deny() {
    let fx = Fx::new().await;
    let run = fx.submit("scenario:one-approval").await;
    let approval = fx.pending_approval().await;
    fx.kernel
        .decide(&approval.id, ApprovalDecision::Deny)
        .await
        .unwrap();
    fx.status(&run, RunStatus::Succeeded).await;
    assert!(
        matches!(
            fx.journal(&run).last(),
            Some(ExecEvent::Done { result: Some(r), .. }) if r == "refused"
        ),
        "{:?}",
        fx.journal(&run)
    );
    fx.kernel.shutdown().await;
}

/// The child's process record is forwarded by the executor and persisted by the kernel, so a
/// crash of the daemon leaves a row for the orphan sweep (the T0.8 follow-up).
#[tokio::test]
async fn the_child_process_record_is_persisted_on_the_run() {
    let fx = Fx::new().await;
    let run = fx.submit("scenario:one-approval").await;
    let approval = fx.pending_approval().await;

    // Persisted while the run waits, not only at the end.
    let row = within("the process row", async {
        loop {
            let row = fx.store.get_run(&run).await.unwrap().unwrap();
            if row.pid.is_some() {
                return row;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    let pid = row.pid.expect("pid");
    assert_eq!(row.pgid, Some(pid), "the CLI is its own group leader");
    assert!(row.process_start.is_some_and(|start| start > 0));

    fx.kernel
        .decide(&approval.id, ApprovalDecision::ApproveOnce)
        .await
        .unwrap();
    fx.status(&run, RunStatus::Succeeded).await;
    // The record survives the run as an event, too.
    let events = fx.store.events_since(Seq(0), 1000).await.unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "run.process_recorded" && e.data["pid"] == u64::from(pid)),
        "{events:?}"
    );
    fx.kernel.shutdown().await;
}
