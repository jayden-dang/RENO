//! What the app keeps on this machine besides the daemon: which repository a folder is in, and
//! when you last opened each run (the sidebar's "has a message" dot, ui-brief 2026-11-17).  And what the composer's picker remembers (stars, effort and options per model).

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::vm::composer_prefs::ComposerPrefs;
use crate::vm::projects::Seen;

/// Finds repository roots on disk, once per directory.
#[derive(Default)]
pub(crate) struct Roots {
    cache: RefCell<HashMap<PathBuf, Option<PathBuf>>>,
    /// The fake daemon's folders do not exist: each is its own repository.
    literal: bool,
}

impl Roots {
    /// Roots for the fake daemon: a folder is its own root, and the disk is not read.
    pub(crate) fn literal() -> Self {
        Self {
            cache: RefCell::default(),
            literal: true,
        }
    }

    /// The canonical root of the git repository `dir` is in: the nearest ancestor (or itself)
    /// holding `.git` (a directory, or a file in a linked worktree). `None` outside any, and for
    /// a directory that does not exist.
    pub(crate) fn root_of(&self, dir: &Path) -> Option<PathBuf> {
        if self.literal {
            return Some(dir.to_path_buf());
        }
        if let Some(hit) = self.cache.borrow().get(dir) {
            return hit.clone();
        }
        let found = std::fs::canonicalize(dir).ok().and_then(|dir| {
            dir.ancestors()
                .find(|d| d.join(".git").exists())
                .map(Path::to_path_buf)
        });
        self.cache
            .borrow_mut()
            .insert(dir.to_path_buf(), found.clone());
        found
    }
}

/// Where [`Seen`] is kept: beside the daemon's socket, in its own file.
pub(crate) fn seen_path(socket: &Path) -> Option<PathBuf> {
    socket.parent().map(|dir| dir.join("app-seen.json"))
}

/// Reads [`Seen`] from `path`. A missing or unreadable file starts afresh with `now_ms` as the
/// baseline, so runs that ended before are not all "new"; the fresh copy is written back.
pub(crate) fn load_seen(path: &Path, now_ms: u64) -> Seen {
    if let Some(seen) = std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Seen>(&bytes).ok())
    {
        return seen;
    }
    let seen = Seen::new(now_ms);
    save_seen(path, &seen);
    seen
}

/// Writes [`Seen`] to `path` through a temporary file, so a crash never leaves half a file.
/// A failure only costs the dots their memory; it is logged, not shown.
#[allow(clippy::print_stderr)]
pub(crate) fn save_seen(path: &Path, seen: &Seen) {
    let write = || -> std::io::Result<()> {
        let bytes = serde_json::to_vec(seen)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)
    };
    if let Err(error) = write() {
        eprintln!("reno-app: could not save {}: {error}", path.display());
    }
}

/// Where the composer's remembered picks are kept: beside the daemon's socket, in their own
/// file.
pub(crate) fn composer_path(socket: &Path) -> Option<PathBuf> {
    socket.parent().map(|dir| dir.join("app-composer.json"))
}

/// Reads the composer's picks from `path`; a missing or unreadable file is a fresh start.
pub(crate) fn load_composer(path: &Path) -> ComposerPrefs {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Writes the composer's picks to `path` through a temporary file, so a crash never leaves half
/// a file. A failure only costs the memory; it is logged, not shown.
#[allow(clippy::print_stderr)]
#[allow(dead_code)] // the picker saves in the next commit
pub(crate) fn save_composer(path: &Path, prefs: &ComposerPrefs) {
    let write = || -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(prefs)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)
    };
    if let Err(error) = write() {
        eprintln!("reno-app: could not save {}: {error}", path.display());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use reno_proto::RunId;

    use super::*;

    #[test]
    fn roots_find_the_repository_and_agree_across_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("porch");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::create_dir_all(repo.join("crates/a")).unwrap();
        std::fs::create_dir_all(tmp.path().join("loose")).unwrap();
        let roots = Roots::default();
        let canonical = std::fs::canonicalize(&repo).unwrap();
        assert_eq!(
            roots.root_of(&repo.join("crates/a")),
            Some(canonical.clone())
        );
        assert_eq!(roots.root_of(&repo), Some(canonical));
        assert_eq!(roots.root_of(&tmp.path().join("loose")), None);
        assert_eq!(roots.root_of(Path::new("/definitely/not/here")), None);
    }

    #[test]
    fn seen_survives_a_restart_and_a_first_start_sets_the_baseline() {
        let tmp = tempfile::tempdir().unwrap();
        let path = seen_path(&tmp.path().join("reno.sock")).unwrap();
        let first = load_seen(&path, 1_000);
        assert_eq!(first, Seen::new(1_000));
        assert!(path.exists(), "a first start writes its baseline");

        let mut seen = first;
        seen.mark(&RunId::from_suffix("a").unwrap(), 2_000);
        save_seen(&path, &seen);
        assert_eq!(load_seen(&path, 9_999), seen);

        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(load_seen(&path, 5_000), Seen::new(5_000));
    }

    #[test]
    fn composer_picks_live_beside_the_socket_and_survive_a_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let path = composer_path(&tmp.path().join("reno.sock")).unwrap();
        assert_eq!(path, tmp.path().join("app-composer.json"));
        assert_eq!(
            load_composer(&path),
            ComposerPrefs::default(),
            "no file yet"
        );
        let mut prefs = ComposerPrefs::default();
        prefs.toggle_favourite(&"codex".parse().unwrap(), "gpt");
        save_composer(&path, &prefs);
        assert_eq!(load_composer(&path), prefs);
        assert!(
            !path.with_extension("json.tmp").exists(),
            "renamed into place"
        );
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(load_composer(&path), ComposerPrefs::default());
    }
}
