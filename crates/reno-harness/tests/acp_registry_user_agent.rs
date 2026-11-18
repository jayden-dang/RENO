//! A download host that turns away a client with no `User-Agent` (Muse Code's lookaside.facebook.com
//! answers one with a redirect to an "unsupported browser" page, so the build never matched its
//! checksum): Reno names itself, and the build installs.

// Tests unwrap freely and set environment variables.
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]
#![cfg(unix)]

use std::io::Write;

use reno_harness::acp_registry as reg;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn archive() -> Vec<u8> {
    let mut tar = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(4);
    header.set_mode(0o755);
    header.set_cksum();
    tar.append_data(&mut header, "agent", &b"#!/x"[..]).unwrap();
    let tar = tar.into_inner().unwrap();
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    gz.write_all(&tar).unwrap();
    gz.finish().unwrap()
}

/// Serves the build to a client that names itself, a page to one that doesn't.
async fn serve(build: Vec<u8>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let build = build.clone();
            tokio::spawn(async move {
                let mut buf = vec![0; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                let body = if request.contains("\nuser-agent: ") {
                    build
                } else {
                    b"<html>unsupported browser</html>".to_vec()
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

#[tokio::test]
async fn a_download_names_reno_to_hosts_that_turn_away_nameless_clients() {
    let root = tempfile::tempdir().unwrap();
    // SAFETY: this file's only test; nothing else reads the environment.
    unsafe {
        std::env::set_var("RENO_AGENTS_DIR", root.path());
        std::env::set_var("RENO_ACP_REGISTRY_LOOPBACK", "1");
    }
    let build = archive();
    let sha = format!("{:x}", Sha256::digest(&build));
    let base = serve(build).await;
    let platform = reg::platform().expect("a registry platform");
    let index = serde_json::json!({ "agents": [{
        "id": "picky-host", "name": "Picky Host", "version": "1.0.0", "description": "",
        "distribution": { "binary": { platform: {
            "archive": format!("{base}/download.tar.gz"), "cmd": "./agent", "sha256": sha
        } } }
    }] });
    let entries = reg::parse_index(&serde_json::to_vec(&index).unwrap()).unwrap();
    let record = reg::install(&entries[0], None).await.expect("installs");
    assert!(record.verified);
}
