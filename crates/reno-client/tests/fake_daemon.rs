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
        model_options: Default::default(),
        effort: None,
        prompt: prompt.to_owned(),
        executor: Some("claude-code".parse().unwrap()),
        bot: None,
        cwd: None,
        model: None,
        in_place: false,
        attachments: Vec::new(),
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
            ..RunListParams::default()
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
        .unwrap()
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
    let (mirror, mut rx) = Mirror::start(
        client.clone(),
        MirrorConfig {
            update_capacity: 1,
            ..MirrorConfig::default()
        },
    );
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

#[tokio::test(start_paused = true)]
async fn a_daemon_without_definitions_is_not_asked_for_bots_or_definitions() {
    let client = FakeDaemon::spawn_with(FakeConfig {
        without_features: vec![reno_proto::FEATURE_DEFS.to_owned()],
        ..FakeConfig::default()
    })
    .await
    .unwrap();
    // The fake does not claim the `defs` feature, so the client refuses before sending.
    assert!(!client.supports(reno_proto::FEATURE_DEFS));
    let mut as_bot = task("go");
    as_bot.bot = Some("keeper".to_owned());
    let error = client.run_task(as_bot).await.unwrap_err();
    assert!(
        matches!(&error, ClientError::Protocol(m) if m.contains("feature `defs`")),
        "{error:?}"
    );
    // Leaving the executor to the router is the `routing` feature, which this daemon has.
    client
        .run_task(TaskRunParams {
            executor: None,
            ..task("go")
        })
        .await
        .unwrap();
    assert!(matches!(
        client.list_defs(None).await.unwrap_err(),
        ClientError::Protocol(_)
    ));
    // A run on a named executor still works.
    client.run_task(task("go")).await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn executors_are_listed_refreshed_and_watched() {
    use reno_proto::{ExecutorId, ModelSource, SignedIn};
    let daemon = FakeDaemon::start(FakeConfig::default()).unwrap();
    let client = daemon.connect().await.unwrap();
    assert!(client.supports(reno_proto::FEATURE_EXECUTORS));

    let listed = client.list_executors().await.unwrap();
    let ids: Vec<&str> = listed.iter().map(|e| e.descriptor.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "claude-code",
            "codex",
            "pi",
            "cursor",
            "acp.grok",
            "acp.hermes",
            "acp.antigravity",
            "acp.devin",
            "acp.fx",
            "mock"
        ]
    );
    let codex = listed
        .iter()
        .find(|e| e.descriptor.id.as_str() == "codex")
        .unwrap();
    let health = codex.health.as_ref().unwrap();
    assert!(matches!(health.signed_in, SignedIn::No { .. }));
    assert_eq!(health.models.source, ModelSource::Live);
    assert!(health.ineligible().is_some());

    // The watch starts with the list as it stands, then follows a refresh.
    let mut watch = client.watch_executors().await.unwrap();
    assert_eq!(watch.next().await.unwrap().unwrap().executors, listed);
    let one: ExecutorId = "codex".parse().unwrap();
    let refreshed = client.refresh_executors(Some(one)).await.unwrap();
    let stamp = |list: &[reno_proto::ExecutorInfo], id: &str| {
        list.iter()
            .find(|e| e.descriptor.id.as_str() == id)
            .and_then(|e| e.health.as_ref())
            .map(|h| h.checked_at_ms)
    };
    assert!(
        stamp(&refreshed, "codex") > stamp(&listed, "codex"),
        "re-probed"
    );
    assert_eq!(
        stamp(&refreshed, "mock"),
        stamp(&listed, "mock"),
        "only that one"
    );
    assert_eq!(watch.next().await.unwrap().unwrap().executors, refreshed);

    // An id the daemon does not have is not found, and failures can be injected.
    let error = client
        .refresh_executors(Some("nowhere".parse().unwrap()))
        .await
        .unwrap_err();
    assert_eq!(error.code(), Some(reno_proto::ErrorCode::NotFound));
    daemon.set_executors(vec![]);
    assert!(client.list_executors().await.unwrap().is_empty());
}

