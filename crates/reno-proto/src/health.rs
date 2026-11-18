//! What `executor.list`, `executor.refresh` and `executors.watch` carry: for each executor its
//! descriptor and the health the daemon last found for it.
//!
//! Health is what a probe learned without running a model call: whether the harness is installed
//! and which version, whether that version is inside what the adapter was verified against,
//! whether the harness is signed in, and which models it offers. Nothing here is a guess: a fact
//! the probe could not establish is [`SignedIn::Unknown`] or [`VersionGate::Unchecked`], never a
//! silent yes.
//!
//! [`HealthProbe`] is what a driver reports (it has no clock). [`ExecutorHealth`] is that plus
//! the time the daemon stamped on it.

use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::exec::{ExecutorDescriptor, ModelInfo, VersionRange};
use crate::ids::ExecutorId;

/// Why a harness cannot be launched.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum NotInstalledReason {
    /// No program was found where the driver looks.
    NotFound {
        /// Where it looks and how to get it, for a person.
        hint: String,
    },
    /// The agent's launcher needs a managed install (an adapter package or a release archive)
    /// that has not been done. A run never installs: this is an explicit step.
    NeedsInstall {
        /// What would be installed.
        what: String,
    },
    /// Something is there but cannot be launched (an override that points at nothing, a file
    /// that is not executable).
    Unusable {
        /// What is wrong with it.
        detail: String,
    },
    /// The probe itself failed, so nothing is known.
    ProbeFailed {
        /// What happened.
        detail: String,
    },
}

/// Whether the harness can be launched, and from where.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum InstallStatus {
    /// A launchable program was found.
    Installed {
        /// The program a run would launch.
        path: PathBuf,
        /// Its version, when it could be read.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<String>,
    },
    /// Nothing launchable.
    NotInstalled {
        /// Why.
        why: NotInstalledReason,
    },
}

impl InstallStatus {
    /// The version, for an installed harness whose version was read.
    pub fn version(&self) -> Option<&str> {
        match self {
            Self::Installed { version, .. } => version.as_deref(),
            Self::NotInstalled { .. } => None,
        }
    }
}

/// The installed version against the adapter's verified range.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum VersionGate {
    /// At least the minimum, and no newer than the last version the adapter was verified on.
    Ok,
    /// Older than the minimum the adapter supports: not eligible.
    TooOld {
        /// The oldest supported version.
        min: String,
    },
    /// Newer than the last version the adapter was verified on: eligible, but unproven.
    Untested {
        /// The newest version the conformance suite passed on.
        verified: String,
    },
    /// There is no version to compare: not installed, or the CLI did not report one.
    Unchecked,
}

/// Whether the harness has a login.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum SignedIn {
    /// A login exists.
    Yes {
        /// Who, when the harness says.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    },
    /// No login: not eligible.
    No {
        /// What to do about it.
        hint: String,
    },
    /// The harness offers no way to tell without running a model call. Stays eligible.
    Unknown,
}

/// Where a list of models came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ModelSource {
    /// The adapter's built-in list.
    Curated,
    /// Asked of the live harness by the last probe.
    Live,
}

/// The models an executor offers and where the list came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelCatalog {
    /// Where the list came from.
    pub source: ModelSource,
    /// The models.
    pub models: Vec<ModelInfo>,
}

impl ModelCatalog {
    /// The adapter's built-in list.
    pub fn curated(models: Vec<ModelInfo>) -> Self {
        Self {
            source: ModelSource::Curated,
            models,
        }
    }

    /// A list the live harness gave.
    pub fn live(models: Vec<ModelInfo>) -> Self {
        Self {
            source: ModelSource::Live,
            models,
        }
    }
}

/// What one probe found, before the daemon stamps a time on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HealthProbe {
    /// Whether and where the harness is installed.
    pub installed: InstallStatus,
    /// The installed version against the verified range.
    pub version_gate: VersionGate,
    /// Whether the harness is signed in.
    pub signed_in: SignedIn,
    /// The models on offer.
    pub models: ModelCatalog,
}

