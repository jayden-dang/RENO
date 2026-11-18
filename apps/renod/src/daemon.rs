//! Start-up and shutdown of one daemon.
//!
//! # Start (a failure at any step undoes the earlier ones)
//!
//! 1. the data directory is made private (0700);
//! 2. the data-directory lock is taken, before anything else is opened;
//! 3. the worktrees directory is made (with the data directory's privacy);
//!    the executors are built, the store is opened, the definitions directory is created on
//!    first start, and the kernel starts: it recovers the last run's leftovers, then loads the
//!    definitions;
//! 4. the socket is bound and the server starts answering.
//!
//! # Shutdown
//!
//! Begins when [`Daemon::request_shutdown`] is called, which is what a signal or the
//! `daemon.shutdown` method does:
//!
//! 1. stop accepting: the listener is dropped, so the socket file goes and nobody new connects.
//!    Connections already open stay up;
//! 2. the wake notifications are unregistered; `Kernel::shutdown`: no more claims, every live run is cancelled and ends `interrupted`
//!    (subscribers see its `Done`), within the kernel's own deadline; then the definitions watch
//!    is stopped;
//! 3. connections get a short grace to finish what they are owed (a `daemon.shutdown` reply, a
//!    last event), then they are cancelled, and the server is awaited with a deadline;
//! 4. the socket file is removed if anything left it behind;
//! 5. the lock is released, last, after the store has been closed.

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reno_harness::exec::Executor;
use reno_kernel::{
    DefsSetup, Kernel, KernelConfig, KernelError, Limits, MaintenancePorts, RunLink, Secrets,
    SystemClock, WakeSignal,
};
use reno_proto::defs::ValidationContext;
use reno_proto::{ExecutorId, IdError};
use reno_rpc::{RpcError, Server, UnixServer};
use reno_store::{Store, StoreError};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::defs::{self, FsDefinitions, GitInit};
use crate::executors::{self, ExecutorSet};
use crate::fswatch::FsWatch;
use crate::handlers::{self, Ctx};
use crate::install::Installer;
use crate::link::{FsRunLink, resolve_reno};
use crate::lock::{DataDirLock, LockError};
use crate::paths::{Paths, ensure_private_dir};
use crate::power::{self, PowerWatch};
use crate::terminals::{self, PtySpawner};
use crate::watcher::{NotifyWatchers, WatcherFactory};
use crate::worktrees::GitWorkspace;

/// How long connections get, after the kernel has stopped, to finish what they are owed before
/// they are cancelled.
pub const CONNECTION_GRACE: Duration = Duration::from_millis(250);
/// How long the server gets, once its connections are cancelled, to stop.
pub const SERVER_DEADLINE: Duration = Duration::from_secs(5);

/// Where the daemon's executors come from.
#[derive(Debug)]
pub enum ExecutorSource {
    /// `mock`, plus every harness Reno can drive, installed or not; the kernel probes them once
    /// the daemon starts. Built after the lock is taken, so a second daemon fails fast.
    Discover {
        /// Use the mock's hold script: it waits to be steered or cancelled.
        hold_mock: bool,
        /// Look for the harness CLIs at all. Tests turn this off so that none is ever touched.
        harnesses: bool,
    },
    /// Exactly these, for tests.
    Given(ExecutorSet),
}

