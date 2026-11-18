//! The native opencode executor: starts the installed `opencode serve`, speaks its HTTP and SSE
//! server protocol, and implements [`Executor`]. Not ACP.
//!
//! # Why not ACP
//!
//! opencode's own ACP layer ends a prompt on the FIRST `session.status{idle}` it sees after
//! subscribing, uncorrelated with the prompt it sent, and ignores `session.error` and
//! `session.idle` altogether (zeron, reading `packages/opencode/src/acp/event.ts` of 1.18.x): the
//! source of "done while still working" and of turns that hang. This driver ends a run on the idle
//! of ITS session, after that session was busy, confirmed (see `wire::opencode::reducer`).
//!
//! # What "verified" covers
//!
//! [`MIN_CLI_VERSION`] is 1.19.5 and [`VERIFIED_CLI_VERSION`] 2.3.2: the oldest and the newest
//! opencode this driver was run against, and what that means is narrow, and different for the two
//! generations. NOTHING was run with a prompt, so no model was asked anything and no real turn
//! has gone through this driver: the event frames of a turn (`message.part.updated`, the tool
//! parts, `session.status`, `permission.asked`, `question.asked`, usage) are the shapes zeron met
//! live on 1.18.x, 2.0.x and 2.1.2 and its scripted-server tests encode. The live test
//! (`tests/opencode.rs`, `--ignored`, `RENO_OPENCODE_LIVE=1`) runs a turn on opencode's free
//! `opencode/big-pickle` model.
//!
//! - **1.19.5** (the Homebrew install on the owner's Mac, from the `anomalyco/tap` formula): a real
//!   server was started in a throwaway HOME and XDG directories behind a password and sent no
//!   prompt. Read back: `/global/health` (`{"healthy":true,"version":"1.19.5"}`), the OpenAPI
//!   document at `/doc` (the route table the refusal guard was checked against), `/provider` (233
//!   providers, `connected: ["opencode"]` on a clean install: the anonymous tier), `/agent` (the
//!   permission rules of `build` and `plan` with the inline config layered on: the read-back
//!   [`config::rules_in_force`] makes), `/experimental/tool/ids` (the `question` tool appears only
//!   with `OPENCODE_ENABLE_QUESTION_TOOL=true`), `POST /session` and `GET /session/{id}` across a
//!   server restart (resume), the first bus frame (`{payload: {type: "server.connected"}}`).
//!   `/api/health` answers `{"healthy":true}` with no version and `/api/info` the web UI: the
//!   generation is read from the first answer that carries a version.
//! - **2.3.2** (the registry's `opencode-v2` darwin-aarch64 archive, sha256 checked against the
//!   registry, unpacked in a temp directory, never installed): the same, over the `/api/*` wire:
//!   `/api/info` (`{"version":"2.3.2",..}`), `/api/model` (the free provider's models),
//!   `/api/agent/{id}` (the inline `permissions` list merged into `build`; `/api/agent` listed it
//!   too but not at once: the config is read a moment after the server answers, so the read-back
//!   polls), `/api/config`, `/api/mcp` (an inline `mcp.servers` entry registered),
//!   `POST /api/session`, `GET /api/session/{id}` across a restart, the permission reply key
//!   (`decision`, 400 `Missing key ["decision"]` to `reply`), the form reply key (`answer`), and
//!   the first bus frame and its heartbeat (`data: {"type":"server.connected"}`, `: heartbeat`).
//!   2.3.2 has no OpenAPI document (`/doc` is the web UI) and no `question` routes: questions are
//!   forms.
//!
//! Not verified on either: any frame of a turn, the permission request payloads (`action` and
//! `resources` on 2.x), a question or a form being asked, the usage counts' disjointness
//! (`tokens.input` excluding the cache, `tokens.output` excluding `tokens.reasoning`: read from
//! memory of opencode's source, UNVERIFIED), the effect of an `ask` rule on a live tool call, a
//! subagent session, abort, and the Supervised and Auto-accept modes against 2.x beyond the
//! read-back of the rules.
//!
//! # How a run goes
//!
//! - ONE RUN IS ONE `Done`. A server serves one run: `opencode serve --port <free> --hostname
//!   127.0.0.1` behind `OPENCODE_SERVER_PASSWORD` (a fresh random value, HTTP Basic user
//!   `opencode`, never logged), a session created or resumed (`ExecSpec::resume`, a session id the
//!   server keeps in its database), the bus subscribed (the first prompt waits for it to be live),
//!   the prompt posted, and the server torn down with every process it started when the run ends.
//! - A turn ends on the idle of the run's session ([`reducer::Reducer`]). Steering messages queue
//!   and go out as the next prompt of the session when it settles (`Steering::TurnBoundary`); the
//!   run ends when it settles with none waiting.
//! - Retries are surfaced, not hidden: from the third provider retry a note appears in the
//!   transcript, and at the eighth the turn is aborted, since opencode would retry for ever. A
//!   prompt that produces no event within a minute of the server accepting it, a server that dies
//!   (noticed within half a second), a bus silent past its bound, and a server that answers
//!   nothing, each end the run with a diagnostic.
//! - Permission requests of the run and its subagents become `NeedsApproval` (any mode: the
//!   kernel's policy answers a full-access run's), the `question` tool `NeedsInput`. A permission
//!   is answered `once` or `reject`, never `always`, which would save a rule in the owner's store.
//!   Supervised and Auto-accept edits add `ask` rules ([`config`]); the driver reads the agent
//!   back and refuses to prompt unless they are in force, so an owner's or project's config that
//!   outranks ours, or a later opencode that reads another shape, ends the run before it acts.
//!
//! # Never written
//!
//! Reno never writes opencode's configuration or credentials. Inline config goes through
//! `OPENCODE_CONFIG_CONTENT`, the last user config layer, in the server's environment: not under
//! `~/.config/opencode`, not in the project's `opencode.json`. Every request goes through one
//! allowlist (`wire::opencode::requests::permitted`); `PATCH /config`, `/auth/*`,
//! `/api/credential/*`, `POST /mcp` and `/global/upgrade` are among the routes it refuses, and a
//! test lists them. opencode ITSELF writes while it boots, which Reno cannot prevent and
//! observed on 1.19.5: it adds `"$schema"` to a global or project config file that lacks one
//! (read: the files of the throwaway runs, rewritten), creates `opencode.jsonc` and `.gitignore`
//! in its config directory and installs plugin packages into it (`node_modules`, 100 MB, through
//! npm). 2.3.2 did none of that. That is why `isolates_user_config` is false: pointing
//! `XDG_CONFIG_HOME` at a scratch directory would stop it, and would reinstall those packages
//! from the network on every run.
//!
//! `OPENCODE_DISABLE_AUTOUPDATE=1` is set for every server (environment, not config): opencode
//! upgrades itself in place, through Homebrew for a brew install, and that must not happen under a
//! running run.
//!
//! # Process records
//!
//! The server is reported through `ExecControls::processes` at once. The probe's server is
//! reaped, with its descendants, before the probe returns.

use std::collections::VecDeque;
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
    Decision, ExecAnswer, ExecControls, ExecError, Executor, OpenRequests, ProcessSink, Resolved,
    UnknownFrames, probe, require_access, require_sandbox,
};
use crate::wire::opencode::catalog::{self, DEFAULT_AGENT_CHOICE, ProviderCatalog};
use crate::wire::opencode::config::{self, RunConfig};
use crate::wire::opencode::permission::{Asked, Permission};
use crate::wire::opencode::reducer::{Effect, Ending, Reducer, STALL_HINT};
use crate::wire::opencode::requests::{self, Reply, Request};
use crate::wire::opencode::v2;
use crate::wire::opencode::version::Protocol;
use crate::{HarnessError, HarnessId};

