//! `workspace.file_sources`: both sides of one changed file, whole.
//!
//! The diff decides everything: the same `workspace.diff` the client holds (same run, scope and
//! path) says which files changed, how, from where (a rename), against which commit, and what
//! its checksum is now. The old side is a blob of that commit; the new side is the worktree file
//! (read through the Files sandbox, so a link that leaves the tree is refused) or, for one
//! commit, that commit's blob. Both are cut at 1 MiB and have the run's secrets redacted.

use reno_proto::{
    ChangeKind, ChangedFile, DiffScope, FILE_SOURCE_MAX_BYTES, FileSourcesParams,
    FileSourcesResult, image_hash,
};

use super::error::HistoryError;
use super::git;
use crate::errors::kernel_error;
use crate::files::sandbox::{Root, lexical};
use crate::files::{FsError, Served};
use crate::handlers::Ctx;

/// How much of a file is looked at to tell text from binary, as git does.
const SNIFF_BYTES: usize = 8 * 1024;

/// One side of a file, as read.
#[derive(Debug, PartialEq, Eq)]
struct Side {
    text: Option<String>,
    /// SHA-256 of the bytes read.
    hash: String,
    binary: bool,
    truncated: bool,
}

/// The bytes of a side as the Files tab reads a file: a NUL in the first 8 KiB is binary, a byte
/// order mark is not text, and what is not UTF-8 is read lossily. `cut` says they were cut at
/// the limit (a cut inside a character drops its start).
fn decode(bytes: &[u8], cut: bool) -> Side {
    let hash = image_hash(bytes);
    if bytes[..bytes.len().min(SNIFF_BYTES)].contains(&0) {
        return Side {
            text: None,
            hash,
            binary: true,
            truncated: false,
        };
    }
    let shown = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    let text = match std::str::from_utf8(shown) {
        Ok(text) => text.to_owned(),
        Err(error) if cut && error.error_len().is_none() => {
            String::from_utf8_lossy(shown.get(..error.valid_up_to()).unwrap_or_default())
                .into_owned()
        }
        Err(_) => String::from_utf8_lossy(shown).into_owned(),
    };
    Side {
        text: Some(text),
        hash,
        binary: false,
        truncated: cut,
    }
}

/// Where the new side is read from.
enum New {
    /// The worktree file (what is there now, committed or not).
    Worktree,
    /// A commit's blob.
    Commit(String),
}

