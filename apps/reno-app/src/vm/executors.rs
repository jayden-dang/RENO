//! The Executors list: one row per executor with what the daemon's last probe found, grouped by
//! what a person can do about it (needs you, ready, unknown, not installed, test executors). From
//! `executors.watch`.
//!
//! Every fact the probe could not establish stays unknown on screen: "Sign-in not checkable" is
//! never shown as signed in, an untested version is never shown as fine. Those facts live in the
//! expanded line only; the one-line row says only what a person acts on.
//!
//! Pure: callers pass `now_ms`, so the job clock and the "still running" notes are plain tests.

use reno_client::{Feed, State};
use reno_proto::{
    ExecutorHealth, ExecutorInfo, ExecutorJob, InstallStatus, JobFailure, JobKind, JobPhase,
    Maintenance, NotInstalledReason, RunId, RunStatus, SignedIn, StartRefusal, StepRunner,
    StopDetail, VersionGate, VersionRange, version_gate,
};

use super::{Banner, Section, banner};
use crate::summary;

/// A job that has run this long gets its clock (a spinner alone reads as stuck).
const CLOCK_AFTER_MS: u64 = 1_000;
/// A job that has run this long says it is still running and that installers take a while.
const STILL_RUNNING_AFTER_MS: u64 = 30_000;

/// How a fact reads: the colour of its dot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
    Good,
    /// Works, with a caveat.
    Warn,
    /// Blocks a run.
    Bad,
    /// Not known, or not applicable.
    Quiet,
}

/// One line of an executor's health: what it is and the detail under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Fact {
    pub(crate) level: Level,
    pub(crate) text: &'static str,
    pub(crate) detail: Option<String>,
}

impl Fact {
    fn new(level: Level, text: &'static str, detail: impl Into<Option<String>>) -> Self {
        Self {
            level,
            text,
            detail: detail.into(),
        }
    }
}

/// What the human has to do, most pressing first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Need {
    SignIn,
    Update,
    /// The daemon can fetch it: the Install button.
    Install,
    /// Not there and not the daemon's to fetch (not on the path, or unusable).
    Setup,
}

impl Need {
    /// What is wrong, for the headline's sub-line: "Codex is signed out".
    const fn phrase(self) -> &'static str {
        match self {
            Self::SignIn => "is signed out",
            Self::Update => "is too old",
            Self::Install => "needs an install",
            Self::Setup => "cannot be used",
        }
    }
}

/// Where an executor is listed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Group {
    /// Installed or managed, but blocked.
    NeedsYou,
    Ready,
    /// The probe failed or nothing has been checked yet.
    Unknown,
    NotInstalled,
    /// `mock`: collapsed.
    Test,
}

impl Group {
    pub(crate) const ALL: [Group; 5] = [
        Group::NeedsYou,
        Group::Ready,
        Group::Unknown,
        Group::NotInstalled,
        Group::Test,
    ];

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::NeedsYou => "Needs you",
            Self::Ready => "Ready",
            Self::Unknown => "Unknown",
            Self::NotInstalled => "Not installed",
            Self::Test => "Test executors",
        }
    }

    /// The small note after the label.
    pub(crate) const fn note(self) -> Option<&'static str> {
        match self {
            Self::NotInstalled => Some("optional"),
            _ => None,
        }
    }
}

/// What the row's dot means: only this.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Dot {
    /// Can take runs.
    Ready,
    /// Blocked.
    Blocked,
    /// Not installed.
    NotInstalled,
    /// A job is running.
    Running,
    /// Not known.
    Unknown,
}

/// How a first install is offered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InstallOffer {
    /// [Install…]: the review panel first, because it runs a vendor script as you.
    Review,
    /// [Install]: at once (a managed adapter, or the daemon's own fetch).
    Now,
    /// `executor.install`, from a daemon without `executor.maintain`.
    Legacy,
}

/// One step of an install recipe, for the review panel and the expanded line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StepView {
    pub(crate) label: String,
    pub(crate) command: String,
    /// Reno runs it; otherwise it needs the person, in Terminal.
    pub(crate) reno: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RecipeView {
    pub(crate) source: String,
    pub(crate) steps: Vec<StepView>,
}

impl RecipeView {
    /// How many commands Reno runs.
    pub(crate) fn reno_steps(&self) -> usize {
        self.steps.iter().filter(|s| s.reno).count()
    }
}

/// The Auto-update switch of a line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SwitchView {
    pub(crate) on: bool,
    /// The accessible label: "Auto-update Codex".
    pub(crate) label: String,
}

/// The Auto-update part of an expanded line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AutoView {
    pub(crate) on: bool,
    pub(crate) verified_only: bool,
    /// The updater can install a given version: offer "Only versions Reno has verified".
    pub(crate) can_pin: bool,
    /// What on or off means for this executor.
    pub(crate) text: String,
    /// Said before it happens: what the setting does to the version now installed.
    pub(crate) warning: Option<String>,
}

