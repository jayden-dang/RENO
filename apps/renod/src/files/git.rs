//! What the Files tab asks `git` for: the status, the file list for name search, and the content
//! search. One small runner, and parsers that are pure functions of git's bytes.
//!
//! Every call is read-only and takes no lock (`GIT_OPTIONAL_LOCKS=0`), so a status refresh never
//! makes a run's own `git add` fail with "index.lock exists".

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use reno_proto::{GitChange, GitFileStatus};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;

use super::error::FsError;

/// The longest one git call may take.
const GIT_TIMEOUT: Duration = Duration::from_secs(20);
/// The most output of a call that is read whole (the file list of a very large tree).
const MAX_OUTPUT: u64 = 64 * 1024 * 1024;

/// A git command in `dir`, read-only and quiet.
pub(crate) fn command(dir: &Path) -> Command {
    let mut git = Command::new("git");
    // The same hygiene as `reno_harness::proc::git_workspace` (no hooks, no fsmonitor daemon, no
    // lock, no prompt, nothing that redirects git), plus the settings the parsers below depend
    // on, pinned so a person's own git configuration cannot change what they read.
    git.current_dir(dir)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.quotepath=off",
            "-c",
            "status.renames=true",
            "-c",
            "grep.column=false",
            "-c",
            "grep.fullName=false",
            "-c",
            "color.ui=false",
        ])
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    for key in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_NAMESPACE",
        "GIT_PREFIX",
    ] {
        git.env_remove(key);
    }
    git
}

/// What a finished git call left.
#[derive(Debug)]
pub(crate) struct Output {
    pub(crate) code: Option<i32>,
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: String,
}

/// Runs git with `args` in `dir`, waiting for it (bounded).
pub(crate) async fn run(dir: &Path, args: &[&str]) -> Result<Output, FsError> {
    let mut child = command(dir)
        .args(args)
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| FsError::Git(format!("cannot run git: {error}")))?;
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let work = async {
        let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
        let read_out = async {
            if let Some(out) = out.as_mut() {
                let _ = out.take(MAX_OUTPUT).read_to_end(&mut stdout).await;
            }
        };
        let read_err = async {
            if let Some(err) = err.as_mut() {
                let _ = err.take(64 * 1024).read_to_end(&mut stderr).await;
            }
        };
        tokio::join!(read_out, read_err);
        let status = child.wait().await;
        (status, stdout, stderr)
    };
    let (status, stdout, stderr) = tokio::time::timeout(GIT_TIMEOUT, work)
        .await
        .map_err(|_| FsError::Git(format!("git {} timed out", args.first().unwrap_or(&""))))?;
    let status = status.map_err(|error| FsError::Git(format!("git did not finish: {error}")))?;
    Ok(Output {
        code: status.code(),
        stdout,
        stderr: String::from_utf8_lossy(&stderr).trim().to_owned(),
    })
}

/// Where `dir` stands in a repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RepoInfo {
    /// `dir` relative to the repository's top, with a trailing `/`; empty at the top.
    pub(crate) prefix: String,
    /// The repository's own git directory (for a linked worktree, its private one).
    pub(crate) git_dir: PathBuf,
}

