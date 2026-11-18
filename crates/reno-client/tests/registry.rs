//! The ACP registry against the fake daemon (feature `executor.registry`): the list in each
//! state, add, update, remove, an own agent and the per-agent environment. Time is tokio's,
//! paused.

#![cfg(feature = "fake")]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use reno_client::Client;
use reno_client::fake::{FakeConfig, FakeDaemon};
use reno_proto::{
    ErrorCode, ExecutorSource, JobFailure, RegistryDistribution, RegistryEntry, RegistryReadiness,
};

async fn daemon() -> (FakeDaemon, Client) {
    let daemon = FakeDaemon::start(FakeConfig {
        registry_agents: true,
        ..FakeConfig::default()
    })
    .unwrap();
    let client = daemon.connect().await.unwrap();
    assert!(client.supports(reno_proto::FEATURE_EXECUTOR_REGISTRY));
    (daemon, client)
}

async fn entry(client: &Client, id: &str) -> RegistryEntry {
    client
        .registry_list(false)
        .await
        .unwrap()
        .entries
        .into_iter()
        .find(|e| e.id == id)
        .unwrap_or_else(|| panic!("{id} is listed"))
}

fn code(error: reno_client::ClientError) -> ErrorCode {
    error
        .code()
        .unwrap_or_else(|| panic!("not a daemon error: {error}"))
}

#[tokio::test(start_paused = true)]
async fn the_list_has_an_entry_in_every_state() {
    let (_daemon, client) = daemon().await;
    let list = client.registry_list(false).await.unwrap();
    assert!(!list.offline);

    let goose = entry(&client, "goose").await;
    assert_eq!(goose.kind, Some(RegistryDistribution::Binary));
    assert_eq!(goose.checksum, Some(true));
    assert_eq!(goose.host.as_deref(), Some("github.com"));
    assert!(goose.added.is_none() && goose.icon.as_deref().unwrap().starts_with("<svg"));

    assert_eq!(entry(&client, "vtcode").await.checksum, Some(false));
    assert_eq!(entry(&client, "cline").await.checksum, None);

    let cline = entry(&client, "cline").await.added.unwrap();
    assert_eq!(cline.version, "1.0.0");
    assert!(cline.update_available);
    assert!(
        !entry(&client, "amp-acp")
            .await
            .added
            .unwrap()
            .update_available
    );

    let uv = entry(&client, "fast-agent").await;
    assert_eq!(uv.readiness, RegistryReadiness::NeedsUv);
    assert_eq!(uv.fix.as_deref(), Some("brew install uv"));
    assert_eq!(
        entry(&client, "gemini").await.readiness,
        RegistryReadiness::NeedsNode
    );
    assert_eq!(
        entry(&client, "windows-only").await.readiness,
        RegistryReadiness::NoBuild
    );

    let claude = entry(&client, "claude-acp").await;
    assert_eq!(claude.built_in.unwrap().as_str(), "claude-code");

    let old = entry(&client, "old-agent").await;
    assert!(old.delisted && old.added.is_some());
    let own = entry(&client, "own.mine").await;
    assert!(own.own && !own.delisted);
}

#[tokio::test(start_paused = true)]
async fn adding_registers_the_agent_and_installs_it_as_a_job() {
    let (_daemon, client) = daemon().await;
    let info = client.registry_add("goose").await.unwrap();
    assert_eq!(info.descriptor.id.as_str(), "acp.goose");
    assert_eq!(info.source, ExecutorSource::Registry);
    assert!(info.job.is_some());
    assert!(entry(&client, "goose").await.adding);

    tokio::time::sleep(Duration::from_secs(10)).await;
    let goose = entry(&client, "goose").await;
    assert!(!goose.adding);
    assert_eq!(goose.added.unwrap().version, "1.60.1");
}

