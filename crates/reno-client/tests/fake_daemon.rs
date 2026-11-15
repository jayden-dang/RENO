//! End-to-end tests of the client and the mirror against the fake daemon. No network, no wall
//! clock: tokio time is paused, so script delays advance instantly and deterministically.

#![cfg(feature = "fake")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use futures::StreamExt;
use reno_client::fake::{FakeConfig, FakeDaemon};
use reno_client::{Client, ClientError, ConnectionStatus, Mirror, MirrorConfig, State, Updates};
use reno_proto::{
    ApprovalDecision, ApprovalId, ExecEvent, PROTOCOL_VERSION, RunId, RunListParams, RunStatus,
    Seq, StopReason, TaskRunParams,
};

fn task(prompt: &str) -> TaskRunParams {
    TaskRunParams {
        prompt: prompt.to_owned(),
        executor: "claude-code".parse().unwrap(),
        cwd: None,
        model: None,
    }
}

fn run(name: &str) -> RunId {
    RunId::from_suffix(name).unwrap()
}

fn appr(name: &str) -> ApprovalId {
    ApprovalId::from_suffix(name).unwrap()
}

async fn events(client: &Client, run: &RunId) -> Vec<(Seq, ExecEvent)> {
    client
        .run_events(run.clone(), None)
        .map(|item| {
            let item = item.unwrap();
            (item.seq, item.event)
        })
        .collect()
        .await
}

fn assert_consecutive(events: &[(Seq, ExecEvent)]) {
    for (i, (seq, _)) in events.iter().enumerate() {
        assert_eq!(seq.0, i as u64 + 1, "events must be ordered and gap-free");
    }
}

fn is_done(event: &ExecEvent, stop: StopReason) -> bool {
    matches!(event, ExecEvent::Done { stop: s, .. } if *s == stop)
}

#[tokio::test(start_paused = true)]
async fn hello_run_task_transcript_and_terminal_state() {
    let client = FakeDaemon::spawn().await.unwrap();
    let hello = client.hello().await.unwrap();
    assert_eq!(hello.protocol_version, PROTOCOL_VERSION);
    assert!(client.supports("approvals"));
    let status = client.status().await.unwrap();
    assert_eq!(status.executors.len(), 2);

    let id = client.run_task(task("tidy the readme")).await.unwrap();
    assert_eq!(client.get_run(&id).await.unwrap().status, RunStatus::Queued);

    let transcript = events(&client, &id).await;
    assert_consecutive(&transcript);
    assert!(matches!(transcript[0].1, ExecEvent::Started { .. }));
    assert!(
        transcript
            .iter()
            .any(|(_, e)| matches!(e, ExecEvent::Text { .. }))
    );
    assert!(
        transcript
            .iter()
            .any(|(_, e)| matches!(e, ExecEvent::Tool { .. }))
    );
    assert!(is_done(
        &transcript.last().unwrap().1,
        StopReason::Completed
    ));

    let summary = client.get_run(&id).await.unwrap();
    assert_eq!(summary.status, RunStatus::Succeeded);
    assert_eq!(summary.stop_reason, Some(StopReason::Completed));
    let listed = client.list_runs(RunListParams::default()).await.unwrap();
    assert_eq!(listed.len(), 7, "six demo runs and the new one");
    assert_eq!(listed[0].run_id, id, "newest first");
}

#[tokio::test(start_paused = true)]
async fn demo_state_matches_the_prototype() {
    let client = FakeDaemon::spawn().await.unwrap();
    let runs = client.list_runs(RunListParams::default()).await.unwrap();
    let status_of = |name: &str| runs.iter().find(|r| r.run_id == run(name)).unwrap().status;
    assert_eq!(status_of("keeper1"), RunStatus::WaitingApproval);
    assert_eq!(status_of("scout1"), RunStatus::WaitingApproval);
    assert_eq!(status_of("builder1"), RunStatus::Running);
    assert_eq!(status_of("reviewer1"), RunStatus::Succeeded);
    assert_eq!(status_of("architect1"), RunStatus::Queued);
    assert_eq!(status_of("builder0"), RunStatus::Failed);
    assert!(
        runs.iter()
            .any(|r| r.title.starts_with("keeper · atlas-api"))
    );

    let approvals = client.list_approvals().await.unwrap();
    assert_eq!(approvals.len(), 2);
    assert_eq!(approvals[0].id, appr("keeperpush"));
    assert_eq!(approvals[0].run_id, run("keeper1"));
    assert_eq!(
        approvals[0].subject.command.as_deref(),
        Some("git push -u origin reno/keeper/deps-1115")
    );
    assert_eq!(approvals[1].id, appr("scoutfetch"));
    assert_eq!(
        approvals[1].subject.action,
        reno_proto::ActionClass::Network
    );

    let only_running = client
        .list_runs(RunListParams {
            status: Some(vec![RunStatus::Running]),
            limit: None,
        })
        .await
        .unwrap();
    assert_eq!(only_running.len(), 1);
}

