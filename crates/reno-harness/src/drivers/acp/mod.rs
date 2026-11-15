//! The native ACP executor: spawns an Agent Client Protocol agent (JSON-RPC 2.0 over stdio,
//! protocol v1) and implements [`Executor`] directly.
//!
//! One [`AcpExecutor`] serves one agent. The agents built ground-up on ACP are Grok
//! ([`AcpExecutor::grok`], `grok agent stdio`), Devin ([`AcpExecutor::devin`], `devin acp`),
//! Hermes ([`AcpExecutor::hermes`], `hermes acp`), fx ([`AcpExecutor::fx`], `fx acp`) and
//! Antigravity ([`AcpExecutor::antigravity`], Google's `agy_acp_server`, installed from its pinned
//! release archive), plus any agent added from the ACP registry
//! ([`AcpExecutor::registry`]). Adapter-mediated ACP for Claude and Codex is not used: the
//! adapters hold prompt turns open for background work the CLIs themselves settle eagerly, which
//! manufactures done-status bugs the native wires do not have. Each executor registers under its
//! own id: `acp.grok`, `acp.devin`, `acp.hermes`, `acp.fx`, `acp.antigravity`, and `acp.<registry
//! id>`.
//!
//! - `initialize` (protocolVersion 1, fs/terminal capabilities declined) then `session/new` with
//!   `mcpServers` from [`ExecSpec::mcp`], or `session/load` (`session/resume` where the agent
//!   prefers it) with a fresh-session fallback when [`ExecSpec::resume`] names a session the agent
//!   no longer has; replayed history during a load is dropped.
//! - `session/prompt` owns the turn: its response's `stopReason` ends it (see
//!   `wire::acp::normalize::stop`). ONE RUN IS ONE `Done`: the run ends with the first settled
//!   turn that has no steering prompt waiting, and the agent process and everything it started
//!   are torn down with it.
//! - `session/update` notifications normalize per `wire::acp::normalize`; `wire/acp` stays pure.
//! - Steering: an agent advertising `_session/steering` takes a steering prompt mid-turn. Any
//!   other agent queues it: a [`Steering::StepBoundary`] agent (Grok, Devin) has its generation
//!   preempted by `session/cancel` once no tool is open and continues with the steer, and a
//!   [`Steering::TurnBoundary`] agent takes it as the next `session/prompt` when the turn ends.
//!   Neither emits a `Done` for the turn it continues.
//! - Cancel: `session/cancel`, escalating SIGTERM then SIGKILL; every open request is answered
//!   `cancelled` first, one that arrives afterwards at once, and the stream ends with
//!   `Done { stop: Interrupted }`.
//! - Process records: every child this module spawns for a run is reported through
//!   `ExecControls::processes` as soon as it exists, so a crash of the daemon leaves a row for
//!   the orphan sweep (ARCHITECTURE 17.3).
//!
//! # Approvals
//!
//! A `session/request_permission` becomes `NeedsApproval` with a structured subject read from
//! the request's `toolCall` (see `wire::acp::permission` for the kind table), and a decision
//! selects the request's `allow_once` or `reject_once` option: never `allow_always`. A request
//! whose options are not all allow/reject kinds is a question and becomes `NeedsInput`.
//! Nothing is parsed from prose.
//!
//! # Access modes
//!
//! Each agent honors only some access modes (the `access_modes` of its spec, declared in the
//! descriptor); [`Executor::start`] refuses any other with [`ExecError::UnsupportedAccess`]
//! before anything is resolved or spawned (ARCH-15). A request is never run with more access than asked.
//!
//! | Agent | `Supervised` | `AutoAcceptEdits` | `FullAccess` | Why |
//! |---|---|---|---|---|
//! | `acp.grok` | no | no | yes | its Auto mode approves through its own classifier and asks only about what it would deny |
//! | `acp.devin` | no | no | yes | only its `bypass` mode value is known; which mode asks before every command is not |
//! | `acp.hermes` | no | no | yes | it advertises no session mode, and nothing shows it asking |
//! | `acp.fx` | yes | no | yes | pinned to its `ask` mode, so every permission reaches Reno |
//! | `acp.antigravity` | yes | yes | yes | its modes `default` (asks), `auto_edit` and `yolo` are known |
//! | `acp.<registry id>` | yes | no | yes | its permission requests reach Reno; no mode that auto-accepts edits is known |
//!
//! For an agent that honors the mode, what is answered here and what reaches Reno is one table
//! (`wire::acp::permission::allowed_without_asking`):
//!
//! | `toolCall.kind` | `FullAccess` | `AutoAcceptEdits` | `Supervised` |
//! |---|---|---|---|
//! | `read`, `search`, `think` | answered here | answered here | answered here |
//! | `edit`, `delete`, `move` | answered here | answered here | `NeedsApproval` |
//! | anything else, or no kind | answered here | `NeedsApproval` | `NeedsApproval` |
//!
//! "Answered here" is `allow_once`, never `allow_always`; looking around (`read`, `search`,
//! `think`) is allowed in every mode, and that is the only place it
//! is decided. In `FullAccess` the session is also switched to the agent's own no-prompts mode
//! where it advertises one, and any other mode keeps an asking mode where it advertises one. A
//! run that asks never starts in a session that approves on its own: an agent that rejects the
//! switch to an asking mode, or has none to switch to while opening in a no-prompts mode, fails
//! the run before it prompts. No mode is widened mid-run.
//!
//! # Models
//!
//! An explicit [`ExecSpec::model`] is strict for every agent: one the session does not
//! advertise, or refuses to switch to, fails the run before it prompts, so the agent's default
//! never runs in its place. The descriptor declares only the models the driver has seen the
//! agent advertise (Grok's and Antigravity's); any model a session advertises can be asked for.

mod antigravity_paths;
mod config;
mod install;
mod launch;
#[cfg(test)]
pub(crate) use launch::find_on_paths;
mod session;
mod signin;
mod spec;

pub(crate) use install::running_command_lines;
pub use install::{
    ANTIGRAVITY_REGISTRY_URL, AntigravityRelease, antigravity_build_version, antigravity_release,
    can_install, install_antigravity_release, install_harness, is_managed_antigravity_server,
    prune_superseded_antigravity_installs,
};
pub use signin::{AuthMethodInfo, NEEDS_TERMINAL, RegistrySignIn, SignInOptions, SignInProgress};

pub(crate) use crate::process::owned as child;

use std::collections::{HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use reno_proto::{
    AccessMode, ExecEvent, ExecSpec, ExecutorCaps, ExecutorDescriptor, ExecutorId, IdError,
    ModelInfo, SessionRef, Steering, StopReason, VersionRange,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::agent_id::HarnessId;
use crate::exec::{
    ExecAnswer, ExecControls, ExecError, Executor, ExecutorHealth, Open, OpenRequests, Resolved,
    require_access,
};
use crate::jsonrpc::{Incoming, RpcClient};
use crate::proc::scratch::ScratchDir;
use crate::process::{Command, Stdio};
use crate::wire::acp::normalize::{self, Normalizer};
use crate::wire::acp::permission;
use crate::wire::acp::system_message::SystemMessageEchoFilter;
use crate::{HarnessError, Signal, send_signal};
use child::Child;
use config::{ConfigRules, select_session_config};
use launch::{Launch, Launcher};
use session::{
    authenticate_for_run, cancel_params, initialize_params, is_auth_required, is_sign_in_prompt,
    not_signed_in, open_session, prompt_turn, signed_out, steering_call_future, steering_supported,
};
use spec::AcpAgentSpec;

const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(120);
/// How long a sign-in state probe may take to open a session.
const DEFAULT_PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// `session/load`'s own budget, on top of the handshake's: it replays the whole transcript, which
/// on a long session takes far longer than the rest of setup. Past it the run errors and keeps
/// the session for the next one: a slow load is no reason to give up the conversation's context.
const SESSION_LOAD_TIMEOUT: Duration = Duration::from_secs(90);
/// The executor contract version this driver implements (`ExecutorDescriptor::contract_version`).
const CONTRACT_VERSION: u32 = 1;
/// What the stream carries before the session loop starts: room for a burst of events.
const EVENT_CAPACITY: usize = 256;

/// The id an ACP agent registers under: `acp.<name>`. A registry agent's slug already is one.
fn executor_id(harness: HarnessId) -> Result<ExecutorId, IdError> {
    match harness {
        HarnessId::Registry(agent) => agent.slug().parse(),
        other => format!("acp.{}", other.as_str()).parse(),
    }
}

/// An ACP agent as an executor. Construct with [`AcpExecutor::grok`] and the like; tests point it
/// at a fake agent with [`AcpExecutor::with_executable`]. See the module docs for what it does
/// and which access modes each agent honors.
pub struct AcpExecutor {
    spec: Arc<AcpAgentSpec>,
    id: ExecutorId,
    executable: Option<PathBuf>,
    /// Grace between `session/cancel` and SIGTERM.
    interrupt_grace: Duration,
    /// Grace between SIGTERM and SIGKILL.
    kill_grace: Duration,
    /// Bound on the initialize to session handshake; a hang past it errors the run instead of
    /// waiting forever.
    handshake_timeout: Duration,
    /// `session/load`'s own budget (see [`SESSION_LOAD_TIMEOUT`]).
    session_load_timeout: Duration,
    /// Bound on a sign-in state probe's session.
    probe_timeout: Duration,
    /// The prompt-stall bound in force: the spec's, unless a caller changed it.
    prompt_stall: Option<Duration>,
    access_modes: Vec<AccessMode>,
    /// The agent's version `probe` last read, for the descriptor.
    cli_version: Arc<Mutex<Option<String>>>,
    tracker: TaskTracker,
}

impl std::fmt::Debug for AcpExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcpExecutor")
            .field("id", &self.id)
            .field("executable", &self.executable)
            .field("access_modes", &self.access_modes)
            .finish_non_exhaustive()
    }
}

