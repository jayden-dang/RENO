//! Behaviour tests. No network, no real daemon: in-memory pipes and a tempdir socket.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use futures::{StreamExt, stream};
use reno_proto::{
    DaemonStatus, DaemonStatusInfo, ErrorCode, ErrorObject, ExecEvent, NoParams, RunEventItem,
    RunEvents, RunGet, RunId, RunStatus, RunSummary, RunsSnapshot, RunsWatch, RunsWatchParams, Seq,
    SubGap, SubGapParams, SubscriptionId,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, ReadHalf, WriteHalf};
use tokio_util::sync::CancellationToken;

use crate::{
    Client, Router, RpcError, Server, ServerConfig, SubEvent, SubItem, UnixServer,
    connect_in_memory, connect_in_memory_with_buffer, connect_unix, in_memory_raw,
};

/// A tempdir with mode 0700, as the socket policy requires of the parent.
fn private_tempdir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).expect("chmod");
    dir
}

fn run_id(s: &str) -> RunId {
    RunId::from_suffix(s).expect("valid id")
}

fn summary(n: u64) -> RunSummary {
    RunSummary {
        model_options: Default::default(),
        effort: None,
        run_id: run_id("a"),
        task_id: "task_a".parse().expect("valid id"),
        executor: "codex".parse().expect("valid id"),
        status: RunStatus::Running,
        attempt: 1,
        title: format!("v{n}"),
        prompt: None,
        model: None,
        stop_reason: None,
        bot: None,
        cwd: None,
        created_at_ms: 0,
        updated_at_ms: n,
        stop: None,
        not_resumed: None,
        session: None,
        report: None,
    }
}

/// Counters the toy router exposes to tests.
#[derive(Default)]
struct Probes {
    stream_dropped: AtomicBool,
    notifications: AtomicUsize,
}

struct DropFlag(Arc<Probes>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.stream_dropped.store(true, Ordering::SeqCst);
    }
}

/// A toy router over real reno-proto methods.
///
/// `run.events` takes its length from the run id suffix: `n5` yields 5 items, `big` yields 1000,
/// `forever` never yields and never ends.
fn toy_router(probes: Arc<Probes>) -> Router {
    let p_events = Arc::clone(&probes);
    let p_notify = Arc::clone(&probes);
    Router::new()
        .method::<DaemonStatus, _, _>(|_: NoParams, ctx| async move {
            Ok(DaemonStatusInfo {
                version: "test".into(),
                pid: u32::try_from(ctx.conn_id()).unwrap_or(0),
                uptime_secs: 1,
                active_runs: 0,
                queued_runs: 0,
                executors: vec![],
                definitions_watch: None,
            })
        })
        .method::<RunGet, _, _>(|p, _| async move {
            if p.run_id == run_id("missing") {
                Err(ErrorObject::new(ErrorCode::NotFound, "no such run"))
            } else {
                Ok(summary(7))
            }
        })
        .subscription::<RunEvents, _, _, _>(move |p, _| {
            let probes = Arc::clone(&p_events);
            async move {
                let name = p.run_id.to_string();
                let from = p.from_seq.map_or(0, |s| s.0);
                let items = |n: u64| {
                    stream::iter((from..n).map(|i| {
                        SubItem::seq(
                            Seq(i),
                            RunEventItem {
                                seq: Seq(i),
                                event: ExecEvent::Text {
                                    text: format!("t{i}"),
                                },
                            },
                        )
                    }))
                };
                let guard = DropFlag(probes);
                match name.as_str() {
                    "run_forever" => Ok(stream::pending::<SubItem<RunEventItem>>()
                        .inspect(move |_| {
                            let _ = &guard;
                        })
                        .boxed()),
                    "run_big" => Ok(items(1000).boxed()),
                    _ => Ok(items(5).boxed()),
                }
            }
        })
        .subscription::<RunsWatch, _, _, _>(|_: RunsWatchParams, _| async move {
            Ok(stream::iter((0..1000).map(|i| {
                SubItem::latest(RunsSnapshot {
                    runs: vec![summary(i)],
                })
            })))
        })
        .notification::<SubGap, _, _>(move |_: SubGapParams, _| {
            let probes = Arc::clone(&p_notify);
            async move {
                probes.notifications.fetch_add(1, Ordering::SeqCst);
            }
        })
}

