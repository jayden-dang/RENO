//! The fake daemon's Files tab (RP1): `fs.list`, `fs.read`, `fs.read_image`, `fs.search_files`,
//! `fs.search_content`, `git.status` and `fs.watch`, over a small tree held in memory. No real
//! file system, no `git`, no watcher: the same answer every time.
//!
//! Which tree a run gets is fixed by its id, like its workspace ([`super::changes`]):
//!
//! - a run with a worktree (`Full`, `Empty`) gets the demo tree below;
//! - `builder0`'s worktree was removed: every method answers `WorkspaceUnavailable`;
//! - `reviewer1` and `architect1` have no worktree: they get the demo tree, flagged `inPlace`,
//!   and `git.status` says it is no repository;
//! - a run the fake does not know is `NotFound`.
//!
//! The paths are sandboxed the way the daemon's are: `..`, an absolute path and `.git` are
//! `PathRejected`.
//!
//! `fs.watch` delivers what a test pushes with [`FakeDaemon::push_fs_change`], and
//! [`FakeDaemon::fs_watches`] counts the open subscriptions, so a test can show that a tab
//! subscribes only while it is visible (ARCH-10).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use futures::stream;
use reno_proto::{
    ErrorCode, ErrorObject, FS_IMAGE_CHUNK_BYTES, FS_IMAGE_MAX_BYTES, FS_LIST_DEFAULT_LIMIT,
    FS_LIST_MAX_LIMIT, FS_READ_MAX_BYTES, FS_SEARCH_DEFAULT_LIMIT, FS_SEARCH_MAX_LIMIT, FsChange,
    FsContentMatch, FsEntry, FsEntryKind, FsFileMatch, FsImageChunk, FsList, FsListParams,
    FsListResult, FsRead, FsReadImage, FsReadImageParams, FsReadParams, FsReadResult,
    FsSearchContent, FsSearchContentParams, FsSearchContentResult, FsSearchFiles,
    FsSearchFilesParams, FsSearchFilesResult, FsWatch, FsWatchParams, GitChange, GitFileStatus,
    GitStatus, GitStatusParams, GitStatusResult, LineEnding, RunId, Seq, encode_chunk, image_hash,
};
use reno_rpc::{Router, SubItem};

use super::changes::{Fixture, fixture};
use super::{FakeDaemon, Shared, injected};

/// How many files `many/` holds: more than one default page.
pub const MANY: usize = 600;

/// A node of the demo tree.
#[derive(Debug, Clone)]
enum Node {
    Dir,
    File(Vec<u8>),
}

/// The demo tree, by path.
fn tree(transcript: bool) -> BTreeMap<String, Node> {
    let mut nodes = BTreeMap::new();
    let mut file = |path: &str, text: &str| {
        nodes.insert(path.to_owned(), Node::File(text.as_bytes().to_vec()));
    };
    file("Cargo.toml", "[workspace]\nmembers = [\"crates/*\"]\n");
    file("README.md", "# Demo\n\nA small tree for tests.\n");
    file("src/lib.rs", "pub fn demo() {}\n");
    file("src/main.rs", "fn main() {\n    demo();\n}\n");
    file("docs/guide.md", "# Guide\r\n\r\nWindows line endings.\r\n");
    file("notes.txt", "alpha\nbeta\nAlpha again\n");
    // A dotfile, for Settings › General › Show hidden files.
    file(".env.example", "API_URL=http://localhost:3000\n");
    nodes.insert(
        "assets/logo.png".to_owned(),
        Node::File([&b"\x89PNG\r\n\x1a\n"[..], &[0u8; 120][..]].concat()),
    );
    nodes.insert("assets/blob.bin".to_owned(), Node::File(vec![0, 1, 2, 3]));
    if transcript {
        // The files the transcript demo's replies point at (see `super::transcript`).
        for (path, bytes) in super::transcript::files() {
            nodes.insert(path.to_owned(), Node::File(bytes));
        }
    }
    for n in 0..MANY {
        nodes.insert(format!("many/f{n:03}.txt"), Node::File(vec![b'x'; n % 7]));
    }
    // Directories are what the paths imply.
    let dirs: Vec<String> = nodes
        .keys()
        .flat_map(|path| {
            let parts: Vec<&str> = path.split('/').collect();
            (1..parts.len())
                .map(|n| parts[..n].join("/"))
                .collect::<Vec<_>>()
        })
        .collect();
    for dir in dirs {
        nodes.insert(dir, Node::Dir);
    }
    nodes
}

