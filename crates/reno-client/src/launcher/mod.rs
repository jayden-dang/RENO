//! Starting, keeping and installing the daemon, for the app and the CLI (ARCH-2: they know the
//! daemon only through this crate).
//!
//! # Two ways to run `renod`
//!
//! - **Detached** (the default): [`Launcher::spawn_detached`] runs `renod --detach` with no
//!   stdin or stdout and its stderr in `<data-dir>/logs/renod.stderr`; `renod` then leaves the
//!   caller's session, so closing the terminal or quitting the app does not end it.
//! - **A LaunchAgent** (opt-in, `reno daemon install`): launchd starts `renod` at login and
//!   restarts it after a crash. When an agent for this data directory is installed, nothing
//!   here spawns `renod` itself: a second copy would lose the data-directory lock and exit with
//!   an error, and launchd would restart it for ever. [`Launcher::start`] asks launchd instead
//!   (`launchctl kickstart`, after `bootstrap` if the job is not loaded).
//!
//! # Modules
//!
//! - [`agent`]: the plist, and whether an installed one is ours.
//! - [`job`]: the plist of a job launchd runs once a day (the nightly conformance run).
//! - [`launchctl`]: `launchctl` behind a trait, with a fake.
//! - [`policy`]: the restart backoff, pure.
//! - [`logs`]: the log files: paths, tails, and a JSON line as text.
//! - [`keeper`]: [`DaemonKeeper`], connect-or-start for the app's connection.

pub mod agent;
pub mod job;
pub mod keeper;
pub mod launchctl;
pub mod logs;
pub mod policy;

use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;

pub use agent::{LABEL, LaunchAgent};
pub use job::ScheduledJob;
pub use keeper::{DaemonKeeper, KeeperConfig};
#[cfg(any(test, feature = "fake"))]
pub use launchctl::FakeLaunchctl;
pub use launchctl::{LAUNCHCTL_ENV, Launchctl, LaunchctlError, SystemLaunchctl};
pub use logs::LogPaths;
pub use policy::{RestartConfig, RestartPolicy, Verdict};

/// The environment variable that replaces `~/Library/LaunchAgents`, for tests.
pub const AGENTS_DIR_ENV: &str = "RENO_LAUNCH_AGENTS_DIR";
/// The environment variable that names the `renod` to run, for tests and for pinning a binary.
pub const RENOD_ENV: &str = "RENO_RENOD";

/// Everything that can go wrong starting, installing or removing the daemon.
#[derive(Debug, thiserror::Error)]
pub enum LauncherError {
    /// `renod` is neither next to this program nor on `PATH`.
    #[error("cannot find `renod`: looked next to this program and on PATH")]
    NoRenod,
    /// A path the LaunchAgent needs is relative or not UTF-8.
    #[error("{} must be an absolute path made of text, for launchd", .0.display())]
    Path(PathBuf),
    /// A time of day launchd cannot run a job at.
    #[error("{hour}:{minute} is not a time of day")]
    Time {
        /// The hour asked for.
        hour: u8,
        /// The minute asked for.
        minute: u8,
    },
    /// `HOME` is not set, so there is nowhere for a LaunchAgent to go.
    #[error("cannot find ~/Library/LaunchAgents: HOME is not set")]
    NoHome,
    /// `renod` could not be started.
    #[error("cannot start {}: {source}", renod.display())]
    Spawn {
        /// The program.
        renod: PathBuf,
        /// What failed.
        source: io::Error,
    },
    /// A file or directory could not be used.
    #[error("{}: {source}", path.display())]
    Io {
        /// The path.
        path: PathBuf,
        /// What failed.
        source: io::Error,
    },
    /// `launchctl` said no.
    #[error(transparent)]
    Launchctl(#[from] LaunchctlError),
}

/// What [`Launcher::start`] did.
#[derive(Debug)]
pub enum Started {
    /// It ran `renod` itself. Keep the child and `try_wait` it, or it stays a zombie when it dies.
    Spawned(Child),
    /// launchd was asked to start the job.
    Launchd,
}

/// Starts and installs the daemon for one data directory. See the module docs.
#[derive(Debug, Clone)]
pub struct Launcher {
    data_dir: PathBuf,
    socket: PathBuf,
    agents_dir: PathBuf,
    launchctl: Arc<dyn Launchctl>,
    uid: u32,
    renod: Option<PathBuf>,
}

fn io_error(path: &Path) -> impl FnOnce(io::Error) -> LauncherError {
    let path = path.to_path_buf();
    move |source| LauncherError::Io { path, source }
}

impl Launcher {
    /// A launcher for the daemon of `data_dir` serving `socket`, using the real `launchctl`
    /// (`$RENO_LAUNCHCTL` if set) and `~/Library/LaunchAgents` (`$RENO_LAUNCH_AGENTS_DIR` if
    /// set).
    pub fn from_env(data_dir: PathBuf, socket: PathBuf) -> Result<Self, LauncherError> {
        let agents_dir = match std::env::var_os(AGENTS_DIR_ENV).filter(|v| !v.is_empty()) {
            Some(dir) => PathBuf::from(dir),
            None => std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|home| PathBuf::from(home).join("Library/LaunchAgents"))
                .ok_or(LauncherError::NoHome)?,
        };
        Ok(Self::new(
            data_dir,
            socket,
            agents_dir,
            Arc::new(SystemLaunchctl::from_env()),
        ))
    }

