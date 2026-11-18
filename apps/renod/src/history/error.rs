//! The one error type of the History service, and what each case is on the wire (ARCH-14).

use reno_proto::{ErrorCode, ErrorObject};
use serde_json::json;

use crate::files::FsError;

/// Everything that can go wrong serving a History request.
#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    /// The tree cannot be shown, a path was refused, or git failed: the Files tab's errors.
    #[error(transparent)]
    Fs(#[from] FsError),
    /// A page cursor that this daemon did not make, or whose commit is gone.
    #[error("invalid cursor")]
    BadCursor,
    /// A search query that is empty, too long or has a NUL.
    #[error("invalid search query: {0}")]
    BadQuery(String),
    /// The scope or the range needs the thread's worktree, and the run has none.
    #[error("this run has no worktree of its own, so {0} has nothing to compare")]
    NeedsWorktree(&'static str),
    /// The kernel refused (a commit that is not the thread's, a run that is not found): its own
    /// error, already typed.
    #[error("{}", .0.message)]
    Remote(ErrorObject),
    /// The file is not one of the diff's changed files.
    #[error("{0:?} is not a changed file of this diff")]
    NotChanged(String),
}

impl HistoryError {
    /// The code the error crosses the socket with.
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::Fs(error) => error.code(),
            Self::BadCursor | Self::BadQuery(_) | Self::NeedsWorktree(_) => {
                ErrorCode::InvalidParams
            }
            Self::NotChanged(_) => ErrorCode::NotFound,
            Self::Remote(object) => object.code,
        }
    }

    /// The error object: the code, the message, and a typed `kind` where a client branches.
    pub fn object(&self) -> ErrorObject {
        match self {
            Self::Fs(error) => return error.object(),
            Self::Remote(object) => return object.clone(),
            _ => {}
        }
        let kind = match self {
            Self::BadCursor => "bad_cursor",
            Self::BadQuery(_) => "bad_query",
            Self::NeedsWorktree(_) => "needs_worktree",
            Self::NotChanged(_) => "not_changed",
            Self::Fs(_) | Self::Remote(_) => unreachable!("returned above"),
        };
        let mut object = ErrorObject::new(self.code(), self.to_string());
        object.data = Some(json!({ "kind": kind }));
        object
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_case_has_one_stable_code_and_kind() {
        assert_eq!(HistoryError::BadCursor.code(), ErrorCode::InvalidParams);
        assert_eq!(
            HistoryError::BadQuery("x".into()).object().data,
            Some(json!({"kind": "bad_query"}))
        );
        assert_eq!(
            HistoryError::NeedsWorktree("thread").object().data,
            Some(json!({"kind": "needs_worktree"}))
        );
        let missing = HistoryError::NotChanged("a".into());
        assert_eq!(missing.code(), ErrorCode::NotFound);
        assert_eq!(missing.object().data, Some(json!({"kind": "not_changed"})));
        assert_eq!(
            HistoryError::from(FsError::Unavailable("gone".into())).code(),
            ErrorCode::WorkspaceUnavailable
        );
        assert_eq!(
            HistoryError::from(FsError::Git("x".into())).code(),
            ErrorCode::Internal
        );
    }
}