impl AcpExecutor {
    fn with_spec(spec: AcpAgentSpec) -> Result<Self, IdError> {
        Ok(Self {
            id: executor_id(spec.id)?,
            executable: None,
            interrupt_grace: Duration::from_secs(2),
            kill_grace: Duration::from_secs(3),
            // Generous: the handshake is local work for every agent (session/load replays from
            // disk), so a hang past this is a wedged agent, not a slow one.
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
            session_load_timeout: SESSION_LOAD_TIMEOUT,
            probe_timeout: DEFAULT_PROBE_TIMEOUT,
            prompt_stall: spec.prompt_stall,
            access_modes: spec.access_modes.to_vec(),
            cli_version: Arc::new(Mutex::new(None)),
            tracker: TaskTracker::new(),
            spec: Arc::new(spec),
        })
    }

    /// A built-in agent's executor: its id is valid by construction.
    fn built_in(spec: AcpAgentSpec) -> Self {
        let name = spec.display_name;
        match Self::with_spec(spec) {
            Ok(executor) => executor,
            Err(error) => unreachable!("{name} has a valid executor id: {error}"),
        }
    }

    /// Devin (`devin acp`): Cognition's native ACP server.
    pub fn devin() -> Self {
        Self::built_in(spec::devin())
    }

    /// Grok Build (`grok agent stdio`): xAI's native ACP agent.
    pub fn grok() -> Self {
        Self::built_in(spec::grok())
    }

    /// fx's native ACP server.
    pub fn fx() -> Self {
        Self::built_in(spec::fx())
    }

    /// Hermes Agent (`hermes acp`): Nous Research's native ACP server.
    pub fn hermes() -> Self {
        Self::built_in(spec::hermes())
    }

    /// Google's Antigravity over its ACP server (`agy_acp_server`).
    pub fn antigravity() -> Self {
        Self::built_in(spec::antigravity())
    }

    /// An agent added from the ACP registry, launched from its record
    /// ([`crate::acp_registry`]): the generic executor, the same for every registry agent. `name`
    /// is how it's shown. Its id is `acp.<registry id>`, which fails to be an executor id only
    /// for a registry id so long that the prefix pushes it past the limit.
    pub fn registry(agent: crate::agent_id::RegistryAgent, name: &str) -> Result<Self, IdError> {
        Self::with_spec(spec::registry(agent, name))
    }

    /// Use a fixed agent binary instead of PATH/known-location resolution.
    #[must_use]
    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self
    }

    /// Tune the cancel, SIGTERM, SIGKILL escalation timing.
    #[must_use]
    pub fn with_graces(mut self, interrupt_grace: Duration, kill_grace: Duration) -> Self {
        self.interrupt_grace = interrupt_grace;
        self.kill_grace = kill_grace;
        self
    }

    /// Tune the handshake bound (tests shrink it; default 120s).
    #[must_use]
    pub fn with_handshake_timeout(mut self, timeout: Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Tune the `session/load` bound (tests shrink it; default 90s).
    #[must_use]
    pub fn with_session_load_timeout(mut self, timeout: Duration) -> Self {
        self.session_load_timeout = timeout;
        self
    }

    /// Replace the prompt-stall bound: how long a prompt may get no sign of life on the wire at
    /// all before the run ends in an error. `None` disables it. Only Grok has one by default.
    #[must_use]
    pub fn with_prompt_stall(mut self, bound: Option<Duration>) -> Self {
        self.prompt_stall = bound;
        self
    }

    /// Honour only these access modes: the descriptor lists them and `start` refuses the rest.
    /// A mode the agent itself does not honor is never added.
    #[must_use]
    pub fn with_access_modes(mut self, modes: Vec<AccessMode>) -> Self {
        self.access_modes = modes
            .into_iter()
            .filter(|mode| self.spec.access_modes.contains(mode))
            .collect();
        self
    }

    fn launcher(&self) -> Launcher<'_> {
        Launcher {
            spec: &self.spec,
            executable: self.executable.as_deref(),
        }
    }

    /// Whether the agent can be launched now: an executable pinned with
    /// [`Self::with_executable`] or the env override must validate, a registry agent must be
    /// added, anything else must be found on PATH, in the login shell's PATH or in a known
    /// install location.
    pub fn installed(&self) -> bool {
        self.launcher().installed()
    }

    /// Test seam: the program a run would spawn (the agent binary, or, for a managed npm
    /// adapter, its installed entry, else npm as the installer that would run first).
    #[doc(hidden)]
    pub fn launch_program(&self) -> Result<PathBuf, HarnessError> {
        match self.launcher().resolve_launch()? {
            Launch::Program(program, _) => Ok(program),
            Launch::Managed { pin, bin_name, .. } => {
                match crate::adapter_install::installed_entry(&pin, bin_name) {
                    Some(entry) => Ok(entry),
                    None => crate::adapter_install::find_npm()
                        .ok_or_else(|| HarnessError::NotInstalled(self.spec.install_hint.into())),
                }
            }
            Launch::Archive { entry, .. } => Ok(entry),
        }
    }

    /// The run's environment for the agent process: the agent's own needs, then the run's
    /// allowlisted environment.
    fn configure_command(
        &self,
        cmd: &mut Command,
        exe: &std::path::Path,
        spec: &ExecSpec,
    ) -> Result<(), HarnessError> {
        let launcher = self.launcher();
        child::configure(cmd);
        crate::compose_child_path(cmd, exe);
        launcher.configure_environment(cmd, exe);
        if !spec.cwd.as_os_str().is_empty() {
            cmd.current_dir(&spec.cwd);
        }
        launcher.registry_environment(cmd);
        if self.spec.id == HarnessId::Antigravity {
            cmd.env("GEMINI_HOME", antigravity_paths::home()?);
            match noop_browser() {
                Ok(browser) => {
                    cmd.env("BROWSER", browser);
                }
                // An unsuppressed browser only matters if the server asks for sign-in, which
                // the prompt watchers then stop.
                Err(error) => {
                    tracing::warn!(%error, "Antigravity browser suppression is unavailable");
                }
            }
        }
        crate::apply_run_env(cmd, &spec.env);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        Ok(())
    }

    /// Start one agent process for a run, report it for the orphan sweep and start reading its
    /// stderr.
    async fn spawn_agent(
        &self,
        exe: &std::path::Path,
        args: &[String],
        spec: &ExecSpec,
        processes: &crate::exec::ProcessSink,
    ) -> Result<Spawned, std::io::Error> {
        let scratch = self
            .launcher()
            .scratch()
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let mut cmd = Command::new(exe);
        cmd.args(args);
        self.configure_command(&mut cmd, exe, spec)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        if let Some(dir) = &scratch {
            dir.apply(&mut cmd);
        }
        let child = cmd.spawn().map_err(|e| {
            let cwd = (!spec.cwd.as_os_str().is_empty()).then_some(spec.cwd.as_path());
            match crate::executable::spawn_error(e, cwd, || crate::executable::binary_hint(exe)) {
                HarnessError::Io(error) => error,
                other => std::io::Error::other(other.to_string()),
            }
        })?;
        let mut child = Child::new(child);
        // Reported at once: a daemon that dies from here on still leaves its row.
        #[cfg(unix)]
        if let Some(record) = child.record() {
            let _ = processes.report(record).await;
        }
        let stderr_tail = crate::StderrTail::default();
        let sign_in_prompted = CancellationToken::new();
        if let Some(stderr) = child.stderr.take() {
            let tail = stderr_tail.clone();
            let prompted = sign_in_prompted.clone();
            let harness = self.spec.id;
            self.tracker.spawn(async move {
                let mut lines = crate::lossy_lines::lossy_lines(stderr);
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "reno_harness::acp", "stderr: {line}");
                    tail.push(&line);
                    if is_sign_in_prompt(harness, &line) {
                        prompted.cancel();
                    }
                }
                tail.close();
            });
        }
        Ok(Spawned {
            child,
            scratch,
            stderr_tail,
            sign_in_prompted,
        })
    }

    fn known_models(&self) -> Vec<ModelInfo> {
        self.spec
            .models
            .iter()
            .map(|(id, name)| ModelInfo {
                id: (*id).to_owned(),
                display_name: Some((*name).to_owned()),
            })
            .collect()
    }
}

