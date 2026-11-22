//! The driver's end of the Reno gate: one Unix socket per attempt, one connection on it.
//!
//! The listener exists from before Pi is spawned until the first connection whose hello carries
//! the attempt's nonce. Then it is dropped and the socket file unlinked, so nothing can connect
//! afterwards, and the nonce (which Pi's own environment holds, and so a command Pi runs can
//! read) reaches no listener. A connection from another user is closed unread; one that says
//! hello with another nonce, no hello or no complete line fails the arming.
//!
//! After that the connection carries the gate's calls one way and the driver's decisions the
//! other. Each call is held open until it is decided exactly once; a gate that breaks the rules
//! (too many calls unanswered, a repeated id, a line that is not a call) is cut off, and Pi's
//! gate then blocks everything it is asked about.

use std::collections::HashSet;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::task::TaskTracker;

use crate::exec::Decision;

/// The longest hello line read. A real one is under a hundred bytes.
const MAX_HELLO: usize = 4096;

/// The longest call line read: a tool's whole input rides on it, a file's content included.
pub(super) const MAX_CALL_LINE: usize = 8 << 20;

/// How many calls the gate may leave unanswered. The gate extension holds its own count at this
/// number (its `MAX_OUTSTANDING`) and queues the rest itself, so only a gate that broke its rule
/// sends more.
pub(super) const MAX_OPEN_GATE_CALLS: usize = 16;

/// How many decisions wait for the writer: one for each call that may be open, and the denial of
/// the one past the bound.
const QUEUED_DECISIONS: usize = MAX_OPEN_GATE_CALLS + 1;

/// What a call that is decided without being raised is told.
const TOO_MANY_OPEN: &str = "Reno could not judge this call: too many calls were open at once.";

/// Why the gate was not armed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(super) enum ArmError {
    #[error("the gate connected but sent no hello")]
    NoHello,
    #[error("the gate's hello did not carry this attempt's nonce")]
    WrongNonce,
    #[error("the gate did not say hello in time")]
    Timeout,
}

/// One tool call Pi is about to run, as the gate sent it, held until it is decided.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct GateCall {
    pub(super) id: u64,
    pub(super) tool: String,
    pub(super) call_id: String,
    pub(super) parent: Option<String>,
    pub(super) input: Value,
    /// The gate had too many calls open: the host denied this one itself and the connection is
    /// broken. A decision for it is ignored, so raising it would only leave a request nobody
    /// can answer.
    pub(super) over_bound: bool,
}

#[derive(Deserialize)]
struct CallLine {
    t: String,
    id: u64,
    #[serde(rename = "toolName")]
    tool: String,
    #[serde(rename = "toolCallId")]
    call_id: String,
    #[serde(rename = "parentToolCallId", default)]
    parent: Option<String>,
    input: Value,
}

/// The read half of the gate's connection, which hands out whole lines.
#[derive(Debug)]
struct LineReader {
    half: OwnedReadHalf,
    buf: Vec<u8>,
    scanned: usize,
}

enum LineEnd {
    Closed,
    TooLong,
}

/// Why the connection was cut off or lost.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lost {
    Closed,
    TooLong,
    NotACall,
    RepeatedId,
    QueueFull,
}

impl Lost {
    fn text(self) -> &'static str {
        match self {
            Self::Closed => super::GATE_CLOSED,
            Self::TooLong => "the gate sent a line that is too long",
            Self::NotACall => "the gate sent a line that is not a call",
            Self::RepeatedId => "the gate repeated or reused a call id",
            Self::QueueFull => "the gate stopped taking Reno's decisions",
        }
    }
}

impl LineReader {
    fn new(half: OwnedReadHalf) -> Self {
        Self {
            half,
            buf: Vec::new(),
            scanned: 0,
        }
    }

