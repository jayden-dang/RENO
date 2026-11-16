//! The native Claude Code executor: spawns the installed `claude` CLI and speaks its
//! stream-json protocol directly, with no adapter process in between, and implements
//! [`Executor`].
//!
//! - stdout JSONL frames are normalized into [`ExecEvent`]s by `wire::claude` (init dedupe,
//!   subagent exclusion, error-code mapping). This module only does the I/O.
//! - PERMISSIONS ride the stdio control channel: `--permission-prompt-tool stdio` (undocumented,
//!   absent from `claude --help`, but the transport the Claude Agent SDK's `query()` drives;
//!   re-validated live against 2.1.228). A `can_use_tool` request carries the tool's name and
//!   input and becomes `NeedsApproval` with a structured [`reno_proto::ApprovalSubject`]; the
//!   decision arrives through `ExecHandle::decide` and goes back as an allow or deny control
//!   response. `AskUserQuestion` is a genuine question: `NeedsInput`, answered through
//!   `ExecHandle::answer_input`. No prose is written or parsed on this path.
//! - ONE RUN IS ONE `Done`. The CLI's session outlives a turn (it wakes itself when a
//!   background task ends); the run does not. The first released `result` frame ends it and
//!   the CLI's process group is torn down, so what the agent left running in the background
//!   ends with the run.
//! - Steering: queued prompts are written to stdin as user lines at any time; the CLI folds
//!   them into the running turn at its own step boundary. Every stdin user line carries a uuid,
//!   and the CLI reports each one's `command_lifecycle`. A `result` while one of ours is still
//!   written or queued is a boundary (a `now` steer aborting the turn, resume settling dead
//!   background tasks), not the run's end: that message's turn ends with its own result. See
//!   [`Commands`].
//! - Cancel: cancelling `ExecControls::cancel` denies every open request, sends the
//!   `interrupt` control request, then SIGTERM and SIGKILL after their graces. The stream ends
//!   with `Done { stop: Interrupted }`.
//! - Resume: `ExecSpec::resume` becomes `--resume=<session>`. A session whose transcript holds
//!   no conversation, or that the CLI cannot restore, starts a fresh one; the `Started` event
//!   names whichever session the run really has.
//! - ISOLATION FROM THE OWNER'S CONFIG (`ExecSpec::inherit_user_config`, off by default): the run
//!   passes `--setting-sources project,local` and sets `CLAUDE_CODE_DISABLE_AUTO_MEMORY=1` and
//!   `ENABLE_CLAUDEAI_MCP_SERVERS=false` (see [`ISOLATION_ENV`]). Per the Claude Code docs
//!   (`--setting-sources` in the CLI reference; "What settingSources does not control" in the
//!   Agent SDK guide):
//!   - EXCLUDED: user settings (`~/.claude/settings.json`: user hooks, `enabledPlugins`,
//!     permissions, `outputStyle`, `env`, ...) and `~/.claude/CLAUDE.md` (loaded by the `user`
//!     source); auto-memory (`~/.claude/projects/<project>/memory/`, which the sources do NOT
//!     control, hence the first variable); claude.ai MCP connectors (second variable).
//!   - STILL LOADED: project and local settings, the project's `CLAUDE.md` and `.mcp.json`,
//!     Reno's injected MCP server and policy (never `--strict-mcp-config`, which would drop the
//!     project's own servers); managed policy settings (MDM / managed file / server-managed);
//!     `~/.claude.json` (global config, always read); `sandbox.credentials` deny entries from
//!     `~/.claude/settings.json`. User-scope MCP servers kept in `~/.claude.json` are not
//!     documented as following the setting sources, so they may still load.
//!   - SIGN-IN: credentials are not settings. The docs describe `--setting-sources` as a settings
//!     filter only and list `~/.claude.json` and the credential environment as read regardless;
//!     the docs do not state authentication independence in so many words, so the first live
//!     run is the confirmation. The flag is documented for the CLI but its introduction version
//!     is not stated; it is assumed to predate [`MIN_CLI_VERSION`].
//! - Process records: every child this module spawns is reported through
//!   `ExecControls::processes` as soon as it exists, so a crash of the daemon leaves a row for
//!   the orphan sweep (ARCHITECTURE 17.3).

pub(crate) mod catalog;
mod health;
pub mod refresh_lock;
mod resume;

use crate::wire::claude::normalize::Normalizer;
use crate::wire::claude::{permission, wire};

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use reno_proto::{
    AccessMode, Effort, ExecEvent, ExecSpec, ExecutorCaps, ExecutorDescriptor, ExecutorId,
    HealthProbe, McpServerSpec, ModelCatalog, SandboxLevel, SignedIn, Steering, StopReason,
    VersionGate, VersionRange,
};
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::exec::{
    ExecAnswer, ExecControls, ExecError, Executor, OpenRequests, Resolved, UnknownFrames, probe,
    require_access, require_sandbox,
};
use crate::process::owned::Child;
use crate::process::{ChildStdin, Command, Stdio};
use crate::{Signal, send_signal};
use resume::Transcript;
use wire::{ControlRequestFrame, Frame, control_error_line, control_response_line};

/// The oldest Claude Code CLI this driver claims to work with: this is the oldest version its protocol code was live-validated against: the stdio
/// permission channel and the stream-json frames, 2.1.228, where the `live-2.1.228-*.jsonl`
/// fixtures were captured.
pub const MIN_CLI_VERSION: &str = "2.1.228";

/// The newest CLI version this driver targets: the one installed where the native contract was
/// written. The driver's fixtures and the fake CLI follow the 2.1.x wire; no live run against
/// this version has been made, so it is a claim to confirm with the live tests (`--ignored`).
pub const VERIFIED_CLI_VERSION: &str = "2.1.325";

/// The executor contract version this driver implements (`ExecutorDescriptor::contract_version`).
const CONTRACT_VERSION: u32 = 1;
/// The sandbox levels this driver honours; `start` refuses any other (ARCH-15).
const SANDBOX_LEVELS: [SandboxLevel; 1] = [SandboxLevel::Unrestricted];

/// The id this executor registers under.
pub const EXECUTOR_ID: &str = "claude-code";

static ID: LazyLock<ExecutorId> = LazyLock::new(|| match EXECUTOR_ID.parse() {
    Ok(id) => id,
    Err(error) => unreachable!("{EXECUTOR_ID} is a valid executor id: {error}"),
});

/// What the stream carries before the session loop starts: room for a burst of frames.
const EVENT_CAPACITY: usize = 256;

/// The CLI's answer to `--resume` with a session it has no conversation for (it exits right
/// after).
fn no_conversation(text: &str) -> bool {
    text.contains("No conversation found")
}

/// Why a run started without the session it asked to resume.
const SESSION_LOST: &str = "The previous Claude session could not be restored, so this run \
     continues in a new one without its earlier context.";

/// Locate the device's installed Claude Code CLI: our own PATH, then the login-shell PATH
/// snapshot (the user's shell init shapes PATH in ways a GUI/service launch never sees, see
/// [`crate::shell_env`]), then known install locations as a last resort. The
/// `CLAUDE_CODE_EXECUTABLE` override is applied by [`ClaudeExecutor::resolve_executable`], so
/// availability and launches agree on one resolution order.
fn resolve_claude_executable() -> Option<PathBuf> {
    let mut extra = Vec::new();
    if let Some(home) = crate::executable::home_dir() {
        extra.push(home.join(".claude").join("local").join("claude"));
        extra.push(home.join(".local").join("bin").join("claude"));
    }
    extra.push(PathBuf::from("/opt/homebrew/bin/claude"));
    extra.push(PathBuf::from("/usr/local/bin/claude"));
    crate::executable::find_on_paths("claude", extra)
}