/// The repository `dir` is in, or `None` when it is in none.
pub(crate) async fn repo_info(dir: &Path) -> Result<Option<RepoInfo>, FsError> {
    let out = run(dir, &["rev-parse", "--show-prefix", "--absolute-git-dir"]).await?;
    if out.code != Some(0) {
        // Not a repository, and the other refusals (dubious ownership, a damaged repo): all of
        // them mean there is no status to show.
        return Ok(None);
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let prefix = lines.next().unwrap_or_default().to_owned();
    let Some(git_dir) = lines.next().filter(|l| !l.is_empty()) else {
        return Ok(None);
    };
    Ok(Some(RepoInfo {
        prefix,
        git_dir: PathBuf::from(git_dir),
    }))
}

/// `git status --porcelain=v2` of `dir`: the branch and the changed paths (relative to `dir`),
/// sorted by path.
pub(crate) async fn status(
    dir: &Path,
    prefix: &str,
) -> Result<(Option<String>, Vec<GitFileStatus>), FsError> {
    let out = run(
        dir,
        &[
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
        ],
    )
    .await?;
    if out.code != Some(0) {
        return Err(FsError::Git(format!("git status: {}", out.stderr)));
    }
    Ok(parse_status(&out.stdout, prefix))
}

fn change(letter: char) -> GitChange {
    match letter {
        'M' => GitChange::Modified,
        'A' => GitChange::Added,
        'D' => GitChange::Deleted,
        'R' => GitChange::Renamed,
        'C' => GitChange::Copied,
        'T' => GitChange::TypeChanged,
        'U' => GitChange::Conflicted,
        _ => GitChange::Unmodified,
    }
}

/// `path` as seen from the directory whose repository-relative prefix is `prefix`; `None` when
/// it is outside that directory.
fn within(prefix: &str, path: &str) -> Option<String> {
    path.strip_prefix(prefix).map(str::to_owned)
}

/// Reads `--porcelain=v2 -z --branch` output. Entries outside `prefix` are dropped and the
/// rest are made relative to it.
pub(crate) fn parse_status(bytes: &[u8], prefix: &str) -> (Option<String>, Vec<GitFileStatus>) {
    let text = String::from_utf8_lossy(bytes);
    let mut records = text.split('\0');
    let mut branch = None;
    let mut files = Vec::new();
    while let Some(record) = records.next() {
        if let Some(head) = record.strip_prefix("# branch.head ") {
            branch = (head != "(detached)").then(|| head.to_owned());
            continue;
        }
        let mut kind = record.chars();
        match (kind.next(), kind.next()) {
            (Some('1'), Some(' ')) => {
                // 1 XY sub mH mI mW hH hI path
                let fields: Vec<&str> = record.splitn(9, ' ').collect();
                if let [_, xy, .., path] = fields[..]
                    && let Some(path) = within(prefix, path)
                {
                    files.push(entry(path, None, xy));
                }
            }
            (Some('2'), Some(' ')) => {
                // 2 XY sub mH mI mW hH hI Xscore path, then the original path as its own record.
                let from = records.next().unwrap_or_default();
                let fields: Vec<&str> = record.splitn(10, ' ').collect();
                if let [_, xy, .., path] = fields[..]
                    && let Some(path) = within(prefix, path)
                {
                    let from = within(prefix, from).unwrap_or_else(|| from.to_owned());
                    files.push(entry(path, Some(from), xy));
                }
            }
            (Some('u'), Some(' ')) => {
                // u XY sub m1 m2 m3 mW h1 h2 h3 path
                let fields: Vec<&str> = record.splitn(11, ' ').collect();
                if let Some(path) = fields.last().and_then(|p| within(prefix, p)) {
                    files.push(GitFileStatus {
                        path,
                        old_path: None,
                        index: GitChange::Conflicted,
                        worktree: GitChange::Conflicted,
                    });
                }
            }
            (Some('?'), Some(' ')) => {
                if let Some(path) = record.get(2..).and_then(|p| within(prefix, p)) {
                    files.push(GitFileStatus {
                        path,
                        old_path: None,
                        index: GitChange::Untracked,
                        worktree: GitChange::Untracked,
                    });
                }
            }
            _ => {}
        }
    }
    files.sort_by(|a, b| a.path.cmp(&b.path));
    (branch, files)
}

fn entry(path: String, old_path: Option<String>, xy: &str) -> GitFileStatus {
    let mut sides = xy.chars();
    GitFileStatus {
        path,
        old_path,
        index: change(sides.next().unwrap_or('.')),
        worktree: change(sides.next().unwrap_or('.')),
    }
}

/// The files of `dir` that git tracks or that are not ignored, relative to `dir`. `None` when
/// `dir` is in no repository.
pub(crate) async fn list_files(dir: &Path) -> Result<Option<Vec<String>>, FsError> {
    let out = run(
        dir,
        &[
            "ls-files",
            "-z",
            "--cached",
            "--others",
            "--exclude-standard",
        ],
    )
    .await?;
    if out.code != Some(0) {
        return Ok(None);
    }
    let mut files: Vec<String> = out
        .stdout
        .split(|b| *b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect();
    files.sort();
    files.dedup();
    Ok(Some(files))
}

/// Which of `paths` (relative to `dir`; a directory written with a trailing `/`) git ignores.
/// Tracked paths are never ignored. Empty when `dir` is in no repository.
pub(crate) async fn ignored(
    dir: &Path,
    paths: &[String],
) -> Result<std::collections::HashSet<String>, FsError> {
    if paths.is_empty() {
        return Ok(std::collections::HashSet::new());
    }
    let mut child = command(dir)
        .args(["check-ignore", "-z", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| FsError::Git(format!("cannot run git: {error}")))?;
    let mut input = Vec::new();
    for path in paths {
        input.extend_from_slice(path.as_bytes());
        input.push(0);
    }
    let mut stdin = child.stdin.take();
    let mut stdout = child.stdout.take();
    let work = async {
        let write = async {
            if let Some(stdin) = stdin.as_mut() {
                let _ = tokio::io::AsyncWriteExt::write_all(stdin, &input).await;
            }
            // Closing stdin lets git finish.
            drop(stdin.take());
        };
        let mut out = Vec::new();
        let read = async {
            if let Some(stdout) = stdout.as_mut() {
                let _ = stdout.take(MAX_OUTPUT).read_to_end(&mut out).await;
            }
        };
        tokio::join!(write, read);
        let status = child.wait().await;
        (status, out)
    };
    let (status, out) = tokio::time::timeout(GIT_TIMEOUT, work)
        .await
        .map_err(|_| FsError::Git("git check-ignore timed out".to_owned()))?;
    // 0: some are ignored; 1: none are; anything else: not a repository, or git failed.
    if !matches!(status.map(|s| s.code()), Ok(Some(0))) {
        return Ok(std::collections::HashSet::new());
    }
    Ok(out
        .split(|b| *b == 0)
        .filter(|name| !name.is_empty())
        .map(|name| String::from_utf8_lossy(name).into_owned())
        .collect())
}

/// What a content search asks for.
#[derive(Debug, Clone)]
pub(crate) struct Grep<'a> {
    pub(crate) query: &'a str,
    pub(crate) regex: bool,
    pub(crate) ignore_case: bool,
    pub(crate) limit: usize,
}

/// One matching line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hit {
    pub(crate) path: String,
    pub(crate) line: u32,
    /// Byte offset of the first match, from 0.
    pub(crate) column: u32,
    pub(crate) text: String,
}

/// The longest text of a hit.
const HIT_CHARS: usize = 500;

/// `git grep` in `dir`: tracked files and untracked ones that are not ignored (a folder that is no
/// repository is searched as it is). Stops after `limit + 1` hits and says whether there were
/// more. Binary files are skipped.
pub(crate) async fn grep(dir: &Path, wanted: &Grep<'_>) -> Result<(Vec<Hit>, bool), FsError> {
    match grep_in(dir, wanted, false).await? {
        Some(done) => Ok(done),
        None => Ok(grep_in(dir, wanted, true).await?.unwrap_or_default()),
    }
}

/// `None` when git says `dir` is no repository (and `no_index` was not asked).
async fn grep_in(
    dir: &Path,
    wanted: &Grep<'_>,
    no_index: bool,
) -> Result<Option<(Vec<Hit>, bool)>, FsError> {
    let mut git = command(dir);
    git.args(["grep", "-n", "--column", "-z", "-I", "--no-color"]);
    if no_index {
        git.args(["--no-index", "--exclude-standard"]);
    } else {
        git.arg("--untracked");
    }
    git.arg(if wanted.regex { "-E" } else { "-F" });
    if wanted.ignore_case {
        git.arg("-i");
    }
    git.arg("-e").arg(wanted.query);
    git.stdout(Stdio::piped());
    let mut child = git
        .spawn()
        .map_err(|error| FsError::Git(format!("cannot run git: {error}")))?;
    let Some(stdout) = child.stdout.take() else {
        return Err(FsError::Git("git grep has no output".to_owned()));
    };
    let mut stderr = child.stderr.take();
    let mut reader = BufReader::new(stdout);
    let mut hits = Vec::new();
    let mut more = false;
    let read = async {
        let mut line = Vec::new();
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line).await {
                Ok(0) | Err(_) => return,
                Ok(_) => {}
            }
            if line.last() == Some(&b'\n') {
                line.pop();
            }
            if let Some(hit) = parse_hit(&line) {
                if hits.len() >= wanted.limit {
                    more = true;
                    return;
                }
                hits.push(hit);
            }
        }
    };
    let timed_out = tokio::time::timeout(GIT_TIMEOUT, read).await.is_err();
    if more || timed_out {
        // Enough, or too slow: what was found is the answer.
        let _ = child.kill().await;
        return Ok(Some((hits, more || timed_out)));
    }
    let status = child
        .wait()
        .await
        .map_err(|error| FsError::Git(format!("git did not finish: {error}")))?;
    let mut message = String::new();
    if let Some(stderr) = stderr.as_mut() {
        let _ = stderr.take(64 * 1024).read_to_string(&mut message).await;
    }
    match status.code() {
        // 0: matches; 1: none.
        Some(0 | 1) => Ok(Some((hits, false))),
        _ if !no_index && message.contains("not a git repository") => Ok(None),
        // Git words a bad pattern differently from one version to the next, so a regular
        // expression that fails to compile is told by elimination: it was a pattern search and
        // the failure was not about the repository.
        Some(128)
            if wanted.regex
                && !message.contains("repository")
                && !message.contains("ownership") =>
        {
            Err(FsError::BadPattern(message.trim().to_owned()))
        }
        _ => Err(FsError::Git(format!("git grep: {}", message.trim()))),
    }
}

