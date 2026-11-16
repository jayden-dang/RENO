//! Where the daemon keeps its files.
//!
//! ```text
//! <data-dir>/              0700
//!   engine.lock            held (flock) for the life of the process
//!   reno.db                the store
//!   journals/              per-run journals (made by the store)
//!   logs/                  JSON logs
//!   workspace/             the working directory of a task that names none
//!   run/                   one directory per live run, 0700, holding its 0600 token file
//!   worktrees/             git worktrees of write runs: <repo key>/<run id>/, branch reno/<run id>
//!   reno.sock              the socket (0600), unless `--socket` puts it elsewhere
//! ```
//!
//! The definitions directory (`~/.reno`) is separate from the data directory: it is the user's
//! to edit and to keep in git, so it is not under `Application Support` and not private to the
//! daemon. [`crate::defs::default_defs_dir`] says where it is by default.
//!
//! The default data directory and socket path are the ones `reno-client` computes for the app and
//! the CLI; a test in this crate keeps the two in step.

use std::ffi::OsString;
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// The environment variable that overrides the data directory.
pub const DATA_DIR_ENV: &str = "RENO_DATA_DIR";
/// The environment variable that overrides the socket path.
pub const SOCKET_ENV: &str = "RENO_SOCKET";

/// The socket's file name inside the data directory.
pub const SOCKET_FILE: &str = "reno.sock";
/// The lock file's name.
pub const LOCK_FILE: &str = "engine.lock";
/// The store's file name.
pub const DB_FILE: &str = "reno.db";
/// The log directory's name.
pub const LOGS_DIR: &str = "logs";
/// The default working directory's name.
pub const WORKSPACE_DIR: &str = "workspace";
/// The directory of the worktrees of write runs.
pub const WORKTREES_DIR: &str = "worktrees";
/// The directory of the run token files.
pub const RUNS_DIR: &str = "run";
/// Where [`Paths::new`] puts the definitions, inside the data directory.
pub const DEFS_DIR: &str = "definitions";

/// The platform's default data directory, or `None` when `HOME` is not set.
///
/// macOS: `~/Library/Application Support/Reno`. Elsewhere: `$XDG_DATA_HOME/reno`, or
/// `~/.local/share/reno`.
pub fn default_data_dir() -> Option<PathBuf> {
    data_dir_from(|key| std::env::var_os(key))
}

fn non_empty(value: Option<OsString>) -> Option<PathBuf> {
    value.filter(|v| !v.is_empty()).map(PathBuf::from)
}

fn data_dir_from(env: impl Fn(&str) -> Option<OsString>) -> Option<PathBuf> {
    let home = non_empty(env("HOME"));
    if cfg!(target_os = "macos") {
        return home.map(|home| home.join("Library/Application Support/Reno"));
    }
    match non_empty(env("XDG_DATA_HOME")) {
        Some(xdg) => Some(xdg.join("reno")),
        None => home.map(|home| home.join(".local/share/reno")),
    }
}

/// Every path the daemon uses, derived from the data directory and the socket choice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    /// The data directory.
    pub data_dir: PathBuf,
    /// The socket.
    pub socket: PathBuf,
    /// The definitions directory. [`Paths::new`] puts it at `<data_dir>/definitions`, so that
    /// paths made for a test can never reach the user's own `~/.reno`; the binary replaces it
    /// with the real one through [`Paths::with_defs_dir`].
    pub defs_dir: PathBuf,
}

impl Paths {
    /// Paths for `data_dir`, with the socket at `socket` or else inside the data directory.
    pub fn new(data_dir: PathBuf, socket: Option<PathBuf>) -> Self {
        let socket = socket.unwrap_or_else(|| data_dir.join(SOCKET_FILE));
        let defs_dir = data_dir.join(DEFS_DIR);
        Self {
            data_dir,
            socket,
            defs_dir,
        }
    }

    /// The same paths with the definitions directory at `defs_dir`.
    #[must_use]
    pub fn with_defs_dir(mut self, defs_dir: PathBuf) -> Self {
        self.defs_dir = defs_dir;
        self
    }

    /// The lock file.
    pub fn lock(&self) -> PathBuf {
        self.data_dir.join(LOCK_FILE)
    }

    /// The store.
    pub fn db(&self) -> PathBuf {
        self.data_dir.join(DB_FILE)
    }

    /// The log directory.
    pub fn logs(&self) -> PathBuf {
        self.data_dir.join(LOGS_DIR)
    }

    /// The worktrees of write runs: `<repo key>/<run id>/` below it. The repo key is
    /// `<name>-<12 hex of the SHA-256 of the repository's canonical git dir>`.
    pub fn worktrees(&self) -> PathBuf {
        self.data_dir.join(WORKTREES_DIR)
    }

