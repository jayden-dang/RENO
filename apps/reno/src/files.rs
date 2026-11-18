//! `reno fs ls|cat|find|grep` and `reno git status`: a run's worktree, read-only (RP1).
//!
//! Each is a few client calls and some printing (ARCH-2): the same methods the app's Files tab
//! uses. Paths are relative to the run's worktree; the daemon enforces the sandbox.

use std::io::{self, Write};
use std::process::ExitCode;

use anyhow::bail;
use clap::{Args, Subcommand};
use reno_client::{Client, ClientError};
use reno_proto::{
    FS_LIST_MAX_LIMIT, FsEntry, FsEntryKind, FsListParams, FsListResult, FsReadParams,
    FsSearchContentParams, FsSearchFilesParams, GitChange, GitFileStatus, GitStatusResult, RunId,
};

use crate::env::{Env, print_json, print_line, print_note};

#[derive(Debug, Subcommand)]
pub(crate) enum FsCommand {
    /// List a directory of a run's worktree, directories first. Every page is fetched.
    Ls(FsLs),
    /// Print a text file of a run's worktree. A binary file is refused; a file over 1 MiB is
    /// cut, with a note on stderr.
    Cat(FsCat),
    /// Find files by name (fuzzy), best match first. Ignored files are left out unless git
    /// tracks them.
    Find(FsFind),
    /// Find lines that contain a text, with `git grep`: `path:line:text`. Exit status 1 when
    /// nothing matches.
    Grep(FsGrep),
}

#[derive(Debug, Args)]
pub(crate) struct FsLs {
    /// The run.
    pub(crate) run: RunId,
    /// The directory, relative to the worktree root [default: the root].
    #[arg(default_value = "")]
    pub(crate) path: String,
}

#[derive(Debug, Args)]
pub(crate) struct FsCat {
    /// The run.
    pub(crate) run: RunId,
    /// The file, relative to the worktree root.
    pub(crate) path: String,
}

#[derive(Debug, Args)]
pub(crate) struct FsFind {
    /// The run.
    pub(crate) run: RunId,
    /// The characters of the path, in order.
    pub(crate) query: String,
    /// At most this many.
    #[arg(long, value_name = "N")]
    pub(crate) limit: Option<u32>,
}

#[derive(Debug, Args)]
pub(crate) struct FsGrep {
    /// The run.
    pub(crate) run: RunId,
    /// The text to find.
    pub(crate) query: String,
    /// Read the query as an extended regular expression.
    #[arg(long)]
    pub(crate) regex: bool,
    /// Ignore case.
    #[arg(short = 'i', long)]
    pub(crate) ignore_case: bool,
    /// At most this many lines.
    #[arg(long, value_name = "N")]
    pub(crate) limit: Option<u32>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum GitCommand {
    /// What changed in a run's worktree, staged and not: two letters (index, worktree) and the
    /// path, as `git status --short`.
    Status(GitStatusArgs),
}

#[derive(Debug, Args)]
pub(crate) struct GitStatusArgs {
    /// The run.
    pub(crate) run: RunId,
}

pub(crate) async fn fs(env: &Env, command: FsCommand) -> anyhow::Result<ExitCode> {
    match command {
        FsCommand::Ls(args) => ls(env, args).await,
        FsCommand::Cat(args) => cat(env, args).await,
        FsCommand::Find(args) => find(env, args).await,
        FsCommand::Grep(args) => grep(env, args).await,
    }
}

pub(crate) async fn git(env: &Env, command: GitCommand) -> anyhow::Result<ExitCode> {
    match command {
        GitCommand::Status(args) => status(env, args).await,
    }
}

/// Every page of a directory.
async fn list_all(client: &Client, run: RunId, dir: String) -> Result<FsListResult, ClientError> {
    let mut entries = Vec::new();
    let mut cursor = None;
    loop {
        let page = client
            .fs_list(FsListParams {
                run_id: run.clone(),
                dir: dir.clone(),
                cursor: cursor.take(),
                limit: Some(FS_LIST_MAX_LIMIT),
            })
            .await?;
        entries.extend(page.entries);
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => {
                return Ok(FsListResult {
                    entries,
                    next_cursor: None,
                    in_place: page.in_place,
                });
            }
        }
    }
}

async fn ls(env: &Env, args: FsLs) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let listed = list_all(&client, args.run, args.path)
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&listed)?;
        return Ok(ExitCode::SUCCESS);
    }
    if listed.in_place {
        print_note("(the run has no worktree: listing its own folder, read-only)")?;
    }
    for entry in &listed.entries {
        print_line(entry_line(entry))?;
    }
    Ok(ExitCode::SUCCESS)
}

/// `d  src/`, `f  1234  README.md`, `l  5  link`: kind, size, name.
fn entry_line(entry: &FsEntry) -> String {
    let (letter, name) = match entry.kind {
        FsEntryKind::Dir => ('d', format!("{}/", entry.name)),
        FsEntryKind::File => ('f', entry.name.clone()),
        FsEntryKind::Symlink => ('l', entry.name.clone()),
    };
    match entry.size {
        Some(size) => format!("{letter}  {size:>10}  {name}"),
        None => format!("{letter}  {:>10}  {name}", "-"),
    }
}

