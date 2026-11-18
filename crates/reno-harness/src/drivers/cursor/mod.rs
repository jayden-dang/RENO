//! The native Cursor executor: drives Cursor's agent runtime through the PINNED `@cursor/sdk`
//! by way of a small Reno-owned Node shim (`shim.mjs`, JSONL over stdio), and implements
//! [`Executor`]. Ported from zeron's driver (MIT, commit 68e460af); the shim keeps its logic.
//!
//! NOT over ACP, and NOT over `cursor-agent -p`. Cursor's ACP surface is lossy (subagent
//! transcripts are stripped at the boundary, verified live by zeron), and its print surface
//! (`cursor-agent -p --trust`) is permission-free. The SDK does not wrap the `cursor-agent`
//! binary at all: it is the agent runtime bundled in-process, speaking Cursor's own protocol to
//! its backend with client-side tool execution, so there is no speakable stdio wire to drive
//! from Rust and the shim IS the wire. The SDK needs Node and the managed install below, never
//! `cursor-agent`.
//!
//! VERSION PIN: [`SDK_VERSION`], `@cursor/sdk@1.0.32`, a public beta with about weekly releases.
//! What was checked, and what was not (this build had no Cursor key and no desktop session):
//!
//! - READ, against 1.0.32 and 1.4.1 (the newest at the time), installed with
//!   `npm install --ignore-scripts` into throwaway directories and never run: the typings for
//!   everything the shim maps (`Agent`, `Run`, `InteractionUpdate`, the tool vocabulary,
//!   `ModelListItem`, `SteerAckOutcome`, the options it passes, the stored-credential shape)
//!   and the strings the shim matches on in the 1.0.32 bundle.
//! - NOT RUN: the real SDK. The shim is exercised under Node against a fake SDK
//!   (`tests/cursor_shim.rs`), the driver against a fake shim (`tests/cursor.rs`) and a fake
//!   SDK under the real shim; the live test (`RENO_CURSOR_LIVE=1`, `--ignored`, needs
//!   `CURSOR_API_KEY`) is the one that runs the real thing.
//! - WHY NOT 1.4.1: its `Run.steer` can answer `{outcome: "queued", run?}`, an acknowledgment
//!   that 1.0.32 does not know and that starts a follow-up run the shim would never await, so a
//!   `turn` would end the run early; it adds a `computerUse` tool family that drives the owner's
//!   desktop; and zeron validated 1.0.32 live. The shim now treats an acknowledgment it does not
//!   know as "send the input again as a follow-up" and never offers `recordScreen` or
//!   `computerUse`, so a later bump starts from a safer shim, but it is a deliberate step.
//!
//! - ONE RUN IS ONE `Done`. A run is one shim process and one SDK agent. The first prompt starts
//!   a turn; a steering message is the SDK's native input at the next model step (acknowledged
//!   when consumed) and, when it races the end of a turn, a follow-up turn of the same run. The
//!   run ends at a `turn` frame with no steering outstanding.
//! - Questions: the SDK has no public answer channel for `askQuestion` (its `request` message
//!   carries only an id), so the tool is not offered: a question would block the run forever
//!   with no way to answer it. A known gap against the ACP surface.
//! - Full access and an unrestricted sandbox only: the SDK runs its tools client-side with
//!   approvals ignored, so there is nothing for Reno to decide. Anything else is refused before
//!   spawning.
//! - AUTH: the SDK's credentials are SEPARATE from `cursor-agent login`. A run needs
//!   `CURSOR_API_KEY` in the daemon's environment, or a login the SDK itself stored at
//!   `~/.cursor/sdk/auth.json`. Reno only READS that file to probe "signed in"; it never writes
//!   it, never refreshes it and never starts the SDK's browser login (a person's step), and
//!   every child gets `NO_OPEN_BROWSER=1`.
//! - Isolation (ARCH-15): unless `inherit_user_config`, the SDK is told to load no ambient
//!   settings (`settingSources: []`), so the owner's `~/.cursor/mcp.json`, plugins, team and MDM
//!   layers are invisible. The project layer is never loaded.
//! - Install: `start` and `probe` never install. [`Executor::install`] runs
//!   `npm install --ignore-scripts @cursor/sdk@1.0.32` into Reno's managed adapter directory and
//!   materializes the shim beside the SDK there. Conversation stores live in `cursor-state`
//!   beside it, never under `~/.cursor`.
//! - Process records: the shim is reported through `ExecControls::processes` at once and its
//!   descendants are traced at teardown. The short-lived `models` probe is bounded and reaped.

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::StreamExt;
use futures::stream::BoxStream;
use reno_proto::{
    AccessMode, ExecEvent, ExecSpec, ExecutorCaps, ExecutorDescriptor, ExecutorId, HealthProbe,
    InstallStatus, ModelCatalog, ModelInfo, NotInstalledReason, SandboxLevel, SignedIn, Steering,
    StopReason, VersionRange,
};
use serde_json::Value;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::exec::{
    ExecControls, ExecError, Executor, ProcessSink, UnknownFrames, probe, require_access,
    require_sandbox,
};
use crate::process::owned::Child as OwnedChild;
use crate::process::{Command, Stdio};
use crate::wire::cursor::auth::{self, Credential};
use crate::wire::cursor::catalog::{self, Listed};
use crate::wire::cursor::frames;
use crate::wire::cursor::normalize::{Normalizer, Signal, Turn};
use crate::wire::cursor::state::valid_agent_id;
use crate::{HarnessError, HarnessId};

