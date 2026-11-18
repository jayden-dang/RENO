//! The wire types of the read-only Files tab (RP1): `fs.list`, `fs.read`, `fs.read_image`,
//! `fs.search_files`, `fs.search_content`, `git.status` and the `fs.watch` subscription.
//!
//! Every request is keyed by a run and answered from that run's worktree (the thread's worktree,
//! whichever run of the thread asks). Paths are **relative to the worktree root**, written with
//! `/`, and sandboxed by the daemon: no `..`, no absolute path, no symlink that leaves the root,
//! and nothing inside `.git`. These are **person-only** methods: a run's token is refused.
//!
//! A run that has no worktree of its own and works in a folder (`in_place`, or a read-only run)
//! is served from its working directory, read-only; its results say so with `inPlace`. A run
//! whose worktree was removed is [`ErrorCode::WorkspaceUnavailable`](crate::ErrorCode).
//! Needs feature [`FEATURE_FILES`].

use base64::Engine as _;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::ids::RunId;

/// Feature: `fs.list`, `fs.read`, `fs.read_image`, `fs.search_files`, `fs.search_content`,
/// `git.status` and the `fs.watch` subscription exist.
pub const FEATURE_FILES: &str = "files";

/// Entries `fs.list` returns when `limit` is absent.
pub const FS_LIST_DEFAULT_LIMIT: u32 = 500;
/// The most entries one `fs.list` page holds; a larger `limit` is cut to this.
pub const FS_LIST_MAX_LIMIT: u32 = 2_000;
/// The most text `fs.read` returns, in bytes; a longer file is cut at a character boundary and
/// `truncated` says so.
pub const FS_READ_MAX_BYTES: usize = 1024 * 1024;
/// The largest image `fs.read_image` serves, in bytes.
pub const FS_IMAGE_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// How many bytes of an image one `fs.read_image` chunk carries.
pub const FS_IMAGE_CHUNK_BYTES: usize = 256 * 1024;
/// Results `fs.search_files` and `fs.search_content` return when `limit` is absent.
pub const FS_SEARCH_DEFAULT_LIMIT: u32 = 100;
/// The most results one search returns; a larger `limit` is cut to this.
pub const FS_SEARCH_MAX_LIMIT: u32 = 1_000;
/// The most directories one `fs.watch` item names; past it the item says `overflow` instead.
pub const FS_WATCH_MAX_DIRS: usize = 256;

/// What a directory entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FsEntryKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link (not followed by `fs.list`).
    Symlink,
}

/// One entry of a directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsEntry {
    /// The entry's name.
    pub name: String,
    /// Its path relative to the worktree root.
    pub path: String,
    /// What it is.
    pub kind: FsEntryKind,
    /// Its size in bytes; absent for a directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<u64>,
    /// When it was last modified, in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<u64>,
    /// Git ignores it (a build directory, say): shown dimmed. Always false outside a repository.
    #[serde(default, skip_serializing_if = "is_false")]
    pub ignored: bool,
}

/// Params of `fs.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsListParams {
    /// The run whose worktree to list.
    pub run_id: RunId,
    /// The directory, relative to the root; empty for the root itself.
    #[serde(default)]
    pub dir: String,
    /// The previous page's `nextCursor`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// At most this many entries ([`FS_LIST_DEFAULT_LIMIT`] when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Result of `fs.list`: directories first, then the rest, each by name; `.git` is never listed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsListResult {
    /// One page of entries.
    pub entries: Vec<FsEntry>,
    /// Pass it as `cursor` for the next page; absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<String>,
    /// The run has no worktree of its own and is served from its working directory.
    #[serde(default, skip_serializing_if = "is_false")]
    pub in_place: bool,
}

/// Params of `fs.read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsReadParams {
    /// The run whose worktree holds the file.
    pub run_id: RunId,
    /// The file, relative to the root.
    pub path: String,
}

/// How a text file ends its lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum LineEnding {
    /// `\n` only.
    Lf,
    /// `\r\n` only.
    Crlf,
    /// More than one kind.
    Mixed,
    /// No line ends at all.
    None,
}

