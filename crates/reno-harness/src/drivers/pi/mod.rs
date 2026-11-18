//! The native Pi executor: spawns the installed `pi` CLI as `pi --mode rpc` and speaks its JSONL
//! RPC (one JSON object per line each way, not JSON-RPC), and implements [`Executor`].
//!
//! VERSION PIN: [`MIN_CLI_VERSION`] and [`VERIFIED_CLI_VERSION`] are both 1.5.0, and what that
//! means is narrow. The wire was read from 1.5.0's own documentation (`docs/rpc.md`,
//! `rpc-commands.md`, `json.md`, `rpc-extension-ui.md`) and source (`dist/core/agent-session.js`,
//! `dist/modes/rpc/rpc-mode.js`, `dist/main.js`), and a real 1.5.0 was started in a throwaway
//! agent directory with `--no-session`, sent only `get_state`, `get_commands`,
//! `get_available_models` and `get_available_thinking_levels` (never a prompt, so no model was
//! asked anything), and had Reno's MCP bridge loaded through `--extension` against a stub MCP
//! server. A whole run (prompt, settle, steer, abort) has not been driven against a real Pi: the
//! live test (`tests/pi_live.rs`, `--ignored`, a local mock provider) does that.
//!
//! - `wire::pi` is sans-IO: frames become [`ExecEvent`]s there, dialogs become questions, a model
//!   catalog becomes `ModelInfo`s. This module only does the I/O.
//! - ONE RUN IS ONE `Done`. A Pi process serves one run: it is started with the run's flags,
//!   asked for its model and thinking level, given the prompt, and torn down when the run settles.
//! - A run ends at Pi's `agent_settled` (retries, compaction recovery and queued input are all
//!   behind it, so no timer decides), unless the prompt was `handled` by an extension (no run
//!   started for it): then one ordered `get_state` after the reply says whether the extension
//!   began work of its own. `agent_end` ends nothing.
//! - Steering is a `prompt` with `streamingBehavior: "steer"`: delivered after the current
//!   assistant turn's tool calls, before the next model call. If Pi has just gone idle it starts a
//!   run of its own for the message, which the run then also waits for. Pi's `set_steering_mode`
//!   is never sent (see below), so several steers reach the model one per turn.
//! - NEVER A SETTINGS WRITE. In 1.5.0 `set_steering_mode`, `set_follow_up_mode`,
//!   `set_auto_compaction`, `set_auto_retry`, `cycle_model` and `cycle_thinking_level` write
//!   `~/.pi/agent/settings.json`. The writer refuses them ([`rpc::Client::send`]) and a test
//!   drives a run to prove none is sent. `set_model` and `set_thinking_level` stay in the session
//!   transcript. The MCP bridge, the instructions and every file a flag names live in a scratch
//!   directory of the run.
//! - Extension dialogs (`select`, `confirm`, `input`, `editor`) become `NeedsInput`, answered
//!   through `ExecHandle::answer_input`. One that is declined, that Pi gives up on, or that is
//!   open when the run ends is dismissed. Pi has no tool-approval protocol: this executor honours
//!   full access only, like Grok and Devin, and refuses any other mode before spawning.
//! - Isolation from the owner's personal tools (ARCH-15): unless `inherit_user_config`, Pi is
//!   started with `--no-extensions --no-skills --no-prompt-templates --no-mcp`. Not covered: the
//!   owner's settings and credentials (they are how Pi signs in), the global context file
//!   `~/.pi/agent/AGENTS.md`, a project's `AGENTS.md` and `.pi/` (project trust is the owner's
//!   decision and RPC mode skips untrusted project resources).
//! - Process records: the Pi a run spawns is reported through `ExecControls::processes` at
//!   once. The short-lived probe Pi is bounded by its own timeout and reaped before it returns.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use reno_proto::{
    AccessMode, ExecEvent, ExecSpec, ExecutorCaps, ExecutorDescriptor, ExecutorId, HealthProbe,
    ModelCatalog, ModelInfo, SandboxLevel, SessionRef, SignedIn, Steering, StopReason, VersionGate,
    VersionRange,
};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::exec::{
    Answer, ExecAnswer, ExecControls, ExecError, Executor, OpenRequests, ProcessSink, Resolved,
    UnknownFrames, probe, require_access, require_sandbox,
};
use crate::proc::scratch::ScratchDir;
use crate::process::owned::Child as OwnedChild;
use crate::process::{Command, Stdio};
use crate::wire::pi::catalog;
use crate::wire::pi::dialog::{self, Dialog};
use crate::wire::pi::frames::{self, Disposition, Reply, State, string};
use crate::wire::pi::known;
use crate::wire::pi::launch::{self, Launch};
use crate::wire::pi::normalize::Normalizer;
use crate::{HarnessError, HarnessId};

mod mcp;
mod rpc;

/// The oldest Pi this driver runs. Checked: 1.5.0 only (see the module docs). A Pi older than
/// this is refused before a run instead of failing in the middle of one.
pub const MIN_CLI_VERSION: &str = "1.5.0";

/// The newest Pi this driver was checked against: 1.5.0, the one installed where it was written.
/// A newer one runs but is reported `Untested`.
pub const VERIFIED_CLI_VERSION: &str = "1.5.0";

/// The executor contract version this driver implements.
const CONTRACT_VERSION: u32 = 1;

/// The id this executor registers under.
pub const EXECUTOR_ID: &str = "pi";

