//! The one public error type of this crate (ARCH-14).

use std::path::PathBuf;

use reno_proto::{ErrorCode, ErrorObject};

/// Everything that can go wrong in `reno-rpc`.
///
/// Errors that cross the socket map to an [`ErrorObject`] with a stable [`ErrorCode`] through
/// `From<RpcError> for ErrorObject`; an [`ErrorObject`] received from a peer becomes
/// [`RpcError::Remote`] through `From<ErrorObject> for RpcError`.
#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    /// The connection closed or was never usable. Every pending call fails with this when the
    /// peer goes away.
    #[error("connection closed")]
    Disconnected,
    /// The peer answered with a JSON-RPC error.
    #[error("remote error {}: {}", .0.code.code(), .0.message)]
    Remote(ErrorObject),
    /// The peer sent something that is not valid JSON-RPC, or a payload of the wrong shape.
    #[error("protocol violation: {0}")]
    Protocol(String),
    /// An outgoing frame is longer than the 8 MiB frame limit.
    #[error("frame of {len} bytes exceeds the {max} byte limit")]
    FrameTooLarge {
        /// Size of the rejected frame.
        len: usize,
        /// The limit.
        max: usize,
    },
    /// A socket or directory operation failed.
    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
    /// Another server is already listening on the socket path.
    #[error("a server is already running on {}", .0.display())]
    AlreadyRunning(PathBuf),
    /// The socket path or its directory is unsafe to use.
    #[error("unsafe socket location {}: {reason}", path.display())]
    Insecure {
        /// The offending path.
        path: PathBuf,
        /// What is wrong with it.
        reason: &'static str,
    },
}

impl From<ErrorObject> for RpcError {
    fn from(e: ErrorObject) -> Self {
        Self::Remote(e)
    }
}

impl From<RpcError> for ErrorObject {
    fn from(e: RpcError) -> Self {
        let code = match &e {
            RpcError::Remote(obj) => return obj.clone(),
            RpcError::Disconnected | RpcError::AlreadyRunning(_) => ErrorCode::Unavailable,
            RpcError::Protocol(_) | RpcError::FrameTooLarge { .. } => ErrorCode::InvalidRequest,
            RpcError::Io(_) | RpcError::Insecure { .. } => ErrorCode::Internal,
        };
        ErrorObject::new(code, e.to_string())
    }
}

impl From<serde_json::Error> for RpcError {
    fn from(e: serde_json::Error) -> Self {
        Self::Protocol(e.to_string())
    }
}