/// `path NUL line NUL column NUL text` of one `git grep -n --column -z` line.
fn parse_hit(line: &[u8]) -> Option<Hit> {
    let mut parts = line.splitn(4, |b| *b == 0);
    let path = String::from_utf8_lossy(parts.next()?).into_owned();
    let number: u32 = std::str::from_utf8(parts.next()?).ok()?.parse().ok()?;
    let column: u32 = std::str::from_utf8(parts.next()?).ok()?.parse().ok()?;
    let text = String::from_utf8_lossy(parts.next()?);
    Some(Hit {
        path,
        line: number,
        column: column.saturating_sub(1),
        text: text.chars().take(HIT_CHARS).collect(),
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn z(records: &[&str]) -> Vec<u8> {
        let mut out = Vec::new();
        for record in records {
            out.extend_from_slice(record.as_bytes());
            out.push(0);
        }
        out
    }

    #[test]
    fn status_records_become_two_sided_states() {
        let bytes = z(&[
            "# branch.oid abc",
            "# branch.head reno/thr_1",
            "1 .M N... 100644 100644 100644 aaa bbb src/lib.rs",
            "1 A. N... 000000 100644 100644 000 bbb new file.rs",
            "1 MD N... 100644 100644 000000 aaa bbb both.rs",
            "1 .D N... 100644 100644 000000 aaa bbb gone.rs",
            "1 .T N... 100644 120000 120000 aaa bbb link",
            "2 R. N... 100644 100644 100644 aaa bbb R100 moved.rs",
            "old.rs",
            "u UU N... 100644 100644 100644 100644 a b c clash.rs",
            "? untracked.txt",
            "! ignored.o",
        ]);
        let (branch, files) = parse_status(&bytes, "");
        assert_eq!(branch.as_deref(), Some("reno/thr_1"));
        let find = |p: &str| files.iter().find(|f| f.path == p).unwrap();
        assert_eq!(
            (find("src/lib.rs").index, find("src/lib.rs").worktree),
            (GitChange::Unmodified, GitChange::Modified)
        );
        assert_eq!(find("new file.rs").index, GitChange::Added);
        assert_eq!(
            (find("both.rs").index, find("both.rs").worktree),
            (GitChange::Modified, GitChange::Deleted)
        );
        assert_eq!(find("gone.rs").worktree, GitChange::Deleted);
        assert_eq!(find("link").worktree, GitChange::TypeChanged);
        let moved = find("moved.rs");
        assert_eq!(
            (moved.index, moved.old_path.as_deref()),
            (GitChange::Renamed, Some("old.rs"))
        );
        assert_eq!(find("clash.rs").index, GitChange::Conflicted);
        assert_eq!(find("untracked.txt").worktree, GitChange::Untracked);
        assert!(files.iter().all(|f| f.path != "ignored.o"));
        // Sorted by path.
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        let mut sorted = paths.clone();
        sorted.sort_unstable();
        assert_eq!(paths, sorted);
    }

    #[test]
    fn a_detached_head_has_no_branch_and_a_subdirectory_sees_only_its_own_paths() {
        let bytes = z(&[
            "# branch.head (detached)",
            "1 .M N... 100644 100644 100644 a b src/a.rs",
            "1 .M N... 100644 100644 100644 a b docs/b.md",
            "? src/new.rs",
        ]);
        let (branch, files) = parse_status(&bytes, "src/");
        assert_eq!(branch, None);
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(paths, ["a.rs", "new.rs"]);
    }

    #[test]
    fn a_grep_line_splits_on_its_separators_and_cuts_a_long_text() {
        let hit = parse_hit(b"d e/b c.txt\x007\x003\x00x: hello").unwrap();
        assert_eq!(
            hit,
            Hit {
                path: "d e/b c.txt".into(),
                line: 7,
                column: 2,
                text: "x: hello".into()
            }
        );
        let mut long = b"a.txt\x001\x001\x00".to_vec();
        long.extend(std::iter::repeat_n(b'x', 900));
        assert_eq!(parse_hit(&long).unwrap().text.chars().count(), HIT_CHARS);
        assert_eq!(parse_hit(b"no separators"), None);
    }
}