#[tokio::test(start_paused = true)]
async fn an_executor_that_needs_its_adapter_is_installed_on_request_and_only_then() {
    use reno_proto::{ErrorCode, InstallStatus, VersionGate};
    let daemon = FakeDaemon::start(FakeConfig::default()).unwrap();
    let client = daemon.connect().await.unwrap();
    assert!(client.supports(reno_proto::FEATURE_EXECUTOR_INSTALL));
    let hermes: reno_proto::ExecutorId = "acp.hermes".parse().unwrap();

    let installed = client.install_executor(&hermes).await.unwrap();
    let health = installed.health.unwrap();
    assert!(matches!(health.installed, InstallStatus::Installed { .. }));
    assert_eq!(health.version_gate, VersionGate::Ok);
    assert!(health.ineligible().is_none());

    // Installed already: nothing to install. Unknown id: not found.
    let again = client.install_executor(&hermes).await.unwrap_err();
    assert_eq!(again.code(), Some(ErrorCode::Conflict));
    let nowhere = client
        .install_executor(&"nowhere".parse().unwrap())
        .await
        .unwrap_err();
    assert_eq!(nowhere.code(), Some(ErrorCode::NotFound));

    // A failure can be injected, and a daemon without the feature is refused up front.
    daemon.fail(
        "executor.install",
        reno_proto::ErrorObject::new(ErrorCode::Unavailable, "registry unreachable"),
    );
    let failed = client.install_executor(&hermes).await.unwrap_err();
    assert_eq!(failed.code(), Some(ErrorCode::Unavailable));
}

#[tokio::test(start_paused = true)]
async fn a_denial_carries_its_reason_into_the_history() {
    use reno_proto::{ApprovalDecision, ApprovalStatus};
    let daemon = FakeDaemon::start(FakeConfig::default()).unwrap();
    let client = daemon.connect().await.unwrap();
    assert!(client.supports(reno_proto::FEATURE_APPROVAL_MESSAGE));
    let pending = client.list_approvals().await.unwrap();
    let first = pending.first().expect("a pending approval").id.clone();
    client
        .decide_with_message(
            &first,
            ApprovalDecision::Deny,
            Some("  use the staging bucket ".into()),
        )
        .await
        .unwrap();
    let history = client.approval_history(None, None).await.unwrap();
    let record = history.iter().find(|r| r.id == first).unwrap();
    assert_eq!(record.status, ApprovalStatus::Denied);
    assert_eq!(record.message.as_deref(), Some("use the staging bucket"));
}

#[tokio::test(start_paused = true)]
async fn the_fake_previews_routes_and_keeps_each_runs_decision() {
    use reno_proto::RuleMatched;
    let client = FakeDaemon::spawn().await.unwrap();

    // Naming an executor is the explicit rule; naming none routes by default, and nothing is
    // created by a preview.
    let named = client.route_preview(task("go")).await.unwrap();
    assert_eq!(named.rule_matched, RuleMatched::ExplicitExecutor);
    assert_eq!(named.chosen, Some("claude-code".parse().unwrap()));
    let before = client
        .list_runs(RunListParams::default())
        .await
        .unwrap()
        .len();
    let mut unnamed_task = task("go");
    unnamed_task.executor = None;
    let unnamed = client.route_preview(unnamed_task.clone()).await.unwrap();
    assert_eq!(unnamed.rule_matched, RuleMatched::Default);
    assert!(unnamed.chosen.is_some());
    let after = client
        .list_runs(RunListParams::default())
        .await
        .unwrap()
        .len();
    assert_eq!(after, before);

    // A submitted task answers with its decision, and `run.routing` returns the same one.
    let submitted = client.submit_task(unnamed_task).await.unwrap();
    let stored = client.run_routing(&submitted.run_id).await.unwrap();
    assert_eq!(stored, submitted.routing);
    let missing = client.run_routing(&run("nowhere")).await.unwrap_err();
    assert_eq!(missing.code(), Some(reno_proto::ErrorCode::NotFound));
}

