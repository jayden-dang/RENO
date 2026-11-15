//! Minimal JSON-RPC 2.0 client over a child agent's stdio (newline-delimited
//! frames, id-multiplexed). Shared by the Codex app-server driver and the ACP
//! driver: both protocols are newline-framed JSON-RPC 2.0 over stdio.
//!
//! - Responses are matched to callers by numeric id (a shared pending map the
//!   reader task resolves directly, so requests can be awaited from anywhere —
//!   including inside the session loop — without starving notifications).
//! - Notifications and server→client requests (approvals) are pumped into an
//!   [`Incoming`] channel the session loop drains.
//! - Writes to a dead child's stdin (EPIPE) are tolerated and logged, but they
//!   close the client: every pending request fails at once instead of waiting
//!   for stdout EOF, which a descendant holding the pipe open can delay
//!   indefinitely.
//!
//! # Bounds and ownership (ARCH-11)
//!
//! Both queues are bounded. [`INCOMING_CAPACITY`] bounds what the reader hands the session
//! loop. [`WRITE_QUEUE_CAPACITY`] bounds the lines waiting for the child's stdin.
//!
//! The write side never parks its caller. The session loop that writes is the same loop that
//! drains [`Incoming`]; if it waited for stdin room while the child waited for stdout room,
//! the reader would block on a full `Incoming` and nothing would ever move again. So a caller
//! queues with `try_send`, and a queue that is full means the peer has stopped reading its
//! stdin: the client closes, every pending request fails, and the line is dropped. The reader
//! never touches the write queue, so a stalled writer cannot stop it from reading.
//!
//! The writer and reader tasks belong to the caller's [`TaskTracker`], so shutdown waits for
//! them, and to the client: they are aborted when the last clone of the client drops, or earlier
//! through [`RpcClient::abort`]. They end by themselves when the child's pipes close, but a
//! descendant that escaped teardown can hold a pipe open forever.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;
use tokio_util::task::TaskTracker;

use crate::HarnessError;

/// Lines that may wait for the child's stdin.
///
/// Every line is small (a reply, a notification or a request) and the queue only fills when the
/// child stops reading. A healthy run queues at most one reply per request the reader has handed
/// over ([`INCOMING_CAPACITY`]) plus a handful of requests of its own, so 1024 leaves a wide
/// margin over any burst while keeping the memory bound small (about a megabyte at the largest
/// realistic lines).
pub(crate) const WRITE_QUEUE_CAPACITY: usize = 1024;

/// Server-originated frames that may wait for the session loop.
pub(crate) const INCOMING_CAPACITY: usize = 256;

/// A non-response line from the app server, in stdout order.
#[derive(Debug)]
pub(crate) enum Incoming {
    Notification {
        method: String,
        params: Value,
    },
    /// Server→client request (approvals); must be answered via
    /// [`RpcClient::respond`] / [`RpcClient::respond_error`].
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// stdout EOF: the app server exited. All pending requests fail.
    Eof,
}

type StdoutObserver = Box<dyn Fn(&str) + Send>;

type Pending = Arc<Mutex<HashMap<i64, oneshot::Sender<Result<Value, String>>>>>;

/// How a request fails when the transport lost its answer: the app server
/// exited first, or its stdin could not take the request (then with the
/// write error after it). [`transport_lost`] recognizes both.
const EXITED: &str = "app-server exited before responding";
const STDIN_CLOSED: &str = "app-server stdin closed";
const QUEUE_FULL: &str = "app-server stdin closed (write queue full: the peer stopped reading)";