/// Claude's config root for a run: the run's own `CLAUDE_CONFIG_DIR` or `HOME` when its
/// environment sets one, else ours.
fn config_root(env: &std::collections::BTreeMap<String, String>) -> PathBuf {
    if let Some(dir) = env.get("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    if let Some(home) = env.get("HOME").filter(|h| !h.is_empty()) {
        return PathBuf::from(home).join(".claude");
    }
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::executable::home_or_current_dir().join(".claude"))
}

/// The setting sources an isolated run loads: the project's and the local ones, never the user's.
const ISOLATED_SETTING_SOURCES: &str = "project,local";

/// What the setting sources do not control, switched off for an isolated run: the agent reading
/// and writing the owner's auto-memory under `~/.claude/projects/`, and the claude.ai MCP
/// connectors of the owner's login.
const ISOLATION_ENV: [(&str, &str); 2] = [
    ("CLAUDE_CODE_DISABLE_AUTO_MEMORY", "1"),
    ("ENABLE_CLAUDEAI_MCP_SERVERS", "false"),
];

/// The inline `--mcp-config` JSON for the injected servers (the CLI accepts a JSON string as
/// well as a file path), or `None` when there are none. A name that repeats keeps its first
/// entry: Reno's own server is first in `ExecSpec::mcp`.
fn mcp_servers_config_arg(servers: &[McpServerSpec]) -> Option<String> {
    if servers.is_empty() {
        return None;
    }
    let mut config = serde_json::Map::new();
    for server in servers {
        config.entry(server.name.clone()).or_insert_with(|| {
            serde_json::json!({
                "command": server.command,
                "args": server.args,
                "env": server.env,
            })
        });
    }
    Some(serde_json::json!({ "mcpServers": config }).to_string())
}

/// The Claude Code executor. Construct with [`ClaudeExecutor::new`]; tests point it at a fake
/// CLI with [`ClaudeExecutor::with_executable`].
///
/// # Access modes
///
/// Claude decides what is allowed without asking through its own permission mode; whatever the
/// mode does not cover is sent to Reno as a `can_use_tool` request. Reno's access mode picks the
/// CLI's mode and what this driver answers itself:
///
/// | `AccessMode` | `--permission-mode` | The CLI asks about | This driver answers itself | Reaches Reno as `NeedsApproval` |
/// |---|---|---|---|---|
/// | `FullAccess` | `default` | what `default` does not allow (commands, network, edits, MCP tools) | every such request: allow | nothing |
/// | `AutoAcceptEdits` | `acceptEdits` | what `acceptEdits` does not allow: commands, network, MCP tools, and edits it does not cover | nothing | all of it |
/// | `Supervised` | `default` | what `default` does not allow (commands, network, edits, MCP tools) | nothing | all of it |
///
/// What the CLI allows by itself in its mode (reading inside the working directory in every
/// mode, edits inside it in `acceptEdits`) never reaches Reno, and a request that does reach
/// Reno is never allowed unseen except under `FullAccess`. In particular an edit request under
/// `AutoAcceptEdits` is an edit `acceptEdits` did not cover, so it is asked.
///
/// `FullAccess` deliberately does not launch with `bypassPermissions` or a skip-permissions
/// flag: the CLI refuses those as root outside a sandbox, so the run stays in `default` and this
/// driver allows each request instead. `ExitPlanMode` is a request like any other:
/// asked in `AutoAcceptEdits` and `Supervised`, and approving it continues in the permission
/// mode of the table. A request that arrives after the run was cancelled is denied at once.
/// No mode is ever widened mid-run.
///
/// [`Self::with_access_modes`] narrows what an instance will honour; `start` refuses any other
/// mode with [`ExecError::UnsupportedAccess`] before spawning (ARCH-15).
pub struct ClaudeExecutor {
    executable: Option<PathBuf>,
    /// Grace between the interrupt control request and SIGTERM.
    interrupt_grace: Duration,
    /// Grace between SIGTERM and SIGKILL.
    kill_grace: Duration,
    access_modes: Vec<AccessMode>,
    /// The CLI version `probe` last read, for the descriptor.
    cli_version: Arc<Mutex<Option<String>>>,
    /// How long `probe` lets each status command (`--version`, `auth status`) run.
    probe_budget: Duration,
    /// Where `probe` looks for a credential in the environment.
    probe_env: health::EnvLookup,
    tracker: TaskTracker,
    /// Frames of a type the wire layer does not know, counted across runs.
    unknown: UnknownFrames,
}

impl std::fmt::Debug for ClaudeExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClaudeExecutor")
            .field("executable", &self.executable)
            .field("access_modes", &self.access_modes)
            .finish_non_exhaustive()
    }
}

impl Default for ClaudeExecutor {
    fn default() -> Self {
        Self {
            executable: None,
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
            access_modes: vec![
                AccessMode::Supervised,
                AccessMode::AutoAcceptEdits,
                AccessMode::FullAccess,
            ],
            cli_version: Arc::new(Mutex::new(None)),
            probe_budget: probe::COMMAND_BUDGET,
            probe_env: health::process_env(),
            tracker: TaskTracker::new(),
            unknown: UnknownFrames::new(),
        }
    }
}

impl ClaudeExecutor {
    /// An executor that finds the CLI on PATH, in the login shell's PATH or in a known
    /// install location.
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

    /// Bound each status command `probe` runs (tests shrink it; default 10 s).
    #[must_use]
    pub fn with_probe_budget(mut self, budget: Duration) -> Self {
        self.probe_budget = budget;
        self
    }

    /// Where `probe` looks for a credential in the environment instead of the process's own
    /// (a test seam: the machine a test runs on may have one set).
    #[doc(hidden)]
    #[must_use]
    pub fn with_probe_env(
        mut self,
        lookup: impl Fn(&str) -> Option<std::ffi::OsString> + Send + Sync + 'static,
    ) -> Self {
        self.probe_env = std::sync::Arc::new(lookup);
        self
    }

    /// Honour only these access modes: the descriptor lists them and `start` refuses the rest.
    #[must_use]
    pub fn with_access_modes(mut self, modes: Vec<AccessMode>) -> Self {
        self.access_modes = modes;
        self
    }

    /// Whether the CLI can be launched now: the launch resolver, not bare discovery, so a valid
    /// `CLAUDE_CODE_EXECUTABLE` (or a fixed `executable`) reports installed and an invalid one
    /// does not.
    pub fn installed(&self) -> bool {
        self.resolve_executable().is_ok()
    }

    fn resolve_executable(&self) -> Result<PathBuf, ExecError> {
        resolve_executable(self.executable.as_deref())
    }

    /// The configuration arguments a run launches with: everything the process bakes in at
    /// spawn except the session to resume.
    fn launch_args(spec: &ExecSpec) -> Vec<String> {
        let mut args: Vec<String> = [
            "--print",
            "--input-format",
            "stream-json",
            "--output-format",
            "stream-json",
            // Required by the CLI alongside `-p --output-format stream-json`.
            "--verbose",
            "--include-partial-messages",
            "--replay-user-messages",
            // Newer Claude models emit no readable thinking text unless a summary is asked
            // for (raw reasoning stays provider-private).
            "--thinking-display",
            "summarized",
            // Route permission prompts to the stdio control channel so `can_use_tool` (and
            // AskUserQuestion in particular) reaches us. Undocumented flag; validated live
            // against 2.1.228.
            "--permission-prompt-tool",
            "stdio",
        ]
        .map(str::to_owned)
        .into();
        if let Some(model) = &spec.model {
            args.extend(["--model".into(), model.clone()]);
        }
        if let Some(instructions) = &spec.instructions {
            // Appended to Claude Code's own system prompt, so its tool guidance stays.
            args.extend(["--append-system-prompt".into(), instructions.clone()]);
        }
        if let Some(effort) = spec.effort {
            let effort = match effort {
                Effort::Low => "low",
                Effort::Medium => "medium",
                Effort::High => "high",
            };
            args.extend(["--effort".into(), effort.into()]);
        }
        args.extend([
            "--permission-mode".into(),
            permission::permission_mode(spec.access).into(),
        ]);
        if !spec.inherit_user_config {
            args.extend(["--setting-sources".into(), ISOLATED_SETTING_SOURCES.into()]);
        }
        args
    }

