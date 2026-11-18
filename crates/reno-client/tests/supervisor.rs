//! The supervised mirror (reconnect, backoff, followed runs), the new feeds, and the fake's
//! triggers, definitions and history. All on paused tokio time.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use futures::StreamExt;
use reno_client::fake::{FakeConfig, FakeDaemon};
use reno_client::{
    Backoff, Client, ClientError, ConnectionStatus, Connector, Feed, FeedState, Mirror,
    MirrorConfig, OfflineKind, RunPage, State, Updates,
};
use reno_proto::defs::{DefError, DefKind, DefsSnapshot};
use reno_proto::{
    ApprovalDecision, ApprovalStatus, ErrorCode, ErrorObject, ExecEvent, FiringOutcome, RunId,
    RunListParams, RunStatus, TaskRunParams, TriggerStatus,
};

fn run(name: &str) -> RunId {
    RunId::from_suffix(name).unwrap()
}

fn task(prompt: &str) -> TaskRunParams {
    TaskRunParams {
        thread_id: None,
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

/// Applies every update that arrives until the channel stays quiet for a (paused) second.
async fn drain(rx: &mut Updates, state: &mut State) {
    while let Ok(Some(update)) = tokio::time::timeout(Duration::from_secs(1), rx.next()).await {
        state.apply(update);
    }
}

fn connector(daemon: &Arc<FakeDaemon>) -> Connector {
    let daemon = Arc::clone(daemon);
    Arc::new(move || {
        let daemon = Arc::clone(&daemon);
        Box::pin(async move { daemon.connect().await })
    })
}

fn config() -> MirrorConfig {
    MirrorConfig {
        backoff: Backoff {
            initial: Duration::from_millis(100),
            max: Duration::from_secs(2),
        },
        ..MirrorConfig::default()
    }
}

#[tokio::test(start_paused = true)]
async fn the_mirror_loads_every_feed() {
    let daemon = Arc::new(FakeDaemon::start(FakeConfig::default()).unwrap());
    let (mirror, mut rx) = Mirror::supervise(connector(&daemon), config());
    let mut state = State::new();
    assert_eq!(state.feed(Feed::Triggers), FeedState::Loading);
    drain(&mut rx, &mut state).await;
    assert_eq!(state.connection(), ConnectionStatus::Connected);
    assert!(state.offline().is_none());
    assert_eq!(state.triggers().len(), 4);
    assert_eq!(state.executors().len(), 12);
    assert_eq!(state.defs().defs.len(), 10);
    for feed in [
        Feed::Runs,
        Feed::Approvals,
        Feed::Triggers,
        Feed::Defs,
        Feed::Executors,
    ] {
        assert_eq!(state.feed(feed), FeedState::Live, "{feed:?}");
    }
    // The history is fetched on demand, never by the mirror.
    assert_eq!(state.feed(Feed::ApprovalHistory), FeedState::Loading);
    assert!(state.daemon().unwrap().status.is_some());
    mirror.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_missing_feature_is_unavailable_not_loading() {
    let daemon = Arc::new(
        FakeDaemon::start(FakeConfig {
            without_features: vec![
                reno_proto::FEATURE_DEFS.to_owned(),
                reno_proto::FEATURE_TRIGGERS.to_owned(),
                reno_proto::FEATURE_EXECUTORS.to_owned(),
            ],
            ..FakeConfig::default()
        })
        .unwrap(),
    );
    let (mirror, mut rx) = Mirror::supervise(connector(&daemon), config());
    let mut state = State::new();
    drain(&mut rx, &mut state).await;
    assert_eq!(state.feed(Feed::Defs), FeedState::Unavailable);
    assert_eq!(state.feed(Feed::Triggers), FeedState::Unavailable);
    assert_eq!(state.feed(Feed::Executors), FeedState::Unavailable);
    assert_eq!(state.feed(Feed::Runs), FeedState::Live);
    mirror.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn live_updates_of_triggers_and_definitions_arrive() {
    let daemon = Arc::new(FakeDaemon::start(FakeConfig::default()).unwrap());
    let (mirror, mut rx) = Mirror::supervise(connector(&daemon), config());
    let mut state = State::new();
    drain(&mut rx, &mut state).await;

    let mut triggers = state.triggers().to_vec();
    triggers[0].status = TriggerStatus::Disabled;
    triggers[0].next_due_at_ms = None;
    daemon.set_triggers(triggers);
    daemon.set_defs(DefsSnapshot {
        defs: state.defs().defs.clone(),
        errors: vec![DefError::new(
            "bots/keeper.toml",
            "executors[0]",
            "unknown executor",
        )],
    });
    drain(&mut rx, &mut state).await;
    assert_eq!(state.triggers()[0].status, TriggerStatus::Disabled);
    assert_eq!(state.defs().errors.len(), 1);
    mirror.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_lost_connection_is_retried_and_the_state_converges() {
    let daemon = Arc::new(FakeDaemon::start(FakeConfig::default()).unwrap());
    let (mirror, mut rx) = Mirror::supervise(connector(&daemon), config());
    let mut state = State::new();
    drain(&mut rx, &mut state).await;
    assert_eq!(state.connection(), ConnectionStatus::Connected);
    assert!(mirror.client().is_some());

    daemon.sever();
    // The loss is visible first, then the retry connects again.
    let mut saw_offline = false;
    while let Ok(Some(update)) = tokio::time::timeout(Duration::from_secs(1), rx.next()).await {
        state.apply(update);
        if state.connection() == ConnectionStatus::Disconnected {
            saw_offline = true;
            let offline = state.offline().unwrap();
            assert_eq!(offline.kind, OfflineKind::Lost);
        }
    }
    assert!(saw_offline, "the loss must be reported");
    assert_eq!(state.connection(), ConnectionStatus::Connected);
    assert!(state.offline().is_none());
    assert_eq!(state.runs().len(), 6);
    mirror.client().unwrap().status().await.unwrap();
    mirror.shutdown().await;
    assert!(mirror.is_idle());
}

#[tokio::test(start_paused = true)]
async fn failed_attempts_back_off_and_do_not_spin() {
    let attempts = Arc::new(AtomicU32::new(0));
    let connector: Connector = {
        let attempts = Arc::clone(&attempts);
        Arc::new(move || {
            attempts.fetch_add(1, Ordering::SeqCst);
            Box::pin(async {
                Err::<Client, _>(ClientError::NotRunning(std::path::PathBuf::from("/x.sock")))
            })
        })
    };
    let (mirror, mut rx) = Mirror::supervise(connector, config());
    let mut state = State::new();
    // 100ms + 200ms + 400ms + 800ms + 1600ms, then 2s each: about 5 s is 6 attempts, never more.
    let until = tokio::time::Instant::now() + Duration::from_millis(3100);
    while let Ok(Some(update)) = tokio::time::timeout_at(until, rx.next()).await {
        state.apply(update);
    }
    let n = attempts.load(Ordering::SeqCst);
    assert!((5..=6).contains(&n), "attempts: {n}");
    let offline = state.offline().unwrap();
    assert_eq!(offline.kind, OfflineKind::NotRunning);
    assert!(offline.attempt >= 5);
    assert_eq!(state.connection(), ConnectionStatus::Disconnected);
    mirror.shutdown().await;
    assert!(mirror.is_idle());
}

#[tokio::test(start_paused = true)]
async fn retry_now_cuts_the_wait_short() {
    let attempts = Arc::new(AtomicU32::new(0));
    let connector: Connector = {
        let attempts = Arc::clone(&attempts);
        Arc::new(move || {
            attempts.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Err::<Client, _>(ClientError::Disconnected) })
        })
    };
    let slow = MirrorConfig {
        backoff: Backoff {
            initial: Duration::from_secs(3600),
            max: Duration::from_secs(3600),
        },
        ..MirrorConfig::default()
    };
    let (mirror, mut rx) = Mirror::supervise(connector, slow);
    let mut state = State::new();
    drain(&mut rx, &mut state).await;
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    mirror.retry_now();
    drain(&mut rx, &mut state).await;
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    mirror.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_followed_run_resumes_after_a_reconnect() {
    let daemon = Arc::new(FakeDaemon::start(FakeConfig::default()).unwrap());
    let (mirror, mut rx) = Mirror::supervise(connector(&daemon), config());
    let mut state = State::new();
    drain(&mut rx, &mut state).await;
    let id = mirror
        .client()
        .unwrap()
        .run_task(task("/long keep going"))
        .await
        .unwrap();
    assert!(mirror.follow_run(id.clone(), None));
    drain(&mut rx, &mut state).await;
    let before = state.transcript(&id).unwrap().events.len();
    assert!(before > 3);

    daemon.sever();
    drain(&mut rx, &mut state).await;
    assert_eq!(state.connection(), ConnectionStatus::Connected);

    // A steer ends the long run; the reconnected follow delivers the rest, gap-free.
    mirror
        .client()
        .unwrap()
        .steer(&id, "wrap up".to_owned())
        .await
        .unwrap();
    drain(&mut rx, &mut state).await;
    let events = &state.transcript(&id).unwrap().events;
    assert!(events.len() > before);
    for (i, (seq, _)) in events.iter().enumerate() {
        assert_eq!(seq.0, i as u64 + 1, "no hole, no repeat");
    }
    assert!(matches!(events.last().unwrap().1, ExecEvent::Done { .. }));
    assert_eq!(state.run(&id).unwrap().status, RunStatus::Succeeded);
    mirror.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn a_run_followed_while_offline_starts_when_the_daemon_is_back() {
    let daemon = Arc::new(FakeDaemon::start(FakeConfig::default()).unwrap());
    let up = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let connector: Connector = {
        let daemon = Arc::clone(&daemon);
        let up = Arc::clone(&up);
        Arc::new(move || {
            let daemon = Arc::clone(&daemon);
            let up = Arc::clone(&up);
            Box::pin(async move {
                if up.load(Ordering::SeqCst) {
                    daemon.connect().await
                } else {
                    Err(ClientError::Disconnected)
                }
            })
        })
    };
    let (mirror, mut rx) = Mirror::supervise(connector, config());
    let mut state = State::new();
    drain(&mut rx, &mut state).await;
    assert_eq!(state.connection(), ConnectionStatus::Disconnected);
    assert!(mirror.client().is_none());
    assert!(mirror.follow_run(run("keeper1"), None));
    assert!(!mirror.follow_run(run("keeper1"), None));

    up.store(true, Ordering::SeqCst);
    mirror.retry_now();
    drain(&mut rx, &mut state).await;
    assert_eq!(state.connection(), ConnectionStatus::Connected);
    assert!(!state.transcript(&run("keeper1")).unwrap().events.is_empty());
    mirror.shutdown().await;
}

#[tokio::test(start_paused = true)]
async fn paging_adds_older_runs_with_a_cursor_per_filter() {
    let daemon = FakeDaemon::start(FakeConfig::default()).unwrap();
    let client = daemon.connect().await.unwrap();
    let mut state = State::new();
    assert_eq!(state.run_page(&[]), RunPage::NotLoaded);

    let page = client
        .list_runs_page(RunListParams {
            limit: Some(4),
            ..RunListParams::default()
        })
        .await
        .unwrap();
    let c = state.apply(reno_client::Update::RunsPage {
        filter: vec![],
        runs: page.runs,
        next: page.next.clone(),
    });
    assert!(c.runs);
    assert_eq!(state.all_runs(&[]).len(), 4);
    assert!(state.runs().is_empty(), "the watch window is separate");
    let RunPage::More(cursor) = state.run_page(&[]) else {
        panic!("more pages expected");
    };
    assert_eq!(Some(cursor.clone()), page.next);

    let page = client
        .list_runs_page(RunListParams {
            limit: Some(4),
            before: Some(cursor),
            ..RunListParams::default()
        })
        .await
        .unwrap();
    state.apply(reno_client::Update::RunsPage {
        filter: vec![],
        runs: page.runs,
        next: page.next,
    });
    assert_eq!(state.all_runs(&[]).len(), 6);
    assert_eq!(state.run_page(&[]), RunPage::Done);
    // Another filter has its own cursor.
    assert_eq!(state.run_page(&[RunStatus::Failed]), RunPage::NotLoaded);
    assert_eq!(state.all_runs(&[RunStatus::Failed]).len(), 1);
}

#[tokio::test(start_paused = true)]
async fn the_fake_fires_triggers_and_records_history() {
    let daemon = FakeDaemon::start(FakeConfig::default()).unwrap();
    let client = daemon.connect().await.unwrap();

    let before = client.list_triggers().await.unwrap();
    let nightly = before.iter().find(|t| t.name == "hotfix-check").unwrap();
    assert_eq!(nightly.status, TriggerStatus::Manual);

    let fired = client.fire_trigger("hotfix-check").await.unwrap();
    assert_eq!(fired.outcome, FiringOutcome::Fired);
    let new_run = fired.run_id.unwrap();
    // Firing again while its run is live merges into it.
    let again = client.fire_trigger("hotfix-check").await.unwrap();
    assert_eq!(again.outcome, FiringOutcome::Coalesced);
    assert_eq!(again.run_id, Some(new_run.clone()));
    let after = client.list_triggers().await.unwrap();
    let nightly = after.iter().find(|t| t.name == "hotfix-check").unwrap();
    assert_eq!(nightly.last_run_id, Some(new_run));

    let err = client.fire_trigger("nope").await.unwrap_err();
    assert_eq!(err.code(), Some(ErrorCode::NotFound));

    // Its previous run is held, so a manual fire merges; once that run ends it starts a new one,
    // and backing off clears.
    let merged = client.fire_trigger("scout-digest").await.unwrap();
    assert_eq!(merged.outcome, FiringOutcome::Coalesced);
    client
        .cancel_run(&merged.run_id.unwrap(), None)
        .await
        .unwrap();
    let fresh = client.fire_trigger("scout-digest").await.unwrap();
    assert_eq!(fresh.outcome, FiringOutcome::Fired);
    let digest = client
        .list_triggers()
        .await
        .unwrap()
        .into_iter()
        .find(|t| t.name == "scout-digest")
        .unwrap();
    assert_eq!(digest.status, TriggerStatus::Scheduled);
    assert_eq!(digest.failures, 0);

    // A decision lands in the history, newest first.
    let seeded = client.approval_history(None, None).await.unwrap();
    assert_eq!(seeded.len(), 3);
    let held = client.list_approvals().await.unwrap();
    client
        .decide(&held[0].id, ApprovalDecision::Deny)
        .await
        .unwrap();
    let history = client.approval_history(None, None).await.unwrap();
    assert_eq!(history.len(), 4);
    assert_eq!(history[0].id, held[0].id);
    assert_eq!(history[0].status, ApprovalStatus::Denied);
}

#[tokio::test(start_paused = true)]
async fn the_fake_serves_and_validates_definitions() {
    let daemon = FakeDaemon::start(FakeConfig::default()).unwrap();
    let client = daemon.connect().await.unwrap();
    let all = client.list_defs(None).await.unwrap();
    assert_eq!(all.defs.len(), 10);
    let bots = client.list_defs(Some(DefKind::Bot)).await.unwrap();
    assert_eq!(bots.defs.len(), 5);

    let keeper = client.get_def(DefKind::Bot, "keeper", None).await.unwrap();
    assert_eq!(keeper.body["access"], "supervised");
    let ok = client
        .validate_def(DefKind::Bot, "keeper", keeper.body.clone())
        .await
        .unwrap();
    assert!(ok.valid && ok.hash.is_some());
    let bad = client
        .validate_def(
            DefKind::Bot,
            "keeper",
            serde_json::json!({ "name": "other" }),
        )
        .await
        .unwrap();
    assert!(!bad.valid);
    assert_eq!(bad.errors.len(), 2);
}

#[tokio::test(start_paused = true)]
async fn injected_errors_and_an_empty_world() {
    let daemon = FakeDaemon::start(FakeConfig {
        empty: true,
        ..FakeConfig::default()
    })
    .unwrap();
    let client = daemon.connect().await.unwrap();
    assert!(client.list_triggers().await.unwrap().is_empty());
    assert!(
        client
            .list_runs(RunListParams::default())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(client.list_defs(None).await.unwrap().defs.is_empty());

    daemon.fail(
        "trigger.list",
        ErrorObject::new(ErrorCode::Unavailable, "busy"),
    );
    assert_eq!(
        client.list_triggers().await.unwrap_err().code(),
        Some(ErrorCode::Unavailable)
    );
    daemon.clear_failures();
    assert!(client.list_triggers().await.is_ok());
}

#[tokio::test(start_paused = true)]
async fn a_long_run_stays_live_until_cancelled_or_steered() {
    let daemon = FakeDaemon::start(FakeConfig::default()).unwrap();
    let client = daemon.connect().await.unwrap();
    let a = client.run_task(task("/long one")).await.unwrap();
    let b = client.run_task(task("/long two")).await.unwrap();
    tokio::time::sleep(Duration::from_secs(60)).await;
    assert_eq!(client.get_run(&a).await.unwrap().status, RunStatus::Running);
    assert_eq!(client.get_run(&b).await.unwrap().status, RunStatus::Running);

    client.cancel_run(&a, None).await.unwrap();
    assert_eq!(
        client.get_run(&a).await.unwrap().status,
        RunStatus::Cancelled
    );
    client.steer(&b, "finish".to_owned()).await.unwrap();
    tokio::time::sleep(Duration::from_secs(5)).await;
    assert_eq!(
        client.get_run(&b).await.unwrap().status,
        RunStatus::Succeeded
    );
}
