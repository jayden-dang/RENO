//! The Files tab's methods on [`Client`]: `fs.list`, `fs.read`, `fs.read_image`,
//! `fs.search_files`, `fs.search_content`, `git.status` and the `fs.watch` subscription.
//!
//! All need feature `files`: on a daemon without it they refuse up front, without sending
//! anything. They are person-only (a run's token is refused by the daemon), and the state mirror
//! holds nothing for them: a caller asks when it needs an answer.

use futures::StreamExt;
use futures::stream::{self, BoxStream};
use reno_proto::{
    FEATURE_FILES, FsChange, FsImageChunk, FsList, FsListParams, FsListResult, FsRead, FsReadImage,
    FsReadImageParams, FsReadParams, FsReadResult, FsSearchContent, FsSearchContentParams,
    FsSearchContentResult, FsSearchFiles, FsSearchFilesParams, FsSearchFilesResult, FsWatch,
    FsWatchParams, GitStatus, GitStatusParams, GitStatusResult, RunId, image_hash,
};
use reno_rpc::{SubEvent, SubStream};

use super::{Client, MAX_STALLED_GAPS};
use crate::error::ClientError;

impl Client {
    /// `fs.list`: one page of a directory of the run's worktree. Needs feature `files`.
    pub async fn fs_list(&self, params: FsListParams) -> Result<FsListResult, ClientError> {
        self.require(FEATURE_FILES)?;
        Ok(self.rpc.call::<FsList>(params).await?)
    }

    /// `fs.read`: a text file, or the fact that it is binary. Needs feature `files`.
    pub async fn fs_read(&self, params: FsReadParams) -> Result<FsReadResult, ClientError> {
        self.require(FEATURE_FILES)?;
        Ok(self.rpc.call::<FsRead>(params).await?)
    }

    /// `fs.read_image`: one chunk of an image. Needs feature `files`.
    pub async fn fs_read_image(
        &self,
        params: FsReadImageParams,
    ) -> Result<FsImageChunk, ClientError> {
        self.require(FEATURE_FILES)?;
        Ok(self.rpc.call::<FsReadImage>(params).await?)
    }

    /// The whole of an image: its chunks in order, joined, and checked against the hash the
    /// daemon sent. An image that changes while it is read, or whose bytes do not match the hash,
    /// is a [`ClientError::Protocol`]. Needs feature `files`.
    pub async fn fs_read_image_all(
        &self,
        run_id: RunId,
        path: String,
    ) -> Result<(String, Vec<u8>), ClientError> {
        let mut bytes = Vec::new();
        let mut hash: Option<String> = None;
        loop {
            let chunk = self
                .fs_read_image(FsReadImageParams {
                    run_id: run_id.clone(),
                    path: path.clone(),
                    offset: bytes.len() as u64,
                })
                .await?;
            if hash.as_ref().is_some_and(|first| *first != chunk.hash) {
                return Err(ClientError::Protocol(format!(
                    "{path}: the image changed while it was read"
                )));
            }
            if chunk.offset != bytes.len() as u64 {
                return Err(ClientError::Protocol(format!(
                    "{path}: asked for byte {}, got a chunk at {}",
                    bytes.len(),
                    chunk.offset
                )));
            }
            let part = chunk
                .bytes()
                .ok_or_else(|| ClientError::Protocol(format!("{path}: a chunk is not base64")))?;
            hash = Some(chunk.hash.clone());
            let empty = part.is_empty();
            bytes.extend(part);
            if chunk.eof {
                if chunk.hash != image_hash(&bytes) {
                    return Err(ClientError::Protocol(format!(
                        "{path}: the bytes do not match the hash"
                    )));
                }
                return Ok((chunk.mime, bytes));
            }
            if empty {
                // Would ask for the same byte for ever.
                return Err(ClientError::Protocol(format!("{path}: an empty chunk")));
            }
        }
    }

