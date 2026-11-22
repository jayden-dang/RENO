//! ROAD-33: the transcript demo world of the fake daemon, and the guard that it is off unless
//! asked for. No network, no wall clock: tokio time is paused.

#![cfg(feature = "fake")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use futures::StreamExt;
use reno_client::fake::{FakeConfig, FakeDaemon};
use reno_proto::{
    ErrorCode, ExecEvent, FsListParams, FsReadParams, RunId, RunListParams, RunStatus, StopReason,
    TaskRunParams,
};

fn run(name: &str) -> RunId {
    RunId::from_suffix(name).unwrap()
}

fn on() -> FakeConfig {
    FakeConfig {
        transcript_threads: true,
        ..FakeConfig::default()
    }
}

fn dir(run_id: &str, dir: &str) -> FsListParams {
    FsListParams {
        run_id: run(run_id),
        dir: dir.into(),
        cursor: None,
        limit: None,
    }
}

#[tokio::test(start_paused = true)]
async fn the_demo_world_is_off_by_default_so_nothing_existing_moves() {
    let client = FakeDaemon::spawn().await.unwrap();
    let runs = client.list_runs(RunListParams::default()).await.unwrap();
    assert!(runs.iter().all(|r| !r.run_id.as_str().contains("acp")));
    assert!(runs.iter().all(|r| !r.run_id.as_str().contains("render1")));
    let src = client.fs_list(dir("builder1", "src")).await.unwrap();
    let names: Vec<&str> = src.entries.iter().map(|e| e.name.as_str()).collect();
    assert_eq!(names, ["lib.rs", "main.rs"]);
    let docs = client.fs_list(dir("builder1", "docs")).await.unwrap();
    assert_eq!(docs.entries.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn the_demo_world_has_the_handshake_thread_and_serves_what_the_replies_link_to() {
    let client = FakeDaemon::spawn_with(on()).await.unwrap();
    let runs = client.list_runs(RunListParams::default()).await.unwrap();
    for name in ["acp1", "acp8", "render1"] {
        assert!(runs.iter().any(|r| r.run_id == run(name)), "{name}");
    }
    let threads = client.list_threads(None).await.unwrap();
    let handshake = threads
        .iter()
        .find(|t| t.runs.iter().any(|r| r.run_id == run("acp1")))
        .expect("the handshake thread");
    assert_eq!(handshake.runs.len(), 8);
    assert_eq!(handshake.runs.last().unwrap().run_id, run("acp8"));

    let (mime, bytes) = client
        .fs_read_image_all(run("render1"), "docs/graph.png".into())
        .await
        .unwrap();
    assert_eq!(mime, "image/png");
    assert_eq!(&bytes[..4], b"\x89PNG");
    let source = client
        .fs_read(FsReadParams {
            run_id: run("render1"),
            path: "src/supervisor.rs".into(),
        })
        .await
        .unwrap();
    assert!(source.text.unwrap().lines().count() >= 42);
}

#[tokio::test(start_paused = true)]
async fn without_the_demo_world_the_linked_files_are_not_there() {
    let client = FakeDaemon::spawn().await.unwrap();
    let missing = client
        .fs_read(FsReadParams {
            run_id: run("builder1"),
            path: "src/supervisor.rs".into(),
        })
        .await
        .unwrap_err();
    assert_eq!(missing.code(), Some(ErrorCode::NotFound));
}

#[tokio::test(start_paused = true)]
async fn longreply_streams_one_long_message_and_ends_with_it_as_the_final_answer() {
    let client = FakeDaemon::spawn().await.unwrap();
    let id = client
        .run_task(TaskRunParams {
            prompt: "/longreply".to_owned(),
            executor: Some("claude-code".parse().unwrap()),
            ..TaskRunParams::default()
        })
        .await
        .unwrap();
    // Several hundred paced steps.
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(
        client.get_run(&id).await.unwrap().status,
        RunStatus::Succeeded
    );
    let events: Vec<ExecEvent> = client
        .run_events(id, None)
        .map(|item| item.unwrap().event)
        .collect()
        .await;
    let texts: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            ExecEvent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(texts.len() > 200);
    assert!(texts.concat().split_whitespace().count() > 3_000);
    // The reply is the last thing said: nothing but its own end follows the last text.
    assert!(matches!(
        events.last(),
        Some(ExecEvent::Done {
            stop: StopReason::Completed,
            ..
        })
    ));
    assert!(matches!(events[events.len() - 2], ExecEvent::Text { .. }));
}
