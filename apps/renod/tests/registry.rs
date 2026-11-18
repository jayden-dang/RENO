//! `executor.registry.*` on a daemon served in-process, against a registry served on this
//! machine's loopback (never the real CDN, and nothing is installed outside a temporary agents
//! root): the list, add, remove while the daemon runs, an own agent and its environment, and a
//! daemon that was given no registry.

// Tests unwrap freely and set environment variables.
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]
#![cfg(unix)]

mod common;

use std::io::Write;
use std::sync::Arc;

use common::{Fixture, within};
use reno_harness::acp_registry as reg;
use reno_harness::proc::maintain::ScriptedRunner;
use reno_harness::proc::self_update::{SelfUpdateControl, SelfUpdateError, SelfUpdateSwitch};
use reno_kernel::{Limits, MaintenancePorts};
use reno_proto::{ErrorCode, ExecutorSource, RegistryReadiness};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

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

fn ports() -> MaintenancePorts {
    MaintenancePorts {
        runner: Arc::new(ScriptedRunner::new()),
        self_update: Arc::new(NoFiles),
        limits: Limits::default(),
        check_at_start: false,
    }
}

/// A tar.gz holding one executable that ends at once.
fn archive() -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    let body = b"#!/bin/sh\nexit 0\n";
    let mut header = tar::Header::new_gnu();
    header.set_size(body.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    tar.append_data(&mut header, "demo", &body[..]).unwrap();
    let tar = tar.into_inner().unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar).unwrap();
    gz.finish().unwrap()
}

