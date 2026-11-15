//! Finding the agent to launch: an explicit executable, the env override, the agent's own CLI on
//! PATH or in a known install location, a managed npm install, a pinned release archive, or a
//! registry agent's record. Everything here is about locating and preparing a program; spawning
//! and the wire are the parent module's.

use std::path::{Path, PathBuf};

use crate::HarnessError;
use crate::proc::scratch::ScratchDir;
use crate::process::Command;
use crate::upstream_proto::HarnessId;

use super::install::newest_antigravity_install;
use super::spec::AcpAgentSpec;

/// PATH + login-shell + extra dirs + node-version-manager scan for a binary.
pub(crate) fn find_on_paths(exe: &str, extra: Vec<PathBuf>) -> Option<PathBuf> {
    crate::executable::find_on_paths(exe, extra)
}

/// A resolved launch: a concrete program, or a managed npm adapter that may still need installing
/// (see [`Launcher::resolve_program`]).
pub(super) enum Launch {
    Program(PathBuf, Vec<String>),
    Managed {
        pin: crate::adapter_install::NpmPin,
        bin_name: &'static str,
        args: Vec<String>,
    },
    Archive {
        entry: PathBuf,
        args: Vec<String>,
    },
}

/// How one agent resolves to a program: its spec and the executable a caller pinned, if any.
#[derive(Clone, Copy)]
pub(super) struct Launcher<'a> {
    pub spec: &'a AcpAgentSpec,
    pub executable: Option<&'a Path>,
}

