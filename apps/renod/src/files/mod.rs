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
pub(crate) mod git;
pub(crate) mod sandbox;
mod search;
mod watch;

use std::path::PathBuf;
use std::sync::Arc;

use futures::Stream;
use reno_kernel::WatchBackoff;
use reno_proto::{
    FS_LIST_DEFAULT_LIMIT, FS_LIST_MAX_LIMIT, FS_SEARCH_DEFAULT_LIMIT, FS_SEARCH_MAX_LIMIT,
    FsChange, FsContentMatch, FsEntryKind, FsImageChunk, FsListResult, FsReadResult,
    FsSearchContentResult, FsSearchFilesResult, GitStatusResult, image_hash,
};

pub use error::{FsError, Rejection};
use sandbox::Root;

use crate::watcher::WatcherFactory;

/// The most trees the daemon watches at once. Each is one platform watcher (an FSEvents stream),
/// from a pool the whole machine shares and that measured about sixteen free, which the
/// definitions and `fs` triggers draw on too. Subscriptions to one tree share its watcher, so
/// this bounds trees, not subscribers.
pub const MAX_OPEN_WATCHES: usize = 4;

/// Names that build tools make at the top of a tree, asked about whether git ignores them even
/// when they do not exist yet, so a first build is noise from its first file.
const BUILD_DIRS: [&str; 7] = [
    "target/",
    "node_modules/",
    ".build/",
    "dist/",
    "build/",
    ".venv/",
    ".next/",
];

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
    hub: Arc<watch::Hub>,
    timing: watch::Timing,
}

impl Files {
    /// A service that starts its watchers with `watchers` and retries a refused start with
    /// `retry`.
    pub fn new(watchers: Arc<dyn WatcherFactory>, retry: WatchBackoff) -> Self {
        Self::with_max_watches(watchers, retry, MAX_OPEN_WATCHES)
    }

    /// Like [`Files::new`], watching at most `max` trees at once.
    pub fn with_max_watches(
        watchers: Arc<dyn WatcherFactory>,
        retry: WatchBackoff,
        max: usize,
    ) -> Self {
        Self {
            hub: Arc::new(watch::Hub::new(watchers, max)),
            timing: watch::Timing {
                retry,
                ..watch::Timing::default()
            },
        }
    }

    /// How many trees are being watched (one watcher each, shared by their subscriptions).
    pub fn open_watches(&self) -> usize {
        self.hub.open()
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
        let for_list = root.clone();
        let mut page =
            blocking(move || browse::list(&for_list, &dir, cursor.as_deref(), limit)).await?;
        // Which of them git ignores, in one call from the listed directory (a folder that is no
        // repository ignores none).
        let asked: Vec<String> = page
            .entries
            .iter()
            .map(|e| match e.kind {
                FsEntryKind::Dir => format!("{}/", e.name),
                _ => e.name.clone(),
            })
            .collect();
        let hidden = git::ignored(&page.real, &asked).await?;
        for entry in &mut page.entries {
            let name = match entry.kind {
                FsEntryKind::Dir => format!("{}/", entry.name),
                _ => entry.name.clone(),
            };
            entry.ignored = hidden.contains(&name);
        }
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
                    column: hit.column,
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
                revision: revision(None, &[]),
                files: Vec::new(),
                in_place: served.in_place,
            });
        };
        let (branch, files) = git::status(root.path(), &repo.prefix).await?;
        Ok(GitStatusResult {
            revision: revision(branch.as_deref(), &files),
            branch,
            repo: true,
            files,
            in_place: served.in_place,
        })
    }

    /// `fs.watch`: starts watching `served` now (or joins the watcher it already has) and
    /// returns the stream of its changes. The watcher lives as long as its last stream.
    pub async fn watch(
        &self,
        served: &Served,
    ) -> Result<impl Stream<Item = FsChange> + Send + 'static + use<>, FsError> {
        let root = Root::open(&served.dir)?;
        let git_dir = git::repo_info(root.path()).await?.map(|repo| repo.git_dir);
        // The top-level entries git ignores, and the usual build directories whether or not they
        // exist yet: their events are noise.
        let mut candidates: Vec<String> = BUILD_DIRS.iter().map(|d| (*d).to_owned()).collect();
        if let Ok(page) = blocking({
            let root = root.clone();
            move || browse::list(&root, "", None, 2_000)
        })
        .await
        {
            candidates.extend(page.entries.iter().map(|e| match e.kind {
                FsEntryKind::Dir => format!("{}/", e.name),
                _ => e.name.clone(),
            }));
        }
        let noise = if git_dir.is_some() {
            git::ignored(root.path(), &candidates)
                .await?
                .into_iter()
                .map(|name| name.trim_end_matches('/').to_owned())
                .collect()
        } else {
            Vec::new()
        };
        let plan = watch::Plan {
            root: root.path().to_path_buf(),
            git_dir,
            noise,
        };
        watch::start(Arc::clone(&self.hub), plan, self.timing).map_err(|refused| match refused {
            watch::Refused::Capacity => FsError::TooManyWatches,
            watch::Refused::Platform(error) => FsError::Io {
                what: "starting a file watcher".to_owned(),
                error,
            },
        })
    }
}

/// A digest of what a status says, so a client can tell "the same" without comparing lists.
fn revision(branch: Option<&str>, files: &[reno_proto::GitFileStatus]) -> String {
    let text = serde_json::to_string(&(branch, files)).unwrap_or_default();
    image_hash(text.as_bytes())
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
