//! The fake daemon's History (RP3): `git.log`, `git.log_search` and `workspace.file_sources`,
//! over a small deterministic history held in memory. No real `git`: the same answer every time.
//!
//! Which history a run gets is fixed by its id, like its workspace ([`super::changes`]):
//!
//! - a run with a worktree (`Full`, `Empty`) gets the demo history below: six commits, one of them
//!   a merge, with tags and a remote branch;
//! - `builder0`'s worktree was removed: every method answers `WorkspaceUnavailable`;
//! - `reviewer1` and `architect1` have no worktree: `git.log` says it is no repository (flagged
//!   `inPlace`), and the scopes that need a worktree are refused;
//! - a run the fake does not know is `NotFound`.
//!
//! The demo history, newest first (each commit lists its parents):
//!
//! ```text
//! M  merge side branch        (F, S)   HEAD, reno/<thread>
//! F  kernel: renew leases     (B)
//! S  docs: note on leases     (B)
//! B  kernel: lease table      (C)      main, origin/main
//! C  ci: add a workflow       (A)      v0.2 (lightweight)
//! A  init                     ()       v0.1 (annotated)
//! ```
//!
//! The thread's own commits are `M`, `F` and `S` (those after `B`).
//!
//! `workspace.file_sources` answers for the files of the demo diff ([`super::changes`]), with the
//! checksum of that diff.

use std::sync::Arc;

use reno_proto::{
    ChangeKind, DiffScope, ErrorCode, ErrorObject, FILE_SOURCE_MAX_BYTES, FileSourcesParams,
    FileSourcesResult, GIT_LOG_DEFAULT_LIMIT, GIT_LOG_MAX_LIMIT, GIT_LOG_QUERY_MAX_BYTES,
    GitCommit, GitLog, GitLogParams, GitLogResult, GitLogRev, GitLogSearch, GitLogSearchParams,
    GitLogSearchResult, GitRef, GitRefKind, RunId, WorkspaceFileSources, author_key, image_hash,
};
use reno_rpc::Router;

use super::changes::{Fixture, fixture};
use super::{Shared, injected};

/// The full names of the demo commits, oldest first.
const SHAS: [&str; 6] = [
    "a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1",
    "c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2",
    "b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3b3",
    "5454545454545454545454545454545454545454",
    "f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5f5",
    "e6e6e6e6e6e6e6e6e6e6e6e6e6e6e6e6e6e6e6e6",
];
/// A thread's base commit in the demo history (`B`).
const BASE_INDEX: usize = 2;
const DAY_MS: u64 = 86_400_000;

fn commit(
    index: usize,
    parents: &[usize],
    subject: &str,
    author: (&str, &str),
    refs: &[(&str, GitRefKind)],
) -> GitCommit {
    GitCommit {
        sha: SHAS[index].to_owned(),
        parents: parents.iter().map(|p| SHAS[*p].to_owned()).collect(),
        subject: subject.to_owned(),
        author_name: author.0.to_owned(),
        author_key: author_key(author.1),
        time_ms: super::data::NOW_MS - (6 - index as u64) * DAY_MS,
        refs: refs
            .iter()
            .map(|(name, kind)| GitRef {
                name: (*name).to_owned(),
                kind: *kind,
            })
            .collect(),
    }
}

/// The demo history, newest first.
fn history(run: &RunId) -> Vec<GitCommit> {
    let (ada, bob) = (("Ada", "ada@demo.invalid"), ("Bob", "bob@demo.invalid"));
    let branch = format!("reno/{}", run.as_str().trim_start_matches("run_"));
    vec![
        commit(
            5,
            &[4, 3],
            "Merge the side branch",
            bob,
            &[("HEAD", GitRefKind::Head), (&branch, GitRefKind::Branch)],
        ),
        commit(4, &[2], "kernel: renew leases on wake", ada, &[]),
        commit(3, &[2], "docs: note on leases", bob, &[]),
        commit(
            2,
            &[1],
            "kernel: lease table",
            ada,
            &[
                ("main", GitRefKind::Branch),
                ("origin/main", GitRefKind::Remote),
            ],
        ),
        commit(
            1,
            &[0],
            "ci: add a workflow",
            bob,
            &[("v0.2", GitRefKind::Tag)],
        ),
        commit(0, &[], "init", ada, &[("v0.1", GitRefKind::Tag)]),
    ]
}

