//! Executor install and update jobs against the fake daemon (feature `executor.maintain`).
//! Time is tokio's, paused: nothing here waits on a wall clock.

#![cfg(feature = "fake")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use futures::StreamExt;
use reno_client::fake::{FakeConfig, FakeDaemon};
use reno_client::{Client, State, Update};
use reno_proto::{
    ErrorCode, ExecutorId, ExecutorInfo, InstallStatus, JobFailure, JobKind, JobPhase, VersionGate,
};

fn id(s: &str) -> ExecutorId {
    s.parse().unwrap()
}

async fn executor(client: &Client, id: &str) -> ExecutorInfo {
    client
        .list_executors()
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.descriptor.id.as_str() == id)
        .unwrap()
}

fn version(info: &ExecutorInfo) -> Option<String> {
    info.health
        .as_ref()
        .and_then(|h| h.installed.version().map(str::to_owned))
}

async fn daemon() -> (FakeDaemon, Client) {
    let daemon = FakeDaemon::start(FakeConfig::default()).unwrap();
    let client = daemon.connect().await.unwrap();
    assert!(client.supports(reno_proto::FEATURE_EXECUTOR_MAINTAIN));
    (daemon, client)
}

async fn settle(secs: u64) {
    tokio::time::sleep(Duration::from_secs(secs)).await;
}

#[tokio::test(start_paused = true)]
async fn the_demo_executors_mirror_the_real_machine() {
    let (_daemon, client) = daemon().await;
    let claude = executor(&client, "claude-code").await.maintenance.unwrap();
    let updater = claude.updater.unwrap();
    assert_eq!(updater.command, "claude update");
    assert!(updater.self_updating && updater.can_pin && updater.latest.is_none());
    assert_eq!(claude.sign_in.unwrap().command, "claude auth login");
    assert!(claude.auto_update.on && !claude.auto_update.verified_only);

    let codex = executor(&client, "codex").await.maintenance.unwrap();
    let updater = codex.updater.unwrap();
    assert!(!updater.self_updating && !updater.can_pin);
    assert_eq!(updater.latest.as_deref(), Some("0.171.0"));
    assert_eq!(codex.sign_in.unwrap().command, "codex login");

    let grok = executor(&client, "acp.grok").await.maintenance.unwrap();
    assert_eq!(grok.updater.unwrap().latest.as_deref(), Some("1.0.92"));

    let antigravity = executor(&client, "acp.antigravity")
        .await
        .maintenance
        .unwrap();
    assert!(antigravity.managed && antigravity.updater.is_none());

    let devin = executor(&client, "acp.devin").await;
    assert!(matches!(
        devin.health.unwrap().installed,
        InstallStatus::NotInstalled { .. }
    ));
    let setup = devin.maintenance.unwrap().setup.unwrap();
    assert_eq!(setup.source, "Homebrew cask devin-cli");
    assert_eq!(setup.steps.len(), 2);
    let fx = executor(&client, "acp.fx").await.maintenance.unwrap();
    assert_eq!(fx.setup.unwrap().steps[1].command, "fx");
    let hermes = executor(&client, "acp.hermes").await.maintenance.unwrap();
    assert_eq!(hermes.setup.unwrap().steps.len(), 2);

    assert!(executor(&client, "mock").await.maintenance.is_none());
}

