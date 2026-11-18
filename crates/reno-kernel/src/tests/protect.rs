#![cfg(test)]

//! Protected paths enforced after the fact (see `supervisor::protect`): a run whose harness
//! cannot be asked writes `.claude/settings.json`, and the supervisor reverts it, audits it and
//! stops the run. The worktree is the `FakeWorkspace`: a test says what the run left in it.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use reno_harness::exec::{MockExecutor, MockStep};
use reno_proto::defs::{DefKind, PROTECTED_RULE};
use reno_proto::{
    ActionClass, ApprovalOutcome, ExecEvent, NotResumed, ResolvedBy, RunId, RunStatus,
    SandboxLevel, Seq, StopDetail, StopReason, ToolCall,
};

use super::approvals::resolved;
use super::defs::{FakePort, file, setup};
use super::*;
use crate::FakeWorkspace;

fn write(id: &str, path: &str) -> [MockStep; 2] {
    [
        MockStep::Emit(ExecEvent::Tool {
            id: id.into(),
            call: ToolCall::new(
                "Write",
                serde_json::json!({ "file_path": path }),
                ActionClass::Edit,
            ),
        }),
        MockStep::Emit(ExecEvent::ToolResult {
            id: id.into(),
            ok: true,
            summary: None,
        }),
    ]
}

fn steps(parts: impl IntoIterator<Item = [MockStep; 2]>) -> Vec<MockStep> {
    let mut steps = vec![
        MockStep::started("s", "mock-1"),
        MockStep::Sleep(Duration::from_secs(5)),
    ];
    steps.extend(parts.into_iter().flatten());
    steps.push(MockStep::Sleep(Duration::from_secs(600)));
    steps.push(MockStep::text("never reached"));
    steps
}

async fn run_in(fx: &Fx, cwd: &str) -> RunId {
    fx.kernel
        .submit("work", id("mock"), PathBuf::from(cwd), None)
        .await
        .unwrap()
}

async fn placed(ws: &FakeWorkspace, run: &RunId) {
    until(async || ws.exists(run).then_some(())).await;
}

async fn ended(fx: &Fx, run: &RunId, status: RunStatus) -> Vec<(Seq, ExecEvent)> {
    fx.wait_status(run, status).await;
    fx.journal_with_done(run).await
}

fn done(journal: &[(Seq, ExecEvent)]) -> (StopReason, Option<StopDetail>, Option<String>) {
    match journal.last() {
        Some((
            _,
            ExecEvent::Done {
                stop,
                reason,
                diagnostic,
                ..
            },
        )) => (*stop, reason.clone(), diagnostic.clone()),
        other => panic!("not a Done: {other:?}"),
    }
}

async fn rig(mock: &MockExecutor, ws: &Arc<FakeWorkspace>, sandbox: SandboxLevel) -> Fx {
    let cfg = KernelConfig {
        sandbox,
        ..config(1)
    };
    Fx::with_workspace(cfg, registry(std::slice::from_ref(mock)), None, ws.clone()).await
}

#[tokio::test(start_paused = true)]
async fn a_write_to_a_protected_path_is_reverted_audited_and_stops_the_run() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let m = mock(steps([
        write("t1", "src/a.rs"),
        write("t2", ".claude/settings.json"),
    ]));
    let fx = rig(&m, &ws, SandboxLevel::Unrestricted).await;
    let run = run_in(&fx, "/repo").await;
    placed(&ws, &run).await;
    // The run wrote an ordinary file and a protected one.
    ws.plant_protected(&run, &[".claude/settings.json", "src/a.rs"]);

    let journal = ended(&fx, &run, RunStatus::Interrupted).await;
    let (stop, reason, diagnostic) = done(&journal);
    assert_eq!(stop, StopReason::Interrupted);
    let detail = StopDetail::ProtectedPathWrite {
        paths: vec![".claude/settings.json".into()],
        reverted: true,
    };
    assert_eq!(reason, Some(detail.clone()));
    assert_eq!(diagnostic, Some(detail.to_string()));
    assert!(
        detail
            .to_string()
            .starts_with("blocked: .claude/settings.json is protected")
    );

    // Only the protected path was put back; the ordinary change stays for the owner.
    assert_eq!(
        ws.reverts(),
        [(run.clone(), vec![".claude/settings.json".to_owned()])]
    );
    assert_eq!(ws.protected_left(&run), ["src/a.rs"]);

    // The journal and the approval history both say it was refused.
    let blocked = resolved(&journal);
    assert_eq!(blocked.len(), 1);
    assert_eq!(blocked[0].1, ApprovalOutcome::Denied);
    assert_eq!(
        blocked[0].2,
        ResolvedBy::Rule {
            name: PROTECTED_RULE.into()
        }
    );
    assert!(
        blocked[0]
            .3
            .as_deref()
            .unwrap()
            .starts_with("blocked: `.claude/settings.json` is protected")
    );
    let history = fx.kernel.approval_history(Some(&run), None).await.unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0].subject.paths,
        [PathBuf::from(".claude/settings.json")]
    );
    // A run stopped for this is never resumed on its own.
    assert_eq!(
        detail.forbids_auto_resume(),
        Some(NotResumed::ProtectedPathWrite)
    );
}