#[tokio::test(start_paused = true)]
async fn approve_removes_the_approval_and_the_run_finishes() {
    let client = FakeDaemon::spawn().await.unwrap();
    client
        .decide(&appr("keeperpush"), ApprovalDecision::ApproveOnce)
        .await
        .unwrap();
    let left = client.list_approvals().await.unwrap();
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, appr("scoutfetch"));

    let transcript = events(&client, &run("keeper1")).await;
    assert_consecutive(&transcript);
    let result = transcript
        .iter()
        .rev()
        .find_map(|(_, e)| match e {
            ExecEvent::ToolResult { id, ok, .. } => Some((id.clone(), *ok)),
            _ => None,
        })
        .unwrap();
    assert_eq!(result, ("t2".to_owned(), true), "answers the held push");
    assert!(is_done(
        &transcript.last().unwrap().1,
        StopReason::Completed
    ));
    let summary = client.get_run(&run("keeper1")).await.unwrap();
    assert_eq!(summary.status, RunStatus::Succeeded);

    let again = client
        .decide(&appr("keeperpush"), ApprovalDecision::ApproveOnce)
        .await;
    assert_eq!(
        again.unwrap_err().code(),
        Some(reno_proto::ErrorCode::NotFound)
    );
}

#[tokio::test(start_paused = true)]
async fn deny_ends_the_run_as_failed() {
    let client = FakeDaemon::spawn().await.unwrap();
    client
        .decide(&appr("scoutfetch"), ApprovalDecision::Deny)
        .await
        .unwrap();
    let transcript = events(&client, &run("scout1")).await;
    assert_consecutive(&transcript);
    assert!(
        transcript
            .iter()
            .any(|(_, e)| matches!(e, ExecEvent::ToolResult { ok: false, .. }))
    );
    let (_, last) = transcript.last().unwrap();
    assert!(
        matches!(last, ExecEvent::Done { stop: StopReason::Error, diagnostic: Some(d), .. } if d == "approval denied")
    );
    assert_eq!(
        client.get_run(&run("scout1")).await.unwrap().status,
        RunStatus::Failed
    );
    assert_eq!(client.list_approvals().await.unwrap().len(), 1);
}

#[tokio::test(start_paused = true)]
async fn a_new_run_holds_for_approval_then_resumes() {
    let client = FakeDaemon::spawn().await.unwrap();
    let mut approvals = client.watch_approvals().await.unwrap();
    assert_eq!(approvals.next().await.unwrap().unwrap().approvals.len(), 2);

    let id = client
        .run_task(task("update the lockfile and push it"))
        .await
        .unwrap();
    let held = loop {
        let snapshot = approvals.next().await.unwrap().unwrap();
        if let Some(a) = snapshot.approvals.iter().find(|a| a.run_id == id) {
            break a.clone();
        }
    };
    assert_eq!(held.subject.tool, "bash");
    assert_eq!(
        client.get_run(&id).await.unwrap().status,
        RunStatus::WaitingApproval
    );

    client
        .decide(&held.id, ApprovalDecision::ApproveOnce)
        .await
        .unwrap();
    let transcript = events(&client, &id).await;
    assert_consecutive(&transcript);
    assert!(
        transcript
            .iter()
            .any(|(_, e)| matches!(e, ExecEvent::NeedsApproval { .. }))
    );
    assert!(is_done(
        &transcript.last().unwrap().1,
        StopReason::Completed
    ));
    assert_eq!(
        client.get_run(&id).await.unwrap().status,
        RunStatus::Succeeded
    );
}

#[tokio::test(start_paused = true)]
async fn cancel_ends_a_running_run() {
    let client = FakeDaemon::spawn().await.unwrap();
    let status = client
        .cancel_run(&run("builder1"), Some("not needed".into()))
        .await
        .unwrap();
    assert_eq!(status, RunStatus::Cancelled);
    let transcript = events(&client, &run("builder1")).await;
    assert!(is_done(
        &transcript.last().unwrap().1,
        StopReason::Interrupted
    ));
    let again = client.cancel_run(&run("builder1"), None).await;
    assert_eq!(
        again.unwrap_err().code(),
        Some(reno_proto::ErrorCode::Conflict)
    );
}