    fn build_command(exe: &Path, spec: &ExecSpec) -> Command {
        let mut cmd = Command::new(exe);
        // Only the allowlisted part of the daemon's environment (see `child_env`).
        crate::child_env::restrict(&mut cmd, crate::child_env::CLAUDE);
        // Its own process group, so teardown reaches the MCP servers and tools it started;
        // and no inherited Claude Code session markers.
        crate::process::owned::configure(&mut cmd);
        crate::compose_child_path(&mut cmd, exe);
        // The allowlisted environment of the run (credentials the host resolved, for one).
        // Never a Claude.ai subscription token: those stay inside the CLI's own login.
        crate::apply_run_env(&mut cmd, &spec.env);
        // After the run's own variables, so nothing a bot supplies switches the isolation off.
        if !spec.inherit_user_config {
            cmd.envs(ISOLATION_ENV);
        }
        cmd.args(Self::launch_args(spec));
        if let Some(resume) = &spec.resume {
            cmd.arg(format!("--resume={}", resume.0));
        }
        // Reno's own server rides beside the user's configured servers (no
        // `--strict-mcp-config`): the CLI merges an inline JSON config with settings-sourced
        // ones.
        if let Some(config) = mcp_servers_config_arg(&spec.mcp) {
            cmd.args(["--mcp-config", &config]);
        }
        if !spec.cwd.as_os_str().is_empty() {
            cmd.current_dir(&spec.cwd);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        cmd
    }
}

/// The CLI to launch: the fixed `executable` if there is one, else `CLAUDE_CODE_EXECUTABLE`,
/// else wherever it is installed.
fn resolve_executable(executable: Option<&Path>) -> Result<PathBuf, ExecError> {
    let not_installed = |error: crate::HarnessError| ExecError::NotInstalled(error.to_string());
    if let Some(p) = executable {
        return crate::executable::validate_native_override(p).map_err(not_installed);
    }
    if let Some(p) = std::env::var_os("CLAUDE_CODE_EXECUTABLE")
        && !p.is_empty()
    {
        return crate::executable::validate_native_override(&PathBuf::from(p))
            .map_err(not_installed);
    }
    resolve_claude_executable().ok_or_else(|| {
        ExecError::NotInstalled(
            "claude (searched PATH, the login shell's PATH, ~/.claude/local, \
             ~/.local/bin, /opt/homebrew/bin, /usr/local/bin, and \
             fnm/nvm/volta/pnpm/bun install dirs; set \
             CLAUDE_CODE_EXECUTABLE to override)"
                .into(),
        )
    })
}

/// Start one CLI process for a run, wire its stdio up and report it for the orphan sweep.
async fn launch(
    tracker: &TaskTracker,
    exe: &Path,
    spec: &ExecSpec,
    processes: &crate::exec::ProcessSink,
) -> Result<Runtime, std::io::Error> {
    let mut child = ClaudeExecutor::build_command(exe, spec)
        .spawn()
        .map_err(|e| {
            let cwd = (!spec.cwd.as_os_str().is_empty()).then_some(spec.cwd.as_path());
            match crate::executable::spawn_error(e, cwd, || crate::executable::binary_hint(exe)) {
                crate::HarnessError::Io(error) => error,
                other => std::io::Error::other(other.to_string()),
            }
        })?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let child = Child::new(child);
    // Reported at once: a daemon that dies from here on still leaves its row.
    #[cfg(unix)]
    if let Some(record) = child.record() {
        let _ = processes.report(record).await;
    }
    let missing = |what: &str| std::io::Error::other(format!("claude child has no {what}"));
    let stdin = stdin.ok_or_else(|| missing("stdin"))?;
    let stdout = stdout.ok_or_else(|| missing("stdout"))?;
    let stderr_tail = crate::StderrTail::for_run(&spec.env);
    let stderr_task = stderr.map(|stderr| tracker.spawn(read_stderr(stderr, stderr_tail.clone())));
    Ok(Runtime {
        child,
        stdout_lines: crate::lossy_lines::lossy_lines(stdout),
        stdin: Stdin::new(tracker, stdin, spec.resume.is_some()),
        stderr_tail,
        stderr_task,
    })
}

#[async_trait]
impl Executor for ClaudeExecutor {
    fn descriptor(&self) -> ExecutorDescriptor {
        let cli = self
            .cli_version
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let models = catalog::supported_models(cli.as_deref());
        ExecutorDescriptor {
            id: ID.clone(),
            contract_version: CONTRACT_VERSION,
            adapter_version: match cli {
                Some(cli) => format!(
                    "reno-harness {} claude-code driver (CLI {cli})",
                    env!("CARGO_PKG_VERSION")
                ),
                None => format!(
                    "reno-harness {} claude-code driver",
                    env!("CARGO_PKG_VERSION")
                ),
            },
            cli_verified: VersionRange {
                min: MIN_CLI_VERSION.into(),
                verified: VERIFIED_CLI_VERSION.into(),
            },
            caps: ExecutorCaps {
                // Prompts land in the running turn at the CLI's next step.
                steering: Steering::StepBoundary,
                resume: true,
                access_modes: self.access_modes.clone(),
                // The CLI has no sandbox Reno can set; its permission system is what the access
                // mode drives. A run is therefore honoured only at `Unrestricted` (no sandbox,
                // as asked); `ReadOnly` and `WorkspaceWrite` would be run with more latitude
                // than requested, so they are not declared (ARCH-15).
                sandbox_levels: SANDBOX_LEVELS.to_vec(),
                mcp_injection: true,
                structured_output: false,
                // Done is the CLI's own terminal `result` frame.
                deterministic_turn_end: true,
                // The CLI has a plan mode and the driver answers its ExitPlanMode, but `ExecSpec`
                // cannot request it yet; never declare a capability a caller cannot use (ARCH-15).
                plan_mode: false,
                models,
                // `--setting-sources project,local` and two environment switches (see
                // `isolation_args`), unless the run opts in with `inherit_user_config`.
                isolates_user_config: true,
            },
        }
    }

