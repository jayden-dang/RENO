//! The `RunLink` port over the file system (T1.9): the token file of each run and the `reno mcp`
//! server that carries it.
//!
//! ```text
//! <data-dir>/run/              0700, emptied when the daemon starts
//!   <run id>/                  0700, one per live run
//!     token                    0600, the run token, no newline
//! ```
//!
//! The token is 32 bytes from the operating system's random source, as `rnt_` and 64 lowercase hex
//! digits. The file is created with mode 0600 in a directory only the daemon's user can enter, so
//! it is never readable by anyone else, not even for a moment. The token is handed to nothing
//! else: the injected server gets the file's *path* in `RENO_TOKEN_FILE`, because some harnesses
//! put MCP configuration on a command line, where `ps` shows it. Nothing here logs the token.
//!
//! The server is `reno mcp`, with `RENO_SOCKET`, `RENO_RUN_ID` and `RENO_TOKEN_FILE` in its
//! environment. Its program is the `reno` binary next to `renod`, or the one the daemon was told
//! to use; with neither, the daemon runs without Reno's MCP server (see [`resolve_reno`]).

use std::collections::BTreeMap;
use std::io::{self, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use reno_kernel::{IssuedLink, LinkError, RunLink};
use reno_proto::{
    ENV_RUN_ID, ENV_SOCKET, ENV_TOKEN_FILE, MCP_SERVER_NAME, McpServerSpec, RunId, token_digest,
};

use crate::paths::ensure_private_dir;

/// The file the token is in, inside the run's directory.
pub const TOKEN_FILE: &str = "token";

/// Where `reno mcp` is, when the daemon was not told: `reno` next to this executable. `None`
/// when there is no such file.
pub fn resolve_reno(given: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(given) = given {
        return Some(given);
    }
    let beside = std::env::current_exe().ok()?.with_file_name("reno");
    beside.is_file().then_some(beside)
}

/// A token: `rnt_` and 64 hex digits of 32 random bytes.
fn new_token() -> io::Result<String> {
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    let mut token = String::with_capacity(4 + 64);
    token.push_str("rnt_");
    for byte in bytes {
        token.push_str(&format!("{byte:02x}"));
    }
    Ok(token)
}

/// The file-backed `RunLink`.
#[derive(Debug)]
pub struct FsRunLink {
    /// `<data-dir>/run`.
    dir: PathBuf,
    socket: PathBuf,
    reno: PathBuf,
}

impl FsRunLink {
    /// A link that keeps its files under `dir`, points `reno mcp` at `socket`, and launches
    /// `reno`.
    pub fn new(dir: PathBuf, socket: PathBuf, reno: PathBuf) -> Self {
        Self { dir, socket, reno }
    }

    fn run_dir(&self, run: &RunId) -> PathBuf {
        self.dir.join(run.as_str())
    }
}

/// Writes `token` to `path` with mode 0600, replacing what is there.
fn write_token(path: &Path, token: &str) -> io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(token.as_bytes())?;
    file.sync_all()
}

fn issue_blocking(dir: &Path, run_dir: &Path) -> io::Result<(String, PathBuf)> {
    ensure_private_dir(dir)?;
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(run_dir)
        .or_else(|error| {
            if error.kind() == io::ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(error)
            }
        })?;
    let token = new_token()?;
    let file = run_dir.join(TOKEN_FILE);
    write_token(&file, &token)?;
    Ok((token, file))
}

#[async_trait]
impl RunLink for FsRunLink {
    async fn issue(&self, run: &RunId) -> Result<IssuedLink, LinkError> {
        let (dir, run_dir) = (self.dir.clone(), self.run_dir(run));
        let (token, file) = tokio::task::spawn_blocking(move || issue_blocking(&dir, &run_dir))
            .await
            .map_err(|error| LinkError(format!("the token task failed: {error}")))?
            .map_err(|error| LinkError(format!("cannot write the run's token file: {error}")))?;
        Ok(IssuedLink {
            token_hash: token_digest(&token),
            server: McpServerSpec {
                name: MCP_SERVER_NAME.to_owned(),
                command: self.reno.to_string_lossy().into_owned(),
                args: vec!["mcp".to_owned()],
                env: BTreeMap::from([
                    (
                        ENV_SOCKET.to_owned(),
                        self.socket.to_string_lossy().into_owned(),
                    ),
                    (ENV_RUN_ID.to_owned(), run.to_string()),
                    (
                        ENV_TOKEN_FILE.to_owned(),
                        file.to_string_lossy().into_owned(),
                    ),
                ]),
            },
        })
    }