#[tokio::test(start_paused = true)]
async fn an_install_job_runs_its_steps_then_the_executor_is_installed() {
    let (_daemon, client) = daemon().await;
    let devin = id("acp.devin");
    let mut watch = client.watch_executors().await.unwrap();
    watch.next().await.unwrap().unwrap();

    let started = client
        .start_executor_job(&devin, JobKind::Install, false)
        .await
        .unwrap();
    let job = started.job.unwrap();
    assert_eq!(job.kind, JobKind::Install);
    assert!(matches!(
        job.phase,
        JobPhase::Running {
            step: 1,
            steps: 1,
            ..
        }
    ));

    // Starting again is refused while it runs.
    let again = client
        .start_executor_job(&devin, JobKind::Install, false)
        .await
        .unwrap_err();
    assert_eq!(again.code(), Some(ErrorCode::JobRunning));

    // The watch carries the job, with its last line, until it succeeds and leaves no trace.
    let mut saw_line = false;
    loop {
        let snapshot = watch.next().await.unwrap().unwrap();
        let info = snapshot
            .executors
            .iter()
            .find(|e| e.descriptor.id == devin)
            .unwrap();
        match &info.job {
            Some(job) => {
                if let JobPhase::Running {
                    last_line: Some(line),
                    ..
                } = &job.phase
                {
                    saw_line |= line.starts_with("$ brew install");
                }
            }
            None => {
                let health = info.health.as_ref().unwrap();
                assert!(matches!(health.installed, InstallStatus::Installed { .. }));
                break;
            }
        }
    }
    assert!(saw_line);

    // Its output is kept, and nothing is left to install.
    let log = client.executor_job_log(&devin).await.unwrap();
    assert!(!log.truncated);
    assert_eq!(
        log.lines.first().unwrap(),
        "$ brew install --cask devin-cli"
    );
    let nothing = client
        .start_executor_job(&devin, JobKind::Install, false)
        .await
        .unwrap_err();
    assert_eq!(nothing.code(), Some(ErrorCode::NothingToDo));
}

#[tokio::test(start_paused = true)]
async fn a_multi_step_install_reports_each_step() {
    let (_daemon, client) = daemon().await;
    let hermes = id("acp.hermes");
    let started = client
        .start_executor_job(&hermes, JobKind::Install, false)
        .await
        .unwrap();
    assert!(matches!(
        started.job.unwrap().phase,
        JobPhase::Running { steps: 2, .. }
    ));
    settle(2).await;
    let mid = executor(&client, "acp.hermes").await;
    let JobPhase::Running { step, .. } = mid.job.unwrap().phase else {
        panic!("still running");
    };
    assert_eq!(step, 2);
    settle(5).await;
    assert!(executor(&client, "acp.hermes").await.job.is_none());
}

#[tokio::test(start_paused = true)]
async fn an_update_moves_the_version_to_the_latest() {
    let (_daemon, client) = daemon().await;
    let grok = id("acp.grok");
    assert_eq!(
        version(&executor(&client, "acp.grok").await).unwrap(),
        "0.4.0"
    );
    client
        .start_executor_job(&grok, JobKind::Update, false)
        .await
        .unwrap();
    settle(5).await;
    let after = executor(&client, "acp.grok").await;
    assert!(after.job.is_none());
    assert_eq!(version(&after).unwrap(), "1.0.92");
    assert_eq!(
        after.health.unwrap().version_gate,
        VersionGate::Untested {
            verified: "0.12.0".into()
        }
    );
    // Up to date now.
    let nothing = client
        .start_executor_job(&grok, JobKind::Update, false)
        .await
        .unwrap_err();
    assert_eq!(nothing.code(), Some(ErrorCode::NothingToDo));
}

#[tokio::test(start_paused = true)]
async fn an_update_with_no_updater_or_nothing_installed_is_refused() {
    let (_daemon, client) = daemon().await;
    for (name, kind, code) in [
        ("acp.antigravity", JobKind::Update, ErrorCode::NoUpdater),
        ("mock", JobKind::Update, ErrorCode::NoUpdater),
        ("acp.devin", JobKind::Update, ErrorCode::NothingToDo),
        ("claude-code", JobKind::Install, ErrorCode::NothingToDo),
        ("nowhere", JobKind::Update, ErrorCode::NotFound),
    ] {
        let error = client
            .start_executor_job(&id(name), kind, false)
            .await
            .unwrap_err();
        assert_eq!(error.code(), Some(code), "{name}");
    }
}

