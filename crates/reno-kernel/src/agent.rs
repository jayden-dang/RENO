//! Reno's own MCP server, from the kernel's side (T1.9): run tokens and what a run may do with
//! them.
//!
//! # Tokens
//!
//! When a run starts, [`provision`] asks the `RunLink` port for a link, stores the token's digest
//! bound to the run, its bot and its board scope, and puts Reno's MCP server first in
//! `ExecSpec::mcp`. The token is bounded by the run: it is revoked when the run ends
//! ([`revoke`]), when a run fails to start, and, for every run at once, when the daemon starts
//! ([`revoke_orphans`]); a resumed attempt is given a fresh one. Its wall-clock expiry is only a
//! safety net, the greater of four times the run's wall-time budget and 24 hours: the budget
//! counts awake time and a Mac's clock keeps going while it sleeps, so a token that expired by
//! the clock at the budget would die under a run that is still live.
//!
//! A token is **valid** when its digest is stored under the run it names, it is not revoked, its
//! safety-net expiry has not passed on the kernel's clock and the run is live in this kernel.
//! Every refusal
//! is [`KernelError::AgentDenied`], so a caller learns nothing about why. Every `agent_*` call
//! checks again, so a token revoked mid-connection stops working at once.
//!
//! # What a run may do
//!
//! Four things, all through [`AgentSession`]:
//!
//! - `report`: a structured status, journaled by the run's supervisor;
//! - `request_approval`: a normal approval (policy, repeat denial, deadline, `approval.decide`)
//!   raised by the supervisor, answered when it is decided, expires or the run ends;
//! - `board_read` and `board_write`: the run's own scope, `bot:<name>`, or `run:<id>` when it has
//!   no bot. The scope is never a parameter: it comes from the token, so a run cannot name
//!   another bot's board.

use std::fmt;
use std::time::Duration;

use reno_proto::{
    AGENT_REASON_MAX_CHARS, AgentApprovalParams, AgentApprovalResult, AgentReportParams,
    ApprovalSubject, BOARD_KEY_MAX_BYTES, BOARD_VALUE_MAX_BYTES, BoardEntry, ExecEvent, ExecSpec,
    REPORT_DETAILS_MAX_CHARS, REPORT_SUMMARY_MAX_CHARS, RunId,
};
use reno_store::{BoardRow, BoardWrite, NewRunToken, RunRecord};
use tokio::sync::oneshot;

use crate::error::{KernelError, Result};
use crate::kernel::Kernel;
use crate::shared::{Shared, SupCmd};

/// The shortest the wall-clock safety net of a token may be.
const TOKEN_SAFETY_FLOOR: Duration = Duration::from_secs(24 * 60 * 60);

/// How many times the run's wall-time budget the safety net of a token is, when that is longer
/// than [`TOKEN_SAFETY_FLOOR`].
const TOKEN_SAFETY_FACTOR: u32 = 4;

/// The wall-clock `expires_at` of a run's token, as a duration from now: the greater of
/// [`TOKEN_SAFETY_FACTOR`] times the run's wall-time budget and a day.
///
/// This is a safety net and not the run's budget. A run's life is measured in awake time (its
/// wall-time budget stops while a Mac sleeps), so a token that expired at "wall time + budget"
/// would die under a run that is still legitimately live after the machine slept. What bounds a
/// token is the run: it is valid while the run is live in this kernel and not revoked. The
/// expiry only catches a token that was never revoked for a run that is gone.
fn token_safety_net(wall_time: Duration) -> Duration {
    wall_time
        .saturating_mul(TOKEN_SAFETY_FACTOR)
        .max(TOKEN_SAFETY_FLOOR)
}

/// The board scope of a run: its bot's private one, or its own when it has no bot.
pub(crate) fn scope_of(bot: Option<&str>, run: &RunId) -> String {
    bot.map_or_else(|| format!("run:{run}"), |bot| format!("bot:{bot}"))
}