#[tokio::test(start_paused = true)]
async fn a_write_made_after_the_last_tool_is_caught_when_the_run_ends() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    // A harness that finishes by itself: a shell write no tool result followed.
    let m = mock([
        MockStep::started("s", "mock-1"),
        MockStep::Sleep(Duration::from_secs(5)),
        MockStep::done(StopReason::Completed),
    ]);
    let fx = rig(&m, &ws, SandboxLevel::Unrestricted).await;
    let run = run_in(&fx, "/repo").await;
    placed(&ws, &run).await;
    ws.plant_protected(&run, &[".mcp.json"]);

    let journal = ended(&fx, &run, RunStatus::Interrupted).await;
    let (stop, reason, _) = done(&journal);
    assert_eq!(stop, StopReason::Interrupted);
    assert!(matches!(
        reason,
        Some(StopDetail::ProtectedPathWrite { reverted: true, .. })
    ));
    assert!(ws.protected_left(&run).is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_run_that_touches_no_protected_path_ends_normally() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let [call, result] = write("t1", "src/a.rs");
    let m = mock([
        MockStep::started("s", "mock-1"),
        MockStep::Sleep(Duration::from_secs(5)),
        call,
        result,
        MockStep::done(StopReason::Completed),
    ]);
    let fx = rig(&m, &ws, SandboxLevel::Unrestricted).await;
    let run = run_in(&fx, "/repo").await;
    placed(&ws, &run).await;
    ws.plant_protected(&run, &["src/a.rs"]);

    let journal = ended(&fx, &run, RunStatus::Succeeded).await;
    let (stop, reason, _) = done(&journal);
    assert_eq!((stop, reason), (StopReason::Completed, None));
    assert!(ws.reverts().is_empty());
    assert_eq!(ws.protected_left(&run), ["src/a.rs"]);
    assert!(resolved(&journal).is_empty());
}

/// A kernel whose bot `loose` may write in place, over a directory that is no repository.
async fn opted_in_rig(mock: &MockExecutor, ws: &Arc<FakeWorkspace>) -> Fx {
    let port = FakePort::with(vec![file(
        DefKind::Bot,
        "loose",
        "name = \"loose\"\ninstructions = \"x\"\nexecutors = [\"mock\"]\nin_place_writes = true\n",
    )]);
    Fx::with_workspace(
        config(1),
        registry(std::slice::from_ref(mock)),
        Some(setup(&port)),
        ws.clone(),
    )
    .await
}

#[tokio::test(start_paused = true)]
async fn a_write_outside_the_worktree_that_the_worktree_cannot_show_still_stops_the_run() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let m = mock(steps([
        write("t1", "src/a.rs"),
        write("t2", "/home/owner/.claude/settings.json"),
    ]));
    let fx = rig(&m, &ws, SandboxLevel::Unrestricted).await;
    let run = run_in(&fx, "/repo").await;
    let journal = ended(&fx, &run, RunStatus::Interrupted).await;
    let (_, reason, _) = done(&journal);
    assert_eq!(
        reason,
        Some(StopDetail::ProtectedPathWrite {
            paths: vec!["/home/owner/.claude/settings.json".into()],
            reverted: false
        })
    );
    assert!(ws.reverts().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_in_place_run_is_stopped_and_nothing_is_deleted() {
    let ws = Arc::new(FakeWorkspace::new());
    let m = mock(steps([
        write("t1", "src/a.rs"),
        write("t2", ".claude/settings.json"),
    ]));
    let fx = opted_in_rig(&m, &ws).await;
    let run = fx
        .kernel
        .submit_for_bot("loose", "go", None, PathBuf::from("/plain"), None)
        .await
        .unwrap();
    ws.plant_protected(&run, &[".claude/settings.json"]);

    let journal = ended(&fx, &run, RunStatus::Interrupted).await;
    let (_, reason, _) = done(&journal);
    assert_eq!(
        reason,
        Some(StopDetail::ProtectedPathWrite {
            paths: vec![".claude/settings.json".into()],
            reverted: false
        })
    );
    // No worktree, so there was nothing of Reno's to revert, and the user's files were not
    // touched.
    assert!(ws.reverts().is_empty());
    assert_eq!(ws.protected_left(&run), [".claude/settings.json"]);
    assert_eq!(resolved(&journal).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn an_in_place_write_that_failed_does_not_stop_the_run() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let [call, _] = write("t1", ".claude/settings.json");
    let m = mock([
        MockStep::started("s", "mock-1"),
        call,
        // A supervised harness whose request the approval step denied: the tool failed.
        MockStep::Emit(ExecEvent::ToolResult {
            id: "t1".into(),
            ok: false,
            summary: None,
        }),
        MockStep::done(StopReason::Completed),
    ]);
    let fx = rig(&m, &ws, SandboxLevel::ReadOnly).await;
    let run = run_in(&fx, "/repo").await;
    let journal = ended(&fx, &run, RunStatus::Succeeded).await;
    assert_eq!(done(&journal).1, None);
}
