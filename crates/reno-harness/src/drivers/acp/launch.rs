//! Finding the agent to launch: an explicit executable, the env override, the agent's own CLI on
//! PATH or in a known install location, a managed npm install, a pinned release archive, or a
//! registry agent's record. Everything here is about locating and preparing a program; spawning
//! and the wire are the parent module's.

use std::path::{Path, PathBuf};

use crate::HarnessError;
use crate::agent_id::HarnessId;
use crate::proc::scratch::ScratchDir;
use crate::process::Command;

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

/// What [`Launcher::inspect`] found.
pub(super) enum Inspected {
    /// A launchable program, and its version when it will say.
    Ready {
        path: PathBuf,
        version: Option<String>,
    },
    /// A managed adapter that has not been installed: an explicit step.
    NeedsInstall { what: String },
    /// Nothing launchable; `overridden` when the person pointed at a program of their own.
    Missing { message: String, overridden: bool },
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

    /// Resolve to a concrete (program, args). Never installs and never spawns: a managed adapter
    /// that is not installed yet is [`HarnessError::NotInstalled`], and putting it there is
    /// [`Self::install`], an explicit step that discovery reports as `NeedsInstall`.
    pub(super) fn resolve_program(&self) -> Result<(PathBuf, Vec<String>), HarnessError> {
        match self.resolve_launch()? {
            Launch::Program(program, args) => Ok((program, args)),
            Launch::Managed {
                pin,
                bin_name,
                args,
            } => {
                let entry = crate::adapter_install::installed_entry(&pin, bin_name)
                    .ok_or_else(|| HarnessError::NotInstalled(self.needs_install(&pin)))?;
                let (program, mut node_args) = crate::adapter_install::launch_for_entry(&entry)?;
                node_args.extend(args);
                Ok((program, node_args))
            }
            Launch::Archive { entry, args } => Ok((entry, args)),
        }
    }

    /// What discovery says about a managed adapter that is not installed yet.
    fn needs_install(&self, pin: &crate::adapter_install::NpmPin) -> String {
        format!(
            "the {} adapter ({}@{}) needs to be installed",
            self.spec.display_name, pin.name, pin.version
        )
    }

    /// What a probe finds: a launchable program (with its version), a managed adapter that needs
    /// its explicit install, or nothing. Blocking: it reads the filesystem and may run
    /// `--version`. It installs nothing.
    pub(super) fn inspect(&self) -> Inspected {
        let overridden = self.executable.is_some()
            || std::env::var_os(self.spec.env_override).is_some_and(|value| !value.is_empty());
        let registry_version = || match (self.spec.id, self.executable) {
            (HarnessId::Registry(agent), None) => {
                crate::acp_registry::record(agent).map(|record| record.version)
            }
            _ => None,
        };
        let ready = |path: PathBuf, version: Option<String>| Inspected::Ready { path, version };
        match self.resolve_launch() {
            // A pinned release archive that was never installed: the explicit step fetches it.
            Err(HarnessError::NotInstalled(_)) if !overridden && self.spec.archive.is_some() => {
                Inspected::NeedsInstall {
                    what: format!(
                        "the {} release archive (a pinned download)",
                        self.spec.display_name
                    ),
                }
            }
            Err(error) => Inspected::Missing {
                message: match error {
                    HarnessError::NotInstalled(message) => message,
                    other => other.to_string(),
                },
                overridden,
            },
            Ok(Launch::Program(program, _)) => {
                let version = registry_version().or_else(|| {
                    crate::executable::binary_version(&program).map(|version| version.to_string())
                });
                ready(program, version)
            }
            Ok(Launch::Archive { entry, .. }) => {
                let version =
                    crate::executable::binary_version(&entry).map(|version| version.to_string());
                ready(entry, version)
            }
            Ok(Launch::Managed { pin, bin_name, .. }) => {
                match crate::adapter_install::installed_entry(&pin, bin_name) {
                    Some(entry) => ready(entry, Some(pin.version.to_owned())),
                    None => Inspected::NeedsInstall {
                        what: format!(
                            "the {} adapter ({}@{}, installed with npm)",
                            self.spec.display_name, pin.name, pin.version
                        ),
                    },
                }
            }
        }
    }

    /// The explicit install step: puts the managed adapter (npm) or the pinned release archive
    /// where a launch finds it. Does nothing for an agent that is already launchable.
    pub(super) async fn install(&self) -> Result<(), HarnessError> {
        match self.resolve_launch() {
            Ok(Launch::Managed { pin, bin_name, .. }) => {
                crate::adapter_install::ensure_installed(pin, bin_name, self.spec.display_name)
                    .await
                    .map(|_| ())
            }
            Ok(Launch::Program(..) | Launch::Archive { .. }) => Ok(()),
            Err(error) => match self.spec.archive.as_ref() {
                Some(pin) if self.executable.is_none() => {
                    crate::archive_install::ensure_installed(*pin, self.spec.display_name)
                        .await
                        .map(|_| ())
                }
                _ => Err(error),
            },
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
