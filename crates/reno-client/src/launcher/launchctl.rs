//! `launchctl`, behind a trait so that nothing but the one real implementation touches the
//! user's launchd. Tests use [`FakeLaunchctl`]; `SystemLaunchctl` also takes the program from
//! `RENO_LAUNCHCTL`, so end-to-end tests can point it at a script.

use std::fmt::Debug;
use std::path::{Path, PathBuf};
use std::process::Command;
#[cfg(any(test, feature = "fake"))]
use std::sync::Mutex;

/// What `launchctl` does for us. The domain is `gui/<uid>`; the service is `<domain>/<label>`.
pub trait Launchctl: Send + Sync + Debug {
    /// Loads the job in `plist` into `domain` (`launchctl bootstrap`).
    fn bootstrap(&self, domain: &str, plist: &Path) -> Result<(), LaunchctlError>;
    /// Unloads the job, stopping it with SIGTERM and waiting for it (`launchctl bootout`).
    fn bootout(&self, domain: &str, label: &str) -> Result<(), LaunchctlError>;
    /// Starts the job now if it is not running (`launchctl kickstart`, without `-k`).
    fn kickstart(&self, domain: &str, label: &str) -> Result<(), LaunchctlError>;
    /// Whether the job is loaded (`launchctl print`).
    fn is_loaded(&self, domain: &str, label: &str) -> Result<bool, LaunchctlError>;
}

/// Why a `launchctl` call failed.
#[derive(Debug, thiserror::Error)]
pub enum LaunchctlError {
    /// The program could not be run.
    #[error("cannot run {}: {reason}", program.display())]
    Run {
        /// The program.
        program: PathBuf,
        /// What went wrong.
        reason: String,
    },
    /// `launchctl` ran and said no.
    #[error("`launchctl {command}` failed ({status}): {message}")]
    Failed {
        /// The subcommand and its target.
        command: String,
        /// The exit status.
        status: String,
        /// What it printed.
        message: String,
    },
}

/// The real `launchctl`.
#[derive(Debug, Clone)]
pub struct SystemLaunchctl {
    program: PathBuf,
}

/// The environment variable that replaces the `launchctl` program, for tests.
pub const LAUNCHCTL_ENV: &str = "RENO_LAUNCHCTL";

/// Exit status of `launchctl print` and `kickstart` for a service that is not loaded.
const NO_SUCH_SERVICE: i32 = 113;

impl SystemLaunchctl {
    /// `/bin/launchctl`, or `$RENO_LAUNCHCTL` when set.
    pub fn from_env() -> Self {
        let program = std::env::var_os(LAUNCHCTL_ENV)
            .filter(|value| !value.is_empty())
            .map_or_else(|| PathBuf::from("/bin/launchctl"), PathBuf::from);
        Self { program }
    }

    fn run(&self, args: &[&str]) -> Result<std::process::Output, LaunchctlError> {
        Command::new(&self.program)
            .args(args)
            .output()
            .map_err(|e| LaunchctlError::Run {
                program: self.program.clone(),
                reason: e.to_string(),
            })
    }

    fn check(&self, args: &[&str]) -> Result<(), LaunchctlError> {
        let output = self.run(args)?;
        if output.status.success() {
            return Ok(());
        }
        Err(failed(args, &output))
    }
}

fn failed(args: &[&str], output: &std::process::Output) -> LaunchctlError {
    let mut message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
    if message.is_empty() {
        message = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    }
    LaunchctlError::Failed {
        command: args.join(" "),
        status: output.status.to_string(),
        message,
    }
}

impl Launchctl for SystemLaunchctl {
    fn bootstrap(&self, domain: &str, plist: &Path) -> Result<(), LaunchctlError> {
        self.check(&["bootstrap", domain, &plist.to_string_lossy()])
    }

    fn bootout(&self, domain: &str, label: &str) -> Result<(), LaunchctlError> {
        self.check(&["bootout", &format!("{domain}/{label}")])
    }

    fn kickstart(&self, domain: &str, label: &str) -> Result<(), LaunchctlError> {
        self.check(&["kickstart", &format!("{domain}/{label}")])
    }

    fn is_loaded(&self, domain: &str, label: &str) -> Result<bool, LaunchctlError> {
        let target = format!("{domain}/{label}");
        let args = ["print", target.as_str()];
        let output = self.run(&args)?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(NO_SUCH_SERVICE) => Ok(false),
            _ => Err(failed(&args, &output)),
        }
    }
}

#[cfg(any(test, feature = "fake"))]
/// A `launchctl` that does nothing but remember what it was asked, for tests. Loading and
/// unloading are tracked, so `is_loaded` answers truthfully.
#[cfg(any(test, feature = "fake"))]
#[derive(Debug, Default)]
pub struct FakeLaunchctl {
    state: Mutex<FakeState>,
}

#[cfg(any(test, feature = "fake"))]
#[derive(Debug, Default)]
struct FakeState {
    calls: Vec<String>,
    loaded: bool,
    fail: Option<String>,
}

#[cfg(any(test, feature = "fake"))]
impl FakeLaunchctl {
    /// A fake with nothing loaded.
    pub fn new() -> Self {
        Self::default()
    }

    /// A fake with the job already loaded.
    pub fn loaded() -> Self {
        let fake = Self::default();
        fake.lock().loaded = true;
        fake
    }

    /// Makes every later call fail with `message`.
    pub fn failing(&self, message: &str) {
        self.lock().fail = Some(message.to_owned());
    }

    /// The calls so far, as `bootstrap <domain> <plist>`, `bootout <target>`, ...
    pub fn calls(&self) -> Vec<String> {
        self.lock().calls.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, FakeState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn record(
        &self,
        call: String,
        effect: impl FnOnce(&mut FakeState),
    ) -> Result<(), LaunchctlError> {
        let mut state = self.lock();
        state.calls.push(call.clone());
        if let Some(message) = &state.fail {
            return Err(LaunchctlError::Failed {
                command: call,
                status: "exit status: 1".to_owned(),
                message: message.clone(),
            });
        }
        effect(&mut state);
        Ok(())
    }
}

#[cfg(any(test, feature = "fake"))]
impl Launchctl for FakeLaunchctl {
    fn bootstrap(&self, domain: &str, plist: &Path) -> Result<(), LaunchctlError> {
        self.record(format!("bootstrap {domain} {}", plist.display()), |s| {
            s.loaded = true;
        })
    }

    fn bootout(&self, domain: &str, label: &str) -> Result<(), LaunchctlError> {
        self.record(format!("bootout {domain}/{label}"), |s| s.loaded = false)
    }

    fn kickstart(&self, domain: &str, label: &str) -> Result<(), LaunchctlError> {
        self.record(format!("kickstart {domain}/{label}"), |_| {})
    }

    fn is_loaded(&self, _domain: &str, _label: &str) -> Result<bool, LaunchctlError> {
        Ok(self.lock().loaded)
    }
}