impl HealthProbe {
    /// The probe of a harness that is not installed.
    pub fn not_installed(why: NotInstalledReason, models: ModelCatalog) -> Self {
        Self {
            installed: InstallStatus::NotInstalled { why },
            version_gate: VersionGate::Unchecked,
            signed_in: SignedIn::Unknown,
            models,
        }
    }

    /// Why a run on this executor would be refused before anything is spawned, or `None` when it
    /// may go ahead. Not installed, too old and signed out are refused; an unknown sign-in, an
    /// untested version and a probe that could not finish (it says nothing about the harness)
    /// are not.
    pub fn ineligible(&self) -> Option<Ineligible> {
        match &self.installed {
            InstallStatus::NotInstalled {
                why: NotInstalledReason::ProbeFailed { .. },
            } => return None,
            InstallStatus::NotInstalled { why } => {
                return Some(Ineligible::NotInstalled(why.clone()));
            }
            InstallStatus::Installed { .. } => {}
        }
        if let VersionGate::TooOld { min } = &self.version_gate {
            return Some(Ineligible::TooOld {
                found: self.installed.version().map(str::to_owned),
                min: min.clone(),
            });
        }
        if let SignedIn::No { hint } = &self.signed_in {
            return Some(Ineligible::SignedOut(hint.clone()));
        }
        None
    }
}

/// Why a probe's findings make an executor ineligible.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ineligible {
    /// Nothing launchable.
    NotInstalled(NotInstalledReason),
    /// Older than the supported minimum.
    TooOld {
        /// The version found.
        found: Option<String>,
        /// The minimum.
        min: String,
    },
    /// No login; carries the hint.
    SignedOut(String),
}

impl std::fmt::Display for Ineligible {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInstalled(NotInstalledReason::NotFound { hint }) => {
                write!(f, "not installed: {hint}")
            }
            Self::NotInstalled(NotInstalledReason::NeedsInstall { what }) => {
                write!(f, "not installed: {what} needs to be installed")
            }
            Self::NotInstalled(NotInstalledReason::Unusable { detail }) => {
                write!(f, "not installed: {detail}")
            }
            Self::NotInstalled(NotInstalledReason::ProbeFailed { detail }) => {
                write!(f, "not installed: the probe failed: {detail}")
            }
            Self::TooOld { found, min } => write!(
                f,
                "version {} is older than the {min} this adapter supports; update it",
                found.as_deref().unwrap_or("unknown")
            ),
            Self::SignedOut(hint) => write!(f, "not signed in: {hint}"),
        }
    }
}

/// What the daemon last found out about one executor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorHealth {
    /// Whether and where the harness is installed.
    pub installed: InstallStatus,
    /// The installed version against the verified range.
    pub version_gate: VersionGate,
    /// Whether the harness is signed in.
    pub signed_in: SignedIn,
    /// The models on offer, and whether they came from the live harness or the adapter's list.
    pub models: ModelCatalog,
    /// When the probe finished, in milliseconds since the Unix epoch.
    pub checked_at_ms: u64,
    /// The account's plan meters, with feature `executor.usage`. Read separately from the probe
    /// (it asks the provider, not the CLI), so it can be newer or older than `checked_at_ms`.
    /// Absent when unavailable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_usage: Option<crate::usage::PlanUsage>,
}

impl ExecutorHealth {
    /// `probe`, stamped as finished at `checked_at_ms`.
    pub fn from_probe(probe: HealthProbe, checked_at_ms: u64) -> Self {
        let HealthProbe {
            installed,
            version_gate,
            signed_in,
            models,
        } = probe;
        Self {
            installed,
            version_gate,
            signed_in,
            models,
            checked_at_ms,
            plan_usage: None,
        }
    }

    /// The findings without the time.
    pub fn probe(&self) -> HealthProbe {
        HealthProbe {
            installed: self.installed.clone(),
            version_gate: self.version_gate.clone(),
            signed_in: self.signed_in.clone(),
            models: self.models.clone(),
        }
    }

    /// See [`HealthProbe::ineligible`].
    pub fn ineligible(&self) -> Option<Ineligible> {
        self.probe().ineligible()
    }
}

