//! The ACP registry as the daemon serves it: the list, add, remove, own agents and per-agent
//! environment (`executor.registry.*`, feature `executor.registry`), and the kernel's port to the
//! index (`RegistryPort`) that updates registry agents.
//!
//! The index URL is given to the daemon (`DaemonConfig::registry_index_url`); with none the
//! daemon neither offers the feature nor reaches the network, so a test daemon never touches the
//! CDN. The index is fetched when a client asks (cached for five minutes, with its ETag), never on
//! a timer. Adding and removing an agent act on the person's behalf; nothing here starts an agent
//! (a quiet probe never spawns one, and only a person's add, check or sign-in does).

use std::sync::Arc;

use futures::StreamExt;
use futures::future::BoxFuture;
use reno_harness::acp_registry::{self as reg, Entry, INDEX_TTL, Readiness};
use reno_harness::{AcpExecutor, HarnessError, RegistryAgent};
use reno_kernel::{InstallWork, Kernel, RegistryPort};
use reno_proto::{
    ErrorCode, ErrorObject, ExecutorId, ExecutorJobResult, ExecutorRegistryAddOwnParams,
    ExecutorRegistryEnvResult, ExecutorRegistryEnvSetParams, ExecutorRegistryListParams,
    ExecutorRegistryListResult, ExecutorRegistryParams, ExecutorRegistryRemoveResult, JobPhase,
    RegistryEntry,
};

use crate::errors::{invalid, kernel_error};

/// How many icons are fetched at once when the list has to fill its cache.
const ICON_FETCHES: usize = 8;

/// The registry as one daemon has it.
#[derive(Debug, Clone)]
pub(crate) struct Registry {
    /// Where the index is fetched from; `None`: the daemon has no registry.
    index_url: Option<String>,
}

type Reply<T> = Result<T, ErrorObject>;

fn unavailable(message: impl Into<String>) -> ErrorObject {
    ErrorObject::new(ErrorCode::Unavailable, message)
}

fn conflict(message: impl Into<String>) -> ErrorObject {
    ErrorObject::new(ErrorCode::Conflict, message)
}

/// The registry's agent for `id` (`gemini`, `own.mine`), or why it is not one.
fn agent_of(id: &str) -> Reply<RegistryAgent> {
    RegistryAgent::new(id).ok_or_else(|| invalid(format!("{id:?} is not an agent id")))
}

fn executor_of(id: &str) -> Reply<ExecutorId> {
    format!("acp.{id}")
        .parse()
        .map_err(|_| invalid(format!("{id:?} is not an agent id")))
}

fn harness_error(error: &HarnessError) -> ErrorObject {
    match error {
        HarnessError::Install(_) | HarnessError::NotInstalled(_) => invalid(error.to_string()),
        other => ErrorObject::new(ErrorCode::Internal, other.to_string()),
    }
}

/// What the device lacks to install `entry`, as the sentence a person acts on.
pub(crate) fn needs(entry: &Entry, readiness: Readiness) -> Option<String> {
    let fix = reg::fix_for(readiness);
    match readiness {
        Readiness::Ready => None,
        Readiness::NeedsNode => Some(format!(
            "{} needs Node.js: {}",
            entry.name,
            fix.unwrap_or("install Node.js")
        )),
        Readiness::NeedsUv => Some(format!(
            "{} needs uv: {}",
            entry.name,
            fix.unwrap_or("install uv")
        )),
        Readiness::NoBuild => Some(format!("{} has no build for this Mac", entry.name)),
    }
}

impl Registry {
    pub(crate) fn new(index_url: Option<String>) -> Self {
        Self { index_url }
    }

    /// The daemon has a registry: it offers feature `executor.registry`.
    pub(crate) fn configured(&self) -> bool {
        self.index_url.is_some()
    }

    fn url(&self) -> Reply<&str> {
        self.index_url
            .as_deref()
            .ok_or_else(|| unavailable("this daemon has no ACP registry configured"))
    }

    async fn index(&self, refresh: bool) -> Reply<reg::LoadedIndex> {
        reg::load_index(self.url()?, refresh, INDEX_TTL)
            .await
            .map_err(unavailable)
    }
}

