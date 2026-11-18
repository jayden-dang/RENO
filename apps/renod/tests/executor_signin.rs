//! `executor.signin.*` on a daemon served in-process, against the fake registry agent
//! (crates/reno-harness/tests/fixtures/fake-registry-signin.py) started for real by the daemon's
//! own driver: the methods the agent offered after a full probe, a sign-in whose page reaches the
//! client (and is never opened by the daemon), the health it ends with, a terminal-only agent, and
//! a cancel that takes the agent's process with it.
//!
//! Nothing here reaches the desktop. The fixture writes `leaked-open` into its home when the
//! `open` it calls is the system's own, so every test asserts that file is not there.

// Tests unwrap freely and set environment variables.
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]
#![cfg(unix)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{Fixture, within};
use futures::StreamExt;
use reno_harness::proc::maintain::ScriptedRunner;
use reno_harness::proc::self_update::{SelfUpdateControl, SelfUpdateError, SelfUpdateSwitch};
use reno_harness::{AcpExecutor, RegistryAgent};
use reno_kernel::{ExecutorRegistry, Limits, MaintenancePorts};
use reno_proto::{AuthMethodKind, ErrorCode, ExecutorId, SignedIn, SigninEvent};
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

const MODES: [&str; 3] = ["droid", "droid-cli", "droid-hang"];

/// The agents, added: one record each, with a home and a state folder of its own.
fn agents() -> &'static Path {
    static ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let root = tempfile::tempdir().unwrap();
        for mode in MODES {
            let dir = root.path().join(format!("sd-{mode}"));
            let (home, state) = (dir.join("home"), dir.join("state"));
            std::fs::create_dir_all(&home).unwrap();
            std::fs::create_dir_all(&state).unwrap();
            let record = serde_json::json!({
                "id": format!("sd-{mode}"), "name": format!("Sd {mode}"), "version": "1.0.0",
                "kind": "binary", "program": fixture_agent(), "args": ["--acp"],
                "env": { "FAKE_SIGNIN": mode, "FAKE_STATE": state, "FAKE_PERSIST_SIGNIN": "1",
                         "HOME": home },
                "verified": true
            });
            std::fs::write(dir.join("agent.json"), record.to_string()).unwrap();
        }
        // SAFETY: set once, before any daemon in this file starts.
        unsafe {
            std::env::set_var("RENO_AGENTS_DIR", root.path());
            std::env::set_var("RENO_ACP_REGISTRY_LOOPBACK", "1");
        }
        root
    })
    .path()
}

fn executor(mode: &str) -> ExecutorId {
    format!("acp.sd-{mode}").parse().unwrap()
}

fn home(mode: &str) -> PathBuf {
    agents().join(format!("sd-{mode}/home"))
}