/// One executor as `executor.list` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorInfo {
    /// What the executor declares it can do. Its `caps.models` is the list `health.models`
    /// labels: the live list when a probe got one, else the curated list.
    pub descriptor: ExecutorDescriptor,
    /// The last probe. Absent until the first probe of this executor has finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<ExecutorHealth>,
    /// How this executor is installed, updated and signed in to. Absent from a daemon without
    /// feature `executor.maintain`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maintenance: Option<Maintenance>,
    /// The install or update running on this executor, or the last one that failed. Absent when
    /// there is none (a job that succeeded leaves no trace: the fresh health shows the result).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<ExecutorJob>,
    /// Whether Reno drives this executor itself or it was added from the ACP registry (feature
    /// `executor.registry`). Left off the wire for a built-in one, which is also what a daemon
    /// without the feature means.
    #[serde(
        default,
        skip_serializing_if = "crate::registry::ExecutorSource::is_builtin"
    )]
    pub source: crate::registry::ExecutorSource,
}

/// What a person and Reno can do to install, update and sign in to one executor (feature
/// `executor.maintain`). The commands are compiled into the daemon per adapter: they are shown,
/// never sent back by a client.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Maintenance {
    /// For an executor that is not found: how Reno installs it. Absent when the adapter
    /// declares no recipe.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setup: Option<SetupRecipe>,
    /// The command a person runs to sign in, when the harness has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sign_in: Option<PersonStep>,
    /// The executor's own updater. Absent for executors with no updater (and for managed
    /// adapters: see `managed`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updater: Option<UpdaterInfo>,
    /// A Reno-managed adapter (pinned and checksum-verified): it updates with Reno.
    pub managed: bool,
    /// The stored auto-update setting.
    pub auto_update: AutoUpdate,
}

/// How Reno installs an executor that is not found.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetupRecipe {
    /// Where the install comes from, for a person ("Homebrew cask devin-cli").
    pub source: String,
    /// The steps, in order.
    pub steps: Vec<SetupStep>,
}

/// One step of a [`SetupRecipe`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetupStep {
    /// What the step does, in a few words ("Install").
    pub label: String,
    /// The command line, shown as is.
    pub command: String,
    /// Who runs it.
    pub runner: StepRunner,
}

/// Who runs a [`SetupStep`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum StepRunner {
    /// The daemon, as the user, with no stdin and no sudo.
    Reno,
    /// The person, in a terminal: it is interactive or needs a login.
    Person,
}

/// A command a person runs themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PersonStep {
    /// What it does ("Sign in").
    pub label: String,
    /// The command line.
    pub command: String,
}

/// An executor's own updater.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdaterInfo {
    /// The command that updates it ("codex update"), shown in the UI.
    pub command: String,
    /// The CLI updates itself already.
    pub self_updating: bool,
    /// The updater can install a given version (so "verified only" can pin).
    pub can_pin: bool,
    /// The newest version a check found, when a check could tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest: Option<String>,
    /// When that check ran, in milliseconds since the Unix epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at_ms: Option<u64>,
}

/// The stored auto-update setting of one executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AutoUpdate {
    /// Update when the executor is idle.
    pub on: bool,
    /// Only to the version the adapter was verified on (needs an updater that can pin).
    pub verified_only: bool,
}

/// An install or update of one executor. At most one per executor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorJob {
    /// Opaque.
    pub id: String,
    /// What it does.
    pub kind: JobKind,
    /// Where it is.
    pub phase: JobPhase,
    /// When it started, in milliseconds since the Unix epoch.
    pub started_at_ms: u64,
}

/// What a job does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// Install an executor that is not found.
    Install,
    /// Update an installed executor with its own updater.
    Update,
}

/// Where a job is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum JobPhase {
    /// An update waiting for the executor's runs to end.
    Queued {
        /// How many runs it waits for.
        waiting_runs: u32,
    },
    /// Running.
    Running {
        /// The step that runs, from 1.
        step: u32,
        /// How many steps there are.
        steps: u32,
        /// The last line of output, when there is one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last_line: Option<String>,
        /// Milliseconds since the Unix epoch of the last output, once the job has been quiet for
        /// a while. The job keeps running.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        quiet_since_ms: Option<u64>,
    },
    /// Ended without success. Stays until the next start on this executor or a dismiss.
    Failed {
        /// Why, as a kind.
        failure: JobFailure,
        /// What happened, for a person.
        detail: String,
        /// When it ended, in milliseconds since the Unix epoch.
        finished_at_ms: u64,
    },
}

