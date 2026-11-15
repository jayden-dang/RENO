//! `triggers/<name>.toml`: something that starts a bot (ARCHITECTURE section 7).
//!
//! This version loads, validates and versions triggers. Firing them is T1.8.

use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::time::{
    MAX_TIMER, MIN_EVERY, check_cron, check_duration, check_instant, check_timezone,
};
use super::{Cx, Sink, check_name, check_ref_name};

/// The longest prompt template, in bytes.
pub(super) const MAX_PROMPT_BYTES: usize = 16 * 1024;
/// The most paths one `fs` trigger may watch.
pub(super) const MAX_FS_PATHS: usize = 64;
/// The longest a glob may be, in bytes.
pub(super) const MAX_GLOB_BYTES: usize = 512;
/// The shortest `fs` debounce.
const MIN_DEBOUNCE: Duration = Duration::from_millis(50);
/// The longest `fs` debounce.
const MAX_DEBOUNCE: Duration = Duration::from_secs(3600);

/// A trigger: when to start, and whom to start.
///
/// ```toml
/// name = "nightly"
/// enabled = true                  # optional, defaults to true
///
/// [when]
/// kind = "cron"
/// cron = "0 2 * * *"
/// timezone = "Asia/Ho_Chi_Minh"
///
/// [target]
/// bot = "keeper"
/// prompt = "Check for outdated dependencies."
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TriggerDef {
    /// The trigger's name; it must equal the file stem.
    pub name: String,
    /// A disabled trigger is kept and versioned but never fires.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// What fires it.
    pub when: TriggerKind,
    /// What it starts.
    pub target: TriggerTarget,
}

fn default_enabled() -> bool {
    true
}

/// What a trigger starts: a bot, given a prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TriggerTarget {
    /// The bot to start. It must exist.
    pub bot: String,
    /// The prompt template for the task. Placeholders are defined with firing, in T1.8; for now
    /// the text is only checked to be present and bounded.
    pub prompt: String,
}

/// What fires a trigger. The `kind` key names the variant, and the other keys belong to it.
///
/// `at`, `after`, `every`, `cron`, `fs` and `manual` can be used today. The rest are reserved so
/// the wire type is complete, and a definition that uses one is rejected until its task lands:
/// `board`, `message`, `flow` and `git` with T2.10, `webhook` in Phase 3.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TriggerKind {
    /// Once, at an instant: RFC 3339 with an offset, such as `2026-12-01T09:00:00+07:00`.
    At {
        /// The instant.
        at: String,
    },
    /// Once, a delay after the definition is loaded: a duration such as `30m`.
    After {
        /// The delay.
        after: String,
    },
    /// Repeatedly, at an interval of at least a minute, such as `6h`.
    Every {
        /// The interval.
        every: String,
    },
    /// On a schedule, in an IANA time zone.
    Cron {
        /// Five fields: minute hour day-of-month month day-of-week.
        cron: String,
        /// An IANA zone name, such as `Asia/Ho_Chi_Minh` or `UTC`.
        timezone: String,
    },
    /// When files change, after the changes settle.
    Fs {
        /// Paths or globs to watch: absolute, or starting with `~/`.
        paths: Vec<String>,
        /// How long to wait for quiet before firing, such as `500ms`. Defaults to `500ms`.
        #[serde(default = "default_debounce")]
        debounce: String,
    },
    /// Only when started by hand: the CLI, the app or an RPC call.
    Manual {},
    /// Reserved: an entry matching a scope and key pattern is written to a board.
    Board {
        /// The board scope.
        scope: String,
        /// The key pattern.
        key: String,
    },
    /// Reserved: an envelope arrives for the bot while it has no live run.
    Message {},
    /// Reserved: a ref moves in a registered repository.
    Git {
        /// The repository.
        repo: String,
        /// The refs to watch.
        refs: Vec<String>,
    },
    /// Reserved: another flow run reaches a terminal state.
    Flow {
        /// The flow to watch.
        flow: String,
    },
    /// Reserved: a loopback HTTP POST with an HMAC signature. Phase 3, off by default.
    Webhook {
        /// The URL path.
        path: String,
    },
}