/// What a daemon needs to start.
#[derive(Debug)]
pub struct DaemonConfig {
    /// Where its files live.
    pub paths: Paths,
    /// The executors to register.
    pub executors: ExecutorSource,
    /// Kernel tunables. The daemon sets the lease owner itself.
    pub kernel: KernelConfig,
    /// See [`CONNECTION_GRACE`].
    pub connection_grace: Duration,
    /// See [`SERVER_DEADLINE`].
    pub server_deadline: Duration,
    /// How long the definition files must be quiet before an edit is loaded.
    pub defs_debounce: Duration,
    /// How `git init` is run on first start; tests replace it.
    pub git_init: GitInit,
    /// The `reno` binary the injected `reno mcp` server runs. `None` looks for `reno` next to
    /// this executable; with none found the daemon runs without Reno's MCP server and says so.
    pub reno_bin: Option<PathBuf>,
    /// Listen for the system waking from sleep and tell the scheduler. Off by default so that
    /// tests never register with the system; the binary turns it on.
    pub system_wake: bool,
    /// Where bots' secrets and the `secret.*` methods go. `None` starts the daemon without a
    /// secret store (tests): `secret.*` answers unavailable, and a bot that lists a secret
    /// cannot run. The binary sets the login Keychain.
    pub secrets: Option<Arc<dyn Secrets>>,
    /// How the platform file watchers (the definitions', and those of `fs` triggers) are
    /// started. The binary uses the platform's; tests may inject one that refuses or counts.
    pub watchers: Arc<dyn WatcherFactory>,
    /// What installs and updates of executors run through: the command runner and the
    /// self-updater switch. `None` (the default, so no test can reach a real command or file)
    /// leaves the daemon without feature `executor.maintain`; the binary sets the real ones
    /// ([`real_maintenance`]).
    pub maintenance: Option<MaintenancePorts>,
    /// What starts the terminals' shells. `None` (the default, so no test can open a real shell)
    /// leaves the daemon without feature `terminals`; the binary sets the real PTY
    /// ([`RealPty`](crate::terminals::RealPty)).
    pub pty: Option<Arc<dyn PtySpawner>>,
    /// How long terminal things take; tests shorten them.
    pub terminal_timing: terminals::Timing,
    /// The environment a terminal's shell starts from (minus `RENO_*`). The binary passes its
    /// own; the default is empty, so a test's shell inherits nothing of the test runner's.
    pub terminal_env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
}

impl DaemonConfig {
    /// The defaults for `paths` and `executors`.
    pub fn new(paths: Paths, executors: ExecutorSource) -> Self {
        Self {
            paths,
            executors,
            kernel: KernelConfig::default(),
            connection_grace: CONNECTION_GRACE,
            server_deadline: SERVER_DEADLINE,
            defs_debounce: defs::DEFAULT_DEBOUNCE,
            git_init: defs::git_init,
            reno_bin: None,
            system_wake: false,
            secrets: None,
            watchers: Arc::new(NotifyWatchers),
            maintenance: None,
            pty: None,
            terminal_timing: terminals::Timing::default(),
            terminal_env: Vec::new(),
        }
    }

    /// The same, installing and updating executors through `ports`.
    #[must_use]
    pub fn with_maintenance(mut self, ports: MaintenancePorts) -> Self {
        self.maintenance = Some(ports);
        self
    }

    /// The same, keeping secrets in `secrets`.
    #[must_use]
    pub fn with_secrets(mut self, secrets: Arc<dyn Secrets>) -> Self {
        self.secrets = Some(secrets);
        self
    }
}

/// The real ports for installs and updates: commands through the person's login shell, and the
/// self-updater files below their home directory. `None` when there is no login shell or no home
/// directory (the daemon then runs without feature `executor.maintain`).
pub fn real_maintenance() -> Option<MaintenancePorts> {
    let runner = reno_harness::proc::shell_runner::ShellRunner::login()?;
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)?;
    Some(MaintenancePorts {
        runner: Arc::new(runner),
        self_update: Arc::new(reno_harness::proc::self_update::SelfUpdateFiles::in_home(
            &home,
        )),
        limits: Limits::default(),
        check_at_start: true,
    })
}