static ID: LazyLock<ExecutorId> = LazyLock::new(|| match EXECUTOR_ID.parse() {
    Ok(id) => id,
    Err(error) => unreachable!("{EXECUTOR_ID} is a valid executor id: {error}"),
});

/// What the stream carries before the consumer reads.
const EVENT_CAPACITY: usize = 256;

/// How long Pi may take to start, take the model and thinking level and report its state.
/// Extensions and an MCP server can start cold.
const STARTUP_BUDGET: Duration = Duration::from_secs(120);

/// How long an ordered `get_state` the run's end waits on may take. Pi answers one at once, even
/// mid-run; silence past this is a wedged process, which fails the run instead of leaving it
/// working for ever.
const STATE_QUERY_TIMEOUT: Duration = Duration::from_secs(15);

/// Dialogs open on a person at once (ARCH-11). More are dismissed.
const MAX_OPEN_DIALOGS: usize = 16;

/// Frames kept while the startup queries run, to be handled once the run has started.
const MAX_BACKLOG: usize = 512;

/// The Pi CLI to launch: `PI_EXECUTABLE`, then our own PATH, the login-shell PATH and the known
/// install locations. Resolved per call: cheap after the snapshot is cached.
pub fn resolve_pi_executable() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("PI_EXECUTABLE").filter(|p| !p.is_empty()) {
        return crate::executable::validate_native_override(&PathBuf::from(p)).ok();
    }
    let mut extra = Vec::new();
    if let Some(home) = crate::executable::home_dir() {
        extra.push(home.join(".local").join("bin").join("pi"));
        extra.push(home.join(".npm-global").join("bin").join("pi"));
    }
    extra.push(PathBuf::from("/opt/homebrew/bin/pi"));
    extra.push(PathBuf::from("/usr/local/bin/pi"));
    crate::executable::find_on_paths("pi", extra)
}

fn resolve_executable(executable: Option<&Path>) -> Result<PathBuf, ExecError> {
    let not_installed = |error: HarnessError| ExecError::NotInstalled(error.to_string());
    if let Some(p) = executable {
        return crate::executable::validate_native_override(p).map_err(not_installed);
    }
    if let Some(p) = std::env::var_os("PI_EXECUTABLE")
        && !p.is_empty()
    {
        return crate::executable::validate_native_override(&PathBuf::from(p))
            .map_err(not_installed);
    }
    resolve_pi_executable().ok_or_else(|| {
        ExecError::NotInstalled(
            "pi (searched PATH, the login shell's PATH, ~/.local/bin, ~/.npm-global/bin, \
             /opt/homebrew/bin and /usr/local/bin; set PI_EXECUTABLE to override; install it \
             with `npm install -g @earendil-works/pi-coding-agent`)"
                .into(),
        )
    })
}

/// `Some(why)` when `version` is a Pi older than [`MIN_CLI_VERSION`]; an unknown version (a
/// wrapper, a dev build) is given the benefit of the doubt.
fn too_old_message(version: Option<&semver::Version>) -> Option<String> {
    let version = version?;
    let release = semver::Version::new(version.major, version.minor, version.patch);
    let min = semver::Version::parse(MIN_CLI_VERSION).ok()?;
    (release < min).then(|| {
        format!(
            "Pi {version} is too old; update it to {min} or newer \
             (npm install -g @earendil-works/pi-coding-agent@latest)"
        )
    })
}

/// The curated list a Pi that has not answered offers: whatever model Pi itself is configured
/// with.
fn default_models() -> Vec<ModelInfo> {
    vec![ModelInfo {
        id: "default".into(),
        display_name: Some("Pi default".into()),
        description: Some("The model configured in Pi".into()),
        efforts: Vec::new(),
        default_effort: None,
        options: Vec::new(),
    }]
}

/// Whether `model` names no model at all (the run uses Pi's own).
fn is_default_model(model: Option<&str>) -> bool {
    model.is_none_or(|m| m.is_empty() || m == "default")
}

/// The Pi executor. Construct with [`PiExecutor::new`]; tests point it at a fake Pi with
/// [`PiExecutor::with_executable`].
///
/// # Access modes
///
/// Pi asks for no approval before a tool call, so there is nothing for Reno to decide: the
/// executor honours [`AccessMode::FullAccess`] and the unrestricted sandbox only, and `start`
/// refuses anything else before spawning (ARCH-15), like Grok and Devin. Pi's own tool allowlist
/// (`--tools read,grep,find,ls`) would make a read-only mode, but it also applies to the tools of
/// MCP servers and extensions, and has not been checked to hold; it is not claimed.
pub struct PiExecutor {
    executable: Option<PathBuf>,
    /// Grace between asking Pi to abort and ending it.
    interrupt_grace: Duration,
    /// Grace between SIGTERM and SIGKILL.
    kill_grace: Duration,
    /// The CLI version `probe` last read, for the descriptor.
    cli_version: Arc<Mutex<Option<String>>>,
    /// The models Pi last listed, for the descriptor.
    models: Arc<Mutex<Option<Vec<ModelInfo>>>>,
    /// How long `probe` lets its short-lived Pi take in all.
    probe_budget: Duration,
    tracker: TaskTracker,
    unknown: UnknownFrames,
}

impl std::fmt::Debug for PiExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PiExecutor")
            .field("executable", &self.executable)
            .finish_non_exhaustive()
    }
}

impl Default for PiExecutor {
    fn default() -> Self {
        Self {
            executable: None,
            interrupt_grace: Duration::from_secs(5),
            kill_grace: Duration::from_secs(3),
            cli_version: Arc::new(Mutex::new(None)),
            models: Arc::new(Mutex::new(None)),
            probe_budget: probe::WIRE_BUDGET,
            tracker: TaskTracker::new(),
            unknown: UnknownFrames::new(),
        }
    }
}