/// Where a run is served from, or why not.
struct Place {
    in_place: bool,
    repo: bool,
}

fn place(s: &Shared, run: &RunId) -> Result<Place, ErrorObject> {
    if s.read(|w| w.run(run).is_none()) {
        return Err(ErrorObject::new(ErrorCode::NotFound, "no such run"));
    }
    match fixture(run) {
        Fixture::Full | Fixture::Empty => Ok(Place {
            in_place: false,
            repo: true,
        }),
        Fixture::Missing => Ok(Place {
            in_place: true,
            repo: false,
        }),
        Fixture::Removed => Err(ErrorObject::new(
            ErrorCode::WorkspaceUnavailable,
            "the files of this run are unavailable: its worktree was removed",
        )),
    }
}

fn typed(code: ErrorCode, message: &str, kind: &str) -> ErrorObject {
    let mut object = ErrorObject::new(code, message);
    object.data = Some(serde_json::json!({ "kind": kind }));
    object
}

/// The commits a `rev` lists, newest first.
fn scoped(
    s: &Shared,
    run: &RunId,
    place: &Place,
    rev: Option<GitLogRev>,
) -> Result<Vec<GitCommit>, ErrorObject> {
    let all = history(run);
    match rev.unwrap_or_default() {
        GitLogRev::Branch => Ok(all),
        _ if place.in_place => Err(typed(
            ErrorCode::InvalidParams,
            "this run has no worktree of its own, so the scope has nothing to compare",
            "needs_worktree",
        )),
        GitLogRev::Thread => Ok(all.into_iter().take(SHAS.len() - 1 - BASE_INDEX).collect()),
        GitLogRev::Run => {
            let follow_up = s.read(|w| w.run(run).is_some_and(|r| r.parent.is_some()));
            let take = if follow_up {
                1
            } else {
                SHAS.len() - 1 - BASE_INDEX
            };
            Ok(all.into_iter().take(take).collect())
        }
    }
}

fn bad_cursor() -> ErrorObject {
    typed(ErrorCode::InvalidParams, "invalid cursor", "bad_cursor")
}

fn log(s: &Shared, p: &GitLogParams, place: &Place) -> Result<GitLogResult, ErrorObject> {
    let empty = |repo| GitLogResult {
        commits: Vec::new(),
        next_cursor: None,
        head: None,
        repo,
        shallow: false,
        in_place: place.in_place,
    };
    let commits = scoped(s, &p.run_id, place, p.rev)?;
    if !place.repo {
        return Ok(empty(false));
    }
    let skip = match &p.cursor {
        None => 0,
        Some(cursor) => {
            let mut parts = cursor.split('.');
            let (Some("h1"), Some(tip), Some(skip), None) =
                (parts.next(), parts.next(), parts.next(), parts.next())
            else {
                return Err(bad_cursor());
            };
            if tip != SHAS[5] {
                return Err(bad_cursor());
            }
            skip.parse::<usize>().map_err(|_| bad_cursor())?
        }
    };
    let limit = p
        .limit
        .unwrap_or(GIT_LOG_DEFAULT_LIMIT)
        .clamp(1, GIT_LOG_MAX_LIMIT) as usize;
    let more = commits.len() > skip + limit;
    let page: Vec<GitCommit> = commits.into_iter().skip(skip).take(limit).collect();
    Ok(GitLogResult {
        next_cursor: more.then(|| format!("h1.{}.{}", SHAS[5], skip + limit)),
        head: Some(SHAS[5].to_owned()),
        commits: page,
        repo: true,
        shallow: false,
        in_place: place.in_place,
    })
}

fn search(
    s: &Shared,
    p: &GitLogSearchParams,
    place: &Place,
) -> Result<GitLogSearchResult, ErrorObject> {
    let query = p.query.trim();
    if query.is_empty()
        || query.len() > GIT_LOG_QUERY_MAX_BYTES
        || query.chars().any(|c| c.is_control() && c != '\t')
    {
        return Err(typed(
            ErrorCode::InvalidParams,
            "invalid search query",
            "bad_query",
        ));
    }
    let commits = scoped(s, &p.run_id, place, p.rev)?;
    if !place.repo {
        return Ok(GitLogSearchResult {
            commits: Vec::new(),
            truncated: false,
            repo: false,
            in_place: place.in_place,
        });
    }
    let needle = query.to_lowercase();
    let mut found: Vec<GitCommit> = commits
        .into_iter()
        .filter(|c| {
            c.subject.to_lowercase().contains(&needle)
                || c.author_name.to_lowercase().contains(&needle)
                || (needle.len() >= 4 && c.sha.starts_with(&needle))
        })
        .collect();
    let limit = p
        .limit
        .unwrap_or(GIT_LOG_DEFAULT_LIMIT)
        .clamp(1, GIT_LOG_MAX_LIMIT) as usize;
    let truncated = found.len() > limit;
    found.truncate(limit);
    Ok(GitLogSearchResult {
        commits: found,
        truncated,
        repo: true,
        in_place: place.in_place,
    })
}

