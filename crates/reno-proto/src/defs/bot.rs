//! `bots/<name>.toml`: a persistent identity with instructions, executor preferences, access,
//! budgets and triggers (ARCHITECTURE section 4).

use std::collections::BTreeSet;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::time::check_duration;
use super::{Cx, Sink, check_name, check_ref_name};
use crate::exec::{AccessMode, SandboxLevel};
use crate::ids::ExecutorId;
use crate::secrets::check_secret_name;

/// The longest `instructions` text, in bytes.
pub(super) const MAX_INSTRUCTIONS_BYTES: usize = 64 * 1024;
/// The most executors a bot may list.
pub(super) const MAX_EXECUTORS: usize = 16;
/// The most triggers a bot may list.
pub(super) const MAX_BOT_TRIGGERS: usize = 64;
/// The most secrets a bot may list.
pub(super) const MAX_BOT_SECRETS: usize = 32;
/// The most runs a bot may have live at once.
pub(super) const MAX_CONCURRENT_RUNS: u32 = 64;
/// The longest wall time one run may be given.
pub(super) const MAX_WALL_TIME: Duration = Duration::from_secs(24 * 3600);

/// A bot: the instructions it works from and the limits it works within.
///
/// ```toml
/// name = "keeper"
/// instructions = """
/// Keep dependencies current. Never push without a human.
/// """
/// executors = ["claude-code", "codex"]   # most preferred first
/// access = "supervised"
/// sandbox = "workspace_write"          # optional: only executors that enforce it
/// team = "core"                          # optional
/// triggers = ["nightly"]                 # names of triggers/<name>.toml
/// in_place_writes = false                # optional: allow writes in a non-git directory
/// secrets = ["GITHUB_TOKEN"]             # optional: Keychain secrets its runs get as env vars
/// resume = "auto"                        # optional: "auto" (default) or "never"
/// model = "claude-sonnet-4-6"            # optional: the executor's default when left out
/// inherit_user_config = false            # optional: true loads the owner's personal config
///
/// [budget]
/// max_concurrent_runs = 2
/// max_wall_time = "45m"
/// daily_token_ceiling = 2000000          # optional
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BotDef {
    /// The bot's name; it must equal the file stem.
    pub name: String,
    /// What the bot is for and how it works. Sent with every run.
    pub instructions: String,
    /// Executors in order of preference. At least one.
    pub executors: Vec<ExecutorId>,
    /// How much a run may do without asking. Defaults to the strictest, `supervised`.
    #[serde(default = "default_access")]
    pub access: AccessMode,
    /// The sandbox ceiling a run asks its harness to enforce. Defaults to `unrestricted`: no
    /// ceiling beyond `access`, so every listed executor is eligible. `workspace_write` or
    /// `read_only` makes only executors that enforce that level eligible.
    #[serde(default = "default_sandbox")]
    pub sandbox: SandboxLevel,
    /// The team the bot belongs to, if any. Teams arrive with T2.4, so only the name's shape is
    /// checked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    /// Names of the triggers that start this bot. Each must exist and must target this bot.
    #[serde(default)]
    pub triggers: Vec<String>,
    /// The limits on this bot's runs.
    #[serde(default)]
    pub budget: Budget,
    /// Let this bot's write runs work directly in the directory they target when it is not a git
    /// repository. Off by default: a write run on a non-git directory is refused, because there
    /// is no worktree to isolate it in. A git repository is never written in place.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub in_place_writes: bool,
    /// Names of the secrets this bot's runs receive, each as an environment variable of the same
    /// name (`GITHUB_TOKEN`). The values live in the Keychain (`reno secret set`), never in this
    /// file. A run whose secret is not set does not start. Left out, the run gets none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<String>,
    /// Whether a run of this bot that was interrupted (the daemon crashed or was stopped) is
    /// resumed by the daemon on its own at the next start. `auto`, the default, resumes it when
    /// its executor supports resume, the run has a session, it is under its attempt cap and not
    /// past its wall-time budget; `never` leaves it `interrupted` for a person to resume with
    /// `run.resume`. A person's cancel and a budget stop are never resumed on their own.
    #[serde(default, skip_serializing_if = "ResumePolicy::is_auto")]
    pub resume: ResumePolicy,
    /// Let this bot's runs load the owner's personal configuration for their harness. Off by
    /// default: a run is isolated from it, so the owner's own hooks, plugins, memory and
    /// instructions do not reach the bot. Only executors that declare
    /// `isolates_user_config` (Claude Code, Codex) can isolate; the others load it either way,
    /// and say so on their descriptor. Reno's own MCP server and its policy apply in both modes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub inherit_user_config: bool,
    /// The model this bot's runs ask for, as its executor names models (`claude-sonnet-4-6`).
    /// Left out, the executor's own default applies. A task that names a model wins over it. The
    /// name is checked against the executor's model list only as a warning, since a list can be
    /// curated and incomplete; a model the executor does not know fails when the run starts.
    /// It is passed to whichever executor the run goes to, so a bot that lists executors of
    /// different vendors should leave it out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Reasoning effort for the bot's runs: `minimal`, `low`, `medium`, `high`, `xhigh` or `max`,
    /// one the run's model takes. Absent: the model's own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<crate::Effort>,
}

/// The longest `model`, in bytes.
pub(super) const MAX_MODEL_BYTES: usize = 200;

/// What a bot allows the daemon to do with its interrupted runs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ResumePolicy {
    /// Resume an interrupted run on its own when it can be.
    #[default]
    Auto,
    /// Leave an interrupted run for a person.
    Never,
}

impl ResumePolicy {
    /// Whether this is the default, so a bot that never mentions `resume` hashes as before.
    pub fn is_auto(&self) -> bool {
        *self == Self::Auto
    }
}