fn server() -> (Server, Arc<Probes>) {
    let probes = Arc::new(Probes::default());
    (Server::new(toy_router(Arc::clone(&probes))), probes)
}

/// A raw line-oriented peer over an in-memory pipe.
struct Raw {
    read: BufReader<ReadHalf<tokio::io::DuplexStream>>,
    write: WriteHalf<tokio::io::DuplexStream>,
}

impl Raw {
    fn connect(server: &Server, cancel: &CancellationToken, buffer: usize) -> Self {
        let io = in_memory_raw(server, cancel.clone(), buffer);
        let (read, write) = tokio::io::split(io);
        Self {
            read: BufReader::new(read),
            write,
        }
    }

    async fn send(&mut self, line: &str) {
        self.write.write_all(line.as_bytes()).await.expect("write");
        self.write.write_all(b"\n").await.expect("write");
    }

    /// The next frame, or `None` on EOF.
    async fn recv(&mut self) -> Option<Value> {
        let mut line = String::new();
        let n = self.read.read_line(&mut line).await.expect("read");
        (n > 0).then(|| serde_json::from_str(&line).expect("server sends valid json"))
    }

    async fn error_code(&mut self) -> (Value, i64) {
        let frame = self.recv().await.expect("a reply");
        (
            frame["id"].clone(),
            frame["error"]["code"].as_i64().expect("error code"),
        )
    }
}

fn request(id: u64, method: &str, params: &Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

#[tokio::test]
async fn typed_call_round_trip() {
    let (server, _) = server();
    let cancel = CancellationToken::new();
    let client = connect_in_memory(&server, cancel.clone());

    let status = client
        .call::<DaemonStatus>(NoParams {})
        .await
        .expect("status");
    assert_eq!(status.version, "test");
    let run = client
        .call::<RunGet>(reno_proto::RunGetParams {
            run_id: run_id("a"),
        })
        .await
        .expect("run");
    assert_eq!(run.updated_at_ms, 7);

    let err = client
        .call::<RunGet>(reno_proto::RunGetParams {
            run_id: run_id("missing"),
        })
        .await
        .expect_err("not found");
    match err {
        RpcError::Remote(obj) => assert_eq!(obj.code, ErrorCode::NotFound),
        other => panic!("unexpected {other:?}"),
    }
    let obj: ErrorObject = RpcError::Disconnected.into();
    assert_eq!(obj.code, ErrorCode::Unavailable);
    cancel.cancel();
}

#[tokio::test]
async fn notify_reaches_the_handler() {
    let (server, probes) = server();
    let cancel = CancellationToken::new();
    let client = connect_in_memory(&server, cancel.clone());
    client
        .notify::<SubGap>(SubGapParams {
            subscription: SubscriptionId::from_suffix("x").expect("id"),
            resume_from: Seq(1),
        })
        .await
        .expect("notify");
    // A call after the notify is processed after it was read; poll the counter via a round trip.
    client
        .call::<DaemonStatus>(NoParams {})
        .await
        .expect("status");
    tokio::task::yield_now().await;
    assert_eq!(probes.notifications.load(Ordering::SeqCst), 1);
    cancel.cancel();
}

#[tokio::test]
async fn bad_frames_get_the_right_codes_and_the_connection_survives() {
    let (server, _) = server();
    let cancel = CancellationToken::new();
    let mut raw = Raw::connect(&server, &cancel, 64 * 1024);

    raw.send(&request(1, "no.such", &json!({}))).await;
    assert_eq!(raw.error_code().await, (json!(1), -32601));

    raw.send("{this is not json").await;
    assert_eq!(raw.error_code().await, (Value::Null, -32700));

    raw.send(&json!([request_value(2), request_value(3)]).to_string())
        .await;
    assert_eq!(raw.error_code().await, (Value::Null, -32600));

    raw.send(&request(4, "run.get", &json!({"runId": "not-an-id"})))
        .await;
    assert_eq!(raw.error_code().await, (json!(4), -32602));

    raw.send(&json!({"jsonrpc": "1.0", "id": 5, "method": "daemon.status"}).to_string())
        .await;
    assert_eq!(raw.error_code().await, (json!(5), -32600));

    // Still alive after all of that.
    raw.send(&request(6, "daemon.status", &json!({}))).await;
    let ok = raw.recv().await.expect("reply");
    assert_eq!(ok["id"], json!(6));
    assert_eq!(ok["result"]["version"], json!("test"));
    cancel.cancel();
}

fn request_value(id: u64) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "daemon.status", "params": {}})
}

