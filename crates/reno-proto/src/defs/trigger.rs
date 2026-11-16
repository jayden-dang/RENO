//! `triggers/<name>.toml`: something that starts a bot (ARCHITECTURE section 7).
//!
//! A trigger is loaded, validated and versioned here; the kernel's scheduler fires it. This module
//! also owns the text of a firing: the prompt template and the note about a late run.

use std::path::{Path, PathBuf};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::time::{
    MAX_TIMER, MIN_EVERY, check_cron, check_duration, check_instant, check_timezone, format_span,
};
use super::{Cx, Sink, check_name, check_ref_name};

/// The longest prompt template, in bytes.
pub(super) const MAX_PROMPT_BYTES: usize = 16 * 1024;
/// The longest `target.cwd`, in bytes.
const MAX_CWD_BYTES: usize = 4096;
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
/// cwd = "~/code/app"              # optional: where the run works
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
    /// The prompt template for the task. `{{trigger}}` is the trigger's name,
    /// `{{scheduled_at}}` the occurrence it fires for (RFC 3339, in the trigger's time zone) and
    /// `{{lateness}}` how late the run started (`0s` when on time). Any other `{{...}}` is an
    /// error. A run that starts late or merges missed occurrences also gets a sentence saying
    /// so after the prompt.
    pub prompt: String,
    /// The directory the run works in: absolute, or starting with `~/` (the daemon user's home).
    /// Omitted, the run uses the daemon's own scratch workspace. A git repository here is not
    /// written to: a write run works in a worktree of it. The directory is checked when the
    /// trigger fires, not when the definition loads, so an unmounted disk does not make the
    /// definition invalid; a missing directory refuses the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

impl TriggerTarget {
    /// The working directory with `~/` expanded against `home`, or `None` when `cwd` is omitted.
    /// `Err` says why it cannot be used (`~/` with no home).
    pub fn resolved_cwd(&self, home: Option<&Path>) -> Option<Result<PathBuf, String>> {
        let cwd = self.cwd.as_deref()?;
        Some(match cwd.strip_prefix("~/") {
            None => Ok(PathBuf::from(cwd)),
            Some(rest) => home
                .filter(|home| home.is_absolute())
                .map(|home| home.join(rest))
                .ok_or_else(|| {
                    format!("{cwd:?} starts with `~/` but the home directory is unknown")
                }),
        })
    }
}

/// Checks a trigger's `cwd`: absolute or `~/...`, with no `..` part.
fn check_cwd(cwd: &str) -> Result<(), String> {
    let rest = match cwd.strip_prefix("~/") {
        Some(rest) => rest,
        None if cwd.starts_with('/') => cwd,
        None => {
            return Err(format!(
                "{cwd:?} must be an absolute path or start with `~/`"
            ));
        }
    };
    if cwd.len() > MAX_CWD_BYTES {
        return Err(format!(
            "is {} bytes; the limit is {MAX_CWD_BYTES}",
            cwd.len()
        ));
    }
    if cwd.contains('\0') || rest.split('/').any(|part| part == "..") {
        return Err(format!("{cwd:?} must not contain NUL or `..`"));
    }
    Ok(())
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
    /// Once, a delay after this version of the definition became active: a duration such as
    /// `30m`. Editing the definition starts the delay again, unless the trigger has fired.
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
        if let Some(cwd) = &self.target.cwd
            && let Err(message) = check_cwd(cwd)
        {
            sink.err("target.cwd", message);
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
        } else if let Err(message) = check_placeholders(&self.target.prompt) {
            sink.err("target.prompt", message);
        }
    }
}

// ---- the text of a firing -------------------------------------------------------------------

/// The placeholders a prompt template may use.
pub const PROMPT_PLACEHOLDERS: [&str; 3] = ["trigger", "scheduled_at", "lateness"];

/// A run that starts less late than this, with nothing missed, is on time: no note is added.
pub const LATE_GRACE: Duration = Duration::from_secs(60);

/// The most occurrences the scheduler counts when it merges missed ones. A count that reaches it
/// is shown as "at least".
pub const MAX_MISSED_COUNT: u32 = 10_000;

/// The pieces a prompt template is filled with.
#[derive(Debug, Clone, Copy)]
pub struct PromptVars<'a> {
    /// The trigger's name.
    pub trigger: &'a str,
    /// The occurrence being fired for, as text.
    pub scheduled_at: &'a str,
    /// How late the run started, as text such as `6h12m`.
    pub lateness: &'a str,
}

/// Splits `template` into literal text and `{{name}}` placeholders, calling `piece` for each.
/// `Err` is a `{{` that is never closed.
fn scan<'t>(template: &'t str, mut piece: impl FnMut(Piece<'t>)) -> Result<(), String> {
    let mut rest = template;
    while let Some(open) = rest.find("{{") {
        piece(Piece::Text(&rest[..open]));
        let after = &rest[open + 2..];
        let Some(close) = after.find("}}") else {
            return Err(format!(
                "has `{{{{` with no closing `}}}}` near {:?}",
                after.chars().take(20).collect::<String>()
            ));
        };
        piece(Piece::Placeholder {
            name: after[..close].trim(),
            raw: &rest[open..open + 2 + close + 2],
        });
        rest = &after[close + 2..];
    }
    piece(Piece::Text(rest));
    Ok(())
}