/// The sign-in ways an agent's own handshake offered, as the list shows them: its `authenticate`
/// methods together, each terminal command, each key. Methods Reno cannot use are left out.
pub(crate) fn probed_sign_in(
    methods: &[reno_proto::AuthMethod],
) -> Vec<reno_proto::RegistrySignInOption> {
    use reno_proto::{AuthMethodKind, RegistrySignInOption};
    let mut ways = Vec::new();
    let browser: Vec<String> = methods
        .iter()
        .filter(|m| m.kind == AuthMethodKind::Agent && m.unusable.is_none())
        .map(|m| m.id.clone())
        .collect();
    if !browser.is_empty() {
        ways.push(RegistrySignInOption::Browser { methods: browser });
    }
    for method in methods.iter().filter(|m| m.unusable.is_none()) {
        match method.kind {
            AuthMethodKind::Terminal => ways.push(RegistrySignInOption::Terminal {
                command: method
                    .command
                    .clone()
                    .unwrap_or_else(|| method.name.clone()),
            }),
            AuthMethodKind::EnvVar if !method.vars.is_empty() => {
                ways.push(RegistrySignInOption::Env {
                    vars: method.vars.clone(),
                });
            }
            _ => {}
        }
    }
    ways
}

/// `executor.registry.list`.
pub(crate) async fn list(
    registry: &Registry,
    kernel: &Kernel,
    p: ExecutorRegistryListParams,
) -> Reply<ExecutorRegistryListResult> {
    let loaded = registry.index(p.refresh).await?;
    let records = reg::records();
    let platform = reg::platform();
    let (node, uv) = reg::tools();
    let running: Vec<ExecutorId> = kernel
        .executors()
        .into_iter()
        .filter(|info| {
            info.job
                .as_ref()
                .is_some_and(|job| !matches!(job.phase, JobPhase::Failed { .. }))
        })
        .map(|info| info.descriptor.id)
        .collect();
    // Icons: cached beside the index, fetched (a few at a time) only when missing.
    let icons: Vec<Option<String>> = futures::stream::iter(loaded.entries.iter())
        .map(reg::icon_of)
        .buffered(ICON_FETCHES)
        .collect()
        .await;
    let mut entries: Vec<RegistryEntry> = loaded
        .entries
        .iter()
        .zip(icons)
        .filter_map(|(entry, icon)| {
            let record = records.iter().find(|r| r.id == entry.id);
            reg::entry_view(entry, record, icon, platform, node, uv)
        })
        .collect();
    entries.sort_by_key(|e| e.name.to_ascii_lowercase());
    // Added here and not in the index: delisted, or your own.
    for record in &records {
        if !loaded.entries.iter().any(|e| e.id == record.id)
            && let Some(view) = reg::record_view(record)
        {
            entries.push(view);
        }
    }
    let live = kernel.executors();
    for entry in &mut entries {
        entry.adding = running.contains(&entry.executor);
        // An added agent that has had a full probe says how it signs in itself: that wins over
        // what the audit found.
        let offered = live
            .iter()
            .find(|info| info.descriptor.id == entry.executor)
            .map(|info| probed_sign_in(&info.auth_methods))
            .filter(|ways| !ways.is_empty());
        if let Some(ways) = offered {
            entry.sign_in = ways;
            entry.sign_in_from_probe = true;
            entry.checked_version = None;
        }
    }
    Ok(ExecutorRegistryListResult {
        entries,
        fetched_at_ms: loaded.fetched_at_ms,
        offline: loaded.offline,
        error: loaded.error,
    })
}

/// `executor.registry.add`: the agent's install as a job the person started. An agent that is
/// already added at an older version is updated; one that is current has nothing to do.
pub(crate) async fn add(
    registry: &Registry,
    kernel: &Kernel,
    p: ExecutorRegistryParams,
) -> Reply<ExecutorJobResult> {
    if p.id.starts_with(reg::OWN_PREFIX) {
        return Err(invalid(
            "an agent of your own is added with its command, not from the registry",
        ));
    }
    let agent = agent_of(&p.id)?;
    let loaded = registry.index(false).await?;
    let entry = loaded
        .entries
        .iter()
        .find(|e| e.id == p.id)
        .ok_or_else(|| {
            ErrorObject::new(
                ErrorCode::NotFound,
                format!("no agent {:?} in the registry", p.id),
            )
        })?;
    if let Some(harness) = reg::built_in(&entry.id) {
        return Err(ErrorObject::new(
            ErrorCode::NothingToDo,
            format!(
                "{} is built in: use {}",
                entry.name,
                reg::built_in_executor(harness)
            ),
        ));
    }
    if let Some(message) = needs(entry, reg::readiness(entry)) {
        return Err(conflict(message));
    }
    let executor = AcpExecutor::registry(agent, &entry.name)
        .map_err(|e| invalid(format!("{:?} cannot be an executor id: {e}", p.id)))?;
    tracing::info!(agent = %p.id, "registry add requested");
    let info = kernel
        .add_registry_executor(Arc::new(executor))
        .await
        .map_err(|e| kernel_error(&e))?;
    Ok(ExecutorJobResult { executor: info })
}

