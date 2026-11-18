//! The one error type of the Terminal service, and what each case is on the wire (ARCH-14).

use reno_proto::{ErrorCode, ErrorObject, TermId};
use serde_json::json;

/// Everything that can go wrong serving a terminal request.
#[derive(Debug, thiserror::Error)]
pub enum TermError {
    /// The daemon is shutting down.
    #[error("the daemon is shutting down")]
    ShuttingDown,
    /// This daemon was started without a PTY spawner.
    #[error("this daemon was started without terminals")]
    Disabled,
    /// No such terminal: closed, reaped, or from before a daemon restart.
    #[error("terminal {0} not found")]
    NotFound(TermId),
    /// The daemon holds as many terminals as it will.
    #[error("the daemon holds {0} terminals already; close one")]
    TooMany(usize),
    /// The directory the shell would start in is gone.
    #[error("the folder for this terminal is unavailable: {0}")]
    FolderUnavailable(String),
    /// The shell would have to run under the sandbox and the sandbox cannot start, so no shell is
    /// opened: a terminal on a live run is never opened without its limits.
    #[error("a terminal on a live run needs the sandbox, which cannot start: {0}")]
    SandboxUnavailable(String),
    /// The shell could not be started.
    #[error("could not start a shell: {0}")]
    Spawn(String),
    /// The shell has exited, so it takes no input.
    #[error("terminal {0} has exited")]
    Exited(TermId),
    /// The input is not valid base64, or is longer than a write may be.
    #[error("invalid input: {0}")]
    BadInput(String),
    /// The terminal's input queue is full: the shell is not reading.
    #[error("terminal {0} is not reading its input; try again")]
    Backlog(TermId),
}

impl TermError {
    /// The code the error crosses the socket with.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Disabled | Self::ShuttingDown | Self::Backlog(_) => ErrorCode::Unavailable,
            Self::NotFound(_) => ErrorCode::NotFound,
            Self::TooMany(_) => ErrorCode::TerminalLimit,
            Self::FolderUnavailable(_) => ErrorCode::WorkspaceUnavailable,
            Self::SandboxUnavailable(_) => ErrorCode::SandboxUnavailable,
            Self::Spawn(_) => ErrorCode::Internal,
            Self::Exited(_) => ErrorCode::Conflict,
            Self::BadInput(_) => ErrorCode::InvalidParams,
        }
    }

    /// The error object, with a typed `kind` where a client branches.
    pub fn object(&self) -> ErrorObject {
        let code = self.code();
        if code == ErrorCode::Internal {
            tracing::error!(error = %self, "internal error while serving a terminal");
        }
        let kind = match self {
            Self::Exited(_) => Some("exited"),
            Self::Backlog(_) => Some("backlog"),
            Self::Disabled => Some("disabled"),
            _ => None,
        };
        let mut object = ErrorObject::new(code, self.to_string());
        object.data = kind.map(|kind| json!({ "kind": kind }));
        object
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_case_has_one_stable_code() {
        let id = TermId::from_suffix("a1").unwrap_or_else(|_| unreachable!());
        assert_eq!(TermError::NotFound(id.clone()).code(), ErrorCode::NotFound);
        assert_eq!(TermError::TooMany(16).code(), ErrorCode::TerminalLimit);
        assert_eq!(
            TermError::SandboxUnavailable("x".into()).code(),
            ErrorCode::SandboxUnavailable
        );
        assert_eq!(
            TermError::FolderUnavailable("x".into()).code(),
            ErrorCode::WorkspaceUnavailable
        );
        assert_eq!(TermError::Exited(id.clone()).code(), ErrorCode::Conflict);
        assert_eq!(
            TermError::BadInput("x".into()).code(),
            ErrorCode::InvalidParams
        );
        assert_eq!(TermError::Backlog(id).code(), ErrorCode::Unavailable);
        assert_eq!(TermError::Disabled.code(), ErrorCode::Unavailable);
        assert_eq!(TermError::ShuttingDown.code(), ErrorCode::Unavailable);
        assert_eq!(TermError::Spawn("x".into()).code(), ErrorCode::Internal);
    }
}
