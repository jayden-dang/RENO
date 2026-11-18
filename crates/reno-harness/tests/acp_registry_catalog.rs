//! The registry catalog against a registry served on this machine's loopback: the index cache and
//! its time to live, revalidation by ETag, the offline copy, icons kept beside it, and an agent of
//! your own with its environment. The real CDN is never reached.

// Tests unwrap freely and set environment variables.
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]
#![cfg(unix)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use reno_harness::acp_registry as reg;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const ETAG: &str = "\"v1\"";

struct Registry {
    base: String,
    index_requests: Arc<AtomicUsize>,
    icon_requests: Arc<AtomicUsize>,
    conditional: Arc<AtomicUsize>,
    down: Arc<AtomicBool>,
}

fn index(base: &str) -> serde_json::Value {
    serde_json::json!({ "agents": [{
        "id": "demo", "name": "Demo", "version": "1.0.0", "description": "A demo",
        "icon": format!("{base}/demo.svg"),
        "distribution": { "npx": { "package": "demo@1.0.0" } }
    }] })
}

async fn serve() -> Registry {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let registry = Registry {
        base: base.clone(),
        index_requests: Arc::default(),
        icon_requests: Arc::default(),
        conditional: Arc::default(),
        down: Arc::default(),
    };
    let (index_requests, icon_requests, conditional, down) = (
        registry.index_requests.clone(),
        registry.icon_requests.clone(),
        registry.conditional.clone(),
        registry.down.clone(),
    );
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let (index_requests, icon_requests, conditional, down, base) = (
                index_requests.clone(),
                icon_requests.clone(),
                conditional.clone(),
                down.clone(),
                base.clone(),
            );
            tokio::spawn(async move {
                let mut buf = vec![0; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase();
                if down.load(Ordering::SeqCst) {
                    return; // closes the connection with no answer
                }
                let (status, headers, body) = if request.starts_with("get /demo.svg") {
                    icon_requests.fetch_add(1, Ordering::SeqCst);
                    (
                        "200 OK",
                        String::new(),
                        "<svg viewBox=\"0 0 1 1\"><path d=\"M0 0\"/></svg>".to_owned(),
                    )
                } else {
                    index_requests.fetch_add(1, Ordering::SeqCst);
                    if request.contains(&format!("if-none-match: {}", ETAG.to_ascii_lowercase())) {
                        conditional.fetch_add(1, Ordering::SeqCst);
                        (
                            "304 Not Modified",
                            format!("etag: {ETAG}\r\n"),
                            String::new(),
                        )
                    } else {
                        (
                            "200 OK",
                            format!("etag: {ETAG}\r\n"),
                            index(&base).to_string(),
                        )
                    }
                };
                let reply = format!(
                    "HTTP/1.1 {status}\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(reply.as_bytes()).await;
            });
        }
    });
    registry
}

/// One sequence in one process: the agents root and the loopback switch are process-wide.
#[tokio::test]
async fn the_index_is_cached_revalidated_and_served_offline_with_its_icons() {
    let root = tempfile::tempdir().unwrap();
    // SAFETY: this file's only test; nothing else reads the environment.
    unsafe {
        std::env::set_var("RENO_AGENTS_DIR", root.path());
        std::env::set_var("RENO_ACP_REGISTRY_LOOPBACK", "1");
    }
    let registry = serve().await;
    let url = format!("{}/registry.json", registry.base);
    let ttl = Duration::from_secs(300);

    // First load: fetched.
    let loaded = reg::load_index(&url, false, ttl).await.unwrap();
    assert_eq!(loaded.entries.len(), 1);
    assert!(!loaded.offline && loaded.error.is_none());
    assert_eq!(registry.index_requests.load(Ordering::SeqCst), 1);

    // Within the time to live: the cache, no request.
    let again = reg::load_index(&url, false, ttl).await.unwrap();
    assert_eq!(again.entries, loaded.entries);
    assert_eq!(registry.index_requests.load(Ordering::SeqCst), 1);

    // A refresh asks again, with the ETag, and the 304 keeps the copy.
    let refreshed = reg::load_index(&url, true, ttl).await.unwrap();
    assert_eq!(refreshed.entries, loaded.entries);
    assert_eq!(registry.index_requests.load(Ordering::SeqCst), 2);
    assert_eq!(registry.conditional.load(Ordering::SeqCst), 1);

    // Past the time to live (a zero one): revalidated.
    reg::load_index(&url, false, Duration::ZERO).await.unwrap();
    assert_eq!(registry.conditional.load(Ordering::SeqCst), 2);

    // Icons are fetched once and then served from beside the index.
    let entry = &loaded.entries[0];
    let icon = reg::icon_of(entry).await.unwrap();
    assert!(icon.starts_with("<svg"));
    assert_eq!(reg::icon_of(entry).await.unwrap(), icon);
    assert_eq!(registry.icon_requests.load(Ordering::SeqCst), 1);

    // The registry goes away: the cached copy is served, and says so.
    registry.down.store(true, Ordering::SeqCst);
    let offline = reg::load_index(&url, true, ttl).await.unwrap();
    assert!(offline.offline, "{offline:?}");
    assert!(offline.error.is_some());
    assert_eq!(offline.entries, loaded.entries);
    assert!(
        reg::icon_of(entry).await.is_some(),
        "the icon is kept beside the index"
    );

    // With nothing cached a failed fetch is an error.
    std::fs::remove_dir_all(root.path().join(".index")).unwrap();
    assert!(reg::load_index(&url, true, ttl).await.is_err());
}
