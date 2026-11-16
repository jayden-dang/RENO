//! The native Codex executor: spawns the installed `codex` CLI as `codex app-server` and speaks
//! JSON-RPC 2.0 over stdio, the interface the Codex IDE extension uses, and implements
//! [`Executor`].
//!
//! VERSION PIN: the app-server API is EXPERIMENTAL (`capabilities.experimentalApi`). The request
//! shapes were read from the 0.164.0 schema and the driver's live fixtures start at 0.146.1;
//! [`MIN_CLI_VERSION`] is the oldest CLI refused up front, [`VERIFIED_CLI_VERSION`] the newest
//! this driver targets (the one installed where the native contract was written: no live run
//! against it has been made, so it is a claim to confirm with the live tests, `--ignored`).
//! Revalidate the method and notification surface when bumping.
//!
//! - `wire::codex` is sans-IO: notifications and items become [`ExecEvent`]s there, requests
//!   become `ApprovalSubject`s and question lists. This module only does the I/O.
//! - `initialize` handshake (clientInfo + `capabilities.experimentalApi`, opting out of the
//!   high-volume notifications nothing consumes) then the `initialized` notification; unknown
//!   notification methods tolerated. An app-server that dies before answering is relaunched
//!   (1, 3, 8 s apart), and every launch is reported as a process record.
//! - `thread/start` (or `thread/resume`, unarchiving an archived thread; only a conversation
//!   that is gone falls back to a fresh thread, and `Started` names the thread the run really
//!   has) then `turn/start`, carrying the prompt, model, effort, `sandboxPolicy` and approval
//!   policy. A resumed thread still in plan mode is taken out of it: the turn names `default`.
//! - APPROVALS ARE STRUCTURED. A command, file-change or permission request, and an MCP tool
//!   approval (an elicitation tagged by its `_meta`), become `NeedsApproval` with a structured
//!   [`reno_proto::ApprovalSubject`] and are answered through `ExecHandle::decide` as Codex's
//!   `accept` or `decline`, never `acceptForSession`. An elicitation form and the agent's own
//!   questions become `NeedsInput`, answered through `ExecHandle::answer_input`. No prose is
//!   written or parsed on this path.
//! - ONE RUN IS ONE `Done`. The app-server and its thread are not kept for a
//!   next turn. The end of the turn ends the run and the app-server's process group
//!   is torn down, so what the agent left running ends with it. The one exception is a steer
//!   Codex refused because its turn just ended: that steer starts the next turn inside the same
//!   run, and the first turn's end is not the run's.
//! - Steering: `turn/steer { expectedTurnId }` into the live turn. A rejected steer (the
//!   turn-completed race) is queued and delivered as the next `turn/start` on the same thread.
//!   Steers queue in order.
//! - Subagents are full child app-server threads. Their notifications are consumed by an
//!   explicit table ([`crate::wire::codex::normalize::route_child_notification`]) so a child's
//!   turn end can never settle the run's turn, and unknown methods fall through to the run (fail
//!   open, never silent loss). A child's approval request is answered like the run's own.
//! - Only the turn in flight settles: a stale or duplicate completion is ignored.
//! - Cancel: cancelling `ExecControls::cancel` declines every open request (and any that arrives
//!   later), sends `turn/interrupt`, then SIGTERM and SIGKILL after their graces. The stream
//!   ends with `Done { stop: Interrupted }`.
//! - Process records: every app-server this module spawns for a run is reported through
//!   `ExecControls::processes` as soon as it exists, so a crash of the daemon leaves a row for
//!   the orphan sweep (ARCHITECTURE 17.3). The short-lived `model/list` probe is not a run and
//!   reports none: it is bounded by its own timeout and reaped before it returns.

pub mod npm_repair;

use crate::wire::codex::catalog;
use crate::wire::codex::elicitation;
use crate::wire::codex::normalize::{
    ChildRoute, Normalizer, Phase, TurnEnd, notification_thread_id, route_child_notification,
    turn_id,
};
use crate::wire::codex::permission::{self, Asked, Authority, Elicitation};

use std::collections::{HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use reno_proto::{
    AccessMode, ApprovalSubject, ExecEvent, ExecSpec, ExecutorCaps, ExecutorDescriptor, ExecutorId,
    HealthProbe, McpServerSpec, ModelCatalog, ModelInfo, SandboxLevel, SessionRef, SignedIn,
    Steering, StopReason, VersionGate, VersionRange,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::exec::{
    Decision, ExecAnswer, ExecControls, ExecError, Executor, InputAnswer, Open, OpenRequests,
    ProcessSink, Resolved, probe, require_access,
};
use crate::jsonrpc::{Incoming, RpcClient, transport_lost};
use crate::process::owned::Child as OwnedChild;
use crate::process::{Command, Stdio};
use crate::{HarnessError, Signal, send_signal};

/// The oldest Codex CLI whose app-server speaks the wire this driver maps: nested `error`
/// notifications, `turn/steer`, child-thread routing (the live captures under
/// `tests/fixtures/codex` start at 0.146.1). An older CLI fails confusingly mid-run instead of
/// saying it needs an update.
pub const MIN_CLI_VERSION: &str = "0.146.0";

/// The newest Codex CLI version this driver targets: the one installed where the native contract
/// was written. The request shapes were read from the 0.164.0 schema and the fixtures follow the
/// 0.146 to 0.164 wire; nothing was run against 0.169.0, so this is a claim to confirm with the
/// live tests (`--ignored`).
pub const VERIFIED_CLI_VERSION: &str = "0.169.0";

/// The executor contract version this driver implements (`ExecutorDescriptor::contract_version`).
const CONTRACT_VERSION: u32 = 1;

/// The id this executor registers under.
pub const EXECUTOR_ID: &str = "codex";

static ID: LazyLock<ExecutorId> = LazyLock::new(|| match EXECUTOR_ID.parse() {
    Ok(id) => id,
    Err(error) => unreachable!("{EXECUTOR_ID} is a valid executor id: {error}"),
});

/// What the stream carries before the consumer reads: room for a burst of notifications.
const EVENT_CAPACITY: usize = 256;

/// Requests the app-server may have open on a person at once (ARCH-11). A flood beyond this (a
/// misbehaving MCP server) is refused as overloaded instead of growing without bound.
const MAX_OPEN_REQUESTS: usize = 32;

/// The longest a Reno MCP tool may take: its longest wait (an hour) plus margin for the reply.
/// Codex gives up on a tool call after 60 s by default.
const RENO_TOOL_TIMEOUT_SECS: u64 = 3600 + 120;

/// Waits before relaunching an app-server that died before answering `initialize`: a
/// just-started machine can lose the first launch (a CLI still finishing its install, a cold
/// disk), and nothing has run yet.
const STARTUP_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_secs(1),
    Duration::from_secs(3),
    Duration::from_secs(8),
];

/// High-volume notifications nothing here consumes: the turn's running diff and every patch's
/// output stream. Opting out at `initialize` spares the wire (and the reader) from parsing them
/// only to drop them.
const OPTED_OUT_NOTIFICATIONS: &[&str] = &["turn/diff/updated", "item/fileChange/outputDelta"];

/// Locate the device's installed Codex CLI: `CODEX_EXECUTABLE`, then our own PATH, then the
/// login-shell PATH snapshot (the user's shell init shapes PATH in ways a GUI/service launch
/// never sees, see [`crate::shell_env`]), then known install locations as a last resort.
/// Resolved per call: cheap after the snapshot is cached.
pub fn resolve_codex_executable() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CODEX_EXECUTABLE").filter(|p| !p.is_empty()) {
        return crate::executable::validate_native_override(&PathBuf::from(p)).ok();
    }
    let mut extra = Vec::new();
    if let Some(home) = crate::executable::home_dir() {
        extra.push(home.join(".local").join("bin").join("codex"));
        extra.push(home.join(".codex").join("bin").join("codex"));
        extra.push(home.join(".npm-global").join("bin").join("codex"));
    }
    extra.push(PathBuf::from("/opt/homebrew/bin/codex"));
    extra.push(PathBuf::from("/usr/local/bin/codex"));
    crate::executable::find_on_paths("codex", extra)
}

/// Dotted `thread/start` config overrides that add the injected MCP servers to the user's
/// `mcp_servers` table. Reno's own server is the first of `servers`; the rest are a plugin's.
///
/// Reno's server is pre-approved (`default_tools_approval_mode`) only when `pre_approve` says
/// nobody is asked: under any mode that asks, a call to it is an approval like any other and
/// reaches Reno as `NeedsApproval`. A plugin's tools are never pre-approved. Only Reno's server
/// gets the long tool timeout and the generous start-up time.
fn mcp_overrides(servers: &[McpServerSpec], pre_approve: bool) -> Vec<(String, Value)> {
    let mut seen = HashSet::new();
    let mut overrides = Vec::new();
    for (at, server) in servers.iter().enumerate() {
        // A name that repeats keeps its first entry: Reno's own server is first.
        if !seen.insert(server.name.as_str()) {
            continue;
        }
        let key = |field: &str| format!("mcp_servers.{}.{field}", server.name);
        overrides.push((key("command"), server.command.clone().into()));
        overrides.push((key("args"), json!(server.args)));
        overrides.push((key("env"), json!(server.env)));
        if at == 0 {
            if pre_approve {
                overrides.push((key("default_tools_approval_mode"), "approve".into()));
            }
            // Reno's `wait_for_turn` (or a send that waits) may block for up to an hour while
            // another chat works.
            overrides.push((key("tool_timeout_sec"), RENO_TOOL_TIMEOUT_SECS.into()));
            // A cold start (a fresh machine paging the binary in) can outlast the 10 s default
            // before the server answers `initialize`.
            overrides.push((key("startup_timeout_sec"), 30.into()));
        }
    }
    overrides
}

/// `Some(why)` when `version` is a CLI older than [`MIN_CLI_VERSION`]; an unknown version (a
/// wrapper, a dev build) is given the benefit of the doubt. A pre-release counts as its release:
/// `0.146.0-alpha.2` already speaks the wire, though semver orders it before `0.146.0`.
fn too_old_message(version: Option<&semver::Version>) -> Option<String> {
    let version = version?;
    let release = semver::Version::new(version.major, version.minor, version.patch);
    let min = semver::Version::parse(MIN_CLI_VERSION).ok()?;
    (release < min).then(|| {
        format!(
            "Codex CLI {version} is too old; update it to {min} or newer \
             (npm install -g @openai/codex@latest)"
        )
    })
}