    async fn release(&self, run: &RunId) {
        let run_dir = self.run_dir(run);
        let removed = tokio::task::spawn_blocking(move || std::fs::remove_dir_all(&run_dir)).await;
        match removed {
            Ok(Ok(())) => {}
            Ok(Err(error)) if error.kind() == io::ErrorKind::NotFound => {}
            Ok(Err(error)) => {
                tracing::warn!(%run, %error, "could not remove the run's token file");
            }
            Err(error) => tracing::warn!(%run, %error, "the token cleanup task failed"),
        }
    }

    async fn clear(&self) {
        let dir = self.dir.clone();
        let cleared = tokio::task::spawn_blocking(move || {
            match std::fs::remove_dir_all(&dir) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            ensure_private_dir(&dir)
        })
        .await;
        match cleared {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(%error, "could not clear the run token directory"),
            Err(error) => tracing::warn!(%error, "the token directory task failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn link(root: &Path) -> FsRunLink {
        FsRunLink::new(
            root.join("run"),
            root.join("reno.sock"),
            PathBuf::from("/opt/reno/reno"),
        )
    }

    fn run(n: &str) -> RunId {
        RunId::from_suffix(n).unwrap()
    }

    #[tokio::test]
    async fn an_issued_link_names_the_file_and_the_file_holds_a_token_that_matches_the_digest() {
        let root = tempfile::tempdir().unwrap();
        let link = link(root.path());
        let issued = link.issue(&run("a")).await.unwrap();

        let server = &issued.server;
        assert_eq!(
            (server.name.as_str(), server.args.as_slice()),
            ("reno", ["mcp".to_owned()].as_slice())
        );
        assert_eq!(server.command, "/opt/reno/reno");
        assert_eq!(server.env[ENV_RUN_ID], "run_a");
        assert_eq!(
            server.env[ENV_SOCKET],
            root.path().join("reno.sock").to_str().unwrap()
        );
        let file = PathBuf::from(&server.env[ENV_TOKEN_FILE]);
        let token = std::fs::read_to_string(&file).unwrap();
        assert!(token.starts_with("rnt_") && token.len() == 68, "{token:?}");
        assert_eq!(token_digest(&token), issued.token_hash);
        // The token is in the file only: not in the server's arguments or environment.
        assert!(!format!("{:?}", issued.server).contains(&token));
    }

    #[tokio::test]
    async fn the_file_is_private_in_a_private_directory() {
        let root = tempfile::tempdir().unwrap();
        let link = link(root.path());
        let issued = link.issue(&run("a")).await.unwrap();
        let file = PathBuf::from(&issued.server.env[ENV_TOKEN_FILE]);
        let mode = |path: &Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&file), 0o600);
        assert_eq!(mode(file.parent().unwrap()), 0o700);
        assert_eq!(mode(&root.path().join("run")), 0o700);
    }

    #[tokio::test]
    async fn every_issue_is_a_new_token_and_a_second_one_replaces_the_file() {
        let root = tempfile::tempdir().unwrap();
        let link = link(root.path());
        let first = link.issue(&run("a")).await.unwrap();
        let other = link.issue(&run("b")).await.unwrap();
        let again = link.issue(&run("a")).await.unwrap();
        assert_ne!(first.token_hash, again.token_hash);
        assert_ne!(first.token_hash, other.token_hash);
        let file = PathBuf::from(&again.server.env[ENV_TOKEN_FILE]);
        assert_eq!(
            token_digest(&std::fs::read_to_string(file).unwrap()),
            again.token_hash
        );
    }

    #[tokio::test]
    async fn release_removes_the_run_and_clear_removes_all() {
        let root = tempfile::tempdir().unwrap();
        let link = link(root.path());
        let a = link.issue(&run("a")).await.unwrap();
        link.issue(&run("b")).await.unwrap();
        let file = PathBuf::from(&a.server.env[ENV_TOKEN_FILE]);

        link.release(&run("a")).await;
        link.release(&run("a")).await;
        assert!(!file.exists());
        assert!(root.path().join("run/run_b/token").exists());

        link.clear().await;
        assert!(!root.path().join("run/run_b").exists());
        assert!(root.path().join("run").is_dir());
    }

    #[test]
    fn a_given_reno_wins_and_a_missing_one_is_none() {
        assert_eq!(
            resolve_reno(Some(PathBuf::from("/x/reno"))),
            Some(PathBuf::from("/x/reno"))
        );
    }
}
