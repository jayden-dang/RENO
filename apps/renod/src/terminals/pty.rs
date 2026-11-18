//! The PTY port (ARCH-16) and its real implementation.
//!
//! [`PtySpawner`] is the only thing in the daemon that starts a shell. Tests use
//! [`FakePty`](super::FakePty); the daemon binary sets [`RealPty`]. A [`SpawnSpec`] carries the
//! sandbox [`Limits`] the shell must run under (D7), so a fake can record that a live run's
//! terminal was limited, and the real spawner turns them into a `sandbox-exec` profile.

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use portable_pty::{CommandBuilder, MasterPty, PtySize, native_pty_system};

use super::sandbox::Limits;

/// What to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    /// The shell.
    pub program: PathBuf,
    /// Its arguments (`-l`).
    pub args: Vec<OsString>,
    /// Where it starts.
    pub cwd: PathBuf,
    /// Its whole environment: nothing else is inherited.
    pub env: Vec<(OsString, OsString)>,
    /// The width, in cells.
    pub cols: u16,
    /// The height, in cells.
    pub rows: u16,
    /// What the shell may not write; `None` for an unlimited shell.
    pub limits: Option<Limits>,
}

/// Why a shell was not started.
#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    /// The sandbox the spec asked for cannot be used. No shell was started.
    #[error("the sandbox is unavailable: {0}")]
    SandboxUnavailable(String),
    /// The PTY or the shell could not be started.
    #[error("could not start the shell: {0}")]
    Failed(String),
}

/// How a shell ended.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExitInfo {
    /// Its exit code, when it exited.
    pub code: Option<i32>,
    /// The signal that ended it, by name.
    pub signal: Option<String>,
}

/// The live parts of a started shell.
pub struct PtyParts {
    /// The shell's process id (the PTY's session leader).
    pub pid: Option<u32>,
    /// Its output: blocks; ends at EOF or an error.
    pub reader: Box<dyn Read + Send>,
    /// Its input: may block when the shell does not read.
    pub writer: Box<dyn Write + Send>,
    /// Resizing and asking what is in the foreground.
    pub control: Arc<dyn PtyControl>,
    /// Waiting for it to end.
    pub child: Box<dyn PtyChild>,
    /// Ending it from outside.
    pub killer: Arc<dyn PtyKiller>,
}

impl std::fmt::Debug for PtyParts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtyParts")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

/// What a running PTY answers to.
pub trait PtyControl: Send + Sync {
    /// Sets the size, in cells.
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()>;
    /// The process group in the PTY's foreground, if it can be told.
    fn foreground_pid(&self) -> Option<u32>;
    /// Lets go of the PTY master (the shell has ended).
    fn release(&self);
}

/// The shell as a process to wait for.
pub trait PtyChild: Send {
    /// Blocks until the shell has ended.
    fn wait(&mut self) -> ExitInfo;
}

/// Ends the shell from another thread.
pub trait PtyKiller: Send + Sync {
    /// Hangs the terminal up: the shell and its foreground jobs get SIGHUP.
    fn hangup(&self);
    /// Kills the shell's process group outright.
    fn kill(&self);
}

/// Starts shells.
pub trait PtySpawner: Send + Sync + std::fmt::Debug {
    /// Starts the shell `spec` describes.
    fn spawn(&self, spec: SpawnSpec) -> Result<PtyParts, SpawnError>;
}

/// The path of the system's sandbox tool.
pub const SANDBOX_EXEC: &str = "/usr/bin/sandbox-exec";

/// The real spawner: a PTY from `portable-pty`, and `sandbox-exec` for a limited shell.
#[derive(Debug, Clone)]
pub struct RealPty {
    sandbox_exec: PathBuf,
}

impl Default for RealPty {
    fn default() -> Self {
        Self {
            sandbox_exec: PathBuf::from(SANDBOX_EXEC),
        }
    }
}

impl RealPty {
    /// The real spawner, with the system's `sandbox-exec`.
    pub fn new() -> Self {
        Self::default()
    }

    /// The same, with another `sandbox-exec` (a test points it at a path that is not there).
    #[must_use]
    pub fn with_sandbox_exec(mut self, path: impl Into<PathBuf>) -> Self {
        self.sandbox_exec = path.into();
        self
    }

