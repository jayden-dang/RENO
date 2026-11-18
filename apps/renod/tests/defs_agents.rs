//! Definitions follow the agents of a live daemon, through the real RPCs: a bot that names an
//! agent added later becomes valid without a restart, and removing an agent a bot names (one
//! registered at start-up, or added while running) makes the bot invalid, saying so, with the
//! file left alone. The agent is the fake registry agent, never started here beyond its probe.

// Tests unwrap freely and set environment variables.
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]
#![cfg(unix)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use common::{Fixture, within};
use futures::StreamExt;
use reno_harness::proc::maintain::ScriptedRunner;
use reno_harness::proc::self_update::{SelfUpdateControl, SelfUpdateError, SelfUpdateSwitch};
use reno_harness::{AcpExecutor, RegistryAgent};
use reno_kernel::{ExecutorRegistry, Limits, MaintenancePorts};
use reno_proto::defs::DefsSnapshot;
use renod::{ExecutorSet, ExecutorSource};

#[derive(Debug)]
struct NoFiles;

impl SelfUpdateControl for NoFiles {
    fn previous(&self, _: SelfUpdateSwitch) -> Result<String, SelfUpdateError> {
        unreachable!("no self-updater here")
    }
    fn turn_off(&self, _: SelfUpdateSwitch) -> Result<String, SelfUpdateError> {
        unreachable!("no self-updater here")
    }
    fn restore(&self, _: SelfUpdateSwitch, _: &str) -> Result<(), SelfUpdateError> {
        unreachable!("no self-updater here")
    }
}

fn fixture_agent() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../crates/reno-harness/tests/fixtures/fake-registry-signin.py")
}

/// The agents folder of this process, with `sd-start` already added.
fn agents() {
    static ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("sd-start");
        std::fs::create_dir_all(&dir).unwrap();
        let record = serde_json::json!({
            "id": "sd-start", "name": "Sd start", "version": "1.0.0", "kind": "binary",
            "program": fixture_agent(), "args": ["--acp"],
            "env": { "FAKE_SIGNIN": "droid" }, "verified": true
        });
        std::fs::write(dir.join("agent.json"), record.to_string()).unwrap();
        // SAFETY: set once, before any daemon in this file starts.
        unsafe {
            std::env::set_var("RENO_AGENTS_DIR", root.path());
            std::env::set_var("RENO_ACP_REGISTRY_LOOPBACK", "1");
        }
        root
    });
}

async fn daemon() -> Fixture {
    agents();
    let agent = RegistryAgent::new("sd-start").unwrap();
    let registry =
        ExecutorRegistry::new().with(Arc::new(AcpExecutor::registry(agent, "Sd start").unwrap()));
    Fixture::with(|c| {
        c.executors = ExecutorSource::Given(ExecutorSet::from_registry(registry));
        c.maintenance = Some(MaintenancePorts {
            runner: Arc::new(ScriptedRunner::new()),
            self_update: Arc::new(NoFiles),
            limits: Limits::default(),
            check_at_start: false,
        });
        c.registry_index_url = Some("http://127.0.0.1:9/registry.json".into());
    })
    .await
}

fn write_bot(fx: &Fixture, name: &str, executor: &str) {
    let dir = fx.paths.defs_dir.join("bots");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(format!("{name}.toml")),
        format!("name = \"{name}\"\ninstructions = \"go\"\nexecutors = [\"{executor}\"]\n"),
    )
    .unwrap();
}

async fn until(
    watch: &mut futures::stream::BoxStream<'static, Result<DefsSnapshot, reno_client::ClientError>>,
    ready: impl Fn(&DefsSnapshot) -> bool,
) -> DefsSnapshot {
    within(async {
        loop {
            let snapshot = watch.next().await.expect("the watch ended").unwrap();
            if ready(&snapshot) {
                return snapshot;
            }
        }
    })
    .await
}

fn has_bot(s: &DefsSnapshot, name: &str) -> bool {
    s.defs.iter().any(|d| d.name == name)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_an_agent_registered_at_start_makes_its_bot_invalid() {
    let fx = daemon().await;
    let mut watch = fx.client.watch_defs().await.unwrap();
    write_bot(&fx, "keeper", "acp.sd-start");
    until(&mut watch, |s| has_bot(s, "keeper") && s.errors.is_empty()).await;

    fx.client.registry_remove("sd-start").await.unwrap();
    let snapshot = until(&mut watch, |s| !s.errors.is_empty()).await;
    let said = snapshot.errors[0].to_string();
    assert!(
        said.contains("unknown executor \"acp.sd-start\"")
            && said.contains("was removed")
            && said.contains("add it again"),
        "{said}"
    );
    // The bot is not deleted: its file is there, and it stays in the set in force.
    assert!(fx.paths.defs_dir.join("bots/keeper.toml").exists());
    assert!(has_bot(&snapshot, "keeper"));
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_agent_added_while_running_makes_its_bot_valid_and_removing_it_breaks_it_again() {
    let fx = daemon().await;
    let mut watch = fx.client.watch_defs().await.unwrap();
    let command = format!("{} --acp", fixture_agent().display());
    write_bot(&fx, "later", "acp.own.later");
    let snapshot = until(&mut watch, |s| !s.errors.is_empty()).await;
    assert!(
        snapshot.errors[0]
            .to_string()
            .ends_with("unknown executor \"acp.own.later\""),
        "the same message as for any unknown executor: {:?}",
        snapshot.errors
    );

    within(fx.client.registry_add_own("later", &command, ""))
        .await
        .unwrap();
    until(&mut watch, |s| has_bot(s, "later") && s.errors.is_empty()).await;

    fx.client.registry_remove("own.later").await.unwrap();
    let snapshot = until(&mut watch, |s| !s.errors.is_empty()).await;
    assert!(snapshot.errors[0].to_string().contains("was removed"));
    assert!(fx.paths.defs_dir.join("bots/later.toml").exists());
    fx.stop().await;
}
