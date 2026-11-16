//! The `RunLink` port (ARCH-7): how a run's own `reno mcp` process reaches the daemon.
//!
//! The kernel never makes a secret or writes a file. When a run starts it asks this port to
//! **issue** a link: the port makes a random token, writes it to a file only the daemon's user can
//! read, and answers with the token's digest (never the token) and the MCP server to inject, with
//! the environment that points `reno mcp` at the daemon, the run and that file. The kernel stores
//! the digest bound to the run, its bot and its board scope. When the run ends the kernel asks
//! the port to **release** it, which removes the file.
//!
//! A token is checked against the stored digest, never against the port, so a port that fails
//! cannot make a revoked token valid. [`FakeRunLink`] is the in-memory double the kernel's tests
//! use; the real one is in `renod`.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Mutex, PoisonError};

use async_trait::async_trait;
use reno_proto::{
    ENV_RUN_ID, ENV_SOCKET, ENV_TOKEN_FILE, MCP_SERVER_NAME, McpServerSpec, RunId, token_digest,
};

/// Why a link could not be issued.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct LinkError(pub String);

/// What [`RunLink::issue`] hands the kernel. The token itself is not in it.
#[derive(Clone, PartialEq, Eq)]
pub struct IssuedLink {
    /// The digest of the token ([`token_digest`]), which is what the kernel stores.
    pub token_hash: String,
    /// Reno's own MCP server, ready to be the first of `ExecSpec::mcp`.
    pub server: McpServerSpec,
}

impl fmt::Debug for IssuedLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // The server's environment holds the path of the token file, not the token; the digest
        // is a credential of a kind, so neither is printed.
        f.debug_struct("IssuedLink")
            .field("token_hash", &"<digest>")
            .field("server", &self.server.name)
            .finish()
    }
}

/// The `RunLink` port.
#[async_trait]
pub trait RunLink: Send + Sync + fmt::Debug {
    /// Makes the token and its file for `run`, replacing a link it already issued for the run (a
    /// resumed run mints again).
    async fn issue(&self, run: &RunId) -> Result<IssuedLink, LinkError>;

    /// Removes what [`issue`](Self::issue) wrote for `run`. Never fails: a file that is already
    /// gone is what was wanted.
    async fn release(&self, run: &RunId);

    /// Removes every issued link: the daemon calls it when it starts, because no run survives a
    /// restart.
    async fn clear(&self);
}

/// An in-memory [`RunLink`] for tests. Its tokens are `fake-token-<run>-<n>`, and it keeps the
/// ones it has issued so a test can present one.
#[derive(Default)]
pub struct FakeRunLink {
    state: Mutex<FakeState>,
}

#[derive(Default)]
struct FakeState {
    issued: BTreeMap<String, String>,
    count: u64,
    fail: bool,
    clears: u64,
}

impl fmt::Debug for FakeRunLink {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeRunLink").finish_non_exhaustive()
    }
}

impl FakeRunLink {
    /// A link with nothing issued.
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The token last issued for `run`, if it has not been released.
    pub fn token_for(&self, run: &RunId) -> Option<String> {
        self.state().issued.get(run.as_str()).cloned()
    }

    /// Makes the next `issue` calls fail.
    pub fn fail_issuing(&self, fail: bool) {
        self.state().fail = fail;
    }

    /// How many times `clear` was called.
    pub fn clears(&self) -> u64 {
        self.state().clears
    }
}

#[async_trait]
impl RunLink for FakeRunLink {
    async fn issue(&self, run: &RunId) -> Result<IssuedLink, LinkError> {
        let mut state = self.state();
        if state.fail {
            return Err(LinkError("the fake link is set to fail".into()));
        }
        state.count += 1;
        let token = format!("fake-token-{run}-{}", state.count);
        let token_hash = token_digest(&token);
        state.issued.insert(run.to_string(), token);
        Ok(IssuedLink {
            token_hash,
            server: McpServerSpec {
                name: MCP_SERVER_NAME.to_owned(),
                command: "/fake/reno".to_owned(),
                args: vec!["mcp".to_owned()],
                env: BTreeMap::from([
                    (ENV_SOCKET.to_owned(), "/fake/reno.sock".to_owned()),
                    (ENV_RUN_ID.to_owned(), run.to_string()),
                    (ENV_TOKEN_FILE.to_owned(), format!("/fake/{run}/token")),
                ]),
            },
        })
    }

    async fn release(&self, run: &RunId) {
        self.state().issued.remove(run.as_str());
    }

    async fn clear(&self) {
        let mut state = self.state();
        state.issued.clear();
        state.clears += 1;
    }
}