/// Result of `fs.read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsReadResult {
    /// The file.
    pub path: String,
    /// Its text, cut to [`FS_READ_MAX_BYTES`]; absent for a binary file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// The file is longer than what `text` holds.
    pub truncated: bool,
    /// `utf-8`; `utf-8-bom` when the file started with a byte order mark (not in `text`);
    /// `utf-8-lossy` when invalid bytes were replaced; `binary`.
    pub encoding: String,
    /// How its lines end.
    pub line_ending: LineEnding,
    /// The file's size in bytes.
    pub size: u64,
    /// The file is not text (it holds NUL bytes): no `text`.
    pub binary: bool,
    /// SHA-256 of the bytes that were read (the whole file unless `truncated`), in lower-case
    /// hex: equal hashes mean equal content. Absent for a binary file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_hash: Option<String>,
    /// When the file was last modified, in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mtime_ms: Option<u64>,
    /// The run has no worktree of its own and is served from its working directory.
    #[serde(default, skip_serializing_if = "is_false")]
    pub in_place: bool,
}

/// Params of `fs.read_image`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsReadImageParams {
    /// The run whose worktree holds the image.
    pub run_id: RunId,
    /// The image (png, jpg, gif, webp or svg), relative to the root.
    pub path: String,
    /// The byte to start at: 0, then each chunk's `offset + length`.
    #[serde(default)]
    pub offset: u64,
}

/// Result of `fs.read_image`: one chunk of at most [`FS_IMAGE_CHUNK_BYTES`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsImageChunk {
    /// The image.
    pub path: String,
    /// Its media type, such as `image/png`.
    pub mime: String,
    /// The whole image's size in bytes.
    pub size: u64,
    /// Where this chunk starts.
    pub offset: u64,
    /// The chunk's bytes, standard base64.
    pub data: String,
    /// The chunk reaches the end of the image.
    pub eof: bool,
    /// SHA-256 of the whole image, in lower-case hex: the same on every chunk of one image, and
    /// a change between chunks means the file changed underneath (start again).
    pub hash: String,
    /// The run has no worktree of its own and is served from its working directory.
    #[serde(default, skip_serializing_if = "is_false")]
    pub in_place: bool,
}

impl FsImageChunk {
    /// The chunk's bytes. `None` when `data` is not valid base64.
    pub fn bytes(&self) -> Option<Vec<u8>> {
        base64::engine::general_purpose::STANDARD
            .decode(&self.data)
            .ok()
    }
}

/// Standard base64 of `bytes`, as [`FsImageChunk::data`] carries them.
pub fn encode_chunk(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// SHA-256 of `bytes` in lower-case hex, as [`FsImageChunk::hash`] carries it.
pub fn image_hash(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    Sha256::digest(bytes)
        .iter()
        .fold(String::with_capacity(64), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}

/// Params of `fs.search_files`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsSearchFilesParams {
    /// The run whose worktree to search.
    pub run_id: RunId,
    /// The characters of a file's path, in order (a fuzzy match on the name).
    pub query: String,
    /// At most this many ([`FS_SEARCH_DEFAULT_LIMIT`] when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// One file `fs.search_files` found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsFileMatch {
    /// The file, relative to the root.
    pub path: String,
    /// How well it matches: higher is better.
    pub score: u32,
}

/// Result of `fs.search_files`: best match first. Gitignored files are not searched unless git
/// tracks them; `.git` never is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsSearchFilesResult {
    /// The matches.
    pub files: Vec<FsFileMatch>,
    /// More files matched than `limit` allowed.
    pub truncated: bool,
    /// The run has no worktree of its own and is served from its working directory.
    #[serde(default, skip_serializing_if = "is_false")]
    pub in_place: bool,
}

/// Params of `fs.search_content`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsSearchContentParams {
    /// The run whose worktree to search.
    pub run_id: RunId,
    /// The text to find: literal, or an extended regular expression with `regex`.
    pub query: String,
    /// Read `query` as an extended regular expression.
    #[serde(default)]
    pub regex: bool,
    /// Match upper and lower case alike.
    #[serde(default)]
    pub ignore_case: bool,
    /// At most this many lines ([`FS_SEARCH_DEFAULT_LIMIT`] when absent).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// One line `fs.search_content` found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsContentMatch {
    /// The file, relative to the root.
    pub path: String,
    /// The line, from 1.
    pub line: u32,
    /// The byte offset of the first match in the line, from 0 (before the line was cut).
    #[serde(default)]
    pub column: u32,
    /// The line's text, cut at 500 characters.
    pub text: String,
}

