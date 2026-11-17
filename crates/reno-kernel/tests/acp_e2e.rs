#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The kernel, end to end, over the native `AcpExecutor` and a fake Antigravity server (the
//! harness crate's `tests/fixtures/fake-antigravity-acp.sh`, which speaks ACP and asks for
//! permission to read, edit and run a command). Antigravity is the built-in agent that honors
//! `Supervised`, which is what the kernel asks for by default. No network and no real harness
//! (ARCH-16). Real time, because a real child process runs; nothing here sleeps to wait, and the
//! timeout is only a failsafe against a hang. The kernel's clock is a manual one: stored times
//! are fixed.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use reno_harness::AcpExecutor;
use reno_kernel::{ExecutorRegistry, Kernel, KernelConfig, ManualClock};
use reno_proto::{
    ActionClass, ApprovalDecision, ExecEvent, ExecutorId, RunId, RunStatus, SandboxLevel, Seq,
    StopReason,
};
use reno_store::Store;

const FAILSAFE: Duration = Duration::from_secs(60);

async fn within<T>(what: &str, fut: impl Future<Output = T>) -> T {
    tokio::time::timeout(FAILSAFE, fut)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

fn fixture(name: &str) -> PathBuf {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("reno-harness")
        .join("tests")
        .join("fixtures")
        .join(name);
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
    /// A kernel whose runs ask for no sandbox: Claude and the ACP agents have none of their
    /// own, so `Unrestricted` is the only level they honour.
    async fn new() -> Self {
        Self::with_sandbox(SandboxLevel::Unrestricted).await
    }

    async fn with_sandbox(sandbox: SandboxLevel) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("reno.db")).unwrap();
        let registry = ExecutorRegistry::new()
            .with(Arc::new(
                AcpExecutor::antigravity().with_executable(fixture("fake-antigravity-acp.sh")),
            ))
            // Grok honors full access only, over the same fixture: a path nothing may be spawned
            // on.
            .with(Arc::new(
                AcpExecutor::grok().with_executable(fixture("fake-acp.sh")),
            ));
        // The default access mode is Supervised: the strictest.
        let kernel = Kernel::start(
            KernelConfig {
                sandbox,
                ..KernelConfig::default()
            },
            store.clone(),
            registry,
            // A fixed clock: the test reads no wall time (ARCH-16).
            Arc::new(ManualClock::new(1_700_000_000_000)),
        )
        .await
        .unwrap();
        Self {
            _dir: dir,
            store,
            kernel,
        }
    }

    async fn submit_to(&self, executor: &str, prompt: &str) -> RunId {
        let executor: ExecutorId = executor.parse().unwrap();
        self.kernel
            .submit(prompt, executor, std::env::temp_dir(), None)
            .await
            .unwrap()
    }

    /// A run of the fake server's `permissions` prompt: it reads, then asks to edit, then asks
    /// to run `cargo test`, and replies with the option the kernel's decision selected for each.
    async fn submit(&self) -> RunId {
        self.submit_to("acp.antigravity", "permissions").await
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

    /// The run's journal through its `Done`. Read from the kernel's event stream, which ends
    /// after `Done`: a terminal status commits before `Done` is journaled, so reading the file
    /// as soon as the status shows would race the append.
    async fn journal(&self, run: &RunId) -> Vec<ExecEvent> {
        use futures::StreamExt as _;
        let stream = self.kernel.run_events(run, None).await.unwrap();
        within(&format!("{run} to end"), async {
            stream.map(|item| item.unwrap().event).collect().await
        })
        .await
    }
}

