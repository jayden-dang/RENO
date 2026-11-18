//! The History service (RP3): a person's read-only view of a run's commit history
//! (`git.log`, `git.log_search`) and of both sides of a changed file (`workspace.file_sources`).
//!
//! Like the Files tab this is a person-driven service, not run state, so it lives in the
//! composition root and not the kernel (ARCH-7). The handlers resolve which tree a run is served
//! from (`handlers::served`: its thread's worktree, or its working directory, flagged
//! `in_place`) and read it through short read-only `git` subprocesses on the Files runner's
//! hygiene. A run's token is refused here by the one guard in front of every handler (ARCH-13).
//!
//! # Modules
//!
//! - `git`: the subprocesses and their parsers.
//! - `sources`: `workspace.file_sources`.
//! - [`error`]: [`HistoryError`], and its stable wire codes (ARCH-14).

pub mod error;
mod git;
mod sources;

use std::sync::Arc;

use reno_proto::{
    ErrorObject, FileSourcesParams, FileSourcesResult, GIT_LOG_DEFAULT_LIMIT, GIT_LOG_MAX_LIMIT,
    GIT_LOG_QUERY_MAX_BYTES, GitCommit, GitLogParams, GitLogResult, GitLogRev, GitLogSearchParams,
    GitLogSearchResult,
};

pub use error::HistoryError;
use git::{Filter, LogQuery, Order};

use crate::errors::kernel_error;
use crate::files::Served;
use crate::files::sandbox::Root;
use crate::handlers::{Ctx, served};

type Reply<T> = Result<T, ErrorObject>;

/// The most commits a cursor may skip: far past any history a person scrolls.
const MAX_SKIP: usize = 5_000_000;

/// `git.log`.
pub(crate) async fn git_log(ctx: Arc<Ctx>, p: GitLogParams) -> Reply<GitLogResult> {
    let tree = served(&ctx, &p.run_id).await?;
    let mut result = log(&ctx, &p, &tree).await.map_err(|e| e.object())?;
    redact_subjects(&ctx, &p.run_id, &mut result.commits).await?;
    Ok(result)
}

/// `git.log_search`.
pub(crate) async fn git_log_search(
    ctx: Arc<Ctx>,
    p: GitLogSearchParams,
) -> Reply<GitLogSearchResult> {
    let tree = served(&ctx, &p.run_id).await?;
    let mut result = search(&ctx, &p, &tree).await.map_err(|e| e.object())?;
    redact_subjects(&ctx, &p.run_id, &mut result.commits).await?;
    Ok(result)
}

/// `workspace.file_sources`.
pub(crate) async fn file_sources(ctx: Arc<Ctx>, p: FileSourcesParams) -> Reply<FileSourcesResult> {
    let tree = served(&ctx, &p.run_id).await?;
    sources::read(&ctx, &p, &tree).await.map_err(|e| e.object())
}

/// A commit message is the agent's text: the values of the run's secrets are redacted from it
/// like from the journal. When they cannot be read now the log is withheld, never shown raw.
async fn redact_subjects(
    ctx: &Ctx,
    run: &reno_proto::RunId,
    commits: &mut [GitCommit],
) -> Reply<()> {
    if commits.is_empty() {
        return Ok(());
    }
    let redactor = ctx
        .kernel
        .run_redactor(run)
        .await
        .map_err(|e| kernel_error(&e))?;
    for commit in commits {
        commit.subject = redactor.redact_cut(&commit.subject);
    }
    Ok(())
}

fn page_limit(limit: Option<u32>) -> usize {
    limit
        .unwrap_or(GIT_LOG_DEFAULT_LIMIT)
        .clamp(1, GIT_LOG_MAX_LIMIT) as usize
}

/// The commit whose history a `rev` leaves out: the thread's base, or where the run started.
async fn excluded(
    ctx: &Ctx,
    run: &reno_proto::RunId,
    tree: &Served,
    rev: GitLogRev,
) -> Result<Option<String>, HistoryError> {
    let what = match rev {
        GitLogRev::Branch => return Ok(None),
        GitLogRev::Thread => "the thread scope",
        GitLogRev::Run => "the run scope",
    };
    if tree.in_place {
        return Err(HistoryError::NeedsWorktree(what));
    }
    let info = ctx
        .kernel
        .workspace(run)
        .await
        .map_err(|e| HistoryError::Fs(crate::files::FsError::Unavailable(e.to_string())))?;
    let mut base = info.base_sha;
    if rev == GitLogRev::Run
        && let Ok(thread) = ctx.kernel.run_thread(run).await
        && let Ok(thread) = ctx.kernel.thread(&thread).await
        && let Some(start) = thread
            .runs
            .iter()
            .find(|r| &r.run_id == run)
            .and_then(|r| r.start_sha.clone())
    {
        base = start;
    }
    Ok(git::is_full_sha(&base).then_some(base))
}

/// The cursor of the page that starts `skip` commits down from `tip`.
fn cursor_of(tip: &str, skip: usize) -> String {
    format!("h1.{tip}.{skip}")
}

fn parse_cursor(cursor: &str) -> Result<(String, usize), HistoryError> {
    let mut parts = cursor.split('.');
    let (Some("h1"), Some(tip), Some(skip), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(HistoryError::BadCursor);
    };
    let skip: usize = skip.parse().map_err(|_| HistoryError::BadCursor)?;
    if !git::is_full_sha(tip) || skip > MAX_SKIP {
        return Err(HistoryError::BadCursor);
    }
    Ok((tip.to_owned(), skip))
}