/// One launched agent process and what watches it.
struct Spawned {
    child: Child,
    scratch: Option<ScratchDir>,
    stderr_tail: crate::StderrTail,
    /// Cancelled when the agent says it is not signed in (Antigravity prints a link).
    sign_in_prompted: CancellationToken,
}

/// The stream of a run that failed before it began: one `Done { Error }`.
fn failed(diagnostic: String) -> BoxStream<'static, ExecEvent> {
    futures::stream::once(async move {
        ExecEvent::Done {
            stop: StopReason::Error,
            result: None,
            diagnostic: Some(crate::redact_secrets(&diagnostic)),
        }
    })
    .boxed()
}

#[async_trait]
impl Executor for AcpExecutor {
    fn descriptor(&self) -> ExecutorDescriptor {
        let cli = self
            .cli_version
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let (min, verified) = self
            .spec
            .cli_verified
            .map_or(("0", "unverified"), |(min, verified)| (min, verified));
        ExecutorDescriptor {
            id: self.id.clone(),
            contract_version: CONTRACT_VERSION,
            adapter_version: match cli {
                Some(cli) => format!(
                    "reno-harness {} acp driver, {} (CLI {cli})",
                    env!("CARGO_PKG_VERSION"),
                    self.spec.display_name
                ),
                None => format!(
                    "reno-harness {} acp driver, {}",
                    env!("CARGO_PKG_VERSION"),
                    self.spec.display_name
                ),
            },
            cli_verified: VersionRange {
                min: min.into(),
                verified: verified.into(),
            },
            caps: ExecutorCaps {
                steering: self.spec.steering,
                resume: true,
                access_modes: self.access_modes.clone(),
                // The agent's own sandbox is not one Reno can set.
                sandbox_levels: Vec::new(),
                mcp_injection: true,
                structured_output: false,
                // `session/prompt`'s response is the turn's end.
                deterministic_turn_end: true,
                // None of these agents advertises a plan mode, and `ExecSpec` cannot ask for one:
                // never declare a capability a caller cannot use (ARCH-15).
                plan_mode: false,
                models: self.known_models(),
            },
        }
    }

