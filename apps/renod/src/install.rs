//! `executor.install`: the one explicit step that fetches and verifies a harness's managed
//! adapter (a pinned npm package or a release archive). A run never installs; the probe says
//! `needs_install` and waits for this.
//!
//! The install runs as a task of the daemon, not of the request: a client that hangs up halfway
//! does not leave a half-done install with nobody to probe the result. The task ends early only
//! when the daemon is shutting down. At most one install of an executor runs at a time.
//!
//! The decision to install rests on a probe made now, not on the last stored one, and on that
//! probe saying `needs_install` exactly: an executor that is installed, or is missing for another
//! reason (`not_found`, `unusable`), has nothing this step can fix, and says so instead of running
//! an install that does nothing.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, PoisonError};

use reno_harness::HarnessError;
use reno_harness::exec::Executor;
use reno_kernel::{ExecutorRegistry, Kernel, KernelError};
use reno_proto::{ExecutorId, ExecutorInfo, InstallStatus, NotInstalledReason};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// Why an install did not happen or did not finish.
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    /// No such executor.
    #[error("executor {0} not found")]
    NotFound(ExecutorId),
    /// The executor does not need an install, or cannot be fixed by one.
    #[error("executor {id} has nothing to install: {state}")]
    NothingToInstall {
        /// The executor.
        id: ExecutorId,
        /// What its probe found, in words.
        state: String,
    },
    /// An install of this executor is running.
    #[error("an install of executor {0} is already running")]
    InProgress(ExecutorId),
    /// The install ran and failed (the network, the registry, a checksum); trying again may work.
    #[error("the install of executor {id} failed: {detail}")]
    Failed {
        /// The executor.
        id: ExecutorId,
        /// What the installer said.
        detail: String,
    },
    /// The installer itself broke (not the fetch).
    #[error("the install of executor {id} broke: {detail}")]
    Broken {
        /// The executor.
        id: ExecutorId,
        /// What happened.
        detail: String,
    },
    /// The daemon is stopping.
    #[error("the daemon is shutting down")]
    ShuttingDown,
    /// The kernel refused the probe.
    #[error(transparent)]
    Kernel(#[from] KernelError),
}

/// The executors that can be installed, and which installs are running.
pub(crate) struct Installer {
    executors: HashMap<ExecutorId, Arc<dyn Executor>>,
    running: Arc<Mutex<HashSet<ExecutorId>>>,
    tasks: TaskTracker,
    stop: CancellationToken,
}

impl std::fmt::Debug for Installer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Installer").finish_non_exhaustive()
    }
}

/// Holds an executor's slot in `running` until dropped.
struct Slot {
    running: Arc<Mutex<HashSet<ExecutorId>>>,
    id: ExecutorId,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.id);
    }
}

impl Installer {
    /// An installer for the executors in `registry`. Installs end early when `stop` is cancelled.
    pub(crate) fn new(registry: &ExecutorRegistry, stop: CancellationToken) -> Self {
        let executors = registry
            .ids()
            .filter_map(|id| Some((id.clone(), Arc::clone(registry.get(id)?))))
            .collect();
        Self {
            executors,
            running: Arc::default(),
            tasks: TaskTracker::new(),
            stop,
        }
    }

    /// The tasks of the installs, for the daemon to wait for at shutdown.
    pub(crate) fn tasks(&self) -> TaskTracker {
        self.tasks.clone()
    }

    fn claim(&self, id: &ExecutorId) -> Result<Slot, InstallError> {
        let fresh = self
            .running
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.clone());
        if fresh {
            Ok(Slot {
                running: Arc::clone(&self.running),
                id: id.clone(),
            })
        } else {
            Err(InstallError::InProgress(id.clone()))
        }
    }

    /// Installs `id`, probes it again and returns it as the probe found it.
    pub(crate) async fn install(
        &self,
        kernel: &Kernel,
        id: &ExecutorId,
    ) -> Result<ExecutorInfo, InstallError> {
        let executor = self
            .executors
            .get(id)
            .cloned()
            .ok_or_else(|| InstallError::NotFound(id.clone()))?;
        let slot = self.claim(id)?;
        // Decide on what the harness is now, not on what it was at the last probe.
        let listed = kernel.refresh_executors(Some(id)).await?;
        let found = listed
            .into_iter()
            .find(|info| &info.descriptor.id == id)
            .ok_or_else(|| InstallError::NotFound(id.clone()))?;
        if let Some(state) = nothing_to_install(&found) {
            return Err(InstallError::NothingToInstall {
                id: id.clone(),
                state,
            });
        }

        let (done, answer) = oneshot::channel();
        let kernel = kernel.clone();
        let stop = self.stop.clone();
        let id = id.clone();
        self.tasks.spawn(async move {
            let _slot = slot;
            tracing::info!(executor = %id, "installing the executor's adapter");
            let outcome = tokio::select! {
                biased;
                () = stop.cancelled() => Err(InstallError::ShuttingDown),
                installed = executor.install() => installed.map_err(|error| match error {
                    HarnessError::Install(detail) | HarnessError::Protocol(detail) => {
                        InstallError::Failed { id: id.clone(), detail }
                    }
                    other => InstallError::Broken { id: id.clone(), detail: other.to_string() },
                }),
            };
            // Whatever happened, the health shown should be what the harness is now.
            let probed = kernel.refresh_executors(Some(&id)).await;
            let result = match (outcome, probed) {
                (Err(error), _) => {
                    tracing::warn!(executor = %id, %error, "the install did not finish");
                    Err(error)
                }
                (Ok(()), Err(error)) => Err(error.into()),
                (Ok(()), Ok(list)) => list
                    .into_iter()
                    .find(|info| info.descriptor.id == id)
                    .ok_or(InstallError::NotFound(id.clone())),
            };
            // The caller may have gone away; the install and its probe stand either way.
            let _ = done.send(result);
        });
        answer.await.unwrap_or(Err(InstallError::ShuttingDown))
    }
}

/// `None` when the probe says an install is what is missing; else what it says, in words.
fn nothing_to_install(info: &ExecutorInfo) -> Option<String> {
    let Some(health) = &info.health else {
        return Some("it has not been probed yet".to_owned());
    };
    match &health.installed {
        InstallStatus::NotInstalled {
            why: NotInstalledReason::NeedsInstall { .. },
        } => None,
        InstallStatus::Installed { .. } => Some("it is installed".to_owned()),
        InstallStatus::NotInstalled {
            why: NotInstalledReason::NotFound { hint },
        } => Some(format!(
            "it is not installed and the daemon cannot fetch it: {hint}"
        )),
        InstallStatus::NotInstalled {
            why: NotInstalledReason::Unusable { detail },
        } => Some(format!("what is there cannot be launched: {detail}")),
        InstallStatus::NotInstalled {
            why: NotInstalledReason::ProbeFailed { detail },
        } => Some(format!("its probe failed: {detail}")),
    }
}
