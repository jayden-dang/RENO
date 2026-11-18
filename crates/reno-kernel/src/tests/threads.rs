#![cfg(test)]

//! Threads (RP0.5): a follow-up continues the thread's worktree, one run of a thread is live at a
//! time, the worktree is released when the thread closes (a person, or the idle limit as a
//! deadline), and a run is judged from where it started. Over the in-memory `FakeWorkspace`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use reno_harness::exec::MockStep;
use reno_proto::{
    ActionClass, CleanOutcome, CleanTarget, DiffScope, ExecEvent, RunId, RunStatus, SandboxLevel,
    StopDetail, StopReason, ThreadId, ThreadStatus, ToolCall, WorkspaceChanges, WorkspaceStatus,
};

use super::*;
use crate::{DEFAULT_THREAD_IDLE_TTL, FakeWorkspace, RunChanges, TaskRequest};

const BASE: &str = "0000000000000000000000000000000000000000";
const MOVED: &str = "1111111111111111111111111111111111111111";

fn changes(commits: u32, dirty: bool) -> WorkspaceChanges {
    WorkspaceChanges { commits, dirty }
}

async fn rig(
    ws: &Arc<FakeWorkspace>,
    steps: Vec<MockStep>,
) -> (Fx, reno_harness::exec::MockExecutor) {
    let m = mock(steps);
    let fx = Fx::with_workspace(
        config(2),
        registry(std::slice::from_ref(&m)),
        None,
        ws.clone(),
    )
    .await;
    (fx, m)
}

async fn start(fx: &Fx, cwd: &str) -> RunId {
    fx.kernel
        .submit("work", id("mock"), PathBuf::from(cwd), None)
        .await
        .unwrap()
}

/// A follow-up in `thread`, naming a folder that the thread overrides.
async fn follow_up(fx: &Fx, thread: &ThreadId) -> Result<RunId, KernelError> {
    let request = TaskRequest::new("more", PathBuf::from("/somewhere/else"))
        .executor(id("mock"))
        .thread(Some(thread.clone()));
    fx.kernel.submit_task(request).await.map(|(run, _)| run)
}

async fn ended(fx: &Fx, run: &RunId, status: RunStatus) {
    fx.wait_status(run, status).await;
    fx.journal_with_done(run).await;
    // The thread's idle clock starts on a task of its own, just after the end.
    let thread = fx.kernel.run_thread(run).await.unwrap();
    until(async || {
        (fx.kernel.thread(&thread).await.unwrap().status != ThreadStatus::Active).then_some(())
    })
    .await;
}

async fn thread_of(fx: &Fx, run: &RunId) -> ThreadId {
    fx.kernel.run_thread(run).await.unwrap()
}

#[tokio::test(start_paused = true)]
async fn a_follow_up_continues_in_the_threads_worktree_and_each_run_records_where_it_started() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let (fx, m) = rig(&ws, vec![]).await;

    let first = start(&fx, "/repo/crates/x").await;
    ended(&fx, &first, RunStatus::Succeeded).await;
    let thread = thread_of(&fx, &first).await;
    // The first run committed: the branch moved on.
    ws.set_head(&first, MOVED);

    let second = follow_up(&fx, &thread).await.unwrap();
    ended(&fx, &second, RunStatus::Succeeded).await;
    assert_eq!(thread_of(&fx, &second).await, thread);

    // One worktree was made, and the follow-up worked in it, in the thread's own folder.
    let specs = m.log().specs;
    assert_eq!(specs.len(), 2);
    assert_eq!(specs[0].cwd, specs[1].cwd);
    assert!(specs[1].cwd.ends_with("crates/x"), "{:?}", specs[1].cwd);
    let calls = ws.calls();
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("plan")).count(),
        1,
        "{calls:?}"
    );
    assert_eq!(
        calls.iter().filter(|c| c.starts_with("create")).count(),
        1,
        "{calls:?}"
    );
    assert!(calls.iter().all(|c| !c.starts_with("release")), "{calls:?}");

    let info = fx.kernel.thread(&thread).await.unwrap();
    assert_eq!(info.status, ThreadStatus::Idle);
    assert_eq!(
        info.branch.as_deref(),
        Some(format!("reno/{thread}").as_str())
    );
    assert_eq!(info.workspace, Some(WorkspaceStatus::Active));
    assert_eq!(info.runs.len(), 2);
    assert_eq!(info.runs[0].run_id, first);
    assert_eq!(info.runs[0].parent_run, None);
    assert_eq!(info.runs[0].start_sha.as_deref(), Some(BASE));
    assert_eq!(info.runs[1].run_id, second);
    assert_eq!(info.runs[1].parent_run, Some(first));
    assert_eq!(info.runs[1].start_sha.as_deref(), Some(MOVED));
}