async fn serve() -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let build = archive();
    let sha = format!("{:x}", Sha256::digest(&build));
    let platform = reg::platform().expect("a registry platform");
    let index = serde_json::json!({ "agents": [
        { "id": "demo", "name": "Demo", "version": "1.0.0", "description": "A demo agent",
          "distribution": { "binary": { platform: {
              "archive": format!("{base}/demo.tar.gz"), "cmd": "./demo", "sha256": sha } } } },
        { "id": "codex-acp", "name": "Codex CLI", "version": "0.4.0", "description": "",
          "distribution": { "npx": { "package": "@x/codex-acp@0.4.0" } } },
        { "id": "elsewhere", "name": "Elsewhere", "version": "2.0.0", "description": "",
          "distribution": { "binary": { "plan9-mips": {
              "archive": format!("{base}/x.tar.gz"), "cmd": "./x" } } } },
    ] })
    .to_string();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let (index, build) = (index.clone(), build.clone());
            tokio::spawn(async move {
                let mut buf = vec![0; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                let body = if request.starts_with("get /demo.tar.gz") {
                    build
                } else {
                    index.into_bytes()
                };
                let mut reply = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                reply.extend_from_slice(&body);
                let _ = socket.write_all(&reply).await;
            });
        }
    });
    base
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_registry_lists_adds_removes_and_keeps_own_agents_on_a_live_daemon() {
    let agents = tempfile::tempdir().unwrap();
    // SAFETY: this file's only test that reads the environment; set before the daemon starts.
    unsafe {
        std::env::set_var("RENO_AGENTS_DIR", agents.path());
        std::env::set_var("RENO_ACP_REGISTRY_LOOPBACK", "1");
    }
    let base = serve().await;
    let fx = Fixture::with(|c| {
        c.maintenance = Some(ports());
        c.registry_index_url = Some(format!("{base}/registry.json"));
    })
    .await;
    let client = &fx.client;
    assert!(client.supports(reno_proto::FEATURE_EXECUTOR_REGISTRY));

    // The list: what this device would install, the built-in one marked, the one with no build.
    let list = within(client.registry_list(false)).await.unwrap();
    assert!(!list.offline);
    let by = |id: &str| list.entries.iter().find(|e| e.id == id).unwrap().clone();
    let demo = by("demo");
    assert_eq!(demo.checksum, Some(true));
    assert_eq!(demo.host.as_deref(), Some("127.0.0.1"));
    assert_eq!(demo.readiness, RegistryReadiness::Ready);
    assert!(demo.added.is_none());
    assert_eq!(by("codex-acp").built_in.unwrap().as_str(), "codex");
    assert_eq!(by("elsewhere").readiness, RegistryReadiness::NoBuild);

    let before = within(client.list_executors()).await.unwrap().len();

    // Refused, each with its own code and a sentence that says what to do.
    let built_in = within(client.registry_add("codex-acp")).await.unwrap_err();
    assert_eq!(built_in.code(), Some(ErrorCode::NothingToDo));
    assert!(
        built_in.to_string().contains("built in: use codex"),
        "{built_in}"
    );
    let no_build = within(client.registry_add("elsewhere")).await.unwrap_err();
    assert_eq!(no_build.code(), Some(ErrorCode::Conflict));
    assert!(
        no_build.to_string().contains("no build for this Mac"),
        "{no_build}"
    );
    let unknown = within(client.registry_add("nope")).await.unwrap_err();
    assert_eq!(unknown.code(), Some(ErrorCode::NotFound));
    let own_via_add = within(client.registry_add("own.x")).await.unwrap_err();
    assert_eq!(own_via_add.code(), Some(ErrorCode::InvalidParams));
    assert_eq!(
        within(client.list_executors()).await.unwrap().len(),
        before,
        "none of those registered anything"
    );

    // Add: registered at once as a registry executor, installed by a job, probed.
    let info = within(client.registry_add("demo")).await.unwrap();
    assert_eq!(info.descriptor.id.as_str(), "acp.demo");
    assert_eq!(info.source, ExecutorSource::Registry);
    within(async {
        loop {
            let list = client.registry_list(false).await.unwrap();
            let demo = list.entries.iter().find(|e| e.id == "demo").unwrap();
            if demo.added.is_some() && !demo.adding {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(agents.path().join("demo/agent.json").is_file());
    let listed = within(client.list_executors()).await.unwrap();
    let live = listed
        .iter()
        .find(|e| e.descriptor.id.as_str() == "acp.demo")
        .unwrap();
    assert!(live.job.is_none(), "{live:?}");
    // A second add of the same version has nothing to do.
    let again = within(client.registry_add("demo")).await.unwrap_err();
    assert_eq!(again.code(), Some(ErrorCode::NothingToDo));

    // A task may name the executor added since the daemon started (the script it runs ends at
    // once, so the run ends), and a remove waits until nothing runs on it.
    let run = fx.run("acp.demo", "go").await;
    within(async {
        loop {
            if fx.status(&run).await.is_terminal() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await;

    // Remove: gone from the live set and from disk.
    let removed = within(client.registry_remove("demo")).await.unwrap();
    assert_eq!(removed.as_str(), "acp.demo");
    assert!(!agents.path().join("demo").exists());
    assert!(
        within(client.list_executors())
            .await
            .unwrap()
            .iter()
            .all(|e| e.descriptor.id.as_str() != "acp.demo")
    );
    let gone = within(client.registry_remove("demo")).await.unwrap_err();
    assert_eq!(gone.code(), Some(ErrorCode::NotFound));

    // An own agent: from its command, with its environment kept in the agents root.
    let script = agents.path().join("my-agent.sh");
    std::fs::write(&script, "#!/bin/sh\nexit 0\n").unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let own = within(client.registry_add_own(
        "My Agent",
        &format!("{} --acp", script.display()),
        "TOKEN=abc\n",
    ))
    .await
    .unwrap();
    assert_eq!(own.descriptor.id.as_str(), "acp.own.my-agent");
    assert_eq!(own.source, ExecutorSource::Registry);
    assert_eq!(
        within(client.registry_env_get("own.my-agent"))
            .await
            .unwrap(),
        "TOKEN=abc\n"
    );
    within(client.registry_env_set("own.my-agent", "A=1\nB=2\n"))
        .await
        .unwrap();
    assert_eq!(
        within(client.registry_env_get("own.my-agent"))
            .await
            .unwrap(),
        "A=1\nB=2\n"
    );
    let bad = within(client.registry_env_set("own.my-agent", "no pair"))
        .await
        .unwrap_err();
    assert_eq!(bad.code(), Some(ErrorCode::InvalidParams));
    let list = within(client.registry_list(false)).await.unwrap();
    assert!(list.entries.iter().any(|e| e.id == "own.my-agent" && e.own));
    // Reno never updates it: there is no update to start.
    within(client.registry_remove("own.my-agent"))
        .await
        .unwrap();
    assert!(!agents.path().join("own.my-agent").exists());
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_given_no_registry_offers_none_and_reaches_for_no_network() {
    let fx = Fixture::with(|c| c.maintenance = Some(ports())).await;
    assert!(!fx.client.supports(reno_proto::FEATURE_EXECUTOR_REGISTRY));
    fx.stop().await;
}
