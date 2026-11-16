//! Screens: Home, Inbox, Runs, one Run, Triggers, Executors and Settings.

use reno_proto::RunId;

pub(crate) mod executors;
pub(crate) mod home;
pub(crate) mod inbox;
pub(crate) mod run;
pub(crate) mod runs;
pub(crate) mod settings;
pub(crate) mod triggers;

/// What a screen asks the shell to do.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ScreenEvent {
    /// Open a run's transcript.
    OpenRun(RunId),
    /// Show the Executors screen.
    OpenExecutors,
}