#[tokio::test(start_paused = true)]
async fn a_thread_takes_one_live_run_and_cannot_be_closed_under_it() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let (fx, _m) = rig(&ws, vec![MockStep::Sleep(Duration::from_secs(5))]).await;

    let first = start(&fx, "/repo").await;
    let thread = thread_of(&fx, &first).await;
    // Queued counts as live, and so does running.
    assert!(matches!(
        follow_up(&fx, &thread).await,
        Err(KernelError::ThreadBusy { run, .. }) if run == first
    ));
    fx.wait_status(&first, RunStatus::Running).await;
    assert!(matches!(
        follow_up(&fx, &thread).await,
        Err(KernelError::ThreadBusy { .. })
    ));
    assert!(matches!(
        fx.kernel.close_thread(&thread, false).await,
        Err(KernelError::ThreadBusy { .. })
    ));
    assert_eq!(
        fx.kernel.thread(&thread).await.unwrap().status,
        ThreadStatus::Active
    );

    ended(&fx, &first, RunStatus::Succeeded).await;
    assert!(follow_up(&fx, &thread).await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn an_unknown_thread_is_not_found_for_a_follow_up_a_close_and_a_get() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let (fx, _m) = rig(&ws, vec![]).await;
    let nobody = ThreadId::from_suffix("nobody").unwrap();
    assert!(matches!(
        follow_up(&fx, &nobody).await,
        Err(KernelError::ThreadNotFound(_))
    ));
    assert!(matches!(
        fx.kernel.close_thread(&nobody, false).await,
        Err(KernelError::ThreadNotFound(_))
    ));
    assert!(matches!(
        fx.kernel.thread(&nobody).await,
        Err(KernelError::ThreadNotFound(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn a_closed_thread_reopens_while_its_worktree_is_kept_and_is_refused_once_it_is_removed() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let (fx, _m) = rig(&ws, vec![]).await;
    let first = start(&fx, "/repo").await;
    ended(&fx, &first, RunStatus::Succeeded).await;
    let thread = thread_of(&fx, &first).await;
    ws.set_changes(&first, changes(1, false));

    let closed = fx.kernel.close_thread(&thread, false).await.unwrap();
    assert_eq!(closed.workspace, Some(CleanOutcome::Kept));
    assert_eq!(closed.thread.status, ThreadStatus::Closed);
    assert_eq!(closed.thread.workspace, Some(WorkspaceStatus::Kept));

    // Kept work is still there to continue.
    let second = follow_up(&fx, &thread).await.unwrap();
    ended(&fx, &second, RunStatus::Succeeded).await;
    assert_eq!(
        fx.kernel.thread(&thread).await.unwrap().status,
        ThreadStatus::Idle
    );
    assert_eq!(
        fx.kernel.workspace(&second).await.unwrap().status,
        WorkspaceStatus::Active
    );

    // Forced, the worktree goes, and nothing is left to continue.
    let forced = fx.kernel.close_thread(&thread, true).await.unwrap();
    assert_eq!(forced.workspace, Some(CleanOutcome::Removed));
    assert!(matches!(
        follow_up(&fx, &thread).await,
        Err(KernelError::ThreadClosed(_))
    ));
    // Closing again changes nothing.
    let again = fx.kernel.close_thread(&thread, false).await.unwrap();
    assert_eq!(again.workspace, None);
}

#[tokio::test(start_paused = true)]
async fn an_idle_thread_closes_at_its_deadline_and_not_before() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let (fx, _m) = rig(&ws, vec![]).await;
    let clean = start(&fx, "/repo").await;
    let worked = start(&fx, "/repo").await;
    for run in [&clean, &worked] {
        ended(&fx, run, RunStatus::Succeeded).await;
    }
    ws.set_changes(&worked, changes(2, false));
    let (clean_thread, worked_thread) =
        (thread_of(&fx, &clean).await, thread_of(&fx, &worked).await);

    // The deadline is on the wire: when the thread last changed, plus the limit.
    let ttl = DEFAULT_THREAD_IDLE_TTL;
    assert_eq!(ttl, Duration::from_secs(7 * 24 * 60 * 60));
    let info = fx.kernel.thread(&clean_thread).await.unwrap();
    assert_eq!(
        info.idle_expires_at_ms,
        Some(info.updated_at_ms + u64::try_from(ttl.as_millis()).unwrap())
    );

    // A second short of the limit: nothing happens.
    let almost = ttl - Duration::from_secs(1);
    fx.clock.advance(i64::try_from(almost.as_millis()).unwrap());
    tokio::time::advance(almost).await;
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        fx.kernel.thread(&clean_thread).await.unwrap().status,
        ThreadStatus::Idle
    );

    // At the limit the timer fires: the clean worktree goes, the one with commits is kept.
    fx.clock.advance(2_000);
    tokio::time::advance(Duration::from_secs(2)).await;
    until(async || {
        // A thread is marked closed first, and its worktree released after.
        let a = fx.kernel.workspace(&clean).await.unwrap().status;
        let b = fx.kernel.workspace(&worked).await.unwrap().status;
        (a == WorkspaceStatus::Removed && b == WorkspaceStatus::Kept).then_some(())
    })
    .await;
    for thread in [&clean_thread, &worked_thread] {
        assert_eq!(
            fx.kernel.thread(thread).await.unwrap().status,
            ThreadStatus::Closed
        );
    }
    assert!(ws.exists(&worked) && ws.branch_exists(&worked));
}

#[tokio::test(start_paused = true)]
async fn a_run_in_flight_keeps_its_thread_from_the_idle_limit() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let (fx, _m) = rig(
        &ws,
        vec![MockStep::Sleep(Duration::from_secs(8 * 24 * 3600))],
    )
    .await;
    let run = start(&fx, "/repo").await;
    fx.wait_status(&run, RunStatus::Running).await;
    let thread = thread_of(&fx, &run).await;
    fx.clock.advance(8 * 24 * 3600 * 1000);
    tokio::time::advance(Duration::from_secs(8 * 24 * 3600 - 1)).await;
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        fx.kernel.thread(&thread).await.unwrap().status,
        ThreadStatus::Active,
        "a thread with a live run is not idle"
    );
}

