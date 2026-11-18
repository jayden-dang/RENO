//! The native Muse Code executor: spawns the installed `muse` CLI as `muse serve` and speaks the
//! Muse Session Protocol (MSP, JSON-RPC 2.0 on stdio) with it, and implements [`Executor`].
//!
//! VERSION PIN: [`MIN_CLI_VERSION`] and [`VERIFIED_CLI_VERSION`] are both 1.4.23, and what that
//! means is narrow. The wire was read from 1.4.23's own exported schema (`muse schema
//! generate-json-schema [--experimental]`) and `--help`, and a real 1.4.23 was started in a
//! throwaway `HOME`, `XDG_CONFIG_HOME` and `XDG_DATA_HOME` and driven with its own `echo`
//! provider, a mock model that sends nothing anywhere: `initialize`, `account/read` (logged out
//! there), `model/list`, `usage/read`, `session/start` and `session/resume` with the run's
//! posture, `session/setApprovalMode`, a whole `turn/start` to `turn/completed`, and the
//! `mcpServers` session config. Nothing Muse wrote landed outside its data directory, and none of
//! those calls wrote a settings file. NOT exercised against a real Muse: a tool call, an
//! approval, a question, a subagent, a steer, an interrupt, a sign-in, a model on Meta's servers,
//! and the default credential store (which on macOS is the Keychain: the throwaway runs set
//! `TBH_CREDENTIAL_BACKEND=file`, a name read off the binary, and saw `loggedOut`). The frame
//! shapes for those come from zeron's recordings of Muse 1.4.3 (`tests/fixtures/muse/`, labelled
//! as such), checked against the 1.4.23 schema. The live test (`tests/muse.rs`, `--ignored`,
//! `RENO_MUSE_LIVE=1`) plays the whole run over the `echo` provider; it is the lead's to run.
//!
//! - `wire::muse` is sans-IO: frames become [`ExecEvent`]s there, approvals and questions become
//!   subjects and answers, a model catalog becomes `ModelInfo`s. This module only does the I/O.
//! - ONE RUN IS ONE `Done`. A host serves one run: it is started with the run's flags, asked for a
//!   session with the run's posture, given the prompt, and torn down when the run is over. The
//!   per-chat host and the session reuse of zeron's driver are gone.
//! - A turn ends at the host's `turn/completed` (deterministic). The run ends when the turns it
//!   started or joined are over, no steering message waits, and no background subagent or workflow
//!   is still running: Muse starts a turn of its own when background work finishes, so a run with
//!   some waits for it (a short grace for that turn to start), and a cancel stops it all.
//! - Steering is `turn/start` with `ifBusy: "steer"`: one atomic command that steers the running
//!   turn or starts one when idle. It lands at the turn's next model step.
//! - EVERY APPROVAL MUSE RAISES GOES TO RENO'S KERNEL, in both access modes: the session runs
//!   with the `promptUnmatched` approval mode and the `:ask-me` permission profile (`allowAll`
//!   and `:unrestricted` would take the decision away from the kernel), and the posture is read
//!   back from the host and refused when it differs. What `:ask-me` asks about is Muse's own
//!   call (its own words: "requires approval for protected writes, new network targets, and
//!   other restricted actions"; it edits the workspace and runs sandboxed commands without
//!   asking), so a supervised run is asked about those and not about every edit: the kernel's
//!   after-the-fact protected-path detector (`supervisor/protect.rs`, which looks at every run's
//!   tool calls and worktree) covers what Muse does not ask. An allow is the single-use choice
//!   the host offered; a choice that grants more or saves a rule is never taken, and a subject
//!   of a kind Reno does not read is never allowed.
//! - NEVER A CONFIG WRITE. `muse serve` has 133 methods; the one writer ([`rpc::Client`]) sends
//!   an allowlist of eleven and refuses the rest, among them everything that writes Muse's
//!   settings, credentials, trust, plugins or skills. Nothing needs a file: the instructions ride
//!   the prompt and the MCP servers ride the `session/start` config, which is session-only. The
//!   launcher script that `muse` is updates itself in the background on every start; every
//!   process Reno starts for Muse (a version check would be one: it does not run one) sets
//!   `MUSE_NO_AUTO_UPDATE=1`.
//! - The environment is an allowlist ([`crate::child_env::MUSE`]): `META_API_KEY` is not on it, so
//!   the owner's own Muse login is what a run uses, never a key that happens to be exported to the
//!   daemon. A bot's own secret named `META_API_KEY` still reaches its runs.
//! - Isolation from the owner's personal tools is NOT claimed (`isolates_user_config` is false):
//!   `muse serve` has no flag that leaves out the owner's skills, plugins, hooks or MCP servers.
//!   A workspace's own skills and rules load only if the owner's trust store trusts it.
//! - Process records: the host a run spawns is reported through `ExecControls::processes` at
//!   once, in its own process group; at teardown its descendants (a tool's, a subagent's) are
//!   traced and ended with it. The short-lived probe host is bounded by its own timeout and
//!   reaped before the probe returns.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use reno_proto::{
    AccessMode, ExecEvent, ExecSpec, ExecutorCaps, ExecutorDescriptor, ExecutorId, HealthProbe,
    ModelCatalog, ModelInfo, SandboxLevel, SessionRef, SignedIn, Steering, StopReason,
    VersionRange,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::exec::{
    Answer, Decision, ExecAnswer, ExecControls, ExecError, Executor, Open, OpenRequests, Resolved,
    UnknownFrames, probe, require_access, require_sandbox,
};
use crate::process::owned::Child as OwnedChild;
use crate::process::{Command, Stdio};
use crate::wire::muse::approval::{self, Requirement};
use crate::wire::muse::frames::{self, Failure, IfBusy, Inbound, Start};
use crate::wire::muse::launch::{self, Mode, Posture};
use crate::wire::muse::{account, catalog, known, normalize};
use crate::{HarnessError, HarnessId};

mod rpc;

/// The oldest Muse Code this driver runs. Checked: 1.4.23 only (see the module docs). An older
/// one is refused before a run instead of failing in the middle of one.
pub const MIN_CLI_VERSION: &str = "1.4.23";

/// The newest Muse Code this driver was checked against: 1.4.23, the one installed where it was
/// written. A newer one runs but is reported `Untested`.
pub const VERIFIED_CLI_VERSION: &str = "1.4.23";

/// The executor contract version this driver implements.
const CONTRACT_VERSION: u32 = 1;

/// The id this executor registers under.
pub const EXECUTOR_ID: &str = "muse";

static ID: LazyLock<ExecutorId> = LazyLock::new(|| match EXECUTOR_ID.parse() {
    Ok(id) => id,
    Err(error) => unreachable!("{EXECUTOR_ID} is a valid executor id: {error}"),
});