/// What the fake server reports it was told for each of its requests.
fn reply(journal: &[ExecEvent]) -> String {
    journal
        .iter()
        .filter_map(|e| match e {
            ExecEvent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// A `Supervised` run hits `NeedsApproval` with a structured subject, once for the edit and once
/// for the command (reading is not asked); the kernel decides approve each time, the agent is
/// told `allow_once`, and the run succeeds.
#[tokio::test]
async fn a_supervised_run_asks_the_kernel_decides_approve_and_the_run_succeeds() {
    let fx = Fx::new().await;
    let run = fx.submit().await;

    let edit = fx.pending_approval().await;
    assert_eq!(edit.run_id, run);
    assert_eq!(edit.subject.action, ActionClass::Edit);
    assert_eq!(edit.subject.tool, "edit");
    assert_eq!(edit.subject.paths, [PathBuf::from("/w/src/lib.rs")]);
    assert_eq!(edit.subject.command, None);
    // The whole request is kept for the audit record.
    assert_eq!(edit.subject.raw["toolCall"]["kind"], "edit");
    fx.status(&run, RunStatus::WaitingApproval).await;
    fx.kernel
        .decide(&edit.id, ApprovalDecision::ApproveOnce)
        .await
        .unwrap();

    let command = loop {
        let approval = fx.pending_approval().await;
        if approval.id != edit.id {
            break approval;
        }
        tokio::task::yield_now().await;
    };
    assert_eq!(command.subject.action, ActionClass::Command);
    assert_eq!(command.subject.tool, "execute");
    assert_eq!(command.subject.command.as_deref(), Some("cargo test"));
    fx.kernel
        .decide(&command.id, ApprovalDecision::ApproveOnce)
        .await
        .unwrap();
    fx.status(&run, RunStatus::Succeeded).await;

    let journal = fx.journal(&run).await;
    let asked: Vec<_> = journal
        .iter()
        .filter_map(|e| match e {
            ExecEvent::NeedsApproval { subject, .. } => Some(subject.action),
            _ => None,
        })
        .collect();
    assert_eq!(asked, [ActionClass::Edit, ActionClass::Command]);
    // The fake server says which option it was answered with: `once` is allow_once, never the
    // `allow_always` it offered beside it.
    assert!(
        reply(&journal).ends_with("perms:read=once;edit=once;execute=once;"),
        "{journal:?}"
    );
    assert!(
        matches!(
            journal.last(),
            Some(ExecEvent::Done {
                stop: StopReason::Completed,
                ..
            })
        ),
        "{journal:?}"
    );
    assert!(fx.kernel.pending_approvals().await.unwrap().is_empty());
    fx.kernel.shutdown().await;
}

#[tokio::test]
async fn a_denied_approval_reaches_the_agent_as_reject_once() {
    let fx = Fx::new().await;
    let run = fx.submit().await;
    let first = fx.pending_approval().await;
    fx.kernel
        .decide(&first.id, ApprovalDecision::Deny)
        .await
        .unwrap();
    let second = loop {
        let approval = fx.pending_approval().await;
        if approval.id != first.id {
            break approval;
        }
        tokio::task::yield_now().await;
    };
    fx.kernel
        .decide(&second.id, ApprovalDecision::Deny)
        .await
        .unwrap();
    // A refused tool is the agent's to handle: the run still ends.
    fx.status(&run, RunStatus::Succeeded).await;
    assert!(
        reply(&fx.journal(&run).await).ends_with("perms:read=once;edit=no;execute=no;"),
        "{:?}",
        fx.journal(&run).await
    );
    fx.kernel.shutdown().await;
}

/// The child's process record is forwarded by the executor and persisted by the kernel, so a
/// crash of the daemon leaves a row for the orphan sweep.
#[tokio::test]
async fn the_agents_process_record_is_persisted_on_the_run() {
    let fx = Fx::new().await;
    let run = fx.submit().await;
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
    assert_eq!(row.pgid, Some(pid), "the agent is its own group leader");
    assert!(row.process_start.is_some_and(|start| start > 0));

    fx.kernel
        .decide(&approval.id, ApprovalDecision::Deny)
        .await
        .unwrap();
    let next = loop {
        let next = fx.pending_approval().await;
        if next.id != approval.id {
            break next;
        }
        tokio::task::yield_now().await;
    };
    fx.kernel
        .decide(&next.id, ApprovalDecision::Deny)
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

/// ARCH-15: the kernel asks for `Supervised`, and Grok honors only full access. Eligibility refuses it
/// before start with the reason journaled; nothing was spawned, so no process row exists.
#[tokio::test]
async fn an_agent_that_cannot_honor_the_access_mode_is_refused_before_it_spawns() {
    let fx = Fx::new().await;
    let run = fx.submit_to("acp.grok", "scenario:happy").await;
    fx.status(&run, RunStatus::Failed).await;
    let journal = fx.journal(&run).await;
    match journal.last() {
        Some(ExecEvent::Done {
            stop: StopReason::Error,
            diagnostic: Some(why),
            ..
        }) => assert!(
            !why.contains("NoEligibleExecutor") && why.contains("supports only Full access"),
            "{why}"
        ),
        other => panic!("{other:?} in {journal:?}"),
    }
    assert!(
        !journal
            .iter()
            .any(|e| matches!(e, ExecEvent::Started { .. } | ExecEvent::Text { .. })),
        "{journal:?}"
    );
    let row = fx.store.get_run(&run).await.unwrap().unwrap();
    assert_eq!(row.pid, None, "nothing was spawned");
    fx.kernel.shutdown().await;
}
