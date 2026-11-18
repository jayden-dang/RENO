#![cfg(test)]

//! Where runs work (T1.3, threads RP0.5): placement, release when a thread closes, clean, and
//! the start-up sweep, over the in-memory `FakeWorkspace`. No git, no disk.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use reno_harness::exec::MockStep;
use reno_proto::defs::DefKind;
use reno_proto::{
    CleanOutcome, CleanTarget, ExecEvent, RunId, RunStatus, SandboxLevel, Seq, ThreadId,
    WorkspaceChanges, WorkspaceInfo, WorkspaceStatus,
};
use reno_store::NewTask;

use super::defs::{FakePort, file, setup};
use super::*;
use crate::{FAKE_WORKTREES, FakeWorkspace, Workspace, WorkspaceError};

fn changes(commits: u32, dirty: bool) -> WorkspaceChanges {
    WorkspaceChanges { commits, dirty }
}

/// Where the fake puts the worktree named `name` (a thread id).
fn worktree(name: &str, repo: &str) -> PathBuf {
    PathBuf::from(FAKE_WORKTREES)
        .join(format!("{repo}-000000000000"))
        .join(name)
}

async fn thread_of(fx: &Fx, run: &RunId) -> ThreadId {
    fx.kernel.run_thread(run).await.unwrap()
}

async fn submit_at(fx: &Fx, cwd: &str) -> RunId {
    fx.kernel
        .submit("work", id("mock"), PathBuf::from(cwd), None)
        .await
        .unwrap()
}

async fn workspace_kinds(fx: &Fx) -> Vec<String> {
    fx.store
        .events_since(Seq(0), 10_000)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .filter(|k| k.starts_with("workspace."))
        .collect()
}

async fn done_diagnostic(fx: &Fx, run: &RunId) -> String {
    match fx.journal_with_done(run).await.last().unwrap().1.clone() {
        ExecEvent::Done { diagnostic, .. } => diagnostic.unwrap_or_default(),
        other => panic!("not a Done: {other:?}"),
    }
}

/// Waits for the run to reach `status` and for its end to be published: the workspace is
/// released before the `Done`.
async fn ended(fx: &Fx, run: &RunId, status: RunStatus) {
    fx.wait_status(run, status).await;
    fx.journal_with_done(run).await;
}

/// Waits until the run has its worktree (a run is `running` just before it is placed).
async fn placed(ws: &FakeWorkspace, run: &RunId) {
    until(async || ws.exists(run).then_some(())).await;
}

async fn with(
    ws: &Arc<FakeWorkspace>,
    config: KernelConfig,
    mocks: &[reno_harness::exec::MockExecutor],
) -> Fx {
    Fx::with_workspace(config, registry(mocks), None, ws.clone()).await
}

#[tokio::test(start_paused = true)]
async fn a_write_run_works_in_its_threads_worktree_which_stays_until_the_thread_closes() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let m = mock([]);
    let fx = with(&ws, config(2), std::slice::from_ref(&m)).await;

    let run = submit_at(&fx, "/repo/crates/x").await;
    ended(&fx, &run, RunStatus::Succeeded).await;
    let thread = thread_of(&fx, &run).await;

    let specs = m.log().specs;
    assert_eq!(specs.len(), 1);
    assert_eq!(
        specs[0].cwd,
        worktree(thread.as_str(), "repo").join("crates/x")
    );
    assert_ne!(specs[0].cwd, PathBuf::from("/repo/crates/x"));

    // The run's end releases nothing: the thread may be continued.
    let info = fx.kernel.workspace(&run).await.unwrap();
    assert_eq!(info.status, WorkspaceStatus::Active);
    assert_eq!(info.branch, format!("reno/{thread}"));
    assert_eq!(info.subdir, "crates/x");
    assert!(ws.exists(&run));

    // Closing it releases a clean worktree.
    let closed = fx.kernel.close_thread(&thread, false).await.unwrap();
    assert_eq!(closed.workspace, Some(CleanOutcome::Removed));
    assert_eq!(
        fx.kernel.workspace(&run).await.unwrap().status,
        WorkspaceStatus::Removed
    );
    assert!(!ws.exists(&run));
    assert_eq!(
        workspace_kinds(&fx).await,
        [
            "workspace.preparing",
            "workspace.active",
            "workspace.removed"
        ]
    );
    // The intent was recorded before the worktree was made.
    assert_eq!(
        ws.calls(),
        [
            format!("plan {run}"),
            format!("create {run}"),
            format!("closing {run}"),
            format!("release {run} IfMerged")
        ]
    );
}