#[tokio::test(start_paused = true)]
async fn an_update_waits_for_runs_unless_forced() {
    let (daemon, client) = daemon().await;
    let codex = id("codex");
    // Codex has a run waiting for approval.
    let queued = client
        .start_executor_job(&codex, JobKind::Update, false)
        .await
        .unwrap();
    assert_eq!(
        queued.job.unwrap().phase,
        JobPhase::Queued { waiting_runs: 1 }
    );
    settle(10).await;
    assert!(matches!(
        executor(&client, "codex").await.job.unwrap().phase,
        JobPhase::Queued { .. }
    ));
    assert_eq!(
        version(&executor(&client, "codex").await).unwrap(),
        "0.170.0"
    );

    // When the run ends the job starts and finishes.
    daemon.set_active_runs(&codex, 0);
    assert!(matches!(
        executor(&client, "codex").await.job.unwrap().phase,
        JobPhase::Running { .. }
    ));
    settle(5).await;
    let after = executor(&client, "codex").await;
    assert!(after.job.is_none());
    assert_eq!(version(&after).unwrap(), "0.171.0");
}

#[tokio::test(start_paused = true)]
async fn force_skips_the_queue() {
    let (_daemon, client) = daemon().await;
    let started = client
        .start_executor_job(&id("codex"), JobKind::Update, true)
        .await
        .unwrap();
    assert!(matches!(
        started.job.unwrap().phase,
        JobPhase::Running { .. }
    ));
}

#[tokio::test(start_paused = true)]
async fn a_cancelled_job_fails_as_cancelled_and_can_be_dismissed() {
    let (_daemon, client) = daemon().await;
    let grok = id("acp.grok");
    client
        .start_executor_job(&grok, JobKind::Update, false)
        .await
        .unwrap();
    settle(1).await;
    let cancelled = client.cancel_executor_job(&grok).await.unwrap();
    assert!(matches!(
        cancelled.job.unwrap().phase,
        JobPhase::Failed {
            failure: JobFailure::Cancelled,
            ..
        }
    ));
    // It stays failed: the task is gone and nothing finishes it.
    settle(10).await;
    let still = executor(&client, "acp.grok").await;
    assert!(matches!(
        still.job.as_ref().unwrap().phase,
        JobPhase::Failed {
            failure: JobFailure::Cancelled,
            ..
        }
    ));
    assert_eq!(version(&still).unwrap(), "0.4.0");
    assert!(
        !client
            .executor_job_log(&grok)
            .await
            .unwrap()
            .lines
            .is_empty()
    );

    let cleared = client.dismiss_executor_job(&grok).await.unwrap();
    assert!(cleared.job.is_none());
    let nothing = client.cancel_executor_job(&grok).await.unwrap_err();
    assert_eq!(nothing.code(), Some(ErrorCode::NothingToDo));
}

#[tokio::test(start_paused = true)]
async fn a_running_job_cannot_be_dismissed() {
    let (_daemon, client) = daemon().await;
    let grok = id("acp.grok");
    client
        .start_executor_job(&grok, JobKind::Update, false)
        .await
        .unwrap();
    let error = client.dismiss_executor_job(&grok).await.unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::JobRunning));
}

#[tokio::test(start_paused = true)]
async fn an_injected_failure_ends_the_job_failed_and_a_new_start_replaces_it() {
    let (daemon, client) = daemon().await;
    let devin = id("acp.devin");
    daemon.fail_next_job(JobFailure::NeedsPassword, "sudo asked for a password");
    client
        .start_executor_job(&devin, JobKind::Install, false)
        .await
        .unwrap();
    settle(5).await;
    let failed = executor(&client, "acp.devin").await;
    let Some(JobPhase::Failed {
        failure, detail, ..
    }) = failed.job.as_ref().map(|j| j.phase.clone())
    else {
        panic!("failed job");
    };
    assert_eq!(failure, JobFailure::NeedsPassword);
    assert_eq!(detail, "sudo asked for a password");
    assert!(matches!(
        failed.health.unwrap().installed,
        InstallStatus::NotInstalled { .. }
    ));
    let log = client.executor_job_log(&devin).await.unwrap();
    assert!(log.lines.last().unwrap().contains("sudo asked"));

    // The failure was for one job only; a new start replaces the failed job and succeeds.
    client
        .start_executor_job(&devin, JobKind::Install, false)
        .await
        .unwrap();
    settle(5).await;
    let done = executor(&client, "acp.devin").await;
    assert!(done.job.is_none());
    assert!(matches!(
        done.health.unwrap().installed,
        InstallStatus::Installed { .. }
    ));
}