/// Everything that can stop a daemon from starting, or end it badly.
#[derive(Debug, thiserror::Error)]
pub enum DaemonError {
    /// Another daemon holds the data directory, or the lock file is unusable.
    #[error(transparent)]
    Lock(#[from] LockError),
    /// A directory could not be prepared.
    #[error("cannot prepare {}: {source}", path.display())]
    Dir {
        /// The directory.
        path: PathBuf,
        /// What failed.
        source: io::Error,
    },
    /// The store could not be opened.
    #[error("cannot open the store: {0}")]
    Store(#[from] StoreError),
    /// The kernel could not start.
    #[error("cannot start the kernel: {0}")]
    Kernel(#[from] KernelError),
    /// The definitions directory could not be created or opened.
    #[error("cannot use the definitions directory {}: {source}", path.display())]
    Definitions {
        /// The directory.
        path: PathBuf,
        /// What failed.
        source: io::Error,
    },
    /// The socket could not be bound.
    #[error("cannot serve on the socket: {0}")]
    Socket(#[from] RpcError),
    /// A built-in id is malformed (a bug).
    #[error(transparent)]
    Id(#[from] IdError),
    /// A task of the daemon panicked or was cancelled.
    #[error("a daemon task failed: {0}")]
    Task(String),
}

/// How a daemon ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stopped {
    /// The kernel's shutdown deadline passed with runs still live; they were dropped where they
    /// stood, and the next start's recovery interrupts them.
    pub kernel_aborted: bool,
    /// The server did not stop in time and was dropped.
    pub server_aborted: bool,
}

/// A running daemon.
#[derive(Debug)]
#[must_use = "a daemon runs until it is asked to stop and waited for"]
pub struct Daemon {
    socket: PathBuf,
    stop: CancellationToken,
    task: JoinHandle<Stopped>,
    /// Dropping the daemon without waiting for it asks it to shut down, so its watchers and tasks
    /// do not outlive their owner.
    _shutdown_on_drop: tokio_util::sync::DropGuard,
}

impl Daemon {
    /// Starts a daemon. Returns once the socket is bound and answering.
    pub async fn start(config: DaemonConfig) -> Result<Self, DaemonError> {
        let DaemonConfig {
            paths,
            executors,
            mut kernel,
            connection_grace,
            server_deadline,
            defs_debounce,
            git_init,
            system_wake,
            secrets,
            reno_bin,
            watchers,
            maintenance,
            pty,
            terminal_timing,
            terminal_env,
        } = config;

        for dir in [paths.data_dir.clone(), paths.workspace(), paths.worktrees()] {
            ensure_private_dir(&dir).map_err(|source| DaemonError::Dir { path: dir, source })?;
        }
        let lock = DataDirLock::acquire(&paths.lock())?;

        let executors = match executors {
            ExecutorSource::Discover {
                hold_mock,
                harnesses,
            } => executors::discover(hold_mock, harnesses)?,
            ExecutorSource::Given(set) => set,
        };
        let db = paths.db();
        let store = tokio::task::spawn_blocking(move || Store::open(db))
            .await
            .map_err(|e| DaemonError::Task(e.to_string()))??;

        let ExecutorSet {
            registry,
            harnesses,
        } = executors;
        let stop = CancellationToken::new();
        let installer = Installer::new(&registry, stop.clone());
        let installs = installer.tasks();
        let mut ids: Vec<ExecutorId> = registry.ids();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));

        // Everything Reno can build is a name a definition may use, installed or not.
        let context = ValidationContext::with_builtin(ids.iter().cloned());
        let defs_error = |source| DaemonError::Definitions {
            path: paths.defs_dir.clone(),
            source,
        };
        defs::bootstrap(&paths.defs_dir, git_init)
            .await
            .map_err(defs_error)?;
        let fs_defs = Arc::new(
            FsDefinitions::open(&paths.defs_dir, defs_debounce)
                .map_err(defs_error)?
                .with_watchers(Arc::clone(&watchers))
                .with_retry(kernel.watch_retry),
        );

        kernel.owner = format!("renod-{}", std::process::id());
        if kernel.default_executors.is_empty() {
            kernel.default_executors = default_executors(&ids);
        }
        // Resolved once, so that it is comparable with the resolved working directories that
        // `task.run` hands the kernel (a symlinked data directory must still count as scratch).
        let scratch = tokio::fs::canonicalize(paths.workspace())
            .await
            .map_err(|source| DaemonError::Dir {
                path: paths.workspace(),
                source,
            })?;
        kernel.trigger_cwd.clone_from(&scratch);
        kernel.scratch_root = Some(scratch.clone());
        // What a trigger's `cwd = "~/..."` is expanded against.
        kernel.home = std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from);
        let files = crate::files::Files::new(Arc::clone(&watchers), kernel.watch_retry);
        let terminals = terminals::Terminals::new(pty, terminal_env, terminal_timing);
        let fs_watch = Arc::new(FsWatch::with_watchers(watchers));
        let wake = WakeSignal::new();
        let mut setup = DefsSetup::new(fs_defs.clone(), context)
            .with_watch(fs_watch.clone())
            .with_wake(wake.clone());
        if let Some(secrets) = &secrets {
            setup = setup.with_secrets(Arc::clone(secrets));
        }
        let link: Option<Arc<dyn RunLink>> = match resolve_reno(reno_bin) {
            Some(reno) => Some(Arc::new(FsRunLink::new(
                paths.runs(),
                paths.socket.clone(),
                reno,
            ))),
            None => {
                tracing::warn!(
                    "no `reno` binary next to renod: runs start without Reno's MCP server"
                );
                None
            }
        };
        let kernel = match Kernel::start_linked(
            kernel,
            store,
            registry,
            Arc::new(SystemClock),
            Some(setup),
            Some(Arc::new(
                GitWorkspace::new(paths.worktrees()).with_terminals(terminals.clone()),
            )),
            link,
        )
        .await
        {
            Ok(kernel) => kernel,
            Err(error) => {
                fs_defs.shutdown().await;
                return Err(error.into());
            }
        };

        let can_maintain = maintenance.is_some();
        if let Some(ports) = maintenance
            && let Err(error) = kernel.attach_maintenance(ports).await
        {
            kernel.shutdown().await;
            fs_defs.shutdown().await;
            return Err(error.into());
        }
        // The daemon's start is a definite signal to find out what each executor can do. The
        // probes run on the kernel's own tasks; what they find reaches clients as it lands.
        kernel.probe_executors_in_background();
        // After the kernel, so the scheduler is listening before the first notification.
        let power = if system_wake {
            match power::watch(wake) {
                Ok(watch) => Some(watch),
                Err(error) => {
                    tracing::warn!(%error, "triggers due while the Mac sleeps will fire late");
                    None
                }
            }
        } else {
            None
        };

        let ctx = Arc::new(Ctx {
            kernel: kernel.clone(),
            installer,
            started: Instant::now(),
            executors: ids,
            default_cwd: scratch,
            stop: stop.clone(),
            secrets,
            fs_defs: Arc::clone(&fs_defs),
            maintenance: can_maintain,
            files,
            terminals: terminals.clone(),
        });
        let server = Arc::new(Server::new(handlers::router(&ctx)));
        drop(ctx);

        let listener = match UnixServer::bind(&paths.socket).await {
            Ok(listener) => listener,
            Err(error) => {
                kernel.shutdown().await;
                fs_defs.shutdown().await;
                return Err(error.into());
            }
        };
        tracing::info!(socket = %paths.socket.display(), pid = std::process::id(), "renod is serving");

        let stop_accepting = CancellationToken::new();
        let connections = CancellationToken::new();
        let serving = tokio::spawn({
            let (server, stop_accepting, connections) = (
                Arc::clone(&server),
                stop_accepting.clone(),
                connections.clone(),
            );
            async move {
                server
                    .serve_until(listener, stop_accepting, connections)
                    .await
            }
        });

        let socket = paths.socket.clone();
        let task = tokio::spawn(shutdown_sequence(Shutdown {
            stop: stop.clone(),
            stop_accepting,
            connections,
            serving,
            server,
            kernel,
            fs_defs,
            fs_watch,
            terminals,
            power,
            harnesses,
            installs,
            socket: paths.socket,
            lock,
            connection_grace,
            server_deadline,
        }));
        Ok(Self {
            socket,
            _shutdown_on_drop: stop.clone().drop_guard(),
            stop,
            task,
        })
    }

    /// The socket the daemon serves on.
    pub fn socket(&self) -> &std::path::Path {
        &self.socket
    }

    /// Cancelled when shutdown has been requested, by [`request_shutdown`](Self::request_shutdown),
    /// a signal handler the caller wired up, or the `daemon.shutdown` method.
    pub fn shutdown_requested(&self) -> &CancellationToken {
        &self.stop
    }

    /// Asks the daemon to begin its shutdown sequence. Idempotent.
    pub fn request_shutdown(&self) {
        self.stop.cancel();
    }

    /// Waits for the shutdown sequence to finish. It does not start one.
    pub async fn wait(self) -> Result<Stopped, DaemonError> {
        self.task
            .await
            .map_err(|e| DaemonError::Task(e.to_string()))
    }
}

struct Shutdown {
    stop: CancellationToken,
    stop_accepting: CancellationToken,
    connections: CancellationToken,
    serving: JoinHandle<Result<(), RpcError>>,
    server: Arc<Server>,
    kernel: Kernel,
    fs_defs: Arc<FsDefinitions>,
    fs_watch: Arc<FsWatch>,
    terminals: terminals::Terminals,
    power: Option<PowerWatch>,
    harnesses: Vec<Arc<dyn Executor>>,
    installs: TaskTracker,
    socket: PathBuf,
    lock: DataDirLock,
    connection_grace: Duration,
    server_deadline: Duration,
}

async fn shutdown_sequence(mut s: Shutdown) -> Stopped {
    s.stop.cancelled().await;
    tracing::info!("shutting down");

    // 1. Stop accepting. The listener drops inside `serve_until`; open connections stay.
    s.stop_accepting.cancel();

    // 2. Installs in flight end (they stop with the daemon), then the kernel ends every live run
    // `interrupted`, within its own deadline.
    s.installs.close();
    s.installs.wait().await;
    // The shells are the daemon's children: they end with it.
    s.terminals.shutdown().await;
    drop(s.power.take());
    let report = s.kernel.shutdown().await;
    s.fs_defs.shutdown().await;
    s.fs_watch.shutdown().await;
    for harness in &s.harnesses {
        harness.shutdown().await;
    }

    // 3. Connections get a moment to finish what they are owed, then are cancelled; the server
    // is awaited with a deadline.
    let server_aborted = match timeout(s.connection_grace, &mut s.serving).await {
        Ok(joined) => {
            log_serve_end(joined);
            false
        }
        Err(_) => {
            s.connections.cancel();
            match timeout(s.server_deadline, &mut s.serving).await {
                Ok(joined) => {
                    log_serve_end(joined);
                    false
                }
                Err(_) => {
                    tracing::warn!("the server did not stop in time; dropping it");
                    s.serving.abort();
                    true
                }
            }
        }
    };

    // 4. The socket file goes with the listener; remove it if something kept it.
    match std::fs::remove_file(&s.socket) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(%error, "could not remove the socket"),
    }

