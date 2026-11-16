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
