//! `reno mcp`: Reno's own MCP server, on stdio, for one run (T1.9).
//!
//! Reno starts it, not a person: every run is launched with it injected as an MCP server (ARCH-4:
//! this is the only place that speaks MCP). The environment says which run and how to reach the
//! daemon:
//!
//! | Variable | Holds |
//! |---|---|
//! | `RENO_SOCKET` | the daemon's socket |
//! | `RENO_RUN_ID` | the run this process serves |
//! | `RENO_TOKEN_FILE` | the path of the file with the run token, never the token itself |
//!
//! It connects to the socket, which it never starts a daemon for, authenticates with the token
//! (`agent.auth`), and from then on can do only what a run may: the four tools below, each one a
//! call of an `agent.*` method. It holds no state of its own, and writes nothing to stdout but
//! MCP frames. It ends when the harness closes its stdin.
//!
//! | Tool | Does |
//! |---|---|
//! | `report` | journals a structured status in the run's transcript |
//! | `request_approval` | asks a human to approve an action, and waits for the answer |
//! | `board_read` | reads a key of the bot's private board, or lists its keys |
//! | `board_write` | writes a key of it, optionally compare-and-set |
//!
//! The token is read once, sent once, and dropped: it is never logged, printed or kept.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context as _, bail};
use clap::Args;
use reno_client::{Client, ClientError};
use reno_proto::{
    AgentApprovalParams, AgentBoardReadParams, AgentBoardWriteParams, AgentReportParams,
    ENV_RUN_ID, ENV_SOCKET, ENV_TOKEN_FILE, ErrorCode, RunId,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{ServerHandler, ServiceExt, tool, tool_handler, tool_router, transport::stdio};
use serde::Serialize;

/// What the harness is told about this server, once, at `initialize`.
const INSTRUCTIONS: &str = "Reno's own tools for this run. `report` records where the work stands; \
`request_approval` asks the person for a decision on an exact action and returns it; \
`board_read` and `board_write` keep notes that outlive this run, for you and the later runs of \
the same bot.";

/// Arguments of `reno mcp`. Reno sets them through the environment; they exist as flags so that
/// the server can be started by hand for a run that is live.
#[derive(Debug, Args)]
pub(crate) struct McpArgs {
    /// The run to serve.
    #[arg(long, env = ENV_RUN_ID, value_name = "RUN")]
    run: Option<RunId>,
    /// The file the run's token is in.
    #[arg(long, env = ENV_TOKEN_FILE, value_name = "PATH")]
    token_file: Option<PathBuf>,
}

/// Runs the server until the harness closes stdin.
///
/// `socket` is the global `--socket` (`RENO_SOCKET`).
pub(crate) fn run(args: McpArgs, socket: Option<PathBuf>) -> anyhow::Result<ExitCode> {
    let Some(socket) = socket else {
        bail!("{ENV_SOCKET} is not set: `reno mcp` is started by Reno, for a run");
    };
    let Some(run) = args.run else {
        bail!("{ENV_RUN_ID} is not set: `reno mcp` is started by Reno, for a run");
    };
    let Some(token_file) = args.token_file else {
        bail!("{ENV_TOKEN_FILE} is not set: `reno mcp` is started by Reno, for a run");
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(serve(&socket, &run, &token_file))
}

async fn serve(socket: &Path, run: &RunId, token_file: &Path) -> anyhow::Result<ExitCode> {
    let token = read_token(token_file)?;
    let client = Client::connect_unix(socket)
        .await
        .with_context(|| format!("cannot reach the daemon at {}", socket.display()))?;
    client
        .agent_auth(run, token)
        .await
        .with_context(|| format!("the daemon refused the token for {run}"))?;
    let service = RenoMcp { client }
        .serve(stdio())
        .await
        .context("cannot start the MCP server on stdio")?;
    service.waiting().await.context("the MCP server failed")?;
    Ok(ExitCode::SUCCESS)
}

/// The token in `path`, without surrounding whitespace.
fn read_token(path: &Path) -> anyhow::Result<String> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read the token file {}", path.display()))?;
    let token = text.trim();
    if token.is_empty() {
        bail!("the token file {} is empty", path.display());
    }
    Ok(token.to_owned())
}

/// The server: one authenticated connection to the daemon, and the tools that use it.
#[derive(Debug, Clone)]
struct RenoMcp {
    client: Client,
}

/// A tool's answer: the value as JSON text.
fn answer<T: Serialize>(value: &T) -> CallToolResult {
    match serde_json::to_string_pretty(value) {
        Ok(text) => CallToolResult::success(vec![ContentBlock::text(text)]),
        Err(error) => failed(&format!("cannot encode the answer: {error}")),
    }
}

/// A tool that failed, as the tool's own error (the model sees it and can act on it).
fn failed(message: &str) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message.to_owned())])
}