fn default_debounce() -> String {
    "500ms".to_owned()
}

impl TriggerKind {
    /// The `kind` value of this variant.
    pub fn name(&self) -> &'static str {
        match self {
            Self::At { .. } => "at",
            Self::After { .. } => "after",
            Self::Every { .. } => "every",
            Self::Cron { .. } => "cron",
            Self::Fs { .. } => "fs",
            Self::Manual {} => "manual",
            Self::Board { .. } => "board",
            Self::Message {} => "message",
            Self::Git { .. } => "git",
            Self::Flow { .. } => "flow",
            Self::Webhook { .. } => "webhook",
        }
    }

    fn validate(&self, sink: &mut Sink<'_>) {
        match self {
            Self::At { at } => {
                if let Err(message) = check_instant(at) {
                    sink.err("when.at", message);
                }
            }
            Self::After { after } => {
                if let Err(message) =
                    check_duration(after, Duration::from_secs(1), MAX_TIMER, "the delay")
                {
                    sink.err("when.after", message);
                }
            }
            Self::Every { every } => {
                if let Err(message) = check_duration(every, MIN_EVERY, MAX_TIMER, "the interval") {
                    sink.err("when.every", message);
                }
            }
            Self::Cron { cron, timezone } => {
                if let Err(message) = check_cron(cron) {
                    sink.err("when.cron", message);
                }
                if let Err(message) = check_timezone(timezone) {
                    sink.err("when.timezone", message);
                }
            }
            Self::Fs { paths, debounce } => validate_fs(paths, debounce, sink),
            Self::Manual {} => {}
            Self::Board { .. }
            | Self::Message {}
            | Self::Git { .. }
            | Self::Flow { .. }
            | Self::Webhook { .. } => sink.err(
                "when.kind",
                format!("kind {:?} is reserved and not available yet", self.name()),
            ),
        }
    }
}

fn validate_fs(paths: &[String], debounce: &str, sink: &mut Sink<'_>) {
    if paths.is_empty() {
        sink.err("when.paths", "must list at least one path");
    }
    if paths.len() > MAX_FS_PATHS {
        sink.err(
            "when.paths",
            format!("lists {} paths; the limit is {MAX_FS_PATHS}", paths.len()),
        );
    }
    for (i, path) in paths.iter().enumerate() {
        let at = format!("when.paths[{i}]");
        if !(path.starts_with('/') || path.starts_with("~/")) {
            sink.err(
                at,
                format!("{path:?} must be an absolute path or start with `~/`"),
            );
        } else if let Err(message) = check_glob(path) {
            sink.err(at, message);
        }
    }
    if let Err(message) = check_duration(debounce, MIN_DEBOUNCE, MAX_DEBOUNCE, "the debounce") {
        sink.err("when.debounce", message);
    }
}

/// Checks that `glob` is a well-formed glob of bounded size.
pub(super) fn check_glob(glob: &str) -> Result<(), String> {
    if glob.is_empty() {
        return Err("a glob must not be empty".to_owned());
    }
    if glob.len() > MAX_GLOB_BYTES {
        return Err(format!(
            "a glob is {} bytes; the limit is {MAX_GLOB_BYTES}",
            glob.len()
        ));
    }
    globset::Glob::new(glob)
        .map(|_| ())
        .map_err(|e| format!("{glob:?} is not a valid glob: {}", e.kind()))
}

impl TriggerDef {
    pub(super) fn validate(&self, stem: &str, cx: &Cx<'_>, sink: &mut Sink<'_>) {
        check_name(sink, &self.name, stem);
        self.when.validate(sink);

        if check_ref_name(sink, "target.bot", &self.target.bot) && !cx.bot_exists(&self.target.bot)
        {
            sink.err("target.bot", format!("unknown bot {:?}", self.target.bot));
        }
        if self.target.prompt.trim().is_empty() {
            sink.err("target.prompt", "must not be empty");
        } else if self.target.prompt.len() > MAX_PROMPT_BYTES {
            sink.err(
                "target.prompt",
                format!(
                    "is {} bytes; the limit is {MAX_PROMPT_BYTES}",
                    self.target.prompt.len()
                ),
            );
        }
    }
}