mod pipe;
mod state;

macro_rules! sdk_version {
    () => {
        "1.0.32"
    };
}

/// The SDK this driver runs and was read against (see the module docs). A later one is never
/// installed by Reno on its own: the pin moves with a Reno release.
pub const SDK_VERSION: &str = sdk_version!();

/// The npm package and version the managed install fetches.
const SDK_PIN: &str = concat!("@cursor/sdk@", sdk_version!());

/// The oldest Node the SDK runs on (its `engines`).
const MIN_NODE: semver::Version = semver::Version::new(22, 13, 0);

/// The shim's file name; a build's copy is content-addressed (`reno-cursor-shim-<hash>.mjs`).
const SHIM_NAME: &str = "reno-cursor-shim.mjs";
const SHIM_SOURCE: &str = include_str!("shim.mjs");

/// The executor contract version this driver implements.
const CONTRACT_VERSION: u32 = 1;

/// The id this executor registers under.
pub const EXECUTOR_ID: &str = "cursor";

static ID: LazyLock<ExecutorId> = LazyLock::new(|| match EXECUTOR_ID.parse() {
    Ok(id) => id,
    Err(error) => unreachable!("{EXECUTOR_ID} is a valid executor id: {error}"),
});

/// What the stream carries before the consumer reads.
const EVENT_CAPACITY: usize = 256;

/// How long the shim may take to say `ready`: it creates or resumes the agent, which asks the
/// backend for its models and can wait out a rate limit (up to about a minute).
const STARTUP_BUDGET: Duration = Duration::from_secs(180);

/// How long a send may go without one SDK update before the shim abandons the turn. A run
/// streams updates from its first model step, so this only bounds a wedged send, never a
/// long-running tool.
const SILENCE_TIMEOUT: Duration = Duration::from_secs(10 * 60);

/// The most of the shim's `models` reply the probe reads.
const MAX_CATALOG_BYTES: u64 = 16 * 1024 * 1024;

/// The `CURSOR_API_KEY` of the environment.
const API_KEY_VAR: &str = "CURSOR_API_KEY";

/// Where a credential is looked for.
#[derive(Debug, Clone)]
enum Auth {
    /// This process's environment and the owner's `~/.cursor/sdk/auth.json`, read only.
    Real,
    /// Fixed by a test: whether a key is in the environment, and the stored login's path.
    Fixed {
        api_key: bool,
        file: Option<PathBuf>,
    },
}

/// What a probe or a start found of the shim and Node.
#[derive(Debug)]
enum Located {
    /// A program to spawn, with the arguments that name the shim.
    Ready {
        program: PathBuf,
        args: Vec<String>,
        shim: PathBuf,
    },
    /// The managed SDK is not installed (the explicit install step puts it there).
    NeedsInstall(String),
    /// Node is missing or too old for the SDK.
    NoNode(String),
    /// There is no data directory to keep the managed install in.
    NoData(String),
}

/// Where to look for the shim, cheap to move onto a blocking thread.
#[derive(Debug, Clone)]
struct Place {
    executable: Option<PathBuf>,
    root: Option<PathBuf>,
}

impl Place {
    /// The program that runs the shim. Blocking (it reads the file system and may run
    /// `node --version`); installs nothing.
    fn locate(&self) -> Located {
        if let Some(shim) = &self.executable {
            return Located::Ready {
                program: shim.clone(),
                args: Vec::new(),
                shim: shim.clone(),
            };
        }
        let Some(root) = &self.root else {
            return Located::NoData(
                "cannot locate a platform data directory for managed adapters".into(),
            );
        };
        let pin = crate::adapter_install::NpmPin::parse(SDK_PIN);
        let Some(shim) =
            crate::adapter_install::installed_shim_in(root, &pin, SHIM_NAME, SHIM_SOURCE)
        else {
            return Located::NeedsInstall(format!(
                "the Cursor SDK ({SDK_PIN}, installed with npm into Reno's adapter directory)"
            ));
        };
        match crate::adapter_install::launch_for_entry(&shim) {
            Ok((program, args)) => {
                if let Some(found) = crate::executable::binary_version(&program)
                    && found < MIN_NODE
                {
                    return Located::NoNode(format!(
                        "Node {found} is too old for the Cursor SDK, which needs Node {MIN_NODE} \
                         or newer"
                    ));
                }
                Located::Ready {
                    program,
                    args,
                    shim,
                }
            }
            Err(error) => Located::NoNode(error.to_string()),
        }
    }
}