    /// A launcher with everything given, for tests.
    pub fn new(
        data_dir: PathBuf,
        socket: PathBuf,
        agents_dir: PathBuf,
        launchctl: Arc<dyn Launchctl>,
    ) -> Self {
        Self {
            data_dir,
            socket,
            agents_dir,
            launchctl,
            uid: rustix::process::getuid().as_raw(),
            renod: None,
        }
    }

    /// The same, running this `renod` instead of looking for one.
    #[must_use]
    pub fn with_renod(mut self, renod: PathBuf) -> Self {
        self.renod = Some(renod);
        self
    }

    /// The data directory.
    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// The daemon's log files.
    pub fn logs(&self) -> LogPaths {
        LogPaths::of(&self.data_dir)
    }

    /// The launchd domain of this user: `gui/<uid>`.
    pub fn domain(&self) -> String {
        format!("gui/{}", self.uid)
    }

    /// Where the agent's plist is, or would be.
    pub fn agent_path(&self) -> PathBuf {
        self.agents_dir.join(agent::file_name())
    }

    /// `renod`: `$RENO_RENOD`, else next to the running program, else on `PATH`.
    pub fn find_renod(&self) -> Result<PathBuf, LauncherError> {
        if let Some(renod) = &self.renod {
            return Ok(renod.clone());
        }
        if let Some(renod) = std::env::var_os(RENOD_ENV).filter(|v| !v.is_empty()) {
            return Ok(PathBuf::from(renod));
        }
        let beside = std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf));
        let path = std::env::var_os("PATH").unwrap_or_default();
        let dirs: Vec<PathBuf> = beside
            .into_iter()
            .chain(std::env::split_paths(&path))
            .collect();
        find_in(&dirs).ok_or(LauncherError::NoRenod)
    }

    /// The LaunchAgent for this data directory, running the `renod` that [`find_renod`] finds,
    /// by its absolute path.
    ///
    /// [`find_renod`]: Self::find_renod
    pub fn agent(&self) -> Result<LaunchAgent, LauncherError> {
        let renod = self.find_renod()?;
        let renod = std::fs::canonicalize(&renod).map_err(io_error(&renod))?;
        let defs = std::env::var_os("RENO_DEFS_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        LaunchAgent::new(
            renod,
            self.data_dir.clone(),
            self.socket.clone(),
            self.logs().stderr,
            defs,
        )
    }

    /// Whether an agent that serves this data directory is installed. A plist of ours for
    /// another data directory, or someone else's file, does not count.
    pub fn is_managed(&self) -> bool {
        std::fs::read_to_string(self.agent_path())
            .ok()
            .and_then(|text| agent::data_dir_of(&text))
            .is_some_and(|dir| dir == self.data_dir)
    }

    /// Starts the daemon: through launchd when its agent is installed, else detached.
    pub fn start(&self) -> Result<Started, LauncherError> {
        if self.is_managed() {
            self.kickstart()?;
            return Ok(Started::Launchd);
        }
        self.spawn_detached().map(Started::Spawned)
    }

    /// Asks launchd to run the agent's job, loading it first if it is not loaded.
    pub fn kickstart(&self) -> Result<(), LauncherError> {
        let domain = self.domain();
        if !self.launchctl.is_loaded(&domain, LABEL)? {
            self.launchctl.bootstrap(&domain, &self.agent_path())?;
        }
        Ok(self.launchctl.kickstart(&domain, LABEL)?)
    }

    /// Runs `renod --detach` with no stdin or stdout and its stderr appended to
    /// `renod.stderr` in the log directory. It makes its own session.
    pub fn spawn_detached(&self) -> Result<Child, LauncherError> {
        let renod = self.find_renod()?;
        let logs = self.logs();
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&logs.dir)
            .map_err(io_error(&logs.dir))?;
        let stderr = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&logs.stderr)
            .map_err(io_error(&logs.stderr))?;
        Command::new(&renod)
            .arg("--data-dir")
            .arg(&self.data_dir)
            .arg("--socket")
            .arg(&self.socket)
            .arg("--detach")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .spawn()
            .map_err(|source| LauncherError::Spawn { renod, source })
    }

    /// Writes the plist atomically (a temporary file in the same directory, then a rename), mode
    /// 0644, and returns where it went.
    pub fn write_agent(&self, agent: &LaunchAgent) -> Result<PathBuf, LauncherError> {
        let path = self.agent_path();
        std::fs::create_dir_all(&self.agents_dir).map_err(io_error(&self.agents_dir))?;
        let temporary = self.agents_dir.join(format!(".{}.tmp", agent::file_name()));
        std::fs::write(&temporary, agent.plist()).map_err(io_error(&temporary))?;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o644))
            .map_err(io_error(&temporary))?;
        std::fs::rename(&temporary, &path).map_err(io_error(&path))?;
        Ok(path)
    }

    /// Deletes the plist. `false` when there was none.
    pub fn remove_agent(&self) -> Result<bool, LauncherError> {
        let path = self.agent_path();
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(source) => Err(LauncherError::Io { path, source }),
        }
    }

    /// Whether launchd has the job loaded.
    pub fn is_loaded(&self) -> Result<bool, LauncherError> {
        Ok(self.launchctl.is_loaded(&self.domain(), LABEL)?)
    }

    /// Loads the job (`launchctl bootstrap`); with `RunAtLoad` it starts at once.
    pub fn load(&self) -> Result<(), LauncherError> {
        Ok(self
            .launchctl
            .bootstrap(&self.domain(), &self.agent_path())?)
    }

    /// Unloads the job (`launchctl bootout`), which stops the daemon with SIGTERM and waits for
    /// it. `false` when it was not loaded.
    pub fn unload(&self) -> Result<bool, LauncherError> {
        let domain = self.domain();
        if !self.launchctl.is_loaded(&domain, LABEL)? {
            return Ok(false);
        }
        self.launchctl.bootout(&domain, LABEL)?;
        Ok(true)
    }
}

