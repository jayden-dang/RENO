//! Screens: Home, Runs, one Run, one Project, Triggers, Executors and Settings.

use reno_proto::RunId;

pub(crate) mod executors;
pub(crate) mod home;
pub(crate) mod project;
pub(crate) mod run;
pub(crate) mod runs;
pub(crate) mod settings;
pub(crate) mod triggers;

/// What a screen asks the shell to do.
#[allow(clippy::enum_variant_names)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ScreenEvent {
    /// Open a run's transcript.
    OpenRun(RunId),
    /// Show the Executors screen.
    OpenExecutors,
    /// Show a project's page (its name in the new-thread heading).
    OpenProject(crate::vm::projects::FolderKey),
}