/// The Cursor executor. Construct with [`CursorExecutor::new`]; tests point it at a fake shim
/// with [`CursorExecutor::with_executable`] and a throwaway root with
/// [`CursorExecutor::with_adapters_root`].
pub struct CursorExecutor {
    /// Test seam: run this program AS the shim, instead of node and the managed SDK.
    executable: Option<PathBuf>,
    /// The managed adapter root, `RENO_ADAPTERS_DIR` and the platform default when unset. The
    /// conversation stores live beside it.
    adapters: Option<PathBuf>,
    /// Test seam: the npm the explicit install runs.
    npm: Option<PathBuf>,
    auth: Auth,
    interrupt_grace: Duration,
    kill_grace: Duration,
    silence_timeout: Duration,
    probe_budget: Duration,
    /// The catalog `probe` last listed: the picker's models, and what a run fills in.
    listed: Arc<Mutex<Option<Listed>>>,
    tracker: TaskTracker,
    unknown: UnknownFrames,
}

impl std::fmt::Debug for CursorExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CursorExecutor")
            .field("executable", &self.executable)
            .finish_non_exhaustive()
    }
}

impl Default for CursorExecutor {
    fn default() -> Self {
        Self {
            executable: None,
            adapters: None,
            npm: None,
            auth: Auth::Real,
            interrupt_grace: Duration::from_secs(5),
            kill_grace: Duration::from_secs(3),
            silence_timeout: SILENCE_TIMEOUT,
            probe_budget: probe::WIRE_BUDGET,
            listed: Arc::new(Mutex::new(None)),
            tracker: TaskTracker::new(),
            unknown: UnknownFrames::new(),
        }
    }
}

impl CursorExecutor {
    /// An executor that finds Node and the managed SDK install on its own.
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `path` AS the shim instead of Node and the managed SDK (tests). Credentials are
    /// taken as present unless [`Self::with_auth`] says otherwise, and the conversation stores
    /// must be named with [`Self::with_adapters_root`]: a fake shim never reaches the owner's
    /// own directories.
    #[doc(hidden)]
    #[must_use]
    pub fn with_executable(mut self, path: impl Into<PathBuf>) -> Self {
        self.executable = Some(path.into());
        self.auth = Auth::Fixed {
            api_key: true,
            file: None,
        };
        self
    }

