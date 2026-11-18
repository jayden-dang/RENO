//! The one error type of the Files service, and what each case is on the wire (ARCH-14).

use reno_proto::{ErrorCode, ErrorObject};
use serde_json::json;

/// Why a path was refused by the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// It has a `..` component.
    Traversal,
    /// It is absolute, or has a drive or root component.
    Absolute,
    /// It leaves the root through a symbolic link.
    SymlinkEscape,
    /// It is, or is inside, `.git`.
    GitInternals,
    /// It is malformed: a NUL byte, or too long.
    Malformed,
}

impl Rejection {
    /// The stable word `data.kind` carries.
    pub fn word(self) -> &'static str {
        match self {
            Self::Traversal => "traversal",
            Self::Absolute => "absolute",
            Self::SymlinkEscape => "symlink_escape",
            Self::GitInternals => "git_internals",
            Self::Malformed => "malformed",
        }
    }
}

/// Everything that can go wrong serving a Files request.
#[derive(Debug, thiserror::Error)]
pub enum FsError {
    /// The run's files cannot be shown: the worktree is removed or gone, or the run has no
    /// folder.
    #[error("the files of this run are unavailable: {0}")]
    Unavailable(String),
    /// The sandbox refused the path.
    #[error("path refused ({}): {path:?}", .why.word())]
    Rejected {
        /// Why.
        why: Rejection,
        /// The path as asked.
        path: String,
    },
    /// Nothing is at the path.
    #[error("no such file or directory: {0:?}")]
    NotFound(String),
    /// A directory was needed.
    #[error("not a directory: {0:?}")]
    NotADirectory(String),
    /// A file was needed.
    #[error("not a file: {0:?}")]
    NotAFile(String),
    /// Not an image the viewer shows.
    #[error("not a supported image: {0:?}")]
    NotAnImage(String),
    /// Larger than the limit.
    #[error("{path:?} is {size} bytes, over the {max} byte limit")]
    TooLarge {
        /// The path.
        path: String,
        /// Its size.
        size: u64,
        /// The limit.
        max: u64,
    },
    /// A page cursor that this daemon did not make.
    #[error("invalid cursor")]
    BadCursor,
    /// A search pattern git refused.
    #[error("invalid search pattern: {0}")]
    BadPattern(String),
    /// The daemon watches as many trees as it will.
    #[error("too many file watches are open")]
    TooManyWatches,
    /// `git` could not be run, or failed.
    #[error("git failed: {0}")]
    Git(String),
    /// A file system call failed.
    #[error("{what}: {error}")]
    Io {
        /// What was being done.
        what: String,
        /// The failure.
        error: String,
    },
}

impl FsError {
    pub(crate) fn io(what: impl Into<String>, error: &std::io::Error) -> Self {
        Self::Io {
            what: what.into(),
            error: error.to_string(),
        }
    }

    /// The code the error crosses the socket with.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Unavailable(_) => ErrorCode::WorkspaceUnavailable,
            Self::Rejected { .. } => ErrorCode::PathRejected,
            Self::NotFound(_) => ErrorCode::NotFound,
            Self::NotADirectory(_)
            | Self::NotAFile(_)
            | Self::NotAnImage(_)
            | Self::TooLarge { .. }
            | Self::BadCursor
            | Self::BadPattern(_) => ErrorCode::InvalidParams,
            Self::TooManyWatches => ErrorCode::Unavailable,
            Self::Git(_) | Self::Io { .. } => ErrorCode::Internal,
        }
    }

    /// The error object: the code, the message, and a typed `kind` where a client branches.
    pub fn object(&self) -> ErrorObject {
        let code = self.code();
        if code == ErrorCode::Internal {
            tracing::error!(error = %self, "internal error while serving files");
        }
        let kind = match self {
            Self::Rejected { why, .. } => Some(why.word()),
            Self::TooLarge { .. } => Some("too_large"),
            Self::NotAnImage(_) => Some("not_an_image"),
            Self::NotADirectory(_) => Some("not_a_directory"),
            Self::NotAFile(_) => Some("not_a_file"),
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
        let rejected = FsError::Rejected {
            why: Rejection::Traversal,
            path: "../x".into(),
        };
        assert_eq!(rejected.code(), ErrorCode::PathRejected);
        assert_eq!(rejected.object().data, Some(json!({"kind": "traversal"})));
        assert_eq!(
            FsError::Unavailable("removed".into()).code(),
            ErrorCode::WorkspaceUnavailable
        );
        assert_eq!(FsError::NotFound("a".into()).code(), ErrorCode::NotFound);
        assert_eq!(FsError::BadCursor.code(), ErrorCode::InvalidParams);
        assert_eq!(FsError::TooManyWatches.code(), ErrorCode::Unavailable);
        assert_eq!(FsError::Git("x".into()).code(), ErrorCode::Internal);
    }
}
