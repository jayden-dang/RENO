//! What differs between ACP agents: which binary to launch and how, which access modes the agent
//! honors, how steering and effort reach it, and the quirks pinned against its real CLI.
//!
//! The wire is the same for all of them (see the parent module); everything here is data.

use std::path::PathBuf;
use std::time::Duration;

use reno_proto::{AccessMode, Effort, Steering};

use crate::upstream_proto::HarnessId;

/// Per-agent configuration: which binary to spawn and what the descriptor declares.
pub(super) struct AcpAgentSpec {
    pub id: HarnessId,
    pub display_name: &'static str,
    /// Binary name searched on PATH (and platform install dirs).
    pub executable: &'static str,
    /// Env var overriding executable resolution (tests, custom installs).
    pub env_override: &'static str,
    /// Arguments that put the binary in ACP-serving mode.
    pub args: &'static [&'static str],
    /// Pinned npm package (`name@version`) installed ONCE into the managed adapters dir when the
    /// binary isn't already present: the launch then spawns `node <entry>` directly, keeping npm
    /// (and every way a user's npm state can break) out of runs. See [`crate::adapter_install`].
    pub npm_package: Option<&'static str>,
    /// Pinned release archive for this platform, installed once into the managed adapters dir
    /// when the binary isn't already present. See [`crate::archive_install`].
    pub archive: Option<crate::archive_install::ArchivePin>,
    /// Extra install locations to probe after PATH.
    pub extra_paths: fn() -> Vec<PathBuf>,
    /// Search summary and install hint for the `NotInstalled` error.
    pub install_hint: &'static str,
    /// Models the agent is known to offer, as `(id, display name)`. Not exhaustive and not a
    /// promise: the agent's own session decides, and a model it does not advertise fails the run.
    pub models: &'static [(&'static str, &'static str)],
    /// When a steering message lands.
    pub steering: Steering,
    /// Preference-ordered `thought_level` value ids for a run's effort. The first value the agent
    /// actually advertises wins.
    pub effort_values: fn(Option<Effort>) -> Vec<&'static str>,
    /// The agent emits `_x.ai/session/prompt_complete` (Grok): treat it as the AUTHORITATIVE turn
    /// end, because Grok's `session/prompt` RPC can hang silently after the turn really finished.
    /// The prompt response stays as the fallback; whichever lands first settles the turn, once.
    pub prompt_complete_extension: bool,
    /// Bound on prompt-send to FIRST sign of life on the wire. `None` disables. Grok's healthy
    /// runs acknowledge a prompt within milliseconds, so total silence past this window is a
    /// wedged agent (a stale shared leader, a dead update check).
    pub prompt_stall: Option<Duration>,
    /// Agent-specific advice appended to the stall diagnostic.
    pub stall_hint: &'static str,
    /// The agent advertises every effort as its own model id (`...-low`, `...-high`) instead of
    /// a `thought_level` option: a run sends the variant for its effort.
    pub effort_in_model_id: bool,
    /// Auth method to sign in with when `session/new` answers auth_required, for agents that
    /// expect the client to pick one before the first session. A run never signs in itself: it
    /// ends with a "not signed in" diagnostic.
    pub auth_method: Option<&'static str>,
    /// A prompt cancelled before it produced anything vanishes from the agent's history (Grok,
    /// verified live): a steer that preempts it re-sends its text.
    pub drops_unstarted_cancelled_prompt: bool,
    /// The access modes the agent honors. Beyond full access an agent must have a session mode
    /// that asks before acting (see `ASKING_MODES`): one that approves on its own never sends the
    /// permission to decide.
    pub access_modes: &'static [AccessMode],
    /// The oldest CLI version and the newest one the driver was verified against, when the
    /// imported code pins any. `None`: nothing is pinned, and the descriptor says so.
    pub cli_verified: Option<(&'static str, &'static str)>,
}

/// Only full access: the agent decides its own approvals.
const FULL_ACCESS_ONLY: &[AccessMode] = &[AccessMode::FullAccess];