#[tokio::test(start_paused = true)]
async fn clean_acts_on_the_thread_and_a_removed_worktree_closes_it() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let (fx, _m) = rig(&ws, vec![]).await;
    let run = start(&fx, "/repo").await;
    ended(&fx, &run, RunStatus::Succeeded).await;
    let thread = thread_of(&fx, &run).await;

    let items = fx
        .kernel
        .clean_workspaces(
            CleanTarget::Thread {
                thread_id: thread.clone(),
            },
            false,
        )
        .await
        .unwrap();
    assert_eq!(items[0].outcome, CleanOutcome::Removed);
    assert_eq!(
        fx.kernel.thread(&thread).await.unwrap().status,
        ThreadStatus::Closed
    );
    let nobody = ThreadId::from_suffix("nobody").unwrap();
    assert!(matches!(
        fx.kernel
            .clean_workspaces(CleanTarget::Thread { thread_id: nobody }, false)
            .await,
        Err(KernelError::ThreadNotFound(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn a_run_with_no_worktree_has_a_thread_with_none_and_a_follow_up_keeps_its_folder() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let m = mock([]);
    let cfg = KernelConfig {
        sandbox: SandboxLevel::ReadOnly,
        ..config(1)
    };
    let fx = Fx::with_workspace(cfg, registry(std::slice::from_ref(&m)), None, ws.clone()).await;
    let first = start(&fx, "/repo/src").await;
    ended(&fx, &first, RunStatus::Succeeded).await;
    let thread = thread_of(&fx, &first).await;
    let info = fx.kernel.thread(&thread).await.unwrap();
    assert_eq!(
        (info.worktree, info.branch, info.workspace),
        (None, None, None)
    );
    assert_eq!(info.cwd, Some(PathBuf::from("/repo/src")));
    assert_eq!(info.runs[0].start_sha, None);

    let second = follow_up(&fx, &thread).await.unwrap();
    ended(&fx, &second, RunStatus::Succeeded).await;
    let specs = m.log().specs;
    assert_eq!(specs[1].cwd, PathBuf::from("/repo/src"));
    // Closing a thread with nothing to release says so.
    let closed = fx.kernel.close_thread(&thread, false).await.unwrap();
    assert_eq!((closed.workspace, closed.reason), (None, None));
}

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
            output: None,
        }),
    ]
}

