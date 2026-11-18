//! `reno git log|show`: a run's commit history and one of its commits, read-only (RP3).
//!
//! Each is a few client calls and some printing (ARCH-2): the same methods the app's History tab
//! and Review use. The daemon does the reading; nothing here runs `git`.

use std::process::ExitCode;

use anyhow::bail;
use clap::{Args, ValueEnum};
use reno_proto::{
    DiffScope, GIT_LOG_MAX_LIMIT, GitCommit, GitLogParams, GitLogRev, GitLogSearchParams,
    GitRefKind, RunId,
};

use crate::env::{Env, print_json, print_line, print_note};
use crate::render;

/// Which commits to list.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum LogRev {
    /// Everything reachable from the worktree's `HEAD`, the history the branch was cut from too.
    Branch,
    /// Only the thread's own commits.
    Thread,
    /// Only the commits made since the run started.
    Run,
}

impl From<LogRev> for GitLogRev {
    fn from(rev: LogRev) -> Self {
        match rev {
            LogRev::Branch => Self::Branch,
            LogRev::Thread => Self::Thread,
            LogRev::Run => Self::Run,
        }
    }
}

#[derive(Debug, Args)]
pub(crate) struct GitLogArgs {
    /// The run.
    pub(crate) run: RunId,
    /// At most this many commits [default: 50].
    #[arg(long, value_name = "N")]
    pub(crate) limit: Option<u32>,
    /// Only commits whose message, author or name (a hexadecimal start of it) match this text,
    /// as typed, in any case. Exit status 1 when none do.
    #[arg(long, value_name = "TEXT")]
    pub(crate) search: Option<String>,
    /// Which commits to list.
    #[arg(long, value_enum, default_value = "branch")]
    pub(crate) rev: LogRev,
}

#[derive(Debug, Args)]
pub(crate) struct GitShowArgs {
    /// The run.
    pub(crate) run: RunId,
    /// A commit of the thread's branch: 7 to 64 hexadecimal characters.
    pub(crate) sha: String,
    /// Print only the changed files, not their patches.
    #[arg(long)]
    pub(crate) stat: bool,
}

/// `reno git log`.
pub(crate) async fn log(env: &Env, args: GitLogArgs) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let want = args.limit.unwrap_or(50).max(1);
    let rev = Some(GitLogRev::from(args.rev));
    let (commits, more) = if let Some(query) = args.search {
        let found = client
            .git_log_search(GitLogSearchParams {
                run_id: args.run,
                query,
                limit: Some(want.min(GIT_LOG_MAX_LIMIT)),
                rev,
            })
            .await
            .map_err(|e| env.explain(e))?;
        (found.commits, found.truncated)
    } else {
        // Pages until enough, a page being at most the daemon's limit.
        let mut commits: Vec<GitCommit> = Vec::new();
        let mut cursor = None;
        let mut more = false;
        loop {
            let room = want as usize - commits.len();
            let page = client
                .git_log(GitLogParams {
                    run_id: args.run.clone(),
                    cursor: cursor.take(),
                    limit: Some(
                        u32::try_from(room)
                            .unwrap_or(u32::MAX)
                            .min(GIT_LOG_MAX_LIMIT),
                    ),
                    rev,
                })
                .await
                .map_err(|e| env.explain(e))?;
            commits.extend(page.commits);
            match page.next_cursor {
                Some(next) if commits.len() < want as usize => cursor = Some(next),
                Some(_) => {
                    more = true;
                    break;
                }
                None => break,
            }
        }
        (commits, more)
    };
    if env.json {
        print_json(&commits)?;
    } else {
        for commit in &commits {
            print_line(commit_line(commit))?;
        }
        if more {
            print_note("(more commits: raise --limit)")?;
        }
    }
    Ok(if commits.is_empty() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    })
}