/// What the stream carries before the consumer reads.
const EVENT_CAPACITY: usize = 256;

/// How long Muse may take to start, open the session and answer the first prompt's command.
const STARTUP_BUDGET: Duration = Duration::from_secs(120);

/// Questions and approvals open on a person at once (ARCH-11). More are refused.
const MAX_OPEN_REQUESTS: usize = 16;

/// How long a run waits, once its last background work ended, for the turn Muse starts on its own.
const WAKE_GRACE: Duration = Duration::from_secs(3);

/// The Muse CLI to launch: `MUSE_EXECUTABLE`, then our own PATH, the login-shell PATH and the
/// known install locations. The first match wins: a candidate's version is never asked here, since
/// the launcher script that `muse` is updates itself on any start.
pub fn resolve_muse_executable() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("MUSE_EXECUTABLE").filter(|p| !p.is_empty()) {
        return crate::executable::validate_native_override(&PathBuf::from(p)).ok();
    }
    let mut extra = Vec::new();
    if let Some(home) = crate::executable::home_dir() {
        extra.push(home.join(".local").join("bin").join("muse"));
    }
    extra.push(PathBuf::from("/opt/homebrew/bin/muse"));
    extra.push(PathBuf::from("/usr/local/bin/muse"));
    crate::executable::find_on_paths_with(
        "muse",
        extra,
        &|key| std::env::var_os(key),
        crate::shell_env::user_path(),
    )
}

fn resolve_executable(executable: Option<&Path>) -> Result<PathBuf, ExecError> {
    let not_installed = |error: HarnessError| ExecError::NotInstalled(error.to_string());
    if let Some(p) = executable {
        return crate::executable::validate_native_override(p).map_err(not_installed);
    }
    if let Some(p) = std::env::var_os("MUSE_EXECUTABLE")
        && !p.is_empty()
    {
        return crate::executable::validate_native_override(&PathBuf::from(p))
            .map_err(not_installed);
    }
    resolve_muse_executable().ok_or_else(|| {
        ExecError::NotInstalled(
            "muse (searched PATH, the login shell's PATH, ~/.local/bin, /opt/homebrew/bin and \
             /usr/local/bin; set MUSE_EXECUTABLE to override; install it with \
             `curl -fsSL https://dev.meta.ai/install.sh | sh`)"
                .into(),
        )
    })
}

/// `Some(why)` when `version` is a Muse Code older than [`MIN_CLI_VERSION`]; an unknown version
/// is given the benefit of the doubt.
fn too_old_message(version: Option<&str>) -> Option<String> {
    let found = parse_version(version?)?;
    let min = parse_version(MIN_CLI_VERSION)?;
    (found < min).then(|| {
        format!(
            "Muse Code {found} is too old; update it to {min} or newer (an interactive `muse` \
             updates itself)"
        )
    })
}

/// `1.4.23` (the launcher's `1.4.23-R8977.1` build suffix counts as a pre-release and is cut).
fn parse_version(text: &str) -> Option<semver::Version> {
    let core = text.split(['-', ' ', '(']).next()?.trim();
    semver::Version::parse(core).ok()
}

/// The Muse Code executor. Construct with [`MuseExecutor::new`]; tests point it at a fake Muse
/// with [`MuseExecutor::with_executable`].
///
/// # Access modes
///
/// [`AccessMode::Supervised`] and [`AccessMode::FullAccess`], both with the unrestricted sandbox
/// level: Muse's own sandbox stays on in either (a run never has less than the owner's Muse
/// gives it), and every approval it raises goes to the kernel, which allows a full-access run's
/// after the built-in protected paths and policy rules. [`AccessMode::AutoAcceptEdits`] and the
/// read-only and workspace-write sandbox levels are not claimed: nothing here has seen a tool
/// run, so nothing here says what Muse's profiles allow.
pub struct MuseExecutor {
    executable: Option<PathBuf>,
    provider: String,
    interrupt_grace: Duration,
    kill_grace: Duration,
    wake_grace: Duration,
    /// The CLI version `probe` last read, for the descriptor.
    cli_version: Arc<Mutex<Option<String>>>,
    /// The models Muse last listed, for the descriptor.
    models: Arc<Mutex<Option<Vec<ModelInfo>>>>,
    /// How long `probe` lets its short-lived host take in all.
    probe_budget: Duration,
    tracker: TaskTracker,
    unknown: UnknownFrames,
}

impl std::fmt::Debug for MuseExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MuseExecutor")
            .field("executable", &self.executable)
            .finish_non_exhaustive()
    }
}

impl Default for MuseExecutor {
    fn default() -> Self {
        Self {
            executable: None,
            provider: launch::PROVIDER.to_owned(),
            interrupt_grace: Duration::from_secs(5),
            kill_grace: Duration::from_secs(3),
            wake_grace: WAKE_GRACE,
            cli_version: Arc::new(Mutex::new(None)),
            models: Arc::new(Mutex::new(None)),
            probe_budget: probe::WIRE_BUDGET,
            tracker: TaskTracker::new(),
            unknown: UnknownFrames::new(),
        }
    }
}

impl MuseExecutor {
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

    /// Tune the interrupt and the SIGTERM to SIGKILL graces.
    #[must_use]
    pub fn with_graces(mut self, interrupt_grace: Duration, kill_grace: Duration) -> Self {
        self.interrupt_grace = interrupt_grace;
        self.kill_grace = kill_grace;
        self
    }

    /// How long a run waits for the turn Muse starts after its last background work.
    #[must_use]
    pub fn with_wake_grace(mut self, grace: Duration) -> Self {
        self.wake_grace = grace;
        self
    }

    /// Bound the host `probe` starts (tests shrink it; default 20 s).
    #[must_use]
    pub fn with_probe_budget(mut self, budget: Duration) -> Self {
        self.probe_budget = budget;
        self
    }

    /// The model provider the host is served and the session started with. `meta` unless a test
    /// (the live one plays the whole run over Muse's own `echo` provider, a mock model) says
    /// otherwise.
    #[must_use]
    pub fn with_provider(mut self, provider: impl Into<String>) -> Self {
        self.provider = provider.into();
        self
    }

    /// Whether the CLI can be launched now.
    pub fn installed(&self) -> bool {
        resolve_executable(self.executable.as_deref()).is_ok()
    }

    /// What a read-only host says: its version, who is signed in and the model catalog. One
    /// short-lived `muse serve --disable-shell --disable-write --no-session-log`, reaped before
    /// this returns. No prompt is sent and no model is asked anything.
    async fn query_host(&self, exe: &Path, budget: Duration) -> Result<HostFacts, HarnessError> {
        let mut host = Host::spawn(exe, &self.provider, Mode::Probe, None, &Default::default())?;
        let ask = async {
            let hello = host.initialize(true, false).await?;
            let account = host.call_ok("account/read", json!({})).await?;
            let models = host.call_ok("model/list", json!({})).await?;
            Ok::<_, HarnessError>(HostFacts {
                version: hello.version,
                account,
                models,
            })
        };
        let result = tokio::time::timeout(budget, ask).await;
        host.transport.close_input();
        host.child.shutdown(self.kill_grace).await;
        match result {
            Ok(inner) => inner,
            Err(_) => Err(HarnessError::Protocol("Muse probe timed out".into())),
        }
    }
}