#[tokio::test(start_paused = true)]
async fn a_follow_up_is_judged_against_where_it_started_not_the_threads_base() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let mut steps = vec![
        MockStep::started("s", "mock-1"),
        MockStep::Sleep(Duration::from_secs(5)),
    ];
    steps.extend(write("t1", "src/a.rs"));
    steps.push(MockStep::Sleep(Duration::from_secs(5)));
    steps.push(MockStep::done(StopReason::Completed));
    let (fx, _m) = rig(&ws, steps).await;

    let first = start(&fx, "/repo").await;
    ended(&fx, &first, RunStatus::Succeeded).await;
    let thread = thread_of(&fx, &first).await;
    // Run one committed, and the branch moved. The first run was judged against the base.
    ws.set_head(&first, MOVED);
    let seen_by_first = ws.bases_seen();
    assert!(!seen_by_first.is_empty());
    assert!(
        seen_by_first.iter().all(|(_, base)| base == BASE),
        "{seen_by_first:?}"
    );

    let second = follow_up(&fx, &thread).await.unwrap();
    fx.wait_status(&second, RunStatus::Running).await;
    // The second run writes a protected path (the worktree is the thread's, owned by run one).
    ws.plant_protected(&first, &[".claude/settings.json"]);
    ended(&fx, &second, RunStatus::Interrupted).await;

    let journal = fx.journal_with_done(&second).await;
    let ExecEvent::Done { reason, .. } = journal.last().unwrap().1.clone() else {
        panic!("not a Done");
    };
    assert_eq!(
        reason,
        Some(StopDetail::ProtectedPathWrite {
            paths: vec![".claude/settings.json".into()],
            reverted: true
        })
    );
    // Everything the guard looked at for run two, and the revert, was against its start.
    let after: Vec<_> = ws
        .bases_seen()
        .into_iter()
        .skip(seen_by_first.len())
        .collect();
    assert!(!after.is_empty());
    assert!(after.iter().all(|(_, base)| base == MOVED), "{after:?}");
    assert!(after.iter().any(|(kind, _)| kind == "revert"));
}