pub(super) async fn read(
    ctx: &Ctx,
    p: &FileSourcesParams,
    tree: &Served,
) -> Result<FileSourcesResult, HistoryError> {
    // The path's own shape first, so a refusal is a refusal and not "not changed".
    lexical(&p.path)?;
    if tree.in_place {
        return Err(HistoryError::Fs(FsError::Unavailable(
            "this run has no worktree of its own, so it has no diff".to_owned(),
        )));
    }
    let scope = p.scope.clone().unwrap_or_default();
    let diff = ctx
        .kernel
        .workspace_diff_scoped(&p.run_id, Some(&p.path), &scope)
        .await
        .map_err(|e| HistoryError::Remote(kernel_error(&e)))?;
    let entry: &ChangedFile = diff
        .files
        .iter()
        .find(|f| f.path == p.path)
        .ok_or_else(|| HistoryError::NotChanged(p.path.clone()))?;
    let root = Root::open(&tree.dir)?;
    let dir = root.path();

    // Which commit the old side is read from, and where the new side is.
    let (old_commit, new) = match &scope {
        DiffScope::Commit { sha } => {
            let full = git::commit_named(dir, sha)
                .await?
                .ok_or_else(|| HistoryError::NotChanged(p.path.clone()))?;
            (git::first_parent(dir, &full).await?, New::Commit(full))
        }
        _ => (
            git::is_full_sha(&diff.base_sha).then(|| diff.base_sha.clone()),
            New::Worktree,
        ),
    };
    let old_path = entry.from.clone().unwrap_or_else(|| entry.path.clone());

    let old = match (&old_commit, entry.kind) {
        (Some(commit), kind) if kind != ChangeKind::Added => {
            blob_side(dir, commit, &old_path).await?
        }
        _ => None,
    };
    let new = match (new, entry.kind) {
        (_, ChangeKind::Deleted) => None,
        (New::Commit(commit), _) => blob_side(dir, &commit, &entry.path).await?,
        (New::Worktree, _) => worktree_side(ctx, tree, &entry.path).await?,
    };

    let binary = entry.binary
        || old.as_ref().is_some_and(|s| s.binary)
        || new.as_ref().is_some_and(|s| s.binary);
    let truncated =
        old.as_ref().is_some_and(|s| s.truncated) || new.as_ref().is_some_and(|s| s.truncated);
    let (old_hash, new_hash) = (
        old.as_ref().filter(|_| !binary).map(|s| s.hash.clone()),
        new.as_ref().filter(|_| !binary).map(|s| s.hash.clone()),
    );
    let (mut old_text, mut new_text) = (
        old.as_ref().and_then(|s| s.text.clone()),
        new.as_ref().and_then(|s| s.text.clone()),
    );
    if binary {
        (old_text, new_text) = (None, None);
    } else {
        // The run's secrets are not shown in a file either; when they cannot be read the text is
        // withheld (an error), never shown raw.
        let redactor = ctx
            .kernel
            .run_redactor(&p.run_id)
            .await
            .map_err(|e| HistoryError::Remote(kernel_error(&e)))?;
        let cut = |side: &Option<Side>| side.as_ref().is_some_and(|s| s.truncated);
        for (text, was_cut) in [(&mut old_text, cut(&old)), (&mut new_text, cut(&new))] {
            if let Some(shown) = text {
                *shown = if was_cut {
                    redactor.redact_cut(shown)
                } else {
                    redactor.redact(shown)
                };
            }
        }
    }
    Ok(FileSourcesResult {
        path: entry.path.clone(),
        old_path: entry.from.clone(),
        old_text,
        new_text,
        old_hash,
        new_hash,
        stale: p.checksum.as_ref().is_some_and(|c| *c != diff.checksum),
        checksum: diff.checksum,
        binary,
        truncated,
    })
}

/// The blob of `path` in `commit`; `None` when it is not there.
async fn blob_side(
    dir: &std::path::Path,
    commit: &str,
    path: &str,
) -> Result<Option<Side>, HistoryError> {
    let cap = FILE_SOURCE_MAX_BYTES as u64;
    Ok(git::blob(dir, commit, path, cap)
        .await?
        .map(|(bytes, cut)| decode(&bytes, cut)))
}

/// The worktree file `path`, through the sandbox; `None` when nothing is there or it is no file.
async fn worktree_side(ctx: &Ctx, tree: &Served, path: &str) -> Result<Option<Side>, HistoryError> {
    match ctx.files.read(tree, path.to_owned()).await {
        Ok(read) => Ok(Some(Side {
            binary: read.binary,
            truncated: read.truncated,
            hash: read.content_hash.unwrap_or_default(),
            text: read.text,
        })),
        Err(FsError::NotFound(_) | FsError::NotAFile(_)) => Ok(None),
        Err(other) => Err(other.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_side_is_read_as_the_files_tab_reads_a_file() {
        let plain = decode(b"a\nb\n", false);
        assert_eq!(plain.text.as_deref(), Some("a\nb\n"));
        assert!(!plain.binary && !plain.truncated);
        assert_eq!(plain.hash, image_hash(b"a\nb\n"));
        // A NUL is binary: no text, the hash stays.
        let nul = decode(b"ab\0cd", false);
        assert!(nul.binary && nul.text.is_none());
        // A byte order mark is not text.
        assert_eq!(decode(b"\xEF\xBB\xBFhi", false).text.as_deref(), Some("hi"));
        // A cut inside a character drops its start; one that is not a cut is read lossily.
        let cut = decode(b"ab\xE2\x82", true);
        assert_eq!(cut.text.as_deref(), Some("ab"));
        assert!(cut.truncated);
        assert_eq!(decode(b"a\xFFb", false).text.as_deref(), Some("a\u{FFFD}b"));
    }
}