enum Piece<'t> {
    Text(&'t str),
    Placeholder { name: &'t str, raw: &'t str },
}

/// Checks that every `{{...}}` in `template` is one of [`PROMPT_PLACEHOLDERS`] and is closed.
pub(super) fn check_placeholders(template: &str) -> Result<(), String> {
    let mut unknown = None;
    scan(template, |piece| {
        if let Piece::Placeholder { name, raw } = piece
            && unknown.is_none()
            && !PROMPT_PLACEHOLDERS.contains(&name)
        {
            unknown = Some(raw.to_owned());
        }
    })?;
    match unknown {
        Some(raw) => Err(format!(
            "has the placeholder {raw}; the known ones are {}",
            PROMPT_PLACEHOLDERS
                .map(|p| format!("{{{{{p}}}}}"))
                .join(", ")
        )),
        None => Ok(()),
    }
}

/// Fills `template` with `vars`. A placeholder that is not known (validation refuses them) is
/// left as written.
pub fn render_prompt(template: &str, vars: &PromptVars<'_>) -> String {
    let mut out = String::with_capacity(template.len());
    let scanned = scan(template, |piece| match piece {
        Piece::Text(text) => out.push_str(text),
        Piece::Placeholder { name, raw } => out.push_str(match name {
            "trigger" => vars.trigger,
            "scheduled_at" => vars.scheduled_at,
            "lateness" => vars.lateness,
            _ => raw,
        }),
    });
    if scanned.is_err() {
        // An unclosed `{{`: the scan stopped there; the rest is plain text.
        return template.to_owned();
    }
    out
}

/// The sentence added after a prompt whose run is late or stands for several occurrences, or
/// `None` when the run is on time and alone.
///
/// `missed` is how many occurrences were due when the run started, this one included (at least
/// 1); `lateness` is the age of the earliest of them. A `missed` of [`MAX_MISSED_COUNT`] is a
/// lower bound.
pub fn lateness_note(
    scheduled_at: &str,
    lateness: Duration,
    missed: u32,
    trigger_name: &str,
) -> Option<String> {
    if lateness < LATE_GRACE && missed <= 1 {
        return None;
    }
    let late = format_span(lateness);
    let mut note = format!(
        "This run (trigger {trigger_name}) was scheduled for {scheduled_at} and started {late} late"
    );
    if missed > 1 {
        let count = if missed >= MAX_MISSED_COUNT {
            format!("at least {MAX_MISSED_COUNT}")
        } else {
            missed.to_string()
        };
        note.push_str(&format!(
            "; {count} occurrences were missed and merged into this one"
        ));
    }
    note.push('.');
    Some(note)
}

#[cfg(test)]
mod tests {
    use super::*;

    const VARS: PromptVars<'static> = PromptVars {
        trigger: "nightly",
        scheduled_at: "2026-12-01T02:00:00+07:00",
        lateness: "6h12m",
    };

    #[test]
    fn placeholders_are_filled() {
        assert_eq!(
            render_prompt(
                "{{trigger}} at {{ scheduled_at }}, {{lateness}} late; {{trigger}}",
                &VARS
            ),
            "nightly at 2026-12-01T02:00:00+07:00, 6h12m late; nightly"
        );
        assert_eq!(
            render_prompt("no placeholders { } {x}", &VARS),
            "no placeholders { } {x}"
        );
    }

    #[test]
    fn unknown_and_unclosed_placeholders_are_refused() {
        assert!(check_placeholders("{{trigger}} and {{lateness}}").is_ok());
        let unknown = check_placeholders("hello {{ name }}").unwrap_err();
        assert!(
            unknown.contains("{{ name }}") && unknown.contains("{{scheduled_at}}"),
            "{unknown}"
        );
        let unclosed = check_placeholders("hello {{trigger").unwrap_err();
        assert!(unclosed.contains("no closing"), "{unclosed}");
        // Left alone when rendered, never half-filled.
        assert_eq!(render_prompt("a {{trigger", &VARS), "a {{trigger");
    }

    #[test]
    fn the_note_says_how_late_and_how_many() {
        let on_time = lateness_note("t", Duration::from_secs(2), 1, "n");
        assert_eq!(on_time, None);
        assert_eq!(
            lateness_note(
                "2026-12-01T02:00:00+07:00",
                Duration::from_secs(6 * 3600 + 12 * 60),
                1,
                "nightly"
            )
            .unwrap(),
            "This run (trigger nightly) was scheduled for 2026-12-01T02:00:00+07:00 and started 6h12m late."
        );
        assert_eq!(
            lateness_note(
                "02:00",
                Duration::from_secs(6 * 3600 + 12 * 60),
                3,
                "nightly"
            )
            .unwrap(),
            "This run (trigger nightly) was scheduled for 02:00 and started 6h12m late; \
             3 occurrences were missed and merged into this one."
        );
        // Several merged occurrences are noted even when the run started at once.
        assert!(
            lateness_note("02:00", Duration::ZERO, 2, "n")
                .unwrap()
                .contains("2 occurrences")
        );
        assert!(
            lateness_note("02:00", Duration::from_secs(1), MAX_MISSED_COUNT, "n")
                .unwrap()
                .contains("at least 10000 occurrences")
        );
    }
}