/// The paths `git.status` reports in the demo tree.
fn status_files() -> Vec<GitFileStatus> {
    let row = |path: &str, old: Option<&str>, index, worktree| GitFileStatus {
        path: path.to_owned(),
        old_path: old.map(str::to_owned),
        index,
        worktree,
    };
    vec![
        row(
            "Cargo.toml",
            None,
            GitChange::Unmodified,
            GitChange::Modified,
        ),
        row(
            "docs/guide.md",
            Some("docs/old-guide.md"),
            GitChange::Renamed,
            GitChange::Unmodified,
        ),
        row(
            "notes.txt",
            None,
            GitChange::Untracked,
            GitChange::Untracked,
        ),
        row("src/lib.rs", None, GitChange::Added, GitChange::Unmodified),
        row(
            "src/old.rs",
            None,
            GitChange::Unmodified,
            GitChange::Deleted,
        ),
    ]
}

/// Where a run is served from, or why not.
pub(super) struct Place {
    pub(super) in_place: bool,
    repo: bool,
    branch: String,
    /// Whether the demo tree has the transcript demo's extra files.
    transcript: bool,
}

pub(super) fn place(s: &Shared, run: &RunId) -> Result<Place, ErrorObject> {
    if s.read(|w| w.run(run).is_none()) {
        return Err(ErrorObject::new(ErrorCode::NotFound, "no such run"));
    }
    match fixture(run) {
        Fixture::Full | Fixture::Empty => Ok(Place {
            in_place: false,
            repo: true,
            branch: format!("reno/{}", run.as_str().trim_start_matches("run_")),
            transcript: s.config.transcript_threads,
        }),
        Fixture::Missing => Ok(Place {
            in_place: true,
            repo: false,
            branch: String::new(),
            transcript: s.config.transcript_threads,
        }),
        Fixture::Removed => Err(ErrorObject::new(
            ErrorCode::WorkspaceUnavailable,
            "the files of this run are unavailable: its worktree was removed",
        )),
    }
}

fn rejected(why: &str, path: &str) -> ErrorObject {
    let mut object = ErrorObject::new(
        ErrorCode::PathRejected,
        format!("path refused ({why}): {path:?}"),
    );
    object.data = Some(serde_json::json!({ "kind": why }));
    object
}

/// The sandbox's lexical rules: the normal parts of `path`.
fn parts(path: &str) -> Result<Vec<String>, ErrorObject> {
    if path.starts_with(['/', '\\']) {
        return Err(rejected("absolute", path));
    }
    let mut parts = Vec::new();
    for part in path.split(['/', '\\']) {
        match part {
            "" | "." => {}
            ".." => return Err(rejected("traversal", path)),
            ".git" => return Err(rejected("git_internals", path)),
            name => parts.push(name.to_owned()),
        }
    }
    Ok(parts)
}

fn invalid(message: &str) -> ErrorObject {
    ErrorObject::new(ErrorCode::InvalidParams, message)
}

fn not_found(path: &str) -> ErrorObject {
    ErrorObject::new(
        ErrorCode::NotFound,
        format!("no such file or directory: {path:?}"),
    )
}

fn list(p: &FsListParams, place: &Place) -> Result<FsListResult, ErrorObject> {
    let nodes = tree(place.transcript);
    let dir = parts(&p.dir)?.join("/");
    if !dir.is_empty() {
        match nodes.get(&dir) {
            Some(Node::Dir) => {}
            Some(Node::File(_)) => return Err(invalid("not a directory")),
            None => return Err(not_found(&p.dir)),
        }
    }
    let prefix = if dir.is_empty() {
        String::new()
    } else {
        format!("{dir}/")
    };
    let mut entries: Vec<FsEntry> = nodes
        .iter()
        .filter_map(|(path, node)| {
            let name = path.strip_prefix(&prefix)?;
            (!name.is_empty() && !name.contains('/')).then(|| FsEntry {
                name: name.to_owned(),
                path: path.clone(),
                kind: match node {
                    Node::Dir => FsEntryKind::Dir,
                    Node::File(_) => FsEntryKind::File,
                },
                size: match node {
                    Node::Dir => None,
                    Node::File(bytes) => Some(bytes.len() as u64),
                },
                mtime_ms: Some(super::data::NOW_MS - 60_000),
                // The demo tree has one ignored directory, as a build output is.
                ignored: path == "many",
            })
        })
        .collect();
    let key = |kind: FsEntryKind, name: &str| {
        (
            u8::from(kind != FsEntryKind::Dir),
            name.to_lowercase(),
            name.to_owned(),
        )
    };
    entries.sort_by_cached_key(|e| key(e.kind, &e.name));
    if let Some(cursor) = &p.cursor {
        let (rank, name) = cursor
            .split_once(':')
            .ok_or_else(|| invalid("invalid cursor"))?;
        let rank: u8 = rank.parse().map_err(|_| invalid("invalid cursor"))?;
        let after = (rank, name.to_lowercase(), name.to_owned());
        entries.retain(|e| key(e.kind, &e.name) > after);
    }
    let limit = p
        .limit
        .unwrap_or(FS_LIST_DEFAULT_LIMIT)
        .clamp(1, FS_LIST_MAX_LIMIT) as usize;
    let more = entries.len() > limit;
    entries.truncate(limit);
    let next_cursor = more
        .then(|| {
            entries
                .last()
                .map(|e| format!("{}:{}", u8::from(e.kind != FsEntryKind::Dir), e.name))
        })
        .flatten();
    Ok(FsListResult {
        entries,
        next_cursor,
        in_place: place.in_place,
    })
}

