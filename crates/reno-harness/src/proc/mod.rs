//! Process lifecycle, executable lookup, installs, code signatures, shell environment and
//! scratch space.

pub(crate) mod adapter_install;
pub mod archive_install;
pub(crate) mod code_signature;
pub(crate) mod executable;
pub mod install;
pub mod process;
#[cfg(unix)]
pub mod record;
pub(crate) mod scratch;
pub mod shell_env;