/// Why a job failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobFailure {
    /// A step exited with a failure status.
    ExitCode,
    /// A step asked for a password. Reno never supplies one.
    NeedsPassword,
    /// The job ran longer than the ceiling and was killed.
    TimedOut,
    /// The job made no progress.
    Stalled,
    /// A person cancelled it.
    Cancelled,
    /// The steps finished but the probe after them did not find the executor.
    Probe,
    /// The job could not run at all (the shell would not start, the recipe is unusable).
    Broken,
}

/// Params of `executor.job.start`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorJobStartParams {
    /// The executor to install or update.
    pub id: ExecutorId,
    /// What to do.
    pub kind: JobKind,
    /// Start an update now even when the executor has runs in flight.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub force: bool,
}

/// Params of `executor.job.cancel`, `executor.job.dismiss` and `executor.job.log`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorJobParams {
    /// The executor whose job it is.
    pub id: ExecutorId,
}

/// Result of `executor.job.start`, `executor.job.cancel`, `executor.job.dismiss` and
/// `executor.auto_update.set`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorJobResult {
    /// The executor after the call.
    pub executor: ExecutorInfo,
}

/// Result of `executor.job.log`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorJobLogResult {
    /// The last lines of output of the current or last job, oldest first (at most 500).
    pub lines: Vec<String>,
    /// Older lines were dropped.
    pub truncated: bool,
}

/// Params of `executor.auto_update.set`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorAutoUpdateSetParams {
    /// The executor.
    pub id: ExecutorId,
    /// Update when idle.
    pub on: bool,
    /// Only to the verified version.
    pub verified_only: bool,
}

/// Compares `version` with the range an adapter was verified on.
///
/// `Ok` inside `[min, verified]`; `TooOld` below `min`; `Untested` above `verified` (or when
/// `verified` is not a version, as for an agent no conformance run covers); `Unchecked` when
/// there is no readable version.
pub fn version_gate(version: Option<&str>, range: &VersionRange) -> VersionGate {
    let Some(found) = version.and_then(parse_loose) else {
        return VersionGate::Unchecked;
    };
    if let Some(min) = parse_loose(&range.min)
        && found < min
    {
        return VersionGate::TooOld {
            min: range.min.clone(),
        };
    }
    match parse_loose(&range.verified) {
        Some(verified) if found <= verified => VersionGate::Ok,
        _ => VersionGate::Untested {
            verified: range.verified.clone(),
        },
    }
}

/// `1.2.3`, `v1.2.3` or `1.2` as a semantic version.
fn parse_loose(text: &str) -> Option<semver::Version> {
    let text = text.trim().trim_start_matches('v');
    if let Ok(version) = semver::Version::parse(text) {
        return Some(version);
    }
    let mut parts = text.split('.');
    let major = parts.next()?;
    let minor = parts.next().unwrap_or("0");
    let patch = parts.next().unwrap_or("0");
    if parts.next().is_some() {
        return None;
    }
    semver::Version::parse(&format!("{major}.{minor}.{patch}")).ok()
}

/// Params of `executor.refresh`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorRefreshParams {
    /// The executor to probe again; every executor when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<ExecutorId>,
    /// Only read the plan usage again (feature `executor.usage`): no probe, so it answers in
    /// the time of one HTTPS request. Throttled to once per 30 s per executor either way.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub usage_only: bool,
}

/// Params of `executor.install`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorInstallParams {
    /// The executor whose managed adapter or release archive is to be fetched and verified. Its
    /// last probe must say `needs_install`.
    pub id: ExecutorId,
}

/// Result of `executor.install`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorInstallResult {
    /// The executor after the install and the probe that followed it.
    pub executor: ExecutorInfo,
}

/// Result of `executor.list` and `executor.refresh`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorListResult {
    /// The executors, by id.
    pub executors: Vec<ExecutorInfo>,
}

/// Params of `executor.frames`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorFramesParams {
    /// The executor to report; every executor when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<ExecutorId>,
}