    /// Keep the managed install (and, beside it, the conversation stores) under `root`.
    #[doc(hidden)]
    #[must_use]
    pub fn with_adapters_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.adapters = Some(root.into());
        self
    }

    /// Run the explicit install with this `npm` (tests).
    #[doc(hidden)]
    #[must_use]
    pub fn with_npm(mut self, npm: impl Into<PathBuf>) -> Self {
        self.npm = Some(npm.into());
        self
    }

    /// Decide credentials without reading the environment or the owner's home: whether a
    /// `CURSOR_API_KEY` is set, and the path of the SDK's stored login.
    #[doc(hidden)]
    #[must_use]
    pub fn with_auth(mut self, api_key: bool, stored_login: Option<PathBuf>) -> Self {
        self.auth = Auth::Fixed {
            api_key,
            file: stored_login,
        };
        self
    }

    /// Tune the interrupt grace and the SIGTERM to SIGKILL grace.
    #[must_use]
    pub fn with_graces(mut self, interrupt_grace: Duration, kill_grace: Duration) -> Self {
        self.interrupt_grace = interrupt_grace;
        self.kill_grace = kill_grace;
        self
    }

    /// Tune how long a send may go silent before the shim abandons the turn (default 10
    /// minutes).
    #[doc(hidden)]
    #[must_use]
    pub fn with_silence_timeout(mut self, timeout: Duration) -> Self {
        self.silence_timeout = timeout;
        self
    }

    /// Bound the `models` listing `probe` starts (tests shrink it; default 20 s).
    #[must_use]
    pub fn with_probe_budget(mut self, budget: Duration) -> Self {
        self.probe_budget = budget;
        self
    }

    /// Whether the shim and Node can be launched now (the SDK is installed).
    pub fn installed(&self) -> bool {
        matches!(self.place().locate(), Located::Ready { .. })
    }

    fn root(&self) -> Option<PathBuf> {
        self.adapters
            .clone()
            .or_else(crate::adapter_install::adapters_root)
    }

    /// Where the shim is looked for: the test seam, or the managed adapter root.
    fn place(&self) -> Place {
        Place {
            executable: self.executable.clone(),
            root: self.root(),
        }
    }

    fn state_root(&self) -> Result<PathBuf, ExecError> {
        let root = match (&self.adapters, &self.executable) {
            (Some(root), _) => root.clone(),
            // A fake shim with no root named would write to the owner's real directory.
            (None, Some(_)) => {
                return Err(ExecError::InvalidSpec(
                    "a fake Cursor shim needs an explicit adapters root (with_adapters_root)"
                        .into(),
                ));
            }
            (None, None) => self.root().ok_or_else(|| {
                ExecError::NotInstalled(
                    "cannot locate a platform data directory for managed adapters".into(),
                )
            })?,
        };
        Ok(state::state_root(&root))
    }

    /// Whether the SDK would find a credential, from what may be read of it: the environment's
    /// key and the stored login. Nothing is written, refreshed or started.
    async fn credential(
        &self,
        run_env: Option<&std::collections::BTreeMap<String, String>>,
    ) -> Credential {
        let (mut api_key, file) = match &self.auth {
            Auth::Real => (
                std::env::var_os(API_KEY_VAR).is_some_and(|key| !key.is_empty()),
                crate::executable::home_dir()
                    .map(|home| home.join(".cursor").join("sdk").join("auth.json")),
            ),
            Auth::Fixed { api_key, file } => (*api_key, file.clone()),
        };
        // A bot's own secret counts for the run that carries it.
        api_key |= run_env
            .and_then(|env| env.get(API_KEY_VAR))
            .is_some_and(|key| !key.is_empty());
        let read = tokio::task::spawn_blocking(move || {
            let file = file?;
            let metadata = std::fs::metadata(&file).ok()?;
            // A stored login is a few hundred bytes.
            if metadata.len() > 1024 * 1024 {
                return None;
            }
            std::fs::read(&file).ok()
        })
        .await
        .ok()
        .flatten();
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX));
        auth::credential(api_key, read.as_deref(), now_ms)
    }

    fn last_models(&self) -> Option<Vec<ModelInfo>> {
        self.listed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|listed| listed.models.clone())
    }

    /// The command that runs the shim with `mode`/`args`, with Reno's restricted environment.
    fn command(&self, program: &Path, args: &[String]) -> Command {
        let mut cmd = Command::new(program);
        crate::child_env::restrict(&mut cmd, crate::child_env::CURSOR);
        cmd.args(args);
        crate::compose_child_path(&mut cmd, program);
        cmd
    }

    /// The models the SDK lists for the signed-in account: one short-lived shim in `models`
    /// mode, bounded in time and output and reaped before this returns. It makes the network
    /// call of the SDK's `models.list()` (the SDK limits its catalog calls, about 30 a
    /// minute), never a model call.
    async fn query_models(&self, program: &Path, args: &[String]) -> Result<Listed, ModelsError> {
        let mut cmd = self.command(program, args);
        cmd.arg("models")
            .env("NO_OPEN_BROWSER", "1")
            .current_dir(std::env::temp_dir())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = crate::process::owned::spawn_grouped(&mut cmd).map_err(|e| {
            ModelsError::Failed(
                match crate::executable::spawn_error(e, None, || {
                    crate::executable::binary_hint(program)
                }) {
                    HarnessError::NotInstalled(why) => why,
                    other => other.to_string(),
                },
            )
        })?;
        let Some(stdout) = child.stdout.take() else {
            child.shutdown(self.kill_grace).await;
            return Err(ModelsError::Failed("the Cursor shim has no stdout".into()));
        };
        let read = async {
            let mut bytes = Vec::new();
            stdout
                .take(MAX_CATALOG_BYTES)
                .read_to_end(&mut bytes)
                .await
                .map(|_| bytes)
        };
        let outcome = tokio::time::timeout(self.probe_budget, read).await;
        child.shutdown(self.kill_grace).await;
        let bytes = match outcome {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(error)) => return Err(ModelsError::Failed(error.to_string())),
            Err(_) => {
                return Err(ModelsError::Failed(
                    "the Cursor model listing timed out".into(),
                ));
            }
        };
        let mut items = None;
        for line in String::from_utf8_lossy(&bytes).lines() {
            let Some(frame) = crate::wire::pi::framing::parse_frame(line.trim().as_bytes()) else {
                continue;
            };
            match frame.get("ev").and_then(Value::as_str) {
                Some("fatal") => {
                    return Err(ModelsError::Refused(
                        frame
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("Cursor model discovery failed")
                            .to_owned(),
                    ));
                }
                Some("models") => items = frame.get("items").cloned(),
                _ => {}
            }
        }
        let items = items
            .ok_or_else(|| ModelsError::Failed("the Cursor shim returned no catalog".into()))?;
        let listed = catalog::listed(&items);
        *self.listed.lock().unwrap_or_else(PoisonError::into_inner) = Some(listed.clone());
        Ok(listed)
    }
}

/// Why the model listing gave nothing.
#[derive(Debug)]
enum ModelsError {
    /// The shim said it cannot go on (`fatal`), usually authentication.
    Refused(String),
    /// It could not be asked, or did not answer.
    Failed(String),
}

#[async_trait]
impl Executor for CursorExecutor {
    /// A Reno-managed install: it is fetched by [`Executor::install`] and moves with Reno.
    fn maintenance(&self) -> Option<crate::exec::maintenance::MaintenanceDecl> {
        crate::exec::maintenance::declaration(&HarnessId::Cursor)
    }