    /// The next line without its newline. Cancel-safe: the only await is a read whose bytes go
    /// into the buffer this value keeps.
    async fn line(&mut self, max: usize) -> Result<Vec<u8>, LineEnd> {
        loop {
            if let Some(at) = self.buf[self.scanned..].iter().position(|&b| b == b'\n') {
                let at = self.scanned + at;
                self.scanned = 0;
                let mut line: Vec<u8> = self.buf.drain(..=at).collect();
                line.pop();
                return if line.len() > max {
                    Err(LineEnd::TooLong)
                } else {
                    Ok(line)
                };
            }
            self.scanned = self.buf.len();
            if self.buf.len() > max {
                return Err(LineEnd::TooLong);
            }
            let mut chunk = [0_u8; 8192];
            match self.half.read(&mut chunk).await {
                Ok(0) | Err(_) => return Err(LineEnd::Closed),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
            }
        }
    }
}

/// The accepted connection, before it is split between reading calls and writing decisions.
struct Connection {
    reader: LineReader,
    writer: OwnedWriteHalf,
}

/// The socket of one attempt, and, once armed, the gate's connection.
pub(super) struct GateHost {
    listener: Option<UnixListener>,
    path: PathBuf,
    nonce: String,
    owner: u32,
    tracker: TaskTracker,
    outcome: Option<Result<(), ArmError>>,
    reader: Option<LineReader>,
    /// Decisions on their way to the writer task: bounded, and never waited on.
    lines: Option<mpsc::Sender<Vec<u8>>>,
    writer: Option<JoinHandle<()>>,
    /// The calls read and not yet decided.
    open: HashSet<u64>,
    /// The connection broke the rules or was lost: nothing more is read.
    broken: bool,
    /// Why it was cut off, when it was not closed by us.
    lost: Option<Lost>,
    /// The highest call id seen: the gate numbers its calls upwards.
    highest: u64,
}

impl std::fmt::Debug for GateHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GateHost")
            .field("path", &self.path)
            .field("open", &self.open.len())
            .field("broken", &self.broken)
            .field("lost", &self.lost)
            .finish_non_exhaustive()
    }
}

impl GateHost {
    /// Listen on `path`, readable and writable by this user only. The directory holding it is
    /// the caller's private one. The writer task of the armed connection runs on `tracker`.
    pub(super) fn bind(path: &Path, nonce: &str, tracker: &TaskTracker) -> io::Result<Self> {
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let owner = std::fs::symlink_metadata(path)?.uid();
        Ok(Self {
            listener: Some(listener),
            path: path.to_path_buf(),
            nonce: nonce.to_owned(),
            owner,
            tracker: tracker.clone(),
            outcome: None,
            reader: None,
            lines: None,
            writer: None,
            open: HashSet::new(),
            broken: false,
            lost: None,
            highest: 0,
        })
    }

    /// Wait for the gate's connection and its hello. Done once: the same answer after that.
    /// The listener goes, and the socket file with it, whichever way this ends.
    pub(super) async fn arm(&mut self, budget: Duration) -> Result<(), ArmError> {
        if let Some(outcome) = self.outcome {
            return outcome;
        }
        let Some(listener) = self.listener.take() else {
            return Err(ArmError::NoHello);
        };
        let admitted =
            tokio::time::timeout(budget, admit(&listener, self.owner, &self.nonce, is_owner))
                .await
                .unwrap_or(Err(ArmError::Timeout));
        drop(listener);
        let _ = std::fs::remove_file(&self.path);
        let outcome = match admitted {
            Ok(Connection { reader, writer }) => {
                let (lines, queue) = mpsc::channel(QUEUED_DECISIONS);
                self.writer = Some(self.tracker.spawn(write_decisions(writer, queue)));
                self.lines = Some(lines);
                self.reader = Some(reader);
                Ok(())
            }
            Err(error) => Err(error),
        };
        self.outcome = Some(outcome);
        outcome
    }

    /// The gate's next call, or `None` once the connection is gone, broken, or was never armed.
    /// Cancel-safe: it sits in a `select!`.
    pub(super) async fn next(&mut self) -> Option<GateCall> {
        if self.broken {
            return None;
        }
        let line = match self.reader.as_mut()?.line(MAX_CALL_LINE).await {
            Ok(line) => line,
            Err(LineEnd::Closed) => return self.cut_off(Lost::Closed),
            Err(LineEnd::TooLong) => return self.cut_off(Lost::TooLong),
        };
        let Some(call) = parse_call(&line) else {
            return self.cut_off(Lost::NotACall);
        };
        if call.id <= self.highest {
            return self.cut_off(Lost::RepeatedId);
        }
        self.highest = call.id;
        if self.open.len() >= MAX_OPEN_GATE_CALLS {
            self.send(call.id, Decision::Deny, Some(TOO_MANY_OPEN));
            self.broken = true;
            return Some(GateCall {
                over_bound: true,
                ..call
            });
        }
        self.open.insert(call.id);
        Some(call)
    }

