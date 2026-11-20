//! Restarting `renod` when a newer one is installed than the one running (ROAD-38): what the
//! person is told before it happens, and the steps it goes through. Pure; the status bar (and
//! each draft's own surface) draws it.

use reno_client::daemon_desk::{DaemonVersions, RestartError};

/// Where a restart is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Nothing happening: the affordance shows when a newer `renod` is installed.
    Idle,
    /// "Restart renod?" is up.
    Asking,
    /// The old process stopped; the app waits for the new one.
    Restarting,
    /// It did not come back: why, in words.
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Restart {
    pub(crate) phase: Phase,
}

impl Default for Restart {
    fn default() -> Self {
        Self { phase: Phase::Idle }
    }
}

impl Restart {
    /// The affordance was pressed. A failure says so again rather than hiding: pressing it asks
    /// once more.
    pub(crate) fn ask(&mut self) {
        if matches!(self.phase, Phase::Idle | Phase::Failed(_)) {
            self.phase = Phase::Asking;
        }
    }

    /// Esc, or "Not now". Also dismisses a failure.
    pub(crate) fn cancel(&mut self) {
        if matches!(self.phase, Phase::Asking | Phase::Failed(_)) {
            self.phase = Phase::Idle;
        }
    }

    /// "Restart" was confirmed. `false` when nothing was asked.
    pub(crate) fn begin(&mut self) -> bool {
        let asked = self.phase == Phase::Asking;
        if asked {
            self.phase = Phase::Restarting;
        }
        asked
    }

    /// What the daemon answered. The new version running ends it; a failure stays on show.
    pub(crate) fn finish(&mut self, result: Result<String, RestartError>) {
        if self.phase == Phase::Restarting {
            self.phase = match result {
                Ok(_) => Phase::Idle,
                Err(error) => Phase::Failed(error.to_string()),
            };
        }
    }
}

/// The affordance shows only when a newer `renod` is installed than the one running, or while a
/// restart is going or has failed.
pub(crate) fn offered(versions: Option<&DaemonVersions>, restart: &Restart) -> bool {
    match restart.phase {
        Phase::Idle => versions.is_some_and(DaemonVersions::restart_available),
        _ => true,
    }
}

/// What happens to the runs that are live, in words: they keep going, and the app reconnects.
pub(crate) fn live_words(live: usize) -> String {
    match live {
        0 => "No runs are live. The app reconnects in a moment.".to_owned(),
        1 => "1 live run keeps going; the app reconnects.".to_owned(),
        n => format!("{n} live runs keep going; the app reconnects."),
    }
}

/// The question the confirm asks.
pub(crate) fn question(versions: &DaemonVersions) -> String {
    format!(
        "Restart renod {} \u{2192} {}?",
        versions.running, versions.installed
    )
}

/// The pill's words.
pub(crate) fn pill_label(phase: &Phase) -> &'static str {
    match phase {
        Phase::Idle | Phase::Asking => "Restart renod",
        Phase::Restarting => "Restarting\u{2026}",
        Phase::Failed(_) => "Restart failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn versions(running: &str, installed: &str) -> DaemonVersions {
        DaemonVersions {
            running: running.to_owned(),
            installed: installed.to_owned(),
        }
    }

    #[test]
    fn it_is_offered_only_when_a_newer_renod_is_installed() {
        let idle = Restart::default();
        assert!(offered(Some(&versions("0.4.1", "0.4.2")), &idle));
        assert!(!offered(Some(&versions("0.4.2", "0.4.2")), &idle));
        assert!(!offered(None, &idle), "a daemon that cannot restart");
    }

    #[test]
    fn it_stays_while_it_runs_and_when_it_failed() {
        let mut restart = Restart::default();
        restart.ask();
        assert!(offered(Some(&versions("0.4.2", "0.4.2")), &restart));
        assert!(restart.begin());
        assert!(offered(None, &restart));
        restart.finish(Err(RestartError::DidNotComeBack));
        assert!(offered(None, &restart));
    }

    #[test]
    fn the_confirm_says_what_happens_to_live_runs() {
        assert_eq!(
            live_words(0),
            "No runs are live. The app reconnects in a moment."
        );
        assert_eq!(live_words(1), "1 live run keeps going; the app reconnects.");
        assert_eq!(live_words(2), "2 live runs keep going; the app reconnects.");
        assert_eq!(
            question(&versions("0.4.1", "0.4.2")),
            "Restart renod 0.4.1 \u{2192} 0.4.2?"
        );
    }

    #[test]
    fn nothing_restarts_without_the_question_being_asked_and_confirmed() {
        let mut restart = Restart::default();
        assert!(!restart.begin(), "no confirm, no restart");
        restart.finish(Ok("0.4.2".into()));
        assert_eq!(
            restart.phase,
            Phase::Idle,
            "an answer nobody asked for is ignored"
        );
        restart.ask();
        restart.cancel();
        assert_eq!(restart.phase, Phase::Idle);
        restart.ask();
        assert!(restart.begin());
        restart.ask();
        assert_eq!(
            restart.phase,
            Phase::Restarting,
            "no second ask while it runs"
        );
        restart.cancel();
        assert_eq!(restart.phase, Phase::Restarting, "it cannot be cancelled");
    }

    #[test]
    fn a_restart_that_worked_is_done_and_one_that_failed_says_why_and_can_be_tried_again() {
        let mut restart = Restart::default();
        restart.ask();
        restart.begin();
        restart.finish(Ok("0.4.2".into()));
        assert_eq!(restart.phase, Phase::Idle);

        restart.ask();
        restart.begin();
        restart.finish(Err(RestartError::DidNotComeBack));
        assert_eq!(
            restart.phase,
            Phase::Failed("renod did not come back; run `reno daemon start`".to_owned())
        );
        assert_eq!(pill_label(&restart.phase), "Restart failed");
        restart.ask();
        assert_eq!(restart.phase, Phase::Asking, "the person may try again");
        restart.cancel();
        assert_eq!(restart.phase, Phase::Idle);
    }
}