impl PiExecutor {
    /// An executor that finds the CLI on PATH, in the login shell's PATH or in a known install
    /// location.
    pub fn new() -> Self {
        Self::default()
    }

    /// Use a fixed CLI binary instead of PATH and known-location resolution.
    #[must_use]
    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    /// Tune the abort and the SIGTERM to SIGKILL graces.
    #[must_use]
    pub fn with_graces(mut self, interrupt_grace: Duration, kill_grace: Duration) -> Self {
        self.interrupt_grace = interrupt_grace;
        self.kill_grace = kill_grace;
        self
    }

    /// Bound the Pi `probe` starts (tests shrink it; default 20 s).
    #[must_use]
    pub fn with_probe_budget(mut self, budget: Duration) -> Self {
        self.probe_budget = budget;
        self
    }

    /// Whether the CLI can be launched now.
    pub fn installed(&self) -> bool {
        resolve_executable(self.executable.as_deref()).is_ok()
    }

    /// The models Pi lists, and so whether it holds a credential for any provider
    /// (`get_available_models` lists only the models of providers Pi has one for). One
    /// short-lived `pi --mode rpc --no-session`, reaped before this returns, in a scratch
    /// session: no prompt is sent, no model is asked anything. It runs offline and without
    /// telemetry, so the probe makes no network call of its own.
    async fn query_models(
        &self,
        exe: &Path,
        budget: Duration,
    ) -> Result<Vec<ModelInfo>, HarnessError> {
        let mut cmd = Command::new(exe);
        crate::child_env::restrict(&mut cmd, crate::child_env::PI);
        cmd.args(launch::args(Launch {
            isolate: true,
            ..Launch::default()
        }))
        .arg("--no-session");
        crate::compose_child_path(&mut cmd, exe);
        cmd.env("PI_OFFLINE", "1")
            .env("PI_TELEMETRY", "0")
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = crate::process::owned::spawn_grouped(&mut cmd).map_err(|e| {
            crate::executable::spawn_error(e, None, || crate::executable::binary_hint(exe))
        })?;
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            child.shutdown(self.kill_grace).await;
            return Err(HarnessError::Protocol("pi child has no stdio".into()));
        };
        let mut transport = rpc::Transport::new(stdin, stdout);
        let ask = async {
            let id = transport.client.request(frames::get_available_models())?;
            loop {
                let frame = transport
                    .incoming
                    .recv()
                    .await
                    .unwrap_or_else(|| Err(HarnessError::Protocol("Pi disconnected".into())))?;
                let Some(reply) = Reply::parse(&frame).filter(|r| r.id == id) else {
                    // A startup extension's dialog would wait for ever: dismiss it.
                    if let Some(Ok(dialog)) = dialog::parse(&frame) {
                        let _ = transport.client.send(dialog::dismissed(&dialog));
                    }
                    continue;
                };
                return reply.outcome.map_err(HarnessError::Protocol);
            }
        };
        let result = tokio::time::timeout(budget, ask).await;
        transport.close_input();
        child.shutdown(self.kill_grace).await;
        let data = match result {
            Ok(inner) => inner?,
            Err(_) => return Err(HarnessError::Protocol("Pi probe timed out".into())),
        };
        let models = catalog::model_infos(&data);
        *self.models.lock().unwrap_or_else(PoisonError::into_inner) = Some(models.clone());
        Ok(models)
    }
}