async fn log(ctx: &Ctx, p: &GitLogParams, tree: &Served) -> Result<GitLogResult, HistoryError> {
    let limit = page_limit(p.limit);
    let rev = p.rev.unwrap_or_default();
    let root = Root::open(&tree.dir)?;
    let dir = root.path();
    let empty = |repo: bool, head, shallow| GitLogResult {
        commits: Vec::new(),
        next_cursor: None,
        head,
        repo,
        shallow,
        in_place: tree.in_place,
    };
    if !git::is_repo(dir).await? {
        // The scopes that need a worktree still say so; a plain folder has no history.
        excluded(ctx, &p.run_id, tree, rev).await?;
        return Ok(empty(false, None, false));
    }
    let base = excluded(ctx, &p.run_id, tree, rev).await?;
    let head = git::head(dir).await?;
    let shallow = git::is_shallow(dir).await;
    let (tip, skip) = match &p.cursor {
        Some(cursor) => {
            let (tip, skip) = parse_cursor(cursor)?;
            // A commit that is gone (rebased away and collected) ends the paging.
            if git::commit_named(dir, &tip).await?.as_deref() != Some(tip.as_str()) {
                return Err(HistoryError::BadCursor);
            }
            (tip, skip)
        }
        None => match &head {
            Some(head) => (head.clone(), 0),
            // A repository with no commits yet.
            None => return Ok(empty(true, None, shallow)),
        },
    };
    let page = git::log(
        dir,
        &LogQuery {
            tip: &tip,
            base: base.as_deref(),
            skip,
            count: limit + 1,
            order: Order::Topo,
            filter: None,
        },
    )
    .await?;
    let mut commits = page.commits;
    let next_cursor = (commits.len() > limit).then(|| cursor_of(&tip, skip + limit));
    commits.truncate(limit);
    if !commits.is_empty() {
        git::decorate(&mut commits, git::refs(dir).await?, head.as_deref());
    }
    Ok(GitLogResult {
        commits,
        next_cursor,
        head,
        repo: true,
        shallow,
        in_place: tree.in_place,
    })
}

/// A search text that is safe to hand to git as a literal: not empty, not long, one line.
fn check_query(query: &str) -> Result<&str, HistoryError> {
    let query = query.trim();
    if query.is_empty() {
        return Err(HistoryError::BadQuery("the query is empty".to_owned()));
    }
    if query.len() > GIT_LOG_QUERY_MAX_BYTES {
        return Err(HistoryError::BadQuery(format!(
            "the query is over {GIT_LOG_QUERY_MAX_BYTES} bytes"
        )));
    }
    if query.chars().any(|c| c.is_control() && c != '\t') {
        return Err(HistoryError::BadQuery(
            "the query has a control character".to_owned(),
        ));
    }
    Ok(query)
}

async fn search(
    ctx: &Ctx,
    p: &GitLogSearchParams,
    tree: &Served,
) -> Result<GitLogSearchResult, HistoryError> {
    let query = check_query(&p.query)?;
    let limit = page_limit(p.limit);
    let rev = p.rev.unwrap_or_default();
    let root = Root::open(&tree.dir)?;
    let dir = root.path();
    let empty = |repo: bool| GitLogSearchResult {
        commits: Vec::new(),
        truncated: false,
        repo,
        in_place: tree.in_place,
    };
    let base = excluded(ctx, &p.run_id, tree, rev).await?;
    if !git::is_repo(dir).await? {
        return Ok(empty(false));
    }
    let Some(head) = git::head(dir).await? else {
        return Ok(empty(true));
    };
    // git ANDs `--grep` with `--author`, so each is asked alone and the answers are merged.
    let asked = |filter: Filter| {
        let (head, base) = (head.clone(), base.clone());
        async move {
            git::log(
                dir,
                &LogQuery {
                    tip: &head,
                    base: base.as_deref(),
                    skip: 0,
                    count: limit + 1,
                    order: Order::AuthorDate,
                    filter: Some(&filter),
                },
            )
            .await
        }
    };
    let (by_message, by_author) = tokio::join!(
        asked(Filter::Message(query.to_owned())),
        asked(Filter::Author(query.to_owned()))
    );
    let mut found = by_message?.commits;
    found.extend(by_author?.commits);
    if let Some(commit) = by_name(dir, query, &head, base.as_deref()).await? {
        found.push(commit);
    }
    found.sort_by(|a, b| b.time_ms.cmp(&a.time_ms).then_with(|| a.sha.cmp(&b.sha)));
    found.dedup_by(|a, b| a.sha == b.sha);
    let truncated = found.len() > limit;
    found.truncate(limit);
    if !found.is_empty() {
        git::decorate(&mut found, git::refs(dir).await?, Some(&head));
    }
    Ok(GitLogSearchResult {
        commits: found,
        truncated,
        repo: true,
        in_place: tree.in_place,
    })
}

/// The commit whose name starts with `query`, when it is a hexadecimal prefix and the commit is
/// in the searched history.
async fn by_name(
    dir: &std::path::Path,
    query: &str,
    tip: &str,
    base: Option<&str>,
) -> Result<Option<GitCommit>, HistoryError> {
    if !(4..=40).contains(&query.len()) || !query.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let Some(full) = git::commit_named(dir, query).await? else {
        return Ok(None);
    };
    if !git::is_ancestor(dir, &full, tip).await? {
        return Ok(None);
    }
    // In a thread's or a run's range: not in what the range leaves out.
    if let Some(base) = base
        && git::is_ancestor(dir, &full, base).await?
    {
        return Ok(None);
    }
    let page = git::log(
        dir,
        &LogQuery {
            tip: &full,
            base: None,
            skip: 0,
            count: 1,
            order: Order::AuthorDate,
            filter: None,
        },
    )
    .await?;
    Ok(page.commits.into_iter().next())
}
