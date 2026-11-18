//! What History asks `git` for: the log, the refs, a blob. Read-only subprocesses on the Files
//! runner's hygiene (`files::git::command`: no hooks, no fsmonitor, no lock, no prompt, nothing
//! that redirects git, and the settings pinned) plus the settings a log is shaped by, pinned
//! here so a person's own configuration cannot change what is read.
//!
//! Nothing a client sent reaches git as a revision: the caller passes full object names it
//! checked ([`is_full_sha`]) and a range as `tip` and `base`.

use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use reno_proto::{GitCommit, GitRef, GitRefKind, author_key};
use tokio::io::AsyncReadExt;

use crate::files::FsError;
use crate::files::git::command;

/// The longest one git call may take.
const GIT_TIMEOUT: Duration = Duration::from_secs(20);
/// The most output of a log page or of the refs.
const MAX_OUTPUT: u64 = 16 * 1024 * 1024;
/// The most refs shown on one commit.
const MAX_REFS_PER_COMMIT: usize = 24;
/// The fields of one log record.
const FIELDS: usize = 6;

/// Settings that shape what `log`, `rev-list` and `cat-file` print, set on the command line.
const PINS: [&str; 16] = [
    "-c",
    "log.showSignature=false",
    "-c",
    "log.mailmap=false",
    "-c",
    "log.decorate=no",
    "-c",
    "log.date=default",
    "-c",
    "log.abbrevCommit=false",
    "-c",
    "format.pretty=medium",
    "-c",
    "i18n.logOutputEncoding=UTF-8",
    "-c",
    "core.abbrev=40",
];

/// What a finished git call left.
pub(crate) struct Output {
    pub(crate) code: Option<i32>,
    pub(crate) stdout: Vec<u8>,
    /// Stdout was longer than the cap asked for and was cut there.
    pub(crate) cut: bool,
}

/// Runs git with `args` in `dir`, keeping at most `cap` bytes of its output (bounded in time).
pub(crate) async fn run(dir: &Path, args: &[&str], cap: u64) -> Result<Output, FsError> {
    let mut child = command(dir)
        .args(PINS)
        .args(args)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| FsError::Git(format!("cannot run git: {error}")))?;
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let work = async {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let read_out = async {
            if let Some(out) = out.as_mut() {
                let _ = out.take(cap + 1).read_to_end(&mut stdout).await;
            }
            let cut = stdout.len() as u64 > cap;
            if cut {
                // More than was wanted: stop git now, or it waits on a pipe nobody reads and
                // keeps its stderr open.
                let _ = child.start_kill();
            }
            cut
        };
        let read_err = async {
            if let Some(err) = err.as_mut() {
                let _ = err.take(64 * 1024).read_to_end(&mut stderr).await;
            }
        };
        let (cut, ()) = tokio::join!(read_out, read_err);
        let status = child.wait().await;
        (status, stdout, cut)
    };
    let (status, mut stdout, cut) = tokio::time::timeout(GIT_TIMEOUT, work)
        .await
        .map_err(|_| FsError::Git(format!("git {} timed out", args.first().unwrap_or(&""))))?;
    let status = status.map_err(|error| FsError::Git(format!("git did not finish: {error}")))?;
    if cut {
        stdout.truncate(usize::try_from(cap).unwrap_or(usize::MAX));
    }
    Ok(Output {
        code: status.code(),
        stdout,
        cut,
    })
}