/// The Codex executor. Construct with [`CodexExecutor::new`]; tests point it at a fake
/// app-server with [`CodexExecutor::with_executable`].
///
/// # Access modes
///
/// Codex decides what runs without asking from its approval policy and its sandbox; whatever
/// they do not cover is sent to Reno as a server request. Reno's access mode picks both, and the
/// spec's sandbox can only narrow the sandbox (Codex is never launched with more than asked,
/// ARCH-15):
///
/// | `AccessMode` | `approvalPolicy` | Sandbox before the spec's ceiling | Codex asks about | Reaches Reno as `NeedsApproval` | This driver answers itself |
/// |---|---|---|---|---|---|
/// | `FullAccess` | `never` | `danger-full-access` | nothing | nothing | every command, file change and permission request that arrives anyway, when the sandbox is unrestricted; MCP tool approvals always |
/// | `AutoAcceptEdits` | `untrusted` | `workspace-write` (network on) | everything but its known-safe reads, file changes included | commands, permission grants, MCP tool calls, and file changes that are not provably inside the working directory | file changes whose every path, symlinks followed, is inside the working directory |
/// | `Supervised` | `untrusted` | `read-only` | everything but its known-safe reads | all of it | nothing |
///
/// A run whose mode asks routes its approvals to the person explicitly (`approvalsReviewer:
/// user`): a configured reviewer could otherwise hand them to an AI. Under `FullAccess` with a
/// sandbox narrower than unrestricted, Codex never asks, so a request that arrives anyway asks
/// to leave the sandbox the spec asked for: it is asked, not accepted (its file changes inside
/// the working directory excepted, as under `AutoAcceptEdits`). Whatever the mode, an approval
/// that is neither a command, a file change, a permission grant nor an MCP tool call (another
/// privileged elicitation, `browser_auth` for one) is asked: no mode answers it unseen. The
/// table is applied by `wire::codex::permission`, and the symlinks by this module. An access
/// mode is never widened mid-run.
///
/// [`Self::with_access_modes`] narrows what an instance will honour; `start` refuses any other
/// mode with [`ExecError::UnsupportedAccess`] before spawning (ARCH-15).
pub struct CodexExecutor {
    executable: Option<PathBuf>,
    /// Grace between `turn/interrupt` and SIGTERM.
    interrupt_grace: Duration,
    /// Grace between SIGTERM and SIGKILL.
    kill_grace: Duration,
    /// Waits before each relaunch of an app-server that died before answering `initialize`.
    startup_retry_delays: Vec<Duration>,
    access_modes: Vec<AccessMode>,
    /// The CLI version `probe` last read, for the descriptor.
    cli_version: Arc<Mutex<Option<String>>>,
    /// The models `model/list` last returned, for the descriptor.
    models: Arc<Mutex<Option<Vec<ModelInfo>>>>,
    /// How long `probe` lets its short-lived app-server take in all.
    probe_budget: Duration,
    tracker: TaskTracker,
}

impl std::fmt::Debug for CodexExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CodexExecutor")
            .field("executable", &self.executable)
            .field("access_modes", &self.access_modes)
            .finish_non_exhaustive()
    }
}

impl Default for CodexExecutor {
    fn default() -> Self {
        Self {
            executable: None,
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
            startup_retry_delays: STARTUP_RETRY_DELAYS.to_vec(),
            access_modes: vec![
                AccessMode::Supervised,
                AccessMode::AutoAcceptEdits,
                AccessMode::FullAccess,
            ],
            cli_version: Arc::new(Mutex::new(None)),
            models: Arc::new(Mutex::new(None)),
            probe_budget: probe::WIRE_BUDGET,
            tracker: TaskTracker::new(),
        }
    }
}

impl CodexExecutor {
    /// An executor that finds the CLI on PATH, in the login shell's PATH or in a known install
    /// location.
    pub fn new() -> Self {
        Self::default()
    }

    /// Use a fixed CLI binary instead of PATH/known-location resolution.
    #[must_use]
    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    /// Tune the interrupt, SIGTERM, SIGKILL escalation timing.
    #[must_use]
    pub fn with_graces(mut self, interrupt_grace: Duration, kill_grace: Duration) -> Self {
        self.interrupt_grace = interrupt_grace;
        self.kill_grace = kill_grace;
        self
    }

    /// Tune the waits before relaunching an app-server that died before answering `initialize`:
    /// one relaunch per entry.
    #[must_use]
    pub fn with_startup_retry_delays(mut self, delays: Vec<Duration>) -> Self {
        self.startup_retry_delays = delays;
        self
    }

    /// Bound the app-server `probe` starts (tests shrink it; default 20 s).
    #[must_use]
    pub fn with_probe_budget(mut self, budget: Duration) -> Self {
        self.probe_budget = budget;
        self
    }

    /// Honour only these access modes: the descriptor lists them and `start` refuses the rest.
    #[must_use]
    pub fn with_access_modes(mut self, modes: Vec<AccessMode>) -> Self {
        self.access_modes = modes;
        self
    }

    /// Whether the CLI can be launched now: the launch resolver, not bare discovery, so a valid
    /// `CODEX_EXECUTABLE` (or a fixed `executable`) reports installed and an invalid one does
    /// not.
    pub fn installed(&self) -> bool {
        self.resolve_executable().is_ok()
    }

    fn resolve_executable(&self) -> Result<PathBuf, ExecError> {
        resolve_executable(self.executable.as_deref())
    }

    /// Ask the app-server which models the signed-in account may use (`model/list`, paged;
    /// hidden models are skipped, the account's default leads) and keep the answer for the
    /// descriptor. A failure leaves what the descriptor already offered: the last discovery, or
    /// the curated list. [`Executor::probe`] does this too, together with the sign-in check.
    ///
    /// Spawns a short-lived app-server, reaped before this returns; it is not a run, so it
    /// reports no process record.
    pub async fn discover_models(&self) -> Result<Vec<ModelInfo>, HarnessError> {
        let exe = self
            .resolve_executable()
            .map_err(|error| HarnessError::NotInstalled(error.to_string()))?;
        self.query_app_server(&exe, false, Duration::from_secs(10))
            .await?
            .models
    }

    /// One short-lived `codex app-server` that answers, within `budget`, the model catalog and,
    /// when `want_account`, `account/read` (the login Codex holds, read without refreshing it).
    /// `Err` only when the app-server could not be started, did not initialize or ran out of
    /// time; a failed `account/read` is `Unknown` and a failed catalog is in
    /// [`AppServerAnswers::models`]. A catalog that was found is kept for the descriptor.
    async fn query_app_server(
        &self,
        exe: &Path,
        want_account: bool,
        budget: Duration,
    ) -> Result<AppServerAnswers, HarnessError> {
        let mut cmd = Command::new(exe);
        cmd.arg("app-server");
        crate::compose_child_path(&mut cmd, exe);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = crate::process::owned::spawn_grouped(&mut cmd).map_err(|e| {
            crate::executable::spawn_error(e, None, || crate::executable::binary_hint(exe))
        })?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            child.shutdown(self.kill_grace).await;
            return Err(HarnessError::Protocol("codex child has no stdio".into()));
        };
        let (client, _incoming) = RpcClient::spawn(&self.tracker, stdin, stdout);
        let discovery = async {
            client
                .request("initialize", initialize_params(false))
                .await?;
            client.notify("initialized", None);
            let signed_in = if want_account {
                Some(
                    match client
                        .request("account/read", json!({ "refreshToken": false }))
                        .await
                    {
                        Ok(reply) => catalog::account_state(&reply),
                        Err(_) => SignedIn::Unknown,
                    },
                )
            } else {
                None
            };
            Ok::<_, HarnessError>(AppServerAnswers {
                signed_in,
                models: list_models(&client).await,
            })
        };
        let result = tokio::time::timeout(budget, discovery).await;
        child.shutdown(self.kill_grace).await;
        client.abort();
        let answers = match result {
            Ok(inner) => inner?,
            Err(_) => return Err(HarnessError::Protocol("app-server probe timed out".into())),
        };
        if let Ok(models) = &answers.models {
            *self.models.lock().unwrap_or_else(PoisonError::into_inner) = Some(models.clone());
        }
        Ok(answers)
    }
}

/// What a short-lived app-server told the probe.
struct AppServerAnswers {
    /// `account/read`'s verdict; `None` when it was not asked.
    signed_in: Option<SignedIn>,
    /// The model catalog, or why there is none.
    models: Result<Vec<ModelInfo>, HarnessError>,
}

/// The account's model catalog, paged, hidden models skipped, the account's default first.
async fn list_models(client: &RpcClient) -> Result<Vec<ModelInfo>, HarnessError> {
    let mut models: Vec<ModelInfo> = Vec::new();
    let mut seen_cursors = HashSet::new();
    let mut cursor: Option<String> = None;
    let mut default_model: Option<String> = None;
    loop {
        let mut params = json!({ "limit": 20, "includeHidden": false });
        if let Some(cursor) = cursor.as_deref() {
            params["cursor"] = Value::String(cursor.to_owned());
        }
        let page = client.request("model/list", params).await?;
        let (found, next) = catalog::parse_model_page(&page);
        for (model, is_default) in found {
            if models.iter().all(|known| known.id != model.id) {
                if is_default && default_model.is_none() {
                    default_model = Some(model.id.clone());
                }
                models.push(model);
            }
        }
        let Some(next) = next.filter(|next| !next.is_empty()) else {
            break;
        };
        if !seen_cursors.insert(next.clone()) {
            break;
        }
        cursor = Some(next);
    }
    if let Some(default) = default_model
        && let Some(at) = models.iter().position(|model| model.id == default)
        && at != 0
    {
        let model = models.remove(at);
        models.insert(0, model);
    }
    if models.is_empty() {
        return Err(HarnessError::Protocol(
            "Codex returned an empty model catalog".into(),
        ));
    }
    Ok(models)
}

/// The CLI to launch: the fixed `executable` if there is one, else `CODEX_EXECUTABLE`, else
/// wherever it is installed.
fn resolve_executable(executable: Option<&Path>) -> Result<PathBuf, ExecError> {
    let not_installed = |error: HarnessError| ExecError::NotInstalled(error.to_string());
    if let Some(p) = executable {
        return crate::executable::validate_native_override(p).map_err(not_installed);
    }
    if let Some(p) = std::env::var_os("CODEX_EXECUTABLE")
        && !p.is_empty()
    {
        return crate::executable::validate_native_override(&PathBuf::from(p))
            .map_err(not_installed);
    }
    resolve_codex_executable().ok_or_else(|| {
        ExecError::NotInstalled(
            "codex (searched PATH, the login shell's PATH, ~/.local/bin, \
             ~/.codex/bin, ~/.npm-global/bin, /opt/homebrew/bin, /usr/local/bin, \
             and fnm/nvm/volta/pnpm/bin install dirs; set CODEX_EXECUTABLE \
             to override)"
                .into(),
        )
    })
}

