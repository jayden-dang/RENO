//! A hand-written daemon over a pipe that cuts a `term.attach` off with `sub.gap`, as the real
//! server does to a viewer that cannot keep up. The client must attach again after the last
//! item it delivered, so every item arrives once and in order.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::{Arc, Mutex};

use futures::StreamExt;
use reno_client::Client;
use reno_proto::{PROTOCOL_VERSION, TermEvent, TermId, encode_bytes};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio_util::sync::CancellationToken;

fn data(seq: u64) -> Value {
    json!({"type": "data", "seq": seq, "data": encode_bytes(format!("chunk {seq}").as_bytes())})
}

/// Answers the handshake and `term.attach`. The first attach delivers items 1 and 2 and is cut
/// off with `resumeFrom: 3`; every later one delivers what follows its `afterSeq`, up to `last`,
/// and finishes. The `afterSeq` of each attach is recorded.
async fn daemon(
    server_end: tokio::io::DuplexStream,
    cuts: u32,
    last: u64,
    asked: Arc<Mutex<Vec<Option<u64>>>>,
) {
    let (read, mut write) = tokio::io::split(server_end);
    let mut lines = BufReader::new(read).lines();
    let mut attaches = 0;
    while let Ok(Some(line)) = lines.next_line().await {
        let frame: Value = serde_json::from_str(&line).unwrap();
        let id = frame["id"].clone();
        let out = match frame["method"].as_str() {
            Some("daemon.hello") => vec![json!({"jsonrpc": "2.0", "id": id, "result": {
                "serverVersion": "t", "protocolVersion": PROTOCOL_VERSION,
                "features": ["terminals"], "pid": 1}})],
            Some("term.attach") => {
                attaches += 1;
                let after = frame["params"]["afterSeq"].as_u64();
                asked.lock().unwrap().push(after);
                let sub = format!("sub_t{attaches}");
                let mut out = vec![json!({"jsonrpc": "2.0", "id": id,
                                          "result": {"subscription": sub}})];
                let from = after.unwrap_or(0) + 1;
                if attaches <= cuts {
                    // Two items, then cut off before the third.
                    for seq in from..from + 2 {
                        out.push(json!({"jsonrpc": "2.0", "method": "sub.item",
                                        "params": {"subscription": sub, "seq": seq,
                                                   "item": data(seq)}}));
                    }
                    out.push(json!({"jsonrpc": "2.0", "method": "sub.gap",
                                    "params": {"subscription": sub, "resumeFrom": from + 2}}));
                } else {
                    for seq in from..=last {
                        out.push(json!({"jsonrpc": "2.0", "method": "sub.item",
                                        "params": {"subscription": sub, "seq": seq,
                                                   "item": data(seq)}}));
                    }
                    out.push(json!({"jsonrpc": "2.0", "method": "sub.done",
                                    "params": {"subscription": sub}}));
                }
                out
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

async fn connect(
    cuts: u32,
    last: u64,
) -> (
    Client,
    Arc<Mutex<Vec<Option<u64>>>>,
    CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    let (client_end, server_end) = tokio::io::duplex(64 * 1024);
    let cancel = CancellationToken::new();
    let asked = Arc::new(Mutex::new(Vec::new()));
    let server = tokio::spawn(daemon(server_end, cuts, last, Arc::clone(&asked)));
    let rpc = reno_rpc::Client::from_io(client_end, cancel.clone());
    (Client::connect(rpc).await.unwrap(), asked, cancel, server)
}

fn term() -> TermId {
    TermId::from_suffix("t1").unwrap()
}

#[tokio::test]
async fn a_cut_off_attach_comes_back_after_the_last_item_so_nothing_repeats_or_is_lost() {
    let (client, asked, cancel, server) = connect(2, 9).await;
    let mut out = client.term_attach(term(), None).await.unwrap();
    let mut seqs = Vec::new();
    while let Some(event) = out.next().await {
        let event = event.unwrap();
        assert!(matches!(event, TermEvent::Data { .. }));
        seqs.push(event.seq());
    }
    // Every item once, in order, over two cuts.
    assert_eq!(seqs, (1..=9).collect::<Vec<_>>());
    // Each attach asked for what followed the last item it had seen.
    assert_eq!(*asked.lock().unwrap(), vec![None, Some(2), Some(4)]);
    cancel.cancel();
    server.abort();
}

#[tokio::test]
async fn a_viewer_that_is_cut_off_again_and_again_but_keeps_progressing_is_not_ended() {
    let (client, _asked, cancel, server) = connect(u32::MAX, 9).await;
    let mut out = client.term_attach(term(), Some(5)).await.unwrap();
    // Items arrive between the cuts, so it keeps going; only gaps with nothing between them
    // would end it (the client does not end with a protocol error here). Read a bounded number.
    let mut seen = 0;
    while let Some(event) = out.next().await {
        event.unwrap();
        seen += 1;
        if seen == 12 {
            break;
        }
    }
    assert_eq!(seen, 12);
    cancel.cancel();
    server.abort();
}

#[tokio::test]
async fn the_client_hands_the_daemons_own_gap_notice_through() {
    // A `Gap` item is the daemon saying output was lost: it is delivered, and the next attach
    // after it starts before the item that followed it.
    let (client_end, server_end) = tokio::io::duplex(64 * 1024);
    let cancel = CancellationToken::new();
    let server = tokio::spawn(async move {
        let (read, mut write) = tokio::io::split(server_end);
        let mut lines = BufReader::new(read).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let frame: Value = serde_json::from_str(&line).unwrap();
            let id = frame["id"].clone();
            let out = match frame["method"].as_str() {
                Some("daemon.hello") => vec![json!({"jsonrpc": "2.0", "id": id, "result": {
                    "serverVersion": "t", "protocolVersion": PROTOCOL_VERSION,
                    "features": ["terminals"], "pid": 1}})],
                Some("term.attach") => vec![
                    json!({"jsonrpc": "2.0", "id": id, "result": {"subscription": "sub_g"}}),
                    json!({"jsonrpc": "2.0", "method": "sub.item", "params": {
                        "subscription": "sub_g", "seq": 40,
                        "item": {"type": "gap", "lostFrom": 1, "seq": 40}}}),
                    json!({"jsonrpc": "2.0", "method": "sub.item", "params": {
                        "subscription": "sub_g", "seq": 40, "item": data(40)}}),
                    json!({"jsonrpc": "2.0", "method": "sub.done",
                           "params": {"subscription": "sub_g"}}),
                ],
                _ => vec![],
            };
            for frame in out {
                write
                    .write_all(format!("{frame}\n").as_bytes())
                    .await
                    .unwrap();
            }
        }
    });
    let rpc = reno_rpc::Client::from_io(client_end, cancel.clone());
    let client = Client::connect(rpc).await.unwrap();
    let mut out = client.term_attach(term(), None).await.unwrap();
    let first = out.next().await.unwrap().unwrap();
    assert_eq!(
        first,
        TermEvent::Gap {
            lost_from: 1,
            seq: 40
        }
    );
    assert_eq!(out.next().await.unwrap().unwrap().seq(), 40);
    assert!(out.next().await.is_none());
    cancel.cancel();
    server.abort();
}