#[tokio::test]
async fn oversized_line_is_rejected_and_the_connection_closes() {
    let (server, _) = server();
    let cancel = CancellationToken::new();
    let mut raw = Raw::connect(&server, &cancel, 64 * 1024);
    raw.send(&request(1, "daemon.status", &json!({}))).await;
    assert!(raw.recv().await.is_some());

    // Write the oversized line concurrently, since the pipe buffer is small.
    let big = "x".repeat(crate::frame::MAX_LINE + 1024);
    let Raw {
        mut read,
        mut write,
    } = raw;
    let writer = tokio::spawn(async move {
        let _ = write.write_all(big.as_bytes()).await;
        let _ = write.write_all(b"\n").await;
        write
    });
    let mut line = String::new();
    read.read_line(&mut line).await.expect("read");
    let frame: Value = serde_json::from_str(&line).expect("json");
    assert_eq!(frame["error"]["code"], json!(-32600));
    assert_eq!(frame["id"], Value::Null);
    // Documented behaviour: the connection then closes.
    line.clear();
    assert_eq!(read.read_line(&mut line).await.expect("read"), 0);
    drop(writer);
    cancel.cancel();
}

#[tokio::test]
async fn a_frame_split_across_many_writes_still_parses() {
    let (server, _) = server();
    let cancel = CancellationToken::new();
    let mut raw = Raw::connect(&server, &cancel, 64 * 1024);
    let line = request(9, "daemon.status", &json!({}));
    for chunk in line.as_bytes().chunks(3) {
        raw.write.write_all(chunk).await.expect("write");
        raw.write.flush().await.expect("flush");
        tokio::task::yield_now().await;
    }
    raw.write.write_all(b"\n").await.expect("write");
    let reply = raw.recv().await.expect("reply");
    assert_eq!(reply["id"], json!(9));
    assert!(reply["result"].is_object());
    cancel.cancel();
}

#[tokio::test]
async fn subscription_delivers_in_order_then_done() {
    let (server, _) = server();
    let cancel = CancellationToken::new();
    let client = connect_in_memory(&server, cancel.clone());
    let mut sub = client
        .subscribe::<RunEvents>(reno_proto::RunEventsParams {
            run_id: run_id("n5"),
            from_seq: None,
        })
        .await
        .expect("subscribe");
    let mut seqs = vec![];
    loop {
        match sub.next().await.expect("event") {
            SubEvent::Item(item) => seqs.push(item.seq.0),
            SubEvent::Done => break,
            SubEvent::Gap { .. } => panic!("unexpected gap"),
        }
    }
    assert_eq!(seqs, [0, 1, 2, 3, 4]);
    assert!(sub.next().await.is_none());

    // Resume: from_seq is the handler's business, and the subscription starts afresh.
    let mut sub = client
        .subscribe::<RunEvents>(reno_proto::RunEventsParams {
            run_id: run_id("n5"),
            from_seq: Some(Seq(3)),
        })
        .await
        .expect("subscribe");
    let first = sub.next().await.expect("event");
    assert!(matches!(first, SubEvent::Item(i) if i.seq == Seq(3)));
    cancel.cancel();
}