fn initialize_params(opt_out: bool) -> Value {
    let mut capabilities = json!({ "experimentalApi": true });
    if opt_out {
        capabilities["optOutNotificationMethods"] = json!(OPTED_OUT_NOTIFICATIONS);
    }
    json!({
        "clientInfo": {
            "name": "reno-native",
            "title": "Reno",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "capabilities": capabilities,
    })
}

#[async_trait]
impl Executor for CodexExecutor {
    fn descriptor(&self) -> ExecutorDescriptor {
        let cli = self
            .cli_version
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let models = self
            .models
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .unwrap_or_else(catalog::static_model_infos);
        ExecutorDescriptor {
            id: ID.clone(),
            contract_version: CONTRACT_VERSION,
            adapter_version: match cli {
                Some(cli) => format!(
                    "reno-harness {} codex driver (CLI {cli})",
                    env!("CARGO_PKG_VERSION")
                ),
                None => format!("reno-harness {} codex driver", env!("CARGO_PKG_VERSION")),
            },
            cli_verified: VersionRange {
                min: MIN_CLI_VERSION.into(),
                verified: VERIFIED_CLI_VERSION.into(),
            },
            caps: ExecutorCaps {
                // `turn/steer` lands in the running turn; a steer that misses it starts the next
                // turn.
                steering: Steering::StepBoundary,
                resume: true,
                access_modes: self.access_modes.clone(),
                // The spec's sandbox is a ceiling: each level is what a run gets at most, and
                // the access mode can only narrow it (see the table on the type).
                sandbox_levels: vec![
                    SandboxLevel::ReadOnly,
                    SandboxLevel::WorkspaceWrite,
                    SandboxLevel::Unrestricted,
                ],
                mcp_injection: true,
                structured_output: false,
                // Done is the app-server's own terminal `turn/completed`.
                deterministic_turn_end: true,
                // Codex has a plan collaboration mode, but `ExecSpec` cannot request it; never
                // declare a capability a caller cannot use (ARCH-15).
                plan_mode: false,
                models,
            },
        }
    }

    /// Where the CLI is, its version against the verified range, and, from one short-lived
    /// app-server, whether Codex holds a login (`account/read`, no refresh, no model call) and
    /// which models the account may use (`model/list`). When the app-server cannot be asked the
    /// sign-in is `Unknown` and the models are the last live list, else the curated one.
    async fn probe(&self) -> HealthProbe {
        let overridden = self.executable.is_some()
            || std::env::var_os("CODEX_EXECUTABLE").is_some_and(|value| !value.is_empty());
        let fixed = self.executable.clone();
        let resolved =
            tokio::task::spawn_blocking(move || resolve_executable(fixed.as_deref())).await;
        let fallback = || {
            let found = self
                .models
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .clone();
            match found {
                Some(models) => ModelCatalog::live(models),
                None => ModelCatalog::curated(catalog::static_model_infos()),
            }
        };
        let exe = match resolved {
            Ok(Ok(exe)) => exe,
            Ok(Err(error)) => {
                return HealthProbe::not_installed(
                    probe::not_installed_reason(error.to_string(), overridden),
                    fallback(),
                );
            }
            Err(error) => return probe::failed(error, fallback()),
        };
        let version = probe::cli_version(exe.clone()).await;
        *self
            .cli_version
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = version.clone();
        let version_gate = reno_proto::version_gate(
            version.as_deref(),
            &VersionRange {
                min: MIN_CLI_VERSION.into(),
                verified: VERIFIED_CLI_VERSION.into(),
            },
        );
        let (signed_in, models) = if matches!(version_gate, VersionGate::TooOld { .. }) {
            // An app-server this old is not launched.
            (SignedIn::Unknown, fallback())
        } else {
            match self.query_app_server(&exe, true, self.probe_budget).await {
                Ok(answers) => (
                    answers.signed_in.unwrap_or(SignedIn::Unknown),
                    answers.models.map_or_else(
                        |error| {
                            tracing::debug!(%error, "codex model discovery failed; keeping the last list");
                            fallback()
                        },
                        ModelCatalog::live,
                    ),
                ),
                Err(error) => {
                    tracing::debug!(%error, "the codex app-server probe failed");
                    (SignedIn::Unknown, fallback())
                }
            }
        };
        HealthProbe {
            installed: probe::installed(exe, version),
            version_gate,
            signed_in,
            models,
        }
    }

    async fn start(
        &self,
        spec: ExecSpec,
        controls: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError> {
        // Fail closed before anything is resolved or spawned (ARCH-15).
        require_access(spec.access, &self.access_modes)?;
        if spec.output_schema.is_some() {
            return Err(ExecError::UnsupportedOutputSchema);
        }
        let cwd = spec
            .cwd
            .to_str()
            .ok_or_else(|| ExecError::InvalidSpec("cwd is not valid UTF-8".into()))?
            .to_owned();
        let exe = self.resolve_executable()?;
        // Pin the physical release for this run: an installer may move the current symlink.
        let exe = exe
            .canonicalize()
            .map_err(|error| ExecError::NotInstalled(format!("{}: {error}", exe.display())))?;
        // Cached per binary; a fixed test executable is never version-probed.
        if self.executable.is_none() {
            let probe = exe.clone();
            let version =
                tokio::task::spawn_blocking(move || crate::executable::binary_version(&probe))
                    .await
                    .ok()
                    .flatten();
            if let Some(why) = too_old_message(version.as_ref()) {
                return Err(ExecError::NotInstalled(why));
            }
        }
        let ExecControls {
            steering,
            cancel,
            answers,
            processes,
        } = controls;

        let app = match launch(&self.tracker, &exe, &spec, &processes).await {
            Ok(app) => app,
            // Any failure to launch is a failed run, not an ineligible executor.
            Err(error) => {
                let diagnostic = crate::redact_secrets(&error.to_string());
                return Ok(futures::stream::once(async move {
                    ExecEvent::Done {
                        stop: StopReason::Error,
                        result: None,
                        diagnostic: Some(diagnostic),
                    }
                })
                .boxed());
            }
        };

        let (event_tx, event_rx) = mpsc::channel::<ExecEvent>(EVENT_CAPACITY);
        let session = Session {
            exe,
            cwd,
            authority: Authority::new(spec.access, spec.sandbox, &spec.cwd),
            spec,
            event_tx,
            steering,
            cancel,
            answers,
            processes,
            interrupt_grace: self.interrupt_grace,
            kill_grace: self.kill_grace,
            startup_retry_delays: self.startup_retry_delays.clone(),
            tracker: self.tracker.clone(),
        };
        self.tracker.spawn(run_session(session, app));

        Ok(futures::stream::unfold(event_rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })
        .boxed())
    }

    async fn shutdown(&self) {
        self.tracker.close();
        self.tracker.wait().await;
    }
}

// ---------------------------------------------------------------------------
// The app-server process
// ---------------------------------------------------------------------------

/// One `codex app-server` process and its wire.
struct AppServer {
    child: OwnedChild,
    client: RpcClient,
    incoming: mpsc::Receiver<Incoming>,
    stderr_tail: crate::StderrTail,
    /// The JSON-RPC writer and reader belong to `client`. Like the stderr reader they end when
    /// the child's pipes close, which a grandchild that escaped teardown can hold open forever:
    /// the run aborts them with [`Self::release`], so `shutdown` is bounded.
    stderr_task: Option<tokio::task::JoinHandle<()>>,
}

impl AppServer {
    /// Stop the tasks that read and write the child's pipes, once the child is reaped.
    fn release(&mut self) {
        self.client.abort();
        if let Some(handle) = self.stderr_task.take() {
            handle.abort();
        }
    }
}

/// Start one app-server for a run, wire its stdio up and report it for the orphan sweep.
async fn launch(
    tracker: &TaskTracker,
    exe: &Path,
    spec: &ExecSpec,
    processes: &ProcessSink,
) -> Result<AppServer, std::io::Error> {
    let mut cmd = Command::new(exe);
    cmd.arg("app-server");
    crate::compose_child_path(&mut cmd, exe);
    // The allowlisted environment of the run (host-resolved credentials: a Cloud device's
    // managed CODEX_HOME, GH_TOKEN, ...). Absent on laptops: the CLI's own login is used.
    crate::apply_run_env(&mut cmd, &spec.env);
    let has_cwd = !spec.cwd.as_os_str().is_empty();
    if has_cwd {
        cmd.current_dir(&spec.cwd);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = crate::process::owned::spawn_grouped(&mut cmd).map_err(|e| {
        let cwd = has_cwd.then_some(spec.cwd.as_path());
        match crate::executable::spawn_error(e, cwd, || crate::executable::binary_hint(exe)) {
            HarnessError::Io(error) => error,
            other => std::io::Error::other(other.to_string()),
        }
    })?;
    // Reported at once: a daemon that dies from here on still leaves its row.
    #[cfg(unix)]
    if let Some(record) = child.record() {
        let _ = processes.report(record).await;
    }
    let missing = |what: &str| std::io::Error::other(format!("codex child has no {what}"));
    let stdin = child.stdin.take().ok_or_else(|| missing("stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| missing("stdout"))?;
    let stderr_tail = crate::StderrTail::default();
    let stderr_task = child.stderr.take().map(|stderr| {
        let tail = stderr_tail.clone();
        tracker.spawn(async move {
            let mut lines = crate::lossy_lines::lossy_lines(stderr);
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "reno_harness::codex", "stderr: {line}");
                tail.push(&line);
            }
            // The tail is complete: a crash report need not wait it out.
            tail.close();
        })
    });
    let (client, incoming) = RpcClient::spawn(tracker, stdin, stdout);
    Ok(AppServer {
        child,
        client,
        incoming,
        stderr_tail,
        stderr_task,
    })
}

/// A setup request's failure as display text. When the app-server died under it, "exited before
/// responding" says nothing: report its exit status and last stderr lines instead.
async fn setup_failure(error: HarnessError, app: &mut AppServer) -> String {
    // Only a lost answer: a refusal that arrived before the exit says more.
    if !app.client.is_closed() || !transport_lost(&error) {
        return error.to_string();
    }
    crash_report(app).await
}

/// The exited app-server's status and last stderr lines, once its stderr has drained and it has
/// been reaped (each briefly waited for).
async fn crash_report(app: &mut AppServer) -> String {
    app.stderr_tail.wait_closed().await;
    let status = match tokio::time::timeout(Duration::from_millis(500), app.child.wait()).await {
        Ok(Ok(status)) => Some(status),
        _ => app.child.try_wait().ok().flatten(),
    };
    crate::crash_message("codex app-server", status, &app.stderr_tail)
}

/// Why a `thread/resume` failure means the conversation is gone for good, in words for the
/// restore notice: no rollout on this machine, one Codex can't read (empty, not UTF-8,
/// unparseable; live, 0.164.0: "failed to read thread: thread-store internal error: ..."), a
/// thread id Codex never issued. A fresh thread is the only way forward then. `None` for
/// anything else (a dead process, a config error), which would fail a fresh start too, or lose
/// history needlessly. A bare "not found" is no such sign: an I/O error ("entity not found"), a
/// missing environment variable or helper binary say it too.
fn resume_unrecoverable(error: &HarnessError) -> Option<&'static str> {
    let HarnessError::Protocol(message) = error else {
        return None;
    };
    if transport_lost(error) {
        return None;
    }
    let message = message.to_ascii_lowercase();
    let says = |needles: &[&str]| needles.iter().any(|needle| message.contains(needle));
    if says(&[
        "no rollout",
        "thread not found",
        "session not found",
        "conversation not found",
    ]) {
        Some("it isn't saved on this device")
    } else if says(&["failed to read thread", "rollout at"]) {
        // Only a history that is empty or unreadable as data is gone. One the OS would not hand
        // over (permissions, a lock, a busy disk) is still there for the next message.
        if says(&[
            "os error",
            "permission denied",
            "locked",
            "busy",
            "too many open files",
            "timed out",
        ]) {
            None
        } else if message.contains("is empty") {
            Some("its saved history is empty")
        } else if says(&[
            "valid utf-8",
            "failed to parse",
            "parse error",
            "expected value",
            "eof while parsing",
            "invalid json",
        ]) {
            Some("its saved history couldn't be read")
        } else {
            None
        }
    } else if says(&["invalid session id", "invalid thread id"]) {
        Some("Codex doesn't recognize its id")
    } else {
        None
    }
}

/// `thread/resume`, unarchiving the thread first when Codex reports it archived (the CLI's own
/// archive command, or another client's).
async fn resume_thread(client: &RpcClient, params: Value) -> Result<Value, HarnessError> {
    match client.request("thread/resume", params.clone()).await {
        Err(HarnessError::Protocol(message)) if message.contains("is archived") => {
            tracing::info!(
                target: "reno_harness::codex",
                "thread/resume: thread archived; unarchiving it"
            );
            client
                .request(
                    "thread/unarchive",
                    json!({ "threadId": params["threadId"] }),
                )
                .await?;
            client.request("thread/resume", params).await
        }
        other => other,
    }
}

// ---------------------------------------------------------------------------
// Turns
// ---------------------------------------------------------------------------

/// Turn-routing state: the `turn/start` response and the turn lifecycle notifications are
/// separate app-server messages that may arrive in either order. Never revive a turn that
/// `turn/completed` already declared finished.
#[derive(Default)]
struct TurnRouter {
    active: Option<String>,
    completed: VecDeque<String>,
}

impl TurnRouter {
    fn is_completed(&self, id: &str) -> bool {
        self.completed.iter().any(|c| c == id)
    }

    fn note_started(&mut self, id: String) {
        if id.is_empty() || self.is_completed(&id) {
            return;
        }
        // A replacement `turn/started` is authoritative evidence that a stale active turn is
        // over, even if its completion notification was lost.
        if let Some(prev) = self.active.take()
            && prev != id
        {
            self.remember_completed(prev);
        }
        self.active = Some(id);
    }

    fn note_completed(&mut self, id: &str) {
        if id.is_empty() {
            return;
        }
        self.remember_completed(id.to_owned());
        if self.active.as_deref() == Some(id) {
            self.active = None;
        }
    }

    /// Adopt a turn id from a `turn/start` RESPONSE (the notification is allowed to beat it). A
    /// turn already running keeps its id: a follow-up `turn/start` folds into it and answers
    /// with that same id anyway, and an announced turn is authoritative.
    fn adopt_started(&mut self, id: String) {
        if self.active.is_some() || id.is_empty() || self.is_completed(&id) {
            return;
        }
        self.active = Some(id);
    }

    /// Whether a `turn/completed` (or failed/aborted) for `id` ends the turn in flight. A
    /// completion for a turn already settled, or for another turn while ours runs, is stale:
    /// settling on it marked the running turn done mid-way.
    fn settles(&self, id: &str) -> bool {
        id.is_empty() || (!self.is_completed(id) && self.active.as_deref().is_none_or(|a| a == id))
    }

    fn remember_completed(&mut self, id: String) {
        self.completed.push_back(id);
        // Bounded, however many turns a steer-heavy run starts.
        while self.completed.len() > 32 {
            self.completed.pop_front();
        }
    }
}

/// What `thread/start` / `thread/resume` reports the thread runs with: the model and effort a
/// collaboration mode falls back to when the request names none, and whether a resumed thread is
/// still in plan mode.
struct ThreadMode {
    model: String,
    effort: Option<String>,
    plan: bool,
}

/// `turn/start`'s `collaborationMode` (`plan` or `default`). Its settings take precedence over
/// the turn's model and effort, so they repeat them; a null `developer_instructions` keeps the
/// mode's built-in ones (0.164.0 schema).
fn collaboration_mode(plan: bool, model: &str, effort: Option<&str>) -> Value {
    json!({
        "mode": if plan { "plan" } else { "default" },
        "settings": {
            "model": model,
            "reasoning_effort": effort,
            "developer_instructions": null,
        },
    })
}

/// What every `turn/start` of the run carries: the policy, the model and the effort. Each sticks
/// to the thread for later turns, so a queued steer's turn repeats them rather than relying on
/// that.
struct TurnParams {
    thread_id: String,
    policy: permission::Policy,
    asks_person: bool,
    model: Option<String>,
    effort: Option<&'static str>,
    /// The thread is (or was resumed) in plan mode: every turn names its mode, so that leaving
    /// plan is explicit. A thread that never planned is sent none.
    names_mode: bool,
    thread: ThreadMode,
}

impl TurnParams {
    fn new(spec: &ExecSpec, thread_id: String, thread: ThreadMode) -> Self {
        Self {
            thread_id,
            policy: permission::policy(spec.access, spec.sandbox),
            asks_person: permission::asks_person(spec.access),
            model: spec.model.clone(),
            effort: spec.effort.map(catalog::effort),
            names_mode: thread.plan,
            thread,
        }
    }

    fn build(&self, text: &str) -> Value {
        let mut p = serde_json::Map::new();
        p.insert("threadId".into(), Value::String(self.thread_id.clone()));
        p.insert("input".into(), json!([{ "type": "text", "text": text }]));
        p.insert("approvalPolicy".into(), self.policy.approval_policy.into());
        p.insert(
            "sandboxPolicy".into(),
            catalog::sandbox_policy_value(self.policy.sandbox),
        );
        if self.asks_person {
            p.insert("approvalsReviewer".into(), "user".into());
        }
        // Reasoning summaries stream (`item/reasoning/summaryTextDelta`) only when asked for;
        // without this codex "thinks" in silence for minutes.
        p.insert("summary".into(), "auto".into());
        if let Some(model) = &self.model {
            p.insert("model".into(), Value::String(model.clone()));
        }
        if let Some(effort) = self.effort {
            p.insert("effort".into(), effort.into());
        }
        if self.names_mode {
            let model = self.model.as_deref().unwrap_or(&self.thread.model);
            let effort = self.effort.or(self.thread.effort.as_deref());
            if model.is_empty() {
                tracing::warn!(
                    target: "reno_harness::codex",
                    "no model to name the collaboration mode with; leaving it unchanged"
                );
            } else {
                p.insert(
                    "collaborationMode".into(),
                    collaboration_mode(false, model, effort),
                );
            }
        }
        Value::Object(p)
    }
}

async fn start_turn(client: &RpcClient, params: Value) -> Result<String, HarnessError> {
    let started = client.request("turn/start", params).await?;
    Ok(started["turn"]["id"].as_str().unwrap_or("").to_owned())
}

/// Seconds since the Unix epoch, for the normalizer's rate-limit reset text.
fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

// ---------------------------------------------------------------------------
// Requests
// ---------------------------------------------------------------------------

/// How an answer goes back for an open approval.
#[derive(Debug)]
enum ApprovalKind {
    /// `{ decision: accept | decline }`: a command or a file change.
    Decision,
    /// A permission request, granted for the turn or refused with an empty grant; holds the
    /// request's params.
    Permissions(Value),
    /// An approval filed as an elicitation: `accept` or `decline`.
    Elicitation,
}

/// What the driver needs to answer an approval later.
#[derive(Debug)]
struct ApprovalPayload {
    rpc_id: Value,
    kind: ApprovalKind,
}

impl ApprovalPayload {
    fn reply(&self, accept: bool) -> Value {
        match &self.kind {
            ApprovalKind::Decision => permission::decision(accept),
            ApprovalKind::Permissions(params) => permission::permissions_reply(params, accept),
            ApprovalKind::Elicitation => {
                if accept {
                    elicitation::accept()
                } else {
                    elicitation::decline()
                }
            }
        }
    }
}

/// What the driver needs to answer a question later.
#[derive(Debug)]
struct InputPayload {
    rpc_id: Value,
    kind: InputKind,
}

#[derive(Debug)]
enum InputKind {
    /// `item/tool/requestUserInput`.
    Agent(Vec<Asked>),
    /// An elicitation form.
    Form(elicitation::Form),
}

impl InputPayload {
    /// The reply for `answers`; an empty list declines.
    fn reply(&self, answers: &[InputAnswer]) -> Value {
        match &self.kind {
            InputKind::Agent(asked) => permission::user_input_reply(asked, answers),
            InputKind::Form(form) => form.response(answers),
        }
    }
}

type Requests = OpenRequests<ApprovalPayload, InputPayload>;

/// The id an open request is known by to the consumer: the JSON-RPC id, unquoted.
fn request_key(id: &Value) -> String {
    match id {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A request id the app-server reused while its first request was still open: the new request
/// supersedes the old one, which it can no longer be waiting on under that id. Declining it
/// would be read as the answer to the new request.
fn note_superseded(replaced: Option<Open<ApprovalPayload, InputPayload>>, request_id: &str) {
    if replaced.is_some() {
        tracing::warn!(
            target: "reno_harness::codex",
            request_id,
            "the app-server reused the id of a request still open; the earlier one is dropped"
        );
    }
}

/// Whether every path leads inside `cwd` once symlinks are followed. A path that does not exist
/// yet (a new file, a new directory) is judged by its deepest ancestor that does, so a link
/// above it is followed too; a `..` below that ancestor, or anything that cannot be resolved,
/// is not inside (the change is asked about instead).
async fn resolves_inside(paths: &[PathBuf], cwd: &Path) -> bool {
    use std::path::Component;
    let Ok(root) = tokio::fs::canonicalize(cwd).await else {
        return false;
    };
    for path in paths {
        let mut existing = path.as_path();
        let real = loop {
            match tokio::fs::canonicalize(existing).await {
                Ok(real) => break real,
                Err(_) => match existing.parent() {
                    Some(parent) => existing = parent,
                    None => return false,
                },
            }
        };
        let Ok(tail) = path.strip_prefix(existing) else {
            return false;
        };
        if tail
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
            || !real.join(tail).starts_with(&root)
        {
            return false;
        }
    }
    true
}

/// The reply that refuses a request nobody can be asked about: after cancel, or past the cap.
/// `None` for a method this driver does not serve (answered with an error instead).
fn refusal(method: &str, params: &Value) -> Option<Value> {
    Some(match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            permission::decision(false)
        }
        "item/permissions/requestApproval" => permission::permissions_reply(params, false),
        "item/tool/requestUserInput" => json!({ "answers": {} }),
        "mcpServer/elicitation/request" => elicitation::decline(),
        _ => return None,
    })
}

/// What the loop does next.
enum Step {
    Continue,
    /// The run's `Done` went out.
    Finished,
    /// The consumer is gone.
    Gone,
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

struct Session {
    exe: PathBuf,
    /// `spec.cwd` as the wire carries it.
    cwd: String,
    spec: ExecSpec,
    authority: Authority,
    event_tx: mpsc::Sender<ExecEvent>,
    steering: mpsc::Receiver<String>,
    cancel: CancellationToken,
    answers: mpsc::Receiver<ExecAnswer>,
    processes: ProcessSink,
    interrupt_grace: Duration,
    kill_grace: Duration,
    startup_retry_delays: Vec<Duration>,
    tracker: TaskTracker,
}

/// The state of the turn loop, and the parts of it that talk to the app-server.
struct Run {
    client: RpcClient,
    event_tx: mpsc::Sender<ExecEvent>,
    authority: Authority,
    cwd: PathBuf,
    thread_id: String,
    turn_params: TurnParams,
    norm: Normalizer,
    router: TurnRouter,
    requests: Requests,
    /// Steers whose `turn/steer` lost the turn-completed race; delivered, in order, as the next
    /// `turn/start` when the expected turn's end arrives.
    queued_steers: VecDeque<String>,
    interrupted: bool,
}

impl Run {
    async fn emit(&self, event: ExecEvent) -> bool {
        self.event_tx.send(event).await.is_ok()
    }

    async fn emit_all(&self, events: Vec<ExecEvent>) -> bool {
        for event in events {
            if !self.emit(event).await {
                return false;
            }
        }
        true
    }

    /// Answer every open request with a refusal: the run is over or was cancelled, and nothing
    /// is allowed by default.
    fn deny_all(&mut self) {
        for (_, open) in self.requests.drain() {
            match open {
                Open::Approval(payload) => {
                    self.client.respond(&payload.rpc_id, payload.reply(false))
                }
                Open::Input(payload) => self.client.respond(&payload.rpc_id, payload.reply(&[])),
            }
        }
    }

    /// Start a turn with `text` (the next queued steer, or one that found no turn to join).
    async fn begin_turn(&mut self, text: &str) -> Result<(), HarnessError> {
        let id = start_turn(&self.client, self.turn_params.build(text)).await?;
        self.router.adopt_started(id);
        Ok(())
    }

    /// A steer: into the live turn, or as the next turn when there is none to join.
    async fn steer(&mut self, text: String) -> Step {
        // Later messages stay behind queued ones: they are delivered in order.
        if !self.queued_steers.is_empty() {
            self.queued_steers.push_back(text);
            return Step::Continue;
        }
        let Some(expected) = self.router.active.clone() else {
            return match self.begin_turn(&text).await {
                Ok(()) => Step::Continue,
                Err(error) => self.fail(format!("Steering failed: {error}")).await,
            };
        };
        let steer = json!({
            "threadId": self.thread_id,
            "expectedTurnId": expected,
            "clientUserMessageId": uuid::Uuid::new_v4().to_string(),
            "input": [{ "type": "text", "text": text }],
        });
        match self.client.request("turn/steer", steer).await {
            Ok(_) => Step::Continue,
            // A failed `turn/steer` does NOT mean the text is bad: most commonly the active turn
            // finished between the send and this request. Queue it for redelivery as the next
            // `turn/start` when the expected turn's end arrives (also the safe fallback for an
            // older Codex without steering).
            Err(error) => {
                tracing::debug!(
                    target: "reno_harness::codex",
                    "turn/steer rejected (queued as next turn): {error}"
                );
                if self.router.active.as_deref() == Some(expected.as_str())
                    && !self.router.is_completed(&expected)
                {
                    self.queued_steers.push_back(text);
                    Step::Continue
                } else {
                    match self.begin_turn(&text).await {
                        Ok(()) => Step::Continue,
                        Err(error) => self.fail(format!("Steering failed: {error}")).await,
                    }
                }
            }
        }
    }

    /// End the run in error.
    async fn fail(&mut self, diagnostic: String) -> Step {
        self.deny_all();
        let done = ExecEvent::Done {
            stop: StopReason::Error,
            result: None,
            diagnostic: Some(crate::redact_secrets(&diagnostic)),
        };
        if self.emit(done).await {
            Step::Finished
        } else {
            Step::Gone
        }
    }

    /// One notification from the app-server.
    async fn notification(&mut self, method: &str, params: Value) -> Step {
        // Foreign-thread traffic FIRST: a child thread's turn and thread bookkeeping must never
        // reach the turn router below (a child's turn/completed would settle the RUN's turn).
        if let Some(thread) = notification_thread_id(method, &params)
            && !thread.is_empty()
            && thread != self.thread_id
            && route_child_notification(method) == ChildRoute::Dropped
        {
            return Step::Continue;
        }
        let events = match method {
            "turn/started" => {
                self.router.note_started(turn_id(&params));
                Vec::new()
            }
            "item/agentMessage/delta" => self.norm.agent_delta(&params),
            "item/reasoning/textDelta"
            | "item/reasoning/summaryTextDelta"
            | "item/reasoning/summaryPartAdded" => self.norm.reasoning(method, &params),
            "item/started" | "item/completed" => {
                let phase = if method == "item/started" {
                    Phase::Started
                } else {
                    Phase::Completed
                };
                self.norm
                    .item(phase, params.get("item").unwrap_or(&Value::Null))
            }
            "thread/tokenUsage/updated" => {
                self.norm.token_usage(&params);
                Vec::new()
            }
            "turn/completed" | "turn/failed" | "turn/aborted" => {
                return self.settle(method, &params).await;
            }
            "error" => {
                self.norm.error(&params, unix_now());
                Vec::new()
            }
            "account/rateLimits/updated" => {
                self.norm.rate_limits(&params);
                Vec::new()
            }
            // thread/status, mcpServer startup, account noise, ...: unknown notification
            // methods are tolerated by design.
            _ => Vec::new(),
        };
        if self.emit_all(events).await {
            Step::Continue
        } else {
            Step::Gone
        }
    }

    /// A turn ended. Only the turn in flight settles; and a completed turn with a queued steer
    /// is not the run's end: the steer is the next turn.
    async fn settle(&mut self, method: &str, params: &Value) -> Step {
        let id = turn_id(params);
        if !self.router.settles(&id) {
            tracing::debug!(
                target: "reno_harness::codex",
                turn = %id,
                "stale {method} ignored (not the turn in flight)"
            );
            self.router.note_completed(&id);
            return Step::Continue;
        }
        self.router.note_completed(&id);
        let mut end: TurnEnd = self
            .norm
            .finish_turn(method, params, self.interrupted, unix_now());
        if let Some(usage) = end.usage.take()
            && !self.emit(usage).await
        {
            return Step::Gone;
        }
        if end.stop == StopReason::Completed
            && !self.interrupted
            && let Some(next) = self.queued_steers.pop_front()
        {
            return match self.begin_turn(&next).await {
                Ok(()) => Step::Continue,
                Err(error) => self.fail(format!("Steering failed: {error}")).await,
            };
        }
        self.deny_all();
        if self.emit(end.done()).await {
            Step::Finished
        } else {
            Step::Gone
        }
    }

    /// One server-to-client request. The app-server blocks until SOME response arrives, so every
    /// request is answered or recorded as open.
    ///
    /// What the run's access mode covers is accepted at once (see [`CodexExecutor`]); anything
    /// else becomes a `NeedsApproval` and waits for `ExecHandle::decide`. Questions become
    /// `NeedsInput`. A request after cancel is refused at once; one of an unknown method is
    /// answered with an error, so the server never wedges awaiting a reply.
    async fn request(&mut self, id: Value, method: &str, params: Value) -> Step {
        let key = request_key(&id);
        if self.interrupted {
            return self.refuse(&id, method, &params);
        }
        let item = params
            .get("itemId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let ask = match method {
            "item/commandExecution/requestApproval" => {
                if self.authority.accepts_command() {
                    return self.accept(&id, method, &params);
                }
                Ask::Approval(permission::command_subject(&params), ApprovalKind::Decision)
            }
            "item/fileChange/requestApproval" => {
                let change =
                    permission::file_change(&params, self.norm.file_paths(item), &self.cwd);
                // Lexically inside is not enough: a link inside the working directory can lead
                // out of it, and what is accepted unseen must stay where the run was confined.
                if self
                    .authority
                    .accepts_file_change(&change.subject.paths, change.grant_root)
                    && (!self.authority.judges_file_changes_by_path()
                        || resolves_inside(&change.subject.paths, &self.cwd).await)
                {
                    return self.accept(&id, method, &params);
                }
                Ask::Approval(change.subject, ApprovalKind::Decision)
            }
            "item/permissions/requestApproval" => {
                if self.authority.accepts_permissions() {
                    return self.accept(&id, method, &params);
                }
                Ask::Approval(
                    permission::permissions_subject(&params),
                    ApprovalKind::Permissions(params.clone()),
                )
            }
            "mcpServer/elicitation/request" => {
                match permission::classify_elicitation(&key, &params) {
                    Elicitation::Decline => {
                        tracing::info!(
                            target: "reno_harness::codex",
                            mode = params["mode"].as_str().unwrap_or_default(),
                            "declining an MCP elicitation Reno can't ask"
                        );
                        self.client.respond(&id, elicitation::decline());
                        return Step::Continue;
                    }
                    Elicitation::Form(form) => Ask::Form(form),
                    Elicitation::Approval(subject) => {
                        if subject.action == reno_proto::ActionClass::McpTool
                            && self.authority.accepts_mcp_tool()
                        {
                            self.client.respond(&id, elicitation::accept());
                            return Step::Continue;
                        }
                        Ask::Approval(subject, ApprovalKind::Elicitation)
                    }
                }
            }
            "item/tool/requestUserInput" => {
                let asked = permission::user_input_questions(&key, &params);
                if asked.is_empty() {
                    self.client.respond(&id, json!({ "answers": {} }));
                    return Step::Continue;
                }
                Ask::Agent(asked)
            }
            _ => {
                tracing::debug!(
                    target: "reno_harness::codex",
                    "unhandled server request: {method}"
                );
                self.client
                    .respond_error(&id, -32601, &format!("unsupported method: {method}"));
                return Step::Continue;
            }
        };
        if self.requests.len() >= MAX_OPEN_REQUESTS {
            tracing::warn!(
                target: "reno_harness::codex",
                "too many open requests; refusing {method}"
            );
            self.client
                .respond_error(&id, -32000, "client overloaded: too many pending requests");
            return Step::Continue;
        }
        let event = match ask {
            Ask::Approval(subject, kind) => {
                let replaced = self
                    .requests
                    .open_approval(key.clone(), ApprovalPayload { rpc_id: id, kind });
                note_superseded(replaced, &key);
                ExecEvent::NeedsApproval {
                    request_id: key,
                    subject,
                }
            }
            Ask::Form(form) => {
                let questions = form.questions.clone();
                let replaced = self.requests.open_input(
                    key.clone(),
                    InputPayload {
                        rpc_id: id,
                        kind: InputKind::Form(form),
                    },
                );
                note_superseded(replaced, &key);
                ExecEvent::NeedsInput {
                    request_id: key,
                    questions,
                }
            }
            Ask::Agent(asked) => {
                let questions = asked.iter().map(|a| a.question.clone()).collect();
                let replaced = self.requests.open_input(
                    key.clone(),
                    InputPayload {
                        rpc_id: id,
                        kind: InputKind::Agent(asked),
                    },
                );
                note_superseded(replaced, &key);
                ExecEvent::NeedsInput {
                    request_id: key,
                    questions,
                }
            }
        };
        if self.emit(event).await {
            Step::Continue
        } else {
            Step::Gone
        }
    }

    /// Accept an approval the run's access mode covers.
    fn accept(&self, id: &Value, method: &str, params: &Value) -> Step {
        let reply = match method {
            "item/permissions/requestApproval" => permission::permissions_reply(params, true),
            _ => permission::decision(true),
        };
        self.client.respond(id, reply);
        Step::Continue
    }

    fn refuse(&self, id: &Value, method: &str, params: &Value) -> Step {
        match refusal(method, params) {
            Some(reply) => self.client.respond(id, reply),
            None => self
                .client
                .respond_error(id, -32601, &format!("unsupported method: {method}")),
        }
        Step::Continue
    }

    /// Apply one answer from the consumer.
    fn answer(&mut self, answer: ExecAnswer) {
        let request_id = answer.request_id.clone();
        match self.requests.resolve(answer) {
            Resolved::Decision(payload, decision, message) => {
                // The app-server's approval reply is `{decision}` and nothing else: it has no
                // field for a reason, so a rule's or a person's message cannot reach the model. It is
                // logged so the refusal is explainable from the daemon's side.
                if let Some(message) = message.as_deref().filter(|_| decision == Decision::Deny) {
                    tracing::info!(
                        target: "reno_harness::codex",
                        request_id,
                        message,
                        "declined with a reason; the app-server reply cannot carry the message"
                    );
                }
                self.client
                    .respond(&payload.rpc_id, payload.reply(decision == Decision::Allow));
            }
            Resolved::Input(payload, given) => {
                self.client.respond(&payload.rpc_id, payload.reply(&given))
            }
            Resolved::WrongKind => tracing::warn!(
                target: "reno_harness::codex",
                request_id,
                "an answer of the wrong kind for this request is ignored; it stays open"
            ),
            Resolved::Unknown => tracing::warn!(
                target: "reno_harness::codex",
                request_id,
                "answer for an unknown or already resolved request"
            ),
        }
    }
}

/// What a request becomes.
enum Ask {
    Approval(ApprovalSubject, ApprovalKind),
    Form(elicitation::Form),
    Agent(Vec<Asked>),
}

/// A `Done { Error }` and the teardown of a run that never got a thread.
async fn end_early(
    session: &Session,
    stop: StopReason,
    diagnostic: Option<String>,
    app: &mut AppServer,
) {
    let _ = session
        .event_tx
        .send(ExecEvent::Done {
            stop,
            result: None,
            diagnostic,
        })
        .await;
    app.child.shutdown(session.kill_grace).await;
    app.release();
}

/// The handshake, interruptible, relaunching an app-server that died first. `Err` carries the
/// diagnostic of the `Done` that ends the run, or `None` when the run was cancelled.
async fn handshake(session: &Session, app: &mut AppServer) -> Result<(), Option<String>> {
    let mut attempt = 0;
    let mut opt_out = true;
    loop {
        let handshake = app.client.request("initialize", initialize_params(opt_out));
        let failed = tokio::select! {
            res = handshake => match res {
                Ok(_) => return Ok(()),
                Err(e) => e,
            },
            () = session.cancel.cancelled() => return Err(None),
        };
        // Only a process that is gone gets another launch: an answer that refuses (bad config,
        // unsupported version) would refuse again.
        let exited = app.client.is_closed();
        // An app-server that predates notification opt-outs may reject the capability
        // outright; the opt-out is an optimization, so ask again without it.
        if !exited && std::mem::take(&mut opt_out) {
            tracing::debug!(
                target: "reno_harness::codex",
                "initialize refused ({failed}); retrying without notification opt-outs"
            );
            continue;
        }
        if exited && attempt < session.startup_retry_delays.len() {
            let status = app.child.try_wait().ok().flatten();
            tracing::warn!(
                target: "reno_harness::codex",
                attempt = attempt + 1,
                error = %crate::crash_message("codex app-server", status, &app.stderr_tail),
                "app-server died before initialize; relaunching"
            );
            app.child.shutdown(session.kill_grace).await;
            app.release();
            tokio::select! {
                () = tokio::time::sleep(session.startup_retry_delays[attempt]) => {}
                () = session.cancel.cancelled() => return Err(None),
            }
            attempt += 1;
            // The relaunch is a child the orphan sweep must know about, like the first.
            match launch(
                &session.tracker,
                &session.exe,
                &session.spec,
                &session.processes,
            )
            .await
            {
                Ok(next) => {
                    *app = next;
                    continue;
                }
                Err(error) => return Err(Some(error.to_string())),
            }
        }
        // Out of launches: say why, with what the process printed.
        return Err(Some(if exited {
            app.stderr_tail.wait_closed().await;
            format!(
                "initialize: {}",
                crate::crash_message(
                    "codex app-server",
                    app.child.try_wait().ok().flatten(),
                    &app.stderr_tail
                )
            )
        } else {
            failed.to_string()
        }));
    }
}

/// What `thread/start` and `thread/resume` carry besides the thread id. The bot's `instructions`
/// go as `developerInstructions`: extra developer-role guidance for the thread, which leaves
/// Codex's own base instructions in place (so it is the closest to Claude's appended system
/// prompt). The field is in the Codex 0.169 app-server schema for both `ThreadStartParams` and
/// `ThreadResumeParams`.
fn thread_params(
    spec: &ExecSpec,
    cwd: &str,
    policy: permission::Policy,
) -> serde_json::Map<String, Value> {
    let mut p = serde_json::Map::new();
    if !cwd.is_empty() {
        p.insert("cwd".into(), Value::String(cwd.to_owned()));
    }
    let overrides = mcp_overrides(&spec.mcp, spec.access == AccessMode::FullAccess);
    if !overrides.is_empty() {
        // Reno's own MCP server and the plugins' as dotted config overrides on top of the user's
        // `mcp_servers` table.
        p.insert(
            "config".into(),
            Value::Object(overrides.into_iter().collect()),
        );
    }
    p.insert("approvalPolicy".into(), policy.approval_policy.into());
    p.insert(
        "sandbox".into(),
        catalog::sandbox_mode(policy.sandbox).into(),
    );
    // A mode that asks routes its approvals to the person explicitly: a configured
    // `approvals_reviewer` could otherwise hand them to an AI reviewer instead.
    if permission::asks_person(spec.access) {
        p.insert("approvalsReviewer".into(), "user".into());
    }
    if let Some(model) = &spec.model {
        p.insert("model".into(), Value::String(model.clone()));
    }
    if let Some(instructions) = &spec.instructions {
        p.insert(
            "developerInstructions".into(),
            Value::String(instructions.clone()),
        );
    }
    p
}

/// `thread/start` or `thread/resume`: the thread and what it runs with.
async fn open_thread(
    client: &RpcClient,
    spec: &ExecSpec,
    cwd: &str,
    policy: permission::Policy,
) -> Result<(String, ThreadMode), HarnessError> {
    let p = thread_params(spec, cwd, policy);
    let thread = if let Some(resume) = &spec.resume {
        let mut resume_params = p.clone();
        resume_params.insert("threadId".into(), Value::String(resume.0.clone()));
        match resume_thread(client, Value::Object(resume_params))
            .await
            .map_err(|e| (resume_unrecoverable(&e), e))
        {
            Ok(thread) => thread,
            // A missing, unreadable or foreign rollout falls back to a fresh thread; `Started`
            // names the thread the run really has. The log keeps Codex's own words.
            Err((Some(reason), e)) => {
                tracing::warn!(
                    target: "reno_harness::codex",
                    "thread/resume failed (starting fresh): {e}"
                );
                tracing::warn!(
                    target: "reno_harness::codex",
                    "Couldn't restore the earlier Codex conversation ({reason}), so this run \
                     starts a new one without its context."
                );
                client
                    .request("thread/start", Value::Object(p.clone()))
                    .await?
            }
            // A dead process or a refusal a fresh thread would hit too: fail the run rather
            // than silently dropping its history.
            Err((None, e)) => return Err(e),
        }
    } else {
        client.request("thread/start", Value::Object(p)).await?
    };
    let thread_id = thread["thread"]["id"].as_str().unwrap_or("").to_owned();
    let mode = ThreadMode {
        model: thread["model"].as_str().unwrap_or_default().to_owned(),
        effort: thread["reasoningEffort"].as_str().map(str::to_owned),
        plan: thread.pointer("/collaborationMode/mode") == Some(&json!("plan")),
    };
    Ok((thread_id, mode))
}

/// The per-run event loop: one task multiplexing app-server messages, the steering queue, the
/// answers, the cancel token and consumer liveness.
async fn run_session(mut session: Session, mut app: AppServer) {
    // ---- handshake (interruptible; relaunches a process that died first) ----
    if let Err(why) = handshake(&session, &mut app).await {
        let (stop, diagnostic) = match why {
            None => (StopReason::Interrupted, None),
            Some(why) => (StopReason::Error, Some(crate::redact_secrets(&why))),
        };
        end_early(&session, stop, diagnostic, &mut app).await;
        return;
    }
    app.client.notify("initialized", None);

    // ---- thread (interruptible) ----------------------------------------------
    let policy = permission::policy(session.spec.access, session.spec.sandbox);
    let opened = tokio::select! {
        res = open_thread(&app.client, &session.spec, &session.cwd, policy) => res,
        () = session.cancel.cancelled() => {
            end_early(&session, StopReason::Interrupted, None, &mut app).await;
            return;
        }
    };
    let (thread_id, thread_mode) = match opened {
        Ok(opened) => opened,
        Err(e) => {
            let error = setup_failure(e, &mut app).await;
            end_early(
                &session,
                StopReason::Error,
                Some(crate::redact_secrets(&error)),
                &mut app,
            )
            .await;
            return;
        }
    };

    let started = ExecEvent::Started {
        session: SessionRef(thread_id.clone()),
        model: session
            .spec
            .model
            .clone()
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| thread_mode.model.clone()),
    };
    if session.event_tx.send(started).await.is_err() {
        end_early_silent(&session, &mut app).await;
        return;
    }

    let mut run = Run {
        client: app.client.clone(),
        event_tx: session.event_tx.clone(),
        authority: session.authority.clone(),
        cwd: session.spec.cwd.clone(),
        turn_params: TurnParams::new(&session.spec, thread_id.clone(), thread_mode),
        thread_id,
        norm: Normalizer::new(),
        router: TurnRouter::default(),
        requests: Requests::new(),
        queued_steers: VecDeque::new(),
        interrupted: false,
    };

    // ---- the turn --------------------------------------------------------------
    let mut ended = false;
    let mut escalation: Option<tokio::task::JoinHandle<()>> = None;
    // The runtime's process tree when its teardown began (see [`crate::shutdown_agent`]).
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut torn_down: Vec<crate::process::Tracked> = Vec::new();
    let prompt = session.spec.prompt.clone();
    // A run cancelled before its turn is acknowledged has no turn to interrupt: it ends at once
    // (the bookkeeping below says `Interrupted`, and teardown reaps the app-server).
    let mut cancelled = false;
    let started = tokio::select! {
        started = run.begin_turn(&prompt) => Some(started),
        () = session.cancel.cancelled() => None,
    };
    match started {
        Some(Ok(())) => {}
        Some(Err(e)) => {
            let error = setup_failure(e, &mut app).await;
            ended = matches!(run.fail(error).await, Step::Finished);
        }
        None => {
            cancelled = true;
            run.interrupted = true;
        }
    }

    let mut steering_open = true;
    let mut answers_open = true;
    let mut interrupt_sent = false;
    'main: while !ended && !cancelled {
        let step = tokio::select! {
            inc = app.incoming.recv() => match inc {
                Some(Incoming::Notification { method, params }) => {
                    run.notification(&method, params).await
                }
                Some(Incoming::Request { id, method, params }) => {
                    run.request(id, &method, params).await
                }
                // stdout EOF or reader gone: the app server exited.
                Some(Incoming::Eof) | None => break 'main,
            },

            prompt = session.steering.recv(), if steering_open && !run.interrupted => match prompt {
                // A steer waiting on a wedged app-server must not hold off a cancel: dropped
                // here, the cancelled token fires the arm below on the next turn of the loop.
                Some(prompt) => tokio::select! {
                    step = run.steer(prompt) => step,
                    () = session.cancel.cancelled() => Step::Continue,
                },
                // The queue closed. The run goes on: approvals and answers still need it, and it
                // ends with its `Done`.
                None => {
                    steering_open = false;
                    Step::Continue
                }
            },

            answer = session.answers.recv(), if answers_open => match answer {
                Some(answer) => {
                    run.answer(answer);
                    Step::Continue
                }
                None => {
                    answers_open = false;
                    Step::Continue
                }
            },

            () = session.cancel.cancelled(), if !interrupt_sent => {
                interrupt_sent = true;
                run.interrupted = true;
                // A request still open when the run is cancelled is refused, before anything
                // else reaches the app-server.
                run.deny_all();
                // The runtime is going: so is everything it started.
                #[cfg(unix)]
                if let Some(pid) = app.child.id() {
                    torn_down = crate::process::descendants(pid).await;
                }
                match run.router.active.clone() {
                    Some(turn) => {
                        let client = app.client.clone();
                        let thread = run.thread_id.clone();
                        session.tracker.spawn(async move {
                            if let Err(e) = client
                                .request("turn/interrupt", json!({ "threadId": thread, "turnId": turn }))
                                .await
                            {
                                tracing::debug!(
                                    target: "reno_harness::codex",
                                    "turn/interrupt failed (escalation will reap): {e}"
                                );
                            }
                        });
                        // Escalate if the app server doesn't wind down (turn/aborted) within the
                        // grace periods: SIGTERM, then SIGKILL. Aborted once the child is reaped.
                        if let Some(pid) = crate::process::signal_target(&app.child) {
                            let (interrupt_grace, kill_grace) =
                                (session.interrupt_grace, session.kill_grace);
                            escalation = Some(session.tracker.spawn(async move {
                                tokio::time::sleep(interrupt_grace).await;
                                send_signal(&pid, Signal::Term);
                                tokio::time::sleep(kill_grace).await;
                                send_signal(&pid, Signal::Kill);
                            }));
                        }
                        Step::Continue
                    }
                    // No turn is known to interrupt: the terminal bookkeeping below still
                    // guarantees Done { Interrupted }.
                    None => break 'main,
                }
            },

            () = run.event_tx.closed() => break 'main,
        };
        match step {
            Step::Continue => {}
            Step::Finished => ended = true,
            Step::Gone => break 'main,
        }
    }

    // Terminal bookkeeping: never end the stream without a Done unless the consumer already hung
    // up.
    if !run.event_tx.is_closed() && !ended {
        let done = if run.interrupted {
            ExecEvent::Done {
                stop: StopReason::Interrupted,
                result: None,
                diagnostic: None,
            }
        } else {
            // The app-server died mid-turn (OS memory pressure, `killall codex`): never a
            // silent success. A final error the turn already reported (and its end never came
            // to carry) leads: it is usually why the turn went nowhere.
            let crash = crash_report(&mut app).await;
            let diagnostic = match run.norm.take_held_error() {
                Some(held) => format!("{held}\n\n{crash}"),
                None => crash,
            };
            ExecEvent::Done {
                stop: StopReason::Error,
                result: None,
                diagnostic: Some(crate::redact_secrets(&diagnostic)),
            }
        };
        let _ = run.event_tx.send(done).await;
    }

    // Whatever is still open is refused: the run is over.
    run.deny_all();

    // Its whole process group goes with it, and whatever was traced before, on every exit path.
    crate::shutdown_agent(&mut app.child, torn_down, session.kill_grace).await;
    if let Some(handle) = escalation {
        handle.abort();
    }
    app.release();
}