#[tokio::test(start_paused = true)]
async fn the_diff_scopes_compare_the_thread_the_run_or_one_commit() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let (fx, _m) = rig(&ws, vec![]).await;
    let first = start(&fx, "/repo").await;
    ended(&fx, &first, RunStatus::Succeeded).await;
    let thread = thread_of(&fx, &first).await;
    ws.set_head(&first, MOVED);
    let second = follow_up(&fx, &thread).await.unwrap();
    ended(&fx, &second, RunStatus::Succeeded).await;
    ws.set_diff(
        &first,
        RunChanges {
            commits: 3,
            ..RunChanges::default()
        },
    );
    ws.set_commit(
        &first,
        "abcdef1234",
        RunChanges {
            commits: 1,
            files: vec![reno_proto::ChangedFile {
                path: "a.rs".into(),
                from: None,
                kind: reno_proto::ChangeKind::Added,
                additions: 1,
                deletions: 0,
                binary: false,
            }],
            ..RunChanges::default()
        },
    );
    let seen = ws.bases_seen().len();

    // The default is the whole thread against its base, as before threads.
    let whole = fx.kernel.workspace_diff(&second, None).await.unwrap();
    assert_eq!(
        (whole.run_id.clone(), whole.base_sha.as_str()),
        (second.clone(), BASE)
    );
    assert_eq!(whole.commits, 3);
    // "This run" starts where the run started.
    let this_run = fx
        .kernel
        .workspace_diff_scoped(&second, None, &DiffScope::Run)
        .await
        .unwrap();
    assert_eq!(this_run.base_sha, MOVED);
    // The first run's own scope is the thread's base again.
    let first_run = fx
        .kernel
        .workspace_diff_scoped(&first, None, &DiffScope::Run)
        .await
        .unwrap();
    assert_eq!(first_run.base_sha, BASE);
    let bases: Vec<_> = ws
        .bases_seen()
        .into_iter()
        .skip(seen)
        .map(|(_, b)| b)
        .collect();
    assert_eq!(bases, [BASE, MOVED, BASE]);

    // One commit.
    let one = fx
        .kernel
        .workspace_diff_scoped(
            &second,
            None,
            &DiffScope::Commit {
                sha: "abcdef1234".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!((one.commits, one.files.len()), (1, 1));
    // A name that is not a commit name, or not a commit of the thread, is refused.
    for sha in ["", "xyz", "abc", "-rf", "abcdef1234; rm"] {
        let refused = fx
            .kernel
            .workspace_diff_scoped(&second, None, &DiffScope::Commit { sha: sha.into() })
            .await;
        assert!(
            matches!(refused, Err(KernelError::Invalid(_))),
            "{sha:?}: {refused:?}"
        );
    }
    let unknown = fx
        .kernel
        .workspace_diff_scoped(
            &second,
            None,
            &DiffScope::Commit {
                sha: "deadbeef00".into(),
            },
        )
        .await;
    assert!(
        matches!(unknown, Err(KernelError::Invalid(_))),
        "{unknown:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn start_up_keeps_an_open_threads_worktree_and_releases_a_closed_ones() {
    let dir = tempfile::tempdir().unwrap();
    let store = reno_store::Store::open(dir.path().join("reno.db")).unwrap();
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let mut runs = Vec::new();
    for n in 1..=2 {
        let task = reno_proto::TaskId::from_suffix(&format!("t{n}")).unwrap();
        let run = RunId::from_suffix(&format!("r{n}")).unwrap();
        store
            .create_task(
                reno_store::NewTask {
                    id: task.clone(),
                    goal: "g".into(),
                    class: None,
                    spec: serde_json::json!({"cwd": "/repo"}),
                },
                1,
            )
            .await
            .unwrap();
        store
            .enqueue_run(&task, &run, &id("mock"), 1)
            .await
            .unwrap();
        store
            .transition(&run, 0, RunStatus::Cancelled, 2)
            .await
            .unwrap();
        let thread = store.get_run(&run).await.unwrap().unwrap().thread_id;
        let info = reno_proto::WorkspaceInfo {
            run_id: run.clone(),
            repo: "/repo".into(),
            repo_key: "repo-000000000000".into(),
            path: PathBuf::from(crate::FAKE_WORKTREES)
                .join("repo-000000000000")
                .join(thread.as_str()),
            subdir: String::new(),
            branch: format!("reno/{thread}"),
            base_sha: BASE.into(),
            status: WorkspaceStatus::Active,
            changes: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        };
        store.begin_workspace(info.clone(), 3).await.unwrap();
        store
            .set_workspace_status(&run, WorkspaceStatus::Active, None, 3)
            .await
            .unwrap();
        crate::Workspace::create(ws.as_ref(), &info).await.unwrap();
        runs.push((run, thread));
    }
    // The second thread was closed, but its worktree was never released (a crash).
    store.close_thread(&runs[1].1, 4).await.unwrap();

    let fx = Fx::over_workspace(
        dir,
        store,
        config(1),
        registry(&[mock([])]),
        None,
        ws.clone(),
    )
    .await;
    assert_eq!(
        fx.kernel.workspace(&runs[0].0).await.unwrap().status,
        WorkspaceStatus::Active,
        "an idle thread keeps its worktree across a restart"
    );
    assert!(ws.exists(&runs[0].0));
    assert_eq!(
        fx.kernel.workspace(&runs[1].0).await.unwrap().status,
        WorkspaceStatus::Removed
    );
    assert!(!ws.exists(&runs[1].0));
}

#[tokio::test(start_paused = true)]
async fn threads_list_by_status_with_their_runs() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let (fx, _m) = rig(&ws, vec![]).await;
    let a = start(&fx, "/repo").await;
    ended(&fx, &a, RunStatus::Succeeded).await;
    let b = start(&fx, "/repo").await;
    ended(&fx, &b, RunStatus::Succeeded).await;
    let closed = thread_of(&fx, &a).await;
    fx.kernel.close_thread(&closed, false).await.unwrap();

    let all = fx.kernel.threads(None, None).await.unwrap();
    assert_eq!(all.len(), 2);
    let idle = fx
        .kernel
        .threads(Some(&[ThreadStatus::Idle]), None)
        .await
        .unwrap();
    assert_eq!(idle.len(), 1);
    assert_eq!(idle[0].runs[0].run_id, b);
    let done = fx
        .kernel
        .threads(Some(&[ThreadStatus::Closed]), None)
        .await
        .unwrap();
    assert_eq!(done[0].thread_id, closed);
}

#[tokio::test(start_paused = true)]
async fn a_read_only_follow_up_still_works_in_the_threads_worktree() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let (fx, _m) = rig(&ws, vec![]).await;
    let first = start(&fx, "/repo").await;
    ended(&fx, &first, RunStatus::Succeeded).await;
    let thread = thread_of(&fx, &first).await;
    let shared = fx.kernel.shared();
    let row = shared.run_record(&first).await.unwrap();

    // A reviewer (read-only) continuing the thread reads what the first run committed.
    let placed = crate::workspaces::place(
        shared,
        &row,
        PathBuf::from("/repo"),
        SandboxLevel::ReadOnly,
        false,
    )
    .await
    .unwrap();
    let info = fx.kernel.workspace(&first).await.unwrap();
    assert_eq!(placed.cwd(), info.cwd());
    assert!(matches!(placed, crate::Placement::Isolated(_)));
    assert!(info.cwd().ends_with(thread.as_str()), "{:?}", info.cwd());
}

