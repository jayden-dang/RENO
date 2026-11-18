//! A hand-written daemon over a pipe, for a frame a real server never sends on a latest-value
//! subscription: `sub.gap`. The client must subscribe again and deliver the current value.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use futures::StreamExt;
use reno_client::Client;
use reno_proto::PROTOCOL_VERSION;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

/// Answers the handshake and `runs.watch`. The first `gaps` watches are cut off with `sub.gap`
/// before any item; the next one delivers an empty list.
async fn daemon(server_end: tokio::io::DuplexStream, gaps: u32) {
    let (read, mut write) = tokio::io::split(server_end);
    let mut lines = BufReader::new(read).lines();
    let mut watches = 0;
    while let Ok(Some(line)) = lines.next_line().await {
        let frame: Value = serde_json::from_str(&line).unwrap();
        let id = frame["id"].clone();
        let out = match frame["method"].as_str() {
            Some("daemon.hello") => vec![json!({"jsonrpc": "2.0", "id": id, "result": {
                "serverVersion": "t", "protocolVersion": PROTOCOL_VERSION,
                "features": ["executors", "files"], "pid": 1}})],
            Some(method @ ("runs.watch" | "executors.watch" | "fs.watch")) => {
                watches += 1;
                let items = match method {
                    "runs.watch" => json!({"runs": []}),
                    "fs.watch" => json!({"dirs": ["a"]}),
                    _ => json!({"executors": []}),
                };
                let sub = format!("sub_w{watches}");
                let ack = json!({"jsonrpc": "2.0", "id": id, "result": {"subscription": sub}});
                let next = if watches <= gaps {
                    json!({"jsonrpc": "2.0", "method": "sub.gap",
                           "params": {"subscription": sub, "resumeFrom": 1}})
                } else {
                    json!({"jsonrpc": "2.0", "method": "sub.item",
                           "params": {"subscription": sub, "item": items}})
                };
                vec![ack, next]
            }
            _ => vec![],
        };
        for frame in out {
            write
                .write_all(format!("{frame}\n").as_bytes())
                .await
                .unwrap();
        }
    }
}

async fn connect(gaps: u32) -> (Client, CancellationToken, tokio::task::JoinHandle<()>) {
    let (client_end, server_end) = tokio::io::duplex(64 * 1024);
    let cancel = CancellationToken::new();
    let server = tokio::spawn(daemon(server_end, gaps));
    let rpc = reno_rpc::Client::from_io(client_end, cancel.clone());
    (Client::connect(rpc).await.unwrap(), cancel, server)
}

#[tokio::test]
async fn a_gap_on_a_watch_resubscribes_and_delivers_the_current_value() {
    let (client, cancel, server) = connect(2).await;
    let mut watch = client.watch_runs().await.unwrap();
    let first = watch.next().await.unwrap().unwrap();
    assert!(first.runs.is_empty());
    cancel.cancel();
    server.abort();
}

#[tokio::test]
async fn a_gap_on_the_executors_watch_resubscribes_too() {
    let (client, cancel, server) = connect(2).await;
    let mut watch = client.watch_executors().await.unwrap();
    let first = watch.next().await.unwrap().unwrap();
    assert!(first.executors.is_empty());
    cancel.cancel();
    server.abort();
}

#[tokio::test]
async fn a_watch_that_only_gaps_ends_with_a_protocol_error() {
    let (client, cancel, server) = connect(u32::MAX).await;
    let mut watch = client.watch_runs().await.unwrap();
    let first = watch.next().await.unwrap();
    assert!(matches!(first, Err(reno_client::ClientError::Protocol(_))));
    assert!(watch.next().await.is_none());
    cancel.cancel();
    server.abort();
}

#[tokio::test]
async fn a_gap_on_a_file_watch_resubscribes_and_says_to_rescan_before_going_on() {
    let (client, cancel, server) = connect(1).await;
    let run = reno_proto::RunId::from_suffix("r1").unwrap();
    let mut watch = client.watch_fs(run).await.unwrap();
    // What changed while the daemon had cut it off is unknown.
    let rescan = watch.next().await.unwrap().unwrap();
    assert!(rescan.overflow && rescan.git && rescan.dirs.is_empty());
    let next = watch.next().await.unwrap().unwrap();
    assert_eq!(next.dirs, ["a"]);
    assert!(!next.overflow);
    cancel.cancel();
    server.abort();
}

#[tokio::test]
async fn a_file_watch_that_only_gaps_ends_with_a_protocol_error() {
    let (client, cancel, server) = connect(u32::MAX).await;
    let run = reno_proto::RunId::from_suffix("r1").unwrap();
    let mut watch = client.watch_fs(run).await.unwrap();
    let mut last = None;
    while let Some(item) = watch.next().await {
        last = Some(item);
    }
    assert!(matches!(last, Some(Err(reno_client::ClientError::Protocol(_)))));
    cancel.cancel();
    server.abort();
}