/// A full object name (SHA-1 or SHA-256), the only kind of revision this module is given.
pub(crate) fn is_full_sha(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether `dir` is in a repository.
pub(crate) async fn is_repo(dir: &Path) -> Result<bool, FsError> {
    Ok(run(dir, &["rev-parse", "--git-dir"], 4096).await?.code == Some(0))
}

/// Whether the repository is shallow.
pub(crate) async fn is_shallow(dir: &Path) -> bool {
    run(dir, &["rev-parse", "--is-shallow-repository"], 64)
        .await
        .is_ok_and(|out| out.stdout.starts_with(b"true"))
}

/// The commit `HEAD` is on; `None` in a repository with no commits yet.
pub(crate) async fn head(dir: &Path) -> Result<Option<String>, FsError> {
    commit_named(dir, "HEAD").await
}

/// The full name of the commit `name` is (a hexadecimal name of 4 to 64 characters, or `HEAD`);
/// `None` when it is none, or ambiguous.
pub(crate) async fn commit_named(dir: &Path, name: &str) -> Result<Option<String>, FsError> {
    let spec = format!("{name}^{{commit}}");
    let out = run(
        dir,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &spec,
        ],
        4096,
    )
    .await?;
    if out.code != Some(0) {
        return Ok(None);
    }
    let full = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    Ok(is_full_sha(&full).then_some(full))
}

/// The first parent of `commit`, none for a root commit.
pub(crate) async fn first_parent(dir: &Path, commit: &str) -> Result<Option<String>, FsError> {
    commit_named(dir, &format!("{commit}^")).await
}

/// Whether `ancestor` is reachable from `tip` (or is it).
pub(crate) async fn is_ancestor(dir: &Path, ancestor: &str, tip: &str) -> Result<bool, FsError> {
    let out = run(dir, &["merge-base", "--is-ancestor", ancestor, tip], 64).await?;
    Ok(out.code == Some(0))
}

/// How a log is ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Order {
    /// Every commit before all its parents, branches kept together: what a lane graph wants.
    Topo,
    /// By author time, newest first, parents still after their children: for a search.
    AuthorDate,
}

/// What a log looks for in a message or an author.
#[derive(Debug, Clone)]
pub(crate) enum Filter {
    /// The text in the message (subject or body), any case.
    Message(String),
    /// The text in an author's name or email, any case.
    Author(String),
}

/// A page of a log.
pub(crate) struct Page {
    pub(crate) commits: Vec<GitCommit>,
}

/// `at` is the full name of the tip; `base`, when given, is a full name whose history is left
/// out (`base..tip`); `skip` and `count` cut the page.
pub(crate) struct LogQuery<'a> {
    pub(crate) tip: &'a str,
    pub(crate) base: Option<&'a str>,
    pub(crate) skip: usize,
    pub(crate) count: usize,
    pub(crate) order: Order,
    pub(crate) filter: Option<&'a Filter>,
}

/// Commits of `query`, without their refs.
pub(crate) async fn log(dir: &Path, query: &LogQuery<'_>) -> Result<Page, FsError> {
    debug_assert!(is_full_sha(query.tip));
    let mut args: Vec<String> = vec![
        "log".into(),
        "-z".into(),
        "--no-color".into(),
        "--no-notes".into(),
        "--no-show-signature".into(),
        "--no-mailmap".into(),
        "--encoding=UTF-8".into(),
        "--format=%H%x00%P%x00%an%x00%ae%x00%at%x00%s".into(),
        match query.order {
            Order::Topo => "--topo-order".into(),
            Order::AuthorDate => "--author-date-order".into(),
        },
        format!("--skip={}", query.skip),
        format!("--max-count={}", query.count),
    ];
    match query.filter {
        Some(Filter::Message(text)) => {
            args.extend(["-F".into(), "-i".into(), format!("--grep={text}")]);
        }
        Some(Filter::Author(text)) => {
            args.extend(["-F".into(), "-i".into(), format!("--author={text}")]);
        }
        None => {}
    }
    args.push("--end-of-options".into());
    args.push(query.tip.to_owned());
    if let Some(base) = query.base {
        debug_assert!(is_full_sha(base));
        args.push(format!("^{base}"));
    }
    args.push("--".into());
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = run(dir, &refs, MAX_OUTPUT).await?;
    if out.code != Some(0) || out.cut {
        return Err(FsError::Git("git log failed".to_owned()));
    }
    Ok(Page {
        commits: parse_log(&out.stdout)?,
    })
}