/// A daemon with the three agents registered (installed, never probed) and a registry it never
/// asks anything of.
async fn daemon() -> Fixture {
    agents();
    let mut registry = ExecutorRegistry::new();
    for mode in MODES {
        let agent = RegistryAgent::new(&format!("sd-{mode}")).unwrap();
        registry = registry.with(Arc::new(
            AcpExecutor::registry(agent, &format!("Sd {mode}")).unwrap(),
        ));
    }
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

fn user_code_page(event: &SigninEvent) -> bool {
    matches!(event, SigninEvent::Page { url } if url.contains("user_code=WXYZ-9876"))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sign_in_hands_the_client_the_page_and_ends_with_the_health_signed_in() {
    let fx = daemon().await;
    let client = &fx.client;
    assert!(client.supports(reno_proto::FEATURE_EXECUTOR_SIGNIN));
    let id = executor("droid");

    // Registered and not yet looked at in full: nothing offered, and the agent has not run.
    let before = client
        .list_executors()
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.descriptor.id == id)
        .unwrap();
    assert!(before.auth_methods.is_empty());
    assert!(!agents().join("sd-droid/state/spawns").exists());

    // A person's check: now the handshake has been seen.
    let checked = within(client.refresh_executors(Some(id.clone())))
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.descriptor.id == id)
        .unwrap();
    assert!(matches!(
        checked.health.as_ref().unwrap().signed_in,
        SignedIn::No { .. }
    ));
    let kinds: Vec<_> = checked
        .auth_methods
        .iter()
        .map(|m| (m.id.as_str(), m.kind))
        .collect();
    assert_eq!(
        kinds,
        [
            ("device-pairing", AuthMethodKind::Agent),
            ("api-key", AuthMethodKind::EnvVar)
        ]
    );
    assert_eq!(checked.auth_methods[1].vars, ["FACTORY_API_KEY"]);

    let mut events = within(client.signin_executor(id.clone(), None))
        .await
        .unwrap();
    let mut seen = Vec::new();
    while let Some(event) = within(events.next()).await {
        let event = event.unwrap();
        if user_code_page(&event) {
            // The person pairs on the page the client was told about.
            std::fs::write(home("droid").join("paired"), "").unwrap();
        }
        seen.push(event);
    }
    assert!(
        seen.iter().any(user_code_page),
        "the page reached the client: {seen:?}"
    );
    assert!(
        seen.iter()
            .any(|e| matches!(e, SigninEvent::Output { line } if line.contains("WXYZ-9876"))),
        "its code: {seen:?}"
    );
    assert_eq!(
        seen.last().unwrap(),
        &SigninEvent::Done {
            signed_in: true,
            message: None
        }
    );
    assert!(
        !home("droid").join("leaked-open").exists(),
        "the agent's `open` reached the system's"
    );
    let after = client
        .list_executors()
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.descriptor.id == id)
        .unwrap();
    assert!(
        matches!(after.health.unwrap().signed_in, SignedIn::Yes { .. }),
        "the closing full probe shows it signed in"
    );
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_terminal_only_agent_ends_with_the_command_for_the_app_to_run() {
    let fx = daemon().await;
    let id = executor("droid-cli");
    let events: Vec<_> = within(async {
        fx.client
            .signin_executor(id.clone(), None)
            .await
            .unwrap()
            .map(|e| e.unwrap())
            .collect()
            .await
    })
    .await;
    let command = events
        .iter()
        .find_map(|e| match e {
            SigninEvent::NeedsTerminal { command } => Some(command.clone()),
            _ => None,
        })
        .expect("a needs-terminal event");
    assert!(
        command.contains("fake-registry-signin.py") && !command.contains("--acp"),
        "{command}"
    );
    assert!(matches!(
        events.last().unwrap(),
        SigninEvent::Done { signed_in: false, message: Some(m) } if m.contains("terminal")
    ));
    assert!(!home("droid-cli").join("leaked-open").exists());
    fx.stop().await;
}

fn alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .args(["-0", pid])
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_a_sign_in_stops_the_agent_and_a_second_start_is_refused_meanwhile() {
    let fx = daemon().await;
    let client = &fx.client;
    let id = executor("droid-hang");
    let mut events = within(client.signin_executor(id.clone(), None))
        .await
        .unwrap();
    // The page arrives and nobody pairs: the agent waits.
    loop {
        let event = within(events.next()).await.unwrap().unwrap();
        if matches!(event, SigninEvent::Page { .. }) {
            break;
        }
    }
    let Err(reno_client::ClientError::Remote(refused)) =
        client.signin_executor(id.clone(), None).await
    else {
        panic!("a second sign-in was accepted");
    };
    assert_eq!(refused.code, ErrorCode::Conflict);
    let pid = std::fs::read_to_string(agents().join("sd-droid-hang/state/pid")).unwrap();
    assert!(alive(pid.trim()), "the agent is waiting for the pairing");

    assert!(client.cancel_signin(&id).await.unwrap());
    let mut last = None;
    while let Some(event) = within(events.next()).await {
        last = Some(event.unwrap());
    }
    assert!(
        matches!(&last, Some(SigninEvent::Done { signed_in: false, message: Some(m) }) if m.contains("cancelled")),
        "{last:?}"
    );
    // The agent died with its sign-in.
    within(async {
        while alive(pid.trim()) {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(!home("droid-hang").join("leaked-open").exists());
    // And the executor is free again.
    assert!(!client.cancel_signin(&id).await.unwrap());
    fx.stop().await;
}