/// Where a job is, in the words a person reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum JobState {
    Queued {
        waiting_runs: u32,
    },
    Running {
        step: u32,
        steps: u32,
        /// "0:07", once past a second.
        clock: Option<String>,
        last_line: Option<String>,
        /// "still running — installers can take a few minutes", after 30 s.
        long: bool,
        /// "no output for 5 min": how long it has been quiet, in minutes.
        quiet_minutes: Option<u64>,
    },
    Failed {
        /// "Install failed", "Update failed" or "Stopped".
        title: &'static str,
        detail: String,
        /// A person stopped it: not an error.
        stopped: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct JobView {
    pub(crate) kind: JobKind,
    pub(crate) state: JobState,
    /// What [Run in Terminal] runs. `None` for a managed adapter, which has no commands.
    pub(crate) terminal_command: Option<String>,
}

impl JobView {
    pub(crate) fn running(&self) -> bool {
        matches!(self.state, JobState::Running { .. })
    }

    pub(crate) fn failed(&self) -> bool {
        matches!(self.state, JobState::Failed { .. })
    }

    /// The status line: announced, so the state is never colour alone.
    pub(crate) fn words(&self, name: &str) -> String {
        let verb = match self.kind {
            JobKind::Install => "Installing",
            JobKind::Update => "Updating",
        };
        match &self.state {
            JobState::Queued { waiting_runs } => format!(
                "Update queued \u{2014} waits for {waiting_runs} running {} on {name}",
                if *waiting_runs == 1 { "task" } else { "tasks" }
            ),
            JobState::Running { step, steps, .. } if *steps > 1 => {
                format!("{verb} \u{b7} step {step} of {steps}")
            }
            JobState::Running { .. } => verb.to_owned(),
            JobState::Failed { title, .. } => (*title).to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExecutorRow {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) need: Option<Need>,
    pub(crate) group: Group,
    pub(crate) dot: Dot,
    /// `None` until the first probe has finished.
    pub(crate) installed: Fact,
    /// `None` when there is nothing installed to have a version.
    pub(crate) version: Option<Fact>,
    /// `None` when there is nothing installed to sign in to.
    pub(crate) signed_in: Option<Fact>,
    /// "checked 3m ago" / "not checked yet".
    pub(crate) checked: String,
    /// The Install button of a daemon with `executor.install` but not `executor.maintain`.
    pub(crate) can_install: bool,
    /// The daemon runs installs, updates and the switch (`executor.maintain` and a record).
    pub(crate) maintain: bool,
    /// A Reno-managed adapter: it updates with Reno.
    pub(crate) managed: bool,
    /// The installed version, as the probe read it.
    pub(crate) version_text: Option<String>,
    /// A newer version than the installed one, when a check found one.
    pub(crate) available: Option<String>,
    /// The account of a signed-in executor; only when the probe says signed in.
    pub(crate) account: Option<String>,
    /// What is wrong or not known, for the middle of a line that is not ready.
    pub(crate) middle: Option<Fact>,
    /// Where a not-installed executor comes from: "Homebrew cask devin-cli".
    pub(crate) source: Option<String>,
    pub(crate) recipe: Option<RecipeView>,
    /// How a first install is offered.
    pub(crate) install: Option<InstallOffer>,
    /// The sign-in command, offered while signed out.
    pub(crate) sign_in: Option<String>,
    /// [Update] is on the line.
    pub(crate) update: bool,
    /// [Update] is the primary button: the executor is blocked as too old.
    pub(crate) update_primary: bool,
    pub(crate) switch: Option<SwitchView>,
    /// Lock icon and "Updates with Reno".
    pub(crate) lock: bool,
    pub(crate) auto: Option<AutoView>,
    pub(crate) job: Option<JobView>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExecutorsView {
    pub(crate) banner: Option<Banner>,
    /// In group order, then by name.
    pub(crate) rows: Section<ExecutorRow>,
    pub(crate) lede: String,
    pub(crate) detail: String,
    /// How many need the human.
    pub(crate) needing: usize,
    /// How many can take runs.
    pub(crate) ready: usize,
    /// Whether the daemon has `executor.install`.
    pub(crate) installs: bool,
    /// Whether the daemon runs installs and updates (`executor.maintain`).
    pub(crate) maintain: bool,
    /// "checked 12m ago": the oldest probe.
    pub(crate) checked: Option<String>,
    /// How many jobs are running.
    pub(crate) jobs_running: usize,
    /// Executors with a known update, auto-update off and no job: "Update N" when more than one.
    pub(crate) updates_waiting: Vec<String>,
}

impl ExecutorsView {
    /// The rows of one group, in order.
    pub(crate) fn group(&self, group: Group) -> Vec<&ExecutorRow> {
        self.rows
            .rows()
            .iter()
            .filter(|r| r.group == group)
            .collect()
    }
}

fn installed_fact(health: Option<&ExecutorHealth>) -> Fact {
    let Some(health) = health else {
        return Fact::new(Level::Quiet, "Not checked yet", None);
    };
    match &health.installed {
        InstallStatus::Installed { path, version } => Fact::new(
            Level::Good,
            "Installed",
            Some(match version {
                Some(version) => format!("{} \u{b7} {version}", path.display()),
                None => format!("{} \u{b7} version unknown", path.display()),
            }),
        ),
        InstallStatus::NotInstalled { why } => match why {
            NotInstalledReason::NotFound { hint } => {
                Fact::new(Level::Bad, "Not installed", Some(hint.clone()))
            }
            NotInstalledReason::NeedsInstall { what } => Fact::new(
                Level::Bad,
                "Needs install",
                Some(format!("{what} is fetched by Install, never by a run")),
            ),
            NotInstalledReason::Unusable { detail } => {
                Fact::new(Level::Bad, "Cannot be launched", Some(detail.clone()))
            }
            NotInstalledReason::ProbeFailed { detail } => {
                Fact::new(Level::Quiet, "Could not be checked", Some(detail.clone()))
            }
        },
    }
}

fn version_fact(health: &ExecutorHealth) -> Option<Fact> {
    let InstallStatus::Installed { .. } = &health.installed else {
        return None;
    };
    Some(match &health.version_gate {
        VersionGate::Ok => Fact::new(Level::Good, "Version supported", None),
        VersionGate::TooOld { min } => Fact::new(
            Level::Bad,
            "Version too old",
            Some(format!("needs {min} or newer")),
        ),
        VersionGate::Untested { verified } => Fact::new(
            Level::Warn,
            "Version untested",
            Some(if verified == "unverified" {
                "no version of this agent is verified yet".to_owned()
            } else {
                format!("verified up to {verified}")
            }),
        ),
        VersionGate::Unchecked => Fact::new(
            Level::Quiet,
            "Version not checked",
            Some("the CLI did not report one".to_owned()),
        ),
    })
}

fn signed_in_fact(health: &ExecutorHealth) -> Option<Fact> {
    let InstallStatus::Installed { .. } = &health.installed else {
        return None;
    };
    Some(match &health.signed_in {
        SignedIn::Yes { account } => Fact::new(Level::Good, "Signed in", account.clone()),
        SignedIn::No { hint } => Fact::new(Level::Bad, "Signed out", Some(hint.clone())),
        SignedIn::Unknown => Fact::new(
            Level::Quiet,
            "Sign-in not checkable",
            Some("only a run would find out".to_owned()),
        ),
    })
}

/// What the human has to do for this executor, if anything.
fn need_of(health: Option<&ExecutorHealth>) -> Option<Need> {
    let health = health?;
    match &health.installed {
        InstallStatus::NotInstalled {
            why: NotInstalledReason::NeedsInstall { .. },
        } => Some(Need::Install),
        InstallStatus::NotInstalled {
            why: NotInstalledReason::NotFound { .. } | NotInstalledReason::Unusable { .. },
        } => Some(Need::Setup),
        // A probe that could not finish says nothing about the harness.
        InstallStatus::NotInstalled {
            why: NotInstalledReason::ProbeFailed { .. },
        } => None,
        InstallStatus::Installed { .. } => {
            if matches!(health.signed_in, SignedIn::No { .. }) {
                Some(Need::SignIn)
            } else if matches!(health.version_gate, VersionGate::TooOld { .. }) {
                Some(Need::Update)
            } else {
                None
            }
        }
    }
}

/// Whether `latest` is a newer version than `installed`. Versions that do not parse are never
/// newer: "an update is available" is said only when it is known.
fn is_newer(latest: &str, installed: &str) -> bool {
    let range = VersionRange {
        min: latest.to_owned(),
        verified: latest.to_owned(),
    };
    matches!(
        version_gate(Some(installed), &range),
        VersionGate::TooOld { .. }
    )
}

/// What the daemon and the list know, passed to every row.
#[derive(Clone, Copy)]
struct Ctx {
    installs: bool,
    maintain: bool,
    now_ms: u64,
}

fn group_of(
    info: &ExecutorInfo,
    need: Option<Need>,
    ctx: Ctx,
    has_recipe: bool,
    managed: bool,
) -> Group {
    if info.descriptor.id.as_str() == "mock" {
        return Group::Test;
    }
    let Some(health) = info.health.as_ref() else {
        return Group::Unknown;
    };
    match &health.installed {
        InstallStatus::NotInstalled {
            why: NotInstalledReason::ProbeFailed { .. },
        } => Group::Unknown,
        InstallStatus::NotInstalled {
            why: NotInstalledReason::NotFound { .. },
        } => Group::NotInstalled,
        // The daemon's own fetch of a managed adapter is a need; a recipe is optional.
        InstallStatus::NotInstalled {
            why: NotInstalledReason::NeedsInstall { .. },
        } if ctx.maintain && has_recipe && !managed => Group::NotInstalled,
        _ if need.is_some() => Group::NeedsYou,
        _ => Group::Ready,
    }
}

fn recipe_of(maintenance: Option<&Maintenance>) -> Option<RecipeView> {
    let recipe = maintenance?.setup.as_ref()?;
    Some(RecipeView {
        source: recipe.source.clone(),
        steps: recipe
            .steps
            .iter()
            .map(|s| StepView {
                label: s.label.clone(),
                command: s.command.clone(),
                reno: s.runner == StepRunner::Reno,
            })
            .collect(),
    })
}

fn auto_view(
    name: &str,
    maintenance: &Maintenance,
    version: Option<&str>,
    gate: Option<&VersionGate>,
) -> Option<AutoView> {
    let updater = maintenance.updater.as_ref()?;
    let auto = maintenance.auto_update;
    let stays = match version {
        Some(v) => format!("it stays on {v}"),
        None => "it stays on its current version".to_owned(),
    };
    let mut text = match (auto.on, updater.self_updating) {
        (true, true) => format!("{name} updates itself; Reno leaves that on."),
        (true, false) => format!(
            "Reno runs `{}` once a day, only when {name} has no runs.",
            updater.command
        ),
        (false, true) => {
            format!("Reno turns off {name}'s own updater; {stays} until you press Update.")
        }
        (false, false) => match version {
            Some(v) => format!("{name} stays on {v}."),
            None => format!("{name} stays on its current version."),
        },
    };
    if !auto.on && !updater.self_updating && updater.checked_at_ms.is_some() {
        text.push_str(" Reno still checks daily.");
    }
    // Said before it happens: with "verified only" set, a CLI past the verified version goes
    // back to it.
    let warning = match gate {
        Some(VersionGate::Untested { verified })
            if auto.verified_only && updater.can_pin && verified != "unverified" =>
        {
            Some(format!(
                "{name} is past the verified version and goes back to {verified}."
            ))
        }
        _ => None,
    };
    Some(AutoView {
        on: auto.on,
        verified_only: auto.verified_only,
        can_pin: updater.can_pin,
        text,
        warning,
    })
}

fn clock(ms: u64) -> String {
    let secs = ms / 1_000;
    format!("{}:{:02}", secs / 60, secs % 60)
}

fn job_view(job: &ExecutorJob, now_ms: u64, maintenance: Option<&Maintenance>) -> JobView {
    let managed = maintenance.is_some_and(|m| m.managed);
    let terminal_command = if managed {
        None
    } else {
        match job.kind {
            JobKind::Install => recipe_of(maintenance).map(|r| {
                r.steps
                    .iter()
                    .filter(|s| s.reno)
                    .map(|s| s.command.as_str())
                    .collect::<Vec<_>>()
                    .join(" && ")
            }),
            JobKind::Update => maintenance
                .and_then(|m| m.updater.as_ref())
                .map(|u| u.command.clone()),
        }
        .filter(|c| !c.is_empty())
    };
    let state = match &job.phase {
        JobPhase::Queued { waiting_runs } => JobState::Queued {
            waiting_runs: *waiting_runs,
        },
        JobPhase::Running {
            step,
            steps,
            last_line,
            quiet_since_ms,
        } => {
            let elapsed = now_ms.saturating_sub(job.started_at_ms);
            JobState::Running {
                step: *step,
                steps: *steps,
                clock: (elapsed >= CLOCK_AFTER_MS).then(|| clock(elapsed)),
                last_line: last_line.clone(),
                long: elapsed >= STILL_RUNNING_AFTER_MS,
                quiet_minutes: quiet_since_ms
                    .map(|since| (now_ms.saturating_sub(since) / 60_000).max(1)),
            }
        }
        JobPhase::Failed {
            failure, detail, ..
        } => {
            let stopped = *failure == JobFailure::Cancelled;
            JobState::Failed {
                title: if stopped {
                    "Stopped"
                } else {
                    match job.kind {
                        JobKind::Install => "Install failed",
                        JobKind::Update => "Update failed",
                    }
                },
                detail: detail.clone(),
                stopped,
            }
        }
    };
    JobView {
        kind: job.kind,
        state,
        terminal_command,
    }
}

fn row(info: &ExecutorInfo, ctx: Ctx) -> ExecutorRow {
    let health = info.health.as_ref();
    let id = info.descriptor.id.as_str();
    let name = summary::executor_name(id);
    let need = need_of(health);
    let maintenance = info.maintenance.as_ref().filter(|_| ctx.maintain);
    let maintain = maintenance.is_some();
    let managed = maintenance.is_some_and(|m| m.managed);
    let recipe = recipe_of(maintenance);
    let installed_ok = matches!(
        health.map(|h| &h.installed),
        Some(InstallStatus::Installed { .. })
    );
    let version_text = health.and_then(|h| h.installed.version().map(str::to_owned));
    let updater = maintenance.and_then(|m| m.updater.as_ref());
    let available = updater
        .and_then(|u| u.latest.as_deref())
        .zip(version_text.as_deref())
        .filter(|(latest, installed)| installed_ok && is_newer(latest, installed))
        .map(|(latest, _)| latest.to_owned());
    let account = match health.map(|h| &h.signed_in) {
        Some(SignedIn::Yes { account }) if installed_ok => {
            Some(account.clone().unwrap_or_else(|| "signed in".to_owned()))
        }
        _ => None,
    };
    let installed = installed_fact(health);
    let version = health.and_then(version_fact);
    let signed_in = health.and_then(signed_in_fact);
    let group = group_of(info, need, ctx, recipe.is_some(), managed);
    let job = info
        .job
        .as_ref()
        .map(|j| job_view(j, ctx.now_ms, maintenance));
    let busy = job.as_ref().is_some_and(|j| !j.failed());
    let auto_on = maintenance.is_some_and(|m| m.auto_update.on);

    let middle = match (group, need) {
        (Group::Unknown, _) => Some(installed.clone()),
        (Group::NotInstalled | Group::Ready | Group::Test, None) => None,
        (_, Some(Need::SignIn)) => signed_in.clone(),
        (_, Some(Need::Update)) => version.clone(),
        (_, Some(Need::Install | Need::Setup)) => Some(installed.clone()),
        _ => None,
    };
    let source = match (group, &recipe) {
        (Group::NotInstalled, Some(r)) => {
            let tail = r.steps.iter().find(|s| !s.reno).map(|s| {
                if s.label.eq_ignore_ascii_case("sign in") {
                    "then sign in".to_owned()
                } else {
                    format!("then {}", s.label.to_lowercase())
                }
            });
            Some(match tail {
                Some(tail) => format!("{} \u{b7} {tail}", r.source),
                None => r.source.clone(),
            })
        }
        (Group::NotInstalled, None) => installed.detail.clone(),
        _ => None,
    };
    let not_found = matches!(
        health.map(|h| &h.installed),
        Some(InstallStatus::NotInstalled {
            why: NotInstalledReason::NotFound { .. } | NotInstalledReason::NeedsInstall { .. }
        })
    );
    let install = if busy || !not_found {
        None
    } else if maintain {
        if recipe.is_some() && !managed {
            Some(InstallOffer::Review)
        } else if managed || need == Some(Need::Install) {
            Some(InstallOffer::Now)
        } else {
            None
        }
    } else if ctx.installs && need == Some(Need::Install) {
        Some(InstallOffer::Legacy)
    } else {
        None
    };
    let update_known = available.is_some() || need == Some(Need::Update);
    let update = maintain
        && installed_ok
        && !busy
        && updater.is_some()
        && ((update_known && !auto_on) || need == Some(Need::Update));
    let sign_in = (need == Some(Need::SignIn) && !busy)
        .then(|| {
            maintenance
                .and_then(|m| m.sign_in.as_ref())
                .map(|s| s.command.clone())
        })
        .flatten();
    let dot = if job.as_ref().is_some_and(JobView::running) {
        Dot::Running
    } else {
        match group {
            Group::Ready | Group::Test => Dot::Ready,
            Group::NeedsYou => Dot::Blocked,
            Group::NotInstalled => Dot::NotInstalled,
            Group::Unknown => Dot::Unknown,
        }
    };
    ExecutorRow {
        id: id.to_owned(),
        name: name.clone(),
        need,
        group,
        dot,
        installed,
        version,
        signed_in,
        checked: health.map_or_else(
            || "not checked yet".to_owned(),
            |h| {
                format!(
                    "checked {}",
                    summary::ago_label(ctx.now_ms.saturating_sub(h.checked_at_ms))
                )
            },
        ),
        can_install: !maintain && ctx.installs && need == Some(Need::Install),
        maintain,
        managed,
        version_text,
        available,
        account,
        middle,
        source,
        recipe,
        install,
        sign_in,
        update,
        update_primary: update && need == Some(Need::Update),
        switch: maintenance
            .filter(|m| updater.is_some() && !m.managed && installed_ok)
            .map(|m| SwitchView {
                on: m.auto_update.on,
                label: format!("Auto-update {name}"),
            }),
        lock: maintenance.is_some_and(|m| m.managed) && installed_ok,
        auto: maintenance.filter(|_| installed_ok).and_then(|m| {
            auto_view(
                &name,
                m,
                health.and_then(|h| h.installed.version()),
                health.map(|h| &h.version_gate),
            )
        }),
        job,
    }
}

/// "Claude Code is signed out · Codex is too old".
fn blockers(rows: &[ExecutorRow]) -> String {
    rows.iter()
        .filter(|r| r.group == Group::NeedsYou)
        .filter_map(|r| r.need.map(|n| format!("{} {}", r.name, n.phrase())))
        .collect::<Vec<_>>()
        .join(" \u{b7} ")
}

/// "Devin, Fx, Hermes can be installed from here."
fn installable(rows: &[ExecutorRow]) -> Option<String> {
    let names: Vec<&str> = rows
        .iter()
        .filter(|r| r.group == Group::NotInstalled && r.install.is_some())
        .map(|r| r.name.as_str())
        .collect();
    match names.len() {
        0 => None,
        1 => Some(format!("{} can be installed from here.", names[0])),
        _ => Some(format!("{} can be installed from here.", names.join(", "))),
    }
}

/// What the Executors screen shows.
pub(crate) fn build(state: &State, now_ms: u64) -> ExecutorsView {
    let installs = state.supports(reno_proto::FEATURE_EXECUTOR_INSTALL);
    let maintain = state.supports(reno_proto::FEATURE_EXECUTOR_MAINTAIN);
    let ctx = Ctx {
        installs,
        maintain,
        now_ms,
    };
    let mut rows: Vec<ExecutorRow> = state
        .executors()
        .iter()
        .map(|info| row(info, ctx))
        .collect();
    // Groups in order; what needs the human first within Needs you, the rest by name.
    rows.sort_by(|a, b| {
        a.group
            .cmp(&b.group)
            .then_with(|| {
                // Only what needs the person is ranked by how pressing it is.
                let rank = |r: &ExecutorRow| r.need.filter(|_| r.group == Group::NeedsYou);
                rank(a).cmp(&rank(b))
            })
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
            .then_with(|| a.id.cmp(&b.id))
    });
    let needing = rows.iter().filter(|r| r.group == Group::NeedsYou).count();
    let ready = rows.iter().filter(|r| r.group == Group::Ready).count();
    let updates: Vec<String> = rows
        .iter()
        .filter(|r| r.update && r.job.is_none())
        .map(|r| r.id.clone())
        .collect();
    let installable = installable(&rows);
    let (lede, detail) = match (needing, ready, rows.len()) {
        (0, _, 0) => (
            "No executors".to_owned(),
            "Reno drives agent CLIs; the daemon lists the ones it can drive here.".to_owned(),
        ),
        (0, 0, _) => (
            "No executor is ready to run".to_owned(),
            installable.unwrap_or_else(|| {
                "A run goes only to one that is installed, supported and signed in.".to_owned()
            }),
        ),
        (0, n, _) => (
            format!("{n} ready to run"),
            installable.unwrap_or_else(|| {
                "A run goes only to one that is installed, supported and signed in.".to_owned()
            }),
        ),
        (1, _, _) => ("1 executor can't take runs".to_owned(), blockers(&rows)),
        (n, _, _) => (format!("{n} executors can't take runs"), blockers(&rows)),
    };
    let jobs_running = rows
        .iter()
        .filter(|r| r.job.as_ref().is_some_and(|j| !j.failed()))
        .count();
    let checked = state
        .executors()
        .iter()
        .filter_map(|e| e.health.as_ref().map(|h| h.checked_at_ms))
        .max()
        .map(|at| format!("checked {}", summary::ago_label(now_ms.saturating_sub(at))));
    ExecutorsView {
        banner: banner(state),
        rows: Section::of_state(state, state.feed(Feed::Executors), rows),
        lede,
        detail,
        needing,
        ready,
        installs,
        maintain,
        checked,
        jobs_running,
        updates_waiting: updates,
    }
}

/// Why a failed run's executor is worth a look, for the Run screen: the run failed because its
/// executor could not be used, and what the daemon knows about that executor now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FailedExecutor {
    pub(crate) row: ExecutorRow,
}

/// Whether the run's typed stop reason says its executor could not take it: not installed, not
/// signed in, or not eligible (an access mode or sandbox it cannot honour, a health gate). The
/// daemon says so in `RunSummary::stop`; the diagnostic's text is never read.
pub(crate) fn is_executor_refusal(reason: &StopDetail) -> bool {
    matches!(
        reason,
        StopDetail::StartRefused {
            failure: StartRefusal::NotInstalled
                | StartRefusal::NotSignedIn
                | StartRefusal::NoEligibleExecutor,
            ..
        }
    )
}

/// The health of `run`'s executor to show inline when the run failed for an executor reason.
/// `None` for any other run, and when the daemon has not listed that executor.
pub(crate) fn failed_executor(state: &State, run: &RunId, now_ms: u64) -> Option<FailedExecutor> {
    let summary = state.run(run)?;
    if summary.status != RunStatus::Failed
        || !summary.stop.as_ref().is_some_and(is_executor_refusal)
    {
        return None;
    }
    let info = state
        .executors()
        .iter()
        .find(|e| e.descriptor.id == summary.executor)?;
    let ctx = Ctx {
        installs: state.supports(reno_proto::FEATURE_EXECUTOR_INSTALL),
        maintain: state.supports(reno_proto::FEATURE_EXECUTOR_MAINTAIN),
        now_ms,
    };
    Some(FailedExecutor {
        row: row(info, ctx),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use reno_client::{FeedState, Update};
    use reno_proto::{
        AccessMode, ExecutorCaps, ExecutorDescriptor, ModelCatalog, ModelInfo, SandboxLevel,
        Steering,
    };

    use super::*;

    const NOW: u64 = 1_000 * 3_600_000;
    const MIN: u64 = 60_000;

    fn healthy() -> ExecutorHealth {
        ExecutorHealth {
            installed: InstallStatus::Installed {
                path: "/usr/local/bin/claude".into(),
                version: Some("2.1.325".into()),
            },
            version_gate: VersionGate::Ok,
            signed_in: SignedIn::Yes {
                account: Some("ada@example.com".into()),
            },
            models: ModelCatalog::curated(vec![]),
            checked_at_ms: NOW - 3 * MIN,
        }
    }

    fn info(id: &str, health: Option<ExecutorHealth>) -> ExecutorInfo {
        let models: Vec<ModelInfo> = health
            .as_ref()
            .map(|h| h.models.models.clone())
            .unwrap_or_default();
        ExecutorInfo {
            descriptor: ExecutorDescriptor {
                id: id.parse().unwrap(),
                contract_version: 1,
                adapter_version: "t".into(),
                cli_verified: VersionRange {
                    min: "1".into(),
                    verified: "2".into(),
                },
                caps: ExecutorCaps {
                    live_effort: None,
                    live_model_options: None,
                    steering: Steering::None,
                    resume: false,
                    access_modes: vec![AccessMode::Supervised, AccessMode::AutoAcceptEdits],
                    sandbox_levels: vec![SandboxLevel::ReadOnly, SandboxLevel::Unrestricted],
                    mcp_injection: false,
                    structured_output: false,
                    deterministic_turn_end: false,
                    plan_mode: false,
                    models,
                    model_switch: false,
                    isolates_user_config: false,
                    model_aliases: vec![],
                },
            },
            health,
            maintenance: None,
            job: None,
        }
    }

    fn with(health: impl FnOnce(&mut ExecutorHealth)) -> ExecutorHealth {
        let mut h = healthy();
        health(&mut h);
        h
    }

    fn state_of(list: Vec<ExecutorInfo>) -> State {
        let mut state = State::new();
        state.apply(Update::Executors(list));
        state
    }

    fn daemon_with(features: &[&str]) -> reno_client::DaemonInfo {
        reno_client::DaemonInfo {
            hello: reno_proto::HelloResult {
                server_version: "t".into(),
                protocol_version: reno_proto::PROTOCOL_VERSION,
                features: features.iter().map(|f| (*f).to_owned()).collect(),
                pid: 1,
            },
            status: None,
        }
    }

    fn only(view: &ExecutorsView) -> &ExecutorRow {
        view.rows.rows().first().unwrap()
    }

    #[test]
    fn loading_empty_unavailable_and_failed_say_which() {
        let mut state = State::new();
        assert_eq!(build(&state, NOW).rows, Section::Loading);
        state.apply(Update::Executors(vec![]));
        let view = build(&state, NOW);
        assert_eq!(view.rows, Section::Empty);
        assert_eq!(view.lede, "No executors");
        state.apply(Update::Feed(Feed::Executors, FeedState::Unavailable));
        assert_eq!(build(&state, NOW).rows, Section::Unavailable);
        state.apply(Update::Feed(
            Feed::Executors,
            FeedState::Failed("boom".into()),
        ));
        assert_eq!(build(&state, NOW).rows, Section::Failed("boom".into()));
        // Not connected: the banner says so, over whatever is known.
        assert!(build(&state, NOW).banner.is_some());
    }

    #[test]
    fn a_healthy_executor_shows_everything_it_found() {
        let state = state_of(vec![info("claude-code", Some(healthy()))]);
        let view = build(&state, NOW);
        assert_eq!(view.lede, "1 ready to run");
        assert_eq!(view.needing, 0);
        let row = only(&view);
        assert_eq!(row.name, "Claude Code");
        assert_eq!(row.need, None);
        assert_eq!(
            row.installed,
            Fact::new(
                Level::Good,
                "Installed",
                Some("/usr/local/bin/claude \u{b7} 2.1.325".to_owned())
            )
        );
        assert_eq!(row.version.as_ref().unwrap().text, "Version supported");
        assert_eq!(
            row.signed_in,
            Some(Fact::new(
                Level::Good,
                "Signed in",
                Some("ada@example.com".to_owned())
            ))
        );
        assert_eq!(row.checked, "checked 3m ago");
        assert!(!row.can_install);
    }

    #[test]
    fn signed_out_is_a_need_with_the_hint() {
        let signed_out = with(|h| {
            h.signed_in = SignedIn::No {
                hint: "run `codex login`".into(),
            }
        });
        let state = state_of(vec![info("codex", Some(signed_out))]);
        let view = build(&state, NOW);
        assert_eq!(view.lede, "1 executor can't take runs");
        assert_eq!(view.detail, "Codex is signed out");
        let row = only(&view);
        assert_eq!(row.need, Some(Need::SignIn));
        assert_eq!(
            row.signed_in,
            Some(Fact::new(
                Level::Bad,
                "Signed out",
                Some("run `codex login`".to_owned())
            ))
        );
        assert!(!row.can_install);
    }

    #[test]
    fn every_version_gate_reads_as_what_it_is() {
        let gate = |gate: VersionGate| {
            let state = state_of(vec![info("codex", Some(with(|h| h.version_gate = gate)))]);
            let view = build(&state, NOW);
            (only(&view).version.clone().unwrap(), only(&view).need)
        };
        assert_eq!(gate(VersionGate::Ok).0.level, Level::Good);
        let (old, need) = gate(VersionGate::TooOld {
            min: "0.9.0".into(),
        });
        assert_eq!(
            (old.level, old.text, old.detail.as_deref()),
            (Level::Bad, "Version too old", Some("needs 0.9.0 or newer"))
        );
        assert_eq!(need, Some(Need::Update));
        let (untested, need) = gate(VersionGate::Untested {
            verified: "0.169.0".into(),
        });
        assert_eq!(
            (untested.level, untested.detail.as_deref()),
            (Level::Warn, Some("verified up to 0.169.0"))
        );
        assert_eq!(need, None, "untested is a caveat, not a need");
        let (unverified, _) = gate(VersionGate::Untested {
            verified: "unverified".into(),
        });
        assert_eq!(
            unverified.detail.as_deref(),
            Some("no version of this agent is verified yet")
        );
        let (unchecked, need) = gate(VersionGate::Unchecked);
        assert_eq!((unchecked.level, need), (Level::Quiet, None));
    }

    #[test]
    fn an_unknown_sign_in_is_never_shown_as_signed_in() {
        let unknown = with(|h| h.signed_in = SignedIn::Unknown);
        let state = state_of(vec![info("acp.grok", Some(unknown))]);
        let view = build(&state, NOW);
        let fact = only(&view).signed_in.clone().unwrap();
        assert_eq!(
            (fact.level, fact.text),
            (Level::Quiet, "Sign-in not checkable")
        );
        assert_eq!(only(&view).need, None);
    }

    #[test]
    fn not_installed_comes_in_four_kinds() {
        let not = |why: NotInstalledReason| {
            let mut state = state_of(vec![info(
                "acp.hermes",
                Some(with(|h| h.installed = InstallStatus::NotInstalled { why })),
            )]);
            state.apply(Update::Daemon(daemon_with(&[
                "executors",
                "executor.install",
            ])));
            build(&state, NOW)
        };
        let view = not(NotInstalledReason::NeedsInstall {
            what: "the Hermes adapter (npm)".into(),
        });
        let row = only(&view);
        assert_eq!(row.need, Some(Need::Install));
        assert!(row.can_install, "the daemon offers executor.install");
        assert_eq!(row.installed.text, "Needs install");
        assert!(row.version.is_none() && row.signed_in.is_none());

        let view = not(NotInstalledReason::NotFound {
            hint: "install it with brew".into(),
        });
        let row = only(&view);
        assert_eq!(row.need, Some(Need::Setup));
        assert!(!row.can_install, "not the daemon's to fetch");
        assert_eq!(
            row.installed.detail.as_deref(),
            Some("install it with brew")
        );

        let view = not(NotInstalledReason::Unusable {
            detail: "not executable".into(),
        });
        assert_eq!(only(&view).need, Some(Need::Setup));

        // A probe that could not finish says nothing: no need, quiet.
        let view = not(NotInstalledReason::ProbeFailed {
            detail: "timed out".into(),
        });
        let row = only(&view);
        assert_eq!(row.need, None);
        assert_eq!(row.installed.level, Level::Quiet);
        assert_eq!(row.group, Group::Unknown);
        assert_eq!(view.lede, "No executor is ready to run");
    }

    #[test]
    fn an_executor_nobody_has_probed_says_so() {
        let state = state_of(vec![info("codex", None)]);
        let view = build(&state, NOW);
        let row = only(&view);
        assert_eq!(row.installed.text, "Not checked yet");
        assert_eq!(row.checked, "not checked yet");
        assert_eq!(row.need, None);
        assert!(row.version.is_none() && row.signed_in.is_none());
    }

    #[test]
    fn what_needs_you_leads_and_the_rest_follows_by_name() {
        let installed = |id: &str| info(id, Some(healthy()));
        let out = info(
            "codex",
            Some(with(|h| h.signed_in = SignedIn::No { hint: "x".into() })),
        );
        let old = info(
            "acp.grok",
            Some(with(|h| {
                h.version_gate = VersionGate::TooOld { min: "9".into() }
            })),
        );
        let needs_install = info(
            "acp.hermes",
            Some(with(|h| {
                h.installed = InstallStatus::NotInstalled {
                    why: NotInstalledReason::NeedsInstall { what: "x".into() },
                }
            })),
        );
        let state = state_of(vec![
            installed("mock"),
            needs_install,
            installed("claude-code"),
            old,
            out,
        ]);
        let view = build(&state, NOW);
        let order: Vec<&str> = view.rows.rows().iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            order,
            ["codex", "acp.grok", "acp.hermes", "claude-code", "mock"]
        );
        assert_eq!(view.needing, 3);
        assert_eq!(view.lede, "3 executors can't take runs");
        assert_eq!(
            view.detail,
            "Codex is signed out \u{b7} Grok is too old \u{b7} Hermes needs an install"
        );
    }

    #[test]
    fn install_is_offered_only_when_the_daemon_has_it() {
        use reno_proto::{HelloResult, PROTOCOL_VERSION};
        let needs_install = info(
            "acp.hermes",
            Some(with(|h| {
                h.installed = InstallStatus::NotInstalled {
                    why: NotInstalledReason::NeedsInstall { what: "x".into() },
                }
            })),
        );
        let mut state = State::new();
        state.apply(Update::Executors(vec![needs_install]));
        assert!(!only(&build(&state, NOW)).can_install, "no daemon yet");
        let hello = |features: &[&str]| reno_client::DaemonInfo {
            hello: HelloResult {
                server_version: "t".into(),
                protocol_version: PROTOCOL_VERSION,
                features: features.iter().map(|f| (*f).to_owned()).collect(),
                pid: 1,
            },
            status: None,
        };
        state.apply(Update::Daemon(hello(&["executors"])));
        let view = build(&state, NOW);
        assert!(!view.installs && !only(&view).can_install);
        state.apply(Update::Daemon(hello(&["executors", "executor.install"])));
        let view = build(&state, NOW);
        assert!(view.installs && only(&view).can_install);
    }

    #[test]
    fn a_failed_run_shows_its_executors_health_only_for_an_executor_reason() {
        use reno_proto::RunSummary;
        let refused = |failure: StartRefusal| StopDetail::StartRefused {
            failure,
            detail: "words for a person, never read".into(),
        };
        let run = |status: RunStatus, stop: Option<StopDetail>| RunSummary {
            model_options: Default::default(),
            effort: None,
            run_id: RunId::from_suffix("r1").unwrap(),
            task_id: "task_t1".parse().unwrap(),
            title: "x".into(),
            executor: "codex".parse().unwrap(),
            status,
            attempt: 1,
            cwd: None,
            created_at_ms: NOW,
            updated_at_ms: NOW,
            model: None,
            prompt: None,
            bot: None,
            stop_reason: None,
            stop,
            report: None,
            not_resumed: None,
            session: None,
        };
        let signed_out = with(|h| {
            h.signed_in = SignedIn::No {
                hint: "login".into(),
            }
        });
        let mut state = State::new();
        state.apply(Update::Executors(vec![info("codex", Some(signed_out))]));
        let id = RunId::from_suffix("r1").unwrap();
        let shown = |state: &State| failed_executor(state, &id, NOW);

        // The three executor reasons, whatever the words say.
        for failure in [
            StartRefusal::NotSignedIn,
            StartRefusal::NotInstalled,
            StartRefusal::NoEligibleExecutor,
        ] {
            state.apply(Update::Runs(vec![run(
                RunStatus::Failed,
                Some(refused(failure)),
            )]));
            let found = shown(&state).unwrap_or_else(|| panic!("{failure:?}"));
            assert_eq!(found.row.need, Some(Need::SignIn));
        }
        // Other failures are not about the executor, even when the words say they are.
        for stop in [
            Some(refused(StartRefusal::WorkspaceRefused)),
            Some(refused(StartRefusal::Invalid)),
            Some(StopDetail::Crashed),
            None,
        ] {
            state.apply(Update::Runs(vec![run(RunStatus::Failed, stop)]));
            assert!(shown(&state).is_none());
        }
        // A run that did not fail, or whose executor is not listed, shows nothing.
        let refusal = Some(refused(StartRefusal::NotSignedIn));
        state.apply(Update::Runs(vec![run(
            RunStatus::Succeeded,
            refusal.clone(),
        )]));
        assert!(shown(&state).is_none());
        state.apply(Update::Runs(vec![run(RunStatus::Failed, refusal)]));
        state.apply(Update::Executors(vec![]));
        assert!(shown(&state).is_none());
    }
}

/// The roster: groups, the headline, what a line says and offers, and the job states.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod roster_tests {
    use reno_client::Update;
    use reno_proto::{
        AccessMode, AutoUpdate, ExecutorCaps, ExecutorDescriptor, ExecutorHealth, ExecutorInfo,
        ExecutorJob, HelloResult, InstallStatus, JobFailure, JobKind, JobPhase, Maintenance,
        ModelCatalog, NotInstalledReason, PROTOCOL_VERSION, PersonStep, SandboxLevel, SetupRecipe,
        SetupStep, SignedIn, Steering, StepRunner, UpdaterInfo, VersionGate, VersionRange,
    };

    use super::*;

    const NOW: u64 = 1_000 * 3_600_000;

    fn health(version: &str, signed_in: SignedIn, gate: VersionGate) -> ExecutorHealth {
        ExecutorHealth {
            installed: InstallStatus::Installed {
                path: "/usr/local/bin/x".into(),
                version: Some(version.into()),
            },
            version_gate: gate,
            signed_in,
            models: ModelCatalog::curated(vec![]),
            checked_at_ms: NOW - 12 * 60_000,
        }
    }

    fn ok_in(email: &str) -> SignedIn {
        SignedIn::Yes {
            account: Some(email.into()),
        }
    }

    fn not_found() -> ExecutorHealth {
        ExecutorHealth {
            installed: InstallStatus::NotInstalled {
                why: NotInstalledReason::NotFound {
                    hint: "install it".into(),
                },
            },
            version_gate: VersionGate::Unchecked,
            signed_in: SignedIn::Unknown,
            models: ModelCatalog::curated(vec![]),
            checked_at_ms: NOW - 12 * 60_000,
        }
    }

    fn updater(cmd: &str, self_updating: bool, can_pin: bool, latest: Option<&str>) -> UpdaterInfo {
        UpdaterInfo {
            command: cmd.into(),
            self_updating,
            can_pin,
            latest: latest.map(str::to_owned),
            checked_at_ms: None,
        }
    }

    fn maint(updater: Option<UpdaterInfo>, auto: bool) -> Maintenance {
        Maintenance {
            setup: None,
            sign_in: Some(PersonStep {
                label: "Sign in".into(),
                command: "x login".into(),
            }),
            updater,
            managed: false,
            auto_update: AutoUpdate {
                on: auto,
                verified_only: false,
            },
        }
    }

    fn recipe() -> SetupRecipe {
        let step = |label: &str, command: &str, runner| SetupStep {
            label: label.into(),
            command: command.into(),
            runner,
        };
        SetupRecipe {
            source: "Homebrew cask devin-cli".into(),
            steps: vec![
                step("Install", "brew install --cask devin-cli", StepRunner::Reno),
                step("Sign in", "devin auth login", StepRunner::Person),
            ],
        }
    }

    fn info(
        id: &str,
        health: Option<ExecutorHealth>,
        maintenance: Option<Maintenance>,
        job: Option<ExecutorJob>,
    ) -> ExecutorInfo {
        ExecutorInfo {
            descriptor: ExecutorDescriptor {
                id: id.parse().unwrap(),
                contract_version: 1,
                adapter_version: "t".into(),
                cli_verified: VersionRange {
                    min: "0".into(),
                    verified: "9".into(),
                },
                caps: ExecutorCaps {
                    live_effort: None,
                    live_model_options: None,
                    steering: Steering::None,
                    resume: false,
                    access_modes: vec![AccessMode::Supervised],
                    sandbox_levels: vec![SandboxLevel::ReadOnly],
                    mcp_injection: false,
                    structured_output: false,
                    deterministic_turn_end: false,
                    plan_mode: false,
                    models: vec![],
                    model_switch: false,
                    isolates_user_config: false,
                    model_aliases: vec![],
                },
            },
            health,
            maintenance,
            job,
        }
    }

    fn job(kind: JobKind, phase: JobPhase) -> ExecutorJob {
        ExecutorJob {
            id: "job-1".into(),
            kind,
            phase,
            started_at_ms: NOW,
        }
    }

    fn running(step: u32, steps: u32) -> JobPhase {
        JobPhase::Running {
            step,
            steps,
            last_line: Some("downloading".into()),
            quiet_since_ms: None,
        }
    }

    fn view_with(list: Vec<ExecutorInfo>, features: &[&str], now: u64) -> ExecutorsView {
        let mut state = State::new();
        state.apply(Update::Executors(list));
        state.apply(Update::Daemon(reno_client::DaemonInfo {
            hello: HelloResult {
                server_version: "t".into(),
                protocol_version: PROTOCOL_VERSION,
                features: features.iter().map(|f| (*f).to_owned()).collect(),
                pid: 1,
            },
            status: None,
        }));
        build(&state, now)
    }

    const MAINTAIN: &[&str] = &["executors", "executor.install", "executor.maintain"];

    fn view(list: Vec<ExecutorInfo>) -> ExecutorsView {
        view_with(list, MAINTAIN, NOW)
    }

    fn row<'a>(view: &'a ExecutorsView, id: &str) -> &'a ExecutorRow {
        view.rows.rows().iter().find(|r| r.id == id).unwrap()
    }

    #[test]
    fn executors_are_grouped_by_what_a_person_can_do_and_the_headline_counts_the_ready() {
        let v = view(vec![
            info(
                "mock",
                Some(health("1", SignedIn::Unknown, VersionGate::Ok)),
                None,
                None,
            ),
            info(
                "claude-code",
                Some(health("2.1.327", ok_in("a@b.c"), VersionGate::Ok)),
                Some(maint(
                    Some(updater("claude update", true, true, None)),
                    true,
                )),
                None,
            ),
            info(
                "codex",
                Some(health("0.169.0", ok_in("a@b.c"), VersionGate::Ok)),
                None,
                None,
            ),
            info(
                "acp.devin",
                Some(not_found()),
                Some(Maintenance {
                    setup: Some(recipe()),
                    ..maint(None, true)
                }),
                None,
            ),
            info("acp.fx", None, None, None),
        ]);
        let groups: Vec<(Group, Vec<&str>)> = Group::ALL
            .iter()
            .map(|g| (*g, v.group(*g).iter().map(|r| r.id.as_str()).collect()))
            .collect();
        assert_eq!(
            groups,
            [
                (Group::NeedsYou, vec![]),
                (Group::Ready, vec!["claude-code", "codex"]),
                (Group::Unknown, vec!["acp.fx"]),
                (Group::NotInstalled, vec!["acp.devin"]),
                (Group::Test, vec!["mock"]),
            ]
        );
        assert_eq!(v.lede, "2 ready to run");
        assert_eq!(v.detail, "Devin can be installed from here.");
        assert_eq!(v.checked.as_deref(), Some("checked 12m ago"));
        // The rows come in group order.
        let order: Vec<&str> = v.rows.rows().iter().map(|r| r.id.as_str()).collect();
        assert_eq!(
            order,
            ["claude-code", "codex", "acp.fx", "acp.devin", "mock"]
        );
    }

    #[test]
    fn a_blocked_executor_turns_the_headline_into_what_is_wrong() {
        let v = view(vec![
            info(
                "claude-code",
                Some(health(
                    "2.1.327",
                    SignedIn::No {
                        hint: "login".into(),
                    },
                    VersionGate::Ok,
                )),
                Some(maint(
                    Some(updater("claude update", true, true, None)),
                    true,
                )),
                None,
            ),
            info(
                "codex",
                Some(health(
                    "0.1.0",
                    ok_in("a@b.c"),
                    VersionGate::TooOld {
                        min: "0.146.0".into(),
                    },
                )),
                Some(maint(
                    Some(updater("codex update", false, false, Some("0.171.0"))),
                    true,
                )),
                None,
            ),
        ]);
        assert_eq!(v.lede, "2 executors can't take runs");
        assert_eq!(
            v.detail,
            "Claude Code is signed out \u{b7} Codex is too old"
        );
        assert_eq!(v.needing, 2);
        assert!(v.group(Group::Ready).is_empty());
        let signed_out = row(&v, "claude-code");
        assert_eq!(signed_out.dot, Dot::Blocked);
        assert_eq!(signed_out.sign_in.as_deref(), Some("x login"));
        assert!(!signed_out.update);
        // Too old: Update is the primary control, even with auto-update on.
        let old = row(&v, "codex");
        assert!(old.update && old.update_primary);
        assert_eq!(old.middle.as_ref().unwrap().text, "Version too old");
    }

    #[test]
    fn a_ready_line_says_the_version_the_update_and_the_full_account_and_nothing_else() {
        let latest = |latest: &str| {
            let v = view(vec![info(
                "codex",
                Some(health(
                    "0.170.0",
                    ok_in("betty.rodriguez+077@googlemail.com"),
                    VersionGate::Untested {
                        verified: "0.169.0".into(),
                    },
                )),
                Some(maint(
                    Some(updater("codex update", false, false, Some(latest))),
                    true,
                )),
                None,
            )]);
            row(&v, "codex").clone()
        };
        let r = latest("0.171.0");
        assert_eq!(r.version_text.as_deref(), Some("0.170.0"));
        assert_eq!(r.available.as_deref(), Some("0.171.0"));
        assert_eq!(
            r.account.as_deref(),
            Some("betty.rodriguez+077@googlemail.com")
        );
        // Untested is not on the line: only the expanded line says it.
        assert_eq!(r.group, Group::Ready);
        assert_eq!(r.version.as_ref().unwrap().text, "Version untested");
        assert!(r.middle.is_none());
        // Only a newer version is "available".
        assert_eq!(latest("0.170.0").available, None);
        assert_eq!(latest("0.169.9").available, None);
        assert_eq!(latest("0.170.1").available.as_deref(), Some("0.170.1"));
        assert_eq!(
            latest("nightly").available,
            None,
            "not a version: not newer"
        );
        // Signed in without an account says so; an unknown sign-in shows no check mark.
        let unknown = view(vec![info(
            "acp.grok",
            Some(health("1.0.0", SignedIn::Unknown, VersionGate::Ok)),
            None,
            None,
        )]);
        assert_eq!(row(&unknown, "acp.grok").account, None);
    }

    #[test]
    fn the_right_side_offers_update_only_with_auto_update_off_and_the_switch_where_there_is_an_updater()
     {
        let codex = |auto: bool, latest: Option<&str>| {
            let v = view(vec![info(
                "codex",
                Some(health("0.170.0", ok_in("a@b.c"), VersionGate::Ok)),
                Some(maint(
                    Some(updater("codex update", false, false, latest)),
                    auto,
                )),
                None,
            )]);
            row(&v, "codex").clone()
        };
        let on = codex(true, Some("0.171.0"));
        assert!(!on.update, "auto-update will do it");
        assert_eq!(on.switch.as_ref().unwrap().label, "Auto-update Codex");
        assert!(on.switch.as_ref().unwrap().on);
        let off = codex(false, Some("0.171.0"));
        assert!(off.update && !off.update_primary);
        assert!(!off.switch.as_ref().unwrap().on);
        assert!(!codex(false, None).update, "no update is known");
        assert!(!codex(false, Some("0.170.0")).update, "already on it");

        // A managed adapter updates with Reno: a lock, no switch, no Update.
        let managed = view(vec![info(
            "acp.antigravity",
            Some(health("1.2.1", SignedIn::Unknown, VersionGate::Ok)),
            Some(Maintenance {
                managed: true,
                ..maint(None, true)
            }),
            None,
        )]);
        let m = row(&managed, "acp.antigravity");
        assert!(m.lock && m.switch.is_none() && !m.update);
        // No updater: nothing at all.
        let none = view(vec![info(
            "acp.hermes",
            Some(health("1.0.0", SignedIn::Unknown, VersionGate::Ok)),
            Some(maint(None, true)),
            None,
        )]);
        let n = row(&none, "acp.hermes");
        assert!(!n.lock && n.switch.is_none() && !n.update && n.install.is_none());
    }

    #[test]
    fn a_first_install_is_reviewed_first_and_a_managed_one_runs_at_once() {
        let devin = |maintenance: Maintenance, health: ExecutorHealth| {
            let v = view(vec![info(
                "acp.devin",
                Some(health),
                Some(maintenance),
                None,
            )]);
            row(&v, "acp.devin").clone()
        };
        let r = devin(
            Maintenance {
                setup: Some(recipe()),
                ..maint(None, true)
            },
            not_found(),
        );
        assert_eq!(r.group, Group::NotInstalled);
        assert_eq!(r.dot, Dot::NotInstalled);
        assert_eq!(r.install, Some(InstallOffer::Review));
        assert_eq!(
            r.source.as_deref(),
            Some("Homebrew cask devin-cli \u{b7} then sign in")
        );
        let recipe = r.recipe.unwrap();
        assert_eq!(recipe.reno_steps(), 1);
        assert!(!recipe.steps[1].reno);

        let mut needs_install = not_found();
        needs_install.installed = InstallStatus::NotInstalled {
            why: NotInstalledReason::NeedsInstall {
                what: "the adapter".into(),
            },
        };
        let managed = devin(
            Maintenance {
                managed: true,
                ..maint(None, true)
            },
            needs_install.clone(),
        );
        assert_eq!(
            managed.group,
            Group::NeedsYou,
            "a managed adapter is a need, not optional"
        );
        assert_eq!(managed.install, Some(InstallOffer::Now));
        // With no recipe and no managed adapter there is nothing Reno can run.
        let nothing = devin(maint(None, true), not_found());
        assert_eq!(nothing.install, None);
    }

    #[test]
    fn a_daemon_without_executor_maintain_gets_check_again_and_the_managed_install_only() {
        let mut needs_install = not_found();
        needs_install.installed = InstallStatus::NotInstalled {
            why: NotInstalledReason::NeedsInstall {
                what: "the adapter".into(),
            },
        };
        let v = view_with(
            vec![
                info(
                    "acp.hermes",
                    Some(needs_install),
                    Some(Maintenance {
                        setup: Some(recipe()),
                        ..maint(None, true)
                    }),
                    None,
                ),
                info(
                    "codex",
                    Some(health("0.170.0", ok_in("a@b.c"), VersionGate::Ok)),
                    Some(maint(
                        Some(updater("codex update", false, false, Some("0.171.0"))),
                        false,
                    )),
                    None,
                ),
            ],
            &["executors", "executor.install"],
            NOW,
        );
        assert!(!v.maintain);
        let h = row(&v, "acp.hermes");
        assert!(!h.maintain && h.recipe.is_none() && h.can_install);
        assert_eq!(h.install, Some(InstallOffer::Legacy));
        assert_eq!(h.group, Group::NeedsYou);
        let c = row(&v, "codex");
        assert!(!c.update && c.switch.is_none() && c.available.is_none() && c.auto.is_none());
        assert!(v.updates_waiting.is_empty());
    }

    #[test]
    fn a_queued_update_says_what_it_waits_for() {
        let v = view(vec![info(
            "codex",
            Some(health("0.170.0", ok_in("a@b.c"), VersionGate::Ok)),
            Some(maint(
                Some(updater("codex update", false, false, Some("0.171.0"))),
                false,
            )),
            Some(job(JobKind::Update, JobPhase::Queued { waiting_runs: 1 })),
        )]);
        let r = row(&v, "codex");
        assert_eq!(
            r.job.as_ref().unwrap().words("Codex"),
            "Update queued \u{2014} waits for 1 running task on Codex"
        );
        assert!(!r.update, "the job owns the line");
        assert_eq!(v.jobs_running, 1, "a queued update counts as a job");
        assert_eq!(r.dot, Dot::Ready, "queued is not running");
    }

    #[test]
    fn a_running_job_counts_a_clock_after_a_second_and_notes_long_and_quiet_runs() {
        let at = |elapsed: u64, phase: JobPhase| {
            let v = view_with(
                vec![info(
                    "acp.devin",
                    Some(not_found()),
                    Some(Maintenance {
                        setup: Some(recipe()),
                        ..maint(None, true)
                    }),
                    Some(job(JobKind::Install, phase)),
                )],
                MAINTAIN,
                NOW + elapsed,
            );
            row(&v, "acp.devin").clone()
        };
        let state = |r: &ExecutorRow| r.job.as_ref().unwrap().state.clone();

        let early = at(500, running(1, 2));
        assert_eq!(early.dot, Dot::Running);
        assert_eq!(
            early.job.as_ref().unwrap().words("Devin"),
            "Installing \u{b7} step 1 of 2"
        );
        assert!(matches!(
            state(&early),
            JobState::Running {
                clock: None,
                long: false,
                ..
            }
        ));

        let s = at(7_000, running(1, 2));
        match state(&s) {
            JobState::Running {
                clock,
                last_line,
                long,
                quiet_minutes,
                ..
            } => {
                assert_eq!(clock.as_deref(), Some("0:07"));
                assert_eq!(last_line.as_deref(), Some("downloading"));
                assert!(!long && quiet_minutes.is_none());
            }
            other => panic!("{other:?}"),
        }
        match state(&at(31_000, running(2, 2))) {
            JobState::Running { long, .. } => assert!(long),
            other => panic!("{other:?}"),
        }
        let quiet = JobPhase::Running {
            step: 1,
            steps: 2,
            last_line: None,
            quiet_since_ms: Some(NOW),
        };
        match state(&at(5 * 60_000 + 2_000, quiet)) {
            JobState::Running { quiet_minutes, .. } => assert_eq!(quiet_minutes, Some(5)),
            other => panic!("{other:?}"),
        }
        // One step needs no "step 1 of 1".
        let one = at(2_000, running(1, 1));
        assert_eq!(one.job.as_ref().unwrap().words("Devin"), "Installing");
    }

    #[test]
    fn a_failed_job_says_what_failed_and_offers_a_terminal_unless_managed() {
        let failed = |kind, failure, managed: bool| {
            let phase = JobPhase::Failed {
                failure,
                detail: "brew asked for your password (sudo). Reno never types a password for you."
                    .into(),
                finished_at_ms: NOW,
            };
            let maintenance = Maintenance {
                setup: Some(recipe()),
                managed,
                ..maint(Some(updater("codex update", false, false, None)), true)
            };
            let h = if kind == JobKind::Install {
                not_found()
            } else {
                health("1", ok_in("a@b.c"), VersionGate::Ok)
            };
            let v = view(vec![info(
                "acp.devin",
                Some(h),
                Some(maintenance),
                Some(job(kind, phase)),
            )]);
            row(&v, "acp.devin").job.clone().unwrap()
        };
        let install = failed(JobKind::Install, JobFailure::NeedsPassword, false);
        assert!(install.failed());
        assert_eq!(install.words("Devin"), "Install failed");
        assert_eq!(
            install.terminal_command.as_deref(),
            Some("brew install --cask devin-cli")
        );
        match &install.state {
            JobState::Failed {
                detail, stopped, ..
            } => {
                assert!(detail.starts_with("brew asked for your password"));
                assert!(!stopped);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            failed(JobKind::Update, JobFailure::ExitCode, false).words("Devin"),
            "Update failed"
        );
        assert_eq!(
            failed(JobKind::Update, JobFailure::ExitCode, false)
                .terminal_command
                .as_deref(),
            Some("codex update")
        );
        // A person's cancel is "Stopped", not a failure. A managed adapter has no commands.
        assert_eq!(
            failed(JobKind::Install, JobFailure::Cancelled, false).words("Devin"),
            "Stopped"
        );
        assert_eq!(
            failed(JobKind::Install, JobFailure::Broken, true).terminal_command,
            None
        );
        // A failed job is not a running one.
        let v = view(vec![info(
            "acp.devin",
            Some(not_found()),
            Some(Maintenance {
                setup: Some(recipe()),
                ..maint(None, true)
            }),
            Some(job(
                JobKind::Install,
                JobPhase::Failed {
                    failure: JobFailure::ExitCode,
                    detail: "x".into(),
                    finished_at_ms: NOW,
                },
            )),
        )]);
        assert_eq!(v.jobs_running, 0);
    }

    #[test]
    fn the_header_counts_jobs_and_offers_update_all_for_more_than_one_waiting() {
        let waiting = |id: &str| {
            info(
                id,
                Some(health("0.1.0", ok_in("a@b.c"), VersionGate::Ok)),
                Some(maint(
                    Some(updater("u", false, false, Some("0.2.0"))),
                    false,
                )),
                None,
            )
        };
        let v = view(vec![waiting("codex"), waiting("acp.grok")]);
        assert_eq!(v.updates_waiting, ["codex", "acp.grok"]);
        // Auto-update on: nothing waits.
        let on = view(vec![info(
            "codex",
            Some(health("0.1.0", ok_in("a@b.c"), VersionGate::Ok)),
            Some(maint(Some(updater("u", false, false, Some("0.2.0"))), true)),
            None,
        )]);
        assert!(on.updates_waiting.is_empty());
    }

    #[test]
    fn auto_update_says_what_on_and_off_mean_for_each_kind_of_cli() {
        let text = |id: &str, version: &str, self_updating: bool, auto: bool, can_pin: bool| {
            let v = view(vec![info(
                id,
                Some(health(version, ok_in("a@b.c"), VersionGate::Ok)),
                Some(maint(
                    Some(updater(
                        &format!("{id} update"),
                        self_updating,
                        can_pin,
                        None,
                    )),
                    auto,
                )),
                None,
            )]);
            row(&v, id).auto.clone().unwrap()
        };
        assert_eq!(
            text("claude-code", "2.1.327", true, true, true).text,
            "Claude Code updates itself; Reno leaves that on."
        );
        assert_eq!(
            text("codex", "0.169.0", false, true, false).text,
            "Reno runs `codex update` once a day, only when Codex has no runs."
        );
        assert_eq!(
            text("claude-code", "2.1.327", true, false, true).text,
            "Reno turns off Claude Code's own updater; it stays on 2.1.327 until you press Update."
        );
        assert_eq!(
            text("codex", "0.169.0", false, false, false).text,
            "Codex stays on 0.169.0."
        );
        assert!(text("claude-code", "2.1.327", true, true, true).can_pin);
        assert!(!text("codex", "0.169.0", false, true, false).can_pin);
    }

    #[test]
    fn the_expanded_facts_keep_the_honesty_rules() {
        let v = view(vec![info(
            "codex",
            Some(health(
                "0.170.0",
                SignedIn::Unknown,
                VersionGate::Untested {
                    verified: "0.169.0".into(),
                },
            )),
            None,
            None,
        )]);
        let r = row(&v, "codex");
        assert_eq!(r.signed_in.as_ref().unwrap().text, "Sign-in not checkable");
        assert_eq!(r.version.as_ref().unwrap().text, "Version untested");
        assert_eq!(r.account, None);
        assert_eq!(
            r.group,
            Group::Ready,
            "untested and unknown are caveats, not blocks"
        );
    }
}
