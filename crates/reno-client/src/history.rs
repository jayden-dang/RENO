//! History and the Review upgrade on [`Client`]: `git.log`, `git.log_search` and
//! `workspace.file_sources`.
//!
//! All need feature `history`: on a daemon without it they refuse up front, without sending
//! anything. They are person-only (a run's token is refused by the daemon), and the state mirror
//! holds nothing for them: a caller asks when it needs an answer.

use reno_proto::{
    FEATURE_HISTORY, FileSourcesParams, FileSourcesResult, GitLog, GitLogParams, GitLogResult,
    GitLogSearch, GitLogSearchParams, GitLogSearchResult, WorkspaceFileSources,
};

use super::Client;
use crate::error::ClientError;

impl Client {
    /// `git.log`: one page of the run's commit history, topologically ordered. Needs feature
    /// `history`.
    pub async fn git_log(&self, params: GitLogParams) -> Result<GitLogResult, ClientError> {
        self.require(FEATURE_HISTORY)?;
        Ok(self.rpc.call::<GitLog>(params).await?)
    }

    /// `git.log_search`: the commits whose message, author or name match a text. Needs feature
    /// `history`.
    pub async fn git_log_search(
        &self,
        params: GitLogSearchParams,
    ) -> Result<GitLogSearchResult, ClientError> {
        self.require(FEATURE_HISTORY)?;
        Ok(self.rpc.call::<GitLogSearch>(params).await?)
    }

    /// `workspace.file_sources`: both sides of one changed file, whole. Needs feature `history`.
    pub async fn workspace_file_sources(
        &self,
        params: FileSourcesParams,
    ) -> Result<FileSourcesResult, ClientError> {
        self.require(FEATURE_HISTORY)?;
        Ok(self.rpc.call::<WorkspaceFileSources>(params).await?)
    }
}