/// What a probe host said.
struct HostFacts {
    version: Option<String>,
    account: Value,
    models: Value,
}

#[async_trait]
impl Executor for MuseExecutor {
    fn maintenance(&self) -> Option<crate::exec::maintenance::MaintenanceDecl> {
        crate::exec::maintenance::declaration(&HarnessId::Muse)
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
            .unwrap_or_else(catalog::default_models);
        ExecutorDescriptor {
            id: ID.clone(),
            contract_version: CONTRACT_VERSION,
            adapter_version: match cli {
                Some(cli) => format!(
                    "reno-harness {} muse driver (CLI {cli})",
                    env!("CARGO_PKG_VERSION")
                ),
                None => format!("reno-harness {} muse driver", env!("CARGO_PKG_VERSION")),
            },
            cli_verified: VersionRange {
                min: MIN_CLI_VERSION.into(),
                verified: VERIFIED_CLI_VERSION.into(),
            },
            caps: ExecutorCaps {
                // The effort rides each turn, set when the run starts; not live.
                live_effort: None,
                live_model_options: None,
                // A steering message lands at the running turn's next model step.
                steering: Steering::StepBoundary,
                resume: true,
                access_modes: vec![AccessMode::Supervised, AccessMode::FullAccess],
                sandbox_levels: vec![SandboxLevel::Unrestricted],
                mcp_injection: true,
                structured_output: false,
                // Done is the host's own `turn/completed`.
                deterministic_turn_end: true,
                plan_mode: false,
                models,
                model_switch: false,
                // `muse serve` has no flag that leaves out the owner's skills, plugins, hooks or
                // MCP servers: not claimed.
                isolates_user_config: false,
                model_aliases: vec![],
            },
        }
    }