async fn cat(env: &Env, args: FsCat) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let read = client
        .fs_read(FsReadParams {
            run_id: args.run,
            path: args.path.clone(),
        })
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&read)?;
        return Ok(if read.binary {
            ExitCode::FAILURE
        } else {
            ExitCode::SUCCESS
        });
    }
    let Some(text) = &read.text else {
        bail!("{}: a binary file ({} bytes)", args.path, read.size);
    };
    // As it is: no newline added, so the output is the file.
    io::stdout().lock().write_all(text.as_bytes())?;
    if read.truncated {
        print_note(format!(
            "(cut: {} of {} bytes shown)",
            text.len(),
            read.size
        ))?;
    }
    Ok(ExitCode::SUCCESS)
}

async fn find(env: &Env, args: FsFind) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let found = client
        .fs_search_files(FsSearchFilesParams {
            run_id: args.run,
            query: args.query,
            limit: args.limit,
        })
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&found)?;
    } else {
        for file in &found.files {
            print_line(&file.path)?;
        }
        if found.truncated {
            print_note("(more files match: raise --limit)")?;
        }
    }
    Ok(if found.files.is_empty() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

async fn grep(env: &Env, args: FsGrep) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let found = client
        .fs_search_content(FsSearchContentParams {
            run_id: args.run,
            query: args.query,
            regex: args.regex,
            ignore_case: args.ignore_case,
            limit: args.limit,
        })
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&found)?;
    } else {
        for hit in &found.matches {
            print_line(format!("{}:{}:{}", hit.path, hit.line, hit.text))?;
        }
        if found.truncated {
            print_note("(more lines match: raise --limit)")?;
        }
    }
    Ok(if found.matches.is_empty() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

async fn status(env: &Env, args: GitStatusArgs) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let status = client
        .git_status(args.run)
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&status)?;
        return Ok(ExitCode::SUCCESS);
    }
    for line in status_lines(&status) {
        print_line(line)?;
    }
    Ok(ExitCode::SUCCESS)
}

/// One letter for one side of a path, as `git status --short` writes it.
fn letter(change: GitChange) -> char {
    match change {
        GitChange::Unmodified => ' ',
        GitChange::Modified => 'M',
        GitChange::Added => 'A',
        GitChange::Deleted => 'D',
        GitChange::Renamed => 'R',
        GitChange::Copied => 'C',
        GitChange::TypeChanged => 'T',
        GitChange::Untracked => '?',
        GitChange::Conflicted => 'U',
    }
}

fn file_line(file: &GitFileStatus) -> String {
    let codes = format!("{}{}", letter(file.index), letter(file.worktree));
    match &file.old_path {
        Some(old) => format!("{codes} {old} -> {}", file.path),
        None => format!("{codes} {}", file.path),
    }
}

/// The header and the changed paths.
fn status_lines(status: &GitStatusResult) -> Vec<String> {
    let mut lines = Vec::new();
    if !status.repo {
        lines.push("not a git repository (the run's folder is not in one)".to_owned());
        return lines;
    }
    lines.push(match &status.branch {
        Some(branch) => format!("On branch {branch}"),
        None => "HEAD is detached".to_owned(),
    });
    if status.in_place {
        lines.push("(the run has no worktree: its own folder, read-only)".to_owned());
    }
    if status.files.is_empty() {
        lines.push("nothing changed".to_owned());
    }
    lines.extend(status.files.iter().map(file_line));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, old: Option<&str>, index: GitChange, worktree: GitChange) -> GitFileStatus {
        GitFileStatus {
            path: path.into(),
            old_path: old.map(Into::into),
            index,
            worktree,
        }
    }

    #[test]
    fn status_reads_like_git_status_short() {
        let status = GitStatusResult {
            branch: Some("reno/thr_1".into()),
            repo: true,
            files: vec![
                file("a.rs", None, GitChange::Unmodified, GitChange::Modified),
                file("b.rs", Some("c.rs"), GitChange::Renamed, GitChange::Unmodified),
                file("d.rs", None, GitChange::Added, GitChange::Deleted),
                file("u.txt", None, GitChange::Untracked, GitChange::Untracked),
            ],
            in_place: false,
        };
        assert_eq!(
            status_lines(&status),
            [
                "On branch reno/thr_1",
                " M a.rs",
                "R  c.rs -> b.rs",
                "AD d.rs",
                "?? u.txt"
            ]
        );
        let clean = GitStatusResult {
            files: vec![],
            in_place: true,
            ..status
        };
        assert_eq!(
            status_lines(&clean),
            [
                "On branch reno/thr_1",
                "(the run has no worktree: its own folder, read-only)",
                "nothing changed"
            ]
        );
        let none = GitStatusResult {
            branch: None,
            repo: false,
            files: vec![],
            in_place: true,
        };
        assert_eq!(status_lines(&none).len(), 1);
    }

    #[test]
    fn a_listing_line_has_kind_size_and_name() {
        let entry = |kind, size| FsEntry {
            name: "x".into(),
            path: "x".into(),
            kind,
            size,
            mtime_ms: None,
        };
        assert_eq!(entry_line(&entry(FsEntryKind::Dir, None)), "d           -  x/");
        assert_eq!(entry_line(&entry(FsEntryKind::File, Some(12))), "f          12  x");
        assert_eq!(entry_line(&entry(FsEntryKind::Symlink, Some(3))), "l           3  x");
    }
}