#[tokio::test(start_paused = true)]
async fn a_thread_closed_with_work_left_behind_keeps_its_worktree_and_branch() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let m = mock([MockStep::Sleep(Duration::from_secs(5))]);
    let fx = with(&ws, config(1), &[m]).await;

    let run = submit_at(&fx, "/repo").await;
    fx.wait_status(&run, RunStatus::Running).await;
    placed(&ws, &run).await;
    ws.set_changes(&run, changes(2, true));
    ended(&fx, &run, RunStatus::Succeeded).await;
    assert_eq!(
        fx.kernel.workspace(&run).await.unwrap().status,
        WorkspaceStatus::Active
    );

    let thread = thread_of(&fx, &run).await;
    let closed = fx.kernel.close_thread(&thread, false).await.unwrap();
    assert_eq!(closed.workspace, Some(CleanOutcome::Kept));
    assert!(closed.reason.unwrap().contains("--force"));
    assert_eq!(closed.thread.status, reno_proto::ThreadStatus::Closed);
    let info = fx.kernel.workspace(&run).await.unwrap();
    assert_eq!(info.status, WorkspaceStatus::Kept);
    assert_eq!(info.changes, Some(changes(2, true)));
    assert!(ws.exists(&run) && ws.branch_exists(&run));
    assert!(
        workspace_kinds(&fx)
            .await
            .ends_with(&["workspace.kept".to_owned()])
    );
}

#[tokio::test(start_paused = true)]
async fn two_runs_on_one_repository_get_separate_worktrees() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let m = mock([MockStep::Sleep(Duration::from_secs(5))]);
    let fx = with(&ws, config(2), std::slice::from_ref(&m)).await;

    let a = submit_at(&fx, "/repo").await;
    let b = submit_at(&fx, "/repo").await;
    fx.wait_status(&a, RunStatus::Running).await;
    placed(&ws, &a).await;
    fx.wait_status(&b, RunStatus::Running).await;
    placed(&ws, &b).await;
    until(async || (m.log().specs.len() == 2).then_some(())).await;
    let cwds: Vec<_> = m.log().specs.iter().map(|s| s.cwd.clone()).collect();
    assert_eq!(cwds.len(), 2);
    assert_ne!(cwds[0], cwds[1]);
    assert!(ws.exists(&a) && ws.exists(&b));
}