mod bus;
mod server;

use bus::{BusMsg, BusTimeouts};
use server::{Api, Server, SpawnSpec};

/// The oldest opencode this driver was run against (see the module docs).
pub const MIN_CLI_VERSION: &str = "1.19.5";

/// The newest opencode this driver was run against: the registry's 2.3.2 (handshake only).
pub const VERIFIED_CLI_VERSION: &str = "2.3.2";

/// The executor contract version this driver implements.
const CONTRACT_VERSION: u32 = 1;

/// The id this executor registers under.
pub const EXECUTOR_ID: &str = "opencode";

static ID: LazyLock<ExecutorId> = LazyLock::new(|| match EXECUTOR_ID.parse() {
    Ok(id) => id,
    Err(error) => unreachable!("{EXECUTOR_ID} is a valid executor id: {error}"),
});

/// What the stream carries before the consumer reads.
const EVENT_CAPACITY: usize = 256;

/// opencode loads plugins and MCP config before the server answers; cold plugin-heavy starts can
/// take minutes.
const DEFAULT_STARTUP: Duration = Duration::from_secs(300);
const STARTUP_ENV: &str = "RENO_OPENCODE_STARTUP_SECS";

/// How long the rules and MCP servers of the inline config may take to show in the server.
const READ_BACK_BUDGET: Duration = Duration::from_secs(20);

/// Default bound on prompt accepted to first event of the session.
const DEFAULT_STALL: Duration = Duration::from_secs(60);
const STALL_ENV: &str = "RENO_OPENCODE_STALL_MS";

/// How often the run checks that its own server process is still alive.
const SERVER_POLL: Duration = Duration::from_millis(500);

/// Prompt deliveries: one retry, posted only once the session is checked for the message.
const PROMPT_ATTEMPTS: u32 = 2;
const PROMPT_RETRY_DELAY: Duration = Duration::from_millis(500);

/// Permission and question replies: the agent blocks until one lands.
const REPLY_ATTEMPTS: u32 = 3;
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Budget for aborting a stopped run's subagent sessions.
const CHILD_ABORT_BUDGET: Duration = Duration::from_secs(15);

/// Requests open on a person at once (ARCH-11). More are refused.
const MAX_OPEN_REQUESTS: usize = 32;

fn startup_timeout() -> Duration {
    std::env::var(STARTUP_ENV)
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map_or(DEFAULT_STARTUP, Duration::from_secs)
}

fn stall_bound() -> Option<Duration> {
    match std::env::var(STALL_ENV).map(|v| v.parse::<u64>()) {
        Ok(Ok(0)) => None,
        Ok(Ok(ms)) => Some(Duration::from_millis(ms)),
        _ => Some(DEFAULT_STALL),
    }
}

/// The opencode CLI to launch: `OPENCODE_EXECUTABLE`, then our own PATH, the login-shell PATH and
/// the known install locations.
pub fn resolve_opencode_executable() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("OPENCODE_EXECUTABLE").filter(|p| !p.is_empty()) {
        return crate::executable::validate_native_override(&PathBuf::from(p)).ok();
    }
    crate::executable::find_on_paths("opencode", server::install_locations())
}

fn resolve_executable(executable: Option<&Path>) -> Result<PathBuf, ExecError> {
    let not_installed = |error: HarnessError| ExecError::NotInstalled(error.to_string());
    if let Some(p) = executable {
        return crate::executable::validate_native_override(p).map_err(not_installed);
    }
    if let Some(p) = std::env::var_os("OPENCODE_EXECUTABLE")
        && !p.is_empty()
    {
        return crate::executable::validate_native_override(&PathBuf::from(p))
            .map_err(not_installed);
    }
    resolve_opencode_executable().ok_or_else(|| {
        ExecError::NotInstalled(
            "opencode (searched PATH, the login shell's PATH, ~/.opencode/bin, ~/.local/bin, \
             ~/.bun/bin, ~/.npm-global/bin, /opt/homebrew/bin and /usr/local/bin; set \
             OPENCODE_EXECUTABLE to override; install it with `brew install \
             anomalyco/tap/opencode` or `npm install -g opencode-ai`, then `opencode auth login`)"
                .into(),
        )
    })
}

/// `Some(why)` when `version` is an opencode older than [`MIN_CLI_VERSION`].
fn too_old_message(version: &semver::Version) -> Option<String> {
    let release = semver::Version::new(version.major, version.minor, version.patch);
    let min = semver::Version::parse(MIN_CLI_VERSION).ok()?;
    (release < min).then(|| {
        format!("opencode {version} is too old; update it to {min} or newer (brew upgrade anomalyco/tap/opencode)")
    })
}

/// The list an opencode that has not answered offers: whatever model it is configured with.
fn default_models() -> Vec<ModelInfo> {
    vec![ModelInfo {
        id: "default".into(),
        display_name: Some("opencode default".into()),
        description: Some("The model configured in opencode".into()),
        efforts: Vec::new(),
        default_effort: None,
        options: Vec::new(),
    }]
}

/// Whether `model` names no model at all (the run uses opencode's own).
fn is_default_model(model: Option<&str>) -> bool {
    model.is_none_or(|m| m.is_empty() || m == "default")
}

/// The agent a spec chose: the `agent` model option, other than "the server's default".
fn chosen_agent(spec: &ExecSpec) -> Option<&str> {
    spec.model_options
        .get("agent")
        .map(String::as_str)
        .filter(|a| !a.is_empty() && *a != DEFAULT_AGENT_CHOICE)
}

/// The agent a run is held to, and so the one it names: the chosen one, else, in a restricted
/// mode, `build`. A restricted run must not take whatever agent the owner's config makes the
/// default, since it is the agent read back from the server that proves the rules; full access
/// leaves the choice to opencode.
fn pinned_agent(spec: &ExecSpec) -> Option<&str> {
    chosen_agent(spec).or((spec.access != AccessMode::FullAccess).then_some("build"))
}

/// The opencode executor. Construct with [`OpencodeExecutor::new`]; tests point it at a fake
/// opencode with [`OpencodeExecutor::with_executable`].
///
/// # Access modes
///
/// All three. Full access adds no rules (opencode allows every tool by default; what it still
/// asks, such as reaching outside the project, goes to the kernel's policy like any request).
/// Auto-accept edits and supervised add `ask` rules to the server's inline config, and the run
/// refuses to prompt unless the agent read back from the server is held to them
/// ([`config::rules_in_force`]). The sandbox is `Unrestricted` only: nothing here is a sandbox.
pub struct OpencodeExecutor {
    executable: Option<PathBuf>,
    interrupt_grace: Duration,
    kill_grace: Duration,
    probe_budget: Duration,
    startup: Duration,
    read_back: Duration,
    stall: Option<Duration>,
    bus: BusTimeouts,
    cli_version: Arc<Mutex<Option<String>>>,
    models: Arc<Mutex<Option<Vec<ModelInfo>>>>,
    tracker: TaskTracker,
    unknown: UnknownFrames,
}

impl std::fmt::Debug for OpencodeExecutor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpencodeExecutor")
            .field("executable", &self.executable)
            .finish_non_exhaustive()
    }
}