#[tokio::test(start_paused = true)]
async fn the_idle_limit_closes_a_threads_terminals_before_it_removes_the_worktree() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let (fx, _m) = rig(&ws, vec![]).await;
    let run = start(&fx, "/repo").await;
    ended(&fx, &run, RunStatus::Succeeded).await;
    let thread = thread_of(&fx, &run).await;
    ws.set_terminals(&run, vec![term("one"), term("two")]);

    let ttl = DEFAULT_THREAD_IDLE_TTL;
    fx.clock
        .advance(i64::try_from(ttl.as_millis()).unwrap() + 1_000);
    tokio::time::advance(ttl + Duration::from_secs(1)).await;
    until(async || {
        (fx.kernel.workspace(&run).await.unwrap().status == WorkspaceStatus::Removed).then_some(())
    })
    .await;

    let calls = ws.calls();
    assert!(
        at(&calls, &format!("closing {run}")) < at(&calls, &format!("release {run}")),
        "the hook runs before the release: {calls:?}"
    );
    assert_eq!(
        fx.kernel.thread(&thread).await.unwrap().status,
        ThreadStatus::Closed
    );
}

#[tokio::test(start_paused = true)]
async fn closing_a_thread_reports_the_terminals_the_hook_closed_even_when_the_worktree_is_kept() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let (fx, _m) = rig(&ws, vec![]).await;
    let run = start(&fx, "/repo").await;
    ended(&fx, &run, RunStatus::Succeeded).await;
    ws.set_changes(&run, changes(1, false));
    ws.set_terminals(&run, vec![term("one")]);
    let thread = thread_of(&fx, &run).await;

    let closed = fx.kernel.close_thread(&thread, false).await.unwrap();
    assert_eq!(closed.workspace, Some(CleanOutcome::Kept));
    assert_eq!(closed.closed_terminals, vec![term("one")]);
    let calls = ws.calls();
    assert!(at(&calls, &format!("closing {run}")) < at(&calls, &format!("release {run}")));
    assert_eq!(
        calls
            .iter()
            .filter(|call| call.starts_with("closing"))
            .count(),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn clean_of_a_named_worktree_closes_terminals_first_and_a_bulk_clean_only_for_what_it_removes()
 {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let (fx, _m) = rig(&ws, vec![]).await;
    let named = start(&fx, "/repo").await;
    let kept = start(&fx, "/repo").await;
    let removed = start(&fx, "/repo").await;
    for run in [&named, &kept, &removed] {
        ended(&fx, run, RunStatus::Succeeded).await;
    }
    ws.set_terminals(&named, vec![term("named")]);
    ws.set_terminals(&kept, vec![term("kept")]);
    ws.set_terminals(&removed, vec![term("removed")]);
    ws.set_changes(&kept, changes(2, false));

    let items = fx
        .kernel
        .clean_workspaces(
            CleanTarget::Run {
                run_id: named.clone(),
            },
            false,
        )
        .await
        .unwrap();
    assert_eq!(items[0].closed_terminals, vec![term("named")]);
    let calls = ws.calls();
    assert!(at(&calls, &format!("closing {named}")) < at(&calls, &format!("release {named}")));

    let items = fx
        .kernel
        .clean_workspaces(CleanTarget::AllClean, false)
        .await
        .unwrap();
    let of = |run: &RunId| items.iter().find(|item| &item.run_id == run).unwrap();
    assert_eq!(of(&removed).outcome, CleanOutcome::Removed);
    assert_eq!(of(&removed).closed_terminals, vec![term("removed")]);
    assert_eq!(of(&kept).outcome, CleanOutcome::Kept);
    assert!(of(&kept).closed_terminals.is_empty());
}

fn term(suffix: &str) -> reno_proto::TermId {
    reno_proto::TermId::from_suffix(suffix).unwrap()
}

/// The position of the first call that starts with `prefix`.
fn at(calls: &[String], prefix: &str) -> usize {
    calls
        .iter()
        .position(|call| call.starts_with(prefix))
        .unwrap_or_else(|| panic!("no `{prefix}` in {calls:?}"))
}