/// Issues the run's link, stores its token and puts Reno's MCP server first in `spec.mcp`. A
/// kernel without a `RunLink` does nothing. `Err` is the run's diagnostic: a run that cannot be
/// given its link does not start without it.
pub(crate) async fn provision(
    shared: &Shared,
    row: &RunRecord,
    wall_time: Duration,
    spec: &mut ExecSpec,
) -> std::result::Result<(), String> {
    let Some(link) = &shared.link else {
        return Ok(());
    };
    let issued = link
        .issue(&row.id)
        .await
        .map_err(|error| format!("McpLinkFailed: {error}"))?;
    let bot = row.bot.as_ref().map(|pin| pin.name.clone());
    let lifetime = token_safety_net(wall_time);
    let expires_at = shared
        .db
        .now_ms()
        .saturating_add(i64::try_from(lifetime.as_millis()).unwrap_or(i64::MAX));
    let token = NewRunToken {
        token_hash: issued.token_hash,
        run_id: row.id.clone(),
        scopes: vec![scope_of(bot.as_deref(), &row.id)],
        bot,
        expires_at,
    };
    if let Err(error) = shared.db.put_run_token(token).await {
        link.release(&row.id).await;
        return Err(format!(
            "McpLinkFailed: cannot store the run token: {error}"
        ));
    }
    // Reno's own server is always first; a harness keeps the first of a name.
    spec.mcp.retain(|server| server.name != issued.server.name);
    spec.mcp.insert(0, issued.server);
    Ok(())
}

/// Revokes the run's tokens and removes its link. Called when the run ends or fails to start.
pub(crate) async fn revoke(shared: &Shared, run: &RunId) {
    let Some(link) = &shared.link else {
        return;
    };
    if let Err(error) = shared.db.revoke_run_tokens(run).await {
        tracing::error!(%run, %error, "could not revoke the run's tokens");
    }
    link.release(run).await;
}

/// Start-up: no run survives a restart, so every open token is revoked and the link cleared.
pub(crate) async fn revoke_orphans(shared: &Shared) -> Result<()> {
    shared.db.revoke_open_run_tokens().await?;
    if let Some(link) = &shared.link {
        link.clear().await;
    }
    Ok(())
}

/// A connection's proof that it speaks for one run. Made by [`Kernel::agent_authenticate`];
/// every call made with it checks the token again.
#[derive(Clone, PartialEq, Eq)]
pub struct AgentSession {
    run: RunId,
    bot: Option<String>,
    scope: String,
    token_hash: String,
}

impl fmt::Debug for AgentSession {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentSession")
            .field("run", &self.run)
            .field("bot", &self.bot)
            .field("scope", &self.scope)
            .finish_non_exhaustive()
    }
}

impl AgentSession {
    /// The run the session speaks for.
    pub fn run(&self) -> &RunId {
        &self.run
    }

    /// The run's bot.
    pub fn bot(&self) -> Option<&str> {
        self.bot.as_deref()
    }

    /// The board scope the session reads and writes.
    pub fn scope(&self) -> &str {
        &self.scope
    }
}

fn entry(row: BoardRow, with_value: bool) -> BoardEntry {
    BoardEntry {
        key: row.key,
        value: with_value.then_some(row.value),
        version: row.version,
        size: row.size,
        updated_at_ms: u64::try_from(row.updated_at).unwrap_or(0),
    }
}

fn too_long(what: &str, limit: usize, unit: &str) -> KernelError {
    KernelError::Invalid(format!("{what} is longer than {limit} {unit}"))
}

impl Kernel {
    /// Checks a run token and answers the session it opens.
    ///
    /// `token_hash` is the digest of the token the caller presented
    /// ([`reno_proto::token_digest`]); the kernel never sees the token.
    ///
    /// Errors: [`KernelError::AgentDenied`] for every reason the token is not valid.
    pub async fn agent_authenticate(&self, run: &RunId, token_hash: &str) -> Result<AgentSession> {
        let shared = self.shared();
        let Some(token) = shared.db.run_token(run, token_hash).await? else {
            return Err(KernelError::AgentDenied);
        };
        let valid = token.revoked_at.is_none()
            && token.expires_at > shared.db.now_ms()
            && shared.is_live(run);
        let Some(scope) = token.scopes.first().cloned().filter(|_| valid) else {
            return Err(KernelError::AgentDenied);
        };
        Ok(AgentSession {
            run: run.clone(),
            bot: token.bot,
            scope,
            token_hash: token_hash.to_owned(),
        })
    }