/// `executor.registry.remove`: unregistered first (refused while a run is queued or live), then the
/// install is deleted.
pub(crate) async fn remove(
    kernel: &Kernel,
    p: ExecutorRegistryParams,
) -> Reply<ExecutorRegistryRemoveResult> {
    let agent = agent_of(&p.id)?;
    let executor = executor_of(&p.id)?;
    tracing::info!(agent = %p.id, "registry remove requested");
    match kernel.remove_registry_executor(&executor).await {
        Ok(()) => {}
        // An agent that is added on disk but not registered (an add whose failure was dismissed
        // after the install had landed): it can still be removed.
        Err(reno_kernel::KernelError::ExecutorNotFound(_)) if reg::record(agent).is_some() => {}
        Err(error) => return Err(kernel_error(&error)),
    }
    tokio::task::spawn_blocking(move || reg::remove(agent))
        .await
        .map_err(|e| ErrorObject::new(ErrorCode::Internal, e.to_string()))?
        .map_err(|e| harness_error(&e))?;
    Ok(ExecutorRegistryRemoveResult { executor })
}

/// `executor.registry.add_own`: an agent of your own, registered and probed (a person added it,
/// so the probe starts it).
pub(crate) async fn add_own(
    kernel: &Kernel,
    p: ExecutorRegistryAddOwnParams,
) -> Reply<ExecutorJobResult> {
    let (name, command, env) = (p.name.clone(), p.command.clone(), p.env.clone());
    let record = tokio::task::spawn_blocking(move || reg::add_own(&name, &command, &env))
        .await
        .map_err(|e| ErrorObject::new(ErrorCode::Internal, e.to_string()))?
        .map_err(|e| harness_error(&e))?;
    let agent = agent_of(&record.id)?;
    let registered = AcpExecutor::registry(agent, &record.name)
        .map_err(|e| invalid(format!("cannot be an executor id: {e}")))
        .map(Arc::new);
    let outcome = match registered {
        Ok(executor) => kernel
            .register_executor(executor, true)
            .await
            .map_err(|e| kernel_error(&e)),
        Err(error) => Err(error),
    };
    match outcome {
        Ok(info) => {
            tracing::info!(agent = %record.id, "own agent added");
            Ok(ExecutorJobResult { executor: info })
        }
        Err(error) => {
            // Not registered: the record would be an agent nobody can see.
            let _ = reg::remove(agent);
            Err(error)
        }
    }
}

/// An added agent's record, or `NotFound`.
fn added(id: &str) -> Reply<RegistryAgent> {
    let agent = agent_of(id)?;
    reg::record(agent).ok_or_else(|| {
        ErrorObject::new(
            ErrorCode::NotFound,
            format!("{id} is not added on this device"),
        )
    })?;
    Ok(agent)
}

/// `executor.registry.env_get`. The values go to the caller (the daemon's socket is owner-only)
/// and nowhere else: they are never logged.
pub(crate) async fn env_get(p: ExecutorRegistryParams) -> Reply<ExecutorRegistryEnvResult> {
    let agent = added(&p.id)?;
    let env = reg::env_text(&reg::user_env(agent));
    Ok(ExecutorRegistryEnvResult { id: p.id, env })
}

/// `executor.registry.env_set`: replaces the agent's own environment. A line that is not
/// `KEY=VALUE` is refused by its number, never repeated.
pub(crate) async fn env_set(p: ExecutorRegistryEnvSetParams) -> Reply<ExecutorRegistryEnvResult> {
    let agent = added(&p.id)?;
    tracing::info!(agent = %p.id, "registry env set");
    let text = p.env;
    let env = tokio::task::spawn_blocking(move || reg::set_user_env(agent, &text))
        .await
        .map_err(|e| ErrorObject::new(ErrorCode::Internal, e.to_string()))?
        .map_err(|e| harness_error(&e))?;
    Ok(ExecutorRegistryEnvResult {
        id: p.id,
        env: reg::env_text(&env),
    })
}

impl RegistryPort for Registry {
    fn latest<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Option<String>> {
        Box::pin(async move {
            let loaded = self.index(false).await.ok()?;
            loaded
                .entries
                .into_iter()
                .find(|e| e.id == id)
                .map(|e| e.version)
        })
    }

    fn install_work<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Option<InstallWork>> {
        Box::pin(async move {
            let loaded = self.index(false).await.ok()?;
            let entry = loaded.entries.into_iter().find(|e| e.id == id)?;
            let work: InstallWork = Arc::new(move || {
                let entry = entry.clone();
                Box::pin(async move {
                    let icon = reg::icon_of(&entry).await;
                    reg::install(&entry, icon).await.map(drop)
                })
            });
            Some(work)
        })
    }
}