impl Default for OpencodeExecutor {
    fn default() -> Self {
        Self {
            executable: None,
            interrupt_grace: Duration::from_secs(10),
            kill_grace: Duration::from_secs(3),
            probe_budget: Duration::from_secs(45),
            startup: startup_timeout(),
            read_back: READ_BACK_BUDGET,
            stall: stall_bound(),
            bus: BusTimeouts::default(),
            cli_version: Arc::new(Mutex::new(None)),
            models: Arc::new(Mutex::new(None)),
            tracker: TaskTracker::new(),
            unknown: UnknownFrames::new(),
        }
    }
}

impl OpencodeExecutor {
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

    /// Tune the abort grace and the SIGTERM to SIGKILL grace.
    #[must_use]
    pub fn with_graces(mut self, interrupt_grace: Duration, kill_grace: Duration) -> Self {
        self.interrupt_grace = interrupt_grace;
        self.kill_grace = kill_grace;
        self
    }

    /// Bound the server `probe` starts (tests shrink it; default 45 s).
    #[must_use]
    pub fn with_probe_budget(mut self, budget: Duration) -> Self {
        self.probe_budget = budget;
        self
    }

    /// How long a server may take to answer after it was started.
    #[must_use]
    pub fn with_startup_timeout(mut self, startup: Duration) -> Self {
        self.startup = startup;
        self
    }

    /// How long the rules and MCP servers of the inline config may take to show in the server.
    #[must_use]
    pub fn with_read_back_budget(mut self, budget: Duration) -> Self {
        self.read_back = budget;
        self
    }

    /// The bound between a prompt's acceptance and the first event of its session (`None`: none).
    #[must_use]
    pub fn with_stall_bound(mut self, stall: Option<Duration>) -> Self {
        self.stall = stall;
        self
    }

    /// Tune the bus liveness bounds (tests shrink them).
    #[must_use]
    pub fn with_bus_bounds(mut self, silence: Duration, probe: Duration, wedged: Duration) -> Self {
        self.bus = BusTimeouts {
            silence,
            probe,
            wedged,
        };
        self
    }

    /// Whether the CLI can be launched now.
    pub fn installed(&self) -> bool {
        resolve_executable(self.executable.as_deref()).is_ok()
    }

    /// The models opencode lists, and whether it can run any: one short-lived server, reaped before
    /// this returns, in a scratch directory, offline for its model catalog (no models.dev fetch).
    /// No prompt is sent, no model is asked anything. 1.x says which providers it holds a
    /// credential for (`connected`); 2.x lists the enabled models.
    async fn query(
        &self,
        exe: &Path,
        protocol: Protocol,
    ) -> Result<(Vec<ModelInfo>, SignedIn), HarnessError> {
        let started = Instant::now();
        let cwd = std::env::temp_dir();
        let env = std::collections::BTreeMap::new();
        let spec = SpawnSpec {
            exe,
            cwd: &cwd,
            expected: protocol,
            inline_config: None,
            run_env: &env,
            offline_catalog: true,
            startup: self.probe_budget,
        };
        let mut server = tokio::time::timeout(self.probe_budget, Server::spawn(&spec))
            .await
            .map_err(|_| HarnessError::Protocol("opencode probe timed out".into()))??;
        let remaining = self.probe_budget.saturating_sub(started.elapsed());
        let asked = tokio::time::timeout(remaining, async {
            let catalog = fetch_catalog(&server.api, None, true).await?;
            let agents = server
                .api
                .json(&requests::agents(protocol), None)
                .await
                .ok();
            let option = agents.as_ref().and_then(catalog::agent_option);
            let models = catalog.models(option.as_ref());
            // 1.x says which providers hold a credential; 2.x lists only the enabled models.
            let signed_in = match catalog.connected() {
                Some(connected) => !connected.is_empty(),
                None => !models.is_empty(),
            };
            Ok::<_, HarnessError>((models, signed_in))
        })
        .await;
        server.shutdown(self.kill_grace).await;
        let (models, signed_in) =
            asked.map_err(|_| HarnessError::Protocol("opencode probe timed out".into()))??;
        *self.models.lock().unwrap_or_else(PoisonError::into_inner) = Some(models.clone());
        let signed_in = if signed_in {
            SignedIn::Yes { account: None }
        } else {
            SignedIn::No {
                hint: "opencode has no usable provider: run `opencode auth login`, or set a \
                       provider API key (ANTHROPIC_API_KEY, OPENAI_API_KEY, ...)"
                    .into(),
            }
        };
        Ok((models, signed_in))
    }
}

