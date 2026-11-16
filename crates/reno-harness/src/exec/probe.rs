//! Pieces the drivers' probes share. A probe never runs a model, never installs anything, and
//! is bounded in time and output; what it cannot tell it reports as unknown.

use std::path::{Path, PathBuf};
use std::time::Duration;

use reno_proto::{HealthProbe, InstallStatus, ModelCatalog, NotInstalledReason};

use crate::executable::{self, Capture};

/// How long a short status command (`--version`, `auth status`) may take.
pub(crate) const COMMAND_BUDGET: Duration = Duration::from_secs(10);

/// How long a probe that talks to the harness over its wire (a short-lived app-server, an ACP
/// agent) may take in all.
pub(crate) const WIRE_BUDGET: Duration = Duration::from_secs(20);

/// The CLI's version as it reports it, read off the executor's thread.
pub(crate) async fn cli_version(path: PathBuf) -> Option<String> {
    tokio::task::spawn_blocking(move || executable::binary_version(&path))
        .await
        .ok()
        .flatten()
        .map(|version| version.to_string())
}

/// Whether `path args` exited with status zero, and what it printed on standard output (at most
/// 64 KiB), when it finished inside `budget`; `None` when it could not be run or did
/// not finish. It runs with no stdin and in its own process group, killed when the call ends.
pub(crate) async fn status_and_stdout(
    path: &Path,
    args: &'static [&'static str],
    budget: Duration,
) -> Option<(bool, Vec<u8>)> {
    let path = path.to_owned();
    let captured =
        tokio::task::spawn_blocking(move || executable::run_bounded(&path, args, budget))
            .await
            .ok()?;
    match captured {
        Capture::Finished {
            success, stdout, ..
        } => Some((success, stdout)),
        Capture::Inconclusive => None,
    }
}

/// An installed harness.
pub(crate) fn installed(path: PathBuf, version: Option<String>) -> InstallStatus {
    InstallStatus::Installed { path, version }
}

/// Why `message` (a resolver's complaint) means "not installed": `Unusable` when the user pointed
/// at a program of their own (`overridden`), `NotFound` otherwise.
pub(crate) fn not_installed_reason(message: String, overridden: bool) -> NotInstalledReason {
    if overridden {
        NotInstalledReason::Unusable { detail: message }
    } else {
        NotInstalledReason::NotFound { hint: message }
    }
}

/// A probe that could not run at all.
pub(crate) fn failed(detail: impl std::fmt::Display, models: ModelCatalog) -> HealthProbe {
    HealthProbe::not_installed(
        NotInstalledReason::ProbeFailed {
            detail: detail.to_string(),
        },
        models,
    )
}
