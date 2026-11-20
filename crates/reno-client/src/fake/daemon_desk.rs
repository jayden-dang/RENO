//! The fake daemon's side of the mocked daemon-desk contract (see [`crate::daemon_desk`]). A
//! demo world started with `FakeConfig::first_run` runs `renod` 0.4.1 with 0.4.2 installed; any
//! other world runs the same version it has installed, so there is nothing to restart. A restart
//! takes about two seconds. One armed with [`FakeDaemon::fail_next_restart`] does not come back,
//! once.

use std::sync::Mutex;

use super::FakeDaemon;
use crate::daemon_desk::{DaemonDesk, RestartError};

/// A restart takes this long.
const RESTART_MS: u64 = 2_000;

/// The version a plain fake daemon runs and has installed.
const PLAIN: &str = "0.4.2";

#[derive(Debug)]
struct Inner {
    running: String,
    installed: String,
    fail_next: bool,
}

#[derive(Debug)]
pub(super) struct Sim(Mutex<Inner>);

pub(super) fn seed(first_run: bool) -> Sim {
    let (running, installed) = if first_run {
        ("0.4.1", "0.4.2")
    } else {
        (PLAIN, PLAIN)
    };
    Sim(Mutex::new(Inner {
        running: running.to_owned(),
        installed: installed.to_owned(),
        fail_next: false,
    }))
}

impl FakeDaemon {
    fn daemon_state(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.daemon_desk
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// The next restart ends with `renod` not coming back (`RENO_DRAFT38_OPEN=restart-fail`).
    pub fn fail_next_restart(&self) {
        self.daemon_state().fail_next = true;
    }

    /// A newer `renod` lands on disk (a test or a demo installs one while the old one runs).
    pub fn install_daemon(&self, version: &str) {
        self.daemon_state().installed = version.to_owned();
    }
}

impl DaemonDesk for FakeDaemon {
    fn running_version(&self) -> String {
        self.daemon_state().running.clone()
    }

    fn installed_version(&self) -> String {
        self.daemon_state().installed.clone()
    }

    fn restart_duration_ms(&self) -> u64 {
        RESTART_MS
    }

    fn restart(&self) -> Result<String, RestartError> {
        let mut state = self.daemon_state();
        if state.running == state.installed {
            return Err(RestartError::UpToDate);
        }
        if std::mem::take(&mut state.fail_next) {
            return Err(RestartError::DidNotComeBack);
        }
        state.running = state.installed.clone();
        Ok(state.running.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::FakeConfig;

    fn daemon(first_run: bool) -> FakeDaemon {
        FakeDaemon::start(FakeConfig {
            first_run,
            ..FakeConfig::default()
        })
        .unwrap()
    }

    #[test]
    fn the_first_run_world_runs_0_4_1_with_0_4_2_installed() {
        let d = daemon(true);
        assert_eq!(
            (d.running_version().as_str(), d.installed_version().as_str()),
            ("0.4.1", "0.4.2")
        );
        assert!(d.versions().restart_available());
    }

    #[test]
    fn a_plain_fake_has_nothing_to_restart() {
        let d = daemon(false);
        assert!(!d.versions().restart_available());
        assert_eq!(d.restart(), Err(RestartError::UpToDate));
    }

    #[test]
    fn a_restart_takes_two_seconds_and_runs_the_installed_version() {
        let d = daemon(true);
        assert_eq!(d.restart_duration_ms(), 2_000);
        assert_eq!(d.restart(), Ok("0.4.2".to_owned()));
        let after = d.versions();
        assert_eq!(after.running, "0.4.2");
        assert!(!after.restart_available(), "the affordance goes");
        assert_eq!(d.restart(), Err(RestartError::UpToDate), "twice is refused");
    }

    #[test]
    fn a_restart_that_does_not_come_back_keeps_the_old_version_and_works_the_second_time() {
        let d = daemon(true);
        d.fail_next_restart();
        let failed = d.restart().unwrap_err();
        assert_eq!(failed, RestartError::DidNotComeBack);
        assert_eq!(
            failed.to_string(),
            "renod did not come back; run `reno daemon start`"
        );
        assert_eq!(d.versions().running, "0.4.1");
        assert_eq!(d.restart(), Ok("0.4.2".to_owned()), "the failure was once");
    }

    #[test]
    fn a_newer_install_makes_a_restart_available_again() {
        let d = daemon(false);
        d.install_daemon("0.5.0");
        assert!(d.versions().restart_available());
        assert_eq!(d.restart(), Ok("0.5.0".to_owned()));
    }
}