/// `renod` in the first of `dirs` that holds an executable file of that name.
pub fn find_in(dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter()
        .map(|dir| dir.join("renod"))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fx {
        _root: tempfile::TempDir,
        launcher: Launcher,
        launchctl: Arc<FakeLaunchctl>,
        data: PathBuf,
    }

    fn fx(launchctl: FakeLaunchctl) -> Fx {
        let root = tempfile::tempdir().expect("tempdir");
        let data = root.path().join("data");
        let launchctl = Arc::new(launchctl);
        let renod = root.path().join("renod");
        std::fs::write(&renod, "#!/bin/sh\nexit 0\n").expect("renod");
        std::fs::set_permissions(&renod, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let launcher = Launcher::new(
            data.clone(),
            data.join("reno.sock"),
            root.path().join("LaunchAgents"),
            launchctl.clone(),
        )
        .with_renod(renod);
        Fx {
            _root: root,
            launcher,
            launchctl,
            data,
        }
    }

    #[test]
    fn renod_is_looked_for_in_order_and_must_be_executable() {
        let first = tempfile::tempdir().expect("tempdir");
        let second = tempfile::tempdir().expect("tempdir");
        let dirs = vec![first.path().to_path_buf(), second.path().to_path_buf()];
        assert_eq!(find_in(&dirs), None);
        std::fs::write(first.path().join("renod"), b"#!/bin/sh\n").expect("write");
        assert_eq!(find_in(&dirs), None, "not executable");
        let later = second.path().join("renod");
        std::fs::write(&later, b"#!/bin/sh\n").expect("write");
        std::fs::set_permissions(&later, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert_eq!(find_in(&dirs), Some(later));
        let earlier = first.path().join("renod");
        std::fs::set_permissions(&earlier, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert_eq!(find_in(&dirs), Some(earlier));
    }

    #[test]
    fn without_an_agent_start_spawns_and_never_touches_launchctl() {
        let fx = fx(FakeLaunchctl::new());
        assert!(!fx.launcher.is_managed());
        let started = fx.launcher.start().expect("start");
        let Started::Spawned(mut child) = started else {
            panic!("expected a spawn");
        };
        child.wait().expect("wait");
        assert!(fx.launchctl.calls().is_empty());
        assert!(
            fx.launcher.logs().stderr.is_file(),
            "stderr goes to its file"
        );
    }

    #[test]
    fn with_an_agent_for_this_data_dir_start_defers_to_launchd_and_never_spawns() {
        let fx = fx(FakeLaunchctl::loaded());
        let agent = fx.launcher.agent().expect("agent");
        let path = fx.launcher.write_agent(&agent).expect("write");
        assert_eq!(
            std::fs::metadata(&path).expect("meta").permissions().mode() & 0o777,
            0o644
        );
        assert!(fx.launcher.is_managed());
        assert!(matches!(
            fx.launcher.start().expect("start"),
            Started::Launchd
        ));
        assert_eq!(
            fx.launchctl.calls(),
            [format!(
                "kickstart gui/{}/dev.reno.renod",
                rustix::process::getuid().as_raw()
            )]
        );
        assert!(!fx.launcher.logs().stderr.exists(), "nothing was spawned");
    }

    #[test]
    fn an_agent_that_is_not_loaded_is_bootstrapped_first() {
        let fx = fx(FakeLaunchctl::new());
        let agent = fx.launcher.agent().expect("agent");
        let path = fx.launcher.write_agent(&agent).expect("write");
        fx.launcher.start().expect("start");
        let uid = rustix::process::getuid().as_raw();
        assert_eq!(
            fx.launchctl.calls(),
            [
                format!("bootstrap gui/{uid} {}", path.display()),
                format!("kickstart gui/{uid}/dev.reno.renod"),
            ]
        );
    }

    #[test]
    fn an_agent_for_another_data_dir_or_someone_elses_file_is_not_ours() {
        let fx = fx(FakeLaunchctl::loaded());
        let elsewhere = Launcher::new(
            fx.data.join("other"),
            fx.data.join("other/reno.sock"),
            fx.launcher.agents_dir.clone(),
            fx.launchctl.clone(),
        )
        .with_renod(fx.launcher.find_renod().expect("renod"));
        let agent = elsewhere.agent().expect("agent");
        elsewhere.write_agent(&agent).expect("write");
        assert!(elsewhere.is_managed());
        assert!(!fx.launcher.is_managed(), "another data dir");

        std::fs::write(fx.launcher.agent_path(), "not a plist").expect("write");
        assert!(!fx.launcher.is_managed(), "a foreign file");
        assert!(matches!(
            fx.launcher.start().expect("start"),
            Started::Spawned(_)
        ));
    }

    #[test]
    fn unload_boots_out_only_what_is_loaded_and_remove_reports_whether_there_was_a_file() {
        let fx = fx(FakeLaunchctl::loaded());
        assert!(fx.launcher.unload().expect("unload"));
        assert!(!fx.launcher.unload().expect("again"));
        assert_eq!(fx.launchctl.calls().len(), 1, "one bootout, no second one");
        assert!(!fx.launcher.remove_agent().expect("remove"));
        let agent = fx.launcher.agent().expect("agent");
        fx.launcher.write_agent(&agent).expect("write");
        assert!(fx.launcher.remove_agent().expect("remove"));
        assert!(!fx.launcher.agent_path().exists());
    }
}