    fn descriptor(&self) -> ExecutorDescriptor {
        let models = self.last_models().unwrap_or_else(catalog::curated);
        ExecutorDescriptor {
            id: ID.clone(),
            contract_version: CONTRACT_VERSION,
            adapter_version: format!(
                "reno-harness {} cursor driver (SDK {SDK_VERSION})",
                env!("CARGO_PKG_VERSION")
            ),
            cli_verified: VersionRange {
                min: SDK_VERSION.into(),
                verified: SDK_VERSION.into(),
            },
            caps: ExecutorCaps {
                // The model and its options are set when the agent starts.
                live_effort: None,
                live_model_options: None,
                // A steering message lands at the next model step of the live turn.
                steering: Steering::StepBoundary,
                resume: true,
                // The SDK runs its tools client-side with approvals ignored: full access,
                // unrestricted, or refused.
                access_modes: vec![AccessMode::FullAccess],
                sandbox_levels: vec![SandboxLevel::Unrestricted],
                mcp_injection: true,
                structured_output: false,
                // Done is the SDK run's own terminal result.
                deterministic_turn_end: true,
                plan_mode: false,
                models,
                model_switch: false,
                // The SDK is told to load no ambient settings unless the run opts in.
                isolates_user_config: true,
                model_aliases: vec![],
            },
        }
    }

    /// Where the SDK is (or that it needs the explicit install), against the pin; and whether
    /// it can sign in, from the environment's `CURSOR_API_KEY` or the SDK's stored login, read
    /// only. When signed in, one short-lived shim lists the account's models (no prompt, no
    /// model call); otherwise the last live list, else the curated one.
    async fn probe(&self) -> HealthProbe {
        let fallback = || match self.last_models() {
            Some(models) => ModelCatalog::live(models),
            None => ModelCatalog::curated(catalog::curated()),
        };
        let place = self.place();
        let located = tokio::task::spawn_blocking(move || place.locate()).await;
        let (program, args, shim) = match located {
            Ok(Located::Ready {
                program,
                args,
                shim,
            }) => (program, args, shim),
            Ok(Located::NeedsInstall(what)) => {
                return HealthProbe::not_installed(
                    NotInstalledReason::NeedsInstall { what },
                    fallback(),
                );
            }
            Ok(Located::NoNode(hint)) => {
                return HealthProbe::not_installed(
                    NotInstalledReason::NotFound { hint },
                    fallback(),
                );
            }
            Ok(Located::NoData(detail)) => {
                return probe::failed(detail, fallback());
            }
            Err(error) => return probe::failed(error, fallback()),
        };
        let version_gate = reno_proto::version_gate(
            Some(SDK_VERSION),
            &VersionRange {
                min: SDK_VERSION.into(),
                verified: SDK_VERSION.into(),
            },
        );
        let credential = self.credential(None).await;
        let (signed_in, models) = if credential.usable() {
            match self.query_models(&program, &args).await {
                Ok(listed) => (
                    SignedIn::Yes {
                        account: match &credential {
                            Credential::Login { email } => email.clone(),
                            _ => None,
                        },
                    },
                    ModelCatalog::live(listed.models),
                ),
                // The credential is there but Cursor refuses it.
                Err(ModelsError::Refused(why)) if auth::is_auth_failure(&why) => (
                    SignedIn::No {
                        hint: format!(
                            "Cursor rejected the credential ({}). {}",
                            crate::redact_secrets(&why),
                            auth::sign_in_hint(&Credential::None)
                        ),
                    },
                    fallback(),
                ),
                Err(ModelsError::Refused(why) | ModelsError::Failed(why)) => {
                    tracing::debug!(%why, "the cursor models probe failed");
                    (SignedIn::Unknown, fallback())
                }
            }
        } else {
            (
                SignedIn::No {
                    hint: auth::sign_in_hint(&credential),
                },
                fallback(),
            )
        };
        HealthProbe {
            installed: InstallStatus::Installed {
                path: shim,
                version: Some(SDK_VERSION.into()),
            },
            version_gate,
            signed_in,
            models,
        }
    }

    /// The one explicit install step: npm installs the pinned SDK (scripts off) into Reno's
    /// managed adapter directory and the shim is written beside it. Nothing else installs.
    async fn install(&self) -> Result<(), HarnessError> {
        if self.executable.is_some() {
            return Ok(());
        }
        let root = self.root().ok_or_else(|| {
            HarnessError::Install(
                "cannot locate a platform data directory for managed adapters".into(),
            )
        })?;
        let npm = self.npm.clone().or_else(crate::adapter_install::find_npm);
        let installing = self.tracker.spawn(async move {
            let pin = crate::adapter_install::NpmPin::parse(SDK_PIN);
            crate::adapter_install::ensure_installed_shim_in(
                &root,
                npm,
                pin,
                "Cursor",
                SHIM_NAME,
                SHIM_SOURCE,
            )
            .await
            .map(|_| ())
        });
        installing
            .await
            .map_err(|error| HarnessError::Install(format!("the install task failed: {error}")))?
    }