    /// Where the CLI is and its version, whether its version is one the driver supports, and
    /// whether it is signed in (`claude auth status`, or a credential in the environment; see
    /// [`health`]). The model list is the curated one: the CLI has no cheap, offline way to
    /// list models.
    async fn probe(&self) -> HealthProbe {
        let overridden = self.executable.is_some()
            || std::env::var_os("CLAUDE_CODE_EXECUTABLE").is_some_and(|value| !value.is_empty());
        let fixed = self.executable.clone();
        let resolved =
            tokio::task::spawn_blocking(move || resolve_executable(fixed.as_deref())).await;
        let curated = |cli: Option<&str>| ModelCatalog::curated(catalog::supported_models(cli));
        let exe = match resolved {
            Ok(Ok(exe)) => exe,
            Ok(Err(error)) => {
                return HealthProbe::not_installed(
                    probe::not_installed_reason(error.to_string(), overridden),
                    curated(None),
                );
            }
            Err(error) => return probe::failed(error, curated(None)),
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
        // An older CLI is ineligible already, and may not know `auth status`: not asked.
        let signed_in = if matches!(version_gate, VersionGate::TooOld { .. }) {
            SignedIn::Unknown
        } else {
            health::sign_in(&exe, self.probe_budget, &self.probe_env).await
        };
        HealthProbe {
            installed: probe::installed(exe, version.clone()),
            version_gate,
            signed_in,
            models: curated(version.as_deref()),
        }
    }

    fn unknown_frames(&self) -> std::collections::BTreeMap<String, u64> {
        self.unknown.snapshot()
    }

    async fn start(
        &self,
        spec: ExecSpec,
        controls: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError> {
        // Fail closed before anything is resolved or spawned (ARCH-15).
        require_access(spec.access, &self.access_modes)?;
        require_sandbox(spec.sandbox, &SANDBOX_LEVELS)?;
        if spec.output_schema.is_some() {
            return Err(ExecError::UnsupportedOutputSchema);
        }
        let exe = self.resolve_executable()?;
        let ExecControls {
            steering,
            cancel,
            answers,
            processes,
        } = controls;

        // A session whose transcript holds no conversation cannot be resumed: the CLI says so
        // and exits, every time. Start fresh instead: the history is gone either way. A
        // transcript not found here is still resumed: the CLI may keep it elsewhere, and says
        // so when it does not.
        let mut spec = spec;
        if let Some(session) = spec.resume.clone()
            && resume::transcript(&config_root(&spec.env), &session.0).await == Transcript::Empty
        {
            tracing::info!(
                target: "reno_harness::claude",
                session = %session.0,
                "no conversation to resume; starting a new session"
            );
            tracing::warn!(target: "reno_harness::claude", "{SESSION_LOST}");
            spec.resume = None;
        }

        let Runtime {
            child,
            stdout_lines,
            mut stdin,
            stderr_tail,
            stderr_task,
        } = match launch(&self.tracker, &exe, &spec, &processes).await {
            Ok(runtime) => runtime,
            // Any failure to launch is a failed run, not an ineligible executor.
            Err(error) => {
                let diagnostic = crate::redact_secrets(&error.to_string());
                return Ok(futures::stream::once(async move {
                    ExecEvent::Done {
                        stop: StopReason::Error,
                        result: None,
                        diagnostic: Some(diagnostic),
                        reason: None,
                    }
                })
                .boxed());
            }
        };

        stdin.line(wire::initialize_request_line("reno_initialize"));
        // The prompt as the first stdin user line (streaming-input mode).
        let first_command = uuid::Uuid::new_v4().to_string();
        stdin.line(wire::prompt_line(&spec.prompt, &first_command, &[]));

        let (event_tx, event_rx) = mpsc::channel::<ExecEvent>(EVENT_CAPACITY);
        let lock_dir = refresh_lock::config_dir_for(&spec.env);
        self.tracker.spawn(run_session(Session {
            normalizer: Normalizer::new(),
            exe,
            access: spec.access,
            launched: spec,
            child,
            stdout_lines,
            first_command,
            stdin,
            event_tx,
            steering,
            cancel,
            answers,
            processes,
            interrupt_grace: self.interrupt_grace,
            kill_grace: self.kill_grace,
            lock_dir,
            stderr_tail,
            stderr_task,
            tracker: self.tracker.clone(),
            unknown: self.unknown.clone(),
        }));

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

/// Forward a child's stderr to the debug log and the crash-message tail, and say when it
/// closed: an exit's last lines land just after stdout's.
async fn read_stderr(stderr: impl tokio::io::AsyncRead + Unpin, tail: crate::StderrTail) {
    let mut lines = crate::lossy_lines::lossy_lines(stderr);
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!(target: "reno_harness::claude", "stderr: {}", tail.scrub(&line));
        tail.push(&line);
    }
    tail.close();
}

/// One launched CLI process and its stdio.
struct Runtime {
    child: Child,
    stdout_lines: crate::lossy_lines::LossyLines<crate::process::ChildStdout>,
    stdin: Stdin,
    stderr_tail: crate::StderrTail,
    /// Ends when the pipe closes, which a grandchild that escaped teardown can hold open
    /// forever: the run aborts it, so `shutdown` is bounded.
    stderr_task: Option<tokio::task::JoinHandle<()>>,
}

/// The run's stdin, through its writer task. Until the CLI reports its session, a resuming run
/// keeps a copy of everything it wrote: when the session cannot be restored, the fresh process
/// is told the same things.
struct Stdin {
    tx: mpsc::Sender<String>,
    replay: Option<Vec<String>>,
}

/// Lines queued for the CLI's stdin (ARCH-11). The CLI reads each at once; a queue this deep
/// means it is wedged, and the run ends rather than growing without bound.
const STDIN_CAPACITY: usize = 1024;

impl Stdin {
    fn new(tracker: &TaskTracker, stdin: ChildStdin, keep_for_replay: bool) -> Self {
        let (tx, rx) = mpsc::channel(STDIN_CAPACITY);
        tracker.spawn(stdin_writer(stdin, rx));
        Self {
            tx,
            replay: keep_for_replay.then(Vec::new),
        }
    }

    /// Queue one line. False once the writer is gone (the child died) or the CLI has stopped
    /// reading.
    fn line(&mut self, line: String) -> bool {
        if let Some(replay) = &mut self.replay {
            replay.push(line.clone());
        }
        match self.tx.try_send(line) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                tracing::warn!(target: "reno_harness::claude", "the CLI's stdin queue is full; the CLI is not reading");
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => false,
        }
    }
}

/// Owns the child's stdin; a write failure (EPIPE after the child died) is tolerated and
/// logged. The pipe closes when the run drops its [`Stdin`], after the child was torn down.
async fn stdin_writer(mut stdin: ChildStdin, mut rx: mpsc::Receiver<String>) {
    while let Some(line) = rx.recv().await {
        let write = async {
            stdin.write_all(line.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        };
        if let Err(e) = write.await {
            tracing::debug!(target: "reno_harness::claude", "stdin write failed (tolerated): {e}");
            return;
        }
    }
}

struct Session {
    normalizer: Normalizer,
    /// What the process was launched from, for a fresh relaunch when its session cannot be
    /// restored.
    exe: PathBuf,
    launched: ExecSpec,
    access: AccessMode,
    child: Child,
    stdout_lines: crate::lossy_lines::LossyLines<crate::process::ChildStdout>,
    /// The uuid the opening prompt was written with.
    first_command: String,
    stdin: Stdin,
    event_tx: mpsc::Sender<ExecEvent>,
    steering: mpsc::Receiver<String>,
    cancel: CancellationToken,
    answers: mpsc::Receiver<ExecAnswer>,
    processes: crate::exec::ProcessSink,
    interrupt_grace: Duration,
    kill_grace: Duration,
    /// Claude's credential dir: no signal lands mid-refresh (see [`refresh_lock`]).
    lock_dir: PathBuf,
    /// Rolling stderr tail for the crash message on an unexpected exit.
    stderr_tail: crate::StderrTail,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
    tracker: TaskTracker,
    unknown: UnknownFrames,
}

/// What the driver needs to answer an approval later: the tool's input, which an allow echoes
/// back, and whether the call asks to leave plan mode.
struct ApprovalPayload {
    input: Value,
    exit_plan: bool,
}

/// What the driver needs to answer a question later.
struct InputPayload {
    input: Value,
    asked: Vec<permission::Asked>,
}

type Requests = OpenRequests<ApprovalPayload, InputPayload>;

/// Write the control response for `request_id`. False once the child's stdin is gone.
fn respond(stdin: &mut Stdin, request_id: &str, response: Value) -> bool {
    stdin.line(control_response_line(request_id, response))
}

/// Refuse every open request: the run is over or was cancelled, and nothing is allowed by
/// default.
fn deny_all(requests: &mut Requests, stdin: &mut Stdin) {
    for (request_id, open) in requests.drain() {
        let message = match open {
            crate::exec::Open::Approval(_) => permission::DENIED,
            crate::exec::Open::Input(_) => permission::QUESTIONS_DISMISSED,
        };
        respond(stdin, &request_id, permission::deny(message));
    }
}

/// Serve one control request from the CLI. The CLI blocks until SOME response arrives, so every
/// request is answered or recorded as open. Returns false when the consumer is gone.
///
/// What the run's access mode covers is allowed at once (see [`ClaudeExecutor`]); anything else
/// becomes a `NeedsApproval` and waits for `ExecHandle::decide`. `AskUserQuestion` becomes a
/// `NeedsInput`. A request of any other subtype is answered with an error: one left unanswered
/// would stall the run.
async fn serve_control_request(
    frame: ControlRequestFrame,
    access: AccessMode,
    cancelled: bool,
    requests: &mut Requests,
    stdin: &mut Stdin,
    event_tx: &mpsc::Sender<ExecEvent>,
) -> bool {
    let ControlRequestFrame {
        request_id,
        request,
        raw,
    } = frame;
    if request.subtype != "can_use_tool" {
        tracing::debug!(
            target: "reno_harness::claude",
            "unhandled control_request subtype: {}", request.subtype
        );
        stdin.line(control_error_line(
            &request_id,
            &format!("unsupported control request: {}", request.subtype),
        ));
        return true;
    }
    // The run is being cancelled: nothing new is allowed, and nobody is left to ask.
    if cancelled {
        let message = if request.tool_name == permission::ASK_USER_QUESTION {
            permission::QUESTIONS_DISMISSED
        } else {
            permission::DENIED
        };
        respond(stdin, &request_id, permission::deny(message));
        return true;
    }
    let tool = request.tool_name;
    let input = request.input;
    if tool == permission::ASK_USER_QUESTION {
        let asked = permission::parse_questions(&request_id, &input);
        if asked.is_empty() {
            // Nothing to ask: the tool is unblocked rather than wedged.
            respond(
                stdin,
                &request_id,
                permission::deny(permission::QUESTIONS_DISMISSED),
            );
            return true;
        }
        let questions = asked.iter().map(|a| a.question.clone()).collect();
        let replaced = requests.open_input(request_id.clone(), InputPayload { input, asked });
        note_superseded(replaced, &request_id);
        return event_tx
            .send(ExecEvent::NeedsInput {
                request_id,
                questions,
            })
            .await
            .is_ok();
    }
    let exit_plan = tool == permission::EXIT_PLAN_MODE;
    // Approving the plan leaves plan mode for the run's access mode, not the CLI's `default`.
    let after_plan = exit_plan.then(|| permission::permission_mode(access));
    if permission::answered_here(access) {
        respond(stdin, &request_id, permission::allow(input, after_plan));
        return true;
    }
    let subject = permission::subject(&tool, &input, &raw);
    let replaced = requests.open_approval(request_id.clone(), ApprovalPayload { input, exit_plan });
    note_superseded(replaced, &request_id);
    event_tx
        .send(ExecEvent::NeedsApproval {
            request_id,
            subject,
        })
        .await
        .is_ok()
}

/// A request id the CLI reused while its first request was still open: the new request
/// supersedes the old one, which the CLI can no longer be waiting on under that id. Denying it
/// would be read as the answer to the new request.
fn note_superseded(
    replaced: Option<crate::exec::Open<ApprovalPayload, InputPayload>>,
    request_id: &str,
) {
    if replaced.is_some() {
        tracing::warn!(
            target: "reno_harness::claude",
            request_id,
            "the CLI reused the id of a request still open; the earlier one is dropped"
        );
    }
}

/// Apply one answer from the consumer.
fn apply_answer(
    answer: ExecAnswer,
    access: AccessMode,
    requests: &mut Requests,
    stdin: &mut Stdin,
) {
    let request_id = answer.request_id.clone();
    let after_plan = permission::permission_mode(access);
    match requests.resolve(answer) {
        Resolved::Decision(open, crate::exec::Decision::Allow, _) => {
            let leave_plan = open.exit_plan.then_some(after_plan);
            respond(
                stdin,
                &request_id,
                permission::allow(open.input, leave_plan),
            );
        }
        Resolved::Decision(_, crate::exec::Decision::Deny, message) => {
            // The model reads the permission response's message: a policy rule's own words
            // reach it, so it does not retry a call policy will refuse again.
            let message = permission::refusal_text(message.as_deref());
            respond(stdin, &request_id, permission::deny(&message));
        }
        Resolved::Input(open, given) => {
            // No answers is the person closing the questions: a deny that says so, rather than
            // an allow with empty picks.
            let response = if given.is_empty() {
                permission::deny(permission::QUESTIONS_DISMISSED)
            } else {
                permission::allow(
                    permission::updated_input_with_answers(&open.input, &open.asked, &given),
                    None,
                )
            };
            respond(stdin, &request_id, response);
        }
        Resolved::WrongKind => tracing::warn!(
            target: "reno_harness::claude",
            request_id,
            "an answer of the wrong kind for this request is ignored; it stays open"
        ),
        Resolved::Unknown => tracing::warn!(
            target: "reno_harness::claude",
            request_id,
            "answer for an unknown or already resolved request"
        ),
    }
}

/// The per-run event loop: one task multiplexing stdout frames, the steering queue, the
/// answers, the cancel token and consumer liveness.
async fn run_session(session: Session) {
    let Session {
        normalizer: mut norm,
        exe,
        launched,
        access,
        mut child,
        mut stdout_lines,
        first_command,
        mut stdin,
        event_tx,
        mut steering,
        cancel,
        mut answers,
        processes,
        interrupt_grace,
        kill_grace,
        lock_dir,
        mut stderr_tail,
        mut stderr_task,
        tracker,
        unknown,
    } = session;

    let mut pending_steers = std::collections::VecDeque::new();
    let mut requests = Requests::new();
    // Top-level tool calls in flight: a steer must not abort them (see
    // `wire::steer_message_line_with_images`).
    let mut open_tools = std::collections::HashSet::new();
    let mut steering_open = true;
    let mut answers_open = true;
    let mut interrupted = false;
    let mut interrupt_sent = false;
    // The run's one `Done` has gone out (or the consumer left).
    let mut ended = false;
    // A resumed session the CLI turns out to have no conversation for is relaunched fresh,
    // once (see [`no_conversation`]).
    let mut restore_fallback = launched.resume.is_some();
    let mut relaunch = false;
    let mut commands = Commands::default();
    commands.written(first_command);
    // A `result` that is a steer boundary rather than the run's end (see [`Commands`]). It is
    // superseded (dropped) once the waiting message starts its own turn, which ends with its
    // own result; it is released as the run's end if that message is cancelled before it
    // starts.
    let mut held_done: Option<ExecEvent> = None;
    // Last stdout activity, for the held result's settle fallback.
    let mut last_frame_at = tokio::time::Instant::now();
    let mut escalation: Option<tokio::task::JoinHandle<()>> = None;
    // The runtime's process tree when its teardown began (see [`crate::shutdown_agent`]).
    #[cfg_attr(not(unix), allow(unused_mut))]
    let mut torn_down: Vec<crate::process::Tracked> = Vec::new();

    'main: loop {
        if std::mem::take(&mut relaunch) {
            // The CLI had no conversation for the session and exited without starting one.
            // Everything written to it goes to a fresh process; its own init reports the new
            // session id.
            let replay = stdin.replay.take().unwrap_or_default();
            if matches!(child.try_wait(), Ok(None)) {
                refresh_lock::settle(&lock_dir, refresh_lock::SETTLE_MAX).await;
            }
            child.shutdown(kill_grace).await;
            let mut fresh = launched.clone();
            fresh.resume = None;
            match launch(&tracker, &exe, &fresh, &processes).await {
                Ok(runtime) => {
                    child = runtime.child;
                    stdout_lines = runtime.stdout_lines;
                    stdin = runtime.stdin;
                    stderr_tail = runtime.stderr_tail;
                    if let Some(old) = std::mem::replace(&mut stderr_task, runtime.stderr_task) {
                        old.abort();
                    }
                }
                Err(error) => {
                    let diagnostic = crate::redact_secrets(&error.to_string());
                    ended = event_tx
                        .send(ExecEvent::Done {
                            stop: StopReason::Error,
                            result: None,
                            diagnostic: Some(diagnostic),
                            reason: None,
                        })
                        .await
                        .is_ok();
                    break 'main;
                }
            }
            for line in replay {
                if !stdin.line(line) {
                    break;
                }
            }
            tracing::warn!(target: "reno_harness::claude", "{SESSION_LOST}");
        }
        tokio::select! {
            line = stdout_lines.next_line() => match line {
                Ok(Some(line)) => {
                    let line = line.trim();
                    if line.is_empty() {
                        continue;
                    }
                    // The CLI is still producing: whatever it is doing is not the quiet end a
                    // held result's fallback waits for.
                    last_frame_at = tokio::time::Instant::now();
                    let frame = match wire::parse_frame(line) {
                        Ok(frame) => frame,
                        Err(e) => {
                            tracing::debug!(
                                target: "reno_harness::claude",
                                "unparseable frame (skipped): {}",
                                stderr_tail.scrub(&e.to_string())
                            );
                            continue;
                        }
                    };
                    if let Frame::Unknown(kind) = &frame {
                        unknown.note(&format!("type:{kind}"));
                    }
                    if restore_fallback
                        && !interrupted
                        && !norm.started()
                        && let Frame::Result(result) = &frame
                        && result
                            .errors
                            .iter()
                            .any(|e| e.as_str().is_some_and(no_conversation))
                    {
                        tracing::info!(
                            target: "reno_harness::claude",
                            session = ?launched.resume,
                            "session could not be restored; relaunching fresh"
                        );
                        restore_fallback = false;
                        relaunch = true;
                        continue;
                    }
                    if let Frame::ControlResponse(response) = &frame {
                        // Only requests this driver sends itself (the initialize handshake,
                        // the interrupt) are answered here; a failure of one is worth a line.
                        if let Some(error) = response.response.error_message() {
                            tracing::warn!(
                                target: "reno_harness::claude",
                                request = %response.response.request_id,
                                "control request failed: {error}"
                            );
                        }
                        continue;
                    }
                    if let Frame::ControlRequest(request) = frame {
                        if !serve_control_request(request, access, interrupted, &mut requests, &mut stdin, &event_tx).await {
                            break 'main; // consumer gone: reap below
                        }
                        continue;
                    }
                    // Only the CLI confirms that a prompt joined its conversation: its
                    // lifecycle or its replay, whichever comes first. Writing stdin must not
                    // split ongoing text. A confirmation covers its steer and every earlier
                    // one: superseded steers are never replayed themselves. A message a turn
                    // took up (started, or replayed) also supersedes a result held for it:
                    // that result was only the boundary, and the message's turn ends with its
                    // own.
                    let (confirmed, taken_up) = match &frame {
                        Frame::CommandLifecycle(cmd) => {
                            let started = commands.lifecycle(&cmd.command_uuid, &cmd.state);
                            let confirmed = started || Commands::is_terminal(&cmd.state);
                            (confirmed.then(|| cmd.command_uuid.clone()), started)
                        }
                        Frame::User(user) if user.parent_tool_use_id.is_none() => {
                            let replayed = user.uuid.clone().filter(|id| pending_steers.contains(id));
                            let taken_up = replayed.is_some();
                            (replayed, taken_up)
                        }
                        _ => (None, false),
                    };
                    if taken_up {
                        held_done = None;
                    }
                    if let Some(at) = confirmed
                        .and_then(|id| pending_steers.iter().position(|p| *p == id))
                    {
                        pending_steers.drain(..=at);
                    }
                    // Every waiting message was cancelled before it started: the held result
                    // was the run's real end after all.
                    if matches!(frame, Frame::CommandLifecycle(_))
                        && !commands.waiting()
                        && let Some(done) = held_done.take()
                    {
                        ended = event_tx.send(done).await.is_ok();
                        break 'main;
                    }
                    let events = norm.normalize(frame, interrupted);
                    // The session is the CLI's now: nothing left to replay.
                    if norm.started() {
                        restore_fallback = false;
                        stdin.replay = None;
                    }
                    for ev in events {
                        match &ev {
                            ExecEvent::Tool { id, .. } => {
                                open_tools.insert(id.clone());
                            }
                            ExecEvent::ToolResult { id, .. } => {
                                open_tools.remove(id);
                            }
                            ExecEvent::Done { .. } => open_tools.clear(),
                            _ => {}
                        }
                        let is_done = matches!(ev, ExecEvent::Done { .. });
                        // A `now` steer ends the turn it interrupts with a result frame, and a
                        // message queued behind a turn that just ended starts another: either
                        // way that result is a steer boundary, not the end of the run.
                        let boundary = if commands.reported {
                            commands.waiting()
                        } else {
                            !pending_steers.is_empty()
                        };
                        if is_done && !interrupted && boundary {
                            held_done = Some(ev);
                            continue;
                        }
                        if is_done {
                            held_done = None;
                        }
                        if event_tx.send(ev).await.is_err() {
                            break 'main; // consumer gone: reap below
                        }
                        if is_done {
                            // The run ends with its `Done`: the CLI's session would go on
                            // (and wake itself), the run does not.
                            ended = true;
                            break 'main;
                        }
                    }
                }
                // stdout EOF: the CLI exited. An older CLI says it has no conversation for the
                // session on stderr alone.
                Ok(None) => {
                    if restore_fallback && !interrupted && !norm.started() {
                        stderr_tail.wait_closed().await;
                        if stderr_tail.snapshot().is_some_and(|tail| no_conversation(&tail)) {
                            restore_fallback = false;
                            relaunch = true;
                            continue;
                        }
                    }
                    break 'main;
                }
                Err(e) => {
                    ended = event_tx
                        .send(ExecEvent::Done {
                            stop: StopReason::Error,
                            result: None,
                            diagnostic: Some(crate::redact_secrets(&e.to_string())), reason: None,
                        })
                        .await
                        .is_ok();
                    break 'main;
                }
            },

            prompt = steering.recv(), if steering_open && !interrupted => match prompt {
                Some(prompt) => {
                    let id = uuid::Uuid::new_v4().to_string();
                    let line = wire::steer_message_line_with_images(
                        &prompt,
                        &id,
                        open_tools.is_empty(),
                        &[],
                    );
                    pending_steers.push_back(id.clone());
                    commands.written(id);
                    if !stdin.line(line) { break 'main; }
                }
                // The queue closed. Stdin stays open: approvals and answers still need it, and
                // the run ends with its `Done`.
                None => steering_open = false,
            },

            answer = answers.recv(), if answers_open => match answer {
                Some(answer) => apply_answer(answer, access, &mut requests, &mut stdin),
                None => answers_open = false,
            },

            _ = cancel.cancelled(), if !interrupt_sent => {
                interrupt_sent = true;
                interrupted = true;
                // A request still open when the run is cancelled is refused, before anything
                // else reaches the CLI.
                deny_all(&mut requests, &mut stdin);
                // The runtime is going: so is everything it started.
                #[cfg(unix)]
                if let Some(pid) = child.id() {
                    torn_down = crate::process::descendants(pid).await;
                }
                stdin.line(wire::interrupt_request_line("int_1"));
                // Escalate if the CLI doesn't wind down within the grace periods: SIGTERM
                // (kills bash trees, runs SessionEnd hooks), then SIGKILL, neither while a
                // login refresh is being saved. Aborted once the child is reaped.
                if let Some(pid) = crate::process::signal_target(&child) {
                    let lock_dir = lock_dir.clone();
                    escalation = Some(tracker.spawn(async move {
                        tokio::time::sleep(interrupt_grace).await;
                        refresh_lock::settle(&lock_dir, refresh_lock::SETTLE_MAX).await;
                        send_signal(&pid, Signal::Term);
                        tokio::time::sleep(kill_grace).await;
                        refresh_lock::settle(&lock_dir, refresh_lock::SETTLE_MAX).await;
                        send_signal(&pid, Signal::Kill);
                    }));
                }
            },

            _ = tokio::time::sleep_until(
                last_frame_at + commands.held_settle().unwrap_or_default()
            ), if held_done.is_some() && commands.held_settle().is_some() => {
                // The steers were absorbed into the turn that just ended.
                commands.cancel_waiting();
                pending_steers.clear();
                if let Some(done) = held_done.take() {
                    ended = event_tx.send(done).await.is_ok();
                }
                break 'main;
            },

            () = event_tx.closed() => break 'main,
        }
    }

    // A result still held when the CLI exited is the run's real end.
    if !ended
        && let Some(done) = held_done.take()
        && !event_tx.is_closed()
    {
        ended = event_tx.send(done).await.is_ok();
    }

    // Terminal bookkeeping: never end the stream without a Done unless the consumer already
    // hung up.
    if !event_tx.is_closed() && !ended {
        if interrupted {
            let _ = event_tx
                .send(ExecEvent::Done {
                    stop: StopReason::Interrupted,
                    result: None,
                    diagnostic: None,
                    reason: None,
                })
                .await;
        } else {
            // The CLI died mid-run. Its tools and MCP servers are traced while it can still be;
            // its exit status and last stderr lines land just after stdout closes, and say why.
            #[cfg(unix)]
            if let Some(pid) = child.id() {
                for pid in crate::process::descendants(pid).await {
                    if !torn_down.contains(&pid) {
                        torn_down.push(pid);
                    }
                }
            }
            let status = tokio::time::timeout(Duration::from_millis(200), child.wait())
                .await
                .ok()
                .and_then(Result::ok);
            stderr_tail.wait_closed().await;
            let _ = event_tx
                .send(ExecEvent::Done {
                    stop: StopReason::Error,
                    result: None,
                    diagnostic: Some(crate::redact_secrets(&crate::crash_message(
                        "claude",
                        status,
                        &stderr_tail,
                    ))),
                    reason: None,
                })
                .await;
        }
    }

    // Whatever is still open is refused: the run is over.
    deny_all(&mut requests, &mut stdin);

    // Its whole process group goes with it, and whatever was traced before, on every exit path,
    // not only a cancel; never while a login refresh is being saved.
    if matches!(child.try_wait(), Ok(None)) {
        refresh_lock::settle(&lock_dir, refresh_lock::SETTLE_MAX).await;
    }
    child.shutdown_with(torn_down, kill_grace).await;
    if let Some(handle) = escalation {
        handle.abort();
    }
    if let Some(handle) = stderr_task {
        handle.abort();
    }
}

/// How far the CLI has taken each stdin user message we wrote, by the uuid it was written with.
/// A `result` frame ends the RUN only when no message is still waiting to be taken up. A
/// waiting one makes the result a steer boundary (a `now` steer aborting the turn it
/// interrupts, or a message queued behind a turn that just finished) and that message's own turn
/// ends with its own result. Deciding this from the CLI's own lifecycle rather than from quiet
/// time: a held result released by a quiet timer marked the steered turn done mid-way through any
/// long tool call.
#[derive(Default)]
struct Commands {
    /// Uuid to how far it got. Ended commands are dropped.
    open: std::collections::HashMap<String, CommandState>,
    /// The CLI reports `command_lifecycle`. Older CLIs never do: their only confirmation is the
    /// replay, and the settle fallback covers the rest.
    reported: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CommandState {
    /// On stdin; the CLI has not acknowledged it yet.
    Written,
    /// The CLI read it and will start or cancel it.
    Queued,
    /// A turn took it up.
    Started,
}

impl Commands {
    /// Without lifecycle reports, how long a held result waits in silence for the replay that
    /// supersedes it.
    const UNREPORTED_SETTLE: Duration = Duration::from_secs(5);
    /// A lifecycle-reporting CLI acknowledges a message as soon as it reads it. A hold resting
    /// only on unacknowledged messages is a safety net against a message the CLI never
    /// reports, not a timing guess.
    const UNACKNOWLEDGED_SETTLE: Duration = Duration::from_secs(30);

    fn written(&mut self, id: String) {
        self.open.insert(id, CommandState::Written);
    }

    fn is_terminal(state: &str) -> bool {
        matches!(state, "completed" | "cancelled" | "canceled" | "failed")
    }

    /// Apply one `command_lifecycle` frame. True when a turn took the command up (`started`).
    fn lifecycle(&mut self, id: &str, state: &str) -> bool {
        self.reported = true;
        if Self::is_terminal(state) {
            self.open.remove(id);
            return false;
        }
        let next = match state {
            "started" => CommandState::Started,
            "queued" => CommandState::Queued,
            _ => return false,
        };
        if let Some(current) = self.open.get_mut(id)
            && *current != CommandState::Started
        {
            *current = next;
        }
        next == CommandState::Started
    }

    /// A message is written or queued but no turn has taken it up.
    fn waiting(&self) -> bool {
        self.open.values().any(|s| *s != CommandState::Started)
    }

    /// Drop every message not yet taken up.
    fn cancel_waiting(&mut self) {
        self.open.retain(|_, s| *s == CommandState::Started);
    }

    /// The quiet time after which a held result is released as the run's end; `None` = only the
    /// CLI's own lifecycle settles it.
    fn held_settle(&self) -> Option<Duration> {
        if !self.reported {
            Some(Self::UNREPORTED_SETTLE)
        } else if self.open.values().any(|s| *s == CommandState::Queued) {
            // A queued message always starts or is cancelled.
            None
        } else {
            Some(Self::UNACKNOWLEDGED_SETTLE)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reno_proto::{Effort, RunId};
    use serde_json::json;

    fn spec(access: AccessMode) -> ExecSpec {
        ExecSpec {
            run_id: RunId::from_suffix("01HZX9").unwrap(),
            inherit_user_config: false,
            prompt: "do it".into(),
            cwd: PathBuf::from("/work"),
            model: None,
            effort: None,
            access,
            sandbox: SandboxLevel::Unrestricted,
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

    fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
        let at = args.iter().position(|a| a == name)?;
        args.get(at + 1).map(String::as_str)
    }

    #[test]
    fn every_run_speaks_stream_json_and_routes_permissions_to_stdio() {
        let args = ClaudeExecutor::launch_args(&spec(AccessMode::Supervised));
        assert_eq!(flag(&args, "--input-format"), Some("stream-json"));
        assert_eq!(flag(&args, "--output-format"), Some("stream-json"));
        assert_eq!(flag(&args, "--permission-prompt-tool"), Some("stdio"));
        assert!(args.contains(&"--print".to_owned()));
        assert!(args.contains(&"--verbose".to_owned()));
    }

    #[test]
    fn access_modes_launch_with_their_permission_mode_and_never_a_skip_flag() {
        for (access, mode) in [
            (AccessMode::Supervised, "default"),
            (AccessMode::AutoAcceptEdits, "acceptEdits"),
            (AccessMode::FullAccess, "default"),
        ] {
            let args = ClaudeExecutor::launch_args(&spec(access));
            assert_eq!(flag(&args, "--permission-mode"), Some(mode), "{access:?}");
            assert!(
                !args.iter().any(|a| a.contains("skip-permissions")),
                "{access:?}: {args:?}"
            );
        }
    }

    #[test]
    fn a_bots_instructions_are_appended_to_the_system_prompt() {
        let none = ClaudeExecutor::launch_args(&spec(AccessMode::Supervised));
        assert_eq!(flag(&none, "--append-system-prompt"), None);
        assert_eq!(flag(&none, "--system-prompt"), None);
        let mut with = spec(AccessMode::Supervised);
        with.instructions = Some("Be terse.\nAnswer in French.".into());
        let args = ClaudeExecutor::launch_args(&with);
        assert_eq!(
            flag(&args, "--append-system-prompt"),
            Some("Be terse.\nAnswer in French.")
        );
        // Appended, never a replacement of Claude Code's own system prompt.
        assert_eq!(flag(&args, "--system-prompt"), None);
    }

    #[test]
    fn model_and_effort_come_from_the_spec() {
        let none = ClaudeExecutor::launch_args(&spec(AccessMode::Supervised));
        assert_eq!(flag(&none, "--model"), None);
        assert_eq!(flag(&none, "--effort"), None);
        let mut with = spec(AccessMode::Supervised);
        with.model = Some("claude-opus-5-5".into());
        with.effort = Some(Effort::High);
        let args = ClaudeExecutor::launch_args(&with);
        assert_eq!(flag(&args, "--model"), Some("claude-opus-5-5"));
        assert_eq!(flag(&args, "--effort"), Some("high"));
        with.effort = Some(Effort::Low);
        assert_eq!(
            flag(&ClaudeExecutor::launch_args(&with), "--effort"),
            Some("low")
        );
        with.effort = Some(Effort::Medium);
        assert_eq!(
            flag(&ClaudeExecutor::launch_args(&with), "--effort"),
            Some("medium")
        );
    }

    #[test]
    fn mcp_config_arg_spells_the_servers_the_way_the_cli_reads_them() {
        assert_eq!(mcp_servers_config_arg(&[]), None);
        let both: Value = serde_json::from_str(
            &mcp_servers_config_arg(&[
                server("reno", "/opt/reno/reno"),
                server("plugin-focus", "/data/plugins/focus/bin/server"),
            ])
            .unwrap(),
        )
        .unwrap();
        assert_eq!(both["mcpServers"]["reno"]["command"], "/opt/reno/reno");
        assert_eq!(both["mcpServers"]["reno"]["args"], json!(["mcp"]));
        assert_eq!(both["mcpServers"]["reno"]["env"]["RENO_RUN"], "r1");
        assert_eq!(
            both["mcpServers"]["plugin-focus"]["command"],
            "/data/plugins/focus/bin/server"
        );
    }

    #[test]
    fn reno_wins_a_name_clash_because_it_is_first() {
        let parsed: Value = serde_json::from_str(
            &mcp_servers_config_arg(&[server("reno", "/first"), server("reno", "/second")])
                .unwrap(),
        )
        .unwrap();
        assert_eq!(parsed["mcpServers"]["reno"]["command"], "/first");
    }

    fn args_and_env(spec: &ExecSpec) -> (Vec<String>, Vec<(String, Option<String>)>) {
        let command = ClaudeExecutor::build_command(std::path::Path::new("claude"), spec);
        let args = command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let env = command
            .as_std()
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        (args, env)
    }

    fn env_value<'a>(env: &'a [(String, Option<String>)], name: &str) -> Option<&'a str> {
        env.iter()
            .find(|(k, _)| k == name)
            .and_then(|(_, v)| v.as_deref())
    }

    #[test]
    fn a_run_loads_only_project_and_local_settings_unless_the_bot_opts_in() {
        let (args, env) = args_and_env(&spec(AccessMode::Supervised));
        assert_eq!(flag(&args, "--setting-sources"), Some("project,local"));
        // What the setting sources do not control: auto-memory and claude.ai connectors.
        assert_eq!(
            env_value(&env, "CLAUDE_CODE_DISABLE_AUTO_MEMORY"),
            Some("1")
        );
        assert_eq!(
            env_value(&env, "ENABLE_CLAUDEAI_MCP_SERVERS"),
            Some("false")
        );

        let mut opted = spec(AccessMode::Supervised);
        opted.inherit_user_config = true;
        let (args, env) = args_and_env(&opted);
        assert_eq!(flag(&args, "--setting-sources"), None);
        assert_eq!(env_value(&env, "CLAUDE_CODE_DISABLE_AUTO_MEMORY"), None);
        assert_eq!(env_value(&env, "ENABLE_CLAUDEAI_MCP_SERVERS"), None);
    }

    #[test]
    fn reno_s_own_mcp_server_rides_along_in_both_modes() {
        for inherit in [false, true] {
            let mut s = spec(AccessMode::Supervised);
            s.inherit_user_config = inherit;
            s.mcp = vec![server("reno", "reno")];
            let (args, _) = args_and_env(&s);
            assert!(args.contains(&"--mcp-config".to_owned()), "{args:?}");
            // Never `--strict-mcp-config`: it would drop the project's own `.mcp.json`.
            assert!(
                !args.contains(&"--strict-mcp-config".to_owned()),
                "{args:?}"
            );
        }
    }

    #[test]
    fn a_run_envs_own_value_cannot_switch_the_isolation_back_off() {
        let mut s = spec(AccessMode::Supervised);
        s.env
            .insert("CLAUDE_CODE_DISABLE_AUTO_MEMORY".into(), "0".into());
        let (_, env) = args_and_env(&s);
        assert_eq!(
            env_value(&env, "CLAUDE_CODE_DISABLE_AUTO_MEMORY"),
            Some("1")
        );
    }

    #[test]
    fn a_resumed_run_asks_the_cli_to_resume_the_session() {
        let mut resumed = spec(AccessMode::Supervised);
        resumed.resume = Some(reno_proto::SessionRef("sess-9".into()));
        let command = ClaudeExecutor::build_command(std::path::Path::new("claude"), &resumed);
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert!(args.contains(&"--resume=sess-9".to_owned()), "{args:?}");
        assert_eq!(
            command.as_std().get_current_dir(),
            Some(std::path::Path::new("/work"))
        );
    }

    #[test]
    fn a_runs_own_config_dir_wins() {
        let mut env = std::collections::BTreeMap::new();
        env.insert("HOME".to_owned(), "/home/cloud".to_owned());
        assert_eq!(config_root(&env), PathBuf::from("/home/cloud/.claude"));
        env.insert("CLAUDE_CONFIG_DIR".to_owned(), "/cfg".to_owned());
        assert_eq!(config_root(&env), PathBuf::from("/cfg"));
    }

    #[test]
    fn the_descriptor_lists_only_what_the_instance_will_honour() {
        let restricted = ClaudeExecutor::new().with_access_modes(vec![AccessMode::Supervised]);
        assert_eq!(
            restricted.descriptor().caps.access_modes,
            vec![AccessMode::Supervised]
        );
        // The versions are the driver's own, not whatever is installed.
        let cli = ClaudeExecutor::new().descriptor().cli_verified;
        assert_eq!(cli.min, MIN_CLI_VERSION);
        assert_eq!(cli.verified, VERIFIED_CLI_VERSION);
        assert!(
            semver::Version::parse(MIN_CLI_VERSION).unwrap()
                <= semver::Version::parse(VERIFIED_CLI_VERSION).unwrap()
        );
    }
}
