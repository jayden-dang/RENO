//! Process lifecycle, executable lookup, installs, code signatures, shell environment and
//! scratch space.

pub(crate) mod adapter_install;
pub mod archive_install;
pub(crate) mod code_signature;
pub(crate) mod executable;
pub mod git_workspace;
pub mod maintain;
pub mod process;
#[cfg(unix)]
pub mod record;
#[cfg(unix)]
pub use record::{
    DEFAULT_SWEEP_GRACE, ProcessRecord, SkipReason, SweepReport, sweep_orphans,
    sweep_orphans_with_grace,
};
pub(crate) mod scratch;
pub mod self_update;
pub mod shell_env;
#[cfg(unix)]
pub mod shell_runner;