fn file_of(path: &str, place: &Place) -> Result<(String, Vec<u8>), ErrorObject> {
    let rel = parts(path)?.join("/");
    match tree(place.transcript).remove(&rel) {
        Some(Node::File(bytes)) => Ok((rel, bytes)),
        Some(Node::Dir) => Err(invalid("not a file")),
        None => Err(not_found(path)),
    }
}

fn read(p: &FsReadParams, place: &Place) -> Result<FsReadResult, ErrorObject> {
    let (path, bytes) = file_of(&p.path, place)?;
    let size = bytes.len() as u64;
    if bytes.contains(&0) {
        return Ok(FsReadResult {
            path,
            text: None,
            truncated: false,
            encoding: "binary".to_owned(),
            line_ending: LineEnding::None,
            size,
            binary: true,
            content_hash: None,
            mtime_ms: Some(super::data::NOW_MS - 60_000),
            in_place: place.in_place,
        });
    }
    let text = String::from_utf8_lossy(&bytes[..bytes.len().min(FS_READ_MAX_BYTES)]).into_owned();
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf;
    Ok(FsReadResult {
        path,
        line_ending: match (lf, crlf) {
            (0, 0) => LineEnding::None,
            (_, 0) => LineEnding::Lf,
            (0, _) => LineEnding::Crlf,
            _ => LineEnding::Mixed,
        },
        text: Some(text),
        truncated: bytes.len() > FS_READ_MAX_BYTES,
        encoding: "utf-8".to_owned(),
        size,
        binary: false,
        content_hash: Some(image_hash(&bytes[..bytes.len().min(FS_READ_MAX_BYTES)])),
        mtime_ms: Some(super::data::NOW_MS - 60_000),
        in_place: place.in_place,
    })
}

fn read_image(p: &FsReadImageParams, place: &Place) -> Result<FsImageChunk, ErrorObject> {
    let (path, bytes) = file_of(&p.path, place)?;
    let mime = match path
        .rsplit('.')
        .next()
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("svg") => "image/svg+xml",
        _ => return Err(invalid("not a supported image")),
    };
    if bytes.len() as u64 > FS_IMAGE_MAX_BYTES {
        return Err(invalid("too large"));
    }
    let start = usize::try_from(p.offset)
        .unwrap_or(usize::MAX)
        .min(bytes.len());
    let end = (start + FS_IMAGE_CHUNK_BYTES).min(bytes.len());
    Ok(FsImageChunk {
        path,
        mime: mime.to_owned(),
        size: bytes.len() as u64,
        offset: start as u64,
        data: encode_chunk(&bytes[start..end]),
        eof: end >= bytes.len(),
        hash: image_hash(&bytes),
        in_place: place.in_place,
    })
}

/// A subsequence match, shorter paths first: enough to show the shape of a result.
fn search_files(p: &FsSearchFilesParams, place: &Place) -> FsSearchFilesResult {
    let wanted: Vec<char> = p.query.to_lowercase().chars().collect();
    let mut found: Vec<FsFileMatch> = tree(place.transcript)
        .into_iter()
        .filter(|(_, node)| matches!(node, Node::File(_)))
        .filter_map(|(path, _)| {
            let lower = path.to_lowercase();
            let mut chars = lower.chars();
            let ok = !wanted.is_empty() && wanted.iter().all(|w| chars.any(|c| c == *w));
            ok.then(|| FsFileMatch {
                score: u32::try_from(1_000usize.saturating_sub(path.len())).unwrap_or(1),
                path,
            })
        })
        .collect();
    found.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
    let limit = p
        .limit
        .unwrap_or(FS_SEARCH_DEFAULT_LIMIT)
        .clamp(1, FS_SEARCH_MAX_LIMIT) as usize;
    let truncated = found.len() > limit;
    found.truncate(limit);
    FsSearchFilesResult {
        files: found,
        truncated,
        in_place: place.in_place,
    }
}