#[async_trait]
impl Executor for PiExecutor {
    fn maintenance(&self) -> Option<crate::exec::maintenance::MaintenanceDecl> {
        crate::exec::maintenance::declaration(&HarnessId::Pi)
    }

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
            .unwrap_or_else(default_models);
        ExecutorDescriptor {
            id: ID.clone(),
            contract_version: CONTRACT_VERSION,
            adapter_version: match cli {
                Some(cli) => format!(
                    "reno-harness {} pi driver (CLI {cli})",
                    env!("CARGO_PKG_VERSION")
                ),
                None => format!("reno-harness {} pi driver", env!("CARGO_PKG_VERSION")),
            },
            cli_verified: VersionRange {
                min: MIN_CLI_VERSION.into(),
                verified: VERIFIED_CLI_VERSION.into(),
            },
            caps: ExecutorCaps {
                // The model and thinking level are set once, at the start of the run.
                live_effort: None,
                live_model_options: None,
                // A steering message lands after the current assistant turn's tool calls.
                steering: Steering::StepBoundary,
                resume: true,
                // Pi asks nothing before a tool call: full access, unrestricted, or refused.
                access_modes: vec![AccessMode::FullAccess],
                sandbox_levels: vec![SandboxLevel::Unrestricted],
                mcp_injection: true,
                structured_output: false,
                // Done is Pi's own `agent_settled`.
                deterministic_turn_end: true,
                plan_mode: false,
                models,
                model_switch: false,
                // Enforced at launch: see the module docs for what it leaves.
                isolates_user_config: true,
                model_aliases: vec![],
            },
        }
    }

    /// Where the CLI is, its version against the verified range, and, from one short-lived Pi
    /// with no prompt, which models it lists: a Pi with no model has no credential for any
    /// provider and is reported signed out. A listed model says a credential is configured, not
    /// that the provider still accepts it (that takes a model call), so a signed-in answer is
    /// "configured". When Pi cannot be asked the sign-in is `Unknown` and the models are the last
    /// live list, else the curated one.
    async fn probe(&self) -> HealthProbe {
        let overridden = self.executable.is_some()
            || std::env::var_os("PI_EXECUTABLE").is_some_and(|value| !value.is_empty());
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
                None => ModelCatalog::curated(default_models()),
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
            // A Pi this old is not launched.
            (SignedIn::Unknown, fallback())
        } else {
            match self.query_models(&exe, self.probe_budget).await {
                Ok(models) if models.is_empty() => (
                    SignedIn::No {
                        hint: "Pi has no provider credential: run `pi` and use /login, or set a \
                               provider API key (ANTHROPIC_API_KEY, OPENAI_API_KEY, ...)"
                            .into(),
                    },
                    ModelCatalog::live(models),
                ),
                Ok(models) => (SignedIn::Yes { account: None }, ModelCatalog::live(models)),
                Err(error) => {
                    tracing::debug!(%error, "the pi probe failed");
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
        require_access(spec.access, &[AccessMode::FullAccess])?;
        require_sandbox(spec.sandbox, &[SandboxLevel::Unrestricted])?;
        if spec.output_schema.is_some() {
            return Err(ExecError::UnsupportedOutputSchema);
        }
        if let Some(session) = &spec.resume
            && !launch::valid_session_id(&session.0)
        {
            return Err(ExecError::InvalidSpec(format!(
                "{:?} is not a Pi session id",
                session.0
            )));
        }
        if spec.cwd.to_str().is_none() {
            return Err(ExecError::InvalidSpec("cwd is not valid UTF-8".into()));
        }
        let exe = resolve_executable(self.executable.as_deref())?;
        // Pin the physical release for this run: an installer may move the current symlink.
        let exe = exe
            .canonicalize()
            .map_err(|error| ExecError::NotInstalled(format!("{}: {error}", exe.display())))?;
        // A fixed test executable is never version-probed.
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
            // Neither a model switch nor a live config change is claimed: not read.
            ..
        } = controls;

        let launched = launch_pi(&self.tracker, &exe, &spec, &processes).await;
        let pi = match launched {
            Ok(pi) => pi,
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

        let (event_tx, event_rx) = mpsc::channel::<ExecEvent>(EVENT_CAPACITY);
        let run = Run {
            pi,
            spec,
            event_tx,
            steering,
            answers,
            cancel,
            interrupt_grace: self.interrupt_grace,
            kill_grace: self.kill_grace,
            unknown: self.unknown.clone(),
        };
        self.tracker.spawn(run.drive());

        Ok(futures::stream::unfold(event_rx, |mut rx| async move {
            rx.recv().await.map(|event| (event, rx))
        })
        .boxed())
    }

    async fn shutdown(&self) {
        self.tracker.close();
        self.tracker.wait().await;
    }

    fn unknown_frames(&self) -> std::collections::BTreeMap<String, u64> {
        self.unknown.snapshot()
    }
}

// ---------------------------------------------------------------------------
// The Pi process
// ---------------------------------------------------------------------------

/// One `pi --mode rpc` process and its pipe.
struct Pi {
    child: OwnedChild,
    transport: rpc::Transport,
    stderr_tail: crate::StderrTail,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
    /// The run's scratch directory: the bridge, its config and the instructions. Removed with it.
    _scratch: Option<ScratchDir>,
    /// When the process was seen to have exited; frames it left behind are still read until then.
    exit_deadline: Option<Instant>,
}

impl Drop for Pi {
    fn drop(&mut self) {
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }
}

impl Pi {
    /// The next frame, in stdout order. An error when Pi's stdout closed or it exited and
    /// everything it had written is read: a descendant may keep stdout open, so EOF alone
    /// cannot govern liveness.
    async fn next_frame(&mut self) -> Result<Value, HarnessError> {
        loop {
            tokio::select! {
                biased;
                frame = self.transport.incoming.recv() => {
                    return frame.unwrap_or_else(|| Err(HarnessError::Protocol("Pi disconnected".into())));
                }
                () = async {
                    match self.exit_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => return Err(HarnessError::Protocol("Pi exited".into())),
                status = self.child.wait(), if self.exit_deadline.is_none() => {
                    status?;
                    self.exit_deadline = Some(Instant::now() + Duration::from_millis(150));
                }
            }
        }
    }

    /// What to say when Pi went away under a run: its exit status and last stderr lines.
    async fn crash_report(&mut self) -> String {
        self.stderr_tail.wait_closed().await;
        let status = match tokio::time::timeout(Duration::from_millis(500), self.child.wait()).await
        {
            Ok(Ok(status)) => Some(status),
            _ => self.child.try_wait().ok().flatten(),
        };
        crate::crash_message("pi", status, &self.stderr_tail)
    }
}

async fn launch_pi(
    tracker: &TaskTracker,
    exe: &Path,
    spec: &ExecSpec,
    processes: &ProcessSink,
) -> Result<Pi, std::io::Error> {
    let scratch = ScratchDir::new("pi")?;
    let instructions = match spec
        .instructions
        .as_deref()
        .filter(|i| !i.trim().is_empty())
    {
        Some(text) => Some(mcp::write_private(
            scratch.path(),
            "instructions.md",
            text.as_bytes(),
        )?),
        None => None,
    };
    let bridge = mcp::install(scratch.path(), &spec.mcp)?;
    let mut cmd = Command::new(exe);
    crate::child_env::restrict(&mut cmd, crate::child_env::PI);
    cmd.args(launch::args(Launch {
        resume: spec.resume.as_ref().map(|s| s.0.as_str()),
        isolate: !spec.inherit_user_config,
        instructions: instructions.as_deref(),
        bridge: bridge.as_ref().map(|b| b.extension.as_path()),
    }));
    crate::compose_child_path(&mut cmd, exe);
    // The allowlisted environment of the run (the bot's secrets, Reno's `RENO_*` run variables).
    crate::apply_run_env(&mut cmd, &spec.env);
    if let Some(bridge) = &bridge {
        cmd.env(mcp::CONFIG_ENV, &bridge.config);
    }
    if !spec.cwd.as_os_str().is_empty() {
        cmd.current_dir(&spec.cwd);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = crate::process::owned::spawn_grouped(&mut cmd).map_err(|e| {
        let cwd = (!spec.cwd.as_os_str().is_empty()).then_some(spec.cwd.as_path());
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
    #[cfg(not(unix))]
    let _ = processes;
    let missing = |what: &str| std::io::Error::other(format!("pi child has no {what}"));
    let stdin = child.stdin.take().ok_or_else(|| missing("stdin"))?;
    let stdout = child.stdout.take().ok_or_else(|| missing("stdout"))?;
    let stderr_tail = crate::StderrTail::for_run(&spec.env);
    let stderr_task = child.stderr.take().map(|stderr| {
        let tail = stderr_tail.clone();
        tracker.spawn(async move {
            let mut lines = crate::lossy_lines::lossy_lines(stderr);
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "reno_harness::pi", "stderr: {}", tail.scrub(&line));
                tail.push(&line);
            }
            tail.close();
        })
    });
    Ok(Pi {
        child,
        transport: rpc::Transport::new(stdin, stdout),
        stderr_tail,
        stderr_task,
        _scratch: Some(scratch),
        exit_deadline: None,
    })
}

// ---------------------------------------------------------------------------
// One run
// ---------------------------------------------------------------------------

/// What a command still waiting for its reply is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    /// The task prompt.
    Prompt,
    /// A steering message.
    Steer,
    /// The ordered `get_state` that closes a run Pi did not say it started, or re-checks one
    /// after a compaction or retry.
    Barrier,
    /// `abort`.
    Abort,
}

struct Run {
    pi: Pi,
    spec: ExecSpec,
    event_tx: mpsc::Sender<ExecEvent>,
    steering: mpsc::Receiver<String>,
    answers: mpsc::Receiver<ExecAnswer>,
    cancel: CancellationToken,
    interrupt_grace: Duration,
    kill_grace: Duration,
    unknown: UnknownFrames,
}

/// How the part of a run before its prompt went.
enum Startup {
    Ready(State, Vec<Value>),
    Failed(String),
    Cancelled,
    ConsumerGone,
}

impl Run {
    /// The whole run: start, play, end exactly once, tear down.
    async fn drive(mut self) {
        let cancel = self.cancel.clone();
        let startup = tokio::select! {
            () = cancel.cancelled() => Startup::Cancelled,
            result = tokio::time::timeout(STARTUP_BUDGET, self.start_up()) => match result {
                Ok(startup) => startup,
                Err(_) => Startup::Failed("Pi startup timed out".into()),
            },
        };
        // Everything Pi started goes with it, whichever way this ends: traced before anything
        // can reparent it.
        #[cfg(unix)]
        let tree = match self.pi.child.id() {
            Some(pid) => crate::process::descendants(pid).await,
            None => Vec::new(),
        };
        #[cfg(not(unix))]
        let tree = Vec::new();
        let (stop, result, diagnostic) = match startup {
            Startup::Ready(state, backlog) => {
                let mut player = Player::new(&mut self, state);
                player.play(backlog).await
            }
            Startup::Failed(why) => (StopReason::Error, None, Some(why)),
            Startup::Cancelled => (StopReason::Interrupted, None, None),
            Startup::ConsumerGone => {
                self.teardown(tree).await;
                return;
            }
        };
        let done = ExecEvent::Done {
            stop,
            result,
            diagnostic: diagnostic.map(|d| crate::redact_secrets(&d)),
            reason: None,
        };
        let _ = self.event_tx.send(done).await;
        self.teardown(tree).await;
    }

    /// Ask Pi for its model and thinking level and its state; say what the run starts as.
    async fn start_up(&mut self) -> Startup {
        let mut backlog = Vec::new();
        match self.set_up(&mut backlog).await {
            Ok(state) => {
                let started = ExecEvent::Started {
                    session: SessionRef(state.session_id.clone()),
                    model: state.model.clone().unwrap_or_else(|| "default".into()),
                };
                if self.event_tx.send(started).await.is_err() {
                    return Startup::ConsumerGone;
                }
                // `--session-id` opens the session with that id or starts one with it: tell the
                // person when it was the second, since the history they resume is gone.
                if let Some(wanted) = &self.spec.resume
                    && state.message_count == 0
                    && state
                        .session_file
                        .as_deref()
                        .is_some_and(|file| !Path::new(file).exists())
                {
                    let note = ExecEvent::Text {
                        text: format!(
                            "Pi did not find session {}; starting a new session without the \
                             previous context.\n",
                            wanted.0
                        ),
                    };
                    if self.event_tx.send(note).await.is_err() {
                        return Startup::ConsumerGone;
                    }
                }
                Startup::Ready(state, backlog)
            }
            Err(why) => Startup::Failed(why),
        }
    }

    async fn set_up(&mut self, backlog: &mut Vec<Value>) -> Result<State, String> {
        if !is_default_model(self.spec.model.as_deref()) {
            let wanted = self.spec.model.clone().unwrap_or_default();
            let models = self.query(frames::get_available_models(), backlog).await?;
            let (provider, id) = catalog::resolve_model(&models, &wanted)?;
            self.query(frames::set_model(&provider, &id), backlog)
                .await?;
        }
        if let Some(level) = catalog::level_for(self.spec.effort, &self.spec.model_options) {
            self.query(frames::set_thinking_level(level), backlog)
                .await?;
        }
        let state = self.query(frames::get_state(), backlog).await?;
        let state =
            State::parse(&state).ok_or_else(|| "Pi get_state omitted sessionId".to_owned())?;
        if let Some(wanted) = &self.spec.resume
            && state.session_id != wanted.0
        {
            return Err("Pi resumed a different session".into());
        }
        Ok(state)
    }

    /// One command and its reply. A frame that is not the reply is kept for the run, except a
    /// dialog: no run is listening yet, so it is dismissed.
    async fn query(&mut self, command: Value, backlog: &mut Vec<Value>) -> Result<Value, String> {
        let id = self
            .pi
            .transport
            .client
            .request(command)
            .map_err(|e| e.to_string())?;
        loop {
            let frame = match self.pi.next_frame().await {
                Ok(frame) => frame,
                Err(error) => {
                    let crash = self.pi.crash_report().await;
                    return Err(format!("{error}: {crash}"));
                }
            };
            if let Some(reply) = Reply::parse(&frame)
                && reply.id == id
            {
                return reply.outcome;
            }
            if frame["type"] == "extension_ui_request" {
                if let Some(parsed) = dialog::parse(&frame) {
                    let answer = match parsed {
                        Ok(dialog) => dialog::dismissed(&dialog),
                        Err(response) => response,
                    };
                    let _ = self.pi.transport.client.send(answer);
                }
            } else if backlog.len() < MAX_BACKLOG {
                backlog.push(frame);
            }
        }
    }

    /// End Pi and everything it started: close its stdin (its own orderly shutdown), give it a
    /// moment, then SIGTERM and SIGKILL to the process group and the descendants traced earlier.
    async fn teardown(&mut self, tree: Vec<crate::process::Tracked>) {
        self.pi.transport.close_input();
        let _ = tokio::time::timeout(
            self.kill_grace.min(Duration::from_secs(1)),
            self.pi.child.wait(),
        )
        .await;
        crate::shutdown_agent(&mut self.pi.child, tree, self.kill_grace).await;
        if let Some(task) = self.pi.stderr_task.take() {
            task.abort();
        }
    }
}

/// The run after it started: what is open, what is waiting, and when it is over.
struct Player<'a> {
    run: &'a mut Run,
    norm: Normalizer,
    pending: HashMap<String, Pending>,
    /// Dialogs open on a person.
    dialogs: OpenRequests<(), Dialog>,
    /// When Pi gives up on each open dialog by itself.
    deadlines: Vec<(Instant, String)>,
    /// Pi is running something for this run: it said it started or queued a prompt, or an
    /// `agent_start` came, and no `agent_settled` since.
    open: bool,
    /// The task prompt was answered.
    answered: bool,
    /// The task prompt was refused.
    refused: Option<String>,
    interrupted: bool,
    /// The last barrier found Pi busy with no settle guaranteed to follow: look again when a
    /// compaction or retry ends.
    recheck: bool,
    barrier_deadline: Option<Instant>,
    steering_open: bool,
    answers_open: bool,
}

type Ended = (StopReason, Option<String>, Option<String>);

impl<'a> Player<'a> {
    fn new(run: &'a mut Run, state: State) -> Self {
        let mut norm = Normalizer::new();
        norm.set_window(state.context_window);
        Self {
            run,
            norm,
            pending: HashMap::new(),
            dialogs: OpenRequests::new(),
            deadlines: Vec::new(),
            open: false,
            answered: false,
            refused: None,
            interrupted: false,
            recheck: false,
            barrier_deadline: None,
            steering_open: true,
            answers_open: true,
        }
    }

    fn client(&self) -> &rpc::Client {
        &self.run.pi.transport.client
    }

    async fn emit(&self, event: ExecEvent) -> bool {
        self.run.event_tx.send(event).await.is_ok()
    }

    /// Play the run from the prompt to its end.
    async fn play(&mut self, backlog: Vec<Value>) -> Ended {
        for frame in backlog {
            if self.frame(frame).await.is_err() {
                return self.gone().await;
            }
        }
        match self.client().request(frames::prompt(&self.run.spec.prompt)) {
            Ok(id) => {
                self.pending.insert(id, Pending::Prompt);
            }
            Err(error) => return (StopReason::Error, None, Some(error.to_string())),
        }
        loop {
            if let Some(ended) = self.ended() {
                return ended;
            }
            let next_deadline = self.next_deadline();
            let expiry = async {
                match next_deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            let barrier = async {
                match self.barrier_deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                frame = self.run.pi.next_frame() => match frame {
                    Ok(frame) => {
                        if self.frame(frame).await.is_err() {
                            return self.gone().await;
                        }
                    }
                    Err(_) => return self.gone().await,
                },
                steer = self.run.steering.recv(), if self.steering_open && !self.interrupted => {
                    match steer {
                        Some(text) => self.steer(&text),
                        None => self.steering_open = false,
                    }
                }
                answer = self.run.answers.recv(), if self.answers_open => match answer {
                    Some(answer) => self.answer(answer),
                    None => self.answers_open = false,
                },
                () = expiry => self.expire().await,
                () = barrier => {
                    return (
                        StopReason::Error,
                        None,
                        Some(format!(
                            "Pi did not answer a state query within {}s",
                            STATE_QUERY_TIMEOUT.as_secs()
                        )),
                    );
                }
                () = self.run.cancel.cancelled(), if !self.interrupted => {
                    self.interrupt().await;
                    return self.dismiss_all_and(self.norm.ending(true)).await;
                }
                () = self.run.event_tx.closed() => {
                    self.dismiss_all();
                    return (StopReason::Interrupted, None, None);
                }
            }
        }
    }

    /// Whether the run is over, and how. Over means Pi answered the prompt, has nothing running
    /// for it, owes no other reply and has no steering message waiting to be handed over.
    fn ended(&mut self) -> Option<Ended> {
        if let Some(why) = &self.refused {
            return Some((StopReason::Error, None, Some(why.clone())));
        }
        let waiting = self
            .pending
            .values()
            .any(|p| matches!(p, Pending::Prompt | Pending::Steer | Pending::Barrier));
        if !self.answered || self.open || waiting {
            return None;
        }
        // A steering message queued while the run settled starts a run of its own on this Pi.
        let mut handed = false;
        while self.steering_open
            && let Ok(text) = self.run.steering.try_recv()
        {
            self.steer(&text);
            handed = true;
        }
        if handed {
            return None;
        }
        let ending = self.norm.ending(self.interrupted);
        self.dismiss_all();
        Some((ending.stop, ending.result, ending.diagnostic))
    }

    /// Pi went away, or its pipe broke, before the run ended.
    async fn gone(&mut self) -> Ended {
        self.dismiss_all();
        if self.interrupted {
            return (StopReason::Interrupted, None, None);
        }
        let crash = self.run.pi.crash_report().await;
        let held = self.norm.ending(false);
        let diagnostic = match held.diagnostic {
            Some(error) => format!("{error}\n\n{crash}"),
            None => crash,
        };
        (StopReason::Error, None, Some(diagnostic))
    }

    async fn dismiss_all_and(&mut self, ending: crate::wire::pi::normalize::Ending) -> Ended {
        self.dismiss_all();
        (ending.stop, ending.result, ending.diagnostic)
    }

    fn steer(&mut self, text: &str) {
        match self.client().request(frames::steer(text)) {
            Ok(id) => {
                self.pending.insert(id, Pending::Steer);
            }
            Err(error) => tracing::warn!(target: "reno_harness::pi", "steer not sent: {error}"),
        }
    }

    fn send_barrier(&mut self) {
        match self.client().request(frames::get_state()) {
            Ok(id) => {
                self.pending.insert(id, Pending::Barrier);
                self.barrier_deadline = Some(Instant::now() + STATE_QUERY_TIMEOUT);
            }
            Err(error) => {
                self.refused = Some(error.to_string());
            }
        }
    }

    /// A dialog's answer from a person.
    fn answer(&mut self, answer: ExecAnswer) {
        let request_id = answer.request_id.clone();
        match self.dialogs.resolve(answer) {
            Resolved::Input(dialog, answers) => {
                self.deadlines.retain(|(_, id)| *id != request_id);
                let label = answers
                    .iter()
                    .find(|a| a.question_id == dialog::QUESTION_ID)
                    .and_then(|a| a.labels.first())
                    .map(String::as_str);
                let _ = self.client().send(dialog::response(&dialog, label));
            }
            Resolved::Decision(..) | Resolved::WrongKind | Resolved::Unknown => {
                tracing::debug!(target: "reno_harness::pi", "answer for {request_id} matches no open question");
            }
        }
    }

    fn dismiss_all(&mut self) {
        self.deadlines.clear();
        for (_, open) in self.dialogs.drain() {
            if let crate::exec::Open::Input(dialog) = open {
                let _ = self.client().send(dialog::dismissed(&dialog));
            }
        }
    }

    fn next_deadline(&self) -> Option<Instant> {
        self.deadlines.iter().map(|(at, _)| *at).min()
    }

    /// Pi gave up waiting on dialogs by itself: say so, and drop them.
    async fn expire(&mut self) {
        let now = Instant::now();
        let due: Vec<String> = self
            .deadlines
            .iter()
            .filter(|(at, _)| *at <= now)
            .map(|(_, id)| id.clone())
            .collect();
        self.deadlines.retain(|(at, _)| *at > now);
        for request_id in due {
            let declined = ExecAnswer {
                request_id: request_id.clone(),
                answer: Answer::Input(Vec::new()),
            };
            if let Resolved::Input(..) = self.dialogs.resolve(declined) {
                self.emit(ExecEvent::InputExpired { request_id }).await;
            }
        }
    }

    /// Stop the run: drop the queues, abort, and give Pi its grace to settle.
    async fn interrupt(&mut self) {
        self.interrupted = true;
        self.dismiss_all();
        let _ = self.client().request(frames::clear_queue());
        let abort = match self.client().request(frames::abort()) {
            Ok(id) => id,
            Err(_) => return,
        };
        self.pending.insert(abort, Pending::Abort);
        let grace = self.run.interrupt_grace;
        let _ = tokio::time::timeout(grace, async {
            while self.pending.values().any(|p| *p == Pending::Abort) || self.open {
                match self.run.pi.next_frame().await {
                    Ok(frame) => {
                        if self.frame(frame).await.is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        })
        .await;
    }

    /// One frame from Pi. `Err` when the consumer is gone.
    async fn frame(&mut self, frame: Value) -> Result<(), ()> {
        if let Some(reply) = Reply::parse(&frame) {
            self.reply(reply).await;
            return Ok(());
        }
        let kind = string(&frame, "type");
        if kind == "extension_ui_request" {
            return self.ui_request(&frame).await;
        }
        let events = self.norm.map(&frame);
        for kind in self.norm.take_unknown() {
            self.run.unknown.note(&kind);
        }
        for event in events {
            if !self.emit(event).await {
                return Err(());
            }
        }
        match kind {
            "agent_start" => self.open = true,
            "agent_settled" => {
                self.open = false;
                self.recheck = false;
            }
            // Compaction and automatic retry can end with no settle: look again.
            "compaction_end" | "auto_retry_end"
                if self.open && std::mem::take(&mut self.recheck) =>
            {
                self.send_barrier();
            }
            _ => {}
        }
        Ok(())
    }

    async fn reply(&mut self, reply: Reply) {
        let Some(pending) = self.pending.remove(&reply.id) else {
            return;
        };
        match pending {
            Pending::Prompt => match reply.outcome {
                Err(error) => self.refused = Some(error),
                Ok(data) => {
                    self.answered = true;
                    self.after_prompt(&data);
                }
            },
            Pending::Steer => match reply.outcome {
                Err(error) => {
                    // Pi refused the message, not the run: say so where the person reads.
                    self.emit(ExecEvent::Text {
                        text: format!("\n[Pi did not take the steering message: {error}]\n"),
                    })
                    .await;
                }
                Ok(data) => self.after_prompt(&data),
            },
            Pending::Barrier => {
                self.barrier_deadline = None;
                match reply.outcome.map(|data| State::parse(&data)) {
                    Ok(Some(state)) => {
                        self.norm.set_window(state.context_window);
                        // Busy with no settle guaranteed: wait, and look again when a
                        // compaction or retry ends.
                        self.open = state.busy();
                        self.recheck = state.busy();
                    }
                    Ok(None) | Err(_) => {
                        self.refused = Some("Pi get_state omitted sessionId".into());
                    }
                }
            }
            Pending::Abort => {}
        }
    }

    /// What Pi's disposition of a prompt means for the run.
    fn after_prompt(&mut self, data: &Value) {
        match Disposition::of(data) {
            Disposition::Started | Disposition::Queued => self.open = true,
            // An extension consumed it, so no run began for it: ask once, in order, whether the
            // extension began work of its own.
            Disposition::Handled | Disposition::Unknown => self.send_barrier(),
        }
    }

    async fn ui_request(&mut self, frame: &Value) -> Result<(), ()> {
        let Some(parsed) = dialog::parse(frame) else {
            let method = string(frame, "method");
            if !known::FIRE_AND_FORGET_METHODS.contains(&method) {
                self.run.unknown.note(&format!("ui:{method}"));
            }
            return Ok(());
        };
        let dialog = match parsed {
            Ok(dialog) => dialog,
            Err(response) => {
                let _ = self.client().send(response);
                return Ok(());
            }
        };
        if self.dialogs.len() >= MAX_OPEN_DIALOGS || self.interrupted {
            let _ = self.client().send(dialog::dismissed(&dialog));
            return Ok(());
        }
        let ask = ExecEvent::NeedsInput {
            request_id: dialog.request_id.clone(),
            questions: vec![dialog.question.clone()],
        };
        if let Some(timeout) = dialog.timeout {
            self.deadlines
                .push((Instant::now() + timeout, dialog.request_id.clone()));
        }
        // A request id Pi reuses replaces the first: nothing is left waiting on it.
        self.dialogs.open_input(dialog.request_id.clone(), dialog);
        if self.emit(ask).await {
            Ok(())
        } else {
            Err(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pi_older_than_the_checked_one_is_refused_and_an_unknown_version_is_not() {
        let old = semver::Version::new(1, 4, 9);
        assert!(too_old_message(Some(&old)).is_some_and(|m| m.contains("1.5.0")));
        let pre =
            semver::Version::parse("1.5.0-rc.1").unwrap_or_else(|_| semver::Version::new(0, 0, 0));
        assert!(
            too_old_message(Some(&pre)).is_none(),
            "a pre-release counts as its release"
        );
        assert!(too_old_message(Some(&semver::Version::new(1, 5, 0))).is_none());
        assert!(too_old_message(Some(&semver::Version::new(2, 0, 0))).is_none());
        assert!(too_old_message(None).is_none());
    }

    #[test]
    fn the_default_model_is_no_model() {
        assert!(is_default_model(None));
        assert!(is_default_model(Some("")));
        assert!(is_default_model(Some("default")));
        assert!(!is_default_model(Some("anthropic/claude-opus-4-8")));
    }

    #[test]
    fn the_descriptor_claims_only_what_the_driver_does() {
        let caps = PiExecutor::new().descriptor().caps;
        assert_eq!(caps.access_modes, [AccessMode::FullAccess]);
        assert_eq!(caps.sandbox_levels, [SandboxLevel::Unrestricted]);
        assert_eq!(caps.steering, Steering::StepBoundary);
        assert!(caps.resume && caps.mcp_injection && caps.deterministic_turn_end);
        assert!(!caps.structured_output && !caps.plan_mode && !caps.model_switch);
        assert!(caps.live_effort.is_none() && caps.live_model_options.is_none());
    }
}