/// Supervised and full access: an agent whose session mode that asks is known, but not one that
/// auto-accepts edits.
const SUPERVISED_OR_FULL: &[AccessMode] = &[AccessMode::Supervised, AccessMode::FullAccess];

/// Every mode.
const EVERY_MODE: &[AccessMode] = &[
    AccessMode::Supervised,
    AccessMode::AutoAcceptEdits,
    AccessMode::FullAccess,
];

/// Generic effort ladder for agents without their own clamping rules.
fn default_effort_values(effort: Option<Effort>) -> Vec<&'static str> {
    match effort {
        None => Vec::new(),
        Some(Effort::Low) => vec!["low", "minimal"],
        Some(Effort::Medium) => vec!["medium"],
        Some(Effort::High) => vec!["high"],
    }
}

fn grok_install_paths() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = crate::executable::home_dir() {
        dirs.push(home.join(".local").join("bin").join("grok"));
        dirs.push(home.join(".grok").join("bin").join("grok"));
        dirs.push(home.join(".npm-global").join("bin").join("grok"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin/grok"));
    dirs.push(PathBuf::from("/usr/local/bin/grok"));
    dirs
}

pub(super) fn grok() -> AcpAgentSpec {
    AcpAgentSpec {
        id: HarnessId::Grok,
        display_name: "Grok",
        executable: "grok",
        env_override: "GROK_EXECUTABLE",
        // Flag placement verified against grok 1.0.4: `--no-auto-update` is TOP-LEVEL (before
        // the subcommand) and kills the launch-time update check (a silent multi-second network
        // stall); `--no-leader` lives on the `agent` subcommand and starts a fresh agent even
        // when `[cli] use_leader` is set. Leader mode ATTACHES `agent stdio` to a shared process
        // via ~/.grok/leader.sock, so a wedged or stale leader (the user's TUI) reads as total
        // silent non-response.
        args: &["--no-auto-update", "agent", "--no-leader", "stdio"],
        npm_package: Some("@xai-official/grok@1.0.4"),
        archive: None,
        extra_paths: grok_install_paths,
        install_hint: "grok (searched PATH, the login shell's PATH, ~/.local/bin, \
             ~/.grok/bin, ~/.npm-global/bin, /opt/homebrew/bin, /usr/local/bin, and \
             fnm/nvm/volta/pnpm/bun install dirs; install with \
             `curl -fsSL https://x.ai/cli/install.sh | bash` or \
             `npm install -g @xai-official/grok`; set GROK_EXECUTABLE to override)",
        models: &[("grok-4.5", "Grok 4.5")],
        // No `_session/steering` extension: a steer preempts the generation (waiting out running
        // tools) and continues the turn, immediately.
        steering: Steering::StepBoundary,
        effort_values: default_effort_values,
        // Verified live (1.0.4): the notification fires with the echoed `_meta.promptId`, just
        // ahead of the RPC response.
        prompt_complete_extension: true,
        prompt_stall: Some(Duration::from_secs(30)),
        stall_hint: "The agent process is likely wedged: a stale shared leader process or a \
             hung startup check. Reno launches it with --no-leader and --no-auto-update to \
             avoid both.",
        effort_in_model_id: false,
        auth_method: None,
        drops_unstarted_cancelled_prompt: true,
        // Grok's Auto mode approves through its own classifier and asks only about what it
        // would deny, so most actions never reach a permission.
        access_modes: FULL_ACCESS_ONLY,
        // 1.0.4 is the pinned install and where the flags and `prompt_complete` were verified;
        // 1.0.46 is the newest version a pinned behavior (the `Path not found.` answer to an
        // unknown session) was observed on.
        cli_verified: Some(("1.0.4", "1.0.46")),
    }
}

fn devin_install_paths() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = crate::executable::home_dir() {
        // The official installer's launcher symlink (the binary lives below
        // ~/.local/share/devin/cli/_versions).
        dirs.push(home.join(".local").join("bin").join("devin"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin/devin"));
    dirs.push(PathBuf::from("/usr/local/bin/devin"));
    dirs
}

pub(super) fn devin() -> AcpAgentSpec {
    AcpAgentSpec {
        id: HarnessId::Devin,
        display_name: "Devin",
        executable: "devin",
        env_override: "DEVIN_EXECUTABLE",
        args: &["acp"],
        // Native ACP server: no adapter package in between.
        npm_package: None,
        archive: None,
        extra_paths: devin_install_paths,
        install_hint: "devin (searched PATH, the login shell's PATH, ~/.local/bin, \
             /opt/homebrew/bin, and /usr/local/bin; install with \
             `curl -fsSL https://cli.devin.ai/install.sh | bash` or \
             `brew install --cask devin-cli`, then `devin auth login`; set \
             DEVIN_EXECUTABLE to override)",
        models: &[
            ("swe-1-7-medium", "SWE-1.7 Medium"),
            ("claude-fable-5-1-high", "Claude Fable 5.1 High"),
            ("adaptive", "Adaptive"),
        ],
        // No `_session/steering` extension: a steer preempts the generation (waiting out running
        // tools) and continues the turn, immediately.
        steering: Steering::StepBoundary,
        effort_values: default_effort_values,
        prompt_complete_extension: false,
        prompt_stall: None,
        stall_hint: "The agent process is likely wedged.",
        effort_in_model_id: false,
        auth_method: None,
        drops_unstarted_cancelled_prompt: false,
        // Only Devin's `bypass` mode value is known; which of its others asks before every
        // command and edit is not.
        access_modes: FULL_ACCESS_ONLY,
        cli_verified: None,
    }
}

/// The spec every registry agent shares: nothing about the agent beyond its id and name, its
/// launch read from its record each time (an update moves it), its sign-in from its own
/// handshake.
pub(super) fn registry(agent: crate::upstream_proto::RegistryAgent, name: &str) -> AcpAgentSpec {
    AcpAgentSpec {
        id: HarnessId::Registry(agent),
        // Interned with the agent's id: bounded by the agents ever added.
        display_name: Box::leak(name.to_owned().into_boxed_str()),
        executable: agent.registry_id(),
        env_override: "",
        args: &[],
        npm_package: None,
        archive: None,
        extra_paths: Vec::new,
        install_hint: "Add it again from the ACP registry.",
        models: &[],
        // Steers wait for the turn's end: what an unknown agent does with a cancel mid-turn
        // isn't known.
        steering: Steering::TurnBoundary,
        effort_values: default_effort_values,
        prompt_complete_extension: false,
        prompt_stall: None,
        stall_hint: "The agent may be waiting on its own sign-in or setup.",
        effort_in_model_id: false,
        auth_method: None,
        drops_unstarted_cancelled_prompt: false,
        access_modes: SUPERVISED_OR_FULL,
        cli_verified: None,
    }
}

fn fx_install_paths() -> Vec<PathBuf> {
    crate::executable::home_dir()
        .map(|home| vec![home.join(".local/bin/fx")])
        .unwrap_or_default()
}

pub(super) fn fx() -> AcpAgentSpec {
    AcpAgentSpec {
        id: HarnessId::Fx,
        display_name: "fx",
        executable: "fx",
        env_override: "FX_EXECUTABLE",
        args: &["acp"],
        npm_package: None,
        archive: None,
        extra_paths: fx_install_paths,
        install_hint: "Install with `curl -fsSL https://fx.sh/setup.sh | bash`, then configure a provider with `fx`. Set FX_EXECUTABLE for a custom installation.",
        models: &[],
        steering: Steering::TurnBoundary,
        effort_values: default_effort_values,
        prompt_complete_extension: false,
        prompt_stall: None,
        stall_hint: "Check fx's provider configuration.",
        effort_in_model_id: false,
        auth_method: None,
        drops_unstarted_cancelled_prompt: false,
        access_modes: SUPERVISED_OR_FULL,
        cli_verified: None,
    }
}

fn hermes_install_paths() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = crate::executable::home_dir() {
        dirs.push(home.join(".local").join("bin").join("hermes"));
        dirs.push(home.join(".hermes").join("bin").join("hermes"));
    }
    dirs.push(PathBuf::from("/opt/homebrew/bin/hermes"));
    dirs.push(PathBuf::from("/usr/local/bin/hermes"));
    dirs
}

pub(super) fn hermes() -> AcpAgentSpec {
    AcpAgentSpec {
        id: HarnessId::Hermes,
        display_name: "Hermes",
        executable: "hermes",
        env_override: "HERMES_EXECUTABLE",
        args: &["acp"],
        // Python/uv install: no npm fallback exists.
        npm_package: None,
        archive: None,
        extra_paths: hermes_install_paths,
        install_hint: "hermes (searched PATH, the login shell's PATH, ~/.local/bin, \
             ~/.hermes/bin, /opt/homebrew/bin, /usr/local/bin, and fnm/nvm/volta/pnpm/bun \
             install dirs; install with \
             `curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash`, then \
             `cd ~/.hermes/hermes-agent && uv pip install -e '.[acp]'` for the ACP \
             server; set HERMES_EXECUTABLE to override)",
        // Hermes derives its model list from the providers the user has authenticated
        // (`hermes model`); these are the Nous flagships every portal account gets. An id the
        // agent doesn't advertise is skipped, and the agent's own default runs.
        models: &[
            ("hermes-4-405b", "Hermes 4 405B"),
            ("hermes-4-70b", "Hermes 4 70B"),
        ],
        // No `_session/steering` extension: steers deliver at turn boundaries.
        steering: Steering::TurnBoundary,
        // Hermes exposes no effort config over ACP today (hybrid reasoning is model-internal).
        effort_values: default_effort_values,
        prompt_complete_extension: false,
        prompt_stall: None,
        stall_hint: "The agent process is likely wedged.",
        effort_in_model_id: false,
        auth_method: None,
        drops_unstarted_cancelled_prompt: false,
        // Hermes advertises no session mode, and nothing shows it asking before every command
        // and edit.
        access_modes: FULL_ACCESS_ONLY,
        cli_verified: None,
    }
}

const ANTIGRAVITY_LINUX_ARGS: &[&str] = &["--uid="];

pub(super) fn antigravity() -> AcpAgentSpec {
    AcpAgentSpec {
        id: HarnessId::Antigravity,
        display_name: "Antigravity",
        executable: "agy_acp_server",
        env_override: "ANTIGRAVITY_ACP_EXECUTABLE",
        args: if cfg!(target_os = "linux") {
            ANTIGRAVITY_LINUX_ARGS
        } else {
            &[]
        },
        npm_package: None,
        archive: super::install::antigravity_archive(),
        extra_paths: Vec::new,
        install_hint: "Install Antigravity to enable, or set ANTIGRAVITY_ACP_EXECUTABLE to its ACP server",
        models: &[
            ("gemini-3.7-flash", "Gemini 3.7 Flash"),
            ("gemini-3.1-pro", "Gemini 3.1 Pro"),
        ],
        // No preemption: agy_acp_server 1.1.1 never answers a prompt (or any later one) when a
        // cancel lands as it starts replying after a tool (verified on the wire). Steers wait
        // for the turn end instead.
        steering: Steering::TurnBoundary,
        effort_values: default_effort_values,
        prompt_complete_extension: false,
        prompt_stall: None,
        stall_hint: "The agent process is likely wedged.",
        effort_in_model_id: true,
        // The personal oauth method is the first-run default.
        auth_method: Some("oauth-personal"),
        drops_unstarted_cancelled_prompt: false,
        // agy_acp_server's mode select is default / auto_edit / yolo: full access runs `yolo`,
        // the others stay in `default`, which asks, and each permission is decided here. No
        // plan mode is advertised.
        access_modes: EVERY_MODE,
        // 1.1.1 is the oldest build whose wire behavior is pinned in the driver; 1.2.1 is the
        // pinned release archive.
        cli_verified: Some(("1.1.1", "1.2.1")),
    }
}