impl Launcher<'_> {
    /// Whether the agent can be launched now. Overrides go through the same validation as
    /// [`Self::resolve_launch`], so an override that points at nothing reports not installed
    /// instead of an agent that is offered and then fails to launch.
    pub(super) fn installed(&self) -> bool {
        if let Some(p) = self.executable {
            return crate::executable::validate_native_override(p).is_ok();
        }
        if let HarnessId::Registry(agent) = self.spec.id {
            return crate::acp_registry::installed(agent);
        }
        if let Some(p) = std::env::var_os(self.spec.env_override)
            && !p.is_empty()
        {
            return crate::executable::validate_native_override(&PathBuf::from(p)).is_ok();
        }
        if self.installed_archive_entry().is_some() {
            return true;
        }
        if self.spec.id == HarnessId::Antigravity {
            self.find_server().is_some()
        } else {
            find_on_paths(self.spec.executable, (self.spec.extra_paths)()).is_some()
        }
    }

    /// The version of what [`Self::resolve_launch`] finds, when it can be told: a registry agent's
    /// record, a pinned npm install's pin, a binary's own report.
    pub(super) fn version(&self) -> Option<String> {
        if let (HarnessId::Registry(agent), None) = (self.spec.id, self.executable) {
            return crate::acp_registry::record(agent).map(|record| record.version);
        }
        match self.resolve_launch().ok()? {
            Launch::Program(program, _) => crate::executable::binary_version(&program),
            Launch::Archive { entry, .. } => crate::executable::binary_version(&entry),
            Launch::Managed { pin, .. } => return Some(pin.version.to_string()),
        }
        .map(|version| version.to_string())
    }

    fn find_server(&self) -> Option<PathBuf> {
        find_on_paths(self.spec.executable, (self.spec.extra_paths)()).or_else(|| {
            if self.spec.id == HarnessId::Antigravity {
                find_on_paths("agy_acp_server.par", (self.spec.extra_paths)())
            } else {
                None
            }
        })
    }

    fn installed_archive_entry(&self) -> Option<PathBuf> {
        if self.spec.id == HarnessId::Antigravity {
            return newest_antigravity_install().map(|(_, entry)| entry);
        }
        crate::archive_install::installed_entry(self.spec.archive.as_ref()?)
    }

    /// Resolve what to spawn: an explicit or installed binary, or the managed install of the
    /// spec's pinned npm package. `NotInstalled` only when neither the binary nor the machinery
    /// to install it (npm) exists.
    pub(super) fn resolve_launch(&self) -> Result<Launch, HarnessError> {
        let spec_args: Vec<String> = self.spec.args.iter().map(|a| (*a).to_owned()).collect();
        if let (HarnessId::Registry(agent), None) = (self.spec.id, self.executable) {
            let (program, args, _env) = crate::acp_registry::launch(agent)?;
            return Ok(Launch::Program(program, args));
        }
        if let Some(p) = self.executable {
            return crate::executable::validate_native_override(p)
                .map(|program| Launch::Program(program, spec_args));
        }
        if let Some(p) = std::env::var_os(self.spec.env_override)
            && !p.is_empty()
        {
            return crate::executable::validate_native_override(&PathBuf::from(p))
                .map(|program| Launch::Program(program, spec_args));
        }
        if let Some(found) = self.find_server() {
            return Ok(Launch::Program(found, spec_args));
        }
        if let Some(pkg) = self.spec.npm_package {
            let pin = crate::adapter_install::NpmPin::parse(pkg);
            if crate::adapter_install::installed_entry(&pin, self.spec.executable).is_some()
                || crate::adapter_install::find_npm().is_some()
            {
                return Ok(Launch::Managed {
                    pin,
                    bin_name: self.spec.executable,
                    args: spec_args,
                });
            }
        }
        if self.spec.archive.is_some() {
            let entry = self
                .installed_archive_entry()
                .ok_or_else(|| HarnessError::NotInstalled(self.spec.install_hint.into()))?;
            return Ok(Launch::Archive {
                entry,
                args: spec_args,
            });
        }
        Err(HarnessError::NotInstalled(self.spec.install_hint.into()))
    }

    /// Resolve to a concrete (program, args), running the managed install if it hasn't completed
    /// yet. `block_on_install: false` never waits on npm: it kicks the install in the background
    /// and errors out, so a status check never stalls for however long a 500MB dependency tree
    /// takes to land.
    pub(super) async fn resolve_program(
        &self,
        block_on_install: bool,
    ) -> Result<(PathBuf, Vec<String>), HarnessError> {
        match self.resolve_launch()? {
            Launch::Program(program, args) => Ok((program, args)),
            Launch::Managed {
                pin,
                bin_name,
                args,
            } => {
                let entry = match crate::adapter_install::installed_entry(&pin, bin_name) {
                    Some(entry) => entry,
                    None if block_on_install => {
                        crate::adapter_install::ensure_installed(
                            pin,
                            bin_name,
                            self.spec.display_name,
                        )
                        .await?
                    }
                    None => {
                        let display_name = self.spec.display_name;
                        tokio::spawn(async move {
                            if let Err(e) = crate::adapter_install::ensure_installed(
                                pin,
                                bin_name,
                                display_name,
                            )
                            .await
                            {
                                tracing::warn!(
                                    target: "reno_harness::adapter_install",
                                    "background adapter install failed: {e}"
                                );
                            }
                        });
                        return Err(HarnessError::Protocol(format!(
                            "{} adapter is installing in the background",
                            self.spec.display_name
                        )));
                    }
                };
                let (program, mut node_args) = crate::adapter_install::launch_for_entry(&entry)?;
                node_args.extend(args);
                Ok((program, node_args))
            }
            Launch::Archive { entry, args } => Ok((entry, args)),
        }
    }

    /// What an agent needs in its environment to launch, beyond the run's own: Antigravity's
    /// sibling harness binary.
    pub(super) fn configure_environment(&self, cmd: &mut Command, executable: &Path) {
        if self.spec.id == HarnessId::Antigravity
            && let Some(parent) = executable.parent()
        {
            let sibling = parent.join("localharness_external");
            if sibling.is_file() {
                cmd.env("ANTIGRAVITY_HARNESS_PATH", sibling);
                cmd.env("PYTHONUNBUFFERED", "1");
            }
        }
    }

    /// A registry agent's environment, as its registry entry gives it.
    pub(super) fn registry_environment(&self, cmd: &mut Command) {
        if let (HarnessId::Registry(agent), None) = (self.spec.id, self.executable)
            && let Ok((_, _, env)) = crate::acp_registry::launch(agent)
        {
            for (key, value) in env {
                cmd.env(key, value);
            }
        }
    }

    /// A scratch directory for an archive launch, which unpacks on start.
    pub(super) fn scratch(&self) -> Result<Option<ScratchDir>, HarnessError> {
        Ok(matches!(self.resolve_launch()?, Launch::Archive { .. })
            .then(|| ScratchDir::new(self.spec.executable))
            .transpose()?)
    }
}
