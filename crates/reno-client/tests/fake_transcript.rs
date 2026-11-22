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

fn render2() -> Vec<ExecEvent> {
    reno_client::fake::transcript::seeded_run_events("render2")
        .into_iter()
        .map(|(_, event)| event)
        .collect()
}

fn calls(events: &[ExecEvent]) -> Vec<(&str, &reno_proto::ToolCall)> {
    events
        .iter()
        .filter_map(|e| match e {
            ExecEvent::Tool { id, call } => Some((id.as_str(), call)),
            _ => None,
        })
        .collect()
}

fn result_of<'a>(events: &'a [ExecEvent], call: &str) -> Option<&'a ExecEvent> {
    events
        .iter()
        .find(|e| matches!(e, ExecEvent::ToolResult { id, .. } if id == call))
}

/// The stop of the attempt a call belongs to: the first `Done` after it.
fn stop_after(events: &[ExecEvent], call: &str) -> StopReason {
    let at = events
        .iter()
        .position(|e| matches!(e, ExecEvent::Tool { id, .. } if id == call))
        .unwrap();
    events[at..]
        .iter()
        .find_map(|e| match e {
            ExecEvent::Done { stop, .. } => Some(*stop),
            _ => None,
        })
        .unwrap()
}

#[tokio::test(start_paused = true)]
async fn the_second_render_run_has_every_remaining_kind_of_content() {
    let client = FakeDaemon::spawn_with(on()).await.unwrap();
    let events = render2();
    let streamed: Vec<ExecEvent> = client
        .run_events(run("render2"), None)
        .map(|item| item.unwrap().event)
        .collect()
        .await;
    assert_eq!(streamed, events);
    let calls = calls(&events);
    let find = |name: &str| {
        calls
            .iter()
            .filter(|(_, c)| c.name == name)
            .copied()
            .collect::<Vec<_>>()
            .into_iter()
    };

    assert!(matches!(
        events.last(),
        Some(ExecEvent::Done { stop: StopReason::Error, result: Some(r), .. }) if !r.is_empty()
    ));

    let (_, edit) = find("edit").next().expect("an Edit call");
    for key in ["file_path", "old_string", "new_string"] {
        assert!(edit.input[key].is_string(), "{key}");
    }

    let (_, patch) = find("apply_patch").next().expect("a patch call");
    let patch = patch.input["patch"].as_str().unwrap();
    assert_eq!(patch.matches("*** Update File: ").count(), 2);

    let (bash_id, bash) = find("bash")
        .find(|(id, _)| {
            matches!(
                result_of(&events, id),
                Some(ExecEvent::ToolResult { ok: false, .. })
            )
        })
        .expect("a failed Bash call");
    assert!(bash.input["command"].is_string());
    assert!(matches!(
        result_of(&events, bash_id),
        Some(ExecEvent::ToolResult { summary: Some(s), .. }) if !s.is_empty()
    ));

    let (read_id, read) = find("read").next().expect("a read call");
    assert!(read.input.get("file_path").is_none());
    let Some(ExecEvent::ToolResult {
        output: Some(out), ..
    }) = result_of(&events, read_id)
    else {
        panic!("the read has output");
    };
    assert!(
        out.text
            .lines()
            .all(|l| l.trim_start().chars().next().unwrap().is_ascii_digit())
    );

    assert!(events.iter().any(|e| matches!(
        e,
        ExecEvent::ToolResult { output: Some(o), .. } if o.text.contains("src/supervisor.rs")
    )));

    let unanswered: Vec<_> = calls
        .iter()
        .filter(|(id, _)| result_of(&events, id).is_none())
        .map(|(id, _)| stop_after(&events, id))
        .collect();
    assert!(unanswered.contains(&StopReason::Interrupted));
    assert!(unanswered.contains(&StopReason::Completed));

    let reply = events
        .iter()
        .filter_map(|e| match e {
            ExecEvent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .find(|t| t.contains("[!NOTE]"))
        .expect("a reply with a callout");
    assert!(reply.starts_with("---\n"), "front matter");
    assert!(reply.contains("- [x] ") && reply.contains("- [ ] "));
    assert!(reply.contains("<!--"));
    assert!(reply.contains("[^1]: ") && reply.contains("[^2]: "));
    let wide = reply
        .lines()
        .filter(|l| l.starts_with('|'))
        .map(|l| l.chars().count())
        .max()
        .unwrap();
    assert!(wide > 120, "a table wider than the column");
    assert!(reply.contains("](docs/flow.svg)"));

    let (mime, bytes) = client
        .fs_read_image_all(run("render2"), "docs/flow.svg".into())
        .await
        .unwrap();
    assert_eq!(mime, "image/svg+xml");
    assert!(bytes.starts_with(b"<svg"));
}