/// `reno git show`.
pub(crate) async fn show(env: &Env, args: GitShowArgs) -> anyhow::Result<ExitCode> {
    let sha = args.sha.trim();
    if !(7..=64).contains(&sha.len()) || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("a commit is named by 7 to 64 hexadecimal characters");
    }
    let scope = DiffScope::Commit {
        sha: sha.to_owned(),
    };
    let client = env.connect().await?;
    let diff = client
        .workspace_diff_scoped(args.run.clone(), None, scope.clone())
        .await
        .map_err(|e| env.explain(e))?;
    // The message and author come from the log's own search, by the start of the name.
    let found = client
        .git_log_search(GitLogSearchParams {
            run_id: args.run.clone(),
            query: sha.to_owned(),
            limit: Some(1),
            rev: None,
        })
        .await
        .map_err(|e| env.explain(e))?;
    let mut patches = Vec::new();
    if !args.stat {
        for file in diff.files.iter().filter(|f| !f.binary) {
            let one = client
                .workspace_diff_scoped(args.run.clone(), Some(file.path.clone()), scope.clone())
                .await
                .map_err(|e| env.explain(e))?;
            if let Some(patch) = one.patch {
                patches.push((patch, one.patch_truncated));
            }
        }
    }
    if env.json {
        print_json(&serde_json::json!({
            "commit": found.commits.first(),
            "diff": diff,
            "patches": patches.iter().map(|(p, _)| p).collect::<Vec<_>>(),
        }))?;
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(commit) = found.commits.first() {
        print_line(format!("commit {}", commit.sha))?;
        if commit.parents.len() > 1 {
            print_line(format!(
                "merge  {}",
                commit
                    .parents
                    .iter()
                    .map(|p| short(p))
                    .collect::<Vec<_>>()
                    .join(" ")
            ))?;
        }
        print_line(format!("author {}", commit.author_name))?;
        print_line(format!("date   {}", date(commit.time_ms)))?;
        print_line(String::new())?;
        print_line(format!("    {}", commit.subject))?;
        print_line(String::new())?;
    }
    for line in render::diff_lines(&diff).into_iter().skip(1) {
        print_line(line)?;
    }
    for (patch, cut) in &patches {
        print_line(String::new())?;
        for line in patch.lines() {
            print_line(line.to_owned())?;
        }
        if *cut {
            print_note("(patch cut at 256 KiB)")?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn short(sha: &str) -> String {
    sha.chars().take(12).collect()
}

/// One commit on one line: the short name, the date, the author, the refs that point at it, and
/// the subject; a merge says so.
fn commit_line(commit: &GitCommit) -> String {
    let mut line = format!(
        "{}  {}  {:<12}  ",
        short(&commit.sha),
        date(commit.time_ms),
        commit.author_name
    );
    if commit.parents.len() > 1 {
        line.push_str("(merge) ");
    }
    if !commit.refs.is_empty() {
        let refs: Vec<String> = commit
            .refs
            .iter()
            .map(|r| match r.kind {
                GitRefKind::Tag => format!("tag: {}", r.name),
                _ => r.name.clone(),
            })
            .collect();
        line.push_str(&format!("({}) ", refs.join(", ")));
    }
    line.push_str(&commit.subject);
    line
}

/// `YYYY-MM-DD` (UTC) of a time in milliseconds since the Unix epoch.
fn date(ms: u64) -> String {
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let days = i64::try_from(ms / 86_400_000).unwrap_or(0);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use reno_proto::GitRef;

    #[test]
    fn a_time_is_a_utc_date() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(951_782_400_000), "2000-02-29");
        assert_eq!(date(1_700_000_000_000), "2023-11-14");
        assert_eq!(date(1_700_000_000_000 + 86_400_000 * 60), "2024-01-13");
    }

    #[test]
    fn a_commit_line_names_its_refs_and_a_merge() {
        let commit = GitCommit {
            sha: "a".repeat(40),
            parents: vec!["b".repeat(40), "c".repeat(40)],
            subject: "merge it".into(),
            author_name: "Ada".into(),
            author_key: String::new(),
            time_ms: 0,
            refs: vec![
                GitRef {
                    name: "HEAD".into(),
                    kind: GitRefKind::Head,
                },
                GitRef {
                    name: "v1".into(),
                    kind: GitRefKind::Tag,
                },
            ],
        };
        assert_eq!(
            commit_line(&commit),
            "aaaaaaaaaaaa  1970-01-01  Ada           (merge) (HEAD, tag: v1) merge it"
        );
    }
}