/// The catalog, from `/provider` or `/api/model`. 2.x syncs its catalog from models.dev shortly
/// after its health route opens: an empty list right then is a race, not a fact, so a catalog
/// that must not be empty is asked again a few times.
async fn fetch_catalog(
    api: &Api,
    dir: Option<&str>,
    settle: bool,
) -> Result<ProviderCatalog, HarnessError> {
    for attempt in 0..5 {
        let body = api.bytes(&requests::models(api.protocol), dir).await?;
        let catalog = match api.protocol {
            Protocol::V1 => ProviderCatalog::from_v1(&body),
            Protocol::V2 => ProviderCatalog::from_v2(&body),
        }
        .map_err(HarnessError::Protocol)?;
        let empty = catalog.models(None).is_empty();
        if !empty || !settle || api.protocol == Protocol::V1 || attempt == 4 {
            return Ok(catalog);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    unreachable!("the loop returns on its last attempt")
}

#[async_trait]
impl Executor for OpencodeExecutor {
    fn maintenance(&self) -> Option<crate::exec::maintenance::MaintenanceDecl> {
        crate::exec::maintenance::declaration(&HarnessId::Opencode)
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
                    "reno-harness {} opencode driver (CLI {cli})",
                    env!("CARGO_PKG_VERSION")
                ),
                None => format!("reno-harness {} opencode driver", env!("CARGO_PKG_VERSION")),
            },
            cli_verified: VersionRange {
                min: MIN_CLI_VERSION.into(),
                verified: VERIFIED_CLI_VERSION.into(),
            },
            caps: ExecutorCaps {
                // The model and the variant are set once, on the session or the first prompt.
                live_effort: None,
                live_model_options: None,
                // A steering message goes out as the next prompt, when the session settles.
                steering: Steering::TurnBoundary,
                resume: true,
                access_modes: vec![
                    AccessMode::Supervised,
                    AccessMode::AutoAcceptEdits,
                    AccessMode::FullAccess,
                ],
                sandbox_levels: vec![SandboxLevel::Unrestricted],
                mcp_injection: true,
                structured_output: false,
                // Done is the idle of the run's own session, after it was busy.
                deterministic_turn_end: true,
                // ExecSpec has no plan input; opencode's `plan` agent is the `agent` model option.
                plan_mode: false,
                models,
                model_switch: false,
                // opencode's global config, plugins and commands load whatever Reno asks (see
                // the module docs).
                isolates_user_config: false,
                model_aliases: vec![],
            },
        }
    }

    /// Where the CLI is, its version against the verified range, and, from one short-lived server
    /// with no prompt, which models it lists and whether it holds a credential for any provider.
    /// A clean install is signed in: opencode's anonymous tier lists free models. When the server
    /// cannot be asked the sign-in is `Unknown` and the models are the last live list, else the
    /// curated one.
    async fn probe(&self) -> HealthProbe {
        let overridden = self.executable.is_some()
            || std::env::var_os("OPENCODE_EXECUTABLE").is_some_and(|value| !value.is_empty());
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
        let parsed = version
            .as_deref()
            .and_then(|v| semver::Version::parse(v).ok());
        let (signed_in, models) = match (&version_gate, &parsed) {
            (VersionGate::TooOld { .. }, _) | (_, None) => (SignedIn::Unknown, fallback()),
            (_, Some(parsed)) => match self.query(&exe, Protocol::for_version(parsed)).await {
                Ok((models, signed_in)) => (signed_in, ModelCatalog::live(models)),
                Err(error) => {
                    tracing::debug!(%error, "the opencode probe failed");
                    (SignedIn::Unknown, fallback())
                }
            },
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
        require_access(
            spec.access,
            &[
                AccessMode::Supervised,
                AccessMode::AutoAcceptEdits,
                AccessMode::FullAccess,
            ],
        )?;
        require_sandbox(spec.sandbox, &[SandboxLevel::Unrestricted])?;
        if spec.output_schema.is_some() {
            return Err(ExecError::UnsupportedOutputSchema);
        }
        if let Some(session) = &spec.resume
            && !requests::valid_session_id(&session.0)
        {
            return Err(ExecError::InvalidSpec(format!(
                "{:?} is not an opencode session id",
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
        // The generation decides the shape of the config the server is started with, so it must
        // be known before it is.
        let probe_exe = exe.clone();
        let version =
            tokio::task::spawn_blocking(move || crate::executable::binary_version(&probe_exe))
                .await
                .ok()
                .flatten()
                .ok_or_else(|| {
                    ExecError::NotInstalled(
                        "could not tell which opencode version is installed (`opencode --version` \
                     printed none), and its wire depends on it"
                            .into(),
                    )
                })?;
        if let Some(why) = too_old_message(&version) {
            return Err(ExecError::NotInstalled(why));
        }
        let ExecControls {
            steering,
            cancel,
            answers,
            processes,
            // Neither a model switch nor a live config change is claimed: not read.
            ..
        } = controls;
        let (event_tx, event_rx) = mpsc::channel::<ExecEvent>(EVENT_CAPACITY);
        let run = Run {
            exe,
            protocol: Protocol::for_version(&version),
            spec,
            event_tx,
            steering,
            answers,
            cancel,
            processes,
            interrupt_grace: self.interrupt_grace,
            kill_grace: self.kill_grace,
            startup: self.startup,
            read_back: self.read_back,
            stall: self.stall,
            bus: self.bus,
            unknown: self.unknown.clone(),
            tracker: self.tracker.clone(),
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
// One run
// ---------------------------------------------------------------------------

/// How a run ended.
type Ended = Ending;

fn failed(diagnostic: impl Into<String>) -> Ended {
    Ending {
        stop: StopReason::Error,
        result: None,
        diagnostic: Some(diagnostic.into()),
    }
}

fn interrupted() -> Ended {
    Ending {
        stop: StopReason::Interrupted,
        result: None,
        diagnostic: None,
    }
}

/// What a detached task reports back to the run.
#[derive(Debug)]
enum Update {
    /// The server took the prompt.
    PromptAccepted,
    /// The prompt never reached the server's queue.
    PromptFailed(String),
    /// A reply could not be delivered: the agent is blocked on an answer it will never get.
    ReplyFailed(String),
}

struct Run {
    exe: PathBuf,
    protocol: Protocol,
    spec: ExecSpec,
    event_tx: mpsc::Sender<ExecEvent>,
    steering: mpsc::Receiver<String>,
    answers: mpsc::Receiver<ExecAnswer>,
    cancel: CancellationToken,
    processes: ProcessSink,
    interrupt_grace: Duration,
    kill_grace: Duration,
    startup: Duration,
    read_back: Duration,
    stall: Option<Duration>,
    bus: BusTimeouts,
    unknown: UnknownFrames,
    tracker: TaskTracker,
}

impl Run {
    /// The whole run: start, play, end exactly once, tear down.
    async fn drive(mut self) {
        let mut server = None;
        let ended = self.lifecycle(&mut server).await;
        if let Some(ended) = ended {
            let done = ExecEvent::Done {
                stop: ended.stop,
                result: ended.result,
                diagnostic: ended
                    .diagnostic
                    .map(|diagnostic| crate::redact_secrets(&diagnostic)),
                reason: None,
            };
            let _ = self.event_tx.send(done).await;
        }
        if let Some(mut server) = server {
            server.shutdown(self.kill_grace).await;
        }
    }

    /// `None`: the consumer is gone, there is nobody to tell.
    async fn lifecycle(&mut self, slot: &mut Option<Server>) -> Option<Ended> {
        let restricted = self.spec.access != AccessMode::FullAccess;
        let mcp_names: Vec<String> = self.spec.mcp.iter().map(|m| m.name.clone()).collect();
        let inline = RunConfig {
            protocol: self.protocol,
            access: self.spec.access,
            mcp: &self.spec.mcp,
        }
        .build()
        .map(|config| config.to_string());
        let dir = self.spec.cwd.to_str()?.to_owned();
        let spawn_spec = SpawnSpec {
            exe: &self.exe,
            cwd: &self.spec.cwd,
            expected: self.protocol,
            inline_config: inline,
            run_env: &self.spec.env,
            offline_catalog: false,
            startup: self.startup,
        };
        let cancel = self.cancel.clone();
        let spawned = tokio::select! {
            () = cancel.cancelled() => return Some(interrupted()),
            spawned = Server::spawn(&spawn_spec) => spawned,
        };
        let server = slot.insert(match spawned {
            Ok(server) => server,
            Err(error) => return Some(failed(error.to_string())),
        });
        // Reported at once: a daemon that dies from here on still leaves its row.
        #[cfg(unix)]
        if let Some(record) = server.record() {
            let _ = self.processes.report(record).await;
        }
        let api = server.api.clone();

        let setup = tokio::select! {
            () = cancel.cancelled() => return Some(interrupted()),
            setup = self.set_up(&api, &dir, restricted, &mcp_names) => setup,
        };
        let setup = match setup {
            Ok(setup) => setup,
            Err(why) => return Some(failed(why)),
        };
        let started = ExecEvent::Started {
            session: SessionRef(setup.session.clone()),
            model: self
                .spec
                .model
                .clone()
                .filter(|m| !is_default_model(Some(m)))
                .unwrap_or_else(|| "default".into()),
        };
        if self.event_tx.send(started).await.is_err() {
            return None;
        }
        if let Some(note) = &setup.note
            && self
                .event_tx
                .send(ExecEvent::Text { text: note.clone() })
                .await
                .is_err()
        {
            return None;
        }

        let (bus_tx, bus_rx) = mpsc::channel(256);
        let bus_task = tokio::spawn(bus::run(api.clone(), bus_tx, self.bus));
        let played = {
            let mut player = Player::new(self, server, &api, &dir, setup, bus_rx, mcp_names);
            player.play().await
        };
        bus_task.abort();
        played
    }

    /// Everything before the prompt: the rules read back, the model resolved, the session made
    /// or resumed and given its model and agent.
    async fn set_up(
        &self,
        api: &Api,
        dir: &str,
        restricted: bool,
        mcp_names: &[String],
    ) -> Result<Setup, String> {
        let pinned = pinned_agent(&self.spec).map(str::to_owned);
        self.read_back(
            api,
            dir,
            restricted,
            pinned.as_deref().unwrap_or("build"),
            mcp_names,
        )
        .await?;
        let catalog = match fetch_catalog(
            api,
            Some(dir),
            !is_default_model(self.spec.model.as_deref()),
        )
        .await
        {
            Ok(catalog) => Some(catalog),
            Err(error) if is_default_model(self.spec.model.as_deref()) => {
                tracing::debug!(%error, "the opencode catalog could not be read; context windows unknown");
                None
            }
            Err(error) => return Err(error.to_string()),
        };
        let model = match (&catalog, self.spec.model.as_deref()) {
            (Some(catalog), Some(wanted)) if !is_default_model(Some(wanted)) => {
                Some(catalog.resolve(wanted)?)
            }
            _ => None,
        };
        let variant = match (&catalog, &model) {
            (Some(catalog), Some((provider, id))) => {
                catalog.variant(provider, id, self.spec.effort)
            }
            _ => None,
        };
        let windows = catalog
            .as_ref()
            .map(ProviderCatalog::context_windows)
            .unwrap_or_default();
        let chosen = pinned;

        let mut note = None;
        let mut session = None;
        if let Some(resume) = &self.spec.resume {
            match api
                .json(&requests::get_session(api.protocol, &resume.0), Some(dir))
                .await
            {
                Ok(info) => {
                    let info = requests::unwrap_data(&info);
                    session = Some(
                        info.get("id")
                            .and_then(Value::as_str)
                            .unwrap_or(&resume.0)
                            .to_owned(),
                    );
                    // A session a plan run left on the plan agent goes back to the default one.
                    if api.protocol == Protocol::V2 {
                        let left_on_plan =
                            info.get("agent").and_then(Value::as_str) == Some("plan");
                        let agent = chosen.as_deref().or(left_on_plan.then_some("build"));
                        if let (Some(id), Some(agent)) = (&session, agent) {
                            api.call(&requests::set_session_agent(id, agent), Some(dir))
                                .await
                                .map_err(|e| e.to_string())?;
                        }
                    }
                }
                Err(error) => {
                    tracing::debug!(%error, "opencode session resume failed");
                    note = Some(format!(
                        "\n[opencode did not find session {}; starting a new session without the previous context]\n",
                        resume.0
                    ));
                }
            }
        }
        let session = match session {
            Some(session) => session,
            None => {
                let created = api
                    .json(
                        &requests::create_session(api.protocol, Some(dir), chosen.as_deref()),
                        Some(dir),
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                requests::unwrap_data(&created)
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or("opencode session create returned no id")?
            }
        };
        // 2.x: the model and variant ride the session, once.
        if api.protocol == Protocol::V2
            && let Some((provider, id)) = &model
        {
            api.call(
                &requests::set_session_model(&session, provider, id, variant.as_deref()),
                Some(dir),
            )
            .await
            .map_err(|e| e.to_string())?;
        }
        Ok(Setup {
            session,
            model,
            variant,
            agent: chosen,
            windows,
            note,
        })
    }

    /// Prove the inline config is in force: the agent held to the access mode's rules, and the
    /// run's MCP servers registered. The config is read a moment after a server answers, so the
    /// read-back polls for its budget; what never shows fails the run.
    async fn read_back(
        &self,
        api: &Api,
        dir: &str,
        restricted: bool,
        agent: &str,
        mcp_names: &[String],
    ) -> Result<(), String> {
        if !restricted && mcp_names.is_empty() {
            return Ok(());
        }
        let deadline = Instant::now() + self.read_back;
        loop {
            let outcome = self
                .check_config(api, dir, restricted, agent, mcp_names)
                .await;
            match outcome {
                Ok(()) => return Ok(()),
                Err(why) if Instant::now() >= deadline => return Err(why),
                Err(_) => tokio::time::sleep(Duration::from_millis(250)).await,
            }
        }
    }

    async fn check_config(
        &self,
        api: &Api,
        dir: &str,
        restricted: bool,
        agent: &str,
        mcp_names: &[String],
    ) -> Result<(), String> {
        if restricted {
            let found = match api.protocol {
                Protocol::V1 => {
                    let list = api
                        .json(&requests::agents(Protocol::V1), Some(dir))
                        .await
                        .map_err(|e| e.to_string())?;
                    list.as_array()
                        .and_then(|a| {
                            a.iter()
                                .find(|x| x.get("name").and_then(Value::as_str) == Some(agent))
                        })
                        .cloned()
                }
                Protocol::V2 => {
                    let one = api
                        .json(&requests::agent(agent), Some(dir))
                        .await
                        .map_err(|e| e.to_string())?;
                    Some(requests::unwrap_data(&one).clone())
                }
            };
            let found = found.ok_or_else(|| format!("opencode lists no agent {agent:?}"))?;
            config::rules_in_force(&found, self.spec.access)?;
        }
        if !mcp_names.is_empty() {
            let listing = api
                .json(&requests::mcp_listing(api.protocol), Some(dir))
                .await
                .map_err(|e| e.to_string())?;
            let names: Vec<&str> = mcp_names.iter().map(String::as_str).collect();
            config::mcp_registered(&listing, &names)?;
        }
        Ok(())
    }
}

/// What the run's setup decided.
struct Setup {
    session: String,
    model: Option<(String, String)>,
    variant: Option<String>,
    /// The agent a 1.x prompt names (2.x set it on the session).
    agent: Option<String>,
    windows: std::collections::HashMap<String, u64>,
    /// Something to tell the person before the run starts.
    note: Option<String>,
}

/// A message id in opencode's own ascending form: `msg_`, 12 hex digits of the millisecond clock
/// times 4096 plus a per-millisecond counter, then 14 base62 characters. Sessions order their
/// messages by id, so ours must sort like one the server would mint at the same moment. Both wires
/// take it on the prompt, which makes a lost answer checkable and a retry safe.
fn prompt_message_id() -> String {
    static CLOCK: Mutex<(u64, u64)> = Mutex::new((0, 0));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or_default())
        .unwrap_or_default();
    let counter = {
        let mut clock = CLOCK.lock().unwrap_or_else(PoisonError::into_inner);
        if clock.0 != now {
            *clock = (now, 0);
        }
        clock.1 += 1;
        clock.1
    };
    let stamp = now.wrapping_mul(0x1000).wrapping_add(counter) & 0xFFFF_FFFF_FFFF;
    const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let random: String = uuid::Uuid::new_v4().as_bytes()[..14]
        .iter()
        .map(|b| BASE62[usize::from(*b) % BASE62.len()] as char)
        .collect();
    format!("msg_{stamp:012x}{random}")
}

/// What a reply is waiting on a person for.
type Table = OpenRequests<Permission, Asked>;

enum Flow {
    Continue,
    Idle,
    Interrupted,
    ConsumerGone,
}

/// The run after setup: the bus, the reducer, what is open and what is waiting.
struct Player<'a> {
    run: &'a mut Run,
    server: &'a mut Server,
    api: &'a Api,
    dir: &'a str,
    session: String,
    model: Option<(String, String)>,
    variant: Option<String>,
    agent: Option<String>,
    reducer: Reducer,
    bus: mpsc::Receiver<BusMsg>,
    updates_tx: mpsc::UnboundedSender<Update>,
    updates: mpsc::UnboundedReceiver<Update>,
    open: Table,
    queued: VecDeque<String>,
    /// The prompt delivery in flight: cancelled when the run ends, so a retry never posts it again.
    delivery: CancellationToken,
    stall_deadline: Option<Instant>,
    poll_at: Option<Instant>,
    steering_open: bool,
    answers_open: bool,
    first_prompt: Option<String>,
}

impl<'a> Player<'a> {
    fn new(
        run: &'a mut Run,
        server: &'a mut Server,
        api: &'a Api,
        dir: &'a str,
        setup: Setup,
        bus: mpsc::Receiver<BusMsg>,
        mcp_names: Vec<String>,
    ) -> Self {
        let (updates_tx, updates) = mpsc::unbounded_channel();
        let reducer = Reducer::new(&setup.session, mcp_names, setup.windows);
        let first_prompt = Some(
            match run
                .spec
                .instructions
                .as_deref()
                .filter(|i| !i.trim().is_empty())
            {
                Some(instructions) => format!("{instructions}\n\n{}", run.spec.prompt),
                None => run.spec.prompt.clone(),
            },
        );
        Self {
            run,
            server,
            api,
            dir,
            session: setup.session,
            model: setup.model,
            variant: setup.variant,
            agent: setup.agent,
            reducer,
            bus,
            updates_tx,
            updates,
            open: Table::new(),
            queued: VecDeque::new(),
            delivery: CancellationToken::new(),
            stall_deadline: None,
            poll_at: None,
            steering_open: true,
            answers_open: true,
            first_prompt,
        }
    }

    async fn emit(&self, event: ExecEvent) -> bool {
        self.run.event_tx.send(event).await.is_ok()
    }

    /// Wait for the bus to be live, then post the prompt and play the run to its end.
    async fn play(&mut self) -> Option<Ended> {
        // The bus has no replay: prompt before the subscription is live and a turn that fails
        // fast can run its whole lifecycle in the gap.
        let connected = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                match self.bus.recv().await {
                    Some(BusMsg::Connected) | None => return,
                    Some(_) => {}
                }
            }
        })
        .await;
        if connected.is_err() {
            tracing::debug!(target: "reno_harness::opencode", "event bus not connected within 15s; prompting anyway");
        }
        let prompt = self.first_prompt.take().unwrap_or_default();
        self.post_prompt(prompt);
        let mut server_poll = tokio::time::interval(SERVER_POLL);
        server_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let cancel = self.run.cancel.clone();
        loop {
            let stall = async {
                match self.stall_deadline.filter(|_| {
                    self.reducer.active()
                        && !self.reducer.saw_activity()
                        && !self.reducer.prompt_in_flight()
                }) {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            let poll = async {
                match self.poll_at {
                    Some(at) => tokio::time::sleep_until(at).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                biased;
                () = cancel.cancelled() => return self.interrupt().await,
                () = self.run.event_tx.closed() => {
                    self.delivery.cancel();
                    return None;
                }
                update = self.updates.recv() => {
                    let Some(update) = update else { continue };
                    if let Some(ended) = self.update(update).await {
                        return ended;
                    }
                }
                steer = self.run.steering.recv(), if self.steering_open => match steer {
                    Some(text) => self.queued.push_back(text),
                    None => self.steering_open = false,
                },
                answer = self.run.answers.recv(), if self.answers_open => match answer {
                    Some(answer) => self.answer(answer),
                    None => self.answers_open = false,
                },
                () = stall => {
                    let message = format!(
                        "opencode made no progress for {}s after the prompt. {STALL_HINT}",
                        self.run.stall.unwrap_or(DEFAULT_STALL).as_secs()
                    );
                    self.abort_quietly().await;
                    return Some(failed(message));
                }
                () = poll => {
                    self.poll_at = None;
                    let running = self.session_running().await;
                    if let Some(effect) = self.reducer.settled_by_status(running.unwrap_or(true)) {
                        let flow = self.apply(vec![effect]).await;
                        if let Some(ended) = self.flow(flow).await {
                            return ended;
                        }
                    } else if self.reducer.active() {
                        self.poll_at = Some(Instant::now() + self.reducer.poll_delay());
                    }
                }
                msg = self.bus.recv() => {
                    let Some(msg) = msg else {
                        return Some(failed("opencode's event bus ended"));
                    };
                    if let Some(ended) = self.bus_msg(msg).await {
                        return ended;
                    }
                }
                // Last, so frames the server wrote before dying are handled before its crash ends
                // the run.
                _ = server_poll.tick() => {
                    if let Some(status) = self.server.exited() {
                        self.server.stderr_tail.wait_closed().await;
                        return Some(failed(crate::crash_message("opencode serve", Some(status), &self.server.stderr_tail)));
                    }
                }
            }
        }
    }

    /// `Some(None)`: the consumer is gone. `Some(Some(ended))`: the run is over.
    async fn flow(&mut self, flow: Flow) -> Option<Option<Ended>> {
        match flow {
            Flow::Continue => None,
            Flow::ConsumerGone => Some(None),
            Flow::Interrupted => Some(Some(interrupted())),
            Flow::Idle => self.settled().await,
        }
    }

    /// The turn is over: the next steering message starts another, else the run ends.
    async fn settled(&mut self) -> Option<Option<Ended>> {
        self.poll_at = None;
        let ending = self.reducer.ending(false);
        if ending.stop == StopReason::Error {
            return Some(Some(ending));
        }
        while self.steering_open
            && let Ok(text) = self.run.steering.try_recv()
        {
            self.queued.push_back(text);
        }
        if self.queued.is_empty() {
            return Some(Some(ending));
        }
        let steer: Vec<String> = self.queued.drain(..).collect();
        self.post_prompt(steer.join("\n\n"));
        None
    }

    /// Start a turn: post `text` as a prompt, on a detached task so a slow POST never blocks the
    /// run's other duties (cancelling, answering).
    fn post_prompt(&mut self, text: String) {
        self.reducer.begin_turn(true);
        self.stall_deadline = None;
        self.delivery = CancellationToken::new();
        let message_id = prompt_message_id();
        let model = self.model.clone();
        let request = requests::prompt(
            self.api.protocol,
            &self.session,
            &text,
            model.as_ref().map(|(p, m)| (p.as_str(), m.as_str())),
            self.variant.as_deref(),
            self.agent.as_deref(),
            &message_id,
        );
        let api = self.api.clone();
        let session = self.session.clone();
        let dir = self.dir.to_owned();
        let delivery = self.delivery.clone();
        let updates = self.updates_tx.clone();
        self.run.tracker.spawn(async move {
            let delivered =
                deliver_prompt(&api, &request, &session, &message_id, &dir, &delivery).await;
            if delivery.is_cancelled() {
                return;
            }
            let _ = updates.send(match delivered {
                Ok(()) => Update::PromptAccepted,
                Err(message) => Update::PromptFailed(message),
            });
        });
    }

    async fn update(&mut self, update: Update) -> Option<Option<Ended>> {
        match update {
            Update::PromptAccepted => {
                self.reducer.prompt_accepted();
                if !self.reducer.saw_activity() {
                    self.stall_deadline = self.run.stall.map(|d| Instant::now() + d);
                }
                None
            }
            // Its POST failed, but the bus already shows the turn running: the server has it.
            Update::PromptFailed(message) if self.reducer.running_anyway() => {
                tracing::warn!(target: "reno_harness::opencode", "prompt POST failed but the turn is running: {message}");
                self.reducer.prompt_accepted();
                None
            }
            Update::PromptFailed(message) => Some(Some(self.lost_server_or(failed(message)).await)),
            Update::ReplyFailed(message) => {
                // Whatever waits on the reply would never resume.
                self.abort_quietly().await;
                Some(Some(self.lost_server_or(failed(message)).await))
            }
        }
    }

    /// A request that failed because the server died reports the crash, not the transport error.
    async fn lost_server_or(&mut self, ended: Ended) -> Ended {
        match self.server.exited() {
            Some(status) => {
                self.server.stderr_tail.wait_closed().await;
                failed(crate::crash_message(
                    "opencode serve",
                    Some(status),
                    &self.server.stderr_tail,
                ))
            }
            None => ended,
        }
    }

    async fn bus_msg(&mut self, msg: BusMsg) -> Option<Option<Ended>> {
        match msg {
            BusMsg::Connected => {
                self.resync().await;
                None
            }
            BusMsg::Frame(frame) => {
                let effects = self.reducer.frame(&frame);
                for kind in self.reducer.take_unknown() {
                    self.run.unknown.note(&kind);
                }
                let flow = self.apply(effects).await;
                self.flow(flow).await
            }
            BusMsg::Disconnected => {
                let ended = self
                    .lost_server_or(failed(
                        "opencode's event bus could not be reconnected; the transcript can no longer be trusted",
                    ))
                    .await;
                Some(Some(ended))
            }
            BusMsg::Unresponsive(message) => {
                let ended = self.lost_server_or(failed(message)).await;
                Some(Some(ended))
            }
        }
    }

    /// The bus came back after a gap that may have swallowed frames. Requests asked in it are
    /// re-listed and handled like live ones (the agent waits on them), then the turn is re-read
    /// from the server's own status.
    async fn resync(&mut self) {
        if self.reducer.active() && !self.reducer.prompt_in_flight() {
            let sessions = self.reducer.owned_sessions();
            let mut frames = Vec::new();
            for request in requests::pending(self.api.protocol, &sessions) {
                let Ok(Ok(list)) = tokio::time::timeout(
                    Duration::from_secs(10),
                    self.api.json(&request, Some(self.dir)),
                )
                .await
                else {
                    continue;
                };
                for item in requests::unwrap_data(&list)
                    .as_array()
                    .into_iter()
                    .flatten()
                {
                    if request.path.contains("form") {
                        frames.extend(v2::question_form_payload(item));
                    } else if request.path.ends_with("/question") {
                        frames.push(
                            serde_json::json!({"type": "question.asked", "properties": item}),
                        );
                    } else {
                        frames.push(
                            serde_json::json!({"type": "permission.asked", "properties": item}),
                        );
                    }
                }
            }
            for frame in frames {
                let effects = self.reducer.frame(&frame);
                let _ = self.apply(effects).await;
            }
        }
        if self.reducer.reconnected() && self.session_running().await == Some(false) {
            self.poll_at = Some(Instant::now());
        }
    }

    /// `Some(running)`, or `None` when the server could not say.
    async fn session_running(&self) -> Option<bool> {
        let request = requests::session_activity(self.api.protocol);
        let activity = tokio::time::timeout(
            Duration::from_secs(2),
            self.api.json(&request, Some(self.dir)),
        )
        .await
        .ok()?
        .ok()?;
        Some(requests::is_running(&activity, &self.session))
    }

    async fn apply(&mut self, effects: Vec<Effect>) -> Flow {
        for effect in effects {
            match effect {
                Effect::Event(event) => {
                    if !self.emit(event).await {
                        return Flow::ConsumerGone;
                    }
                }
                Effect::Permission(permission) => {
                    if self.open.len() >= MAX_OPEN_REQUESTS {
                        self.reply_permission(&permission, Reply::Reject);
                        continue;
                    }
                    let ask = ExecEvent::NeedsApproval {
                        request_id: permission.id.clone(),
                        subject: permission.subject.clone(),
                    };
                    self.open.open_approval(permission.id.clone(), permission);
                    if !self.emit(ask).await {
                        return Flow::ConsumerGone;
                    }
                }
                Effect::Question(asked) => {
                    if self.open.len() >= MAX_OPEN_REQUESTS {
                        self.deliver(asked.reply(None));
                        continue;
                    }
                    let ask = ExecEvent::NeedsInput {
                        request_id: asked.id.clone(),
                        questions: asked.questions.iter().map(|(_, q)| q.clone()).collect(),
                    };
                    self.open.open_input(asked.id.clone(), asked);
                    if !self.emit(ask).await {
                        return Flow::ConsumerGone;
                    }
                }
                Effect::Abort => self.abort_in_background(),
                Effect::CheckIdle => {
                    self.poll_at = Some(Instant::now() + self.reducer.poll_delay());
                }
                Effect::Idle => return Flow::Idle,
                Effect::Interrupted => return Flow::Interrupted,
            }
        }
        Flow::Continue
    }

    /// A person's answer to a request that is open.
    fn answer(&mut self, answer: ExecAnswer) {
        let request_id = answer.request_id.clone();
        match self.open.resolve(answer) {
            Resolved::Decision(permission, decision, _reason) => {
                let reply = match decision {
                    Decision::Allow => Reply::Once,
                    Decision::Deny => Reply::Reject,
                };
                self.reply_permission(&permission, reply);
            }
            Resolved::Input(asked, given) => {
                let labeled: Vec<(String, Vec<String>)> = given
                    .into_iter()
                    .map(|a| (a.question_id, a.labels))
                    .collect();
                self.deliver(asked.reply(Some(&labeled)));
            }
            Resolved::WrongKind | Resolved::Unknown => {
                tracing::debug!(target: "reno_harness::opencode", "answer for {request_id} matches no open request");
            }
        }
    }

    fn reply_permission(&self, permission: &Permission, reply: Reply) {
        let routes = requests::permission_reply(
            self.api.protocol,
            Some(&self.api.version),
            &permission.session_id,
            &permission.id,
            reply,
        );
        self.deliver_routes(routes);
    }

    fn deliver(&self, request: Option<Request>) {
        if let Some(request) = request {
            self.deliver_routes(vec![request]);
        }
    }

    /// Deliver a reply on a detached task: the agent blocks until one lands, so it is retried, and
    /// one that cannot be delivered ends the run.
    fn deliver_routes(&self, routes: Vec<Request>) {
        let api = self.api.clone();
        let dir = self.dir.to_owned();
        let updates = self.updates_tx.clone();
        self.run.tracker.spawn(async move {
            if let Err(error) = deliver_reply(&api, &routes, &dir).await {
                let _ = updates.send(Update::ReplyFailed(format!(
                    "Could not deliver an answer to opencode, so the agent cannot continue: {error}"
                )));
            }
        });
    }

    fn abort_in_background(&self) {
        let api = self.api.clone();
        let request = requests::abort(self.api.protocol, &self.session);
        let dir = self.dir.to_owned();
        self.run.tracker.spawn(async move {
            let _ =
                tokio::time::timeout(Duration::from_secs(5), api.send(&request, Some(&dir))).await;
        });
    }

    async fn abort_quietly(&self) {
        let request = requests::abort(self.api.protocol, &self.session);
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            self.api.send(&request, Some(self.dir)),
        )
        .await;
    }

    /// Stop the run: abort the session and its subagents, and give the server its grace to say
    /// the session is over.
    async fn interrupt(&mut self) -> Option<Ended> {
        self.delivery.cancel();
        self.queued.clear();
        if !self.reducer.active() {
            return Some(interrupted());
        }
        self.reducer.abort_sent();
        let request = requests::abort(self.api.protocol, &self.session);
        let sent = tokio::time::timeout(
            Duration::from_secs(5),
            self.api.send(&request, Some(self.dir)),
        )
        .await;
        if !matches!(sent, Ok(Ok((status, _))) if status.is_success()) {
            // The abort failing means the server itself is wedged: end now, tear down hard.
            return Some(interrupted());
        }
        self.abort_children();
        let grace = self.run.interrupt_grace;
        let _ = tokio::time::timeout(grace, async {
            while let Some(msg) = self.bus.recv().await {
                match msg {
                    BusMsg::Frame(frame) => {
                        let effects = self.reducer.frame(&frame);
                        if effects
                            .iter()
                            .any(|e| matches!(e, Effect::Idle | Effect::Interrupted))
                        {
                            return;
                        }
                    }
                    BusMsg::Disconnected | BusMsg::Unresponsive(_) => return,
                    BusMsg::Connected => {}
                }
            }
        })
        .await;
        Some(interrupted())
    }

    /// After the root is aborted its subagent sessions go too: only foreground subagents end with
    /// their parent, and a background one would keep running and spending on a server that is
    /// about to be killed anyway, but through whatever it spawned. Detached and bounded.
    fn abort_children(&self) {
        let api = self.api.clone();
        let dir = self.dir.to_owned();
        let root = self.session.clone();
        let mut known = self.reducer.children();
        self.run.tracker.spawn(async move {
            let work = async {
                if api.protocol == Protocol::V1
                    && let Ok(listed) = api.json(&requests::children(&root), Some(&dir)).await
                {
                    known.extend(
                        listed
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|child| child.get("id").and_then(Value::as_str))
                            .map(str::to_owned),
                    );
                }
                known.sort();
                known.dedup();
                futures::future::join_all(known.iter().map(|child| {
                    let request = requests::abort(api.protocol, child);
                    let api = api.clone();
                    let dir = dir.clone();
                    async move { api.send(&request, Some(&dir)).await }
                }))
                .await;
            };
            let _ = tokio::time::timeout(CHILD_ABORT_BUDGET, work).await;
        });
    }
}

/// Deliver an answer the agent is blocked on. Each round tries `routes` in order (1.x has a
/// session-scoped fallback); a timeout, a transport fault or a 5xx retries with backoff. A request
/// every route reports missing (404) is no longer pending (answered elsewhere, or gone with its
/// turn), which counts as delivered.
async fn deliver_reply(api: &Api, routes: &[Request], dir: &str) -> Result<(), String> {
    let mut last_error = String::new();
    for attempt in 0..REPLY_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(1) * attempt).await;
        }
        let mut missing = 0;
        let mut retryable = false;
        for route in routes {
            match tokio::time::timeout(REPLY_TIMEOUT, api.send(route, Some(dir))).await {
                Ok(Ok((status, _))) if status.is_success() => return Ok(()),
                Ok(Ok((status, text))) => {
                    missing += usize::from(status == reqwest::StatusCode::NOT_FOUND);
                    retryable |= !status.is_client_error();
                    last_error = format!("opencode {}: {status} {}", route.path, text.trim());
                }
                Ok(Err(error)) => {
                    retryable = true;
                    last_error = error.to_string();
                }
                Err(_) => {
                    retryable = true;
                    last_error = format!(
                        "opencode {}: no answer within {}s",
                        route.path,
                        REPLY_TIMEOUT.as_secs()
                    );
                }
            }
        }
        if missing == routes.len() {
            return Ok(());
        }
        if !retryable {
            break;
        }
    }
    Err(last_error)
}

/// POST a prompt until the server has it. A 4xx is a definitive rejection. A timeout, a dropped
/// connection or a 5xx is ambiguous (the server may have queued the prompt and only the answer was
/// lost), so before posting again, and before giving up, the session is checked for it: running,
/// or holding the message under the id we minted. Only a prompt it does not have is posted again.
/// That check, not the server, keeps a repeat from being a duplicate turn: 2.x does not reject a
/// known id, it answers 200 and updates the stored message in place (zeron, 2.1.2).
async fn deliver_prompt(
    api: &Api,
    request: &Request,
    session: &str,
    message_id: &str,
    dir: &str,
    cancel: &CancellationToken,
) -> Result<(), String> {
    let mut last_error = String::new();
    for attempt in 0..PROMPT_ATTEMPTS {
        if attempt > 0 {
            tokio::select! {
                () = cancel.cancelled() => return Err("prompt delivery cancelled".into()),
                () = tokio::time::sleep(PROMPT_RETRY_DELAY * attempt) => {}
            }
            if prompt_landed(api, session, message_id, dir).await {
                return Ok(());
            }
            if cancel.is_cancelled() {
                return Err("prompt delivery cancelled".into());
            }
        }
        match api.send(request, Some(dir)).await {
            Ok((status, _)) if status.is_success() => return Ok(()),
            Ok((status, text)) if status.is_client_error() => {
                return Err(format!(
                    "opencode {}: {status} {}",
                    request.path,
                    text.trim()
                ));
            }
            Ok((status, text)) => {
                last_error = format!("opencode {}: {status} {}", request.path, text.trim())
            }
            Err(error) => last_error = error.to_string(),
        }
        tracing::warn!(target: "reno_harness::opencode", attempt, "prompt POST failed: {last_error}");
    }
    if prompt_landed(api, session, message_id, dir).await {
        return Ok(());
    }
    Err(last_error)
}

/// Did a prompt whose POST went unanswered reach the session anyway? Running means it did; so does
/// its message existing under the id we minted.
async fn prompt_landed(api: &Api, session: &str, message_id: &str, dir: &str) -> bool {
    let bound = Duration::from_secs(5);
    if let Ok(Ok(activity)) = tokio::time::timeout(
        bound,
        api.json(&requests::session_activity(api.protocol), Some(dir)),
    )
    .await
        && requests::is_running(&activity, session)
    {
        return true;
    }
    match tokio::time::timeout(
        bound,
        api.json(
            &requests::get_message(api.protocol, session, message_id),
            Some(dir),
        ),
    )
    .await
    {
        Ok(Ok(message)) => [
            message.pointer("/info/id"),
            message.pointer("/data/id"),
            message.get("id"),
        ]
        .into_iter()
        .flatten()
        .any(|id| id.as_str() == Some(message_id)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_opencode_older_than_the_checked_one_is_refused() {
        let old = semver::Version::new(1, 18, 31);
        assert!(too_old_message(&old).is_some_and(|m| m.contains("1.19.5")));
        let pre =
            semver::Version::parse("1.19.5-rc.1").unwrap_or_else(|_| semver::Version::new(0, 0, 0));
        assert!(
            too_old_message(&pre).is_none(),
            "a pre-release counts as its release"
        );
        assert!(too_old_message(&semver::Version::new(2, 3, 2)).is_none());
    }

    #[test]
    fn the_default_model_is_no_model_and_the_default_agent_is_build() {
        assert!(is_default_model(None));
        assert!(is_default_model(Some("")));
        assert!(is_default_model(Some("default")));
        assert!(!is_default_model(Some("opencode/big-pickle")));
    }

    #[test]
    fn a_message_id_sorts_in_the_order_it_was_minted() {
        let ids: Vec<String> = (0..50).map(|_| prompt_message_id()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
        assert!(ids[0].starts_with("msg_") && ids[0].len() == 4 + 12 + 14);
    }

    #[test]
    fn the_descriptor_claims_only_what_the_driver_does() {
        let caps = OpencodeExecutor::new().descriptor().caps;
        assert_eq!(
            caps.access_modes,
            [
                AccessMode::Supervised,
                AccessMode::AutoAcceptEdits,
                AccessMode::FullAccess
            ]
        );
        assert_eq!(caps.sandbox_levels, [SandboxLevel::Unrestricted]);
        assert_eq!(caps.steering, Steering::TurnBoundary);
        assert!(caps.resume && caps.mcp_injection && caps.deterministic_turn_end);
        assert!(!caps.structured_output && !caps.plan_mode && !caps.model_switch);
        assert!(!caps.isolates_user_config);
        assert!(caps.live_effort.is_none() && caps.live_model_options.is_none());
    }
}
