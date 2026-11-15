//! Start-up and shutdown of one daemon.
//!
//! # Start (a failure at any step undoes the earlier ones)
//!
//! 1. the data directory is made private (0700);
//! 2. the data-directory lock is taken, before anything else is opened;
//! 3. the executors are looked up, the store is opened, the definitions directory is created on
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
//! 2. `Kernel::shutdown`: no more claims, every live run is cancelled and ends `interrupted`
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

use reno_harness::exec::HarnessExecutor;
use reno_kernel::{DefsSetup, Kernel, KernelConfig, KernelError, SystemClock};
use reno_proto::defs::ValidationContext;
use reno_proto::{ExecutorId, IdError};
use reno_rpc::{RpcError, Server, UnixServer};
use reno_store::{Store, StoreError};
use tokio::task::JoinHandle;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

use crate::defs::{self, FsDefinitions, GitInit};
use crate::executors::{self, ExecutorSet};
use crate::handlers::{self, Ctx};
use crate::lock::{DataDirLock, LockError};
use crate::paths::{Paths, ensure_private_dir};

/// How long connections get, after the kernel has stopped, to finish what they are owed before
/// they are cancelled.
pub const CONNECTION_GRACE: Duration = Duration::from_millis(250);
/// How long the server gets, once its connections are cancelled, to stop.
pub const SERVER_DEADLINE: Duration = Duration::from_secs(5);

/// Where the daemon's executors come from.
#[derive(Debug)]
pub enum ExecutorSource {
    /// `mock`, plus each harness whose CLI is installed. Looked up after the lock is taken, so a
    /// second daemon fails fast.
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
        }
    }
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
        } = config;

        for dir in [paths.data_dir.clone(), paths.workspace()] {
            ensure_private_dir(&dir).map_err(|source| DaemonError::Dir { path: dir, source })?;
        }
        let lock = DataDirLock::acquire(&paths.lock())?;

        let executors = match executors {
            ExecutorSource::Discover {
                hold_mock,
                harnesses,
            } => executors::discover(hold_mock, harnesses).await?,
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
        let mut ids: Vec<ExecutorId> = registry.ids().cloned().collect();
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
        let fs_defs =
            Arc::new(FsDefinitions::open(&paths.defs_dir, defs_debounce).map_err(defs_error)?);

        kernel.owner = format!("renod-{}", std::process::id());
        let setup = DefsSetup {
            port: fs_defs.clone(),
            context,
        };
        let kernel = match Kernel::start_with_definitions(
            kernel,
            store,
            registry,
            Arc::new(SystemClock),
            setup,
        )
        .await
        {
            Ok(kernel) => kernel,
            Err(error) => {
                fs_defs.shutdown().await;
                return Err(error.into());
            }
        };

        let stop = CancellationToken::new();
        let ctx = Arc::new(Ctx {
            kernel: kernel.clone(),
            started: Instant::now(),
            executors: ids,
            default_cwd: paths.workspace(),
            stop: stop.clone(),
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
            harnesses,
            socket: paths.socket,
            lock,
            connection_grace,
            server_deadline,
        }));
        Ok(Self { socket, stop, task })
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
    harnesses: Vec<Arc<HarnessExecutor>>,
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

    // 2. The kernel ends every live run `interrupted`, within its own deadline.
    let report = s.kernel.shutdown().await;
    s.fs_defs.shutdown().await;
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

fn log_serve_end(joined: Result<Result<(), RpcError>, tokio::task::JoinError>) {
    match joined {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "the server ended with an error"),
        Err(error) => tracing::warn!(%error, "the server task failed"),
    }
}