/// The request never got an answer because the app server is gone, as
/// opposed to an answer that refused it.
pub(crate) fn transport_lost(error: &HarnessError) -> bool {
    let HarnessError::Protocol(message) = error else {
        return false;
    };
    message
        .split_once(": ")
        .is_some_and(|(_, reason)| reason.starts_with(EXITED) || reason.starts_with(STDIN_CLOSED))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The writer and reader tasks of a client: aborted when the last clone of the client drops.
#[derive(Debug)]
struct OwnedTasks([AbortHandle; 2]);

impl OwnedTasks {
    fn abort(&self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

impl Drop for OwnedTasks {
    fn drop(&mut self) {
        self.abort();
    }
}

#[derive(Clone, Debug)]
pub(crate) struct RpcClient {
    next_id: Arc<AtomicI64>,
    pending: Pending,
    writer: mpsc::Sender<String>,
    closed: Arc<AtomicBool>,
    tasks: Arc<OwnedTasks>,
}

impl RpcClient {
    /// Spawn the writer and reader tasks over the child's stdio, on `tracker`, and return the
    /// client and the incoming (notification and request) channel.
    pub(crate) fn spawn<W, R>(
        tracker: &TaskTracker,
        stdin: W,
        stdout: R,
    ) -> (Self, mpsc::Receiver<Incoming>)
    where
        W: AsyncWrite + Send + Unpin + 'static,
        R: AsyncRead + Send + Unpin + 'static,
    {
        Self::spawn_observing(tracker, stdin, stdout, None)
    }

    /// [`Self::spawn`], with `observer` called for each sign-in link the agent prints on stdout.
    pub(crate) fn spawn_observing<W, R>(
        tracker: &TaskTracker,
        stdin: W,
        stdout: R,
        observer: Option<StdoutObserver>,
    ) -> (Self, mpsc::Receiver<Incoming>)
    where
        W: AsyncWrite + Send + Unpin + 'static,
        R: AsyncRead + Send + Unpin + 'static,
    {
        let (writer_tx, writer_rx) = mpsc::channel::<String>(WRITE_QUEUE_CAPACITY);
        let pending: Pending = Arc::default();
        let (incoming_tx, incoming_rx) = mpsc::channel(INCOMING_CAPACITY);
        let closed = Arc::new(AtomicBool::new(false));
        let writer = write_loop(stdin, writer_rx, Arc::clone(&pending), Arc::clone(&closed));
        let reader = read_loop(
            stdout,
            Arc::clone(&pending),
            incoming_tx,
            Arc::clone(&closed),
            observer,
        );
        let tasks = OwnedTasks([
            tracker.spawn(writer).abort_handle(),
            tracker.spawn(reader).abort_handle(),
        ]);
        (
            Self {
                next_id: Arc::new(AtomicI64::new(0)),
                pending,
                writer: writer_tx,
                closed,
                tasks: Arc::new(tasks),
            },
            incoming_rx,
        )
    }

    /// Stop both tasks now, for a child that is already reaped. Pending requests failed when
    /// the pipes closed; this only ends what a pipe held open by an escaped descendant would
    /// keep alive.
    pub(crate) fn abort(&self) {
        self.tasks.abort();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Send a request and await its response (resolved by the reader task).
    pub(crate) async fn request(&self, method: &str, params: Value) -> Result<Value, HarnessError> {
        self.request_now(method, params).await
    }

    /// [`Self::request`], but the line is queued for writing before this
    /// returns rather than on first poll — so a notification sent afterwards
    /// (a steer's `session/cancel`) can never overtake it on the wire.
    pub(crate) fn request_now(
        &self,
        method: &str,
        params: Value,
    ) -> futures::future::BoxFuture<'static, Result<Value, HarnessError>> {
        let method = method.to_owned();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = lock(&self.pending);
            // Check under the same lock as EOF cleanup: a request racing the
            // reader exit must either be rejected here or cleared by it.
            if self.is_closed() {
                return Box::pin(async move {
                    Err(HarnessError::Protocol(format!("{method}: {EXITED}")))
                });
            }
            pending.insert(id, tx);
        }
        let line = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        self.enqueue(line.to_string());
        Box::pin(async move {
            match rx.await {
                Ok(Ok(result)) => Ok(result),
                Ok(Err(message)) => Err(HarnessError::Protocol(format!("{method}: {message}"))),
                // Sender dropped: the reader hit EOF and failed all pending.
                Err(_) => Err(HarnessError::Protocol(format!("{method}: {EXITED}"))),
            }
        })
    }

    /// Fire a notification (no id, no response).
    pub(crate) fn notify(&self, method: &str, params: Option<Value>) {
        let line = match params {
            Some(params) => json!({ "jsonrpc": "2.0", "method": method, "params": params }),
            None => json!({ "jsonrpc": "2.0", "method": method }),
        };
        self.enqueue(line.to_string());
    }

    /// Answer a server→client request.
    pub(crate) fn respond(&self, id: &Value, result: Value) {
        let line = json!({ "jsonrpc": "2.0", "id": id, "result": result });
        self.enqueue(line.to_string());
    }

    /// Reject a server→client request (e.g. unknown method).
    pub(crate) fn respond_error(&self, id: &Value, code: i64, message: &str) {
        let line = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": code, "message": message },
        });
        self.enqueue(line.to_string());
    }

    /// Queue one line without waiting (see the module docs for why a caller never parks here).
    /// A closed client drops it; a full queue closes the client.
    fn enqueue(&self, line: String) {
        match self.writer.try_send(line) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(
                    target: "reno_harness::rpc",
                    "write queue full ({WRITE_QUEUE_CAPACITY} lines): the peer stopped reading stdin"
                );
                fail_pending(&self.pending, &self.closed, QUEUE_FULL);
            }
            // The writer ended: the client is already closed, and every pending request failed.
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

/// Owns the child's stdin; a write failure (EPIPE after the child died) is
/// tolerated and logged. Nothing written after it can be answered, so the
/// client closes and every pending request fails now: a caller must not wait
/// on stdout EOF, which a surviving descendant can hold off forever.
async fn write_loop<W: AsyncWrite + Unpin>(
    mut stdin: W,
    mut rx: mpsc::Receiver<String>,
    pending: Pending,
    closed: Arc<AtomicBool>,
) {
    loop {
        let Some(line) = rx.recv().await else { return };
        let write = async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        };
        if let Err(e) = write.await {
            tracing::debug!(target: "reno_harness::rpc", "stdin write failed (tolerated): {e}");
            fail_pending(&pending, &closed, &format!("{STDIN_CLOSED} ({e})"));
            return;
        }
    }
}