#[tokio::test(start_paused = true)]
async fn the_job_duration_is_configurable() {
    let daemon = FakeDaemon::start(FakeConfig {
        job_duration: Duration::from_secs(60),
        ..FakeConfig::default()
    })
    .unwrap();
    let client = daemon.connect().await.unwrap();
    client
        .start_executor_job(&id("acp.grok"), JobKind::Update, false)
        .await
        .unwrap();
    settle(30).await;
    assert!(executor(&client, "acp.grok").await.job.is_some());
    settle(31).await;
    assert!(executor(&client, "acp.grok").await.job.is_none());
}

#[tokio::test(start_paused = true)]
async fn auto_update_is_stored_and_echoed() {
    let (_daemon, client) = daemon().await;
    let codex = id("codex");
    let echoed = client
        .set_executor_auto_update(&codex, false, true)
        .await
        .unwrap();
    let auto = echoed.maintenance.unwrap().auto_update;
    assert!(!auto.on && auto.verified_only);
    let stored = executor(&client, "codex").await.maintenance.unwrap();
    assert_eq!(stored.auto_update, auto);

    for name in ["mock", "acp.antigravity"] {
        let error = client
            .set_executor_auto_update(&id(name), true, false)
            .await
            .unwrap_err();
        assert_eq!(error.code(), Some(ErrorCode::NoUpdater), "{name}");
    }
    let error = client
        .set_executor_auto_update(&id("nowhere"), true, false)
        .await
        .unwrap_err();
    assert_eq!(error.code(), Some(ErrorCode::NotFound));
}

#[tokio::test(start_paused = true)]
async fn without_the_feature_the_calls_are_refused_up_front() {
    let daemon = FakeDaemon::start(FakeConfig {
        without_features: vec![reno_proto::FEATURE_EXECUTOR_MAINTAIN.to_owned()],
        ..FakeConfig::default()
    })
    .unwrap();
    let client = daemon.connect().await.unwrap();
    assert!(!client.supports(reno_proto::FEATURE_EXECUTOR_MAINTAIN));
    let codex = id("codex");
    assert!(matches!(
        client
            .start_executor_job(&codex, JobKind::Update, false)
            .await,
        Err(reno_client::ClientError::Protocol(_))
    ));
    assert!(client.cancel_executor_job(&codex).await.is_err());
    assert!(client.dismiss_executor_job(&codex).await.is_err());
    assert!(client.executor_job_log(&codex).await.is_err());
    assert!(
        client
            .set_executor_auto_update(&codex, true, false)
            .await
            .is_err()
    );
}

#[test]
fn state_keeps_maintenance_and_job_as_the_daemon_sent_them() {
    let mut info = executor_with_job();
    let mut state = State::new();
    state.apply(Update::Executors(vec![info.clone()]));
    assert_eq!(state.executors()[0], info);
    assert!(state.executors()[0].maintenance.is_some());
    assert!(state.executors()[0].job.is_some());
    info.job = None;
    state.apply(Update::Executors(vec![info.clone()]));
    assert!(state.executors()[0].job.is_none());
}

fn executor_with_job() -> ExecutorInfo {
    let mut json: serde_json::Value = serde_json::json!({
        "descriptor": {
            "id": "codex", "contractVersion": 1, "adapterVersion": "x",
            "cliVerified": {"min": "0", "verified": "0"},
            "caps": {"steering": "none", "resume": false, "accessModes": [],
                     "sandboxLevels": [], "mcpInjection": false, "structuredOutput": false,
                     "deterministicTurnEnd": false, "planMode": false, "models": [],
                     "modelSwitch": false, "isolatesUserConfig": false, "modelAliases": []}
        }
    });
    json["maintenance"] = serde_json::json!({
        "managed": false, "autoUpdate": {"on": true, "verifiedOnly": false}
    });
    json["job"] = serde_json::json!({
        "id": "j", "kind": "update", "startedAtMs": 1,
        "phase": {"type": "queued", "waitingRuns": 1}
    });
    serde_json::from_value(json).unwrap()
}
