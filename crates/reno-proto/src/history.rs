//! The wire types of History and the Review upgrade (RP3): `git.log`, `git.log_search` and
//! `workspace.file_sources`, and the checksum `workspace.diff` carries so a client can tell a
//! diff it holds went stale.
//!
//! Every request is keyed by a run and answered from that run's tree, like the Files tab: the
//! thread's worktree, or (for a run with no worktree) its own folder, read-only, flagged
//! `inPlace`. These are **person-only** methods: a run's token is refused. Needs feature
//! [`FEATURE_HISTORY`].
//!
//! No revision, branch name or path a client sends reaches `git` as one: a log is asked for by
//! a [`GitLogRev`] and a cursor the daemon made, a file by a path of the diff.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::files::image_hash;
use crate::ids::RunId;
use crate::workspace::{ChangedFile, DiffScope};

/// Feature: `git.log`, `git.log_search` and `workspace.file_sources` exist, and `workspace.diff`
/// accepts scope `working_tree` and carries a `checksum`.
pub const FEATURE_HISTORY: &str = "history";

/// Commits `git.log` and `git.log_search` return when `limit` is absent.
pub const GIT_LOG_DEFAULT_LIMIT: u32 = 50;
/// The most commits one page holds; a larger `limit` is cut to this.
pub const GIT_LOG_MAX_LIMIT: u32 = 100;
/// The longest search query, in bytes.
pub const GIT_LOG_QUERY_MAX_BYTES: usize = 200;
/// The most text `workspace.file_sources` returns for each side, in bytes; a longer file is cut
/// at a character boundary and `truncated` says so.
pub const FILE_SOURCE_MAX_BYTES: usize = 1024 * 1024;

/// Which commits a log lists. Never a free-form revision: a name a client typed is not handed
/// to git.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitLogRev {
    /// Everything reachable from the worktree's `HEAD` (the thread's branch): the history the
    /// branch was cut from too. The default.
    #[default]
    Branch,
    /// Only the thread's own commits: those after its base commit (`base..HEAD`). Needs a
    /// worktree.
    Thread,
    /// Only the commits made since the run started (`start..HEAD`; the thread's base for a run
    /// that recorded no start). Needs a worktree.
    Run,
}

/// What a ref is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitRefKind {
    /// A local branch.
    Branch,
    /// A tag (an annotated tag is shown on the commit it points at).
    Tag,
    /// The commit `HEAD` is on; its `name` is `HEAD`.
    Head,
    /// A remote-tracking branch, such as `origin/main`.
    Remote,
}

/// A ref that points at a commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitRef {
    /// The short name: `main`, `v1.0`, `origin/main`, or `HEAD`.
    pub name: String,
    /// What it is.
    pub kind: GitRefKind,
}

/// One commit of a log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitCommit {
    /// The full object name.
    pub sha: String,
    /// Its parents, first parent first; none for a root commit. A parent may be absent from the
    /// page (and from a shallow repository): a lane graph treats that as a line that leaves.
    pub parents: Vec<String>,
    /// The first line of the message, redacted like the journal.
    pub subject: String,
    /// The author's name.
    pub author_name: String,
    /// A stable key for the author, to pick an avatar or a color by: a SHA-256 of the lower-cased
    /// email. The email itself never crosses the wire.
    pub author_key: String,
    /// When it was authored, in milliseconds since the Unix epoch.
    pub time_ms: u64,
    /// The branches, tags and remote branches that point at it (and `HEAD`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<GitRef>,
}

/// The stable key of an author, from an email (see [`GitCommit::author_key`]).
pub fn author_key(email: &str) -> String {
    image_hash(email.trim().to_lowercase().as_bytes())
}

/// Params of `git.log`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitLogParams {
    /// The run whose tree to read.
    pub run_id: RunId,
    /// The previous page's `nextCursor`; the first page when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// At most this many ([`GIT_LOG_DEFAULT_LIMIT`] when absent; cut to [`GIT_LOG_MAX_LIMIT`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Which commits; the whole branch when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<GitLogRev>,
}

