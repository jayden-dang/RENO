//! The agent-side protocol: what a run's own `reno mcp` process says to the daemon (T1.9).
//!
//! A connection is either the owner (the app, the CLI) or a run (ARCHITECTURE 8). A run
//! connection starts with `agent.auth`, which swaps a run token for a session bound to that run;
//! from then on it may call the `agent.*` methods below and nothing else. The owner's methods
//! (`approval.decide`, `budget.pause`, ...) are refused on it, and `agent.*` methods are refused
//! on a connection that has not authenticated.
//!
//! The scope an `agent.board_*` call touches is never a parameter: the daemon derives it from
//! the token (the run's bot, or the run itself when it has none).
//!
//! The token itself is a secret. It is written to a file only the run's user can read, its path is
//! handed to `reno mcp` in [`ENV_TOKEN_FILE`], and only its digest ([`token_digest`]) is stored.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::exec::{ActionClass, ApprovalOutcome, ResolvedBy};
use crate::ids::RunId;
use crate::protocol::Method;

/// The name Reno's own MCP server is injected under, and so the prefix of its tools in a
/// harness's own naming (`mcp__reno__report`).
pub const MCP_SERVER_NAME: &str = "reno";

/// Environment variable (of the `reno mcp` process) holding the daemon's socket path.
pub const ENV_SOCKET: &str = "RENO_SOCKET";
/// Environment variable holding the id of the run `reno mcp` serves.
pub const ENV_RUN_ID: &str = "RENO_RUN_ID";
/// Environment variable holding the path of the file that holds the run token. A path, never the
/// token: some harnesses put MCP configuration on a command line, where `ps` reads it.
pub const ENV_TOKEN_FILE: &str = "RENO_TOKEN_FILE";

/// The tools of Reno's own MCP server (v1), by the name an agent calls them.
pub const MCP_TOOLS: &[&str] = &["report", "request_approval", "board_read", "board_write"];

/// Feature: the `agent.*` methods exist, and journals may carry `ExecEvent::AgentReport`.
pub const FEATURE_AGENT: &str = "agent";

/// The longest board key, in bytes.
pub const BOARD_KEY_MAX_BYTES: usize = 200;
/// The largest board value, in bytes.
pub const BOARD_VALUE_MAX_BYTES: usize = 64 * 1024;
/// The most keys one scope holds.
pub const BOARD_MAX_KEYS: usize = 256;
/// The longest report summary, in characters.
pub const REPORT_SUMMARY_MAX_CHARS: usize = 4_000;
/// The longest report details text, in characters.
pub const REPORT_DETAILS_MAX_CHARS: usize = 32_000;
/// The longest approval reason, in characters.
pub const AGENT_REASON_MAX_CHARS: usize = 2_000;

/// The digest a token is stored and looked up by: the SHA-256 of the token, as 64 lowercase hex
/// digits.
pub fn token_digest(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// How a run says its work stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReportStatus {
    /// Work is going on; this is a checkpoint.
    Progress,
    /// The work is finished.
    Done,
    /// The run cannot go on without something it does not have.
    Blocked,
    /// The work failed.
    Failed,
}

/// Params of `agent.auth`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentAuthParams {
    /// The run the token was minted for.
    pub run_id: RunId,
    /// The run token (the contents of the file named by [`ENV_TOKEN_FILE`]). Never logged.
    pub token: String,
}

impl std::fmt::Debug for AgentAuthParams {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AgentAuthParams")
            .field("run_id", &self.run_id)
            .field("token", &"<redacted>")
            .finish()
    }
}

/// Result of `agent.auth`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentAuthResult {
    /// The run the connection now speaks for.
    pub run_id: RunId,
    /// The bot the run is pinned to, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot: Option<String>,
    /// The board scope this run reads and writes: `bot:<name>`, or `run:<id>` for a run with no
    /// bot.
    pub scope: String,
}

/// Params of `agent.report`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentReportParams {
    /// How the work stands.
    pub status: ReportStatus,
    /// One short paragraph.
    pub summary: String,
    /// Anything longer, for the transcript.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<String>,
}