#[tokio::test(start_paused = true)]
async fn a_read_only_run_works_in_place_and_says_so() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let m = mock([]);
    let cfg = KernelConfig {
        sandbox: SandboxLevel::ReadOnly,
        ..config(1)
    };
    let fx = with(&ws, cfg, std::slice::from_ref(&m)).await;

    let run = submit_at(&fx, "/repo/src").await;
    ended(&fx, &run, RunStatus::Succeeded).await;
    assert_eq!(m.log().specs[0].cwd, PathBuf::from("/repo/src"));
    assert!(ws.calls().iter().all(|c| !c.starts_with("create")));
    assert_eq!(workspace_kinds(&fx).await, ["workspace.in_place"]);
    assert!(matches!(
        fx.kernel.workspace(&run).await,
        Err(KernelError::WorkspaceNotFound(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn a_write_run_outside_a_repository_is_refused_with_a_typed_reason() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let m = mock([]);
    let fx = with(&ws, config(1), std::slice::from_ref(&m)).await;

    let run = submit_at(&fx, "/not-a-repo").await;
    ended(&fx, &run, RunStatus::Failed).await;
    let why = done_diagnostic(&fx, &run).await;
    assert!(why.starts_with("WorkspaceRefused:"), "{why}");
    assert!(why.contains("not in a git repository"), "{why}");
    assert!(m.log().specs.is_empty(), "the executor was never started");
}

#[tokio::test(start_paused = true)]
async fn reno_s_own_scratch_directory_is_worked_in_place() {
    let ws = Arc::new(FakeWorkspace::new());
    let m = mock([]);
    let cfg = KernelConfig {
        scratch_root: Some("/data/workspace".into()),
        ..config(1)
    };
    let fx = with(&ws, cfg, std::slice::from_ref(&m)).await;

    let run = submit_at(&fx, "/data/workspace").await;
    ended(&fx, &run, RunStatus::Succeeded).await;
    assert_eq!(m.log().specs[0].cwd, PathBuf::from("/data/workspace"));
    assert_eq!(workspace_kinds(&fx).await, ["workspace.in_place"]);
}

fn bot_text(name: &str, in_place: bool) -> String {
    format!(
        "name = \"{name}\"\ninstructions = \"x\"\nexecutors = [\"mock\"]\nin_place_writes = {in_place}\n"
    )
}

#[tokio::test(start_paused = true)]
async fn only_a_bot_that_opts_in_may_write_in_place_and_never_in_a_repository() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let m = mock([]);
    let port = FakePort::with(vec![
        file(DefKind::Bot, "loose", &bot_text("loose", true)),
        file(DefKind::Bot, "strict", &bot_text("strict", false)),
    ]);
    let fx = Fx::with_workspace(
        config(2),
        registry(std::slice::from_ref(&m)),
        Some(setup(&port)),
        ws.clone(),
    )
    .await;
    let go = |bot: &'static str, cwd: &'static str| {
        let kernel = fx.kernel.clone();
        async move {
            kernel
                .submit_for_bot(bot, "go", None, PathBuf::from(cwd), None)
                .await
                .unwrap()
        }
    };

    let loose = go("loose", "/plain").await;
    ended(&fx, &loose, RunStatus::Succeeded).await;
    assert_eq!(m.log().specs[0].cwd, PathBuf::from("/plain"));

    let strict = go("strict", "/plain").await;
    ended(&fx, &strict, RunStatus::Failed).await;
    assert!(
        done_diagnostic(&fx, &strict)
            .await
            .contains("in_place_writes")
    );

    // Opting in never lets a repository be written in place.
    let in_repo = go("loose", "/repo").await;
    ended(&fx, &in_repo, RunStatus::Succeeded).await;
    let thread = thread_of(&fx, &in_repo).await;
    assert_eq!(m.log().specs[1].cwd, worktree(thread.as_str(), "repo"));
}

#[tokio::test(start_paused = true)]
async fn a_worktree_that_cannot_be_made_fails_the_run_and_leaves_nothing_behind() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    ws.fail_next_create(WorkspaceError::Failed("disk full".into()));
    let m = mock([]);
    let fx = with(&ws, config(1), std::slice::from_ref(&m)).await;

    let run = submit_at(&fx, "/repo").await;
    ended(&fx, &run, RunStatus::Failed).await;
    assert!(done_diagnostic(&fx, &run).await.contains("disk full"));
    assert!(m.log().specs.is_empty());
    let info = fx.kernel.workspace(&run).await.unwrap();
    assert_eq!(
        info.status,
        WorkspaceStatus::Removed,
        "the intent was reconciled"
    );
}

async fn store_with_cancelled_run(dir: &tempfile::TempDir, n: usize) -> (reno_store::Store, RunId) {
    let store = reno_store::Store::open(dir.path().join("reno.db")).unwrap();
    let task = reno_proto::TaskId::from_suffix(&format!("t{n}")).unwrap();
    let run = RunId::from_suffix(&format!("r{n}")).unwrap();
    store
        .create_task(
            NewTask {
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
    (store, run)
}

fn info(run: &RunId) -> WorkspaceInfo {
    WorkspaceInfo {
        run_id: run.clone(),
        repo: "/repo".into(),
        repo_key: "repo-000000000000".into(),
        path: worktree(run.as_str(), "repo"),
        subdir: String::new(),
        branch: format!("reno/{run}"),
        base_sha: "0".repeat(40),
        status: WorkspaceStatus::Active,
        changes: None,
        created_at_ms: 0,
        updated_at_ms: 0,
    }
}

#[tokio::test(start_paused = true)]
async fn start_up_reconciles_what_a_crash_left_behind() {
    let dir = tempfile::tempdir().unwrap();
    let (store, ended_clean) = store_with_cancelled_run(&dir, 1).await;
    let (_, ended_dirty) = {
        // Same store, a second run.
        let task = reno_proto::TaskId::from_suffix("t2").unwrap();
        let run = RunId::from_suffix("r2").unwrap();
        store
            .create_task(
                NewTask {
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
        ((), run)
    };
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    // One crashed while creating (intent recorded, worktree made, never marked active) and holds
    // nothing; the other has commits.
    for run in [&ended_clean, &ended_dirty] {
        store.begin_workspace(info(run), 3).await.unwrap();
        ws.create(&info(run)).await.unwrap();
    }
    ws.set_changes(&ended_dirty, changes(3, false));

    let fx = Fx::over_workspace(
        dir,
        store,
        config(1),
        registry(&[mock([])]),
        None,
        ws.clone(),
    )
    .await;
    let clean = fx.kernel.workspace(&ended_clean).await.unwrap();
    let dirty = fx.kernel.workspace(&ended_dirty).await.unwrap();
    assert_eq!(clean.status, WorkspaceStatus::Removed);
    assert_eq!(dirty.status, WorkspaceStatus::Kept);
    assert_eq!(dirty.changes, Some(changes(3, false)));
    assert!(!ws.exists(&ended_clean) && ws.exists(&ended_dirty));
}

#[tokio::test(start_paused = true)]
async fn start_up_marks_a_vanished_directory_with_work_as_lost_and_without_work_as_removed() {
    let dir = tempfile::tempdir().unwrap();
    let (store, lost) = store_with_cancelled_run(&dir, 1).await;
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    store.begin_workspace(info(&lost), 3).await.unwrap();
    ws.create(&info(&lost)).await.unwrap();
    ws.set_changes(&lost, changes(1, false));
    ws.vanish(&lost);

    let fx = Fx::over_workspace(
        dir,
        store,
        config(1),
        registry(&[mock([])]),
        None,
        ws.clone(),
    )
    .await;
    let got = fx.kernel.workspace(&lost).await.unwrap();
    assert_eq!(got.status, WorkspaceStatus::Lost);
    assert!(ws.branch_exists(&lost), "the branch with commits stays");
}

/// A kept workspace for a finished run whose thread was closed, made the way a run makes it.
async fn kept_after_a_run(ws: &Arc<FakeWorkspace>, fx: &Fx, repo_dir: &str) -> RunId {
    let run = submit_at(fx, repo_dir).await;
    fx.wait_status(&run, RunStatus::Running).await;
    placed(ws, &run).await;
    ws.set_changes(&run, changes(2, false));
    ended(fx, &run, RunStatus::Succeeded).await;
    let thread = thread_of(fx, &run).await;
    fx.kernel.close_thread(&thread, false).await.unwrap();
    assert_eq!(
        fx.kernel.workspace(&run).await.unwrap().status,
        WorkspaceStatus::Kept
    );
    run
}

#[tokio::test(start_paused = true)]
async fn clean_never_deletes_unmerged_commits_without_force() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let m = mock([MockStep::Sleep(Duration::from_secs(1))]);
    let fx = with(&ws, config(1), &[m]).await;
    let run = kept_after_a_run(&ws, &fx, "/repo").await;

    for target in [
        CleanTarget::Run {
            run_id: run.clone(),
        },
        CleanTarget::Merged,
        CleanTarget::AllClean,
    ] {
        let items = fx.kernel.clean_workspaces(target, false).await.unwrap();
        assert!(
            items.iter().all(|i| i.outcome == CleanOutcome::Kept),
            "{items:?}"
        );
        assert!(ws.exists(&run) && ws.branch_exists(&run));
    }
    let items = fx
        .kernel
        .clean_workspaces(
            CleanTarget::Run {
                run_id: run.clone(),
            },
            false,
        )
        .await
        .unwrap();
    assert!(
        items[0].reason.as_deref().unwrap().contains("--force"),
        "{items:?}"
    );

    // Merged work goes with `--merged`; forced work goes with `force`.
    ws.set_merged(&run);
    let items = fx
        .kernel
        .clean_workspaces(CleanTarget::Merged, false)
        .await
        .unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].outcome, CleanOutcome::Removed);
    assert_eq!(
        fx.kernel.workspace(&run).await.unwrap().status,
        WorkspaceStatus::Removed
    );
}

#[tokio::test(start_paused = true)]
async fn force_removes_a_workspace_with_unmerged_commits() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let m = mock([MockStep::Sleep(Duration::from_secs(1))]);
    let fx = with(&ws, config(1), &[m]).await;
    let run = kept_after_a_run(&ws, &fx, "/repo").await;

    let items = fx
        .kernel
        .clean_workspaces(
            CleanTarget::Run {
                run_id: run.clone(),
            },
            true,
        )
        .await
        .unwrap();
    assert_eq!(items[0].outcome, CleanOutcome::Removed);
    assert!(!ws.exists(&run) && !ws.branch_exists(&run));
    let got = fx.kernel.workspace(&run).await.unwrap();
    assert_eq!(got.status, WorkspaceStatus::Removed);
    assert_eq!(
        got.changes,
        Some(changes(2, false)),
        "what was lost stays on record"
    );
}

#[tokio::test(start_paused = true)]
async fn all_clean_removes_what_is_clean_and_leaves_the_rest() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let m = mock([MockStep::Sleep(Duration::from_secs(1))]);
    let fx = with(&ws, config(1), &[m]).await;
    let worked = kept_after_a_run(&ws, &fx, "/repo").await;
    // A kept workspace whose changes were later thrown away by hand.
    let tidied = kept_after_a_run(&ws, &fx, "/repo").await;
    ws.set_changes(&tidied, changes(0, false));

    let items = fx
        .kernel
        .clean_workspaces(CleanTarget::AllClean, false)
        .await
        .unwrap();
    let outcome = |run: &RunId| items.iter().find(|i| &i.run_id == run).unwrap().outcome;
    assert_eq!(outcome(&tidied), CleanOutcome::Removed);
    assert_eq!(outcome(&worked), CleanOutcome::Kept);
}