#[tokio::test(start_paused = true)]
async fn adding_what_cannot_be_added_names_the_fix() {
    let (_daemon, client) = daemon().await;
    let error = client.registry_add("fast-agent").await.unwrap_err();
    assert_eq!(error.code().unwrap(), ErrorCode::Conflict);
    assert!(
        error.to_string().contains("needs uv: brew install uv"),
        "{error}"
    );
    let error = client.registry_add("gemini").await.unwrap_err();
    assert!(error.to_string().contains("needs Node.js"), "{error}");
    let error = client.registry_add("codex-acp").await.unwrap_err();
    assert_eq!(error.code().unwrap(), ErrorCode::NothingToDo);
    assert!(error.to_string().contains("built in"), "{error}");
    let error = client.registry_add("amp-acp").await.unwrap_err();
    assert_eq!(code(error), ErrorCode::NothingToDo);
    let error = client.registry_add("no-such-agent").await.unwrap_err();
    assert_eq!(code(error), ErrorCode::NotFound);
}

#[tokio::test(start_paused = true)]
async fn adding_an_added_agent_at_an_older_version_updates_it() {
    let (_daemon, client) = daemon().await;
    client.registry_add("cline").await.unwrap();
    tokio::time::sleep(Duration::from_secs(10)).await;
    let cline = entry(&client, "cline").await.added.unwrap();
    assert_eq!(cline.version, "1.1.0");
    assert!(!cline.update_available);
}

#[tokio::test(start_paused = true)]
async fn a_failed_add_stays_until_dismissed_and_then_the_agent_is_gone() {
    let (daemon, client) = daemon().await;
    daemon.fail_next_job(JobFailure::ExitCode, "the download broke");
    client.registry_add("goose").await.unwrap();
    tokio::time::sleep(Duration::from_secs(10)).await;
    let listed = client.list_executors().await.unwrap();
    let goose = listed
        .iter()
        .find(|e| e.descriptor.id.as_str() == "acp.goose")
        .unwrap();
    assert!(matches!(
        goose.job.as_ref().unwrap().phase,
        reno_proto::JobPhase::Failed { .. }
    ));
    client
        .dismiss_executor_job(&"acp.goose".parse().unwrap())
        .await
        .unwrap();
    assert!(entry(&client, "goose").await.added.is_none());
}

#[tokio::test(start_paused = true)]
async fn removing_refuses_while_a_run_is_live_and_then_deletes() {
    let (daemon, client) = daemon().await;
    daemon.set_active_runs(&"acp.amp-acp".parse().unwrap(), 1);
    let error = client.registry_remove("amp-acp").await.unwrap_err();
    assert_eq!(code(error), ErrorCode::Conflict);
    daemon.set_active_runs(&"acp.amp-acp".parse().unwrap(), 0);
    let removed = client.registry_remove("amp-acp").await.unwrap();
    assert_eq!(removed.as_str(), "acp.amp-acp");
    assert!(entry(&client, "amp-acp").await.added.is_none());
    assert_eq!(
        code(client.registry_remove("amp-acp").await.unwrap_err()),
        ErrorCode::NotFound
    );
}

#[tokio::test(start_paused = true)]
async fn an_own_agent_and_its_environment() {
    let (_daemon, client) = daemon().await;
    let info = client
        .registry_add_own("My Agent", "my-agent --acp", "TOKEN=abc\nMODE=fast\n")
        .await
        .unwrap();
    assert_eq!(info.descriptor.id.as_str(), "acp.own.my-agent");
    let own = entry(&client, "own.my-agent").await;
    assert!(own.own);
    assert_eq!(
        client.registry_env_get("own.my-agent").await.unwrap(),
        "TOKEN=abc\nMODE=fast\n"
    );
    client
        .registry_env_set("own.my-agent", "TOKEN=xyz\n")
        .await
        .unwrap();
    assert_eq!(
        client.registry_env_get("own.my-agent").await.unwrap(),
        "TOKEN=xyz\n"
    );
    let error = client
        .registry_env_set("own.my-agent", "not a pair")
        .await
        .unwrap_err();
    assert_eq!(error.code().unwrap(), ErrorCode::InvalidParams);
    assert!(
        !error.to_string().contains("not a pair"),
        "no value is echoed"
    );
    assert_eq!(
        code(client.registry_env_get("goose").await.unwrap_err()),
        ErrorCode::NotFound
    );
}

#[tokio::test(start_paused = true)]
async fn the_list_says_so_when_it_is_the_cached_copy() {
    let (daemon, client) = daemon().await;
    daemon.set_registry_offline(true);
    let list = client.registry_list(true).await.unwrap();
    assert!(list.offline && list.error.is_some());
    assert!(!list.entries.is_empty());
}