    /// The run token files: `<run id>/token` below it (see [`crate::link`]).
    pub fn runs(&self) -> PathBuf {
        self.data_dir.join(RUNS_DIR)
    }

    /// The default working directory of a task.
    pub fn workspace(&self) -> PathBuf {
        self.data_dir.join(WORKSPACE_DIR)
    }
}

/// Creates `dir` (and its parents) with mode 0700 if it is missing; tightens it to 0700 if it
/// exists and is ours; refuses a directory owned by someone else.
pub fn ensure_private_dir(dir: &Path) -> io::Result<()> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    let meta = std::fs::metadata(dir)?;
    if !meta.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            format!("{} is not a directory", dir.display()),
        ));
    }
    if meta.uid() != rustix::process::getuid().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("{} is owned by another user", dir.display()),
        ));
    }
    if meta.mode() & 0o077 != 0 {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn env(pairs: &[(&'static str, &'static str)]) -> impl Fn(&str) -> Option<OsString> {
        let map: HashMap<_, _> = pairs.iter().copied().collect();
        move |key| map.get(key).map(OsString::from)
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_default_is_application_support() {
        let vars = env(&[("HOME", "/Users/u"), ("XDG_DATA_HOME", "/ignored")]);
        assert_eq!(
            data_dir_from(vars),
            Some(PathBuf::from("/Users/u/Library/Application Support/Reno"))
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn linux_default_is_xdg_then_the_home_fallback() {
        let xdg = env(&[("HOME", "/home/u"), ("XDG_DATA_HOME", "/data")]);
        assert_eq!(data_dir_from(xdg), Some(PathBuf::from("/data/reno")));
        let home = env(&[("HOME", "/home/u")]);
        assert_eq!(
            data_dir_from(home),
            Some(PathBuf::from("/home/u/.local/share/reno"))
        );
    }

    #[test]
    fn the_defaults_are_the_ones_the_client_computes() {
        // `reno-client` also honours `RENO_DATA_DIR`; compare only when it is not set.
        if std::env::var_os(DATA_DIR_ENV).is_none() && std::env::var_os(SOCKET_ENV).is_none() {
            assert_eq!(default_data_dir(), reno_client::default_data_dir());
            let socket = default_data_dir().map(|dir| Paths::new(dir, None).socket);
            assert_eq!(socket, reno_client::default_socket_path());
        }
        if std::env::var_os(crate::defs::DEFS_DIR_ENV).is_none() {
            assert_eq!(
                crate::defs::default_defs_dir(),
                reno_client::default_defs_dir()
            );
        }
    }

    #[test]
    fn no_home_means_no_default() {
        assert_eq!(data_dir_from(env(&[])), None);
        assert_eq!(data_dir_from(env(&[("HOME", "")])), None);
    }

    #[test]
    fn the_socket_defaults_to_the_data_dir() {
        let paths = Paths::new(PathBuf::from("/d"), None);
        assert_eq!(paths.socket, PathBuf::from("/d/reno.sock"));
        assert_eq!(paths.lock(), PathBuf::from("/d/engine.lock"));
        assert_eq!(paths.db(), PathBuf::from("/d/reno.db"));
        assert_eq!(paths.logs(), PathBuf::from("/d/logs"));
        assert_eq!(paths.workspace(), PathBuf::from("/d/workspace"));
        assert_eq!(paths.worktrees(), PathBuf::from("/d/worktrees"));
        let elsewhere = Paths::new(PathBuf::from("/d"), Some(PathBuf::from("/s/x.sock")));
        assert_eq!(elsewhere.socket, PathBuf::from("/s/x.sock"));
        // A made-up `Paths` never points at the real `~/.reno`.
        assert_eq!(paths.defs_dir, PathBuf::from("/d/definitions"));
        let moved = paths.with_defs_dir(PathBuf::from("/home/u/.reno"));
        assert_eq!(moved.defs_dir, PathBuf::from("/home/u/.reno"));
    }

    #[test]
    fn a_private_dir_is_created_0700_and_a_loose_one_is_tightened() {
        let root = tempfile::tempdir().expect("tempdir");
        let dir = root.path().join("a/b");
        ensure_private_dir(&dir).expect("create");
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").mode() & 0o777;
        assert_eq!(mode(&dir), 0o700);

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        ensure_private_dir(&dir).expect("tighten");
        assert_eq!(mode(&dir), 0o700);
    }

    #[test]
    fn a_file_in_the_way_is_an_error() {
        let root = tempfile::tempdir().expect("tempdir");
        let file = root.path().join("f");
        std::fs::write(&file, b"x").expect("write");
        assert!(ensure_private_dir(&file).is_err());
    }
}