#[tokio::test(start_paused = true)]
async fn a_workspace_of_a_live_run_cannot_be_cleaned() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let m = mock([MockStep::Sleep(Duration::from_secs(60))]);
    let fx = with(&ws, config(1), &[m]).await;
    let run = submit_at(&fx, "/repo").await;
    fx.wait_status(&run, RunStatus::Running).await;
    placed(&ws, &run).await;

    let refused = fx
        .kernel
        .clean_workspaces(
            CleanTarget::Run {
                run_id: run.clone(),
            },
            true,
        )
        .await;
    assert!(matches!(
        refused,
        Err(KernelError::Workspace(WorkspaceError::InUse(_)))
    ));
    // A batch simply skips it.
    let items = fx
        .kernel
        .clean_workspaces(CleanTarget::AllClean, true)
        .await
        .unwrap();
    assert!(items.is_empty());
    assert!(ws.exists(&run));
    assert!(matches!(
        fx.kernel
            .clean_workspaces(
                CleanTarget::Run {
                    run_id: RunId::from_suffix("nope").unwrap()
                },
                false
            )
            .await,
        Err(KernelError::WorkspaceNotFound(_))
    ));
}

#[tokio::test(start_paused = true)]
async fn workspaces_list_by_status() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let m = mock([MockStep::Sleep(Duration::from_secs(1))]);
    let fx = with(&ws, config(1), &[m]).await;
    let kept = kept_after_a_run(&ws, &fx, "/repo").await;
    let all = fx.kernel.workspaces(None).await.unwrap();
    assert_eq!(all.len(), 1);
    let only_kept = fx
        .kernel
        .workspaces(Some(&[WorkspaceStatus::Kept]))
        .await
        .unwrap();
    assert_eq!(only_kept[0].run_id, kept);
    assert!(
        fx.kernel
            .workspaces(Some(&[WorkspaceStatus::Removed]))
            .await
            .unwrap()
            .is_empty()
    );
}