    /// Journals a report as the run's own event.
    pub async fn agent_report(
        &self,
        session: &AgentSession,
        params: AgentReportParams,
    ) -> Result<()> {
        self.agent_authenticate(&session.run, &session.token_hash)
            .await?;
        if params.summary.chars().count() > REPORT_SUMMARY_MAX_CHARS {
            return Err(too_long(
                "the summary",
                REPORT_SUMMARY_MAX_CHARS,
                "characters",
            ));
        }
        if params
            .details
            .as_ref()
            .is_some_and(|details| details.chars().count() > REPORT_DETAILS_MAX_CHARS)
        {
            return Err(too_long(
                "the details",
                REPORT_DETAILS_MAX_CHARS,
                "characters",
            ));
        }
        let event = ExecEvent::AgentReport {
            status: params.status,
            summary: params.summary,
            details: params.details,
        };
        let (reply, rx) = oneshot::channel();
        self.send_to_run(&session.run, SupCmd::Report { event, reply })
            .await?;
        rx.await
            .map_err(|_| KernelError::RunEnded(session.run.clone()))?
    }

    /// Asks for an approval on the run's behalf and waits for the answer: a decision, the
    /// approval's deadline, or the end of the run. The request is judged like any other (policy,
    /// repeat denial) and, when a human must decide, listed with the pending approvals.
    pub async fn agent_request_approval(
        &self,
        session: &AgentSession,
        params: AgentApprovalParams,
    ) -> Result<AgentApprovalResult> {
        self.agent_authenticate(&session.run, &session.token_hash)
            .await?;
        if params.reason.chars().count() > AGENT_REASON_MAX_CHARS {
            return Err(too_long("the reason", AGENT_REASON_MAX_CHARS, "characters"));
        }
        let subject = ApprovalSubject {
            action: params.action,
            tool: "request_approval".to_owned(),
            command: params.command,
            paths: params.paths.into_iter().map(Into::into).collect(),
            raw: serde_json::json!({
                "source": "reno_mcp",
                "reason": params.reason,
            }),
        };
        let (reply, rx) = oneshot::channel();
        self.send_to_run(&session.run, SupCmd::AgentApproval { subject, reply })
            .await?;
        let decision = rx
            .await
            .map_err(|_| KernelError::RunEnded(session.run.clone()))?;
        Ok(AgentApprovalResult {
            decision: decision.outcome,
            by: decision.by,
            message: decision.message,
        })
    }

    /// Reads one key of the run's own scope, or lists the scope without values.
    pub async fn agent_board_read(
        &self,
        session: &AgentSession,
        key: Option<&str>,
    ) -> Result<(String, Vec<BoardEntry>)> {
        let session = self
            .agent_authenticate(&session.run, &session.token_hash)
            .await?;
        let db = &self.shared().db;
        let entries = match key {
            Some(key) => db
                .board_get(session.scope(), key)
                .await?
                .into_iter()
                .map(|row| entry(row, true))
                .collect(),
            None => db
                .board_list(session.scope())
                .await?
                .into_iter()
                .map(|row| entry(row, false))
                .collect(),
        };
        Ok((session.scope, entries))
    }

    /// Writes one key of the run's own scope. With `expected_version` the write is compare and
    /// set: a stale one changes nothing and is [`KernelError::BoardStale`] with the current entry.
    pub async fn agent_board_write(
        &self,
        session: &AgentSession,
        key: &str,
        value: &str,
        expected_version: Option<u64>,
    ) -> Result<(String, u64)> {
        let session = self
            .agent_authenticate(&session.run, &session.token_hash)
            .await?;
        if key.is_empty() {
            return Err(KernelError::Invalid("a board key is not empty".into()));
        }
        if key.len() > BOARD_KEY_MAX_BYTES {
            return Err(too_long("the key", BOARD_KEY_MAX_BYTES, "bytes"));
        }
        if value.len() > BOARD_VALUE_MAX_BYTES {
            return Err(too_long("the value", BOARD_VALUE_MAX_BYTES, "bytes"));
        }
        let written = self
            .shared()
            .db
            .board_put(session.scope(), key, value, expected_version, &session.run)
            .await?;
        match written {
            BoardWrite::Written { version } => Ok((session.scope, version)),
            BoardWrite::Stale { current } => Err(KernelError::BoardStale {
                current: current.map(|row| Box::new(entry(row, true))),
            }),
            BoardWrite::Full => Err(KernelError::BoardFull {
                scope: session.scope,
            }),
        }
    }

    /// Sends `cmd` to the run's supervisor. A run that is not live is not one a token can speak
    /// for.
    async fn send_to_run(&self, run: &RunId, cmd: SupCmd) -> Result<()> {
        let Some(commands) = self.shared().live_commands(run) else {
            return Err(KernelError::AgentDenied);
        };
        commands
            .send(cmd)
            .await
            .map_err(|_| KernelError::RunEnded(run.clone()))
    }
}
