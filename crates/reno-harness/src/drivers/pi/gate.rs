//! The driver's end of the Reno gate: one Unix socket per attempt, one connection on it.
//!
//! The listener exists from before Pi is spawned until the first connection whose hello carries
//! the attempt's nonce. Then it is dropped and the socket file unlinked, so nothing can connect
//! afterwards, and the nonce (which Pi's own environment holds, and so a command Pi runs can
//! read) reaches no listener. A connection from another user is closed unread; one that says
//! hello with another nonce, no hello or no complete line fails the arming.

use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

/// The longest hello line read. A real one is under a hundred bytes.
const MAX_HELLO: u64 = 4096;

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

/// The socket of one attempt, and, once armed, the gate's connection.
#[derive(Debug)]
pub(super) struct GateHost {
    listener: Option<UnixListener>,
    path: PathBuf,
    nonce: String,
    owner: u32,
    outcome: Option<Result<(), ArmError>>,
    /// Held so the gate's connection stays open for the attempt.
    _connection: Option<BufReader<UnixStream>>,
}

impl GateHost {
    /// Listen on `path`, readable and writable by this user only. The directory holding it is
    /// the caller's private one.
    pub(super) fn bind(path: &Path, nonce: &str) -> io::Result<Self> {
        let listener = UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        let owner = std::fs::symlink_metadata(path)?.uid();
        Ok(Self {
            listener: Some(listener),
            path: path.to_path_buf(),
            nonce: nonce.to_owned(),
            owner,
            outcome: None,
            _connection: None,
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
            Ok(connection) => {
                self._connection = Some(connection);
                Ok(())
            }
            Err(error) => Err(error),
        };
        self.outcome = Some(outcome);
        outcome
    }

    #[cfg(test)]
    fn owned_by(mut self, uid: u32) -> Self {
        self.owner = uid;
        self
    }
}

impl Drop for GateHost {
    fn drop(&mut self) {
        // The listener closes with this value; the file is ours to remove.
        let _ = std::fs::remove_file(&self.path);
    }
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
) -> Result<BufReader<UnixStream>, ArmError> {
    loop {
        let (stream, _) = listener.accept().await.map_err(|_| ArmError::NoHello)?;
        // A peer that cannot be identified (it already closed) is no hello, not a reason to wait.
        let peer = stream.peer_cred().map_err(|_| ArmError::NoHello)?;
        if same_user(peer.uid(), owner) {
            return hello(stream, nonce).await;
        }
    }
}

async fn hello(stream: UnixStream, nonce: &str) -> Result<BufReader<UnixStream>, ArmError> {
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let read = (&mut reader).take(MAX_HELLO).read_line(&mut line).await;
    if !matches!(read, Ok(n) if n > 0) || !line.ends_with('\n') {
        return Err(ArmError::NoHello);
    }
    let hello: Value = serde_json::from_str(&line).map_err(|_| ArmError::NoHello)?;
    if hello.get("t").and_then(Value::as_str) != Some("hello") {
        return Err(ArmError::NoHello);
    }
    match hello.get("nonce").and_then(Value::as_str) {
        Some(said) if said == nonce => Ok(reader),
        _ => Err(ArmError::WrongNonce),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use tokio::io::AsyncWriteExt;

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
            GateHost::bind(&self.path(), NONCE).unwrap()
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
        gate.write_all(&vec![b'a'; MAX_HELLO as usize + 100])
            .await
            .unwrap();
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

    #[test]
    fn the_uid_check_admits_the_owner_and_nobody_else() {
        assert!(is_owner(501, 501));
        assert!(!is_owner(502, 501));
        assert!(!is_owner(0, 501));
    }

    #[tokio::test]
    async fn dropping_the_host_removes_the_socket() {
        let socket = Socket::new();
        let host = socket.host();
        assert!(socket.path().exists());
        drop(host);
        assert!(!socket.path().exists());
    }
}