/// Close the client and reject every awaiting request with `reason`. Done
/// under the pending lock so a request racing it is either rejected by
/// [`RpcClient::request_now`]'s closed check or drained here.
fn fail_pending(pending: &Pending, closed: &AtomicBool, reason: &str) {
    let mut pending = lock(pending);
    closed.store(true, Ordering::Release);
    for (_, sender) in pending.drain() {
        let _ = sender.send(Err(reason.to_owned()));
    }
}

/// The id of a response, tolerantly. reno always sends numeric ids, but
/// JSON-RPC lets a server echo them re-encoded — a string `"5"` or float
/// `5.0` still names request 5. Dropping such a response would strand its
/// caller forever (the session would spin Working with no per-turn timeout).
fn response_id(id: &Value) -> Option<i64> {
    if let Some(n) = id.as_i64() {
        return Some(n);
    }
    if let Some(s) = id.as_str() {
        return s.parse().ok();
    }
    id.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)
}

/// Preserve the agent's rejection reason as well as the generic RPC label.
fn response_error(error: &Value) -> String {
    let Some(message) = error.get("message").and_then(Value::as_str) else {
        return error.to_string();
    };
    let mut rendered = message.to_owned();
    if let Some(code) = error.get("code").and_then(Value::as_i64) {
        rendered.push_str(&format!(" (code {code})"));
    }
    if let Some(data) = error.get("data").filter(|v| !v.is_null()) {
        let detail = data
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| data.to_string());
        if !detail.is_empty() {
            rendered.push_str(": ");
            rendered.push_str(&detail);
        }
    }
    rendered
}