    /// Where the CLI is, its version against the verified range, and, from one short-lived
    /// read-only host with no prompt, whether Muse is signed in and which models it offers. A
    /// host that cannot be asked leaves the sign-in `Unknown` and the models at the last live
    /// list, else the curated one.
    async fn probe(&self) -> HealthProbe {
        let overridden = self.executable.is_some()
            || std::env::var_os("MUSE_EXECUTABLE").is_some_and(|value| !value.is_empty());
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
                None => ModelCatalog::curated(catalog::default_models()),
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
        let (version, signed_in, models) = match self.query_host(&exe, self.probe_budget).await {
            Ok(facts) => {
                let listed = catalog::model_infos(&facts.models);
                let models = if listed.is_empty() {
                    fallback()
                } else {
                    *self.models.lock().unwrap_or_else(PoisonError::into_inner) =
                        Some(listed.clone());
                    ModelCatalog::live(listed)
                };
                (facts.version, account::signed_in(&facts.account), models)
            }
            Err(error) => {
                tracing::debug!(%error, "the muse probe failed");
                (None, SignedIn::Unknown, fallback())
            }
        };
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
        require_access(
            spec.access,
            &[AccessMode::Supervised, AccessMode::FullAccess],
        )?;
        require_sandbox(spec.sandbox, &[SandboxLevel::Unrestricted])?;
        if spec.output_schema.is_some() {
            return Err(ExecError::UnsupportedOutputSchema);
        }
        if let Some(session) = &spec.resume
            && !launch::valid_session_id(&session.0)
        {
            return Err(ExecError::InvalidSpec(format!(
                "{:?} is not a Muse session id",
                session.0
            )));
        }
        if spec.cwd.to_str().is_none() {
            return Err(ExecError::InvalidSpec("cwd is not valid UTF-8".into()));
        }
        let exe = resolve_executable(self.executable.as_deref())?;
        let ExecControls {
            steering,
            cancel,
            answers,
            processes,
            // Neither a model switch nor a live config change is claimed: not read.
            ..
        } = controls;

        let host = match Host::spawn(&exe, &self.provider, Mode::Run, Some(&spec.cwd), &spec.env) {
            Ok(host) => host,
            // Any failure to launch is a failed run, not an ineligible executor.
            Err(error) => return Ok(failed_stream(&error.to_string())),
        };
        let mut host = host;
        host.tail = crate::StderrTail::for_run(&spec.env);
        host.watch_stderr(&self.tracker);
        // Reported at once: a daemon that dies from here on still leaves its row.
        #[cfg(unix)]
        if let Some(record) = host.child.record() {
            let _ = processes.report(record).await;
        }
        #[cfg(not(unix))]
        let _ = processes;

        let (event_tx, event_rx) = mpsc::channel::<ExecEvent>(EVENT_CAPACITY);
        let run = Run {
            host,
            spec,
            provider: self.provider.clone(),
            event_tx,
            steering,
            answers,
            cancel,
            interrupt_grace: self.interrupt_grace,
            kill_grace: self.kill_grace,
            wake_grace: self.wake_grace,
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

/// A stream of one `Done { Error }`: the run that could not launch.
fn failed_stream(why: &str) -> BoxStream<'static, ExecEvent> {
    let diagnostic = crate::redact_secrets(why);
    futures::stream::once(async move {
        ExecEvent::Done {
            stop: StopReason::Error,
            result: None,
            diagnostic: Some(diagnostic),
            reason: None,
        }
    })
    .boxed()
}

// ---------------------------------------------------------------------------
// The host process
// ---------------------------------------------------------------------------

/// One `muse serve` process and its pipe.
struct Host {
    child: OwnedChild,
    transport: rpc::Transport,
    tail: crate::StderrTail,
    stderr: Option<tokio::process::ChildStderr>,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
    /// When the process was seen to have exited; frames it left behind are still read until then.
    exit_deadline: Option<Instant>,
    /// Whether the host takes session-scoped MCP servers (known after `initialize`).
    session_mcp: bool,
}

impl Drop for Host {
    fn drop(&mut self) {
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }
}

impl Host {
    fn spawn(
        exe: &Path,
        provider: &str,
        mode: Mode,
        cwd: Option<&Path>,
        run_env: &std::collections::BTreeMap<String, String>,
    ) -> Result<Self, HarnessError> {
        let mut cmd = Command::new(exe);
        crate::child_env::restrict(&mut cmd, crate::child_env::MUSE);
        crate::compose_child_path(&mut cmd, exe);
        // The allowlisted environment of the run (the bot's secrets, Reno's `RENO_*` run
        // variables).
        crate::apply_run_env(&mut cmd, run_env);
        // After the run's own variables: a bot cannot turn the launcher's update back on.
        let (name, value) = launch::NO_AUTO_UPDATE;
        cmd.env(name, value);
        cmd.args(launch::serve_args(mode, provider));
        match cwd {
            Some(cwd) if !cwd.as_os_str().is_empty() => {
                cmd.current_dir(cwd);
            }
            _ => {
                cmd.current_dir(std::env::temp_dir());
            }
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(if mode == Mode::Run {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .kill_on_drop(true);
        let mut child = crate::process::owned::spawn_grouped(&mut cmd)
            .map_err(|e| crate::executable::spawn_error(e, cwd, || exe.display().to_string()))?;
        let missing = |what: &str| HarnessError::Protocol(format!("muse child has no {what}"));
        let stdin = child.stdin.take().ok_or_else(|| missing("stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| missing("stdout"))?;
        let stderr = child.stderr.take();
        Ok(Self {
            child,
            transport: rpc::Transport::new(stdin, stdout),
            tail: crate::StderrTail::default(),
            stderr,
            stderr_task: None,
            exit_deadline: None,
            session_mcp: false,
        })
    }

    /// Keep the tail of the host's stderr, for the message of a crash.
    fn watch_stderr(&mut self, tracker: &TaskTracker) {
        let Some(stderr) = self.stderr.take() else {
            self.tail.close();
            return;
        };
        let tail = self.tail.clone();
        self.stderr_task = Some(tracker.spawn(async move {
            let mut lines = crate::lossy_lines::lossy_lines(stderr);
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::debug!(target: "reno_harness::muse", "stderr: {}", tail.scrub(&line));
                tail.push(&line);
            }
            tail.close();
        }));
    }

    fn client(&self) -> rpc::Client {
        self.transport.client.clone()
    }

    /// The next frame, in stdout order. The host's own requests are answered here (what they ask
    /// also arrives as a notification, which is where the run acts on it). An error when the
    /// host's stdout closed or it exited and everything it had written is read: a descendant may
    /// keep stdout open, so EOF alone cannot govern liveness.
    async fn next_frame(&mut self) -> Result<Inbound, HarnessError> {
        loop {
            tokio::select! {
                biased;
                frame = self.transport.incoming.recv() => {
                    let frame = frame
                        .unwrap_or_else(|| Err(HarnessError::Protocol("Muse disconnected".into())))?;
                    if let Inbound::Request { id, method } = &frame {
                        self.transport.client.answer_host(id, method)?;
                    }
                    return Ok(frame);
                }
                () = async {
                    match self.exit_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => return Err(HarnessError::Protocol("Muse exited".into())),
                status = self.child.wait(), if self.exit_deadline.is_none() => {
                    status?;
                    self.exit_deadline = Some(Instant::now() + Duration::from_millis(150));
                }
            }
        }
    }

    /// `initialize` and the `initialized` notice. Rejects an envelope this wire does not speak.
    async fn initialize(
        &mut self,
        experimental: bool,
        session_mcp: bool,
    ) -> Result<frames::Hello, HarnessError> {
        let result = self
            .call_ok(
                "initialize",
                frames::initialize(env!("CARGO_PKG_VERSION"), experimental, session_mcp),
            )
            .await?;
        let hello = frames::hello(&result).map_err(HarnessError::Protocol)?;
        self.session_mcp = hello.session_mcp;
        self.transport.client.notify("initialized")?;
        Ok(hello)
    }

    /// One request and its answer, outside a run: any other frame that arrives meanwhile is
    /// dropped (a probe host sends nothing else).
    async fn call(
        &mut self,
        method: &str,
        params: Value,
        backlog: &mut Vec<Inbound>,
    ) -> Result<Result<Value, Failure>, HarnessError> {
        let id = self.transport.client.request(method, params)?;
        loop {
            match self.next_frame().await? {
                Inbound::Response {
                    id: answered,
                    result,
                } if answered == id => return Ok(result),
                frame => backlog.push(frame),
            }
        }
    }

    /// [`Self::call`] where the host's refusal is an error.
    async fn call_ok(&mut self, method: &str, params: Value) -> Result<Value, HarnessError> {
        let mut backlog = Vec::new();
        self.call(method, params, &mut backlog)
            .await?
            .map_err(|failure| refused(method, &failure))
    }

    /// What to say when the host went away under a run: its exit status and last stderr lines.
    async fn crash_report(&mut self) -> String {
        self.tail.wait_closed().await;
        let status = match tokio::time::timeout(Duration::from_millis(500), self.child.wait()).await
        {
            Ok(Ok(status)) => Some(status),
            _ => self.child.try_wait().ok().flatten(),
        };
        crate::crash_message("muse", status, &self.tail)
    }
}

/// A refusal as the run's error.
fn refused(method: &str, failure: &Failure) -> HarnessError {
    HarnessError::Protocol(format!("Muse {method} failed: {}", failure.message))
}

// ---------------------------------------------------------------------------
// One run
// ---------------------------------------------------------------------------

struct Run {
    host: Host,
    spec: ExecSpec,
    provider: String,
    event_tx: mpsc::Sender<ExecEvent>,
    steering: mpsc::Receiver<String>,
    answers: mpsc::Receiver<ExecAnswer>,
    cancel: CancellationToken,
    interrupt_grace: Duration,
    kill_grace: Duration,
    wake_grace: Duration,
    unknown: UnknownFrames,
}

/// How the part of a run before its prompt went.
enum Startup {
    Ready(Opened, Vec<Inbound>),
    Failed(String),
    Cancelled,
    ConsumerGone,
}

/// The session a run opened.
struct Opened {
    session: String,
    root: String,
}

type Ended = (StopReason, Option<String>, Option<String>);

impl Run {
    /// The whole run: start, play, end exactly once, tear down.
    async fn drive(mut self) {
        let cancel = self.cancel.clone();
        let startup = tokio::select! {
            () = cancel.cancelled() => Startup::Cancelled,
            result = tokio::time::timeout(STARTUP_BUDGET, self.start_up()) => match result {
                Ok(startup) => startup,
                Err(_) => Startup::Failed("Muse startup timed out".into()),
            },
        };
        let (stop, result, diagnostic) = match startup {
            Startup::Ready(opened, backlog) => {
                let mut player = Player::new(&mut self, opened);
                player.play(backlog).await
            }
            Startup::Failed(why) => (StopReason::Error, None, Some(why)),
            Startup::Cancelled => (StopReason::Interrupted, None, None),
            Startup::ConsumerGone => {
                self.teardown().await;
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
        self.teardown().await;
    }

    /// Handshake, open the session, and say what the run starts as.
    async fn start_up(&mut self) -> Startup {
        let mut backlog = Vec::new();
        match self.set_up(&mut backlog).await {
            Ok((opened, model, note)) => {
                let started = ExecEvent::Started {
                    session: SessionRef(opened.session.clone()),
                    model,
                };
                if self.event_tx.send(started).await.is_err() {
                    return Startup::ConsumerGone;
                }
                if let Some(note) = note
                    && self
                        .event_tx
                        .send(ExecEvent::Text { text: note })
                        .await
                        .is_err()
                {
                    return Startup::ConsumerGone;
                }
                Startup::Ready(opened, backlog)
            }
            Err(why) => Startup::Failed(why),
        }
    }

    /// The handshake and the session: the posture asked for must be the posture read back.
    async fn set_up(
        &mut self,
        backlog: &mut Vec<Inbound>,
    ) -> Result<(Opened, String, Option<String>), String> {
        let mcp = self.spec.mcp.clone();
        let resume = self.spec.resume.clone();
        let model = (!catalog::is_default_model(self.spec.model.as_deref()))
            .then(|| self.spec.model.clone().unwrap_or_default());
        let hello = match self.host.initialize(false, !mcp.is_empty()).await {
            Ok(hello) => hello,
            Err(error) => return Err(self.explain(error).await),
        };
        if let Some(why) = too_old_message(hello.version.as_deref()) {
            return Err(why);
        }
        let config = if mcp.is_empty() {
            None
        } else if hello.session_mcp {
            launch::session_config(&mcp)
        } else {
            // Running without Reno's own tools would be a different run: fail instead.
            return Err(
                "this Muse Code build cannot take session MCP servers, which Reno's \
                        tools ride on"
                    .to_owned(),
            );
        };
        let root = tokio::fs::canonicalize(&self.spec.cwd)
            .await
            .map_err(|e| format!("cannot resolve the working directory: {e}"))?
            .to_string_lossy()
            .into_owned();
        let posture = Posture::ASK;
        let mut note = None;
        let mut session = None;

        if let Some(wanted) = &resume {
            let params = frames::session_resume(
                &wanted.0,
                &rpc::command_id(),
                posture.profile,
                model.as_deref(),
                config.as_ref(),
            );
            match self.host.call("session/resume", params, backlog).await {
                Ok(Ok(result)) => {
                    posture.confirmed_profile(&result["session"])?;
                    // A resumed session keeps the approval mode it last had: set this run's.
                    let mode = frames::set_approval_mode(
                        &wanted.0,
                        &rpc::command_id(),
                        posture.approval_mode,
                    );
                    let applied = match self
                        .host
                        .call("session/setApprovalMode", mode, backlog)
                        .await
                    {
                        Ok(Ok(applied)) => applied,
                        Ok(Err(failure)) => {
                            return Err(refused("session/setApprovalMode", &failure).to_string());
                        }
                        Err(error) => return Err(self.explain(error).await),
                    };
                    posture.mode_applied(&applied)?;
                    session = Some((wanted.0.clone(), result));
                }
                Ok(Err(failure))
                    if matches!(failure.kind.as_str(), "sessionNotFound" | "notFound") =>
                {
                    note = Some(format!(
                        "Muse did not find session {}; starting a new session without the \
                         previous context.\n",
                        wanted.0
                    ));
                }
                Ok(Err(failure)) => return Err(refused("session/resume", &failure).to_string()),
                Err(error) => return Err(self.explain(error).await),
            }
        }
        let (id, result) = match session {
            Some(found) => found,
            None => {
                let id = rpc::command_id();
                let command = rpc::command_id();
                let params = frames::session_start(&Start {
                    session: &id,
                    command: &command,
                    root: &root,
                    provider: &self.provider,
                    model: model.as_deref(),
                    posture,
                    config: config.as_ref(),
                });
                let result = match self.host.call("session/start", params, backlog).await {
                    Ok(Ok(result)) => result,
                    Ok(Err(failure)) => {
                        return Err(refused("session/start", &failure).to_string());
                    }
                    Err(error) => return Err(self.explain(error).await),
                };
                if result["session"]["sessionId"] != id.as_str() {
                    return Err("Muse started a different session than it was asked to".into());
                }
                posture.confirmed_start(&result["session"])?;
                (id, result)
            }
        };
        let reported = result["session"]["modelId"]
            .as_str()
            .map(str::to_owned)
            .or(model)
            .unwrap_or_else(|| catalog::DEFAULT_MODEL.to_owned());
        Ok((Opened { session: id, root }, reported, note))
    }

    /// An error from the host, with how it died when the pipe is what broke.
    async fn explain(&mut self, error: HarnessError) -> String {
        let gone = self.host.exit_deadline.is_some()
            || matches!(self.host.child.try_wait(), Ok(Some(_)))
            || tokio::time::timeout(Duration::from_millis(250), self.host.child.wait())
                .await
                .is_ok();
        if gone {
            format!("{error}: {}", self.host.crash_report().await)
        } else {
            error.to_string()
        }
    }

    /// End the host and everything it started: trace its descendants while it still parents them,
    /// close its stdin (its own orderly shutdown), give it a moment, then SIGTERM and SIGKILL to
    /// the process group and the descendants traced.
    async fn teardown(&mut self) {
        #[cfg(unix)]
        let tree = match self.host.child.id() {
            Some(pid) => crate::process::descendants(pid).await,
            None => Vec::new(),
        };
        #[cfg(not(unix))]
        let tree = Vec::new();
        self.host.transport.close_input();
        let _ = tokio::time::timeout(
            self.kill_grace.min(Duration::from_secs(1)),
            self.host.child.wait(),
        )
        .await;
        crate::shutdown_agent(&mut self.host.child, tree, self.kill_grace).await;
        if let Some(task) = self.host.stderr_task.take() {
            task.abort();
        }
    }
}

/// What a command still waiting for its answer is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pending {
    /// The run's opening prompt.
    Opening { turn: String },
    /// A steering message.
    Steer { turn: String },
    /// Anything else: only a refusal matters.
    Other(&'static str),
}

/// A question open on a person.
struct Asked {
    /// The question's params, to read the answers against.
    params: Value,
}

/// The run after it started: what is open, what is waiting, and when it is over.
struct Player<'a> {
    run: &'a mut Run,
    opened: Opened,
    norm: normalize::Normalizer,
    client: rpc::Client,
    pending: HashMap<u64, Pending>,
    /// The turn the run is in, and turns joined into it (queued behind it, or started by a steer
    /// that met an idle host).
    active: Option<String>,
    joined: HashSet<String>,
    /// How the last turn that ended, ended.
    last: Option<(StopReason, Option<String>)>,
    /// The opening prompt was accepted.
    opening_answered: bool,
    /// Approvals and questions open on a person.
    requests: OpenRequests<Requirement, Asked>,
    /// Requirements already raised (a repeated `approval/updated` is not asked twice).
    raised: HashSet<String>,
    interrupted: bool,
    /// Why the run cannot go on.
    failed: Option<String>,
    /// Background work counted when the last frame was read.
    background: usize,
    /// Once the last background work ended while idle: the turn Muse starts by itself is awaited
    /// until then.
    wake_until: Option<Instant>,
    steering_open: bool,
    answers_open: bool,
}

impl<'a> Player<'a> {
    fn new(run: &'a mut Run, opened: Opened) -> Self {
        let client = run.host.client();
        Self {
            run,
            opened,
            norm: normalize::Normalizer::new(),
            client,
            pending: HashMap::new(),
            active: None,
            joined: HashSet::new(),
            last: None,
            opening_answered: false,
            requests: OpenRequests::new(),
            raised: HashSet::new(),
            interrupted: false,
            failed: None,
            background: 0,
            wake_until: None,
            steering_open: true,
            answers_open: true,
        }
    }

    async fn emit(&self, event: ExecEvent) -> bool {
        self.run.event_tx.send(event).await.is_ok()
    }

    /// Play the run from the prompt to its end.
    async fn play(&mut self, backlog: Vec<Inbound>) -> Ended {
        let prompt =
            launch::opening_prompt(self.run.spec.instructions.as_deref(), &self.run.spec.prompt);
        let turn = rpc::command_id();
        let effort = self.run.spec.effort.map(catalog::tier);
        let params = frames::turn_start(
            &self.opened.session,
            &turn,
            &prompt,
            effort,
            IfBusy::Queue,
            &self.opened.root,
        );
        match self.client.request("turn/start", params) {
            Ok(id) => {
                self.pending
                    .insert(id, Pending::Opening { turn: turn.clone() });
                self.active = Some(turn);
            }
            Err(error) => return (StopReason::Error, None, Some(error.to_string())),
        }
        for frame in backlog {
            if self.frame(frame).await.is_err() {
                return self.gone().await;
            }
        }
        loop {
            if let Some(ended) = self.ended() {
                return ended;
            }
            let wake = async {
                match self.wake_until {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                frame = self.run.host.next_frame() => match frame {
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
                () = wake => self.wake_until = None,
                () = self.run.cancel.cancelled(), if !self.interrupted => {
                    self.interrupt().await;
                    return self.finish_interrupted();
                }
                () = self.run.event_tx.closed() => {
                    self.refuse_all();
                    return (StopReason::Interrupted, None, None);
                }
            }
        }
    }

    /// Whether the run is over, and how. Over means the host owes nothing for the prompt: every
    /// turn it started or joined ended, no background work is left (or Muse's own next turn is
    /// not coming), and no steering message waits to be handed over.
    fn ended(&mut self) -> Option<Ended> {
        if let Some(why) = self.failed.clone() {
            self.refuse_all();
            return Some((StopReason::Error, None, Some(why)));
        }
        let (stop, diagnostic) = self.last.clone()?;
        let waiting = self
            .pending
            .values()
            .any(|p| matches!(p, Pending::Opening { .. } | Pending::Steer { .. }));
        if !self.opening_answered || waiting || self.active.is_some() || !self.joined.is_empty() {
            return None;
        }
        if stop != StopReason::Completed {
            // A turn that failed or was stopped ends the run: background work goes with it.
            self.refuse_all();
            return Some((stop, None, diagnostic));
        }
        // Background work, or the turn Muse starts by itself when it ended, is awaited.
        if self.background > 0 || self.wake_until.is_some_and(|at| at > Instant::now()) {
            return None;
        }
        // A steering message queued while the run settled starts a turn of its own.
        let mut handed = false;
        while self.steering_open
            && !self.interrupted
            && let Ok(text) = self.run.steering.try_recv()
        {
            self.steer(&text);
            handed = true;
        }
        if handed {
            return None;
        }
        self.refuse_all();
        Some((StopReason::Completed, self.norm.final_text(), None))
    }

    /// The host went away, or its pipe broke, before the run ended.
    async fn gone(&mut self) -> Ended {
        self.refuse_all();
        if self.interrupted {
            return (StopReason::Interrupted, None, None);
        }
        let crash = self.run.host.crash_report().await;
        (StopReason::Error, None, Some(crash))
    }

    fn finish_interrupted(&mut self) -> Ended {
        self.refuse_all();
        (StopReason::Interrupted, None, None)
    }

    fn owns(&self, turn: &str) -> bool {
        self.active.as_deref() == Some(turn) || self.joined.contains(turn)
    }

    /// A steering message: one command that steers the running turn or starts one when idle.
    fn steer(&mut self, text: &str) {
        let turn = rpc::command_id();
        let params = frames::turn_start(
            &self.opened.session,
            &turn,
            text,
            None,
            IfBusy::Steer,
            &self.opened.root,
        );
        match self.client.request("turn/start", params) {
            Ok(id) => {
                self.pending
                    .insert(id, Pending::Steer { turn: turn.clone() });
                if self.active.is_none() {
                    // Idle: this prompt starts the next turn, under its own id.
                    self.begin(turn);
                }
            }
            Err(error) => tracing::warn!(target: "reno_harness::muse", "steer not sent: {error}"),
        }
    }

    fn begin(&mut self, turn: String) {
        self.active = Some(turn);
        self.joined.clear();
        self.wake_until = None;
        // `last` stays: if this turn never begins (a refused steer) the run still ends as the
        // turn before it did; the turn's own completion replaces it.
        self.norm.next_turn();
    }

    /// A decision or a refusal for an open approval or question, from the kernel.
    fn answer(&mut self, answer: ExecAnswer) {
        let request_id = answer.request_id.clone();
        match self.requests.resolve(answer) {
            Resolved::Decision(requirement, decision, message) => {
                // What Reno cannot read is never allowed: an unknown kind of subject is refused
                // whatever the kernel decided.
                let (decision, message) = if decision == Decision::Allow && !requirement.known {
                    (
                        Decision::Deny,
                        Some(
                            "Reno does not approve a kind of request it does not know.".to_owned(),
                        ),
                    )
                } else {
                    (decision, message)
                };
                let choice = match decision {
                    Decision::Allow => requirement.allow().or_else(|| requirement.deny()),
                    Decision::Deny => requirement.deny(),
                };
                let Some(choice) = choice else {
                    self.failed = Some(
                        "Muse offered no answer to an approval that Reno can give: nothing \
                         that allows once or refuses once"
                            .into(),
                    );
                    return;
                };
                let params = approval::decide(
                    &self.opened.session,
                    &rpc::command_id(),
                    &requirement,
                    choice,
                    message.as_deref(),
                );
                self.send_other("approval/decide", params);
            }
            Resolved::Input(asked, answers) => {
                if answers.is_empty() {
                    self.cancel_question(
                        &asked.params,
                        "The user dismissed the questions without answering.",
                    );
                } else {
                    let out = approval::answers(&asked.params, &answers);
                    let params = approval::answer_params(
                        &self.opened.session,
                        &rpc::command_id(),
                        crate::wire::muse::frames::string(&asked.params, "userInputId"),
                        out,
                    );
                    self.send_other("userInput/answer", params);
                }
            }
            Resolved::WrongKind | Resolved::Unknown => {
                tracing::debug!(target: "reno_harness::muse", "answer for {request_id} matches no open request");
            }
        }
    }

    /// Tell Muse no to `requirement`, with `why`, if it offers a refusal that saves nothing.
    fn refuse(&mut self, requirement: &Requirement, why: &str) {
        if let Some(choice) = requirement.deny() {
            let params = approval::decide(
                &self.opened.session,
                &rpc::command_id(),
                requirement,
                choice,
                Some(why),
            );
            self.send_other("approval/decide", params);
        }
    }

    fn send_other(&mut self, method: &'static str, params: Value) {
        match self.client.request(method, params) {
            Ok(id) => {
                self.pending.insert(id, Pending::Other(method));
            }
            Err(error) => {
                tracing::warn!(target: "reno_harness::muse", "{method} not sent: {error}");
            }
        }
    }

    fn cancel_question(&mut self, params: &Value, reason: &str) {
        let id = crate::wire::muse::frames::string(params, "userInputId").to_owned();
        let params = approval::cancel_params(&self.opened.session, &rpc::command_id(), &id, reason);
        self.send_other("userInput/cancel", params);
    }

    /// Refuse every request still open: a denial for each approval, a decline for each question.
    fn refuse_all(&mut self) {
        for (_, open) in self.requests.drain() {
            match open {
                Open::Approval(requirement) => {
                    if let Some(choice) = requirement.deny() {
                        let params = approval::decide(
                            &self.opened.session,
                            &rpc::command_id(),
                            &requirement,
                            choice,
                            Some("The run ended."),
                        );
                        self.send_other("approval/decide", params);
                    }
                }
                Open::Input(asked) => {
                    self.cancel_question(&asked.params, "The run ended before this was answered.");
                }
            }
        }
    }

    /// Forget an open request the host settled itself.
    fn forget(&mut self, request_id: &str) {
        let _ = self.requests.resolve(ExecAnswer {
            request_id: request_id.to_owned(),
            answer: Answer::Decision(Decision::Deny, None),
        });
        let _ = self.requests.resolve(ExecAnswer {
            request_id: request_id.to_owned(),
            answer: Answer::Input(Vec::new()),
        });
    }

    /// Stop the run: refuse what is open, interrupt every turn of the run, and give the host its
    /// grace to confirm.
    async fn interrupt(&mut self) {
        self.interrupted = true;
        self.refuse_all();
        let mut turns: Vec<String> = self.joined.iter().cloned().collect();
        turns.extend(self.active.clone());
        for turn in turns {
            let params = frames::turn_interrupt(&self.opened.session, &rpc::command_id(), &turn);
            self.send_other("turn/interrupt", params);
        }
        let grace = self.run.interrupt_grace;
        let _ = tokio::time::timeout(grace, async {
            while self.active.is_some() || !self.joined.is_empty() {
                match self.run.host.next_frame().await {
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

    /// One frame from the host. `Err` when the consumer is gone.
    async fn frame(&mut self, frame: Inbound) -> Result<(), ()> {
        match frame {
            Inbound::Response { id, result } => self.response(id, result).await,
            Inbound::Notification { method, params } => self.notification(&method, params).await,
            Inbound::Request { method, .. } => {
                if !known::ANSWERED_REQUESTS.contains(&method.as_str()) {
                    self.run.unknown.note(&format!("request:{method}"));
                }
                Ok(())
            }
        }
    }

    async fn response(&mut self, id: u64, result: Result<Value, Failure>) -> Result<(), ()> {
        let Some(pending) = self.pending.remove(&id) else {
            return Ok(());
        };
        match (pending, result) {
            (Pending::Opening { turn }, Ok(ack)) => {
                self.opening_answered = true;
                self.absorb(&turn, &ack);
            }
            (Pending::Steer { turn }, Ok(ack)) => self.absorb(&turn, &ack),
            (Pending::Opening { .. }, Err(failure)) => {
                self.failed = Some(format!(
                    "Muse could not take the prompt: {}",
                    failure.message
                ));
            }
            (Pending::Steer { turn }, Err(failure)) => {
                if self.active.as_deref() == Some(&turn) {
                    // The turn this message would have started never began.
                    self.active = None;
                }
                let text = format!(
                    "\n[Muse did not take the steering message: {}]\n",
                    failure.message
                );
                if !self.emit(ExecEvent::Text { text }).await {
                    return Err(());
                }
            }
            (Pending::Other(method), Err(failure)) => {
                tracing::warn!(
                    target: "reno_harness::muse",
                    %method, kind = %failure.kind, "{}", failure.message
                );
            }
            (Pending::Other(_), Ok(_)) => {}
        }
        Ok(())
    }

    /// Which turn a prompt's answer says the prompt went into.
    fn absorb(&mut self, minted: &str, ack: &Value) {
        let absorbed = frames::string(ack, "turnId").to_owned();
        if absorbed.is_empty() || self.owns(&absorbed) {
            return;
        }
        let idle = self.active.is_none() || self.active.as_deref() == Some(minted);
        if frames::string(ack, "disposition") != "queued" && idle {
            // An idle prompt started a turn, or went into one that was already running (a wake
            // not yet announced): that turn is the run's now.
            self.active = Some(absorbed);
            self.wake_until = None;
        } else {
            // Queued behind the running turn: Muse runs it next.
            self.joined.insert(absorbed);
        }
    }

    async fn notification(&mut self, method: &str, params: Value) -> Result<(), ()> {
        if !known::is_known_notification(method) {
            self.run.unknown.note(&format!("method:{method}"));
            return Ok(());
        }
        if frames::string(&params, "sessionId") != self.opened.session {
            return Ok(());
        }
        let turn = params["turnId"]
            .as_str()
            .or_else(|| params["item"]["turnId"].as_str())
            .map(str::to_owned);
        let background_before = self.norm.background();
        let result = self.dispatch(method, &params, turn).await;
        self.background = self.norm.background();
        for kind in self.norm.take_unknown() {
            self.run.unknown.note(&kind);
        }
        // Background work ended while idle: Muse starts a turn of its own for it.
        if background_before > 0 && self.background == 0 && self.active.is_none() {
            self.wake_until = Some(Instant::now() + self.run.wake_grace);
        }
        result
    }

    async fn dispatch(
        &mut self,
        method: &str,
        params: &Value,
        turn: Option<String>,
    ) -> Result<(), ()> {
        match method {
            "turn/started" => {
                let Some(turn) = turn else { return Ok(()) };
                if self.active.as_deref() == Some(turn.as_str()) {
                    return Ok(());
                }
                // A turn queued behind this one has begun, and the run was idle: it is the
                // run's now.
                if self.active.is_none() && self.joined.remove(&turn) {
                    self.active = Some(turn);
                    self.wake_until = None;
                    return Ok(());
                }
                if self.owns(&turn) {
                    return Ok(());
                }
                if self.active.is_some() {
                    self.joined.insert(turn);
                } else if !self.interrupted {
                    // A wake: Muse resumed on its own (background work ended).
                    self.begin(turn);
                }
            }
            "item/started" | "item/updated" | "item/completed" => {
                let item = &params["item"];
                let ours = turn.as_deref().is_none_or(|t| self.owns(t));
                let events = if self.active.is_none() || !ours {
                    // Background work finishing outside a turn still counts down.
                    self.norm.background_item(item)
                } else {
                    self.norm.item(item)
                };
                for event in events {
                    if !self.emit(event).await {
                        return Err(());
                    }
                }
            }
            "item/delta" => {
                if self.active.is_some() {
                    for event in self.norm.delta(params) {
                        if !self.emit(event).await {
                            return Err(());
                        }
                    }
                }
            }
            "turn/completed" => {
                let Some(turn) = turn else { return Ok(()) };
                if !self.owns(&turn) {
                    return Ok(());
                }
                let (stop, diagnostic) = normalize::terminal(params, self.interrupted);
                self.last = Some((stop, diagnostic));
                if self.active.as_deref() == Some(&turn) {
                    // A turn joined behind this one runs next and is the run's.
                    let next = self.joined.iter().next().cloned();
                    self.active = next.inspect(|t| {
                        self.joined.remove(t);
                    });
                } else {
                    self.joined.remove(&turn);
                }
                if self.active.is_none() {
                    self.norm.next_turn();
                }
            }
            "session/tokenUsage" if self.active.is_some() => {
                if let Some(usage) = normalize::usage(params)
                    && !self.emit(usage).await
                {
                    return Err(());
                }
            }
            "session/contextUsage" => {
                if !self.emit(normalize::context_usage(params)).await {
                    return Err(());
                }
            }
            "approval/requested" | "approval/updated" => return self.approval(params).await,
            "approval/resolved" => {
                let approval = frames::string(params, "approvalId");
                for index in 0..16 {
                    self.forget(&format!("{approval}#{index}"));
                }
            }
            "userInput/requested" => return self.question(params).await,
            "userInput/settled" => self.forget(frames::string(params, "userInputId")),
            "view/gap" => {
                let text = "\n[Muse dropped part of this session's event stream]\n".to_owned();
                if !self.emit(ExecEvent::Text { text }).await {
                    return Err(());
                }
            }
            "session/closed" if !self.interrupted && self.last.is_none() => {
                self.failed = Some(format!(
                    "Muse closed the session ({})",
                    params["reason"].as_str().unwrap_or("no reason given")
                ));
            }
            _ => {}
        }
        Ok(())
    }

    /// An approval requirement to decide: raised once, to the kernel.
    async fn approval(&mut self, params: &Value) -> Result<(), ()> {
        let Some(requirement) = approval::requirement(params) else {
            return Ok(());
        };
        if !self.raised.insert(requirement.request_id.clone()) {
            return Ok(());
        }
        if self.interrupted || self.requests.len() >= MAX_OPEN_REQUESTS {
            // Nobody is asked: Muse is told no, instead of waiting on it for ever.
            self.refuse(&requirement, "Too many approvals were open at once.");
            return Ok(());
        }
        let ask = ExecEvent::NeedsApproval {
            request_id: requirement.request_id.clone(),
            subject: requirement.subject.clone(),
        };
        self.requests
            .open_approval(requirement.request_id.clone(), requirement);
        if self.emit(ask).await {
            Ok(())
        } else {
            Err(())
        }
    }

    /// A question for the person.
    async fn question(&mut self, params: &Value) -> Result<(), ()> {
        let id = frames::string(params, "userInputId").to_owned();
        let questions = approval::questions(params);
        if id.is_empty() {
            return Ok(());
        }
        if self.interrupted || questions.is_empty() || self.requests.len() >= MAX_OPEN_REQUESTS {
            self.cancel_question(params, "This run cannot ask the user.");
            return Ok(());
        }
        let ask = ExecEvent::NeedsInput {
            request_id: id.clone(),
            questions,
        };
        self.requests.open_input(
            id,
            Asked {
                params: params.clone(),
            },
        );
        if self.emit(ask).await {
            Ok(())
        } else {
            Err(())
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_muse_older_than_the_checked_one_is_refused_and_an_unknown_version_is_not() {
        assert!(too_old_message(Some("1.4.3")).is_some_and(|m| m.contains("1.4.23")));
        assert!(too_old_message(Some("1.4.22")).is_some());
        assert!(too_old_message(Some("1.4.23")).is_none());
        assert!(too_old_message(Some("1.4.23-R8977.1")).is_none());
        assert!(too_old_message(Some("1.5.0")).is_none());
        assert!(too_old_message(Some("2.0.0")).is_none());
        assert!(too_old_message(None).is_none());
        assert!(too_old_message(Some("not a version")).is_none());
    }

    #[test]
    fn the_launchers_version_line_reads_as_the_version() {
        assert_eq!(
            parse_version("Muse Code 1.4.23 (1.4.23-R8977.1)"),
            None,
            "only the bare version of the wire is parsed"
        );
        assert_eq!(
            parse_version("1.4.23"),
            Some(semver::Version::new(1, 4, 23))
        );
        assert_eq!(
            parse_version("1.4.23-R8977.1"),
            Some(semver::Version::new(1, 4, 23))
        );
    }

    #[test]
    fn the_descriptor_claims_only_what_the_driver_does() {
        let caps = MuseExecutor::new().descriptor().caps;
        assert_eq!(
            caps.access_modes,
            [AccessMode::Supervised, AccessMode::FullAccess]
        );
        assert_eq!(caps.sandbox_levels, [SandboxLevel::Unrestricted]);
        assert_eq!(caps.steering, Steering::StepBoundary);
        assert!(caps.resume && caps.mcp_injection && caps.deterministic_turn_end);
        assert!(!caps.structured_output && !caps.plan_mode && !caps.model_switch);
        assert!(!caps.isolates_user_config);
        assert!(caps.live_effort.is_none() && caps.live_model_options.is_none());
    }

    #[test]
    fn the_effective_command_name_survives_a_json_round_trip() {
        // `json!` keeps the shape the writer guard reads.
        let frame = frames::request(1, "turn/start", json!({}));
        assert_eq!(frame["method"], "turn/start");
    }
}