    /// Runs `true` under `profile`: spawning `sandbox-exec` succeeds even when it then refuses
    /// the profile, so this is the only way to know that a shell would be limited.
    fn preflight(&self, profile: &str) -> Result<(), SpawnError> {
        if !cfg!(target_os = "macos") {
            return Err(SpawnError::SandboxUnavailable(
                "sandboxed terminals need macOS".to_owned(),
            ));
        }
        let output = std::process::Command::new(&self.sandbox_exec)
            .arg("-p")
            .arg(profile)
            .arg("/usr/bin/true")
            .env_clear()
            .stdin(std::process::Stdio::null())
            .output()
            .map_err(|error| {
                SpawnError::SandboxUnavailable(format!("{}: {error}", self.sandbox_exec.display()))
            })?;
        if output.status.success() {
            Ok(())
        } else {
            Err(SpawnError::SandboxUnavailable(format!(
                "{} refused the profile: {}",
                self.sandbox_exec.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
    }
}

impl PtySpawner for RealPty {
    fn spawn(&self, spec: SpawnSpec) -> Result<PtyParts, SpawnError> {
        let SpawnSpec {
            program,
            args,
            cwd,
            env,
            cols,
            rows,
            limits,
        } = spec;
        let mut command = match &limits {
            Some(limits) => {
                let profile = limits.profile();
                self.preflight(&profile)?;
                let mut command = CommandBuilder::new(&self.sandbox_exec);
                command.arg("-p");
                command.arg(profile);
                command.arg(&program);
                command
            }
            None => CommandBuilder::new(&program),
        };
        for arg in &args {
            command.arg(arg);
        }
        command.cwd(&cwd);
        command.env_clear();
        for (name, value) in &env {
            command.env(name, value);
        }
        let pty = native_pty_system();
        let pair = pty
            .openpty(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| SpawnError::Failed(format!("a pty: {error}")))?;
        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| SpawnError::Failed(error.to_string()))?;
        drop(pair.slave);
        let pid = child.process_id();
        let killer = child.clone_killer();
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| SpawnError::Failed(format!("the pty reader: {error}")))?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| SpawnError::Failed(format!("the pty writer: {error}")))?;
        Ok(PtyParts {
            pid,
            reader,
            writer,
            control: Arc::new(RealControl {
                master: Mutex::new(Some(pair.master)),
            }),
            child: Box::new(RealChild(child)),
            killer: Arc::new(RealKiller {
                pid,
                killer: Mutex::new(killer),
            }),
        })
    }
}

struct RealControl {
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
}

impl PtyControl for RealControl {
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        let master = self.master.lock().unwrap_or_else(PoisonError::into_inner);
        let Some(master) = master.as_ref() else {
            return Ok(());
        };
        master
            .resize(PtySize {
                rows: rows.max(1),
                cols: cols.max(1),
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(io::Error::other)
    }

    fn foreground_pid(&self) -> Option<u32> {
        let master = self.master.lock().unwrap_or_else(PoisonError::into_inner);
        let leader = master.as_ref()?.process_group_leader()?;
        u32::try_from(leader).ok().filter(|pid| *pid > 0)
    }

    fn release(&self) {
        self.master
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
    }
}

struct RealChild(Box<dyn portable_pty::Child + Send + Sync>);

impl PtyChild for RealChild {
    fn wait(&mut self) -> ExitInfo {
        match self.0.wait() {
            Ok(status) => ExitInfo {
                code: status
                    .signal()
                    .is_none()
                    .then(|| i32::try_from(status.exit_code()).unwrap_or(i32::MAX)),
                signal: status.signal().map(str::to_owned),
            },
            Err(error) => {
                tracing::debug!(%error, "waiting for a terminal's shell failed");
                ExitInfo::default()
            }
        }
    }
}

struct RealKiller {
    pid: Option<u32>,
    killer: Mutex<Box<dyn portable_pty::ChildKiller + Send + Sync>>,
}

impl RealKiller {
    fn signal_group(&self, signal: rustix::process::Signal) {
        let pid = self
            .pid
            .and_then(|pid| i32::try_from(pid).ok())
            .and_then(rustix::process::Pid::from_raw);
        if let Some(pid) = pid {
            // The shell leads its own session and process group, so this reaches its jobs.
            let _ = rustix::process::kill_process_group(pid, signal);
        }
    }
}

impl PtyKiller for RealKiller {
    fn hangup(&self) {
        self.signal_group(rustix::process::Signal::HUP);
        let _ = self
            .killer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .kill();
    }

    fn kill(&self) {
        self.signal_group(rustix::process::Signal::KILL);
    }
}

/// Whether `path` is an executable file.
pub(crate) fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}