fn changed(path: &str, kind: reno_proto::ChangeKind, additions: u32) -> reno_proto::ChangedFile {
    reno_proto::ChangedFile {
        path: path.into(),
        from: None,
        kind,
        additions,
        deletions: 0,
        binary: false,
    }
}

#[tokio::test(start_paused = true)]
async fn a_kept_workspace_reports_what_the_run_changed_and_the_patch_of_one_file() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let m = mock([MockStep::Sleep(Duration::from_secs(5))]);
    let fx = with(&ws, config(1), &[m]).await;
    let run = submit_at(&fx, "/repo").await;
    fx.wait_status(&run, RunStatus::Running).await;
    placed(&ws, &run).await;
    ws.set_changes(&run, changes(1, true));
    ws.set_diff(
        &run,
        crate::RunChanges {
            commits: 1,
            files: vec![changed("a.rs", reno_proto::ChangeKind::Added, 3)],
            patch: Some("PATCH".into()),
            patch_truncated: true,
        },
    );
    ended(&fx, &run, RunStatus::Succeeded).await;

    let info = fx.kernel.workspace(&run).await.unwrap();
    let list = fx.kernel.workspace_diff(&run, None).await.unwrap();
    assert_eq!(list.run_id, run);
    assert_eq!(
        (list.branch.as_str(), list.base_sha.as_str()),
        (info.branch.as_str(), info.base_sha.as_str())
    );
    assert_eq!((list.commits, list.files.len()), (1, 1));
    assert_eq!(list.patch, None, "no path, no patch");
    let one = fx.kernel.workspace_diff(&run, Some("a.rs")).await.unwrap();
    assert_eq!(one.patch.as_deref(), Some("PATCH"));
    assert!(one.patch_truncated);
}