/// Tear down a run whose consumer left before it began.
async fn end_early_silent(session: &Session, app: &mut AppServer) {
    app.child.shutdown(session.kill_grace).await;
    app.release();
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use reno_proto::{Effort, RunId};

    fn spec(access: AccessMode, sandbox: SandboxLevel) -> ExecSpec {
        ExecSpec {
            run_id: RunId::from_suffix("01HZX9").unwrap(),
            prompt: "do it".into(),
            cwd: PathBuf::from("/work"),
            model: Some("gpt-5.6-sol".into()),
            effort: Some(Effort::High),
            access,
            sandbox,
            resume: None,
            mcp: Vec::new(),
            env: Default::default(),
            output_schema: None,
            instructions: None,
        }
    }

    fn server(name: &str, command: &str) -> McpServerSpec {
        McpServerSpec {
            name: name.into(),
            command: command.into(),
            args: vec!["mcp".into()],
            env: [("RENO_RUN".to_owned(), "r1".to_owned())].into(),
        }
    }

    #[test]
    fn a_bots_instructions_ride_the_thread_as_developer_instructions() {
        let mut spec = spec(AccessMode::Supervised, SandboxLevel::Unrestricted);
        let policy = permission::policy(spec.access, spec.sandbox);
        assert!(!thread_params(&spec, "/work", policy).contains_key("developerInstructions"));
        spec.instructions = Some("Be terse.".into());
        let policy = permission::policy(spec.access, spec.sandbox);
        let params = thread_params(&spec, "/work", policy);
        assert_eq!(params["developerInstructions"], "Be terse.");
        // The base instructions stay Codex's own.
        assert!(!params.contains_key("baseInstructions"));
    }

    #[test]
    fn mcp_overrides_use_the_dotted_mcp_servers_keys() {
        let overrides: serde_json::Map<String, Value> = mcp_overrides(
            &[
                server("reno", "/opt/reno/reno"),
                server("plugin-focus", "/p/server"),
            ],
            true,
        )
        .into_iter()
        .collect();
        assert_eq!(overrides["mcp_servers.reno.command"], "/opt/reno/reno");
        assert_eq!(overrides["mcp_servers.reno.args"], json!(["mcp"]));
        assert_eq!(
            overrides["mcp_servers.reno.env"],
            json!({ "RENO_RUN": "r1" })
        );
        // Long Reno waits must outlive Codex's 60 s default tool timeout.
        assert!(
            overrides["mcp_servers.reno.tool_timeout_sec"]
                .as_u64()
                .unwrap()
                > 3600
        );
        assert_eq!(overrides["mcp_servers.reno.startup_timeout_sec"], 30);
        assert_eq!(
            overrides["mcp_servers.reno.default_tools_approval_mode"],
            "approve"
        );
        // A plugin's server: the three launch keys, no timeouts, never pre-approved.
        assert_eq!(overrides["mcp_servers.plugin-focus.command"], "/p/server");
        let plugin_keys = overrides
            .keys()
            .filter(|k| k.starts_with("mcp_servers.plugin-focus."))
            .count();
        assert_eq!(plugin_keys, 3, "{overrides:?}");
    }

    #[test]
    fn reno_s_server_is_not_pre_approved_when_someone_is_asked() {
        let overrides = mcp_overrides(&[server("reno", "/r")], false);
        assert!(
            !overrides
                .iter()
                .any(|(k, _)| k.ends_with("default_tools_approval_mode")),
            "{overrides:?}"
        );
        assert!(mcp_overrides(&[], true).is_empty());
    }

    #[test]
    fn reno_wins_a_name_clash_because_it_is_first() {
        let overrides: serde_json::Map<String, Value> = mcp_overrides(
            &[server("reno", "/first"), server("reno", "/second")],
            false,
        )
        .into_iter()
        .collect();
        assert_eq!(overrides["mcp_servers.reno.command"], "/first");
    }

    #[test]
    fn only_a_known_old_cli_is_turned_away() {
        let v = |s: &str| semver::Version::parse(s).unwrap();
        assert!(too_old_message(None).is_none());
        assert!(too_old_message(Some(&v("0.169.0"))).is_none());
        assert!(too_old_message(Some(&v(MIN_CLI_VERSION))).is_none());
        // A pre-release of the minimum is that release, not an older one.
        assert!(too_old_message(Some(&v("0.146.0-alpha.2"))).is_none());
        assert!(too_old_message(Some(&v("0.145.0-alpha.9"))).is_some());
        let message = too_old_message(Some(&v("0.120.3"))).unwrap();
        assert!(
            message.contains("Codex CLI 0.120.3 is too old") && message.contains("0.146.0"),
            "{message}"
        );
        assert!(v(MIN_CLI_VERSION) <= v(VERIFIED_CLI_VERSION));
    }

    #[test]
    fn only_a_missing_conversation_starts_fresh() {
        let protocol = |m: &str| HarnessError::Protocol(m.into());
        // Live 0.164.0 wordings, as the RPC client renders them.
        for (gone, reason) in [
            (
                "thread/resume: no rollout found for thread id 019a0000-0000-7000-8000-000000000000 (code -32600)",
                "it isn't saved on this device",
            ),
            (
                "thread/resume: thread not found: abc",
                "it isn't saved on this device",
            ),
            (
                "thread/resume: failed to read thread: thread-store internal error: failed to read session metadata /h/sessions/2026/10/23/rollout-x.jsonl: rollout at /h/sessions/2026/10/23/rollout-x.jsonl is empty (code -32603)",
                "its saved history is empty",
            ),
            (
                "thread/resume: failed to read thread: thread-store internal error: failed to read session metadata /h/sessions/2026/10/23/rollout-x.jsonl: stream did not contain valid UTF-8 (code -32603)",
                "its saved history couldn't be read",
            ),
            (
                "thread/resume: invalid session id: invalid character: expected an optional prefix of `urn:uuid:` (code -32600)",
                "Codex doesn't recognize its id",
            ),
        ] {
            assert_eq!(
                resume_unrecoverable(&protocol(gone)),
                Some(reason),
                "{gone}"
            );
        }
        for keep in [
            "thread/resume: app-server exited before responding",
            "thread/resume: app-server stdin closed",
            "thread/resume: failed to load configuration",
            "thread/resume: session x is archived. Run `codex unarchive x` to unarchive it first.",
            // "not found" alone is about something other than the thread.
            "thread/resume: failed to open /repo/AGENTS.md: entity not found (code -32603)",
            "thread/resume: environment variable not found: MY_PROVIDER_KEY (code -32603)",
            "thread/resume: model provider `corp` not found (code -32600)",
            // A rollout the OS would not hand over still exists.
            "thread/resume: failed to read thread: thread-store internal error: failed to read session metadata /h/sessions/2026/10/23/rollout-x.jsonl: Permission denied (os error 13) (code -32603)",
            "thread/resume: failed to read thread: thread-store internal error: rollout at /h/sessions/2026/10/23/rollout-x.jsonl is locked (code -32603)",
            "thread/resume: failed to read thread: thread-store internal error: Too many open files (os error 24) (code -32603)",
            "thread/resume: failed to read thread: thread-store internal error: something new (code -32603)",
        ] {
            assert_eq!(resume_unrecoverable(&protocol(keep)), None, "{keep}");
        }
        assert!(transport_lost(&protocol(
            "thread/start: app-server exited before responding"
        )));
        // A failed write names its cause after the wire's own words.
        assert!(transport_lost(&protocol(
            "thread/resume: app-server stdin closed (Broken pipe (os error 32))"
        )));
        assert_eq!(
            resume_unrecoverable(&protocol(
                "thread/resume: app-server stdin closed (Broken pipe (os error 32))"
            )),
            None
        );
        assert!(!transport_lost(&protocol("thread/start: bad request")));
    }

    #[test]
    fn turn_router_never_revives_completed_turns() {
        let mut r = TurnRouter::default();
        r.note_completed("t-1");
        // The turn/start response arriving after turn/completed must not resurrect the turn.
        r.adopt_started("t-1".into());
        assert_eq!(r.active, None);
        // Nor may a late turn/started notification.
        r.note_started("t-1".into());
        assert_eq!(r.active, None);

        r.note_started("t-2".into());
        assert_eq!(r.active.as_deref(), Some("t-2"));
        // A replacement started turn retires the stale one.
        r.note_started("t-3".into());
        assert_eq!(r.active.as_deref(), Some("t-3"));
        assert!(r.is_completed("t-2"));
        // Only the turn in flight settles.
        assert!(r.settles("t-3") && r.settles(""));
        assert!(!r.settles("t-2") && !r.settles("t-9"));
        // Bounded.
        for at in 0..100 {
            r.note_completed(&format!("old-{at}"));
        }
        assert!(r.completed.len() <= 32);
    }

    #[test]
    fn a_turn_names_its_policy_model_and_effort() {
        let mode = ThreadMode {
            model: "gpt-resumed".into(),
            effort: Some("medium".into()),
            plan: false,
        };
        let params = TurnParams::new(
            &spec(AccessMode::Supervised, SandboxLevel::WorkspaceWrite),
            "th-1".into(),
            mode,
        )
        .build("hello");
        assert_eq!(params["threadId"], "th-1");
        assert_eq!(params["input"], json!([{"type": "text", "text": "hello"}]));
        assert_eq!(params["approvalPolicy"], "untrusted");
        assert_eq!(params["sandboxPolicy"], json!({"type": "readOnly"}));
        assert_eq!(params["approvalsReviewer"], "user");
        assert_eq!(params["summary"], "auto");
        assert_eq!(params["model"], "gpt-5.6-sol");
        assert_eq!(params["effort"], "high");
        // A thread that never planned is sent no mode.
        assert!(params.get("collaborationMode").is_none());

        let full = TurnParams::new(
            &spec(AccessMode::FullAccess, SandboxLevel::Unrestricted),
            "th-1".into(),
            ThreadMode {
                model: String::new(),
                effort: None,
                plan: false,
            },
        )
        .build("x");
        assert_eq!(full["approvalPolicy"], "never");
        assert_eq!(full["sandboxPolicy"], json!({"type": "dangerFullAccess"}));
        assert!(full.get("approvalsReviewer").is_none());
    }

    #[test]
    fn a_thread_left_in_plan_mode_is_named_default_with_the_specs_model_else_the_threads() {
        let plan = |model: Option<&str>, effort: Option<Effort>| {
            let mut s = spec(AccessMode::FullAccess, SandboxLevel::Unrestricted);
            s.model = model.map(str::to_owned);
            s.effort = effort;
            TurnParams::new(
                &s,
                "th-1".into(),
                ThreadMode {
                    model: "gpt-resumed".into(),
                    effort: Some("medium".into()),
                    plan: true,
                },
            )
            .build("x")
        };
        assert_eq!(
            plan(None, None)["collaborationMode"],
            collaboration_mode(false, "gpt-resumed", Some("medium"))
        );
        assert_eq!(
            plan(Some("gpt-x"), Some(Effort::Low))["collaborationMode"],
            collaboration_mode(false, "gpt-x", Some("low"))
        );
        assert_eq!(
            collaboration_mode(false, "m", None),
            json!({"mode": "default", "settings": {
                "model": "m", "reasoning_effort": null, "developer_instructions": null,
            }})
        );
    }

    #[test]
    fn a_request_is_known_by_its_json_rpc_id() {
        assert_eq!(request_key(&json!(101)), "101");
        assert_eq!(request_key(&json!("abc")), "abc");
    }

    #[test]
    fn a_refusal_exists_for_every_request_the_driver_serves() {
        let params = json!({"permissions": {"network": {"enabled": true}}});
        for method in [
            "item/commandExecution/requestApproval",
            "item/fileChange/requestApproval",
        ] {
            assert_eq!(
                refusal(method, &params),
                Some(json!({"decision": "decline"}))
            );
        }
        assert_eq!(
            refusal("item/permissions/requestApproval", &params),
            Some(json!({"permissions": {}, "scope": "turn"}))
        );
        assert_eq!(
            refusal("item/tool/requestUserInput", &params),
            Some(json!({"answers": {}}))
        );
        assert_eq!(
            refusal("mcpServer/elicitation/request", &params),
            Some(json!({"action": "decline"}))
        );
        assert_eq!(refusal("brand/newRequest", &params), None);
    }

    #[test]
    fn the_handshake_opts_out_of_what_nothing_consumes() {
        let with = initialize_params(true);
        assert_eq!(with["clientInfo"]["name"], "reno-native");
        assert_eq!(with["capabilities"]["experimentalApi"], true);
        assert_eq!(
            with["capabilities"]["optOutNotificationMethods"],
            json!(["turn/diff/updated", "item/fileChange/outputDelta"])
        );
        assert!(
            initialize_params(false)["capabilities"]
                .get("optOutNotificationMethods")
                .is_none()
        );
    }

    #[test]
    fn the_descriptor_lists_only_what_the_instance_will_honour() {
        let restricted = CodexExecutor::new().with_access_modes(vec![AccessMode::Supervised]);
        assert_eq!(
            restricted.descriptor().caps.access_modes,
            vec![AccessMode::Supervised]
        );
        let cli = CodexExecutor::new().descriptor().cli_verified;
        assert_eq!(
            (cli.min.as_str(), cli.verified.as_str()),
            ("0.146.0", "0.169.0")
        );
    }
}