/// The two sides of a file of the demo diff.
fn texts(path: &str) -> (Option<&'static str>, Option<&'static str>) {
    match path {
        "Cargo.toml" => (
            Some("[workspace]\nresolver = \"1\"\n"),
            Some("[workspace]\nresolver = \"3\"\n"),
        ),
        "apps/renod/src/main.rs" => (
            Some("fn start() {\n    run();\n}\n\nfn run() {}\n"),
            Some(
                "fn main() {\n    let args = std::env::args();\n    run();\n    drop(args);\n}\n\nfn run() {}\n",
            ),
        ),
        "crates/reno-kernel/src/lease.rs" => (
            Some("pub struct Lease;\n"),
            Some("pub struct Lease {\n    pub renewed: bool,\n}\n"),
        ),
        "crates/reno-kernel/src/lease_tests.rs" => (None, Some("#[test]\nfn renews() {}\n")),
        "docs/old-notes.md" => (Some("# Old notes\n\nGone.\n"), None),
        _ => (None, None),
    }
}

fn file_sources(s: &Shared, p: &FileSourcesParams) -> Result<FileSourcesResult, ErrorObject> {
    let place = place(s, &p.run_id)?;
    if place.in_place {
        return Err(ErrorObject::new(
            ErrorCode::WorkspaceUnavailable,
            "this run has no worktree of its own, so it has no diff",
        ));
    }
    let scope: DiffScope = p.scope.clone().unwrap_or_default();
    let diff = s
        .read(|w| w.diff_for(&p.run_id, Some(&p.path), &scope))?
        .ok_or_else(|| ErrorObject::new(ErrorCode::NotFound, "no such workspace"))?;
    let entry = diff
        .files
        .iter()
        .find(|f| f.path == p.path)
        .ok_or_else(|| {
            typed(
                ErrorCode::NotFound,
                &format!("{:?} is not a changed file of this diff", p.path),
                "not_changed",
            )
        })?;
    let (old, new) = texts(&entry.path);
    let (old, new) = (
        old.filter(|_| entry.kind != ChangeKind::Added),
        new.filter(|_| entry.kind != ChangeKind::Deleted),
    );
    let hash = |text: Option<&str>| text.map(|t| image_hash(t.as_bytes()));
    let shown = |text: Option<&str>| {
        text.filter(|_| !entry.binary)
            .map(|t| t[..t.len().min(FILE_SOURCE_MAX_BYTES)].to_owned())
    };
    Ok(FileSourcesResult {
        path: entry.path.clone(),
        old_path: entry.from.clone(),
        old_text: shown(old),
        new_text: shown(new),
        old_hash: hash(old).filter(|_| !entry.binary),
        new_hash: hash(new).filter(|_| !entry.binary),
        stale: p.checksum.as_ref().is_some_and(|c| *c != diff.checksum),
        checksum: diff.checksum,
        binary: entry.binary,
        truncated: false,
    })
}

pub(super) fn add(router: Router, shared: &Arc<Shared>) -> Router {
    let s = Arc::clone(shared);
    let router = router.method::<GitLog, _, _>(move |p: GitLogParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "git.log")?;
            let place = place(&s, &p.run_id)?;
            log(&s, &p, &place)
        }
    });
    let s = Arc::clone(shared);
    let router = router.method::<GitLogSearch, _, _>(move |p: GitLogSearchParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "git.log_search")?;
            let place = place(&s, &p.run_id)?;
            search(&s, &p, &place)
        }
    });
    let s = Arc::clone(shared);
    router.method::<WorkspaceFileSources, _, _>(move |p: FileSourcesParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "workspace.file_sources")?;
            file_sources(&s, &p)
        }
    })
}