    /// Installed, and the agent's version. Whether it is signed in is not knowable without
    /// starting a run, so `signed_in` stays `None`.
    async fn probe(&self) -> ExecutorHealth {
        let spec = Arc::clone(&self.spec);
        let executable = self.executable.clone();
        let probed = tokio::task::spawn_blocking(move || {
            let launcher = Launcher {
                spec: &spec,
                executable: executable.as_deref(),
            };
            let installed = launcher.installed();
            let version = installed.then(|| launcher.version()).flatten();
            (installed, version)
        })
        .await;
        match probed {
            Ok((installed, version)) => {
                *self
                    .cli_version
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner) = version.clone();
                ExecutorHealth {
                    installed,
                    version,
                    signed_in: None,
                    detail: (!installed).then(|| self.spec.install_hint.to_owned()),
                }
            }
            Err(error) => ExecutorHealth {
                detail: Some(format!("probe failed: {error}")),
                ..ExecutorHealth::default()
            },
        }
    }

    async fn start(
        &self,
        spec: ExecSpec,
        controls: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError> {
        // Fail closed before anything is resolved, installed or spawned (ARCH-15).
        require_access(spec.access, &self.access_modes)?;
        if spec.output_schema.is_some() {
            return Err(ExecError::UnsupportedOutputSchema);
        }
        let (exe, args) = match self.launcher().resolve_program(true).await {
            Ok(program) => program,
            Err(HarnessError::NotInstalled(why)) => return Err(ExecError::NotInstalled(why)),
            // A failed managed install is a failed run, not an ineligible executor.
            Err(error) => return Ok(failed(error.to_string())),
        };
        let ExecControls {
            steering,
            cancel,
            answers,
            processes,
        } = controls;
        let Spawned {
            child,
            scratch,
            stderr_tail,
            sign_in_prompted,
        } = match self.spawn_agent(&exe, &args, &spec, &processes).await {
            Ok(spawned) => spawned,
            // Any failure to launch is a failed run, not an ineligible executor.
            Err(error) => return Ok(failed(error.to_string())),
        };
        let (event_tx, event_rx) = mpsc::channel::<ExecEvent>(EVENT_CAPACITY);
        self.tracker.spawn(run_session(Session {
            tracker: self.tracker.clone(),
            child,
            scratch,
            event_tx,
            steering,
            cancel,
            answers,
            access: spec.access,
            spec,
            harness: self.spec.id,
            agent_name: self.spec.display_name,
            prompt_complete_extension: self.spec.prompt_complete_extension,
            preempt_steers: self.spec.steering == Steering::StepBoundary,
            resend_unstarted: self.spec.drops_unstarted_cancelled_prompt,
            prompt_stall: self.prompt_stall,
            stall_hint: self.spec.stall_hint,
            effort_in_model_id: self.spec.effort_in_model_id,
            auth_method: self.spec.auth_method,
            effort_values: self.spec.effort_values,
            interrupt_grace: self.interrupt_grace,
            kill_grace: self.kill_grace,
            handshake_timeout: self.handshake_timeout,
            session_load_timeout: self.session_load_timeout,
            stderr_tail,
            sign_in_prompted,
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

// ---------------------------------------------------------------------------
// Antigravity's browser suppression
// ---------------------------------------------------------------------------

/// The `BROWSER` value that keeps Antigravity's sign-in from opening a page nobody is watching.
fn noop_browser() -> Result<String, HarnessError> {
    unix_noop_browser(
        &[
            std::path::Path::new("/usr/bin/true"),
            std::path::Path::new("/bin/true"),
        ],
        std::env::current_exe().ok().as_deref(),
    )
}

/// python's `webbrowser` splits `BROWSER` on `:` and then shell-splits each entry. Hosts without
/// `true` (nixos, minimal containers) fall back to Reno's own `--noop-browser` mode.
fn unix_noop_browser(
    candidates: &[&std::path::Path],
    executable: Option<&std::path::Path>,
) -> Result<String, HarnessError> {
    if let Some(noop) = candidates.iter().find(|path| path.is_file()) {
        return Ok(format!("{} %s", noop.display()));
    }
    let executable = executable
        .and_then(std::path::Path::to_str)
        .ok_or_else(|| HarnessError::Protocol("No executable can suppress the browser".into()))?;
    if executable.contains([':', '\'', '%']) {
        return Err(HarnessError::Protocol(
            "The executable path cannot be used for browser suppression".into(),
        ));
    }
    Ok(format!("'{executable}' --noop-browser %s"))
}

// ---------------------------------------------------------------------------
// The session
// ---------------------------------------------------------------------------

struct Session {
    /// The executor's task tracker: the JSON-RPC reader and writer run on it.
    tracker: TaskTracker,
    child: Child,
    scratch: Option<ScratchDir>,
    event_tx: mpsc::Sender<ExecEvent>,
    steering: mpsc::Receiver<String>,
    cancel: CancellationToken,
    answers: mpsc::Receiver<ExecAnswer>,
    spec: ExecSpec,
    /// The run's access mode: [`Executor::start`] refused any the agent does not honor.
    access: AccessMode,
    harness: HarnessId,
    agent_name: &'static str,
    prompt_complete_extension: bool,
    /// Steers preempt the generation (the spec's steering is a step boundary).
    preempt_steers: bool,
    /// See `AcpAgentSpec::drops_unstarted_cancelled_prompt`.
    resend_unstarted: bool,
    prompt_stall: Option<Duration>,
    stall_hint: &'static str,
    effort_in_model_id: bool,
    auth_method: Option<&'static str>,
    effort_values: fn(Option<reno_proto::Effort>) -> Vec<&'static str>,
    interrupt_grace: Duration,
    kill_grace: Duration,
    handshake_timeout: Duration,
    session_load_timeout: Duration,
    stderr_tail: crate::StderrTail,
    sign_in_prompted: CancellationToken,
}

/// What the driver needs to answer a tool permission later: the request's JSON-RPC id and its
/// options.
struct ApprovalPayload {
    rpc_id: Value,
    options: Vec<Value>,
}

/// What the driver needs to answer a question later.
struct InputPayload {
    rpc_id: Value,
    options: Vec<Value>,
    question_id: String,
}

type Requests = OpenRequests<ApprovalPayload, InputPayload>;

/// The id a request is known by to the consumer: the JSON-RPC id it came with.
fn request_key(rpc_id: &Value) -> String {
    match rpc_id {
        Value::String(id) => id.clone(),
        other => other.to_string(),
    }
}

/// Everything a run does with the agent's traffic: normalizing updates, serving requests
/// against the run's access mode, keeping the table of what is open, and emitting events.
struct Pump {
    client: RpcClient,
    session_id: String,
    access: AccessMode,
    normalizer: Normalizer,
    requests: Requests,
    /// Antigravity echoes its wakeup messages into its replies; they are cut here.
    echo: Option<SystemMessageEchoFilter>,
    tx: mpsc::Sender<ExecEvent>,
    /// The run was cancelled: nothing new is allowed, and nobody is left to ask.
    interrupted: bool,
}

impl Pump {
    /// Send one event to the consumer. False once the consumer is gone.
    async fn emit(&mut self, event: ExecEvent) -> bool {
        let events = match &mut self.echo {
            Some(filter) => filter.apply(event),
            None => vec![event],
        };
        for event in events {
            if self.tx.send(event).await.is_err() {
                return false;
            }
        }
        true
    }

    /// The events a notification stands for.
    fn events_for(&mut self, method: &str, params: &Value) -> Vec<ExecEvent> {
        normalize::session_update(method, params, &self.session_id)
            .map(|update| self.normalizer.update(update))
            .unwrap_or_default()
    }

    /// Serve one server request. A permission is answered here when the run's access mode covers
    /// it, and otherwise recorded as open and raised to the consumer, as `NeedsApproval` for a
    /// tool or `NeedsInput` for a question. Any other method is unsupported: fs and terminal
    /// were declined at `initialize`. False when the consumer is gone.
    async fn request(&mut self, rpc_id: Value, method: &str, params: &Value) -> bool {
        if method != "session/request_permission" {
            tracing::debug!(target: "reno_harness::acp", "unhandled server request: {method}");
            self.client
                .respond_error(&rpc_id, -32601, &format!("unsupported method: {method}"));
            return true;
        }
        // Another session's request, or one that arrives after the run was cancelled, is never
        // asked.
        if self.interrupted
            || params
                .get("sessionId")
                .and_then(Value::as_str)
                .is_some_and(|id| id != self.session_id)
        {
            self.client.respond(&rpc_id, permission::cancelled());
            return true;
        }
        let options = permission::options(params);
        let request_id = request_key(&rpc_id);
        if permission::is_question(&options) {
            let question = permission::question(&request_id, params, &options);
            let replaced = self.requests.open_input(
                request_id.clone(),
                InputPayload {
                    rpc_id,
                    options,
                    question_id: question.id.clone(),
                },
            );
            note_superseded(replaced.is_some(), &request_id);
            return self
                .emit(ExecEvent::NeedsInput {
                    request_id,
                    questions: vec![question],
                })
                .await;
        }
        let kind = params
            .get("toolCall")
            .and_then(permission::kind_of)
            .map(str::to_owned);
        if permission::allowed_without_asking(self.access, kind.as_deref()) {
            self.client
                .respond(&rpc_id, permission::outcome(&options, true));
            return true;
        }
        let subject = permission::subject(params);
        let replaced = self
            .requests
            .open_approval(request_id.clone(), ApprovalPayload { rpc_id, options });
        note_superseded(replaced.is_some(), &request_id);
        self.emit(ExecEvent::NeedsApproval {
            request_id,
            subject,
        })
        .await
    }

    /// Apply one answer from the consumer.
    fn apply_answer(&mut self, answer: ExecAnswer) {
        let request_id = answer.request_id.clone();
        match self.requests.resolve(answer) {
            Resolved::Decision(open, decision, reason) => {
                let allow = decision == crate::exec::Decision::Allow;
                self.client.respond(
                    &open.rpc_id,
                    permission::outcome_with_reason(&open.options, allow, reason.as_deref()),
                );
            }
            // No answers at all declines the question: nothing is selected.
            Resolved::Input(open, given) => {
                self.client.respond(
                    &open.rpc_id,
                    permission::answer_outcome(&open.question_id, &open.options, &given),
                );
            }
            Resolved::WrongKind => tracing::warn!(
                target: "reno_harness::acp",
                request_id,
                "an answer of the wrong kind for this request is ignored; it stays open"
            ),
            Resolved::Unknown => tracing::warn!(
                target: "reno_harness::acp",
                request_id,
                "answer for an unknown or already resolved request"
            ),
        }
    }

    /// Answer every open request `cancelled`: ACP requires it once a prompt is cancelled, and
    /// nothing is allowed by default when a run ends.
    fn cancel_open_requests(&mut self) {
        for (_, open) in self.requests.drain() {
            let rpc_id = match open {
                Open::Approval(payload) => payload.rpc_id,
                Open::Input(payload) => payload.rpc_id,
            };
            self.client.respond(&rpc_id, permission::cancelled());
        }
    }
}

/// A request id the agent reused while its first request was still open: the new request
/// supersedes the old one, which the agent can no longer be waiting on under that id. Answering
/// the old one would be read as the answer to the new one.
fn note_superseded(replaced: bool, request_id: &str) {
    if replaced {
        tracing::warn!(
            target: "reno_harness::acp",
            request_id,
            "the agent reused the id of a request still open; the earlier one is dropped"
        );
    }
}

/// How an update changes what a run knows about its open tool calls: a steer must not preempt a
/// running tool, and a prompt must not be sent into an unowned turn.
fn track_open_tools(event: &ExecEvent, open_tools: &mut HashSet<String>) {
    match event {
        ExecEvent::Tool { id, .. } => {
            open_tools.insert(id.clone());
        }
        ExecEvent::ToolResult { id, .. } => {
            open_tools.remove(id);
        }
        _ => {}
    }
}

/// The model the session is on, as the agent reports it, else the one asked for.
fn current_model(session_options: &Value, requested: Option<&str>) -> String {
    session_options
        .get("configOptions")
        .and_then(Value::as_array)
        .and_then(|options| {
            options
                .iter()
                .find(|o| o.get("category").and_then(Value::as_str) == Some("model"))
        })
        .and_then(|option| option.get("currentValue").and_then(Value::as_str))
        .or_else(|| {
            session_options
                .get("models")
                .and_then(|models| models.get("currentModelId"))
                .and_then(Value::as_str)
        })
        .or(requested)
        .unwrap_or_default()
        .to_owned()
}

/// The per-run event loop: one task multiplexing agent messages, the pending turn, the steering
/// queue, the answers, the cancel token and consumer liveness.
async fn run_session(session: Session) {
    let Session {
        tracker,
        // Locals drop in reverse binding order: reap the child before cleanup.
        scratch: _scratch,
        mut child,
        event_tx,
        mut steering,
        cancel,
        mut answers,
        spec,
        access,
        harness,
        agent_name,
        prompt_complete_extension,
        preempt_steers,
        resend_unstarted,
        prompt_stall,
        stall_hint,
        effort_in_model_id,
        auth_method,
        effort_values,
        interrupt_grace,
        kill_grace,
        handshake_timeout,
        session_load_timeout,
        stderr_tail,
        sign_in_prompted,
    } = session;
    let rules = ConfigRules {
        harness,
        effort_in_model_id,
        effort_values,
        access,
    };

    let (client, mut incoming) = {
        let (Some(stdin), Some(stdout)) = (child.stdin.take(), child.stdout.take()) else {
            let _ = event_tx
                .send(ExecEvent::Done {
                    stop: StopReason::Error,
                    result: None,
                    diagnostic: Some("agent child has no stdio".into()),
                })
                .await;
            child.shutdown(kill_grace).await;
            return;
        };
        if harness == HarnessId::Antigravity {
            let prompted = sign_in_prompted.clone();
            RpcClient::spawn_observing(
                &tracker,
                stdin,
                stdout,
                Some(Box::new(move |_| prompted.cancel())),
            )
        } else {
            RpcClient::spawn(&tracker, stdin, stdout)
        }
    };

    // ---- handshake + session (interruptible) ------------------------------
    let setup = async {
        let init = client.request("initialize", initialize_params()).await?;
        let steer_ext = steering_supported(&init);

        // auth_required gets one sign-in and retry from the agent's own default method; an agent
        // that signs in elsewhere never starts a sign-in mid-run and says so.
        let mut authenticated = false;
        let (session_id, session_response) = loop {
            match open_session(
                &client,
                &mut incoming,
                &init,
                harness,
                &spec,
                access,
                agent_name,
                session_load_timeout,
            )
            .await
            {
                Err(e) if is_auth_required(&e) && auth_method.is_some() => {
                    return Err(HarnessError::Protocol(not_signed_in(agent_name)));
                }
                Err(e) if is_auth_required(&e) && !authenticated => {
                    authenticated = true;
                    authenticate_for_run(&client, &mut incoming, access, &init, agent_name, &e)
                        .await?;
                }
                Err(e) if is_auth_required(&e) => {
                    return Err(HarnessError::Protocol(signed_out(agent_name, &e)));
                }
                other => break other?,
            }
        };
        if session_id.is_empty() {
            return Err(HarnessError::Protocol(
                "session/new returned no sessionId".into(),
            ));
        }
        let session_options = select_session_config(
            &client,
            &mut incoming,
            rules,
            &session_id,
            session_response,
            &spec,
        )
        .await?;
        Ok::<(String, bool, Value), HarnessError>((session_id, steer_ext, session_options))
    };
    let mut setup_exit = None;
    let mut setup_drain_deadline = None;
    let setup_budget = if spec.resume.is_some() {
        handshake_timeout + session_load_timeout
    } else {
        handshake_timeout
    };
    // Scoped: the setup future borrows `incoming` until it is dropped.
    let res = {
        let setup = async {
            tokio::select! {
                result = setup => result,
                _ = sign_in_prompted.cancelled() => {
                    Err(HarnessError::Protocol(not_signed_in(agent_name)))
                }
            }
        };
        tokio::pin!(setup);
        let handshake_deadline = tokio::time::sleep(setup_budget);
        tokio::pin!(handshake_deadline);
        loop {
            tokio::select! {
                res = &mut setup => break res,
                // A hung handshake (agent waiting on a login it can never get, a wedged
                // adapter) must not wait forever: bound it and say what was reached.
                _ = &mut handshake_deadline => {
                    break Err(HarnessError::Protocol(format!(
                        "{agent_name} did not complete the ACP handshake within {}s \
                         (the agent may be waiting for a login: try running it once \
                         in a terminal)",
                        setup_budget.as_secs()
                    )));
                }
                // A crash mid-handshake must not sit out the whole bound: a descendant holding
                // stdout open keeps the requests pending. Same short drain as the main loop for
                // frames already written.
                status = child.wait(), if setup_exit.is_none() => {
                    setup_exit = Some(status.ok());
                    child.request_group_shutdown();
                    setup_drain_deadline =
                        Some(tokio::time::Instant::now() + Duration::from_millis(200));
                }
                _ = tokio::time::sleep_until(
                        setup_drain_deadline.unwrap_or_else(tokio::time::Instant::now)
                    ), if setup_drain_deadline.is_some() =>
                {
                    break Err(HarnessError::Protocol(format!(
                        "{agent_name} exited during the ACP handshake"
                    )));
                }
                _ = cancel.cancelled() => {
                    let _ = event_tx
                        .send(ExecEvent::Done {
                            stop: StopReason::Interrupted,
                            result: None,
                            diagnostic: None,
                        })
                        .await;
                    child.shutdown(kill_grace).await;
                    return;
                }
            }
        }
    };
    let (session_id, steer_ext, session_options) = match res {
        Ok(v) => v,
        Err(e) => {
            // A child that dies before the handshake used to surface only the RPC-side symptom
            // ("transport closed"): its exit status and stderr, both already in hand, say why.
            // When the child is already gone, give the reader task a beat to drain the pipe,
            // then append the crash text. A still-live child (the timeout) contributes its
            // stderr tail.
            let exited = match setup_exit {
                Some(status) => Some(status),
                None => match child.try_wait() {
                    Ok(Some(status)) => {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        Some(Some(status))
                    }
                    _ => None,
                },
            };
            let error = match exited {
                Some(status) => {
                    stderr_tail.wait_closed().await;
                    format!(
                        "{e}; {}",
                        crate::crash_message(agent_name, status, &stderr_tail)
                    )
                }
                None => match stderr_tail.snapshot() {
                    Some(tail) => format!("{e}; stderr: {tail}"),
                    None => e.to_string(),
                },
            };
            tracing::warn!(target: "reno_harness::acp", %error, "agent setup failed");
            let _ = event_tx
                .send(ExecEvent::Done {
                    stop: StopReason::Error,
                    result: None,
                    diagnostic: Some(crate::redact_secrets(&error)),
                })
                .await;
            child.shutdown(kill_grace).await;
            return;
        }
    };

    let mut pump = Pump {
        client: client.clone(),
        session_id: session_id.clone(),
        access,
        normalizer: Normalizer::new(),
        requests: Requests::new(),
        echo: (harness == HarnessId::Antigravity).then(SystemMessageEchoFilter::default),
        tx: event_tx.clone(),
        interrupted: false,
    };
    if !pump
        .emit(ExecEvent::Started {
            session: SessionRef(session_id.clone()),
            model: current_model(&session_options, spec.model.as_deref()),
        })
        .await
    {
        child.shutdown(kill_grace).await;
        return;
    }

    // ---- main loop --------------------------------------------------------
    // Prompt-completion settlement state (the prompt-complete extension): one prompt is
    // outstanding at a time, identified by `current_prompt_id`; settled ids are remembered so a
    // STALE `prompt_complete` (a late replay of an already-settled prompt) can never settle a
    // newer turn.
    let mut prompt_seq: u64 = 1;
    let mut current_prompt_id = prompt_complete_extension.then(|| format!("reno-p{prompt_seq}"));
    let mut completed_prompts: VecDeque<String> = VecDeque::new();
    let mut prompt_stall_deadline: Option<tokio::time::Instant> =
        prompt_stall.map(|d| tokio::time::Instant::now() + d);
    // The prompt in flight (re-sent by a preempt when the agent drops an unstarted cancelled
    // prompt).
    let mut current_prompt = spec.prompt.clone();
    let mut turn: Option<BoxFuture<'static, Result<Value, HarnessError>>> = Some(prompt_turn(
        &client,
        &session_id,
        &current_prompt,
        current_prompt_id.as_deref(),
    ));
    // Steers waiting for the turn boundary (agents without the extension, or extension steers
    // that lost the turn-end race).
    let mut queued_steers: VecDeque<String> = VecDeque::new();
    // The in-flight `_session/steering` call (text + response future), plus followers awaiting
    // their turn. Polled from the main select so the loop keeps draining `incoming` while the
    // agent responds: awaiting inline deadlocks against a full incoming channel when the agent
    // floods updates (the reader blocks on the channel and never parses the steering response).
    let mut steering_call: Option<(String, BoxFuture<'static, Result<Value, HarnessError>>)> = None;
    let mut steer_backlog: VecDeque<String> = VecDeque::new();
    let mut steering_open = true;
    // Immediate steering for agents without a mid-turn steering extension: a steer cancels the
    // current generation (never a running tool: it waits for open tools to finish) and the
    // cancelled turn continues as the steer's prompt in the same session.
    let mut preempt_pending = false;
    let mut preempt_sent = false;
    // The prompt (`prompt_seq`) that last showed progress (text, thought, tool or plan). Grok
    // drops a prompt cancelled before that, so its preempt re-sends the text.
    let mut progress_seq: u64 = 0;
    let mut interrupt_sent = false;
    let mut answers_open = true;
    // The run's one `Done` has gone out.
    let mut ended = false;
    let mut escalation_target = None;
    let mut escalation_deadline = None;
    let mut escalation_signal = Signal::Term;
    // Starved-turn recovery: a `session/prompt` sent while the agent runs a SELF-CONTINUED turn
    // (a background-task re-invocation no prompt started) starves: an adapter that does not
    // track turns it did not start never attributes the merged turn's result to the pending
    // prompt. The tell is protocol evidence, not timing: a steering call answered
    // `promptRequired`/`noRunningTurn` while OUR prompt is outstanding means the agent has no
    // turn that could ever settle it. A short grace covers the true turn-end race (its response
    // lands within milliseconds); past it the dead prompt is closed out.
    const STARVE_GRACE: Duration = Duration::from_secs(2);
    let mut starve_deadline: Option<tokio::time::Instant> = None;
    // Tools in flight: a steer must not preempt them.
    let mut open_tools: HashSet<String> = HashSet::new();
    let mut child_exit = None;
    let mut exit_drain_deadline = None;
    'main: loop {
        tokio::select! {
            _ = sign_in_prompted.cancelled() => {
                // A cancelled run ends `Interrupted` below, whatever the agent printed after.
                if !ended && !pump.interrupted {
                    ended = true;
                    let _ = pump.emit(ExecEvent::Done {
                        stop: StopReason::Error,
                        result: None,
                        diagnostic: Some(not_signed_in(agent_name)),
                    }).await;
                }
                break 'main;
            },
            status = child.wait(), if child_exit.is_none() => {
                escalation_deadline = None;
                child_exit = Some(status.ok());
                child.request_group_shutdown();
                // Descendants can hold stdout open after an adapter crash. Drain already-written
                // frames, but never wait on them forever.
                exit_drain_deadline =
                    Some(tokio::time::Instant::now() + Duration::from_millis(200));
            },
            _ = tokio::time::sleep_until(
                    exit_drain_deadline.unwrap_or_else(tokio::time::Instant::now)
                ), if exit_drain_deadline.is_some() => break 'main,

            res = async {
                match turn.as_mut() {
                    Some(turn) => turn.await,
                    None => std::future::pending().await,
                }
            }, if turn.is_some() => {
                drop(turn.take());
                if res.is_err() && client.is_closed() {
                    break 'main;
                }
                // The settled prompt's tool calls are over with it.
                open_tools.clear();
                starve_deadline = None;
                if let Some(id) = current_prompt_id.take() {
                    completed_prompts.push_back(id);
                    while completed_prompts.len() > 32 {
                        completed_prompts.pop_front();
                    }
                }
                // Settle an in-flight `_session/steering` call BEFORE closing the turn: its
                // response rides the same stdout as the prompt response, so by now it is
                // (nearly always) already parsed: the select just hadn't polled it yet. An
                // injection that landed in this turn is consumed by it; a rejected or unsettled
                // call redelivers as the next turn. The timeout guards the flooded-incoming
                // edge (reader blocked on a full channel never parses the response): past it the
                // call is abandoned and the steer redelivered.
                if let Some((text, mut fut)) = steering_call.take() {
                    let outcome = match tokio::time::timeout(Duration::from_millis(1000), &mut fut)
                        .await
                    {
                        Ok(Ok(resp)) => resp
                            .get("outcome")
                            .and_then(Value::as_str)
                            .unwrap_or("injected")
                            .to_owned(),
                        Ok(Err(_)) | Err(_) => "promptRequired".to_owned(),
                    };
                    if !pump.interrupted && outcome == "promptRequired" {
                        queued_steers.push_back(text);
                    }
                    // Followers waiting on the settled call have no live turn to inject into
                    // anymore: boundary delivery.
                    while let Some(next_text) = steer_backlog.pop_front() {
                        queued_steers.push_back(next_text);
                    }
                }
                // Updates streamed before the prompt response are already queued in stdout
                // order: fold them into the turn before closing it (responses bypass the
                // incoming queue).
                while let Ok(inc) = incoming.try_recv() {
                    let alive = match inc {
                        Incoming::Notification { method, params } => {
                            let events = pump.events_for(&method, &params);
                            let mut alive = true;
                            for ev in events {
                                if !pump.emit(ev).await {
                                    alive = false;
                                    break;
                                }
                            }
                            alive
                        }
                        Incoming::Request { id, method, params } => {
                            pump.request(id, &method, &params).await
                        }
                        _ => true,
                    };
                    if !alive {
                        break 'main;
                    }
                }
                if !pump.interrupted
                    && let Ok(resp) = &res
                    && let Some(usage) = normalize::usage(resp)
                    && !pump.emit(usage).await
                {
                    break 'main;
                }
                // A steer preempted this turn: the session lives on and the steers continue it
                // below, so there is no turn end to report.
                let preempted = std::mem::take(&mut preempt_sent)
                    && !pump.interrupted
                    && res.is_ok()
                    && !queued_steers.is_empty();
                preempt_pending = false;
                let (stop, diagnostic) = stop_of(&res, pump.interrupted);
                // A steer that waited for the turn boundary continues the run: only a turn that
                // ended normally (or was preempted for it) takes the next prompt.
                let continues = !pump.interrupted
                    && res.is_ok()
                    && (preempted || stop == StopReason::Completed)
                    && !queued_steers.is_empty();
                if !continues {
                    let diagnostic = match (&res, diagnostic) {
                        (Err(e), _) if is_auth_required(e) && auth_method.is_some() => {
                            Some(not_signed_in(agent_name))
                        }
                        (_, diagnostic) => diagnostic,
                    };
                    ended = true;
                    let _ = pump.emit(ExecEvent::Done {
                        stop,
                        result: None,
                        diagnostic: diagnostic.map(|d| crate::redact_secrets(&d)),
                    }).await;
                    break 'main;
                }
                // The queued steers become the next prompt, all of them at once.
                let mut prompts = Vec::with_capacity(queued_steers.len() + 1);
                if preempted && resend_unstarted && progress_seq != prompt_seq {
                    prompts.push(std::mem::take(&mut current_prompt));
                }
                prompts.extend(queued_steers.drain(..));
                prompt_seq += 1;
                current_prompt_id =
                    prompt_complete_extension.then(|| format!("reno-p{prompt_seq}"));
                prompt_stall_deadline = prompt_stall.map(|d| tokio::time::Instant::now() + d);
                current_prompt = prompts.join("\n\n");
                turn = Some(prompt_turn(
                    &client,
                    &session_id,
                    &current_prompt,
                    current_prompt_id.as_deref(),
                ));
            },

            inc = incoming.recv() => match inc {
                Some(Incoming::Notification { method, params }) => {
                    if params
                        .get("sessionId")
                        .and_then(Value::as_str)
                        .is_some_and(|id| id != session_id)
                    {
                        continue;
                    }
                    let update = normalize::session_update(&method, &params, &session_id);
                    // Wire traffic is a sign of life for the prompt-stall watchdog, EXCEPT
                    // session boilerplate: opencode emits available_commands_update right after
                    // session/new on every session, including ones whose provider is down (where
                    // it then retries the provider stream forever with nothing further on the
                    // wire). One such frame must not disarm the watchdog for the whole turn;
                    // only turn progress counts.
                    if !update.is_some_and(normalize::is_boilerplate) {
                        prompt_stall_deadline = None;
                    }
                    if update.is_some_and(normalize::is_progress) {
                        progress_seq = prompt_seq;
                    }
                    // `_x.ai/session/prompt_complete`: the AUTHORITATIVE turn end for agents
                    // advertising it (grok): the `session/prompt` RPC can hang after the turn
                    // really finished. Settle through the SAME response arm by swapping the
                    // in-flight future for the synthesized response; the abandoned RPC future is
                    // dropped and its late result discarded. Guards: session
                    // match, an outstanding prompt, and an exact prompt-id match (a stale replay
                    // of an already-settled prompt must never settle a newer turn; grok echoes
                    // the `_meta.promptId` we mint).
                    if prompt_complete_extension
                        && method == "_x.ai/session/prompt_complete"
                        && !pump.interrupted
                        && turn.is_some()
                        && params.get("sessionId").and_then(Value::as_str)
                            == Some(session_id.as_str())
                    {
                        let pid = params
                            .get("promptId")
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        let stale = pid
                            .as_deref()
                            .is_some_and(|p| completed_prompts.iter().any(|c| c == p))
                            || (pid.is_some() && pid != current_prompt_id);
                        if !stale {
                            let stop = params
                                .get("stopReason")
                                .and_then(Value::as_str)
                                .unwrap_or("end_turn")
                                .to_owned();
                            // The abandoned RPC future is dropped: its late answer finds no
                            // receiver, which the reader ignores.
                            drop(turn.take());
                            turn = Some(Box::pin(async move {
                                Ok(json!({ "stopReason": stop }))
                            }));
                        }
                    }
                    // Other notifications (other sessions, agent noise) are tolerated by design.
                    for ev in pump.events_for(&method, &params) {
                        track_open_tools(&ev, &mut open_tools);
                        if !pump.emit(ev).await {
                            break 'main;
                        }
                    }
                    // A waiting steer preempts once no tool is open and the agent is generating
                    // again.
                    if preempt_pending && !preempt_sent && turn.is_some() && open_tools.is_empty() {
                        client.notify("session/cancel", Some(cancel_params(harness, &session_id)));
                        preempt_sent = true;
                    }
                }
                Some(Incoming::Request { id, method, params }) => {
                    prompt_stall_deadline = None;
                    if !pump.request(id, &method, &params).await {
                        break 'main;
                    }
                }
                Some(Incoming::Eof) | None => {
                    // The turn ends via a request RESPONSE, which races EOF through a different
                    // channel than notifications: an agent exiting right after its final
                    // response must read as a clean finish, not a crash. The response (if any)
                    // is already resolved by the reader before it sends Eof. Only a RESOLVED
                    // response is a clean finish; a request failed by the reader's EOF cleanup
                    // falls through to the crash-message bookkeeping below (stderr tail intact).
                    if let Some(mut fut) = turn.take()
                        && let Ok(res @ Ok(_)) =
                            tokio::time::timeout(Duration::from_millis(50), &mut fut).await
                    {
                        if let Ok(resp) = &res
                            && let Some(usage) = normalize::usage(resp)
                        {
                            let _ = pump.emit(usage).await;
                        }
                        let (stop, diagnostic) = stop_of(&res, pump.interrupted);
                        ended = true;
                        let _ = pump.emit(ExecEvent::Done {
                            stop,
                            result: None,
                            diagnostic,
                        }).await;
                    }
                    break 'main;
                }
            },

            res = async {
                    match steering_call.as_mut() {
                        Some((_, call)) => call.as_mut().await,
                        None => std::future::pending().await,
                    }
                },
                if steering_call.is_some() =>
            {
                let Some((text, _)) = steering_call.take() else {
                    continue;
                };
                let outcome = match &res {
                    Ok(resp) => resp
                        .get("outcome")
                        .and_then(Value::as_str)
                        .unwrap_or("injected")
                        .to_owned(),
                    Err(e) => {
                        tracing::debug!(
                            target: "reno_harness::acp",
                            "_session/steering failed (redelivering): {e}"
                        );
                        // Failed calls redeliver like a lost turn-end race.
                        "promptRequired".to_owned()
                    }
                };
                if pump.interrupted {
                    // The run is winding down: the steer is abandoned like any queued steer.
                } else if outcome != "promptRequired" {
                    if turn.is_some() {
                        // The injection proves the turn is LIVE: any settle deadline armed by an
                        // earlier noRunningTurn reply is no longer valid.
                        starve_deadline = None;
                        // Pre-injection updates can still sit in `incoming` (responses bypass
                        // that queue): drain them first, so what the agent streamed before the
                        // injection landed keeps its place in the feed.
                        while let Ok(inc) = incoming.try_recv() {
                            let alive = match inc {
                                Incoming::Notification { method, params } => {
                                    let mut alive = true;
                                    for ev in pump.events_for(&method, &params) {
                                        if !pump.emit(ev).await {
                                            alive = false;
                                            break;
                                        }
                                    }
                                    alive
                                }
                                Incoming::Request { id, method, params } => {
                                    pump.request(id, &method, &params).await
                                }
                                _ => true,
                            };
                            if !alive {
                                break 'main;
                            }
                        }
                    }
                } else if turn.is_some() {
                    // Raced the turn end: redeliver at the boundary the loop is about to hit.
                    // `noRunningTurn` is stronger: the agent says nothing is running while our
                    // prompt is still outstanding, the starved-turn signature. Arm the grace
                    // deadline; if the prompt's response does not land first, the recovery arm
                    // below settles the dead turn.
                    if res
                        .as_ref()
                        .ok()
                        .and_then(|r| r.get("reason"))
                        .and_then(Value::as_str)
                        == Some("noRunningTurn")
                    {
                        tracing::warn!(
                            target: "reno_harness::acp",
                            "steering answered noRunningTurn with a prompt outstanding; arming \
                             starved-turn recovery"
                        );
                        starve_deadline = Some(tokio::time::Instant::now() + STARVE_GRACE);
                    }
                    queued_steers.push_back(text);
                } else {
                    queued_steers.push_back(text);
                }
                while let Some(next_text) = steer_backlog.pop_front() {
                    if turn.is_some() && !pump.interrupted {
                        let fut = steering_call_future(&client, &session_id, &next_text);
                        steering_call = Some((next_text, fut));
                        break;
                    }
                    // No live turn to inject into: boundary delivery.
                    queued_steers.push_back(next_text);
                }
            },

            // Explicit noRunningTurn from the steering extension proves the agent has no running
            // turn. After a grace for its racing response, recover the stranded prompt. Silence,
            // tool results and usage updates must never arm this recovery.
            _ = tokio::time::sleep_until(
                starve_deadline.unwrap_or_else(tokio::time::Instant::now)
            ), if starve_deadline.is_some() && turn.is_some() && !pump.interrupted => {
                starve_deadline = None;
                tracing::warn!(
                    target: "reno_harness::acp",
                    "prompt response missing past turn-end evidence; settling the dead turn \
                     (and promoting any queued steer)"
                );
                // Drop the dead future: a response that somehow arrives later resolves a closed
                // channel harmlessly.
                drop(turn.take());
                open_tools.clear();
                if queued_steers.is_empty() {
                    ended = true;
                    let _ = pump.emit(ExecEvent::Done {
                        stop: StopReason::Completed,
                        result: None,
                        diagnostic: None,
                    }).await;
                    break 'main;
                }
                prompt_seq += 1;
                current_prompt_id =
                    prompt_complete_extension.then(|| format!("reno-p{prompt_seq}"));
                prompt_stall_deadline = prompt_stall.map(|d| tokio::time::Instant::now() + d);
                current_prompt = queued_steers.drain(..).collect::<Vec<_>>().join("\n\n");
                turn = Some(prompt_turn(
                    &client,
                    &session_id,
                    &current_prompt,
                    current_prompt_id.as_deref(),
                ));
            },

            steer = steering.recv(), if steering_open && !pump.interrupted => match steer {
                Some(prompt) => {
                    if steer_ext {
                        // Mid-turn injection via the `_session/steering` extension: start the
                        // call, resolved by its own select branch. One call in flight at a time;
                        // followers wait in the backlog.
                        if steering_call.is_some() {
                            steer_backlog.push_back(prompt);
                        } else {
                            let fut = steering_call_future(&client, &session_id, &prompt);
                            steering_call = Some((prompt, fut));
                        }
                    } else {
                        // No extension: wait for the turn boundary, or preempt the generation
                        // and continue the turn with this steer (see `preempt_pending`).
                        queued_steers.push_back(prompt);
                        preempt_pending = preempt_steers;
                        if preempt_pending && !preempt_sent && open_tools.is_empty() {
                            client.notify(
                                "session/cancel",
                                Some(cancel_params(harness, &session_id)),
                            );
                            preempt_sent = true;
                        }
                    }
                }
                // The queue closed: the run ends with its turn.
                None => steering_open = false,
            },

            answer = answers.recv(), if answers_open => match answer {
                Some(answer) => pump.apply_answer(answer),
                None => answers_open = false,
            },

            _ = cancel.cancelled(), if !interrupt_sent => {
                interrupt_sent = true;
                pump.interrupted = true;
                // A request still open when the run is cancelled is answered `cancelled`,
                // before anything else reaches the agent.
                pump.cancel_open_requests();
                if turn.is_some() {
                    client.notify("session/cancel", Some(cancel_params(harness, &session_id)));
                    // Escalate if the agent doesn't wind down (stopReason "cancelled") within
                    // the grace periods.
                    if let Some(pid) = crate::process::signal_target(&child) {
                        escalation_target = Some(pid);
                        escalation_deadline = Some(tokio::time::Instant::now() + interrupt_grace);
                    }
                } else {
                    break 'main;
                }
            },

            // Prompt-stall watchdog: a prompt was sent and NOTHING has come back on the wire at
            // all: no queue bookkeeping, no updates, no requests. Healthy grok acknowledges
            // within milliseconds; total silence past the bound is a wedged agent (stale shared
            // leader, launch-time update check). End the run with a diagnostic instead of
            // waiting forever.
            _ = tokio::time::sleep_until(
                prompt_stall_deadline.unwrap_or_else(tokio::time::Instant::now),
            ), if prompt_stall_deadline.is_some() && turn.is_some() && !pump.interrupted => {
                ended = true;
                let _ = pump.emit(ExecEvent::Done {
                    stop: StopReason::Error,
                    result: None,
                    diagnostic: Some(format!(
                        "{agent_name} did not respond to the prompt at all (no wire activity \
                         for {}s). {stall_hint}",
                        prompt_stall.map_or(0, |d| d.as_secs()),
                    )),
                }).await;
                break 'main;
            },

            // Keep escalation in the owner task: it cannot outlive child.wait() or signal a pid
            // after the child has been reaped.
            _ = tokio::time::sleep_until(
                escalation_deadline.unwrap_or_else(tokio::time::Instant::now)
            ), if escalation_deadline.is_some() => {
                // Once signal escalation starts, a late prompt response no longer owns this turn.
                turn = None;
                if let Some(target) = &escalation_target {
                    send_signal(target, escalation_signal);
                }
                escalation_deadline = match escalation_signal {
                    Signal::Term => {
                        escalation_signal = Signal::Kill;
                        Some(tokio::time::Instant::now() + kill_grace)
                    }
                    Signal::Kill => None,
                };
            },

            _ = event_tx.closed() => break 'main,
        }
    }

    // Terminal bookkeeping: never end the stream without a Done unless the consumer already hung
    // up.
    if !event_tx.is_closed() && !ended {
        if pump.interrupted {
            let _ = pump
                .emit(ExecEvent::Done {
                    stop: StopReason::Interrupted,
                    result: None,
                    diagnostic: None,
                })
                .await;
        } else {
            // A child killed mid-turn must not read as a silent success.
            let status = match child_exit {
                Some(status) => status,
                None => tokio::time::timeout(Duration::from_millis(200), child.wait())
                    .await
                    .ok()
                    .and_then(Result::ok),
            };
            child.request_group_shutdown();
            stderr_tail.wait_closed().await;
            let _ = pump
                .emit(ExecEvent::Done {
                    stop: StopReason::Error,
                    result: None,
                    diagnostic: Some(crate::redact_secrets(&crate::crash_message(
                        agent_name,
                        status,
                        &stderr_tail,
                    ))),
                })
                .await;
        }
    }

    // Whatever is still open is refused: the run is over.
    pump.cancel_open_requests();
    // The agent and everything it started go with the run, on every exit path.
    child.shutdown(kill_grace).await;
}

/// How a settled `session/prompt` ended (see [`normalize::stop`]).
fn stop_of(
    response: &Result<Value, HarnessError>,
    interrupted: bool,
) -> (StopReason, Option<String>) {
    match response {
        Ok(response) => normalize::stop(Ok(response), interrupted),
        Err(error) => normalize::stop(Err(&error.to_string()), interrupted),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn unix_browser_suppression_falls_back_to_renos_noop_mode() {
        let present = std::env::current_exe().unwrap();
        assert_eq!(
            unix_noop_browser(&[Path::new("/missing/true"), &present], None).unwrap(),
            format!("{} %s", present.display())
        );
        let missing = [
            Path::new("/missing/usr/bin/true"),
            Path::new("/missing/bin/true"),
        ];
        assert_eq!(
            unix_noop_browser(&missing, Some(Path::new("/opt/My Apps/reno"))).unwrap(),
            "'/opt/My Apps/reno' --noop-browser %s"
        );
        for path in ["/opt/a:b/reno", "/opt/it's/reno", "/opt/100%s/reno"] {
            assert!(
                unix_noop_browser(&missing, Some(Path::new(path))).is_err(),
                "{path}"
            );
        }
        assert!(unix_noop_browser(&missing, None).is_err());
    }

    #[test]
    fn executor_ids_are_acp_dot_the_agent_and_a_registry_slug_is_already_one() {
        let ids: Vec<String> = [
            AcpExecutor::grok(),
            AcpExecutor::devin(),
            AcpExecutor::hermes(),
            AcpExecutor::fx(),
            AcpExecutor::antigravity(),
        ]
        .iter()
        .map(|executor| executor.id.to_string())
        .collect();
        assert_eq!(
            ids,
            [
                "acp.grok",
                "acp.devin",
                "acp.hermes",
                "acp.fx",
                "acp.antigravity"
            ]
        );
        let agent = crate::agent_id::RegistryAgent::new("gemini").unwrap();
        assert_eq!(
            AcpExecutor::registry(agent, "Gemini").unwrap().id.as_str(),
            "acp.gemini"
        );
        // A registry id long enough that the prefix breaks the executor id limit is refused, not
        // truncated into another agent's id.
        let long = crate::agent_id::RegistryAgent::new(&"a".repeat(64)).unwrap();
        assert!(AcpExecutor::registry(long, "Long").is_err());
    }

    #[test]
    fn explicit_program_launches_do_not_get_archive_scratch_roots() {
        let executor = AcpExecutor::antigravity().with_executable(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/fake-antigravity-acp.sh"),
        );
        assert!(executor.launcher().scratch().unwrap().is_none());
    }

    #[test]
    fn every_spec_names_its_own_requests_and_honors_the_modes_the_docs_say() {
        // The table in the module docs, from the data it describes.
        let modes = |executor: &AcpExecutor| executor.access_modes.clone();
        assert_eq!(modes(&AcpExecutor::grok()), [AccessMode::FullAccess]);
        assert_eq!(modes(&AcpExecutor::devin()), [AccessMode::FullAccess]);
        assert_eq!(modes(&AcpExecutor::hermes()), [AccessMode::FullAccess]);
        assert_eq!(
            modes(&AcpExecutor::fx()),
            [AccessMode::Supervised, AccessMode::FullAccess]
        );
        assert_eq!(
            modes(&AcpExecutor::antigravity()),
            [
                AccessMode::Supervised,
                AccessMode::AutoAcceptEdits,
                AccessMode::FullAccess
            ]
        );
        // Only Grok pins a prompt-stall bound and a prompt-complete extension.
        assert!(AcpExecutor::grok().prompt_stall.is_some());
        for executor in [
            AcpExecutor::devin(),
            AcpExecutor::hermes(),
            AcpExecutor::fx(),
            AcpExecutor::antigravity(),
        ] {
            assert!(executor.prompt_stall.is_none());
            assert!(!executor.spec.prompt_complete_extension);
        }
        assert!(AcpExecutor::grok().spec.prompt_complete_extension);
    }
}