    /// Tell the gate what became of call `id`. Never blocks, and says nothing for a call that
    /// is not open: each call is decided once.
    pub(super) fn decide(&mut self, id: u64, decision: Decision, reason: Option<String>) {
        if self.open.remove(&id) {
            self.send(id, decision, reason.as_deref());
        }
    }

    /// Why the connection ended, for the run's diagnostic.
    pub(super) fn lost_because(&self) -> &'static str {
        self.lost.unwrap_or(Lost::Closed).text()
    }

    /// Whether the connection is usable: armed, and neither lost nor broken.
    pub(super) fn is_live(&self) -> bool {
        !self.broken && self.reader.is_some()
    }

    /// Let the decisions already queued reach the gate (up to `grace`), then close the
    /// connection and end the writer task.
    pub(super) async fn close(&mut self, grace: Duration) {
        self.lines = None;
        self.reader = None;
        self.broken = true;
        if let Some(mut writer) = self.writer.take()
            && tokio::time::timeout(grace, &mut writer).await.is_err()
        {
            writer.abort();
        }
    }

    fn send(&mut self, id: u64, decision: Decision, reason: Option<&str>) {
        let line = json!({
            "t": "decision",
            "id": id,
            "decision": match decision {
                Decision::Allow => "allow",
                Decision::Deny => "deny",
            },
            "reason": reason,
        });
        let mut bytes = line.to_string().into_bytes();
        bytes.push(b'\n');
        // A decision that cannot be queued is never dropped silently: the gate would wait on it
        // for ever, so the connection goes instead and the gate blocks everything.
        if self
            .lines
            .as_ref()
            .is_none_or(|lines| lines.try_send(bytes).is_err())
        {
            self.cut_off(Lost::QueueFull);
        }
    }

    /// Close the connection at once, without flushing. Always `None`, for `next`.
    fn cut_off(&mut self, why: Lost) -> Option<GateCall> {
        self.broken = true;
        self.lost.get_or_insert(why);
        self.reader = None;
        self.lines = None;
        if let Some(writer) = self.writer.take() {
            writer.abort();
        }
        None
    }

    #[cfg(test)]
    fn owned_by(mut self, uid: u32) -> Self {
        self.owner = uid;
        self
    }
}

impl Drop for GateHost {
    fn drop(&mut self) {
        if let Some(writer) = self.writer.take() {
            writer.abort();
        }
        // The listener closes with this value; the file is ours to remove.
        let _ = std::fs::remove_file(&self.path);
    }
}

fn parse_call(line: &[u8]) -> Option<GateCall> {
    let call: CallLine = serde_json::from_slice(line).ok()?;
    (call.t == "call").then_some(GateCall {
        id: call.id,
        tool: call.tool,
        call_id: call.call_id,
        parent: call.parent,
        input: call.input,
        over_bound: false,
    })
}

/// Write each queued decision, in order, until the queue closes or the gate stops reading, then
/// close the connection's write side.
async fn write_decisions(mut half: OwnedWriteHalf, mut queue: mpsc::Receiver<Vec<u8>>) {
    while let Some(line) = queue.recv().await {
        if half.write_all(&line).await.is_err() {
            return;
        }
    }
    let _ = half.shutdown().await;
}

fn is_owner(peer: u32, owner: u32) -> bool {
    peer == owner
}

/// The first connection from the owner, once it has said hello with `nonce`. Others are closed.
async fn admit(
    listener: &UnixListener,
    owner: u32,
    nonce: &str,
    same_user: fn(u32, u32) -> bool,
) -> Result<Connection, ArmError> {
    loop {
        let (stream, _) = listener.accept().await.map_err(|_| ArmError::NoHello)?;
        // A peer that cannot be identified (it already closed) is no hello, not a reason to wait.
        let peer = stream.peer_cred().map_err(|_| ArmError::NoHello)?;
        if same_user(peer.uid(), owner) {
            return hello(stream, nonce).await;
        }
    }
}