    // 5. Close everything that holds the store, then release the lock.
    let Shutdown {
        server,
        kernel,
        harnesses,
        lock,
        ..
    } = s;
    drop(server);
    drop(kernel);
    drop(harnesses);
    drop(lock);
    tracing::info!("stopped");

    Stopped {
        kernel_aborted: report.aborted,
        server_aborted,
    }
}

/// Where a task with no bot and no named executor may go: Claude Code, then Codex, when the
/// daemon has real harnesses; the mock only when it has nothing else (`RENO_NO_HARNESSES`, tests).
/// The mock is never a default beside real harnesses: it would "succeed" without doing anything.
fn default_executors(registered: &[ExecutorId]) -> Vec<ExecutorId> {
    let real: Vec<ExecutorId> = ["claude-code", "codex"]
        .iter()
        .filter_map(|name| registered.iter().find(|id| id.as_str() == *name))
        .cloned()
        .collect();
    if !real.is_empty() {
        return real;
    }
    registered
        .iter()
        .filter(|id| id.as_str() == "mock")
        .cloned()
        .collect()
}

fn log_serve_end(joined: Result<Result<(), RpcError>, tokio::task::JoinError>) {
    match joined {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "the server ended with an error"),
        Err(error) => tracing::warn!(%error, "the server task failed"),
    }
}
