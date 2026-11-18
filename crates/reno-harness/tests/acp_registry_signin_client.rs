//! A sign-in a person starts from a client (`Executor::sign_in`), against the fake registry agent
//! (tests/fixtures/fake-registry-signin.py): the page reaches the client and is never opened here,
//! a device code and the output lines are reported, a terminal-only agent ends with the command to
//! run, and what the agent offered is listed only after a full probe has seen its handshake.
//!
//! The agent's `open` is the system's own when the interception fails: the fixture then writes
//! `leaked-open` into its home, and every test here checks that nothing is there.

// Tests unwrap freely and set environment variables.
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use reno_harness::exec::{Executor, ProbeDepth};
use reno_harness::{AcpExecutor, RegistryAgent};
use reno_proto::{AuthMethodKind, SigninEvent};

fn fixture() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-registry-signin.py")
}

/// The agents, added: one record each; each has a home of its own (so the fixture's `paired` mark
/// and a leaked `open` are per agent) and a state folder.
fn agents() -> &'static Path {
    static ROOT: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let root = tempfile::tempdir().unwrap();
        for mode in ["droid", "droid-cli", "kimi"] {
            let dir = root.path().join(format!("sc-{mode}"));
            let home = dir.join("home");
            let state = dir.join("state");
            std::fs::create_dir_all(&home).unwrap();
            std::fs::create_dir_all(&state).unwrap();
            let record = serde_json::json!({
                "id": format!("sc-{mode}"), "name": format!("Sc {mode}"), "version": "1.0.0", "kind": "binary",
                "program": fixture(), "args": ["--acp"],
                "env": { "FAKE_SIGNIN": mode, "FAKE_STATE": state, "HOME": home }, "verified": true
            });
            std::fs::write(dir.join("agent.json"), record.to_string()).unwrap();
        }
        // SAFETY: set once, before any test reads it.
        unsafe { std::env::set_var("RENO_AGENTS_DIR", root.path()) };
        root
    })
    .path()
}

fn harness(mode: &str) -> (AcpExecutor, PathBuf) {
    let root = agents();
    let agent = RegistryAgent::new(&format!("sc-{mode}")).unwrap();
    (
        AcpExecutor::registry(agent, &format!("Sc {mode}")).unwrap(),
        root.join(format!("sc-{mode}")),
    )
}

type Seen = Arc<Mutex<Vec<SigninEvent>>>;

fn sink() -> (reno_harness::exec::SignInEvents, Seen) {
    let seen: Seen = Arc::default();
    let log = seen.clone();
    (Arc::new(move |event| log.lock().unwrap().push(event)), seen)
}

async fn within<T>(fut: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(30), fut)
        .await
        .expect("the sign-in ends")
}

#[tokio::test]
async fn a_sign_in_reports_its_page_and_code_and_opens_nothing() {
    let (agent, dir) = harness("droid");
    let (events, seen) = sink();
    // The person pairs the device on the page the client was told about.
    let paired = dir.join("home/paired");
    let watching = seen.clone();
    let pairing = tokio::spawn(async move {
        for _ in 0..400 {
            let page = watching.lock().unwrap().iter().any(
                |e| matches!(e, SigninEvent::Page { url } if url.contains("user_code=WXYZ-9876")),
            );
            if page {
                std::fs::write(&paired, "").unwrap();
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
    within(agent.sign_in(None, events))
        .await
        .expect("paired: signed in");
    pairing.await.unwrap();
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.iter()
            .any(|e| matches!(e, SigninEvent::Page { url } if url.contains("user_code=WXYZ-9876"))),
        "the page, to open: {seen:?}"
    );
    assert!(
        seen.iter().any(
            |e| matches!(e, SigninEvent::Output { line } if line.contains("Your code: WXYZ-9876"))
        ),
        "the code, to show: {seen:?}"
    );
    assert!(
        !dir.join("home/leaked-open").exists(),
        "the agent's `open` reached the system's"
    );
}

#[tokio::test]
async fn a_terminal_only_agent_ends_with_the_command_to_run() {
    let (agent, dir) = harness("droid-cli");
    let (events, seen) = sink();
    let error = within(agent.sign_in(None, events)).await.unwrap_err();
    assert!(error.to_string().contains("terminal"), "{error}");
    let command = seen
        .lock()
        .unwrap()
        .iter()
        .find_map(|e| match e {
            SigninEvent::NeedsTerminal { command } => Some(command.clone()),
            _ => None,
        })
        .expect("a needs-terminal event");
    assert!(
        command.contains("fake-registry-signin.py") && !command.contains("--acp"),
        "the agent's own CLI, run bare: {command}"
    );
    assert!(!dir.join("home/leaked-open").exists());
}

#[tokio::test]
async fn the_methods_an_agent_offers_are_listed_after_a_full_probe_only() {
    let (agent, dir) = harness("droid");
    let spawns = dir.join("state/spawns");
    // Nothing has seen the handshake, and a quiet probe starts nothing.
    assert!(agent.auth_methods().is_empty());
    let _ = agent.probe_at(ProbeDepth::Quiet).await;
    assert!(agent.auth_methods().is_empty());
    assert!(!spawns.exists(), "a quiet probe never starts the agent");

    let _ = agent.probe_at(ProbeDepth::Full).await;
    let methods = agent.auth_methods();
    let by = |id: &str| methods.iter().find(|m| m.id == id).unwrap().clone();
    assert_eq!(by("device-pairing").kind, AuthMethodKind::Agent);
    let key = by("api-key");
    assert_eq!(key.kind, AuthMethodKind::EnvVar);
    assert_eq!(key.vars, ["FACTORY_API_KEY"]);
}

#[tokio::test]
async fn a_terminal_method_is_listed_with_its_exact_command() {
    let (agent, _) = harness("kimi");
    let _ = agent.probe_at(ProbeDepth::Full).await;
    let methods = agent.auth_methods();
    let login = methods.iter().find(|m| m.id == "login").unwrap();
    assert_eq!(login.kind, AuthMethodKind::Terminal);
    let command = login.command.clone().expect("the command to run");
    assert!(
        command.ends_with("fake-registry-signin.py login"),
        "{command}"
    );
}