#[tokio::test(start_paused = true)]
async fn the_demo_router_follows_a_bots_order_and_reports_the_signed_out_one() {
    use reno_proto::{RejectReason, RuleMatched};
    let daemon = FakeDaemon::start(FakeConfig::default()).unwrap();
    let client = daemon.connect().await.unwrap();
    let mut reviewer = task("review it");
    reviewer.executor = None;
    reviewer.bot = Some("reviewer".to_owned());
    let preview = client.route_preview(reviewer.clone()).await.unwrap();
    // The reviewer prefers Codex, which is signed out in the demo.
    assert_eq!(
        preview.rule_matched,
        RuleMatched::BotPreference {
            bot: "reviewer".into()
        }
    );
    assert_eq!(preview.chosen, Some("claude-code".parse().unwrap()));
    assert_eq!(preview.rejected.len(), 1);
    assert_eq!(preview.rejected[0].executor.as_str(), "codex");
    assert!(matches!(
        preview.rejected[0].reason,
        RejectReason::SignedOut { .. }
    ));
    assert_eq!(daemon.route_previews(), [reviewer]);

    let mut stranger = task("x");
    stranger.executor = None;
    stranger.bot = Some("nobody".to_owned());
    assert!(client.run_task(stranger).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn attachments_are_listed_in_the_prompt_the_fake_stores() {
    let client = FakeDaemon::spawn().await.unwrap();
    let mut with_files = task("look at this");
    with_files.attachments = vec![reno_proto::Attachment {
        path: "/tmp/shot.png".into(),
        name: "shot.png".into(),
        mime: Some("image/png".into()),
    }];
    let id = client.run_task(with_files.clone()).await.unwrap();
    let prompt = client.get_run(&id).await.unwrap().prompt.unwrap();
    assert_eq!(
        prompt,
        "look at this\n\nAttached files:\n- /tmp/shot.png (shot.png, image/png)\n"
    );
    with_files.attachments[0].path = "relative.png".into();
    assert!(client.run_task(with_files).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn the_fake_pauses_and_resumes_and_shows_it_in_the_budget() {
    let client = FakeDaemon::spawn().await.unwrap();
    let status = client.budget_status().await.unwrap();
    assert!(!status.paused);
    assert!(status.bots.iter().any(|b| b.bot == "keeper" && !b.paused));

    let global = client.pause(None, false).await.unwrap();
    assert!(global.changed && global.stopped.is_empty());
    assert!(!client.pause(None, false).await.unwrap().changed);
    assert!(client.budget_status().await.unwrap().paused);

    assert!(client.pause(Some("keeper"), false).await.unwrap().changed);
    let status = client.budget_status().await.unwrap();
    assert!(
        status
            .bots
            .iter()
            .find(|b| b.bot == "keeper")
            .unwrap()
            .paused
    );
    assert!(
        !status
            .bots
            .iter()
            .find(|b| b.bot == "scout")
            .unwrap()
            .paused
    );

    assert!(client.resume(None).await.unwrap().changed);
    assert!(!client.resume(None).await.unwrap().changed);
    assert!(client.resume(Some("keeper")).await.unwrap().changed);
    let status = client.budget_status().await.unwrap();
    assert!(!status.paused && status.bots.iter().all(|b| !b.paused));

    let unknown = client.pause(Some("nobody"), false).await.unwrap_err();
    assert_eq!(unknown.code(), Some(reno_proto::ErrorCode::NotFound));
}

#[tokio::test(start_paused = true)]
async fn the_fake_lists_sets_and_deletes_secrets_and_never_returns_a_value() {
    let client = FakeDaemon::spawn().await.unwrap();
    // A name a bot asks for is listed before anyone sets it.
    let listed = client.list_secrets().await.unwrap().secrets;
    let github = listed.iter().find(|s| s.name == "GITHUB_TOKEN").unwrap();
    assert!(!github.set);
    assert_eq!(github.used_by, ["builder", "keeper"]);

    assert!(
        !client
            .set_secret("GITHUB_TOKEN", "ghp_example_value_1")
            .await
            .unwrap()
    );
    assert!(
        client
            .set_secret("GITHUB_TOKEN", "ghp_example_value_2")
            .await
            .unwrap()
    );
    assert!(
        client
            .set_secret("NPM_TOKEN", "npm_example_value")
            .await
            .is_ok()
    );
    let listed = client.list_secrets().await.unwrap();
    assert!(!format!("{listed:?}").contains("example_value"));
    assert_eq!(
        listed
            .secrets
            .iter()
            .map(|s| (s.name.as_str(), s.set))
            .collect::<Vec<_>>(),
        [("GITHUB_TOKEN", true), ("NPM_TOKEN", true)]
    );

    assert!(client.delete_secret("NPM_TOKEN").await.unwrap());
    assert!(!client.delete_secret("NPM_TOKEN").await.unwrap());
    let bad = client.set_secret("lower", "x").await.unwrap_err();
    assert_eq!(bad.code(), Some(reno_proto::ErrorCode::InvalidParams));
}

#[tokio::test(start_paused = true)]
async fn a_refused_run_and_a_cancelled_one_carry_their_typed_stop() {
    use reno_proto::{CancelledBy, StartRefusal, StopDetail};
    let client = FakeDaemon::spawn().await.unwrap();
    assert!(client.supports(reno_proto::FEATURE_STOP_DETAIL));
    let refused = client.run_task(task("/refused")).await.unwrap();
    let transcript = events(&client, &refused).await;
    let Some((
        _,
        ExecEvent::Done {
            reason: Some(reason),
            ..
        },
    )) = transcript.last()
    else {
        panic!("no typed reason: {transcript:?}");
    };
    assert!(matches!(
        reason,
        StopDetail::StartRefused {
            failure: StartRefusal::NotSignedIn,
            ..
        }
    ));
    assert_eq!(
        client.get_run(&refused).await.unwrap().stop.as_ref(),
        Some(reason)
    );

    client
        .cancel_run(&run("builder1"), Some("not needed".into()))
        .await
        .unwrap();
    assert_eq!(
        client.get_run(&run("builder1")).await.unwrap().stop,
        Some(StopDetail::Cancelled {
            by: CancelledBy::User
        })
    );
}

#[tokio::test(start_paused = true)]
async fn a_failed_run_with_a_session_resumes_and_other_runs_are_refused_with_a_reason() {
    use reno_proto::{ErrorCode, NotResumable, NotResumed};
    let client = FakeDaemon::spawn().await.unwrap();
    assert!(client.supports(reno_proto::FEATURE_RUN_RESUME));

    let failing = client.run_task(task("/fails")).await.unwrap();
    events(&client, &failing).await;
    let failed = client.get_run(&failing).await.unwrap();
    assert_eq!(failed.status, RunStatus::Failed);
    assert_eq!(
        failed.session.as_ref().map(|s| s.0.as_str()),
        Some("sess-fake")
    );

    assert_eq!(client.resume_run(&failing, None).await.unwrap(), 2);
    let transcript = events(&client, &failing).await;
    assert!(transcript.iter().any(|(_, e)| matches!(
        e,
        ExecEvent::Resumed {
            from_attempt: 1,
            ..
        }
    )));
    assert!(is_done(
        &transcript.last().unwrap().1,
        StopReason::Completed
    ));
    let resumed = client.get_run(&failing).await.unwrap();
    assert_eq!((resumed.status, resumed.attempt), (RunStatus::Succeeded, 2));

    // A run that is done, and a run that never had a session, say why they cannot.
    let why = |error: ClientError| match error {
        ClientError::Remote(object) => {
            assert_eq!(object.code, ErrorCode::Conflict);
            serde_json::from_value::<NotResumable>(object.data.unwrap())
                .unwrap()
                .why
        }
        other => panic!("{other:?}"),
    };
    assert_eq!(
        why(client.resume_run(&failing, None).await.unwrap_err()),
        NotResumed::WrongStatus {
            status: RunStatus::Succeeded
        }
    );
    let refused = client.run_task(task("/refused")).await.unwrap();
    events(&client, &refused).await;
    assert_eq!(
        why(client.resume_run(&refused, None).await.unwrap_err()),
        NotResumed::NoSession
    );
}

#[tokio::test(start_paused = true)]
async fn the_fake_keeps_a_remembered_answer_as_a_rule_that_can_be_listed_and_revoked() {
    use reno_proto::{Remember, RememberScope, StandingEffect, StandingScope};
    let client = FakeDaemon::spawn().await.unwrap();
    assert!(client.supports(reno_proto::FEATURE_RULES));
    let demo = client.list_rules(false).await.unwrap();
    assert!(demo.len() >= 3, "the demo world has rules to show");

    // keeper's push is held; allow it for the bot.
    client
        .decide_remembering(
            &appr("keeperpush"),
            ApprovalDecision::ApproveOnce,
            None,
            Some(Remember {
                scope: RememberScope::Bot,
                expires_in_secs: Some(3600),
            }),
        )
        .await
        .unwrap();
    let rules = client.list_rules(false).await.unwrap();
    assert_eq!(rules.len(), demo.len() + 1);
    let new = &rules[0];
    assert_eq!(new.effect, StandingEffect::Allow);
    assert_eq!(
        new.scope,
        StandingScope::Bot {
            bot: "keeper".into()
        }
    );
    assert_eq!(new.source_approval, Some(appr("keeperpush")));
    assert!(new.expires_at_ms.is_some());
    let record = &client.approval_history(None, None).await.unwrap()[0];
    assert_eq!(
        record.subject_digest.as_deref(),
        Some(record.subject.digest().as_str())
    );

    client.revoke_rule(&new.id).await.unwrap();
    client.revoke_rule(&new.id).await.unwrap();
    assert_eq!(client.list_rules(false).await.unwrap().len(), demo.len());
    let all = client.list_rules(true).await.unwrap();
    assert!(all[0].revoked_at_ms.is_some());
    let unknown = reno_proto::RuleId::from_suffix("nope").unwrap();
    let missing = client.revoke_rule(&unknown).await.unwrap_err();
    assert_eq!(missing.code(), Some(reno_proto::ErrorCode::NotFound));

    // An expiry outside the range is refused, and the approval stays held.
    let bad = client
        .decide_remembering(
            &appr("scoutfetch"),
            ApprovalDecision::Deny,
            None,
            Some(Remember {
                scope: RememberScope::Always,
                expires_in_secs: Some(0),
            }),
        )
        .await
        .unwrap_err();
    assert_eq!(bad.code(), Some(reno_proto::ErrorCode::InvalidParams));
    assert_eq!(client.list_approvals().await.unwrap().len(), 1);
}

#[tokio::test]
async fn the_fake_daemon_carries_plan_usage_for_claude_and_codex() {
    use reno_proto::{ExecutorId, UsageLevel};
    let daemon = FakeDaemon::start(FakeConfig::default()).unwrap();
    let client = daemon.connect().await.unwrap();
    assert!(client.supports(reno_proto::FEATURE_EXECUTOR_USAGE));

    let listed = client.list_executors().await.unwrap();
    let usage_of = |list: &[reno_proto::ExecutorInfo], id: &str| {
        list.iter()
            .find(|e| e.descriptor.id.as_str() == id)
            .and_then(|e| e.health.clone())
            .and_then(|h| h.plan_usage.map(|u| (u, h.checked_at_ms)))
    };
    let (claude, checked) = usage_of(&listed, "claude-code").expect("claude usage");
    let windows: Vec<_> = claude
        .windows
        .iter()
        .map(|w| (w.label.as_str(), w.used_permille))
        .collect();
    assert_eq!(windows, [("Session", 540), ("Week", 210)]);
    assert_eq!(claude.level(), Some(UsageLevel::Normal));
    let (codex, _) = usage_of(&listed, "codex").expect("codex usage");
    assert_eq!(codex.windows[0].used_permille, 120);
    assert!(usage_of(&listed, "mock").is_none());

    // A usage-only refresh stamps the meters, not the probe.
    let refreshed = client
        .refresh_executor_usage(Some("claude-code".parse::<ExecutorId>().unwrap()))
        .await
        .unwrap();
    let (after, checked_after) = usage_of(&refreshed, "claude-code").unwrap();
    assert!(after.fetched_at_ms > claude.fetched_at_ms);
    assert_eq!(checked_after, checked);
}

#[tokio::test(start_paused = true)]
async fn a_live_fake_run_reports_its_context_and_takes_the_window_choice() {
    let client = FakeDaemon::spawn().await.unwrap();
    client.hello().await.unwrap();
    let health = client.list_executors().await.unwrap();
    let claude = health
        .iter()
        .find(|e| e.descriptor.id.as_str() == "claude-code")
        .unwrap();
    let models = &claude.descriptor.caps.models;
    let default_of = |id: &str| {
        let model = models.iter().find(|m| m.id == id).unwrap();
        model.options[0].default_choice.clone()
    };
    assert_eq!(default_of("claude-opus-5-5"), "1m");
    assert_eq!(default_of("claude-sonnet-5-5"), "200k");
    assert!(models.iter().any(|m| m.options.is_empty()));

    let mut params = task("tidy the readme");
    params.model = Some("claude-opus-5-5".into());
    params.model_options = [("contextWindow".to_owned(), "200k".to_owned())].into();
    let id = client.run_task(params).await.unwrap();
    assert_eq!(
        client.get_run(&id).await.unwrap().model_options["contextWindow"],
        "200k"
    );
    let transcript = events(&client, &id).await;
    let events: Vec<ExecEvent> = transcript.into_iter().map(|(_, e)| e).collect();
    let context = reno_proto::ContextUsage::latest(&events);
    assert_eq!(
        (context.tokens, context.window),
        (Some(84_213), Some(200_000))
    );

    // A choice the model does not list is refused.
    let mut bad = task("x");
    bad.model = Some("claude-haiku-5-5".into());
    bad.model_options = [("contextWindow".to_owned(), "1m".to_owned())].into();
    assert!(client.run_task(bad).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn a_live_fake_run_takes_run_configure_like_the_daemon() {
    use reno_proto::{ConfigTiming, Effort, EffortChange};
    let client = FakeDaemon::spawn().await.unwrap();
    client.hello().await.unwrap();
    let mut params = task("/long keep going");
    params.model = Some("claude-opus-5-5".into());
    params.effort = Some(Effort::High);
    let id = client.run_task(params).await.unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    let result = client
        .configure_run(
            &id,
            Some(EffortChange::Set {
                effort: Effort::Low,
            }),
            [("contextWindow".to_owned(), "200k".to_owned())].into(),
        )
        .await
        .unwrap();
    assert_eq!(result.effort, Some(Effort::Low));
    assert_eq!(result.effort_applies, Some(ConfigTiming::Now));
    assert_eq!(result.options_apply, Some(ConfigTiming::Now));
    let summary = client.get_run(&id).await.unwrap();
    assert_eq!(summary.effort, Some(Effort::Low));
    assert_eq!(summary.model_options["contextWindow"], "200k");
    let cleared = client
        .configure_run(&id, Some(EffortChange::Clear), Default::default())
        .await
        .unwrap();
    assert_eq!(cleared.effort, None);
    // A choice the model does not list, and a run that is not there, are refused.
    assert!(
        client
            .configure_run(
                &id,
                None,
                [("contextWindow".to_owned(), "9m".to_owned())].into()
            )
            .await
            .is_err()
    );
    client.steer(&id, "wrap up".to_owned()).await.unwrap();
    let transcript = events(&client, &id).await;
    let changes = transcript
        .iter()
        .filter(|(_, e)| matches!(e, ExecEvent::ConfigChanged { .. }))
        .count();
    assert_eq!(changes, 2);
}

#[tokio::test(start_paused = true)]
async fn the_fake_resolves_an_alias_and_a_1m_spelling_like_the_daemon() {
    use reno_proto::{Effort, EffortChange};
    let client = FakeDaemon::spawn().await.unwrap();
    client.hello().await.unwrap();
    // The model a seeded run reports (`opus-5.5`) is the listed Opus 5.5.
    let mut params = task("/long keep going");
    params.model = Some("opus-5.5".into());
    params.model_options = [("contextWindow".to_owned(), "1m".to_owned())].into();
    let id = client.run_task(params).await.unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    let result = client
        .configure_run(
            &id,
            Some(EffortChange::Set {
                effort: Effort::Low,
            }),
            [("contextWindow".to_owned(), "200k".to_owned())].into(),
        )
        .await
        .unwrap();
    assert_eq!(result.effort, Some(Effort::Low));
    let mut hinted = task("/long again");
    hinted.model = Some("claude-opus-5-5[1m]".into());
    assert!(client.run_task(hinted).await.is_ok());
}