/// Reads `git log -z --format=%H%x00%P%x00%an%x00%ae%x00%at%x00%s`: a flat run of NUL-ended
/// fields, six to a commit. Git forbids a NUL in any of them.
pub(crate) fn parse_log(bytes: &[u8]) -> Result<Vec<GitCommit>, FsError> {
    let text = String::from_utf8_lossy(bytes);
    let mut fields: Vec<&str> = text.split('\0').collect();
    // The terminator of the last record leaves one empty piece; so does an empty log.
    if fields
        .last()
        .is_some_and(|last| last.is_empty() || *last == "\n")
    {
        fields.pop();
    }
    if !fields.len().is_multiple_of(FIELDS) {
        return Err(FsError::Git(
            "git log printed something unexpected".to_owned(),
        ));
    }
    fields
        .chunks(FIELDS)
        .map(|record| {
            // After the first record, `-z` leaves no newline; before it, none either.
            let sha = record[0].trim_start_matches('\n');
            if !is_full_sha(sha) {
                return Err(FsError::Git("git log printed a bad commit name".to_owned()));
            }
            let seconds: u64 = record[4].trim().parse().unwrap_or(0);
            Ok(GitCommit {
                sha: sha.to_owned(),
                parents: record[1]
                    .split_ascii_whitespace()
                    .filter(|p| is_full_sha(p))
                    .map(str::to_owned)
                    .collect(),
                author_name: record[2].to_owned(),
                author_key: author_key(record[3]),
                time_ms: seconds.saturating_mul(1000),
                subject: record[5].to_owned(),
                refs: Vec::new(),
            })
        })
        .collect()
}

/// The branches, tags and remote branches, by the commit each points at.
pub(crate) async fn refs(dir: &Path) -> Result<HashMap<String, Vec<GitRef>>, FsError> {
    let out = run(
        dir,
        &[
            "for-each-ref",
            "--format=%(refname)%00%(objectname)%00%(*objectname)",
            "refs/heads",
            "refs/tags",
            "refs/remotes",
        ],
        MAX_OUTPUT,
    )
    .await?;
    if out.code != Some(0) {
        return Err(FsError::Git("git for-each-ref failed".to_owned()));
    }
    Ok(parse_refs(&out.stdout))
}

/// Reads `for-each-ref` output; an annotated tag is on the commit it peels to.
pub(crate) fn parse_refs(bytes: &[u8]) -> HashMap<String, Vec<GitRef>> {
    let mut by_commit: HashMap<String, Vec<GitRef>> = HashMap::new();
    for line in String::from_utf8_lossy(bytes).lines() {
        let mut parts = line.split('\0');
        let (Some(full), Some(object), peeled) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        let (name, kind) = if let Some(name) = full.strip_prefix("refs/heads/") {
            (name, GitRefKind::Branch)
        } else if let Some(name) = full.strip_prefix("refs/tags/") {
            (name, GitRefKind::Tag)
        } else if let Some(name) = full.strip_prefix("refs/remotes/") {
            // `origin/HEAD` only says which branch is the default: not a ref of its own.
            if name.ends_with("/HEAD") {
                continue;
            }
            (name, GitRefKind::Remote)
        } else {
            continue;
        };
        let target = peeled.filter(|p| !p.is_empty()).unwrap_or(object);
        by_commit
            .entry(target.to_owned())
            .or_default()
            .push(GitRef {
                name: name.to_owned(),
                kind,
            });
    }
    by_commit
}