async fn hello(stream: UnixStream, nonce: &str) -> Result<Connection, ArmError> {
    let (read, writer) = stream.into_split();
    let mut reader = LineReader::new(read);
    let line = reader
        .line(MAX_HELLO)
        .await
        .map_err(|_| ArmError::NoHello)?;
    let hello: Value = serde_json::from_slice(&line).map_err(|_| ArmError::NoHello)?;
    if hello.get("t").and_then(Value::as_str) != Some("hello") {
        return Err(ArmError::NoHello);
    }
    match hello.get("nonce").and_then(Value::as_str) {
        Some(said) if said == nonce => Ok(Connection { reader, writer }),
        _ => Err(ArmError::WrongNonce),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use tokio::io::{AsyncBufReadExt, BufReader};

    use super::*;

    const NONCE: &str = "8f1c-test-nonce";

    fn hello_line(nonce: &str) -> Vec<u8> {
        format!("{{\"t\":\"hello\",\"nonce\":\"{nonce}\"}}\n").into_bytes()
    }

    struct Socket {
        dir: tempfile::TempDir,
    }

    impl Socket {
        fn new() -> Self {
            Self {
                dir: tempfile::tempdir().unwrap(),
            }
        }

        fn path(&self) -> PathBuf {
            self.dir.path().join("gate.sock")
        }

        fn host(&self) -> GateHost {
            GateHost::bind(&self.path(), NONCE, &TaskTracker::new()).unwrap()
        }

        async fn client(&self) -> UnixStream {
            UnixStream::connect(self.path()).await.unwrap()
        }
    }

    const PLENTY: Duration = Duration::from_secs(10);
    const SHORT: Duration = Duration::from_millis(150);

    /// Whether the daemon closed `client` without a word.
    async fn closed_unserved(client: &mut UnixStream) -> bool {
        let mut byte = [0_u8; 1];
        matches!(
            tokio::time::timeout(PLENTY, client.read(&mut byte)).await,
            Ok(Ok(0) | Err(_))
        )
    }

    #[tokio::test]
    async fn the_socket_is_the_owners_alone() {
        let socket = Socket::new();
        let _host = socket.host();
        let mode = std::fs::metadata(socket.path())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[tokio::test]
    async fn a_hello_with_the_nonce_arms_the_gate_and_the_socket_goes() {
        let socket = Socket::new();
        let mut host = socket.host();
        let mut gate = socket.client().await;
        gate.write_all(&hello_line(NONCE)).await.unwrap();
        host.arm(PLENTY).await.unwrap();
        assert!(!socket.path().exists());
        assert!(UnixStream::connect(socket.path()).await.is_err());
        host.arm(SHORT).await.unwrap();
    }

    #[tokio::test]
    async fn a_hello_with_another_nonce_is_refused() {
        let socket = Socket::new();
        let mut host = socket.host();
        let mut gate = socket.client().await;
        gate.write_all(&hello_line("another")).await.unwrap();
        assert_eq!(host.arm(PLENTY).await, Err(ArmError::WrongNonce));
        assert!(!socket.path().exists());
        assert_eq!(host.arm(PLENTY).await, Err(ArmError::WrongNonce));
    }

    #[tokio::test]
    async fn a_hello_without_a_nonce_is_refused() {
        let socket = Socket::new();
        let mut host = socket.host();
        let mut gate = socket.client().await;
        gate.write_all(b"{\"t\":\"hello\"}\n").await.unwrap();
        assert_eq!(host.arm(PLENTY).await, Err(ArmError::WrongNonce));
    }

    #[tokio::test]
    async fn a_connection_that_closes_without_a_hello_is_no_hello() {
        let socket = Socket::new();
        let mut host = socket.host();
        drop(socket.client().await);
        assert_eq!(host.arm(PLENTY).await, Err(ArmError::NoHello));
        assert!(!socket.path().exists());
    }

    #[tokio::test]
    async fn a_first_line_that_is_not_a_hello_is_no_hello() {
        let lines: [&[u8]; 4] = [
            b"not json\n",
            b"{\"t\":\"call\",\"id\":1}\n",
            b"{\"t\":\"hello\",\"nonce\":\"x\"}",
            b"[1]\n",
        ];
        for line in lines {
            let socket = Socket::new();
            let mut host = socket.host();
            let mut gate = socket.client().await;
            gate.write_all(line).await.unwrap();
            if line.ends_with(b"}") {
                gate.shutdown().await.unwrap();
            }
            assert_eq!(
                host.arm(PLENTY).await,
                Err(ArmError::NoHello),
                "{}",
                String::from_utf8_lossy(line)
            );
        }
    }

    #[tokio::test]
    async fn an_over_long_line_is_no_hello() {
        let socket = Socket::new();
        let mut host = socket.host();
        let mut gate = socket.client().await;
        gate.write_all(&vec![b'a'; MAX_HELLO + 100]).await.unwrap();
        assert_eq!(host.arm(PLENTY).await, Err(ArmError::NoHello));
    }

    #[tokio::test]
    async fn a_gate_that_never_connects_times_out_and_the_socket_goes() {
        let socket = Socket::new();
        let mut host = socket.host();
        assert_eq!(host.arm(SHORT).await, Err(ArmError::Timeout));
        assert!(!socket.path().exists());
        assert_eq!(host.arm(SHORT).await, Err(ArmError::Timeout));
    }

    #[tokio::test]
    async fn a_second_client_is_closed_with_no_decision_and_none_connects_once_armed() {
        let socket = Socket::new();
        let mut host = socket.host();
        let mut gate = socket.client().await;
        let mut second = socket.client().await;
        gate.write_all(&hello_line(NONCE)).await.unwrap();
        second.write_all(&hello_line(NONCE)).await.unwrap();
        host.arm(PLENTY).await.unwrap();
        assert!(closed_unserved(&mut second).await);
        assert!(UnixStream::connect(socket.path()).await.is_err());
    }

    #[tokio::test]
    async fn a_peer_of_another_user_is_closed_unread_and_does_not_arm() {
        let socket = Socket::new();
        let host = socket.host();
        let other = host.owner.wrapping_add(1);
        let mut host = host.owned_by(other);
        let mut stranger = socket.client().await;
        stranger.write_all(&hello_line(NONCE)).await.unwrap();
        assert_eq!(host.arm(SHORT).await, Err(ArmError::Timeout));
        assert!(closed_unserved(&mut stranger).await);
    }

    static SEEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// The first connection is judged another user's, every later one the owner's.
    fn only_the_first_is_a_stranger(_peer: u32, _owner: u32) -> bool {
        SEEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0
    }

    #[tokio::test]
    async fn a_peer_of_another_user_does_not_hide_the_owners_gate_behind_it() {
        let socket = Socket::new();
        let host = socket.host();
        let listener = host.listener.as_ref().unwrap();
        let mut stranger = socket.client().await;
        let mut gate = socket.client().await;
        gate.write_all(&hello_line(NONCE)).await.unwrap();
        let admitted = admit(listener, host.owner, NONCE, only_the_first_is_a_stranger).await;
        assert!(admitted.is_ok());
        assert!(closed_unserved(&mut stranger).await);
    }

    #[tokio::test]
    async fn the_hosts_debug_text_does_not_carry_the_nonce() {
        let socket = Socket::new();
        let host = socket.host();
        assert!(!format!("{host:?}").contains(NONCE));
    }

    #[tokio::test]
    async fn dropping_the_host_removes_the_socket() {
        let socket = Socket::new();
        let host = socket.host();
        assert!(socket.path().exists());
        drop(host);
        assert!(!socket.path().exists());
    }

    fn call_line(id: u64) -> Vec<u8> {
        format!(
            "{{\"t\":\"call\",\"id\":{id},\"toolName\":\"bash\",\"toolCallId\":\"c{id}\",\
             \"parentToolCallId\":null,\"input\":{{\"command\":\"ls\"}}}}\n"
        )
        .into_bytes()
    }

    /// An armed host and the gate's end of its connection.
    async fn armed(socket: &Socket) -> (GateHost, BufReader<UnixStream>) {
        let mut host = socket.host();
        let mut gate = socket.client().await;
        gate.write_all(&hello_line(NONCE)).await.unwrap();
        host.arm(PLENTY).await.unwrap();
        (host, BufReader::new(gate))
    }

    async fn reply(gate: &mut BufReader<UnixStream>) -> Option<Value> {
        let mut line = String::new();
        let read = tokio::time::timeout(PLENTY, gate.read_line(&mut line))
            .await
            .unwrap()
            .unwrap();
        (read > 0).then(|| serde_json::from_str(&line).unwrap())
    }

    #[tokio::test]
    async fn a_call_is_read_whole_and_decided_once() {
        let socket = Socket::new();
        let (mut host, mut gate) = armed(&socket).await;
        gate.get_mut().write_all(&call_line(1)).await.unwrap();
        let call = host.next().await.unwrap();
        assert_eq!(
            call,
            GateCall {
                id: 1,
                tool: "bash".into(),
                call_id: "c1".into(),
                parent: None,
                input: json!({"command": "ls"}),
                over_bound: false,
            }
        );
        host.decide(1, Decision::Deny, Some("no".into()));
        host.decide(1, Decision::Allow, None);
        host.decide(99, Decision::Allow, None);
        host.close(PLENTY).await;
        assert_eq!(
            reply(&mut gate).await.unwrap(),
            json!({"t": "decision", "id": 1, "decision": "deny", "reason": "no"})
        );
        assert_eq!(reply(&mut gate).await, None, "no second decision for id 1");
    }

    #[tokio::test]
    async fn a_call_with_a_parent_keeps_it_and_an_allow_carries_no_reason() {
        let socket = Socket::new();
        let (mut host, mut gate) = armed(&socket).await;
        let line = "{\"t\":\"call\",\"id\":7,\"toolName\":\"read\",\"toolCallId\":\"a/1\",\
                    \"parentToolCallId\":\"a\",\"input\":{}}\n";
        gate.get_mut().write_all(line.as_bytes()).await.unwrap();
        let call = host.next().await.unwrap();
        assert_eq!(call.parent.as_deref(), Some("a"));
        host.decide(7, Decision::Allow, None);
        assert_eq!(
            reply(&mut gate).await.unwrap(),
            json!({"t": "decision", "id": 7, "decision": "allow", "reason": null})
        );
    }

    #[tokio::test]
    async fn reading_a_call_survives_being_cancelled_part_way() {
        let socket = Socket::new();
        let (mut host, mut gate) = armed(&socket).await;
        let line = call_line(3);
        let (first, rest) = line.split_at(20);
        gate.get_mut().write_all(first).await.unwrap();
        let waited = tokio::time::timeout(SHORT, host.next()).await;
        assert!(waited.is_err(), "the line is not whole yet");
        gate.get_mut().write_all(rest).await.unwrap();
        assert_eq!(host.next().await.unwrap().id, 3);
    }

    #[tokio::test]
    async fn the_call_past_the_bound_is_denied_and_the_connection_is_cut() {
        let socket = Socket::new();
        let (mut host, mut gate) = armed(&socket).await;
        for id in 1..=MAX_OPEN_GATE_CALLS as u64 + 1 {
            gate.get_mut().write_all(&call_line(id)).await.unwrap();
        }
        for id in 1..=MAX_OPEN_GATE_CALLS as u64 {
            let call = host.next().await.unwrap();
            assert_eq!((call.id, call.over_bound), (id, false));
        }
        let excess = host.next().await.unwrap();
        assert_eq!((excess.id, excess.over_bound), (17, true));
        assert!(host.next().await.is_none());
        host.decide(17, Decision::Allow, None);
        for id in 1..=MAX_OPEN_GATE_CALLS as u64 {
            host.decide(id, Decision::Deny, None);
        }
        host.close(PLENTY).await;
        let denied = reply(&mut gate).await.unwrap();
        assert_eq!(
            (denied["id"].clone(), denied["decision"].clone()),
            (json!(17), json!("deny"))
        );
        assert!(denied["reason"].as_str().unwrap().contains("too many"));
        for id in 1..=MAX_OPEN_GATE_CALLS {
            assert_eq!(reply(&mut gate).await.unwrap()["id"], id, "none is lost");
        }
        assert_eq!(reply(&mut gate).await, None);
    }

    #[tokio::test]
    async fn a_decided_call_makes_room_for_another() {
        let socket = Socket::new();
        let (mut host, mut gate) = armed(&socket).await;
        for id in 1..=MAX_OPEN_GATE_CALLS as u64 {
            gate.get_mut().write_all(&call_line(id)).await.unwrap();
            host.next().await.unwrap();
        }
        host.decide(1, Decision::Allow, None);
        gate.get_mut().write_all(&call_line(17)).await.unwrap();
        let call = host.next().await.unwrap();
        assert_eq!((call.id, call.over_bound), (17, false));
    }

    #[tokio::test]
    async fn a_repeated_id_or_a_stray_line_cuts_the_gate_off_and_says_which() {
        let lines: [(Vec<u8>, &str); 4] = [
            ([call_line(1), call_line(1)].concat(), "repeated"),
            (b"not json\n".to_vec(), "not a call"),
            (hello_line(NONCE), "not a call"),
            (
                b"{\"t\":\"call\",\"id\":-1,\"toolName\":\"x\"}\n".to_vec(),
                "not a call",
            ),
        ];
        for (n, (bytes, cause)) in lines.iter().enumerate() {
            let socket = Socket::new();
            let (mut host, mut gate) = armed(&socket).await;
            gate.get_mut().write_all(bytes).await.unwrap();
            let mut ended = false;
            for _ in 0..2 {
                ended = host.next().await.is_none();
            }
            assert!(ended, "case {n}");
            assert!(!host.is_live(), "case {n}");
            assert!(host.lost_because().contains(cause), "case {n}");
            assert!(closed_unserved(gate.get_mut()).await, "case {n}");
        }
    }

    #[tokio::test]
    async fn an_id_that_was_decided_is_not_a_new_call_and_ids_only_rise() {
        let cases: [(&[u64], &str); 2] = [(&[1, 1], "after it was decided"), (&[5, 3], "lower")];
        for (ids, case) in cases {
            let socket = Socket::new();
            let (mut host, mut gate) = armed(&socket).await;
            gate.get_mut().write_all(&call_line(ids[0])).await.unwrap();
            let first = host.next().await.unwrap();
            host.decide(first.id, Decision::Allow, None);
            gate.get_mut().write_all(&call_line(ids[1])).await.unwrap();
            assert!(host.next().await.is_none(), "{case}");
            assert!(host.lost_because().contains("repeated"), "{case}");
        }
    }

    #[tokio::test]
    async fn an_over_long_call_line_cuts_the_gate_off() {
        let socket = Socket::new();
        let (mut host, gate) = armed(&socket).await;
        let mut stream = gate.into_inner();
        let flood = tokio::spawn(async move {
            let _ = stream.write_all(&vec![b'a'; MAX_CALL_LINE + 100]).await;
        });
        assert!(host.next().await.is_none());
        assert!(!host.is_live());
        assert!(host.lost_because().contains("too long"));
        flood.await.unwrap();
    }

    #[tokio::test]
    async fn a_gate_that_closes_ends_the_calls() {
        let socket = Socket::new();
        let (mut host, gate) = armed(&socket).await;
        drop(gate);
        assert!(host.next().await.is_none());
        assert!(!host.is_live());
        assert_eq!(host.lost_because(), "the gate's connection closed");
    }

    #[tokio::test]
    async fn closing_sends_what_was_queued_first() {
        let socket = Socket::new();
        let (mut host, mut gate) = armed(&socket).await;
        for id in 1..=3 {
            gate.get_mut().write_all(&call_line(id)).await.unwrap();
            host.next().await.unwrap();
        }
        for id in 1..=3 {
            host.decide(id, Decision::Deny, None);
        }
        host.close(PLENTY).await;
        for id in 1..=3 {
            assert_eq!(reply(&mut gate).await.unwrap()["id"], id);
        }
        assert_eq!(reply(&mut gate).await, None);
    }

    #[tokio::test]
    async fn a_host_that_was_never_armed_has_no_calls() {
        let socket = Socket::new();
        let mut host = socket.host();
        assert!(host.next().await.is_none());
        host.decide(1, Decision::Allow, None);
        assert!(!host.is_live());
    }
}