/// Result of `fs.search_content`, by file then line. Binary files and gitignored files are not
/// searched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsSearchContentResult {
    /// The matching lines.
    pub matches: Vec<FsContentMatch>,
    /// More lines matched than `limit` allowed.
    pub truncated: bool,
    /// The run has no worktree of its own and is served from its working directory.
    #[serde(default, skip_serializing_if = "is_false")]
    pub in_place: bool,
}

/// What git says about one side of a path (the index, or the working tree).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitChange {
    /// No change on this side.
    Unmodified,
    /// Content changed.
    Modified,
    /// New.
    Added,
    /// Removed.
    Deleted,
    /// Renamed (see `oldPath`).
    Renamed,
    /// Copied (see `oldPath`).
    Copied,
    /// Its type changed (file to symlink, say).
    TypeChanged,
    /// Not tracked by git (both sides).
    Untracked,
    /// A merge conflict (both sides).
    Conflicted,
}

/// One path `git.status` reports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitFileStatus {
    /// The path, relative to the root.
    pub path: String,
    /// Where it came from, for a rename or a copy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    /// Its state in the index (what is staged).
    pub index: GitChange,
    /// Its state in the working tree (what is not staged).
    pub worktree: GitChange,
}

/// Params of `git.status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitStatusParams {
    /// The run whose worktree to ask about.
    pub run_id: RunId,
}

/// Result of `git.status`: `git status --porcelain=v2` of the worktree, changed paths only,
/// sorted by path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GitStatusResult {
    /// The checked-out branch; absent when HEAD is detached or the folder is not a repository.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// The folder is inside a git repository. False for a non-git folder served in place.
    pub repo: bool,
    /// The changed paths.
    pub files: Vec<GitFileStatus>,
    /// A digest of `branch` and `files`: the same while nothing changed, so a client can skip
    /// redrawing.
    #[serde(default)]
    pub revision: String,
    /// The run has no worktree of its own and is served from its working directory.
    #[serde(default, skip_serializing_if = "is_false")]
    pub in_place: bool,
}

/// Params of the `fs.watch` subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsWatchParams {
    /// The run whose worktree to watch. The daemon watches it only while this subscription
    /// lives.
    pub run_id: RunId,
}

/// One item of `fs.watch`: a settled burst of changes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FsChange {
    /// The directories whose entries changed (created, removed, renamed or written inside),
    /// relative to the root, sorted; the root is the empty string. Empty with `overflow`.
    #[serde(default)]
    pub dirs: Vec<String>,
    /// Git's own state changed (the index, `HEAD` or a ref): ask `git.status` again.
    #[serde(default)]
    pub git: bool,
    /// Too much changed to list, or events were lost (a slow reader, or a watch that was down and
    /// came back): rescan everything that is shown.
    #[serde(default)]
    pub overflow: bool,
    /// Live updates are down: the watcher could not start (the platform's pool is used up) and
    /// the daemon is retrying with a growing delay. The next item without `degraded` has
    /// `overflow` set.
    #[serde(default)]
    pub degraded: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn a_chunk_round_trips_and_the_hash_is_sha256_hex() {
        let bytes = b"\x89PNG\r\n\x1a\n";
        let chunk = FsImageChunk {
            path: "a.png".into(),
            mime: "image/png".into(),
            size: 8,
            offset: 0,
            data: encode_chunk(bytes),
            eof: true,
            hash: image_hash(bytes),
            in_place: false,
        };
        assert_eq!(chunk.bytes().as_deref(), Some(&bytes[..]));
        assert_eq!(chunk.hash.len(), 64);
        assert_eq!(
            image_hash(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn absent_optional_fields_decode_and_defaults_stay_off_the_wire() {
        let params: FsListParams = serde_json::from_str(r#"{"runId":"run_a"}"#).unwrap();
        assert_eq!(params.dir, "");
        assert_eq!(params.cursor, None);
        let listed = FsListResult {
            entries: vec![],
            next_cursor: None,
            in_place: false,
        };
        assert_eq!(serde_json::to_string(&listed).unwrap(), r#"{"entries":[]}"#);
    }
}