/// Result of `agent.report`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentReportResult {}

/// Params of `agent.request_approval`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentApprovalParams {
    /// The class of the action to be approved.
    pub action: ActionClass,
    /// Why the run wants to do it.
    pub reason: String,
    /// The command line, for a command action.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The paths the action touches.
    #[serde(default)]
    pub paths: Vec<String>,
}

/// Result of `agent.request_approval`: how the request ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentApprovalResult {
    /// Whether the action may go ahead.
    pub decision: ApprovalOutcome,
    /// Who or what decided.
    pub by: ResolvedBy,
    /// For a denial, the reason, when one was given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Params of `agent.board_read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentBoardReadParams {
    /// The key to read. Without one, every key of the scope is listed (without values).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
}

/// One entry of a board scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BoardEntry {
    /// The key.
    pub key: String,
    /// The value. Absent in a listing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    /// How many times the key has been written; the first write is version 1.
    pub version: u64,
    /// Size of the value in bytes.
    pub size: u64,
    /// When it was last written, in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
}

/// Result of `agent.board_read`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentBoardReadResult {
    /// The scope that was read.
    pub scope: String,
    /// The entry asked for (empty when the key does not exist), or every entry of the scope.
    pub entries: Vec<BoardEntry>,
}

/// Params of `agent.board_write`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentBoardWriteParams {
    /// The key.
    pub key: String,
    /// The value, at most [`BOARD_VALUE_MAX_BYTES`] bytes.
    pub value: String,
    /// The version the writer last saw (0 for a key that must not exist yet). When it is not the
    /// current version the write is refused with the current entry. Without it the write always
    /// succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_version: Option<u64>,
}

/// Result of `agent.board_write`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AgentBoardWriteResult {
    /// The scope written.
    pub scope: String,
    /// The key's version after the write.
    pub version: u64,
}

/// The `data` of the `Conflict` error that refuses a stale `agent.board_write`: the entry as it
/// stands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BoardConflict {
    /// The key's current entry, with its value. Absent when the key does not exist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<BoardEntry>,
}

macro_rules! agent_method {
    ($(#[$meta:meta])* $ty:ident, $name:literal, $params:ty => $result:ty) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy)]
        pub struct $ty;

        impl Method for $ty {
            const NAME: &'static str = $name;
            type Params = $params;
            type Result = $result;
        }
    };
}

agent_method!(
    /// `agent.auth`: binds the connection to a run, with the run's token. Every other `agent.*`
    /// method needs it, and after it the connection may call only `agent.*` methods. Needs
    /// feature `agent`.
    AgentAuth, "agent.auth", AgentAuthParams => AgentAuthResult
);
agent_method!(
    /// `agent.report`: a structured status, journaled as `ExecEvent::AgentReport`. Needs feature
    /// `agent`.
    AgentReport, "agent.report", AgentReportParams => AgentReportResult
);
agent_method!(
    /// `agent.request_approval`: asks for an approval on the run's behalf and answers when it is
    /// decided, expires or the run ends. Needs feature `agent`.
    AgentRequestApproval, "agent.request_approval", AgentApprovalParams => AgentApprovalResult
);
agent_method!(
    /// `agent.board_read`: reads the run's own scope. Needs feature `agent`.
    AgentBoardRead, "agent.board_read", AgentBoardReadParams => AgentBoardReadResult
);
agent_method!(
    /// `agent.board_write`: writes the run's own scope, optionally compare-and-set. Needs
    /// feature `agent`.
    AgentBoardWrite, "agent.board_write", AgentBoardWriteParams => AgentBoardWriteResult
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_token_digest_is_sixty_four_lowercase_hex_digits() {
        let digest = token_digest("rnt_abc");
        assert_eq!(digest.len(), 64);
        assert!(digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_ne!(digest, token_digest("rnt_abd"));
    }

    #[test]
    fn the_token_never_shows_in_debug_output() {
        let params = AgentAuthParams {
            run_id: RunId::from_suffix("r1").expect("id"),
            token: "rnt_secret".into(),
        };
        assert!(!format!("{params:?}").contains("rnt_secret"));
    }
}