#[tokio::test]
async fn sub_cancel_stops_the_subscription() {
    let (server, probes) = server();
    let cancel = CancellationToken::new();
    let mut raw = Raw::connect(&server, &cancel, 64 * 1024);
    raw.send(&request(1, "run.events", &json!({"runId": "run_forever"})))
        .await;
    let reply = raw.recv().await.expect("reply");
    let sub = reply["result"]["subscription"].clone();
    assert!(sub.is_string());
    assert!(!probes.stream_dropped.load(Ordering::SeqCst));

    raw.send(&request(2, "sub.cancel", &json!({"subscription": sub})))
        .await;
    let ack = raw.recv().await.expect("ack");
    assert_eq!(ack["id"], json!(2));
    assert!(ack["result"].is_object());
    // The pump is a separate task: wait for it to drop the handler's stream.
    server.tracker().close();
    raw.send(&request(3, "sub.cancel", &json!({"subscription": sub})))
        .await;
    assert_eq!(raw.error_code().await, (json!(3), -32001));
    for _ in 0..20 {
        if probes.stream_dropped.load(Ordering::SeqCst) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(probes.stream_dropped.load(Ordering::SeqCst));
    cancel.cancel();
}

#[tokio::test]
async fn dropping_a_sub_stream_cancels_it_on_the_server() {
    let (server, probes) = server();
    let cancel = CancellationToken::new();
    let client = connect_in_memory(&server, cancel.clone());
    let sub = client
        .subscribe::<RunEvents>(reno_proto::RunEventsParams {
            run_id: run_id("forever"),
            from_seq: None,
        })
        .await
        .expect("subscribe");
    drop(sub);
    for _ in 0..50 {
        if probes.stream_dropped.load(Ordering::SeqCst) {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(probes.stream_dropped.load(Ordering::SeqCst));
    cancel.cancel();
}

#[tokio::test]
async fn slow_consumer_of_a_seq_subscription_gets_a_gap_and_memory_stays_bounded() {
    let (server, _) = server();
    let cancel = CancellationToken::new();
    // A tiny pipe and a peer that is not reading: the writer blocks at once.
    let mut raw = Raw::connect(&server, &cancel, 256);
    raw.send(&request(1, "run.events", &json!({"runId": "run_big"})))
        .await;
    let first = raw.recv().await.expect("reply");
    assert!(first["result"]["subscription"].is_string());

    let mut seqs = vec![];
    let resume_from = loop {
        let frame = raw.recv().await.expect("frame");
        match frame["method"].as_str() {
            Some("sub.item") => seqs.push(frame["params"]["seq"].as_u64().expect("seq")),
            Some("sub.gap") => break frame["params"]["resumeFrom"].as_u64().expect("resume"),
            other => panic!("unexpected {other:?}"),
        }
    };
    // Every delivered item is contiguous from 0, and resume is the next undelivered seq.
    let expected: Vec<u64> = (0..resume_from).collect();
    assert_eq!(seqs, expected);
    assert!(resume_from < 1000, "the source must have been cut off");
    assert!(
        resume_from >= 256,
        "the queue was full before the cut: {resume_from}"
    );

    let stats = server.stats();
    assert_eq!(stats.gaps(), 1);
    assert!(stats.max_queue_len() <= ServerConfig::default().sub_queue_capacity);
    assert_eq!(stats.max_queue_len(), 256);
    cancel.cancel();
}

#[tokio::test]
async fn a_client_that_falls_behind_sees_gap_with_a_resume_point() {
    let (server, _) = server();
    let cancel = CancellationToken::new();
    let client = connect_in_memory_with_buffer(&server, cancel.clone(), 1024);
    let mut sub = client
        .subscribe::<RunEvents>(reno_proto::RunEventsParams {
            run_id: run_id("big"),
            from_seq: None,
        })
        .await
        .expect("subscribe");
    // Do not poll the stream until the whole burst has been routed.
    for _ in 0..200 {
        tokio::task::yield_now().await;
    }
    let mut next_expected = 0;
    loop {
        match sub.next().await {
            Some(SubEvent::Item(item)) => {
                assert_eq!(item.seq, Seq(next_expected));
                next_expected += 1;
            }
            Some(SubEvent::Gap { resume_from }) => {
                assert_eq!(resume_from, Seq(next_expected));
                break;
            }
            Some(SubEvent::Done) | None => panic!("must end in a gap"),
        }
    }
    assert!(next_expected < 1000);
    cancel.cancel();
}

#[tokio::test]
async fn latest_value_subscription_coalesces_and_keeps_the_newest() {
    let (server, _) = server();
    let cancel = CancellationToken::new();
    let mut raw = Raw::connect(&server, &cancel, 256);
    raw.send(&request(1, "runs.watch", &json!({}))).await;
    let _ack = raw.recv().await.expect("reply");

    let mut titles = vec![];
    loop {
        let frame = raw.recv().await.expect("frame");
        match frame["method"].as_str() {
            Some("sub.item") => {
                assert!(frame["params"].get("seq").is_none());
                titles.push(
                    frame["params"]["item"]["runs"][0]["updatedAtMs"]
                        .as_u64()
                        .expect("updatedAtMs"),
                );
            }
            Some("sub.done") => break,
            other => panic!("unexpected {other:?}"),
        }
    }
    assert_eq!(
        titles.last(),
        Some(&999),
        "the newest value is always delivered"
    );
    assert!(titles.windows(2).all(|w| w[0] < w[1]), "order is preserved");
    assert!(titles.len() < 1000, "some values were coalesced");
    assert!(server.stats().coalesced() > 0);
    assert_eq!(server.stats().gaps(), 0);
    assert!(server.stats().max_queue_len() <= 256);
    cancel.cancel();
}

#[tokio::test]
async fn pending_calls_fail_with_disconnected_when_the_server_goes_away() {
    // A peer that never answers.
    let (client_end, server_end) = tokio::io::duplex(1024);
    let cancel = CancellationToken::new();
    let client = Client::from_io(client_end, cancel.clone());
    let pending = {
        let client = client.clone();
        tokio::spawn(async move { client.call::<DaemonStatus>(NoParams {}).await })
    };
    for _ in 0..10 {
        tokio::task::yield_now().await;
    }
    drop(server_end);
    let err = pending.await.expect("join").expect_err("must fail");
    assert!(matches!(err, RpcError::Disconnected), "{err:?}");
    let err = client
        .call::<DaemonStatus>(NoParams {})
        .await
        .expect_err("closed");
    assert!(matches!(err, RpcError::Disconnected), "{err:?}");
    client.close().await;
}

#[tokio::test]
async fn cancelling_the_server_returns_promptly_and_empties_the_tracker() {
    let (server, _) = server();
    let server = Arc::new(server);
    let dir = private_tempdir();
    let path = dir.path().join("reno.sock");
    let listener = UnixServer::bind(&path).await.expect("bind");
    let cancel = CancellationToken::new();
    let serving = {
        let server = Arc::clone(&server);
        let cancel = cancel.clone();
        tokio::spawn(async move { server.serve(listener, cancel).await })
    };
    let client = connect_unix(&path, CancellationToken::new())
        .await
        .expect("connect");
    client
        .call::<DaemonStatus>(NoParams {})
        .await
        .expect("status");
    let _sub = client
        .subscribe::<RunEvents>(reno_proto::RunEventsParams {
            run_id: run_id("forever"),
            from_seq: None,
        })
        .await
        .expect("subscribe");

    cancel.cancel();
    serving.await.expect("join").expect("serve");
    assert!(server.tracker().is_empty());
    assert!(!path.exists(), "the socket file is removed on shutdown");
    let err = client.call::<DaemonStatus>(NoParams {}).await;
    assert!(matches!(err, Err(RpcError::Disconnected)));
}

#[tokio::test]
async fn stopping_accept_closes_the_socket_but_open_connections_carry_on() {
    let (server, _) = server();
    let server = Arc::new(server);
    let dir = private_tempdir();
    let path = dir.path().join("reno.sock");
    let listener = UnixServer::bind(&path).await.expect("bind");
    let (stop_accepting, cancel) = (CancellationToken::new(), CancellationToken::new());
    let serving = {
        let server = Arc::clone(&server);
        let (stop_accepting, cancel) = (stop_accepting.clone(), cancel.clone());
        tokio::spawn(async move { server.serve_until(listener, stop_accepting, cancel).await })
    };
    let client = connect_unix(&path, CancellationToken::new())
        .await
        .expect("connect");
    client
        .call::<DaemonStatus>(NoParams {})
        .await
        .expect("status");

    stop_accepting.cancel();
    // The listener is dropped inside `serve_until`; wait for the file to go without polling.
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while path.exists() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the socket file goes once accepting stops");
    assert!(
        connect_unix(&path, CancellationToken::new()).await.is_err(),
        "nobody new can connect"
    );
    client
        .call::<DaemonStatus>(NoParams {})
        .await
        .expect("an open connection still works");
    assert!(!serving.is_finished(), "serve waits for the connections");

    cancel.cancel();
    serving.await.expect("join").expect("serve");
    assert!(server.tracker().is_empty());
}

#[tokio::test]
async fn unix_socket_policy_and_round_trip() {
    let (server, _) = server();
    let server = Arc::new(server);
    let dir = private_tempdir();
    // The parent does not exist yet: it is created with mode 0700.
    let path = dir.path().join("run").join("reno.sock");
    let listener = UnixServer::bind(&path).await.expect("bind");
    let mode =
        |p: &std::path::Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
    assert_eq!(mode(&path), 0o600);
    assert_eq!(mode(path.parent().expect("parent")), 0o700);

    // A live server is detected.
    match UnixServer::bind(&path).await {
        Err(RpcError::AlreadyRunning(p)) => assert_eq!(p, path),
        other => panic!("expected AlreadyRunning, got {other:?}"),
    }

    let cancel = CancellationToken::new();
    let serving = {
        let server = Arc::clone(&server);
        let cancel = cancel.clone();
        tokio::spawn(async move { server.serve(listener, cancel).await })
    };
    let client = connect_unix(&path, CancellationToken::new())
        .await
        .expect("connect");
    let status = client
        .call::<DaemonStatus>(NoParams {})
        .await
        .expect("status");
    assert_eq!(status.version, "test");
    cancel.cancel();
    serving.await.expect("join").expect("serve");
}

#[tokio::test]
async fn a_stale_socket_file_is_removed() {
    let dir = private_tempdir();
    let path = dir.path().join("reno.sock");
    // Bind with std and leak the file by forgetting the listener's cleanup: closing a Unix
    // listener does not unlink its path.
    drop(std::os::unix::net::UnixListener::bind(&path).expect("std bind"));
    assert!(path.exists());
    let listener = UnixServer::bind(&path)
        .await
        .expect("stale socket is replaced");
    assert_eq!(listener.path(), path);
}

#[tokio::test]
async fn a_regular_file_at_the_socket_path_is_never_removed() {
    let dir = private_tempdir();
    let path = dir.path().join("reno.sock");
    std::fs::write(&path, b"precious").expect("write");
    assert!(matches!(
        UnixServer::bind(&path).await,
        Err(RpcError::Insecure { .. })
    ));
    assert!(path.exists());
}

#[tokio::test]
async fn an_open_parent_directory_is_refused() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let err = UnixServer::bind(dir.path().join("reno.sock")).await;
    assert!(matches!(err, Err(RpcError::Insecure { .. })), "{err:?}");
}

/// Connection state and the guard: a handler marks its connection, and from then on the guard
/// refuses the methods that mark does not allow, on that connection only.
#[tokio::test]
async fn a_guard_sees_the_state_a_handler_set_and_refuses_on_that_connection_only() {
    #[derive(Debug, PartialEq)]
    struct Marked;

    let router = Router::new()
        .method::<DaemonStatus, _, _>(|_: NoParams, ctx| async move {
            // `daemon.status` marks the connection (a stand-in for an authentication).
            assert!(ctx.set_state(Marked));
            // A second value is refused and the first stays.
            assert!(!ctx.set_state(7_u32));
            assert!(ctx.state::<u32>().is_none());
            Ok(DaemonStatusInfo {
                version: "test".into(),
                pid: 0,
                uptime_secs: 0,
                active_runs: 0,
                queued_runs: 0,
                executors: vec![],
                definitions_watch: None,
            })
        })
        .method::<RunGet, _, _>(|_, _| async move {
            Err(ErrorObject::new(ErrorCode::NotFound, "no such run"))
        })
        .guard(|ctx, method| {
            if ctx.state::<Marked>().is_some() && method != "daemon.status" {
                return Err(ErrorObject::new(ErrorCode::Unauthorized, "marked"));
            }
            Ok(())
        });
    let server = Server::new(router);
    let cancel = CancellationToken::new();
    let marked = connect_in_memory(&server, cancel.clone());
    let other = connect_in_memory(&server, cancel.clone());
    let get = || reno_proto::RunGetParams {
        run_id: run_id("a"),
    };

    // Before the mark, `run.get` reaches its handler.
    let error = marked.call::<RunGet>(get()).await.expect_err("not found");
    assert!(matches!(error, RpcError::Remote(e) if e.code == ErrorCode::NotFound));
    marked
        .call::<DaemonStatus>(NoParams {})
        .await
        .expect("marks");
    // After it, the guard answers instead of the handler.
    let error = marked.call::<RunGet>(get()).await.expect_err("guarded");
    assert!(matches!(error, RpcError::Remote(e) if e.code == ErrorCode::Unauthorized));
    // Another connection is untouched.
    let error = other.call::<RunGet>(get()).await.expect_err("not found");
    assert!(matches!(error, RpcError::Remote(e) if e.code == ErrorCode::NotFound));
    cancel.cancel();
}
