//! Threads on the wire (RP0.5): the shapes are literal JSON, the additions are additive (an older
//! client's text still parses and means what it meant), and the feature and method names are the
//! ones the CLI and the app rely on.

#![allow(clippy::unwrap_used)]

use std::fmt::Debug;

use reno_proto::*;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

fn golden<T: Serialize + DeserializeOwned + PartialEq + Debug>(value: &T, wire: Value) {
    assert_eq!(serde_json::to_value(value).unwrap(), wire, "serialise");
    let parsed: T = serde_json::from_str(&wire.to_string()).unwrap();
    assert_eq!(&parsed, value, "deserialise");
}

fn thread(name: &str) -> ThreadId {
    ThreadId::from_suffix(name).unwrap()
}

fn run(name: &str) -> RunId {
    RunId::from_suffix(name).unwrap()
}

#[test]
fn the_names_and_the_feature() {
    assert_eq!(FEATURE_THREADS, "threads");
    assert!(FEATURES.contains(&FEATURE_THREADS));
    assert_eq!(ThreadList::NAME, "thread.list");
    assert_eq!(ThreadGet::NAME, "thread.get");
    assert_eq!(ThreadClose::NAME, "thread.close");
    // Appended: nothing earlier moved (later features follow it).
    let at = FEATURES.iter().position(|f| *f == FEATURE_THREADS).unwrap();
    assert_eq!(at, FEATURES.len() - 2);
    const { assert!(THREAD_LIST_DEFAULT_LIMIT <= THREAD_LIST_MAX_LIMIT) };
}

#[test]
fn thread_ids_have_their_own_prefix() {
    assert_eq!(thread("01HZX9").to_string(), "thr_01HZX9");
    assert!("run_01HZX9".parse::<ThreadId>().is_err());
    assert!("thr_01HZX9".parse::<RunId>().is_err());
}

#[test]
fn a_thread_on_the_wire() {
    let info = ThreadInfo {
        thread_id: thread("a1"),
        status: ThreadStatus::Idle,
        cwd: Some("/work/app/src".into()),
        project: Some("/work/app".into()),
        worktree: Some("/data/worktrees/app-1/thr_a1".into()),
        branch: Some("reno/thr_a1".into()),
        base_sha: Some("abc".into()),
        workspace: Some(WorkspaceStatus::Active),
        runs: vec![
            ThreadRun {
                run_id: run("r1"),
                parent_run: None,
                start_sha: Some("abc".into()),
            },
            ThreadRun {
                run_id: run("r2"),
                parent_run: Some(run("r1")),
                start_sha: Some("def".into()),
            },
        ],
        idle_expires_at_ms: Some(604_800_000),
        created_at_ms: 1,
        updated_at_ms: 2,
    };
    golden(
        &info,
        json!({
            "threadId": "thr_a1", "status": "idle", "cwd": "/work/app/src",
            "project": "/work/app", "worktree": "/data/worktrees/app-1/thr_a1",
            "branch": "reno/thr_a1", "baseSha": "abc", "workspace": "active",
            "runs": [
                {"runId": "run_r1", "startSha": "abc"},
                {"runId": "run_r2", "parentRun": "run_r1", "startSha": "def"}
            ],
            "idleExpiresAtMs": 604_800_000, "createdAtMs": 1, "updatedAtMs": 2
        }),
    );
    // A thread with no worktree leaves those fields out.
    let bare = ThreadInfo {
        thread_id: thread("a2"),
        status: ThreadStatus::Closed,
        cwd: None,
        project: None,
        worktree: None,
        branch: None,
        base_sha: None,
        workspace: None,
        runs: vec![],
        idle_expires_at_ms: None,
        created_at_ms: 0,
        updated_at_ms: 0,
    };
    golden(
        &bare,
        json!({"threadId": "thr_a2", "status": "closed", "runs": [],
               "createdAtMs": 0, "updatedAtMs": 0}),
    );
    assert_eq!(info.latest_run().unwrap().run_id, run("r2"));
}

#[test]
fn the_thread_methods_params_and_results() {
    golden(&ThreadListParams::default(), json!({}));
    golden(
        &ThreadListParams {
            status: Some(vec![ThreadStatus::Active, ThreadStatus::Idle]),
            limit: Some(50),
        },
        json!({"status": ["active", "idle"], "limit": 50}),
    );
    golden(
        &ThreadGetParams {
            thread_id: thread("a1"),
        },
        json!({"threadId": "thr_a1"}),
    );
    golden(
        &ThreadCloseParams {
            thread_id: thread("a1"),
            force: true,
        },
        json!({"threadId": "thr_a1", "force": true}),
    );
    // `force` is optional: a client that leaves it out means a plain close.
    let plain: ThreadCloseParams = serde_json::from_value(json!({"threadId": "thr_a1"})).unwrap();
    assert!(!plain.force);
}

#[test]
fn task_run_and_workspace_diff_take_their_additions_and_older_text_still_parses() {
    // An older client's `task.run` has no `threadId`, and means a new thread.
    let old: TaskRunParams = serde_json::from_value(json!({"prompt": "go"})).unwrap();
    assert_eq!(old.thread_id, None);
    let follow = TaskRunParams {
        prompt: "go".into(),
        thread_id: Some(thread("a1")),
        ..TaskRunParams::default()
    };
    golden(&follow, json!({"prompt": "go", "threadId": "thr_a1"}));
    let result: TaskRunResult = serde_json::from_value(json!({"runId": "run_r1"})).unwrap();
    assert_eq!(result.thread_id, None);

    // `workspace.diff` without a scope is what it always was.
    let old: WorkspaceDiffParams = serde_json::from_value(json!({"runId": "run_r1"})).unwrap();
    assert_eq!(old.scope, None);
    assert_eq!(DiffScope::default(), DiffScope::Thread);
    golden(&DiffScope::Thread, json!({"type": "thread"}));
    golden(&DiffScope::Run, json!({"type": "run"}));
    golden(
        &DiffScope::Commit {
            sha: "abc1234".into(),
        },
        json!({"type": "commit", "sha": "abc1234"}),
    );
    golden(
        &WorkspaceDiffParams {
            run_id: run("r1"),
            path: Some("a.rs".into()),
            scope: Some(DiffScope::Run),
        },
        json!({"runId": "run_r1", "path": "a.rs", "scope": {"type": "run"}}),
    );
    golden(
        &CleanTarget::Thread {
            thread_id: thread("a1"),
        },
        json!({"type": "thread", "threadId": "thr_a1"}),
    );
}

#[test]
fn closing_says_what_became_of_the_worktree() {
    let thread_info = ThreadInfo {
        thread_id: thread("a1"),
        status: ThreadStatus::Closed,
        cwd: None,
        project: None,
        worktree: None,
        branch: None,
        base_sha: None,
        workspace: Some(WorkspaceStatus::Kept),
        runs: vec![],
        idle_expires_at_ms: None,
        created_at_ms: 0,
        updated_at_ms: 0,
    };
    golden(
        &ThreadCloseResult {
            thread: thread_info,
            workspace: Some(CleanOutcome::Kept),
            reason: Some("1 commit(s) not merged; use --force to remove it anyway".into()),
        },
        json!({
            "thread": {"threadId": "thr_a1", "status": "closed", "workspace": "kept",
                       "runs": [], "createdAtMs": 0, "updatedAtMs": 0},
            "workspace": "kept",
            "reason": "1 commit(s) not merged; use --force to remove it anyway"
        }),
    );
}
