#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! The kernel, end to end, over the native `ClaudeExecutor` and a fake `claude` binary (the
//! harness crate's `tests/fixtures/fake-claude.sh`, which speaks stream-json and the control
//! protocol). No network and no real harness (ARCH-16). Real time, because a real child process
//! runs; nothing here sleeps to wait, and the timeout is only a failsafe against a hang. The
//! kernel's clock is a manual one: stored times are fixed.

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use reno_harness::ClaudeExecutor;
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
    /// A kernel whose runs ask for no sandbox: Claude and the ACP agents have none of their
    /// own, so `Unrestricted` is the only level they honour.
    async fn new() -> Self {
        Self::with_sandbox(SandboxLevel::Unrestricted).await
    }

    async fn with_sandbox(sandbox: SandboxLevel) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("reno.db")).unwrap();
        let registry = ExecutorRegistry::new().with(Arc::new(
            ClaudeExecutor::new().with_executable(fake_claude()),
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

    let journal = fx.journal(&run).await;
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
    assert!(fx.kernel.pending_approvals().await.unwrap().is_empty());
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
            fx.journal(&run).await.last(),
            Some(ExecEvent::Done { result: Some(r), .. }) if r == "refused"
        ),
        "{:?}",
        fx.journal(&run).await
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

/// ARCH-15: Claude declares no sandbox of its own, so a run that asks for `WorkspaceWrite` (the
/// kernel's default) is refused before the CLI is spawned, with the reason journaled.
#[tokio::test]
async fn a_run_asking_for_a_sandbox_claude_does_not_declare_is_refused_before_it_spawns() {
    let fx = Fx::with_sandbox(SandboxLevel::WorkspaceWrite).await;
    let run = fx.submit("scenario:one-approval").await;
    fx.status(&run, RunStatus::Failed).await;
    let journal = fx.journal(&run).await;
    match journal.last() {
        Some(ExecEvent::Done {
            stop: StopReason::Error,
            diagnostic: Some(why),
            ..
        }) => assert!(
            !why.contains("NoEligibleExecutor") && why.contains("Workspace write"),
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

/// A definitions port that serves fixed files: a full-access Claude bot and a policy.
#[derive(Debug)]
struct FixedDefs(Vec<reno_proto::defs::DefFile>);

#[async_trait::async_trait]
impl reno_kernel::Definitions for FixedDefs {
    async fn snapshot(&self) -> reno_kernel::Result<reno_kernel::DefSnapshot> {
        Ok(reno_kernel::DefSnapshot {
            files: self.0.clone(),
            problems: Vec::new(),
        })
    }

    fn watch(&self) -> reno_kernel::Result<tokio::sync::mpsc::Receiver<()>> {
        let (_tx, rx) = tokio::sync::mpsc::channel(1);
        Ok(rx)
    }
}

impl Fx {
    /// A kernel with a `fullbot` bot (full access, Claude) and `policy` as its policy.
    async fn full_access(policy: &str) -> Self {
        use reno_proto::defs::{DefFile, DefKind, ValidationContext};
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("reno.db")).unwrap();
        let registry = ExecutorRegistry::new().with(Arc::new(
            ClaudeExecutor::new().with_executable(fake_claude()),
        ));
        let files = vec![
            DefFile {
                kind: DefKind::Bot,
                name: "fullbot".into(),
                text: "name = \"fullbot\"\ninstructions = \"work\"\nexecutors = [\"claude-code\"]\naccess = \"full_access\"\n".into(),
            },
            DefFile {
                kind: DefKind::Policy,
                name: "policy".into(),
                text: policy.into(),
            },
        ];
        let setup = reno_kernel::DefsSetup::new(
            Arc::new(FixedDefs(files)),
            ValidationContext::with_builtin([]),
        );
        let kernel = Kernel::start_with_definitions(
            KernelConfig::default(),
            store.clone(),
            registry,
            Arc::new(ManualClock::new(1_700_000_000_000)),
            setup,
        )
        .await
        .unwrap();
        Self {
            _dir: dir,
            store,
            kernel,
        }
    }

    async fn submit_full(&self, prompt: &str) -> RunId {
        self.kernel
            .submit_for_bot("fullbot", prompt, None, std::env::temp_dir(), None)
            .await
            .unwrap()
    }
}

fn resolutions(
    journal: &[ExecEvent],
) -> Vec<(reno_proto::ApprovalOutcome, reno_proto::ResolvedBy)> {
    journal
        .iter()
        .filter_map(|e| match e {
            ExecEvent::ApprovalResolved { decision, by, .. } => Some((*decision, by.clone())),
            _ => None,
        })
        .collect()
}

/// A full-access Claude run still reaches the kernel: a command the policy denies is refused
/// (the CLI sees a deny) and journaled, with nobody asked.
#[tokio::test]
async fn a_full_access_claude_run_has_a_policy_deny_refused_and_journaled() {
    let fx = Fx::full_access(
        "default = \"ask\"\n[[rules]]\nnote = \"no rm -rf\"\neffect = \"deny\"\nmatch = { action = \"command\", command = \"rm -rf*\" }\n",
    )
    .await;
    let run = fx.submit_full("scenario:one-approval").await;
    fx.status(&run, RunStatus::Succeeded).await;
    let journal = fx.journal(&run).await;
    assert!(
        matches!(
            journal.last(),
            Some(ExecEvent::Done { result: Some(r), .. }) if r == "refused"
        ),
        "{journal:?}"
    );
    assert_eq!(
        resolutions(&journal),
        [(
            reno_proto::ApprovalOutcome::Denied,
            reno_proto::ResolvedBy::Rule {
                name: "no rm -rf".into()
            }
        )]
    );
    assert!(fx.kernel.pending_approvals().await.unwrap().is_empty());
    fx.kernel.shutdown().await;
}

/// What no rule denies is allowed without a person, and the journal says the mode decided it.
#[tokio::test]
async fn a_full_access_claude_run_is_allowed_without_a_person_when_nothing_denies_it() {
    let fx = Fx::full_access("default = \"ask\"\n").await;
    let run = fx.submit_full("scenario:one-approval").await;
    fx.status(&run, RunStatus::Succeeded).await;
    let journal = fx.journal(&run).await;
    assert!(
        matches!(
            journal.last(),
            Some(ExecEvent::Done { result: Some(r), .. }) if r == "ran"
        ),
        "{journal:?}"
    );
    assert_eq!(
        resolutions(&journal),
        [(
            reno_proto::ApprovalOutcome::Approved,
            reno_proto::ResolvedBy::Rule {
                name: "builtin:full-access".into()
            }
        )]
    );
    assert!(fx.kernel.pending_approvals().await.unwrap().is_empty());
    fx.kernel.shutdown().await;
}
