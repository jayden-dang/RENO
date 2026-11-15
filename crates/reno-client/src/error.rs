//! The one error type of this crate (ARCH-14).

use std::path::PathBuf;

use reno_proto::{ErrorCode, ErrorObject};
use reno_rpc::RpcError;

/// Everything that can go wrong in `reno-client`.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The connection to the daemon closed, or was never usable.
    #[error("disconnected from the daemon")]
    Disconnected,
    /// Nothing is listening on the daemon's socket: the file is missing or the connection was
    /// refused.
    #[error("no daemon is listening on {}", .0.display())]
    NotRunning(PathBuf),
    /// The daemon speaks a protocol version this client cannot use.
    #[error(
        "incompatible daemon: client speaks protocol {client}, daemon answered {}",
        server.map_or_else(|| "with a version error".to_owned(), |v| v.to_string())
    )]
    Incompatible {
        /// The protocol version this client speaks.
        client: u32,
        /// The version the daemon reported, when it reported one.
        server: Option<u32>,
    },
    /// The daemon answered a request with an error. Boxed to keep `Result` small.
    #[error("daemon error {}: {}", .0.code.code(), .0.message)]
    Remote(Box<ErrorObject>),
    /// The daemon sent something the protocol does not allow: a malformed payload, a hole in a
    /// run's sequence numbers, or a subscription that makes no progress.
    #[error("protocol violation: {0}")]
    Protocol(String),
    /// A transport failure other than a plain disconnect (I/O, oversized frame).
    #[error("transport error: {0}")]
    Transport(String),
}

impl ClientError {
    /// The daemon's error code, when this error came from the daemon.
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Remote(obj) => Some(obj.code),
            _ => None,
        }
    }
}

impl From<RpcError> for ClientError {
    fn from(e: RpcError) -> Self {
        match e {
            RpcError::Disconnected => Self::Disconnected,
            RpcError::Remote(obj) if obj.code == ErrorCode::ProtocolVersionMismatch => {
                Self::Incompatible {
                    client: reno_proto::PROTOCOL_VERSION,
                    server: None,
                }
            }
            RpcError::Remote(obj) => Self::Remote(obj),
            RpcError::Protocol(msg) => Self::Protocol(msg),
            other => Self::Transport(other.to_string()),
        }
    }
}