    async fn start(
        &self,
        spec: ExecSpec,
        controls: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError> {
        // Fail closed before anything is resolved, installed or spawned (ARCH-15).
        require_access(spec.access, &[AccessMode::FullAccess])?;
        require_sandbox(spec.sandbox, &[SandboxLevel::Unrestricted])?;
        if spec.output_schema.is_some() {
            return Err(ExecError::UnsupportedOutputSchema);
        }
        if let Some(session) = &spec.resume
            && !valid_agent_id(&session.0)
        {
            return Err(ExecError::InvalidSpec(format!(
                "{:?} is not a Cursor agent id",
                session.0
            )));
        }
        let Some(cwd) = spec.cwd.to_str().map(str::to_owned) else {
            return Err(ExecError::InvalidSpec("cwd is not valid UTF-8".into()));
        };
        let state_root = self.state_root()?;
        let credential = self.credential(Some(&spec.env)).await;
        if !credential.usable() {
            return Err(ExecError::NotSignedIn(auth::sign_in_hint(&credential)));
        }
        let place = self.place();
        let located = tokio::task::spawn_blocking(move || place.locate())
            .await
            .map_err(|e| ExecError::NotInstalled(e.to_string()))?;
        let (program, args) = match located {
            Located::Ready { program, args, .. } => (program, args),
            Located::NeedsInstall(what) => {
                return Err(ExecError::NotInstalled(format!(
                    "{what} needs to be installed first"
                )));
            }
            Located::NoNode(why) | Located::NoData(why) => {
                return Err(ExecError::NotInstalled(why));
            }
        };
        let ExecControls {
            steering,
            cancel,
            processes,
            // No approvals or questions are raised, and neither a model switch nor a live
            // config change is claimed: not read.
            ..
        } = controls;

        let launched = self
            .launch(&program, &args, &spec, &cwd, &state_root, &processes)
            .await;
        let shim = match launched {
            Ok(shim) => shim,
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
        let model = spec.model.clone();
        let resume = spec.resume.as_ref().map(|s| s.0.clone());
        let run = Run {
            shim,
            event_tx,
            steering,
            cancel,
            norm: Normalizer::new(resume.as_deref(), model.as_deref()),
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

impl CursorExecutor {
    /// Take the conversation store, start the shim and send it the run frame.
    async fn launch(
        &self,
        program: &Path,
        args: &[String],
        spec: &ExecSpec,
        cwd: &str,
        state_root: &Path,
        processes: &ProcessSink,
    ) -> Result<Shim, HarnessError> {
        let lease =
            state::Lease::acquire(state_root, spec.resume.as_ref().map(|s| s.0.as_str())).await?;
        let store_dir = lease.store_dir.to_str().map(str::to_owned).ok_or_else(|| {
            HarnessError::Protocol("the Cursor state directory is not valid UTF-8".into())
        })?;

        let mut cmd = self.command(program, args);
        // The allowlisted environment of the run (the bot's secrets, Reno's `RENO_*` variables).
        crate::apply_run_env(&mut cmd, &spec.env);
        // After the run's own: a secret cannot point the shim's state or open a browser.
        cmd.env("RENO_CURSOR_STATE_DIR", state_root)
            .env("NO_OPEN_BROWSER", "1");
        if !spec.cwd.as_os_str().is_empty() {
            cmd.current_dir(&spec.cwd);
        }
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = crate::process::owned::spawn_grouped(&mut cmd).map_err(|e| {
            let cwd = (!spec.cwd.as_os_str().is_empty()).then_some(spec.cwd.as_path());
            crate::executable::spawn_error(e, cwd, || crate::executable::binary_hint(program))
        })?;
        // Reported at once: a daemon that dies from here on still leaves its row.
        #[cfg(unix)]
        if let Some(record) = child.record() {
            let _ = processes.report(record).await;
        }
        #[cfg(not(unix))]
        let _ = processes;
        let missing = |what: &str| HarnessError::Protocol(format!("cursor shim has no {what}"));
        let stdin = child.stdin.take().ok_or_else(|| missing("stdin"))?;
        let stdout = child.stdout.take().ok_or_else(|| missing("stdout"))?;
        let stderr_tail = crate::StderrTail::for_run(&spec.env);
        let stderr_task = child.stderr.take().map(|stderr| {
            let tail = stderr_tail.clone();
            self.tracker.spawn(async move {
                let mut lines = crate::lossy_lines::lossy_lines(stderr);
                while let Ok(Some(line)) = lines.next_line().await {
                    tracing::debug!(target: "reno_harness::cursor", "stderr: {}", tail.scrub(&line));
                    tail.push(&line);
                }
                tail.close();
            })
        });
        let transport = pipe::Transport::new(stdin, stdout);
        let prompt = frames::first_prompt(spec.instructions.as_deref(), &spec.prompt);
        let mut model_options = spec.model_options.clone();
        let model = spec
            .model
            .as_deref()
            .filter(|m| !m.is_empty() && *m != "default");
        if let Some(listed) = self
            .listed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
        {
            catalog::fill_defaults(listed, model, &mut model_options);
        }
        let run = frames::Run {
            prompt: &prompt,
            cwd,
            model,
            model_options: &model_options,
            resume: spec.resume.as_ref().map(|s| s.0.as_str()),
            mcp: &spec.mcp,
            store_dir: &store_dir,
            inherit_user_config: spec.inherit_user_config,
            silence_timeout_ms: u64::try_from(self.silence_timeout.as_millis()).unwrap_or(u64::MAX),
        };
        transport.client.send(run.frame())?;
        Ok(Shim {
            child,
            transport,
            stderr_tail,
            stderr_task,
            _lease: lease,
            exit_deadline: None,
        })
    }
}

// ---------------------------------------------------------------------------
// The shim process
// ---------------------------------------------------------------------------

/// One shim process, its pipe, and the conversation store it owns.
struct Shim {
    child: OwnedChild,
    transport: pipe::Transport,
    stderr_tail: crate::StderrTail,
    stderr_task: Option<tokio::task::JoinHandle<()>>,
    /// Held until the shim is reaped, so no other run opens this conversation meanwhile. Last
    /// field: dropped after the rest.
    _lease: state::Lease,
    /// When the process was seen to have exited; frames it left behind are still read until then.
    exit_deadline: Option<Instant>,
}

impl Drop for Shim {
    fn drop(&mut self) {
        if let Some(task) = self.stderr_task.take() {
            task.abort();
        }
    }
}

impl Shim {
    /// The next frame, in stdout order. An error when the shim's stdout closed or it exited and
    /// everything it had written is read: a descendant (an MCP server, a tool) may keep stdout
    /// open, so EOF alone cannot govern liveness.
    async fn next_frame(&mut self) -> Result<Value, HarnessError> {
        loop {
            tokio::select! {
                biased;
                frame = self.transport.incoming.recv() => {
                    return frame.unwrap_or_else(|| {
                        Err(HarnessError::Protocol("the Cursor shim disconnected".into()))
                    });
                }
                () = async {
                    match self.exit_deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => return Err(HarnessError::Protocol("the Cursor shim exited".into())),
                status = self.child.wait(), if self.exit_deadline.is_none() => {
                    status?;
                    self.exit_deadline = Some(Instant::now() + Duration::from_millis(150));
                }
            }
        }
    }

    /// What to say when the shim went away under a run: its exit status and last stderr lines.
    async fn crash_report(&mut self) -> String {
        self.stderr_tail.wait_closed().await;
        let status = match tokio::time::timeout(Duration::from_millis(500), self.child.wait()).await
        {
            Ok(Ok(status)) => Some(status),
            _ => self.child.try_wait().ok().flatten(),
        };
        crate::crash_message("cursor shim", status, &self.stderr_tail)
    }
}

// ---------------------------------------------------------------------------
// One run
// ---------------------------------------------------------------------------

type Ended = (StopReason, Option<String>, Option<String>);

struct Run {
    shim: Shim,
    event_tx: mpsc::Sender<ExecEvent>,
    steering: mpsc::Receiver<String>,
    cancel: CancellationToken,
    norm: Normalizer,
    interrupt_grace: Duration,
    kill_grace: Duration,
    unknown: UnknownFrames,
}

impl Run {
    /// The whole run: play, end exactly once, tear down.
    async fn drive(mut self) {
        let (stop, result, diagnostic) = self.play().await;
        let done = ExecEvent::Done {
            stop,
            result,
            diagnostic: diagnostic.map(|d| crate::redact_secrets(&d)),
            reason: None,
        };
        let _ = self.event_tx.send(done).await;
        self.teardown().await;
    }

    async fn emit(&self, event: ExecEvent) -> bool {
        self.event_tx.send(event).await.is_ok()
    }

    /// Play the run from the shim's first frame to its end.
    async fn play(&mut self) -> Ended {
        // Steering handed to the shim and not yet reported consumed.
        let mut pending = 0usize;
        let mut steering_open = true;
        let mut interrupted = false;
        let mut interrupt_deadline: Option<Instant> = None;
        let mut startup_deadline = Some(Instant::now() + STARTUP_BUDGET);
        let writer = self.shim.transport.client.clone();
        loop {
            let interrupt_at = async {
                match interrupt_deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            let startup_at = async {
                match startup_deadline {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                () = self.cancel.cancelled(), if !interrupted => {
                    interrupted = true;
                    // The shim cancels the live run and settles `cancelled`; this only waits a
                    // grace for that, and teardown ends the process either way.
                    // Waiting for room: a shim with a full queue of steering must still hear it.
                    let _ = writer.send_within(frames::interrupt(), self.interrupt_grace).await;
                    interrupt_deadline = Some(Instant::now() + self.interrupt_grace);
                }
                () = interrupt_at => return (StopReason::Interrupted, None, None),
                () = startup_at => {
                    return (
                        StopReason::Error,
                        None,
                        Some(format!(
                            "Cursor did not start within {}s",
                            STARTUP_BUDGET.as_secs()
                        )),
                    );
                }
                frame = self.shim.next_frame() => {
                    let frame = match frame {
                        Ok(frame) => frame,
                        Err(_) => return self.gone(interrupted).await,
                    };
                    let mapped = self.norm.map(&frame);
                    for kind in self.norm.take_unknown() {
                        self.unknown.note(&kind);
                    }
                    for event in mapped.events {
                        if matches!(event, ExecEvent::Started { .. }) {
                            startup_deadline = None;
                        }
                        if !self.emit(event).await {
                            return (StopReason::Interrupted, None, None);
                        }
                    }
                    match mapped.signal {
                        Signal::None => {}
                        Signal::Steered => pending = pending.saturating_sub(1),
                        Signal::Fatal(why) => {
                            return if interrupted {
                                (StopReason::Interrupted, None, None)
                            } else {
                                (StopReason::Error, None, Some(why))
                            };
                        }
                        Signal::Turn(Turn::Cancelled) => {
                            return (StopReason::Interrupted, None, None);
                        }
                        Signal::Turn(Turn::Error(why)) => {
                            return if interrupted {
                                (StopReason::Interrupted, None, None)
                            } else {
                                (StopReason::Error, None, Some(why))
                            };
                        }
                        Signal::Turn(Turn::Finished) => {
                            if interrupted {
                                return (StopReason::Interrupted, None, None);
                            }
                            // The shim goes on to a follow-up turn for steering it has not
                            // consumed.
                            if pending > 0 {
                                continue;
                            }
                            // Steering that arrived as the turn ended runs as a turn of its own.
                            let mut handed = false;
                            while steering_open && let Ok(text) = self.steering.try_recv() {
                                handed |= self.hand_over(&text, &mut pending).await;
                            }
                            if !handed {
                                return (StopReason::Completed, self.norm.result(), None);
                            }
                        }
                    }
                }
                next = async {
                    // Room first: a message leaves the queue only when it can be handed on,
                    // so a burst is held back, never dropped.
                    let permit = writer.reserve().await.ok()?;
                    let text = self.steering.recv().await?;
                    Some((permit, text))
                }, if steering_open && !interrupted => match next {
                    Some((permit, text)) => {
                        permit.send(frames::steer(&text));
                        pending += 1;
                    }
                    None => steering_open = false,
                },
                () = self.event_tx.closed() => return (StopReason::Interrupted, None, None),
            }
        }
    }

    /// Hand a steering message to the shim, waiting a grace for room. Whether it was sent.
    async fn hand_over(&self, text: &str, pending: &mut usize) -> bool {
        let client = &self.shim.transport.client;
        match client
            .send_within(frames::steer(text), self.interrupt_grace)
            .await
        {
            Ok(()) => {
                *pending += 1;
                true
            }
            Err(error) => {
                tracing::warn!(target: "reno_harness::cursor", "steering not sent: {error}");
                false
            }
        }
    }

    /// The shim went away, or its pipe broke, before the run ended.
    async fn gone(&mut self, interrupted: bool) -> Ended {
        if interrupted {
            return (StopReason::Interrupted, None, None);
        }
        let crash = self.shim.crash_report().await;
        (StopReason::Error, None, Some(crash))
    }

    /// End the shim and everything it started: trace its descendants while it still parents
    /// them, close its stdin (its own orderly shutdown: it cancels the run, closes the SDK and
    /// removes its ownership marker), give it a moment, then SIGTERM and SIGKILL to the process
    /// group and the descendants traced.
    async fn teardown(&mut self) {
        #[cfg(unix)]
        let tree = match self.shim.child.id() {
            Some(pid) => crate::process::descendants(pid).await,
            None => Vec::new(),
        };
        #[cfg(not(unix))]
        let tree = Vec::new();
        self.shim.transport.close_input();
        let _ = tokio::time::timeout(self.interrupt_grace, self.shim.child.wait()).await;
        crate::shutdown_agent(&mut self.shim.child, tree, self.kill_grace).await;
        if let Some(task) = self.shim.stderr_task.take() {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pin_is_the_version_the_descriptor_says_it_was_verified_on() {
        assert_eq!(SDK_PIN, format!("@cursor/sdk@{SDK_VERSION}"));
        let range = CursorExecutor::new().descriptor().cli_verified;
        assert_eq!(
            (range.min.as_str(), range.verified.as_str()),
            (SDK_VERSION, SDK_VERSION)
        );
        assert_eq!(SDK_VERSION, "1.0.32");
    }

    #[test]
    fn the_descriptor_claims_only_what_the_driver_does() {
        let caps = CursorExecutor::new().descriptor().caps;
        assert_eq!(caps.access_modes, [AccessMode::FullAccess]);
        assert_eq!(caps.sandbox_levels, [SandboxLevel::Unrestricted]);
        assert_eq!(caps.steering, Steering::StepBoundary);
        assert!(caps.resume && caps.mcp_injection && caps.deterministic_turn_end);
        assert!(caps.isolates_user_config);
        assert!(!caps.structured_output && !caps.plan_mode && !caps.model_switch);
        assert!(caps.live_effort.is_none() && caps.live_model_options.is_none());
    }

    #[test]
    fn the_shim_source_never_logs_in_and_names_the_variable() {
        // Reno never starts the SDK's browser login: the shim has no mode for it.
        assert!(!SHIM_SOURCE.contains("Cursor.auth.login"));
        assert!(!SHIM_SOURCE.contains("FileCredentialStore"));
        assert!(SHIM_SOURCE.contains("CURSOR_API_KEY"));
        assert!(SHIM_SOURCE.is_ascii());
    }
}
