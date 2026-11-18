//! The Files service (RP1): a person's read-only view of a run's worktree. `fs.list`, `fs.read`,
//! `fs.read_image`, `fs.search_files`, `fs.search_content`, `git.status` and `fs.watch`.
//!
//! This is a person-driven service, not run state, so it lives here in the composition root and
//! not in the kernel (ARCH-7). The handlers resolve which tree a run is served from (its thread's
//! worktree, or for a run with none its working directory, flagged `in_place`) and hand it over
//! as a [`Served`]; everything below works on a directory and never asks the kernel anything.
//!
//! # Modules
//!
//! - [`sandbox`]: the only place a request path becomes a real path.
//! - `browse`: list, read, read an image.
//! - `search`: the fuzzy name scorer.
//! - `git`: `git status`, `git ls-files` and `git grep`, as read-only subprocesses.
//! - `watch`: the `fs.watch` stream (ARCH-10, ARCH-12).
//! - [`error`]: [`FsError`], and its stable wire codes (ARCH-14).

mod browse;
pub mod error;
mod git;
pub(crate) mod sandbox;
mod search;
mod watch;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use futures::Stream;
use reno_kernel::WatchBackoff;
use reno_proto::{
    FS_LIST_DEFAULT_LIMIT, FS_LIST_MAX_LIMIT, FS_SEARCH_DEFAULT_LIMIT, FS_SEARCH_MAX_LIMIT,
    FsChange, FsContentMatch, FsImageChunk, FsListResult, FsReadResult, FsSearchContentResult,
    FsSearchFilesResult, GitStatusResult,
};

pub use error::{FsError, Rejection};
use sandbox::Root;

use crate::watcher::WatcherFactory;

/// The most `fs.watch` subscriptions the daemon serves at once: each holds a platform watcher,
/// a stream from a pool the whole machine shares.
pub const MAX_OPEN_WATCHES: usize = 16;

/// The tree a request is served from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Served {
    /// The directory: the worktree's root, or a run's working directory.
    pub dir: PathBuf,
    /// The run has no worktree and is served from its own folder.
    pub in_place: bool,
}

/// The Files service.
#[derive(Debug)]
pub struct Files {
    watchers: Arc<dyn WatcherFactory>,
    timing: watch::Timing,
    open_watches: Arc<AtomicUsize>,
}

impl Files {
    /// A service that starts its watchers with `watchers` and retries a refused start with
    /// `retry`.
    pub fn new(watchers: Arc<dyn WatcherFactory>, retry: WatchBackoff) -> Self {
        Self {
            watchers,
            timing: watch::Timing {
                retry,
                ..watch::Timing::default()
            },
            open_watches: Arc::default(),
        }
    }