/// Result of `git.log`: newest first, in topological order (a commit before all its parents), so
/// a client can lay the lanes out from `parents` alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitLogResult {
    /// One page of commits.
    pub commits: Vec<GitCommit>,
    /// Pass it as `cursor` for the next page; absent on the last page. It pins the commit the
    /// listing started from, so a commit made while a person scrolls does not shift the pages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// The commit `HEAD` is on now; absent in a repository with no commits, or none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// The tree is in a repository. False for a folder that is none (no commits, no error).
    pub repo: bool,
    /// The repository is shallow: history stops short of its root.
    #[serde(default, skip_serializing_if = "is_false")]
    pub shallow: bool,
    /// The run has no worktree of its own and is served from its working directory.
    #[serde(default, skip_serializing_if = "is_false")]
    pub in_place: bool,
}

/// Params of `git.log_search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitLogSearchParams {
    /// The run whose tree to read.
    pub run_id: RunId,
    /// A text to find, as typed, not a pattern: in the message (any case), in an author's name
    /// or email, or the start of a commit's name (4 to 40 hexadecimal characters). At most
    /// [`GIT_LOG_QUERY_MAX_BYTES`] bytes.
    pub query: String,
    /// At most this many ([`GIT_LOG_DEFAULT_LIMIT`] when absent; cut to [`GIT_LOG_MAX_LIMIT`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Which commits to search; the whole branch when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rev: Option<GitLogRev>,
}

/// Result of `git.log_search`: the newest match (by author time) first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitLogSearchResult {
    /// The matches.
    pub commits: Vec<GitCommit>,
    /// More commits matched than `limit` allowed.
    pub truncated: bool,
    /// The tree is in a repository.
    pub repo: bool,
    /// The run has no worktree of its own and is served from its working directory.
    #[serde(default, skip_serializing_if = "is_false")]
    pub in_place: bool,
}

/// Params of `workspace.file_sources`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileSourcesParams {
    /// The run whose workspace to read.
    pub run_id: RunId,
    /// A changed file of that diff (repository-relative); a path the diff does not list is
    /// refused.
    pub path: String,
    /// What the diff compares; the thread against its base commit when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<DiffScope>,
    /// The `checksum` of the `workspace.diff` that named this same `path` and `scope`, which
    /// the client is showing; the result says `stale` when the diff has moved on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
}

/// Result of `workspace.file_sources`: both sides of one changed file, whole, so a client can
/// highlight the full file and not only the lines of the patch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FileSourcesResult {
    /// The file (the new path of a rename).
    pub path: String,
    /// Where the old side reads from, when the file was renamed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    /// The old side's text; absent for an added file or a binary one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_text: Option<String>,
    /// The new side's text; absent for a deleted file or a binary one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_text: Option<String>,
    /// SHA-256 of the old side's bytes (as read, before redaction); absent when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_hash: Option<String>,
    /// SHA-256 of the new side's bytes (as read, before redaction); absent when there is none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_hash: Option<String>,
    /// The `checksum` of the diff as it is now: pass it back on the next call.
    pub checksum: String,
    /// The `checksum` the client sent is not the diff's now; the text is the current one.
    #[serde(default, skip_serializing_if = "is_false")]
    pub stale: bool,
    /// Git treats the file as binary (or it is not UTF-8): no text.
    #[serde(default, skip_serializing_if = "is_false")]
    pub binary: bool,
    /// A side is longer than [`FILE_SOURCE_MAX_BYTES`] and its text was cut.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

/// The checksum of a diff: a digest of its file list, the patch of the requested file, and
/// whether that patch was cut. It changes when the diff does. Computed over what the client
/// receives (after redaction), by the daemon, once, here.
pub fn diff_checksum(files: &[ChangedFile], patch: Option<&str>, patch_truncated: bool) -> String {
    let text = serde_json::to_string(&(files, patch, patch_truncated)).unwrap_or_default();
    image_hash(text.as_bytes())
}

fn is_false(b: &bool) -> bool {
    !*b
}