/// Puts the refs on `commits`: `HEAD` first, then branches, tags and remote branches, each by
/// name, and no more than [`MAX_REFS_PER_COMMIT`].
pub(crate) fn decorate(
    commits: &mut [GitCommit],
    mut refs: HashMap<String, Vec<GitRef>>,
    head: Option<&str>,
) {
    for commit in commits {
        let mut mine = refs.remove(&commit.sha).unwrap_or_default();
        mine.sort_by(|a, b| (rank(a.kind), &a.name).cmp(&(rank(b.kind), &b.name)));
        if head == Some(commit.sha.as_str()) {
            mine.insert(
                0,
                GitRef {
                    name: "HEAD".to_owned(),
                    kind: GitRefKind::Head,
                },
            );
        }
        mine.truncate(MAX_REFS_PER_COMMIT);
        commit.refs = mine;
    }
}

fn rank(kind: GitRefKind) -> u8 {
    match kind {
        GitRefKind::Head => 0,
        GitRefKind::Branch => 1,
        GitRefKind::Tag => 2,
        GitRefKind::Remote => 3,
    }
}

/// The bytes of the blob `spec` names (`<commit>:<path>`), at most `cap`; whether it was cut.
/// `None` when nothing is there (the file is not in that commit, or is not a plain file).
pub(crate) async fn blob(
    dir: &Path,
    commit: &str,
    path: &str,
    cap: u64,
) -> Result<Option<(Vec<u8>, bool)>, FsError> {
    debug_assert!(is_full_sha(commit));
    let spec = format!("{commit}:{path}");
    let out = run(dir, &["cat-file", "blob", &spec], cap).await?;
    // A cut read ends with git killed, whatever it would have said.
    if out.code != Some(0) && !out.cut {
        return Ok(None);
    }
    Ok(Some((out.stdout, out.cut)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccc";

    #[test]
    fn a_log_is_read_six_fields_to_a_commit() {
        let text =
            format!("{A}\0{B} {C}\0Ada\0ada@x.io\0100\0merge it\0{B}\0\0Bob\0b@x.io\0abc\0root\0");
        let commits = parse_log(text.as_bytes()).unwrap();
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].parents, [B, C]);
        assert_eq!(commits[0].time_ms, 100_000);
        assert_eq!(commits[0].author_key, author_key("ada@x.io"));
        assert_eq!(commits[1].parents, Vec::<String>::new());
        assert_eq!(commits[1].time_ms, 0);
        assert!(parse_log(b"").unwrap().is_empty());
        assert!(parse_log(b"x\0y\0").is_err());
        assert!(parse_log(b"nothex\0\0n\0e\x001\0s\0").is_err());
    }

    #[test]
    fn refs_are_sorted_into_kinds_and_a_tag_sits_on_its_commit() {
        let text = format!(
            "refs/heads/main\0{A}\0\nrefs/tags/v1\0{C}\0{A}\nrefs/remotes/origin/main\0{A}\0\nrefs/remotes/origin/HEAD\0{A}\0\nrefs/heads/dev\0{B}\0\n"
        );
        let mut commits = vec![GitCommit {
            sha: A.into(),
            parents: vec![],
            subject: String::new(),
            author_name: String::new(),
            author_key: String::new(),
            time_ms: 0,
            refs: vec![],
        }];
        decorate(&mut commits, parse_refs(text.as_bytes()), Some(A));
        let shown: Vec<(&str, GitRefKind)> = commits[0]
            .refs
            .iter()
            .map(|r| (r.name.as_str(), r.kind))
            .collect();
        assert_eq!(
            shown,
            [
                ("HEAD", GitRefKind::Head),
                ("main", GitRefKind::Branch),
                ("v1", GitRefKind::Tag),
                ("origin/main", GitRefKind::Remote),
            ]
        );
    }

    #[test]
    fn only_full_object_names_are_revisions() {
        assert!(is_full_sha(A));
        assert!(!is_full_sha("main"));
        assert!(!is_full_sha("--output=x"));
        assert!(!is_full_sha(&A[..39]));
        assert!(!is_full_sha(&format!("{}g", &A[..39])));
    }
}