fn default_access() -> AccessMode {
    AccessMode::Supervised
}

fn default_sandbox() -> SandboxLevel {
    SandboxLevel::Unrestricted
}

/// The limits on one bot's runs (ARCHITECTURE section 8, item 6). Enforcement is T1.7.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    /// Runs of this bot live at once. At least 1.
    #[serde(default = "default_concurrent")]
    pub max_concurrent_runs: u32,
    /// The longest one run may take, as a duration such as `45m`.
    #[serde(default = "default_wall_time")]
    pub max_wall_time: String,
    /// Tokens the bot may spend per day; no ceiling when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_token_ceiling: Option<u64>,
}

fn default_concurrent() -> u32 {
    1
}

fn default_wall_time() -> String {
    "45m".to_owned()
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            max_concurrent_runs: default_concurrent(),
            max_wall_time: default_wall_time(),
            daily_token_ceiling: None,
        }
    }
}

impl BotDef {
    pub(super) fn validate(&self, stem: &str, cx: &Cx<'_>, sink: &mut Sink<'_>) {
        check_name(sink, &self.name, stem);

        if self.instructions.trim().is_empty() {
            sink.err("instructions", "must not be empty");
        } else if self.instructions.len() > MAX_INSTRUCTIONS_BYTES {
            sink.err(
                "instructions",
                format!(
                    "is {} bytes; the limit is {MAX_INSTRUCTIONS_BYTES}",
                    self.instructions.len()
                ),
            );
        }

        self.validate_executors(cx, sink);

        if let Some(team) = &self.team {
            check_ref_name(sink, "team", team);
        }

        self.validate_triggers(stem, cx, sink);
        self.validate_secrets(sink);

        if let Some(model) = &self.model
            && (model.trim().is_empty()
                || model.len() > MAX_MODEL_BYTES
                || model.chars().any(|c| c.is_whitespace() || c.is_control()))
        {
            sink.err(
                "model",
                format!(
                    "{model:?} is not a model name: it must be 1..={MAX_MODEL_BYTES} bytes \
                     with no spaces; leave it out for the executor's default"
                ),
            );
        }

        let budget = &self.budget;
        if !(1..=MAX_CONCURRENT_RUNS).contains(&budget.max_concurrent_runs) {
            sink.err(
                "budget.max_concurrent_runs",
                format!(
                    "{} is outside 1..={MAX_CONCURRENT_RUNS}",
                    budget.max_concurrent_runs
                ),
            );
        }
        if let Err(message) = check_duration(
            &budget.max_wall_time,
            Duration::from_secs(1),
            MAX_WALL_TIME,
            "the wall time",
        ) {
            sink.err("budget.max_wall_time", message);
        }
        if budget.daily_token_ceiling == Some(0) {
            sink.err(
                "budget.daily_token_ceiling",
                "must be at least 1; leave it out for no ceiling",
            );
        }
    }

    fn validate_executors(&self, cx: &Cx<'_>, sink: &mut Sink<'_>) {
        if self.executors.is_empty() {
            sink.err("executors", "must list at least one executor");
        }
        if self.executors.len() > MAX_EXECUTORS {
            sink.err(
                "executors",
                format!(
                    "lists {} executors; the limit is {MAX_EXECUTORS}",
                    self.executors.len()
                ),
            );
        }
        let mut seen = BTreeSet::new();
        for (i, executor) in self.executors.iter().enumerate() {
            let path = format!("executors[{i}]");
            if !seen.insert(executor) {
                sink.err(path, format!("duplicate executor {:?}", executor.as_str()));
            } else if !cx.ctx.knows(executor) {
                let said = format!("unknown executor {:?}", executor.as_str());
                if cx.ctx.was_removed(executor) {
                    sink.err(
                        path,
                        format!(
                            "{said}: the agent was removed from this device; add it again to make this bot valid"
                        ),
                    );
                } else {
                    sink.err(path, said);
                }
            }
        }
    }

    fn validate_secrets(&self, sink: &mut Sink<'_>) {
        if self.secrets.len() > MAX_BOT_SECRETS {
            sink.err(
                "secrets",
                format!(
                    "lists {} secrets; the limit is {MAX_BOT_SECRETS}",
                    self.secrets.len()
                ),
            );
        }
        let mut seen = BTreeSet::new();
        for (i, name) in self.secrets.iter().enumerate() {
            let path = format!("secrets[{i}]");
            if let Err(error) = check_secret_name(name) {
                sink.err(path, format!("{name:?}: {error}"));
            } else if !seen.insert(name.as_str()) {
                sink.err(path, format!("duplicate secret {name:?}"));
            }
        }
    }

    fn validate_triggers(&self, stem: &str, cx: &Cx<'_>, sink: &mut Sink<'_>) {
        if self.triggers.len() > MAX_BOT_TRIGGERS {
            sink.err(
                "triggers",
                format!(
                    "lists {} triggers; the limit is {MAX_BOT_TRIGGERS}",
                    self.triggers.len()
                ),
            );
        }
        let mut seen = BTreeSet::new();
        for (i, name) in self.triggers.iter().enumerate() {
            let path = format!("triggers[{i}]");
            if !check_ref_name(sink, &path, name) {
                continue;
            }
            if !seen.insert(name.as_str()) {
                sink.err(path, format!("duplicate trigger {name:?}"));
            } else if !cx.trigger_exists(name) {
                sink.err(path, format!("unknown trigger {name:?}"));
            } else if let Some(trigger) = cx.triggers.get(name)
                && trigger.target.bot != stem
            {
                sink.err(
                    path,
                    format!(
                        "trigger {name:?} starts bot {:?}, not this one",
                        trigger.target.bot
                    ),
                );
            }
        }
    }
}