/// One kind of frame an executor's wire layer did not recognise.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UnknownFrameKind {
    /// The kind, as the driver names it (`type:foo`, `method:bar`).
    pub kind: String,
    /// How many frames of this kind arrived since the daemon started, or since the executor's CLI
    /// version last changed.
    pub count: u64,
    /// When the daemon first saw this kind under the executor's current CLI version, in
    /// milliseconds since the Unix epoch.
    pub first_seen_ms: u64,
}

/// The unrecognised frames of one executor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorFrameCounts {
    /// The executor.
    pub id: ExecutorId,
    /// The CLI version of the executor's last probe, when it had one: the version the kinds
    /// below were first seen under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cli_version: Option<String>,
    /// The kinds, by name. Bounded: a harness cannot grow the list without limit.
    pub frames: Vec<UnknownFrameKind>,
}

/// Result of `executor.frames`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorFramesResult {
    /// One entry per executor, by id. An executor that never saw an unknown frame has an empty
    /// `frames`.
    pub executors: Vec<ExecutorFrameCounts>,
}

/// Params of the `executors.watch` subscription.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ExecutorsWatchParams {}

/// One item of the `executors.watch` subscription: every executor and its last health (latest
/// value wins). The first item is the list as it stands when the subscription starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorsSnapshot {
    /// The executors, by id.
    pub executors: Vec<ExecutorInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(min: &str, verified: &str) -> VersionRange {
        VersionRange {
            min: min.into(),
            verified: verified.into(),
        }
    }

    #[test]
    fn the_gate_compares_with_the_verified_range() {
        let r = range("0.146.0", "0.169.0");
        assert_eq!(version_gate(Some("0.146.0"), &r), VersionGate::Ok);
        assert_eq!(version_gate(Some("0.169.0"), &r), VersionGate::Ok);
        assert_eq!(
            version_gate(Some("0.145.9"), &r),
            VersionGate::TooOld {
                min: "0.146.0".into()
            }
        );
        assert_eq!(
            version_gate(Some("0.170.0"), &r),
            VersionGate::Untested {
                verified: "0.169.0".into()
            }
        );
        assert_eq!(version_gate(None, &r), VersionGate::Unchecked);
        assert_eq!(version_gate(Some("nightly"), &r), VersionGate::Unchecked);
    }

    #[test]
    fn an_agent_with_no_verified_version_is_untested_not_ok() {
        let r = range("0", "unverified");
        assert_eq!(
            version_gate(Some("1.2.3"), &r),
            VersionGate::Untested {
                verified: "unverified".into()
            }
        );
        // A two-part version still compares.
        assert_eq!(
            version_gate(Some("0.5"), &range("0.6", "1.0")),
            VersionGate::TooOld { min: "0.6".into() }
        );
    }

    #[test]
    fn only_a_missing_old_or_signed_out_executor_is_ineligible() {
        let installed = |version: &str| InstallStatus::Installed {
            path: "/bin/x".into(),
            version: Some(version.into()),
        };
        let mut probe = HealthProbe {
            installed: installed("1.0.0"),
            version_gate: VersionGate::Untested {
                verified: "0.9.0".into(),
            },
            signed_in: SignedIn::Unknown,
            models: ModelCatalog::curated(vec![]),
        };
        assert_eq!(probe.ineligible(), None, "untested and unknown are fine");

        probe.signed_in = SignedIn::No {
            hint: "run x login".into(),
        };
        assert_eq!(
            probe.ineligible(),
            Some(Ineligible::SignedOut("run x login".into()))
        );
        probe.version_gate = VersionGate::TooOld {
            min: "2.0.0".into(),
        };
        assert!(matches!(
            probe.ineligible(),
            Some(Ineligible::TooOld { .. })
        ));
        probe.installed = InstallStatus::NotInstalled {
            why: NotInstalledReason::NotFound { hint: "x".into() },
        };
        assert!(matches!(
            probe.ineligible(),
            Some(Ineligible::NotInstalled(_))
        ));
        // A probe that could not finish says nothing about the harness.
        probe.installed = InstallStatus::NotInstalled {
            why: NotInstalledReason::ProbeFailed { detail: "x".into() },
        };
        assert_eq!(probe.ineligible(), None);
    }
}