    /// How many `fs.watch` subscriptions are open.
    pub fn open_watches(&self) -> usize {
        self.open_watches.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// `fs.list`.
    pub async fn list(
        &self,
        served: &Served,
        dir: String,
        cursor: Option<String>,
        limit: Option<u32>,
    ) -> Result<FsListResult, FsError> {
        let limit = limit
            .unwrap_or(FS_LIST_DEFAULT_LIMIT)
            .clamp(1, FS_LIST_MAX_LIMIT) as usize;
        let root = Root::open(&served.dir)?;
        let page = blocking(move || browse::list(&root, &dir, cursor.as_deref(), limit)).await?;
        Ok(FsListResult {
            entries: page.entries,
            next_cursor: page.next_cursor,
            in_place: served.in_place,
        })
    }

    /// `fs.read`.
    pub async fn read(&self, served: &Served, path: String) -> Result<FsReadResult, FsError> {
        let root = Root::open(&served.dir)?;
        let mut read = blocking(move || browse::read(&root, &path)).await?;
        read.in_place = served.in_place;
        Ok(read)
    }

    /// `fs.read_image`.
    pub async fn read_image(
        &self,
        served: &Served,
        path: String,
        offset: u64,
    ) -> Result<FsImageChunk, FsError> {
        let root = Root::open(&served.dir)?;
        let mut chunk = blocking(move || browse::read_image(&root, &path, offset)).await?;
        chunk.in_place = served.in_place;
        Ok(chunk)
    }

    /// `fs.search_files`.
    pub async fn search_files(
        &self,
        served: &Served,
        query: String,
        limit: Option<u32>,
    ) -> Result<FsSearchFilesResult, FsError> {
        let limit = limit
            .unwrap_or(FS_SEARCH_DEFAULT_LIMIT)
            .clamp(1, FS_SEARCH_MAX_LIMIT) as usize;
        let root = Root::open(&served.dir)?;
        let dir = root.path().to_path_buf();
        let candidates = match git::list_files(&dir).await? {
            Some(files) => files,
            None => {
                let walked = dir.clone();
                blocking(move || Ok(search::walk(&walked))).await?
            }
        };
        let (files, truncated) = blocking(move || {
            // A file git still lists but the tree no longer has is not a result.
            let present: Vec<String> = candidates
                .into_iter()
                .filter(|path| dir.join(path).symlink_metadata().is_ok())
                .collect();
            Ok(search::best(&query, &present, limit))
        })
        .await?;
        Ok(FsSearchFilesResult {
            files,
            truncated,
            in_place: served.in_place,
        })
    }

    /// `fs.search_content`.
    pub async fn search_content(
        &self,
        served: &Served,
        query: &str,
        regex: bool,
        ignore_case: bool,
        limit: Option<u32>,
    ) -> Result<FsSearchContentResult, FsError> {
        if query.is_empty() {
            return Err(FsError::BadPattern("the query is empty".to_owned()));
        }
        let limit = limit
            .unwrap_or(FS_SEARCH_DEFAULT_LIMIT)
            .clamp(1, FS_SEARCH_MAX_LIMIT) as usize;
        let root = Root::open(&served.dir)?;
        let (hits, truncated) = git::grep(
            root.path(),
            &git::Grep {
                query,
                regex,
                ignore_case,
                limit,
            },
        )
        .await?;
        Ok(FsSearchContentResult {
            matches: hits
                .into_iter()
                // Nothing of `.git`, whatever git was asked to search.
                .filter(|hit| !hit.path.split('/').any(|part| part == sandbox::GIT_DIR))
                .map(|hit| FsContentMatch {
                    path: hit.path,
                    line: hit.line,
                    text: hit.text,
                })
                .collect(),
            truncated,
            in_place: served.in_place,
        })
    }

    /// `git.status`.
    pub async fn git_status(&self, served: &Served) -> Result<GitStatusResult, FsError> {
        let root = Root::open(&served.dir)?;
        let Some(repo) = git::repo_info(root.path()).await? else {
            return Ok(GitStatusResult {
                branch: None,
                repo: false,
                files: Vec::new(),
                in_place: served.in_place,
            });
        };
        let (branch, files) = git::status(root.path(), &repo.prefix).await?;
        Ok(GitStatusResult {
            branch,
            repo: true,
            files,
            in_place: served.in_place,
        })
    }

    /// `fs.watch`: arms a watcher for `served` now and returns the stream of its changes. The
    /// watcher lives exactly as long as the stream.
    pub async fn watch(
        &self,
        served: &Served,
    ) -> Result<impl Stream<Item = FsChange> + Send + 'static + use<>, FsError> {
        let root = Root::open(&served.dir)?;
        let slot = watch::Slot::take(&self.open_watches, MAX_OPEN_WATCHES)
            .ok_or(FsError::TooManyWatches)?;
        let git_dir = git::repo_info(root.path()).await?.map(|repo| repo.git_dir);
        let plan = watch::Plan {
            root: root.path().to_path_buf(),
            git_dir,
        };
        Ok(watch::start(
            Arc::clone(&self.watchers),
            plan,
            self.timing,
            slot,
        ))
    }
}

/// Runs blocking file system work off the async threads.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, FsError> + Send + 'static,
) -> Result<T, FsError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| FsError::Io {
            what: "a file task".to_owned(),
            error: error.to_string(),
        })?
}