#[tokio::test(start_paused = true)]
async fn a_removed_workspace_has_an_empty_diff_not_an_error_and_an_unknown_run_is_not_found() {
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let m = mock([MockStep::Sleep(Duration::from_secs(5))]);
    let fx = with(&ws, config(1), &[m]).await;
    let run = submit_at(&fx, "/repo").await;
    fx.wait_status(&run, RunStatus::Running).await;
    placed(&ws, &run).await;
    // Whatever the port would say, a removed workspace has nothing.
    ws.set_diff(
        &run,
        crate::RunChanges {
            commits: 9,
            files: vec![changed("a.rs", reno_proto::ChangeKind::Added, 3)],
            ..Default::default()
        },
    );
    ended(&fx, &run, RunStatus::Succeeded).await;
    // The fake's tree holds nothing, so closing the thread removes it.
    ws.set_changes(&run, changes(0, false));
    let thread = thread_of(&fx, &run).await;
    fx.kernel.close_thread(&thread, false).await.unwrap();
    let info = fx.kernel.workspace(&run).await.unwrap();
    assert_eq!(info.status, WorkspaceStatus::Removed);

    let got = fx.kernel.workspace_diff(&run, Some("a.rs")).await.unwrap();
    assert_eq!((got.commits, got.files.len(), got.patch), (0, 0, None));
    assert_eq!(got.branch, info.branch);
    assert_eq!(got.base_sha, info.base_sha);

    let stranger = RunId::from_suffix("nope").unwrap();
    assert!(matches!(
        fx.kernel.workspace_diff(&stranger, None).await,
        Err(crate::KernelError::WorkspaceNotFound(_))
    ));
}