    /// `fs.search_files`: files whose path matches fuzzily. Needs feature `files`.
    pub async fn fs_search_files(
        &self,
        params: FsSearchFilesParams,
    ) -> Result<FsSearchFilesResult, ClientError> {
        self.require(FEATURE_FILES)?;
        Ok(self.rpc.call::<FsSearchFiles>(params).await?)
    }

    /// `fs.search_content`: lines that match, literally or as a regular expression. Needs feature
    /// `files`.
    pub async fn fs_search_content(
        &self,
        params: FsSearchContentParams,
    ) -> Result<FsSearchContentResult, ClientError> {
        self.require(FEATURE_FILES)?;
        Ok(self.rpc.call::<FsSearchContent>(params).await?)
    }

    /// `git.status`: what changed in the run's worktree. Needs feature `files`.
    pub async fn git_status(&self, run_id: RunId) -> Result<GitStatusResult, ClientError> {
        self.require(FEATURE_FILES)?;
        Ok(self
            .rpc
            .call::<GitStatus>(GitStatusParams { run_id })
            .await?)
    }

    /// `fs.watch`: the directories of the run's worktree that change, in settled bursts, for as
    /// long as the stream lives. The daemon watches only while it is open; dropping it ends the
    /// watch.
    ///
    /// When the daemon cuts the subscription off with `sub.gap` (ARCH-12) the stream subscribes
    /// again and yields one item with `overflow` set first, since what changed in between is
    /// unknown: rescan. Gaps in a row with no item between them end the stream with
    /// [`ClientError::Protocol`]. Needs feature `files`.
    pub async fn watch_fs(
        &self,
        run_id: RunId,
    ) -> Result<BoxStream<'static, Result<FsChange, ClientError>>, ClientError> {
        self.require(FEATURE_FILES)?;
        let params = FsWatchParams { run_id };
        let sub = self.rpc.subscribe::<FsWatch>(params.clone()).await?;
        let state = FsWatchState {
            rpc: self.rpc.clone(),
            params,
            sub: Some(sub),
            owed_rescan: false,
            stalled: 0,
            failed: false,
        };
        Ok(stream::unfold(state, FsWatchState::step).boxed())
    }
}

struct FsWatchState {
    rpc: reno_rpc::Client,
    params: FsWatchParams,
    sub: Option<SubStream<FsChange>>,
    /// A gap happened: the next item delivered is a rescan notice.
    owed_rescan: bool,
    stalled: u32,
    failed: bool,
}

impl FsWatchState {
    async fn step(mut self) -> Option<(Result<FsChange, ClientError>, Self)> {
        if self.failed {
            return None;
        }
        match self.advance().await {
            Ok(Some(item)) => Some((Ok(item), self)),
            Ok(None) => None,
            Err(error) => {
                self.failed = true;
                Some((Err(error), self))
            }
        }
    }

    async fn advance(&mut self) -> Result<Option<FsChange>, ClientError> {
        loop {
            let sub = match self.sub.as_mut() {
                Some(sub) => sub,
                None => self
                    .sub
                    .insert(self.rpc.subscribe::<FsWatch>(self.params.clone()).await?),
            };
            if self.owed_rescan {
                // Subscribed again: what changed while it was cut off is unknown.
                self.owed_rescan = false;
                return Ok(Some(FsChange {
                    overflow: true,
                    git: true,
                    ..FsChange::default()
                }));
            }
            match sub.next().await {
                Some(SubEvent::Item(item)) => {
                    self.stalled = 0;
                    return Ok(Some(item));
                }
                Some(SubEvent::Gap { .. }) => {
                    self.sub = None;
                    self.owed_rescan = true;
                    self.stalled += 1;
                    if self.stalled > MAX_STALLED_GAPS {
                        return Err(ClientError::Protocol(
                            "a file watch keeps gapping without delivering a change".to_owned(),
                        ));
                    }
                }
                Some(SubEvent::Done) | None => return Ok(None),
            }
        }
    }
}