/// What a daemon error says to the model. A stale board write carries the entry as it stands.
fn explain(error: &ClientError) -> CallToolResult {
    match error {
        ClientError::Remote(object) => {
            let mut message = object.message.clone();
            if object.code == ErrorCode::Conflict
                && let Some(data) = &object.data
            {
                message.push('\n');
                message.push_str(&data.to_string());
            }
            failed(&message)
        }
        ClientError::Disconnected | ClientError::NotRunning(_) => {
            failed("the Reno daemon is not reachable; this run can go on without Reno's tools")
        }
        other => failed(&other.to_string()),
    }
}

#[tool_router]
impl RenoMcp {
    #[tool(
        description = "Record where the work stands, for the person watching this run. Use \
`progress` for a checkpoint, `done` when the work is finished, `blocked` when you cannot go on \
without something, `failed` when it did not work. The summary is one short paragraph; details \
may be longer."
    )]
    async fn report(&self, Parameters(params): Parameters<AgentReportParams>) -> CallToolResult {
        match self.client.agent_report(params).await {
            Ok(()) => answer(&serde_json::json!({ "recorded": true })),
            Err(error) => explain(&error),
        }
    }

    #[tool(
        description = "Ask the person to approve an exact action before you take it, and wait \
for the answer. Give the action's class (command, edit, network, read, mcp_tool or other), why \
you want it, and the command line or paths when they apply. Returns the decision, who made it, \
and the reason when it was denied. A denied action must not be done; asking again for the same \
action is refused at once. A request nobody answers in time is denied."
    )]
    async fn request_approval(
        &self,
        Parameters(params): Parameters<AgentApprovalParams>,
    ) -> CallToolResult {
        match self.client.agent_request_approval(params).await {
            Ok(result) => answer(&result),
            Err(error) => explain(&error),
        }
    }

    #[tool(
        description = "Read notes from your bot's private board, which outlives this run and is \
shared with the bot's later runs. With a key, returns that entry (empty when it does not exist); \
without one, lists the keys with their versions and sizes."
    )]
    async fn board_read(
        &self,
        Parameters(params): Parameters<AgentBoardReadParams>,
    ) -> CallToolResult {
        match self.client.agent_board_read(params.key).await {
            Ok(result) => answer(&result),
            Err(error) => explain(&error),
        }
    }

    #[tool(
        description = "Write a note to your bot's private board: a text value under a key, at \
most 64 KiB. Each write bumps the key's version. Pass expectedVersion (0 for a key that must not \
exist yet) to write only when nobody changed it since you read it; a stale write changes \
nothing and returns the entry as it is now."
    )]
    async fn board_write(
        &self,
        Parameters(params): Parameters<AgentBoardWriteParams>,
    ) -> CallToolResult {
        match self.client.agent_board_write(params).await {
            Ok(result) => answer(&result),
            Err(error) => explain(&error),
        }
    }
}

#[tool_handler]
impl ServerHandler for RenoMcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("reno", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use reno_proto::ErrorObject;

    use super::*;

    fn text(result: &CallToolResult) -> String {
        result.content[0]
            .as_text()
            .expect("text content")
            .text
            .clone()
    }

    #[test]
    fn a_token_file_is_read_without_its_whitespace_and_an_empty_one_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("token");
        std::fs::write(&file, "rnt_abc\n").unwrap();
        assert_eq!(read_token(&file).unwrap(), "rnt_abc");
        std::fs::write(&file, " \n").unwrap();
        assert!(read_token(&file).unwrap_err().to_string().contains("empty"));
        assert!(read_token(&dir.path().join("missing")).is_err());
    }

    #[test]
    fn every_missing_variable_is_named_and_nothing_is_started() {
        let args = |run: Option<&str>, file: Option<&str>| McpArgs {
            run: run.map(|r| r.parse().unwrap()),
            token_file: file.map(PathBuf::from),
        };
        let socket = Some(PathBuf::from("/s"));
        let message = |args, socket| run(args, socket).unwrap_err().to_string();
        assert!(message(args(Some("run_a"), Some("/t")), None).contains(ENV_SOCKET));
        assert!(message(args(None, Some("/t")), socket.clone()).contains(ENV_RUN_ID));
        assert!(message(args(Some("run_a"), None), socket).contains(ENV_TOKEN_FILE));
    }

    #[test]
    fn a_daemon_refusal_reaches_the_model_as_a_tool_error_and_a_stale_write_brings_the_entry() {
        let refused = ClientError::Remote(Box::new(ErrorObject::new(
            ErrorCode::Unauthorized,
            "the run token is not valid",
        )));
        let result = explain(&refused);
        assert_eq!(result.is_error, Some(true));
        assert_eq!(text(&result), "the run token is not valid");

        let mut stale = ErrorObject::new(ErrorCode::Conflict, "the entry changed");
        stale.data = Some(serde_json::json!({"current": {"key": "k", "value": "v"}}));
        let result = explain(&ClientError::Remote(Box::new(stale)));
        assert!(text(&result).starts_with("the entry changed\n{"));
        assert!(text(&result).contains("\"value\":\"v\""));

        let gone = explain(&ClientError::Disconnected);
        assert_eq!(gone.is_error, Some(true));
        assert!(text(&gone).contains("not reachable"));
    }
}
