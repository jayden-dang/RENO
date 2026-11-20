//! The MOCKED daemon-desk contract (ROAD-38): which `renod` is running, which one is installed on
//! disk, and restarting the running one onto the installed one. The real daemon has no such RPC
//! yet (it can say its own version in `daemon.status`, but not what is on disk, and it cannot
//! restart itself over the socket), so only the fake daemon implements [`DaemonDesk`]; the app
//! reads and changes it only through the `Store::daemon_*` entries, and only where
//! `Capabilities::daemon_restart` is on.
//!
//! The wire shape the real daemon would take: `daemon.version` answering `{running, installed}`
//! and `daemon.restart` answering when the new process has said hello. The CLI reaches the same
//! (`reno daemon restart`, ARCH-2).

use std::fmt;

/// What `renod` is, and what is on disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonVersions {
    /// The version of the process the app is talking to.
    pub running: String,
    /// The version installed on disk: what a restart would start.
    pub installed: String,
}

impl DaemonVersions {
    /// A restart would change something: what is installed is not what is running.
    #[must_use]
    pub fn restart_available(&self) -> bool {
        self.running != self.installed
    }
}

/// Why a restart did not happen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RestartError {
    /// The running `renod` is the installed one already.
    UpToDate,
    /// The old process stopped and the new one did not answer.
    DidNotComeBack,
}

impl fmt::Display for RestartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::UpToDate => "renod is already the installed version",
            Self::DidNotComeBack => "renod did not come back; run `reno daemon start`",
        })
    }
}

impl std::error::Error for RestartError {}

/// The seam.
pub trait DaemonDesk: Send + Sync {
    /// The version of the `renod` process the app is talking to.
    fn running_version(&self) -> String;
    /// The version installed on disk: what a restart would start.
    fn installed_version(&self) -> String;
    /// Both at once.
    fn versions(&self) -> DaemonVersions {
        DaemonVersions {
            running: self.running_version(),
            installed: self.installed_version(),
        }
    }
    /// How long a restart takes, in ms: the app shows "Restarting" that long, then calls
    /// [`DaemonDesk::restart`].
    fn restart_duration_ms(&self) -> u64;
    /// Restarts `renod` onto the installed version and answers the version now running. Runs in
    /// flight keep going (the daemon hands them over, or the agents outlive it), and the app
    /// reconnects. `UpToDate` when nothing is newer; `DidNotComeBack` leaves the old version
    /// shown as running, and a second try may work.
    fn restart(&self) -> Result<String, RestartError>;
}
