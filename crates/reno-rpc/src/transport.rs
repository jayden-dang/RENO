//! Transports: a Unix domain socket server and client, and an in-memory duplex pair that runs
//! the exact same framing and dispatch (ARCHITECTURE 17.5).
//!
//! # Unix socket policy
//!
//! - The parent directory must exist with no group or other access (mode 0700) and be owned by
//!   this user; it is created with mode 0700 if missing.
//! - A socket file that already exists is probed by connecting. If the connect succeeds another
//!   server is running ([`RpcError::AlreadyRunning`]); if it is refused the file is stale and is
//!   removed. A path that is not a socket is never removed.
//! - The socket file is set to mode 0600. Between `bind` and `chmod` the file may be more open,
//!   but the 0700 directory keeps other users out.
//! - Every accepted connection is checked with `peer_cred()` before a byte is read; a peer whose
//!   uid differs from this process's uid is dropped.

use std::fs;
use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::io::DuplexStream;
use tokio::net::{UnixListener, UnixStream};
use tokio_util::sync::CancellationToken;

use crate::client::Client;
use crate::error::RpcError;
use crate::router::{PeerInfo, PeerKind};
use crate::server::Server;

/// A listening Unix socket that enforces the policy in the module docs.
#[derive(Debug)]
pub struct UnixServer {
    listener: UnixListener,
    path: PathBuf,
    uid: u32,
}

impl UnixServer {
    /// Binds `path`, applying the directory, stale-socket and mode rules.
    pub async fn bind(path: impl AsRef<Path>) -> Result<Self, RpcError> {
        let path = path.as_ref().to_path_buf();
        let uid = rustix::process::getuid().as_raw();
        prepare_dir(&path, uid)?;
        match fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_socket() => match UnixStream::connect(&path).await {
                Ok(_) => return Err(RpcError::AlreadyRunning(path)),
                Err(e) if is_stale(&e) => fs::remove_file(&path)?,
                Err(e) => return Err(e.into()),
            },
            Ok(_) => {
                return Err(RpcError::Insecure {
                    path,
                    reason: "exists and is not a socket",
                });
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let listener = UnixListener::bind(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
        Ok(Self {
            listener,
            path,
            uid,
        })
    }

    /// The socket path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Accepts the next connection from this user, silently dropping others.
    pub(crate) async fn accept(&self) -> io::Result<(UnixStream, PeerInfo)> {
        loop {
            let (stream, _) = self.listener.accept().await?;
            let cred = stream.peer_cred()?;
            if cred.uid() != self.uid {
                tracing::warn!(uid = cred.uid(), "rejecting connection from another user");
                continue;
            }
            let peer = PeerInfo {
                kind: PeerKind::Unix,
                uid: Some(cred.uid()),
                pid: cred.pid(),
            };
            return Ok((stream, peer));
        }
    }
}

impl Drop for UnixServer {
    fn drop(&mut self) {
        // Best effort: a leftover file would only be detected as stale by the next bind.
        let _ = fs::remove_file(&self.path);
    }
}

fn is_stale(e: &io::Error) -> bool {
    matches!(
        e.kind(),
        io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
    )
}

fn prepare_dir(path: &Path, uid: u32) -> Result<(), RpcError> {
    let dir = match path.parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir,
        _ => Path::new("."),
    };
    match fs::metadata(dir) {
        Ok(meta) => {
            if !meta.is_dir() {
                return Err(RpcError::Insecure {
                    path: dir.to_path_buf(),
                    reason: "parent is not a directory",
                });
            }
            if meta.uid() != uid {
                return Err(RpcError::Insecure {
                    path: dir.to_path_buf(),
                    reason: "parent directory is owned by another user",
                });
            }
            if meta.mode() & 0o077 != 0 {
                return Err(RpcError::Insecure {
                    path: dir.to_path_buf(),
                    reason: "parent directory must have mode 0700",
                });
            }
            Ok(())
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)
            .map_err(RpcError::from),
        Err(e) => Err(e.into()),
    }
}

/// Connects a [`Client`] to the Unix socket at `path`. The client's tasks stop when `cancel`
/// fires or the connection closes.
pub async fn connect_unix(
    path: impl AsRef<Path>,
    cancel: CancellationToken,
) -> Result<Client, RpcError> {
    let stream = UnixStream::connect(path).await?;
    Ok(Client::from_io(stream, cancel))
}

/// Default buffer of the in-memory pipe, per direction.
const MEMORY_BUFFER: usize = 64 * 1024;

/// Connects a [`Client`] to `server` over an in-memory duplex pair. The same framing and
/// dispatch run as on a socket. `cancel` stops both ends.
pub fn connect_in_memory(server: &Server, cancel: CancellationToken) -> Client {
    connect_in_memory_with_buffer(server, cancel, MEMORY_BUFFER)
}

/// Like [`connect_in_memory`], with an explicit pipe buffer in bytes. A small buffer makes the
/// server's writer block quickly, which is how tests provoke subscription overflow.
pub fn connect_in_memory_with_buffer(
    server: &Server,
    cancel: CancellationToken,
    buffer: usize,
) -> Client {
    Client::from_io(in_memory_raw(server, cancel.clone(), buffer), cancel)
}

/// Attaches the server to one end of an in-memory pipe and returns the other end, for tests that
/// speak raw frames.
pub fn in_memory_raw(server: &Server, cancel: CancellationToken, buffer: usize) -> DuplexStream {
    let (client_end, server_end) = tokio::io::duplex(buffer);
    server.serve_io(server_end, PeerInfo::in_memory(), cancel);
    client_end
}