/// Parse stdout lines: responses resolve the pending map, everything else is
/// forwarded in order. Non-JSON noise is skipped; on EOF all pending requests
/// fail (their senders drop) and one final [`Incoming::Eof`] is delivered.
async fn read_loop<R: AsyncRead + Unpin>(
    stdout: R,
    pending: Pending,
    tx: mpsc::Sender<Incoming>,
    closed: Arc<AtomicBool>,
    observer: Option<StdoutObserver>,
) {
    let mut lines = crate::lossy_lines::lossy_lines(stdout);
    // A read error ends the loop like EOF: either way the child's stdout is
    // unusable, pending requests must fail, and the session loop must know.
    while let Ok(Some(line)) = lines.next_line().await {
        let line = line.trim();
        if let Some(url) =
            line.strip_prefix("Open the following link to authenticate the ACP server: ")
        {
            if let Some(observer) = &observer {
                observer(url);
            }
            continue;
        }
        if line.is_empty() {
            continue;
        }
        let Ok(mut msg) = serde_json::from_str::<Value>(line) else {
            tracing::debug!(target: "reno_harness::rpc", "non-JSON stdout line (skipped)");
            continue;
        };
        if !msg.is_object() || msg.get("jsonrpc").is_some_and(|version| version != "2.0") {
            continue;
        }
        let method = msg.get("method").and_then(Value::as_str);
        let id = msg.get("id");
        match (method, id) {
            // Response: resolve the awaiting request.
            (None, Some(id)) => {
                // A bare `{jsonrpc, id}` is a JS server answering a void
                // method (`JSON.stringify` drops an undefined `result`);
                // skipping it would strand the caller, so it resolves as a
                // null result. A frame carrying anything else but neither
                // `result` nor `error` is diagnostic noise, not a response.
                let bare = msg
                    .as_object()
                    .is_some_and(|frame| frame.keys().all(|k| k == "jsonrpc" || k == "id"));
                if msg.get("result").is_none() && msg.get("error").is_none() && !bare {
                    continue;
                }
                let Some(id) = response_id(id) else { continue };
                let Some(sender) = lock(&pending).remove(&id) else {
                    continue;
                };
                let outcome = match msg.get("error") {
                    Some(err) => Err(response_error(err)),
                    None => Ok(msg
                        .get_mut("result")
                        .map(Value::take)
                        .unwrap_or(Value::Null)),
                };
                let _ = sender.send(outcome);
            }
            // Server→client request (approvals).
            (Some(method), Some(id)) => {
                let incoming = Incoming::Request {
                    id: id.clone(),
                    method: method.to_owned(),
                    params: msg
                        .get_mut("params")
                        .map(Value::take)
                        .unwrap_or(Value::Null),
                };
                if tx.send(incoming).await.is_err() {
                    return;
                }
            }
            // Notification.
            (Some(method), None) => {
                let incoming = Incoming::Notification {
                    method: method.to_owned(),
                    params: msg
                        .get_mut("params")
                        .map(Value::take)
                        .unwrap_or(Value::Null),
                };
                if tx.send(incoming).await.is_err() {
                    return;
                }
            }
            (None, None) => {}
        }
    }
    // EOF/read error: fail every awaiting request, then signal the loop.
    closed.store(true, Ordering::Release);
    lock(&pending).clear();
    let _ = tx.send(Incoming::Eof).await;
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use tokio::io::{AsyncBufReadExt, BufReader, DuplexStream};

    use super::*;

    /// A client over in-memory pipes. `from_client` yields what the client writes to the child's
    /// stdin; `to_client` is the child's stdout. A `stdin_buffer` of a few bytes makes the child
    /// a peer that stopped reading.
    struct Wire {
        client: RpcClient,
        incoming: mpsc::Receiver<Incoming>,
        tracker: TaskTracker,
        from_client: DuplexStream,
        to_client: DuplexStream,
    }

    fn wire(stdin_buffer: usize) -> Wire {
        let tracker = TaskTracker::new();
        let (stdin, from_client) = tokio::io::duplex(stdin_buffer);
        let (stdout, to_client) = tokio::io::duplex(64 * 1024);
        let (client, incoming) = RpcClient::spawn(&tracker, stdin, stdout);
        Wire {
            client,
            incoming,
            tracker,
            from_client,
            to_client,
        }
    }

    const PATIENCE: Duration = Duration::from_secs(5);

    #[tokio::test]
    async fn cancel_notification_wire_has_no_id() {
        let mut wire = wire(64 * 1024);
        wire.client
            .notify("session/cancel", Some(json!({"sessionId": "parent"})));
        let mut lines = BufReader::new(&mut wire.from_client).lines();
        let line = tokio::time::timeout(PATIENCE, lines.next_line())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let frame: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(
            frame,
            json!({"jsonrpc": "2.0", "method": "session/cancel", "params": {"sessionId": "parent"}})
        );
        assert!(frame.get("id").is_none());
        assert!(wire.client.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn requests_after_eof_fail_without_entering_pending_map() {
        let mut wire = wire(64 * 1024);
        drop(wire.to_client);
        let eof = tokio::time::timeout(PATIENCE, wire.incoming.recv())
            .await
            .unwrap();
        assert!(matches!(eof, Some(Incoming::Eof)));
        let result =
            tokio::time::timeout(PATIENCE, wire.client.request("session/prompt", json!({})))
                .await
                .expect("a request after EOF cannot wait for another EOF");
        assert!(result.unwrap_err().to_string().contains("exited"));
        assert!(wire.client.pending.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_peer_that_stops_reading_fails_requests_and_never_stalls_the_reader() {
        // A 16-byte pipe nobody drains: the writer blocks inside its first write.
        let mut wire = wire(16);
        let first = wire.client.request_now("session/prompt", json!({}));
        let filler = "x".repeat(256);
        for _ in 0..WRITE_QUEUE_CAPACITY + 8 {
            wire.client.notify("n", Some(json!({ "filler": filler })));
        }
        // The queue is bounded: overflowing it closed the client and failed what was waiting.
        assert!(wire.client.is_closed());
        let error = tokio::time::timeout(PATIENCE, first)
            .await
            .expect("overflow must fail the request, not strand it")
            .unwrap_err();
        assert!(transport_lost(&error), "{error}");
        assert!(wire.client.pending.lock().unwrap().is_empty());

        // The reader never waited on the writer: the child's frames still arrive, in order, more
        // of them than the write queue holds.
        let frames = 3 * INCOMING_CAPACITY;
        let mut sender = wire.to_client;
        let feed = tokio::spawn(async move {
            for n in 0..frames {
                let line = format!("{{\"jsonrpc\":\"2.0\",\"method\":\"tick\",\"params\":{n}}}\n");
                if sender.write_all(line.as_bytes()).await.is_err() {
                    return;
                }
            }
            drop(sender);
        });
        for n in 0..frames {
            let incoming = tokio::time::timeout(PATIENCE, wire.incoming.recv())
                .await
                .expect("a stalled writer must not stop the reader")
                .expect("the reader is alive");
            match incoming {
                Incoming::Notification { method, params } => {
                    assert_eq!(method, "tick");
                    assert_eq!(params, json!(n));
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        assert!(matches!(
            tokio::time::timeout(PATIENCE, wire.incoming.recv())
                .await
                .unwrap(),
            Some(Incoming::Eof)
        ));
        feed.await.unwrap();
    }

    #[tokio::test]
    async fn dropping_the_client_ends_its_tasks_even_when_the_pipes_stay_open() {
        let wire = wire(64 * 1024);
        // Both pipe ends stay open (a descendant holding them): only the drop can end the tasks.
        let Wire {
            client,
            incoming,
            tracker,
            from_client,
            to_client,
        } = wire;
        tracker.close();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), tracker.wait())
                .await
                .is_err(),
            "the tasks run while the client lives"
        );
        drop(client);
        tokio::time::timeout(PATIENCE, tracker.wait())
            .await
            .expect("dropping the last client aborts the writer and the reader");
        drop((incoming, from_client, to_client));
    }

    #[tokio::test]
    async fn a_clone_keeps_the_tasks_alive_until_the_last_one_drops() {
        let wire = wire(64 * 1024);
        let clone = wire.client.clone();
        drop(wire.client);
        wire.tracker.close();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), wire.tracker.wait())
                .await
                .is_err()
        );
        clone.abort();
        tokio::time::timeout(PATIENCE, wire.tracker.wait())
            .await
            .expect("abort ends both tasks while a clone lives");
        drop((wire.incoming, wire.from_client, wire.to_client));
    }

    /// A child speaking `script` over piped stdio, plus its client.
    #[cfg(unix)]
    fn scripted_child(
        script: &str,
    ) -> (
        tokio::process::Child,
        RpcClient,
        mpsc::Receiver<Incoming>,
        TaskTracker,
    ) {
        let mut child = tokio::process::Command::new("sh")
            .args(["-c", script])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let tracker = TaskTracker::new();
        let (client, incoming) = RpcClient::spawn(
            &tracker,
            child.stdin.take().unwrap(),
            child.stdout.take().unwrap(),
        );
        (child, client, incoming, tracker)
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn void_response_resolves_its_caller_with_null() {
        let (_child, client, _incoming, _tracker) =
            scripted_child(r#"read line; echo '{"jsonrpc":"2.0","id":1}'; sleep 5"#);
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            client.request("session/set_mode", json!({})),
        )
        .await
        .expect("a void response must not strand its caller");
        assert_eq!(result.unwrap(), Value::Null);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failed_write_fails_pending_requests_while_stdout_stays_open() {
        // The child closes stdin but keeps stdout open, like a crashed adapter
        // whose descendant still holds the pipe.
        let (_child, client, _incoming, _tracker) = scripted_child("exec 0<&-; sleep 5");
        tokio::time::sleep(Duration::from_millis(100)).await;
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            client.request("session/prompt", json!({})),
        )
        .await
        .expect("a failed write must fail the request without waiting for EOF");
        let error = result.unwrap_err();
        assert!(error.to_string().contains("stdin closed"));
        assert!(transport_lost(&error), "{error}");
        assert!(client.is_closed());
        assert!(client.pending.lock().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_lost_answer_reads_as_transport_lost() {
        // EOF with the request outstanding.
        let (_child, client, _incoming, _tracker) = scripted_child("read line");
        let error = tokio::time::timeout(
            Duration::from_secs(3),
            client.request("thread/start", json!({})),
        )
        .await
        .expect("EOF fails the request")
        .unwrap_err();
        assert!(transport_lost(&error), "{error}");
        // And once closed, a request is turned away the same way.
        let error = client.request("turn/start", json!({})).await.unwrap_err();
        assert!(transport_lost(&error), "{error}");
        for refused in [
            HarnessError::Protocol("thread/start: bad request (code -32600)".into()),
            HarnessError::Protocol(EXITED.into()),
            HarnessError::Io(std::io::Error::other(EXITED)),
        ] {
            assert!(!transport_lost(&refused), "{refused}");
        }
    }

    #[test]
    fn rpc_error_preserves_string_and_structured_details() {
        for data in [
            json!("A prompt is already running"),
            json!({"reason": "A prompt is already running"}),
            json!(["busy", 7]),
        ] {
            let rendered = response_error(
                &json!({"code": -32600, "message": "Invalid request", "data": data}),
            );
            assert!(rendered.starts_with("Invalid request (code -32600): "));
            assert!(rendered.ends_with(data.as_str().unwrap_or(&data.to_string())));
        }
    }

    #[test]
    fn rpc_error_handles_missing_null_and_unstructured_fields() {
        for error in [
            json!({"message": "busy"}),
            json!({"message": "busy", "data": null}),
            json!({"message": "busy", "data": ""}),
        ] {
            assert_eq!(response_error(&error), "busy");
        }
        for error in [
            json!({"code": -32600, "data": {"reason": "busy"}}),
            json!("unstructured error"),
        ] {
            assert_eq!(response_error(&error), error.to_string());
        }
    }
}