#[tokio::test(start_paused = true)]
async fn run_events_resumes_across_gaps_and_stays_ordered() {
    let daemon = FakeDaemon::start(FakeConfig::default()).unwrap();
    let client = daemon.connect().await.unwrap();
    let id = client.run_task(task("/burst 700")).await.unwrap();
    // Let the script finish first, so the whole journal is replayed in one burst.
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        client.get_run(&id).await.unwrap().status,
        RunStatus::Succeeded
    );

    let mut stream = Box::pin(client.run_events(id.clone(), None));
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(first.seq, Seq(1));
    // A slow consumer: let the connection's queues overflow before reading on.
    for _ in 0..1_000 {
        tokio::task::yield_now().await;
    }
    let mut seen = vec![first.seq.0];
    while let Some(item) = stream.next().await {
        seen.push(item.unwrap().seq.0);
    }
    assert!(seen.len() > 700);
    assert!(
        seen.iter().enumerate().all(|(i, s)| *s == i as u64 + 1),
        "ordered, no duplicates, no holes"
    );
    assert!(
        daemon.run_events_subscriptions() > 1,
        "the stream must have resumed at least once, saw {} subscription(s)",
        daemon.run_events_subscriptions()
    );
    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn run_events_resumes_across_server_side_gaps() {
    // A tiny server queue and a tiny pipe make the daemon itself cut the subscription off.
    let config = FakeConfig {
        server: reno_rpc::ServerConfig {
            sub_queue_capacity: 2,
            ..reno_rpc::ServerConfig::default()
        },
        pipe_buffer: 256,
        ..FakeConfig::default()
    };
    let daemon = FakeDaemon::start(config).unwrap();
    let client = daemon.connect().await.unwrap();
    let id = client.run_task(task("/burst 300")).await.unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;

    let transcript = events(&client, &id).await;
    assert!(transcript.len() > 300);
    assert_consecutive(&transcript);
    assert!(is_done(
        &transcript.last().unwrap().1,
        StopReason::Completed
    ));
    assert!(
        daemon.stats().gaps() > 0,
        "the daemon itself must have sent sub.gap"
    );
    assert!(daemon.stats().max_queue_len() <= 2);
    daemon.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_protocol_version_mismatch_is_incompatible() {
    let daemon = FakeDaemon::start(FakeConfig {
        protocol_version: PROTOCOL_VERSION + 1,
        ..FakeConfig::default()
    })
    .unwrap();
    let err = daemon.connect().await.unwrap_err();
    assert!(
        matches!(
            err,
            ClientError::Incompatible {
                client: PROTOCOL_VERSION,
                server: Some(v)
            } if v == PROTOCOL_VERSION + 1
        ),
        "{err:?}"
    );
    daemon.shutdown().await;
    assert!(daemon.is_idle());
}

#[tokio::test(start_paused = true)]
async fn shutdown_leaves_no_tasks_running() {
    let daemon = FakeDaemon::start(FakeConfig::default()).unwrap();
    let client = daemon.connect().await.unwrap();
    // Start work that is still in flight at shutdown: a script, a watch and a stream.
    let _id = client.run_task(task("something slow")).await.unwrap();
    let _runs = client.watch_runs().await.unwrap();
    // builder1 is running and never finishes: this stream stays open.
    let mut live = Box::pin(client.run_events(run("builder1"), None));
    assert!(live.next().await.unwrap().is_ok());
    assert_eq!(daemon.run_events_subscriptions(), 1);
    assert!(!daemon.is_idle());

    daemon.shutdown().await;
    assert!(daemon.is_idle());
    assert!(matches!(
        client.status().await,
        Err(ClientError::Disconnected)
    ));
}

#[tokio::test(start_paused = true)]
async fn dropping_the_client_stops_the_fake() {
    let daemon = FakeDaemon::start(FakeConfig::default()).unwrap();
    let client = daemon.connect().await.unwrap();
    client.run_task(task("work")).await.unwrap();
    drop(client);
    // The server notices the closed connection and stops its tasks.
    tokio::time::sleep(Duration::from_secs(5)).await;
    daemon.shutdown().await;
    assert!(daemon.is_idle());
}

/// Applies every update that arrives until the channel stays quiet for a (paused) second.
async fn drain(rx: &mut Updates, state: &mut State) {
    while let Ok(Some(update)) = tokio::time::timeout(Duration::from_secs(1), rx.next()).await {
        state.apply(update);
    }
}

#[tokio::test(start_paused = true)]
async fn the_mirror_converges_on_the_daemons_state() {
    let client = FakeDaemon::spawn().await.unwrap();
    let (mirror, mut rx) = Mirror::start(client, MirrorConfig::default());
    let mut state = State::new();
    assert_eq!(state.connection(), ConnectionStatus::Connecting);
    drain(&mut rx, &mut state).await;

    assert_eq!(state.connection(), ConnectionStatus::Connected);
    let daemon = state.daemon().unwrap();
    assert_eq!(daemon.hello.protocol_version, PROTOCOL_VERSION);
    assert!(daemon.status.is_some());
    assert_eq!(state.runs().len(), 6);
    assert_eq!(state.approvals().len(), 2);

    // Follow a held run, then answer its approval.
    assert!(mirror.follow_run(run("keeper1"), None));
    assert!(!mirror.follow_run(run("keeper1"), None), "already followed");
    drain(&mut rx, &mut state).await;
    let held = state.transcript(&run("keeper1")).unwrap();
    assert!(matches!(
        held.events.last().unwrap().1,
        ExecEvent::NeedsApproval { .. }
    ));

    mirror
        .client()
        .decide(&appr("keeperpush"), ApprovalDecision::ApproveOnce)
        .await
        .unwrap();
    drain(&mut rx, &mut state).await;
    assert_eq!(state.approvals().len(), 1);
    assert_eq!(
        state.run(&run("keeper1")).unwrap().status,
        RunStatus::Succeeded
    );
    let transcript = state.transcript(&run("keeper1")).unwrap();
    assert!(is_done(
        &transcript.events.last().unwrap().1,
        StopReason::Completed
    ));
    assert_consecutive(&transcript.events);
    // The followed stream ended with the run, so it can be followed again later.
    assert!(mirror.follow_run(run("keeper1"), Some(transcript.resume_from())));

    mirror.shutdown().await;
    assert!(mirror.is_idle());
}

#[tokio::test(start_paused = true)]
async fn a_full_update_channel_collapses_lists_and_loses_nothing() {
    let client = FakeDaemon::spawn().await.unwrap();
    // One slot: the consumer cannot keep up with the stream of changes.
    let (mirror, mut rx) = Mirror::start(client.clone(), MirrorConfig { update_capacity: 1 });
    let ids = [
        client.run_task(task("one")).await.unwrap(),
        client.run_task(task("two")).await.unwrap(),
        client.run_task(task("three")).await.unwrap(),
    ];
    for id in &ids {
        assert!(mirror.follow_run(id.clone(), None));
    }
    // Do not read at all while the runs play out.
    tokio::time::sleep(Duration::from_secs(10)).await;

    let mut state = State::new();
    drain(&mut rx, &mut state).await;
    assert_eq!(state.runs().len(), 9);
    for id in &ids {
        assert_eq!(state.run(id).unwrap().status, RunStatus::Succeeded);
        let transcript = state.transcript(id).unwrap();
        assert_consecutive(&transcript.events);
        assert!(is_done(
            &transcript.events.last().unwrap().1,
            StopReason::Completed
        ));
    }
    mirror.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_lost_connection_surfaces_as_disconnected() {
    let daemon = FakeDaemon::start(FakeConfig::default()).unwrap();
    let client = daemon.connect().await.unwrap();
    let (mirror, mut rx) = Mirror::start(client, MirrorConfig::default());
    let mut state = State::new();
    drain(&mut rx, &mut state).await;
    assert_eq!(state.connection(), ConnectionStatus::Connected);

    daemon.shutdown().await;
    drain(&mut rx, &mut state).await;
    assert_eq!(state.connection(), ConnectionStatus::Disconnected);
    mirror.shutdown().await;
    assert!(mirror.is_idle());
}

#[tokio::test(start_paused = true)]
async fn dropping_the_mirror_stops_its_tasks() {
    let client = FakeDaemon::spawn().await.unwrap();
    let (mirror, mut rx) = Mirror::start(client, MirrorConfig::default());
    let mut state = State::new();
    drain(&mut rx, &mut state).await;
    drop(mirror);
    // The sender side is gone once the tasks stop, so the channel closes.
    assert!(
        tokio::time::timeout(Duration::from_secs(5), rx.next())
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test(start_paused = true)]
async fn steering_a_live_run_journals_the_message_and_a_reply() {
    let client = FakeDaemon::spawn().await.unwrap();
    assert!(client.supports("run.steer"));
    let id = client.run_task(task("hello")).await.unwrap();
    client
        .steer(&id, "use fewer words".to_owned())
        .await
        .unwrap();
    let all = events(&client, &id).await;
    assert_consecutive(&all);
    let steer_at = all
        .iter()
        .position(
            |(_, e)| matches!(e, ExecEvent::UserMessage { text } if text == "use fewer words"),
        )
        .expect("the steer is journalled");
    assert!(
        all[steer_at + 1..]
            .iter()
            .any(|(_, e)| matches!(e, ExecEvent::Text { text } if text.starts_with("Understood"))),
        "a scripted reply follows the steer"
    );
    assert!(is_done(&all.last().unwrap().1, StopReason::Completed));
    // A finished run cannot be steered.
    assert!(client.steer(&id, "late".to_owned()).await.is_err());
    assert!(client.steer(&run("nosuch"), "x".to_owned()).await.is_err());
}