/// Literal matching only, whatever `regex` says: the fake has no regex engine.
fn search_content(
    p: &FsSearchContentParams,
    place: &Place,
) -> Result<FsSearchContentResult, ErrorObject> {
    if p.query.is_empty() {
        return Err(invalid("the query is empty"));
    }
    let needle = if p.ignore_case {
        p.query.to_lowercase()
    } else {
        p.query.clone()
    };
    let limit = p
        .limit
        .unwrap_or(FS_SEARCH_DEFAULT_LIMIT)
        .clamp(1, FS_SEARCH_MAX_LIMIT) as usize;
    let mut matches = Vec::new();
    let mut truncated = false;
    'files: for (path, node) in tree(place.transcript) {
        let Node::File(bytes) = node else { continue };
        if bytes.contains(&0) {
            continue;
        }
        for (n, line) in String::from_utf8_lossy(&bytes).lines().enumerate() {
            let hay = if p.ignore_case {
                line.to_lowercase()
            } else {
                line.to_owned()
            };
            if hay.contains(&needle) {
                if matches.len() >= limit {
                    truncated = true;
                    break 'files;
                }
                matches.push(FsContentMatch {
                    path: path.clone(),
                    line: u32::try_from(n + 1).unwrap_or(u32::MAX),
                    column: u32::try_from(hay.find(&needle).unwrap_or(0)).unwrap_or(0),
                    text: line.to_owned(),
                });
            }
        }
    }
    Ok(FsSearchContentResult {
        matches,
        truncated,
        in_place: place.in_place,
    })
}

/// Counts an open `fs.watch` for as long as its stream lives.
struct Watching(Arc<Shared>);

impl Drop for Watching {
    fn drop(&mut self) {
        self.0.fs_watches.fetch_sub(1, Ordering::SeqCst);
    }
}

pub(super) fn add(router: Router, shared: &Arc<Shared>) -> Router {
    macro_rules! method {
        ($router:expr, $ty:ty, $name:literal, $params:ty, |$p:ident, $place:ident| $body:expr) => {{
            let s = Arc::clone(shared);
            $router.method::<$ty, _, _>(move |$p: $params, _| {
                let s = Arc::clone(&s);
                async move {
                    injected(&s, $name)?;
                    let $place = place(&s, &$p.run_id)?;
                    $body
                }
            })
        }};
    }
    let router = method!(router, FsList, "fs.list", FsListParams, |p, place| list(
        &p, &place
    ));
    let router = method!(router, FsRead, "fs.read", FsReadParams, |p, place| read(
        &p, &place
    ));
    let router = method!(
        router,
        FsReadImage,
        "fs.read_image",
        FsReadImageParams,
        |p, place| read_image(&p, &place)
    );
    let router = method!(
        router,
        FsSearchFiles,
        "fs.search_files",
        FsSearchFilesParams,
        |p, place| Ok(search_files(&p, &place))
    );
    let router = method!(
        router,
        FsSearchContent,
        "fs.search_content",
        FsSearchContentParams,
        |p, place| search_content(&p, &place)
    );
    let router = method!(
        router,
        GitStatus,
        "git.status",
        GitStatusParams,
        |p, place| {
            Ok(GitStatusResult {
                branch: place.repo.then(|| place.branch.clone()),
                repo: place.repo,
                revision: if place.repo {
                    "demo-1".to_owned()
                } else {
                    String::new()
                },
                files: if place.repo {
                    status_files()
                } else {
                    Vec::new()
                },
                in_place: place.in_place,
            })
        }
    );
    let s = Arc::clone(shared);
    router.subscription::<FsWatch, _, _, _>(move |p: FsWatchParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "fs.watch")?;
            place(&s, &p.run_id)?;
            let rx = s.fs_tx.subscribe();
            s.fs_watches.fetch_add(1, Ordering::SeqCst);
            let state = (rx, p.run_id, 1u64, Watching(Arc::clone(&s)));
            Ok(stream::unfold(
                state,
                |(mut rx, run, n, guard)| async move {
                    loop {
                        match rx.recv().await {
                            Ok((target, change)) if target == run => {
                                return Some((
                                    SubItem::seq(Seq(n), change),
                                    (rx, run, n + 1, guard),
                                ));
                            }
                            Ok(_) => {}
                            // The queue behind this subscriber overflowed: what was lost is
                            // unknown, so the item says to rescan.
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                let rescan = FsChange {
                                    overflow: true,
                                    git: true,
                                    ..FsChange::default()
                                };
                                return Some((
                                    SubItem::seq(Seq(n), rescan),
                                    (rx, run, n + 1, guard),
                                ));
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                        }
                    }
                },
            ))
        }
    })
}

impl FakeDaemon {
    /// Tells every `fs.watch` subscriber of `run` that `change` happened.
    pub fn push_fs_change(&self, run: &RunId, change: FsChange) {
        let _ = self.shared.fs_tx.send((run.clone(), change));
    }

    /// How many `fs.watch` subscriptions are open now, across connections.
    pub fn fs_watches(&self) -> usize {
        self.shared.fs_watches.load(Ordering::SeqCst)
    }
}
