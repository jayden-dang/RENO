//! What the kernel knows about its executors: the last probe of each, shown to clients and used
//! to refuse a run before anything is spawned.
//!
//! The probing itself is the `Executor` port's (`Executor::probe`: no model call, bounded, no
//! install). The kernel only decides **when**:
//!
//! - at start, when the daemon asks ([`Kernel::probe_executors_in_background`]);
//! - on `executor.refresh` ([`Kernel::refresh_executors`]);
//! - when an executor refused a run as not installed or not signed in, and
//! - when the stored health says an executor is ineligible for a run that wants to start: a
//!   fresh probe confirms it before the run is refused, so a login made since the last probe
//!   is noticed at the moment it matters.
//!
//! # How deep
//!
//! A probe is [`ProbeDepth::Full`] when a person asked for it (`executor.refresh`, and the check
//! before a run they started is refused) and [`ProbeDepth::Quiet`] when nobody did (start-up,
//! the probe a refused run queues, the probes around maintenance jobs and plan usage). A quiet
//! probe of a registry ACP agent starts nothing, since some agents touch the desktop in their
//! handshake; the kernel lays the stored sign-in and models over it while the installed version
//! is unchanged.
//!
//! Nothing probes on a timer (ARCH-10). At most [`PROBE_CONCURRENCY`] probes run at once, each
//! under [`PROBE_DEADLINE`], and probes of one executor never overlap (ARCH-11).
//!
//! # What is shown, and what decides
//!
//! The board starts from the health stored by the previous daemon, so a client sees something
//! at once after a restart. That health is **not** used to refuse a run: only a probe made by
//! this process is `live`. An executor nobody has probed yet is eligible; its run will fail
//! with the executor's own error, which starts a probe.
//!
//! A probe that cannot finish in time keeps the previous health (or, with none, records
//! `ProbeFailed`, which is not a reason to refuse a run).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use futures::future::join_all;
use reno_harness::exec::ProbeDepth;
use reno_proto::{
    ExecutorFrameCounts, ExecutorHealth, ExecutorId, ExecutorInfo, HealthProbe, InstallStatus,
    ModelCatalog, NotInstalledReason, PlanUsage, SignedIn, UnknownFrameKind,
};
use tokio::sync::{Semaphore, watch};

use crate::error::{KernelError, Result};
use crate::shared::Shared;

/// Probes that may run at once.
pub(crate) const PROBE_CONCURRENCY: usize = 4;

/// The longest one probe may take before it is given up on. Every driver bounds its own commands
/// well inside this; it is the backstop for one that does not.
pub(crate) const PROBE_DEADLINE: Duration = Duration::from_secs(45);

/// A run that wants to start on an executor the stored probe calls ineligible asks for a fresh
/// probe, unless the stored one is younger than this: a flood of runs against a signed-out
/// executor is one probe, not one each.
pub(crate) const RECHECK_FLOOR_MS: i64 = 5_000;

/// Plan usage is read from the provider, so it is read at most this often per executor, whoever
/// asks (zeron's `FORCE_MIN_INTERVAL`).
pub(crate) const USAGE_MIN_INTERVAL_MS: i64 = 30_000;

/// How long the last good reading stays shown when the next ones are unavailable (a rate limit
/// or a blip must not blank the meters); its own `fetched_at_ms` tells how old it is.
pub(crate) const USAGE_KEEP_MS: i64 = 30 * 60_000;

/// The longest one usage read may take.
pub(crate) const USAGE_DEADLINE: Duration = Duration::from_secs(20);

/// What the board knows of one executor's plan usage. Memory only: it is not stored, since a
/// restarted daemon would show an old meter as current.
#[derive(Default)]
struct UsageEntry {
    last: Option<PlanUsage>,
    /// When a read was last started, successful or not.
    attempted_ms: Option<i64>,
}

struct Entry {
    health: ExecutorHealth,
    /// Probed by this process; only a live entry decides eligibility.
    live: bool,
    /// The last refusal a run met, with when (ms): the executor's own word, newer than any probe
    /// that began before it.
    refusal: Option<(i64, Refusal)>,
}

/// What an executor itself said when it refused to start a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Refusal {
    SignedOut { hint: String },
    NotInstalled { detail: String },
}

/// Lays a refusal over `found` when it was seen at or after the probe began (`probe_started_ms`):
/// then it is the newer fact. A probe that began later knows better and is left alone.
pub(crate) fn lay_refusal_over(
    found: &mut ExecutorHealth,
    refusal: Option<(i64, &Refusal)>,
    probe_started_ms: i64,
) {
    let Some((at, refusal)) = refusal else { return };
    if at < probe_started_ms {
        return;
    }
    apply_refusal(found, refusal);
}

fn apply_refusal(health: &mut ExecutorHealth, refusal: &Refusal) {
    match refusal {
        Refusal::SignedOut { hint } => {
            health.signed_in = SignedIn::No { hint: hint.clone() };
        }
        Refusal::NotInstalled { detail } => {
            health.installed = InstallStatus::NotInstalled {
                why: NotInstalledReason::NotFound {
                    hint: detail.clone(),
                },
            };
        }
    }
}

/// The board of executor health.
pub(crate) struct HealthBoard {
    entries: Mutex<HashMap<ExecutorId, Entry>>,
    /// The list clients see, replaced on every change.
    tx: watch::Sender<Arc<Vec<ExecutorInfo>>>,
    permits: Semaphore,
    /// One gate per executor, so its probes never overlap.
    gates: Mutex<HashMap<ExecutorId, Arc<Gate>>>,
    /// Executors with a background probe queued, so a burst of failures is one probe.
    queued: Mutex<HashSet<ExecutorId>>,
    /// The unrecognised frame kinds seen so far, per executor (T1.14).
    frames: Mutex<HashMap<ExecutorId, FrameBook>>,
    /// The plan usage of each executor, laid over its health when the list is published.
    usage: Mutex<HashMap<ExecutorId, UsageEntry>>,
}

/// When each unrecognised frame kind of one executor was first seen under one CLI version. The
/// kinds are bounded by the executor's own counter, so this is too (ARCH-11).
#[derive(Default)]
struct FrameBook {
    version: Option<String>,
    /// The executor's cumulative counts when `version` was adopted: what the CLI before it sent.
    baseline: BTreeMap<String, u64>,
    first_seen_ms: BTreeMap<String, u64>,
}

impl HealthBoard {
    pub(crate) fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            tx: watch::channel(Arc::default()).0,
            permits: Semaphore::new(PROBE_CONCURRENCY),
            gates: Mutex::new(HashMap::new()),
            queued: Mutex::new(HashSet::new()),
            frames: Mutex::new(HashMap::new()),
            usage: Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn subscribe(&self) -> watch::Receiver<Arc<Vec<ExecutorInfo>>> {
        self.tx.subscribe()
    }

    pub(crate) fn list(&self) -> Vec<ExecutorInfo> {
        self.tx.borrow().as_ref().clone()
    }

    fn entries(&self) -> MutexGuard<'_, HashMap<ExecutorId, Entry>> {
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The health a probe of this process found for `id`.
    fn live(&self, id: &ExecutorId) -> Option<ExecutorHealth> {
        self.entries()
            .get(id)
            .filter(|entry| entry.live)
            .map(|entry| entry.health.clone())
    }

    fn usage_map(&self) -> MutexGuard<'_, HashMap<ExecutorId, UsageEntry>> {
        self.usage.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn gate_of(&self, id: &ExecutorId) -> Arc<Gate> {
        Arc::clone(
            self.gates
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(id.clone())
                .or_default(),
        )
    }
}

/// Serialises the probes of one executor and counts how many finished.
#[derive(Default)]
struct Gate {
    turn: tokio::sync::Mutex<()>,
    finished: AtomicU64,
}

impl std::fmt::Debug for HealthBoard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HealthBoard").finish_non_exhaustive()
    }
}

/// Lays the stored health over what a quiet probe could not see. A quiet probe of a registry
/// agent says `Unknown` for sign-in and gives the curated models; when the stored health is of
/// the same installed version, its sign-in verdict and models stand, so the page does not drop
/// to "unknown" after every restart. A new version, or nothing stored, leaves `Unknown`.
fn keep_what_a_quiet_probe_cannot_see(found: &mut ExecutorHealth, stored: Option<&ExecutorHealth>) {
    let Some(stored) = stored else { return };
    if !matches!(found.signed_in, SignedIn::Unknown) {
        return;
    }
    let (Some(now), Some(before)) = (found.installed.version(), stored.installed.version()) else {
        return;
    };
    if now != before {
        return;
    }
    found.signed_in = stored.signed_in.clone();
    found.models = stored.models.clone();
}

/// How a probe request relates to one already finished or running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Probing {
    /// Always probe: a person asked.
    Fresh,
    /// A probe that finished while this one waited its turn is good enough.
    Shared,
}

impl Shared {
    /// Publishes the list of every registered executor with the health the board holds.
    pub(crate) fn publish_executors(&self) {
        let health: HashMap<ExecutorId, ExecutorHealth> = self
            .health
            .entries()
            .iter()
            .map(|(id, entry)| (id.clone(), entry.health.clone()))
            .collect();
        let mut health = health;
        for (id, usage) in self.health.usage_map().iter() {
            if let Some(health) = health.get_mut(id) {
                health.plan_usage.clone_from(&usage.last);
            }
        }
        let mut ids: Vec<ExecutorId> = self.executors.ids();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        let list = ids
            .iter()
            .filter_map(|id| {
                let executor = self.executors.get(id)?;
                let mut descriptor = executor.descriptor();
                let health = health.get(id).cloned();
                // The models shown are the ones the health labels: the live list when a probe
                // got one, else the curated one.
                if let Some(found) = &health
                    && !found.models.models.is_empty()
                {
                    descriptor.caps.models.clone_from(&found.models.models);
                }
                let mut info = ExecutorInfo {
                    descriptor,
                    health,
                    maintenance: None,
                    job: None,
                    source: executor.source(),
                    auth_methods: executor.auth_methods(),
                };
                if let Some(maintainer) = self.maint.get() {
                    maintainer.decorate(self, &mut info);
                }
                Some(info)
            })
            .collect();
        self.health.tx.send_replace(Arc::new(list));
    }

    /// Forgets what the board holds for an executor that was removed: its health, probe gate,
    /// queued probe, frame counts and usage, so one added again starts from nothing.
    pub(crate) fn purge_executor(&self, id: &ExecutorId) {
        self.health.entries().remove(id);
        self.health
            .gates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
        self.health
            .queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
        self.health
            .frames
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(id);
        self.health.usage_map().remove(id);
    }

    /// Loads what the previous daemon stored, for display only, then publishes.
    pub(crate) async fn load_executor_health(&self) -> Result<()> {
        let stored = self.db.executor_health().await?;
        {
            let mut entries = self.health.entries();
            for (id, health) in stored {
                if self.executors.get(&id).is_some() {
                    entries.insert(
                        id,
                        Entry {
                            health,
                            live: false,
                            refusal: None,
                        },
                    );
                }
            }
        }
        self.publish_executors();
        Ok(())
    }

    /// Probes `id` and publishes the result. `Err` only for an executor that is not registered.
    pub(crate) async fn probe_executor(
        &self,
        id: &ExecutorId,
        how: Probing,
        depth: ProbeDepth,
    ) -> Result<()> {
        let executor = self
            .executors
            .get(id)
            .ok_or_else(|| KernelError::ExecutorNotFound(id.clone()))?;
        let gate = self.health.gate_of(id);
        let finished_before = gate.finished.load(Ordering::SeqCst);
        let _turn = gate.turn.lock().await;
        if how == Probing::Shared && gate.finished.load(Ordering::SeqCst) != finished_before {
            // A probe finished while this one waited for its turn.
            return Ok(());
        }
        let Ok(_permit) = self.health.permits.acquire().await else {
            return Ok(());
        };
        let started_ms = self.db.now_ms();
        let probe = tokio::select! {
            biased;
            () = self.root.cancelled() => return Err(KernelError::ShuttingDown),
            probe = tokio::time::timeout(PROBE_DEADLINE, executor.probe_at(depth)) => probe,
        };
        let checked_at = u64::try_from(self.db.now_ms()).unwrap_or(0);
        let health = match probe {
            Ok(probe) => {
                let mut health = ExecutorHealth::from_probe(probe, checked_at);
                if depth == ProbeDepth::Quiet {
                    let stored = self.any_health(id);
                    keep_what_a_quiet_probe_cannot_see(&mut health, stored.as_ref());
                }
                health
            }
            Err(_elapsed) => {
                tracing::warn!(executor = %id, "the executor probe did not finish in time");
                gate.finished.fetch_add(1, Ordering::SeqCst);
                if self.health.entries().contains_key(id) {
                    // Nothing new is known: what was known stays, as old as it is.
                    return Ok(());
                }
                let detail = format!(
                    "the probe did not finish within {} s",
                    PROBE_DEADLINE.as_secs()
                );
                ExecutorHealth::from_probe(
                    HealthProbe::not_installed(
                        NotInstalledReason::ProbeFailed { detail },
                        ModelCatalog::curated(executor.descriptor().caps.models),
                    ),
                    checked_at,
                )
            }
        };
        if let Err(error) = self.db.put_executor_health(id, &health).await {
            tracing::error!(executor = %id, %error, "could not store the executor health");
        }
        {
            let mut entries = self.health.entries();
            let refusal = entries.get(id).and_then(|entry| entry.refusal.clone());
            let mut health = health;
            lay_refusal_over(
                &mut health,
                refusal.as_ref().map(|(at, r)| (*at, r)),
                started_ms,
            );
            entries.insert(
                id.clone(),
                Entry {
                    health,
                    live: true,
                    refusal,
                },
            );
        }
        gate.finished.fetch_add(1, Ordering::SeqCst);
        self.publish_executors();
        // What the health says decides which executors the maintenance task looks at.
        if let Some(maintainer) = self.maint.get() {
            maintainer.wake();
        }
        Ok(())
    }

    /// Probes `ids` (every registered executor when `None`), each at most [`PROBE_CONCURRENCY`]
    /// at a time, and waits for all of them.
    pub(crate) async fn probe_executors(
        &self,
        ids: Option<&ExecutorId>,
        how: Probing,
        depth: ProbeDepth,
    ) -> Result<()> {
        let ids: Vec<ExecutorId> = match ids {
            Some(id) => vec![id.clone()],
            None => self.executors.ids(),
        };
        for outcome in join_all(ids.iter().map(|id| self.probe_executor(id, how, depth))).await {
            outcome?;
        }
        Ok(())
    }

    /// Records what an executor itself said when it refused a run (not signed in, not
    /// installed): the answer is known, so the stored health says it now, stamped now, with no
    /// probe. A probe that began before this keeps it; one that begins later may overturn it.
    /// Nothing is recorded for an executor with no health yet: its probe will find out.
    pub(crate) async fn note_refusal(&self, id: &ExecutorId, refusal: Refusal) {
        let now = self.db.now_ms();
        let health = {
            let mut entries = self.health.entries();
            let Some(entry) = entries.get_mut(id) else {
                return;
            };
            apply_refusal(&mut entry.health, &refusal);
            entry.health.checked_at_ms = u64::try_from(now).unwrap_or(0);
            entry.live = true;
            entry.refusal = Some((now, refusal));
            entry.health.clone()
        };
        if let Err(error) = self.db.put_executor_health(id, &health).await {
            tracing::error!(executor = %id, %error, "could not store the executor health");
        }
        self.publish_executors();
    }

    /// Asks for a probe of `id` in the background, unless one is already queued for it. Used
    /// when an executor itself said it is not installed or not signed in.
    pub(crate) fn probe_soon(self: &Arc<Self>, id: &ExecutorId) {
        if self.executors.get(id).is_none() {
            return;
        }
        let newly = self
            .health
            .queued
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(id.clone());
        if !newly {
            return;
        }
        let shared = Arc::clone(self);
        let id = id.clone();
        self.spawn(async move {
            // Nobody asked for this probe, so it stays quiet.
            let outcome = shared
                .probe_executor(&id, Probing::Shared, ProbeDepth::Quiet)
                .await;
            shared
                .health
                .queued
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&id);
            if let Err(error) = outcome {
                tracing::debug!(executor = %id, %error, "background executor probe ended");
            }
        });
    }

    /// The health that decides whether a run may start on `id`. `None` when this process has not
    /// probed it. When the stored probe says the executor is ineligible, a fresh one confirms
    /// that first (unless the stored one is younger than [`RECHECK_FLOOR_MS`]), so a fix made
    /// since is noticed now and not at the next restart.
    pub(crate) async fn health_for_dispatch(&self, id: &ExecutorId) -> Option<ExecutorHealth> {
        let stored = self.health.live(id)?;
        if stored.ineligible().is_none() {
            return Some(stored);
        }
        let age = self
            .db
            .now_ms()
            .saturating_sub(i64::try_from(stored.checked_at_ms).unwrap_or(0));
        // A run is about to start on this executor, so it is spawned anyway: the probe is full.
        if age >= RECHECK_FLOOR_MS
            && self
                .probe_executor(id, Probing::Shared, ProbeDepth::Full)
                .await
                .is_ok()
        {
            return self.health.live(id);
        }
        Some(stored)
    }

    /// Looks at the frames `id`'s wire layer has not recognised so far and logs a warning for
    /// every kind not seen before under the CLI version of the executor's last probe. Called
    /// when a run ends and when the counts are asked for; costs a map lookup and does nothing
    /// when nothing new arrived.
    pub(crate) fn note_unknown_frames(&self, id: &ExecutorId) {
        let _ = self.unknown_frames_of(id);
    }

    /// What `executor.frames` answers for `id`: its counters, with when each kind was first
    /// seen. `None` for an executor that is not registered.
    fn unknown_frames_of(&self, id: &ExecutorId) -> Option<ExecutorFrameCounts> {
        let executor = self.executors.get(id)?;
        let counts = executor.unknown_frames();
        let version = self
            .health
            .entries()
            .get(id)
            .and_then(|entry| entry.health.installed.version().map(str::to_owned));
        let now = u64::try_from(self.db.now_ms()).unwrap_or(0);
        let mut books = self
            .health
            .frames
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let book = books.entry(id.clone()).or_default();
        match (&book.version, &version) {
            (Some(old), Some(new)) if old != new => {
                // A new CLI version: the executor's counters are cumulative, so what is already
                // counted came from the old one. Only what rises from here is the new one's.
                book.version = version;
                book.baseline.clone_from(&counts);
                book.first_seen_ms.clear();
            }
            // The first probe names the version of everything counted so far.
            (None, Some(_)) => book.version = version,
            _ => {}
        }
        let mut frames = Vec::with_capacity(counts.len());
        for (kind, total) in counts {
            let count = total.saturating_sub(book.baseline.get(&kind).copied().unwrap_or(0));
            if count == 0 {
                continue;
            }
            let first_seen_ms = *book.first_seen_ms.entry(kind.clone()).or_insert_with(|| {
                tracing::warn!(
                    executor = %id,
                    cli_version = book.version.as_deref().unwrap_or("unknown"),
                    kind = %kind,
                    "a frame kind this driver does not recognise: the CLI may have moved"
                );
                now
            });
            frames.push(UnknownFrameKind {
                kind,
                count,
                first_seen_ms,
            });
        }
        Some(ExecutorFrameCounts {
            id: id.clone(),
            cli_version: book.version.clone(),
            frames,
        })
    }

    /// The unrecognised frames of `id`, or of every executor by id.
    pub(crate) fn executor_frames(
        &self,
        id: Option<&ExecutorId>,
    ) -> Result<Vec<ExecutorFrameCounts>> {
        let mut ids: Vec<ExecutorId> = match id {
            Some(id) => vec![id.clone()],
            None => self.executors.ids(),
        };
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        ids.iter()
            .map(|id| {
                self.unknown_frames_of(id)
                    .ok_or_else(|| KernelError::ExecutorNotFound(id.clone()))
            })
            .collect()
    }

    /// The health the board holds for `id` (this process's probe, or the previous daemon's).
    pub(crate) fn any_health(&self, id: &ExecutorId) -> Option<ExecutorHealth> {
        self.health
            .entries()
            .get(id)
            .map(|entry| entry.health.clone())
    }

    /// `id` as the last published list shows it.
    pub(crate) fn published(&self, id: &ExecutorId) -> Option<ExecutorInfo> {
        self.health
            .tx
            .borrow()
            .iter()
            .find(|info| &info.descriptor.id == id)
            .cloned()
    }

    /// The health a probe of this process found for `id`, or `None` when it has not probed it.
    pub(crate) fn live_health(&self, id: &ExecutorId) -> Option<ExecutorHealth> {
        self.health.live(id)
    }

    /// Reads the plan usage of `ids` (every executor when `None`) from the executors' own
    /// sources and publishes it with their health. Executors that are not installed or are
    /// signed out are not asked, and no executor is asked twice within
    /// [`USAGE_MIN_INTERVAL_MS`]. An executor with no health yet is probed first. `Err` only for
    /// an id that is not registered.
    pub(crate) async fn refresh_plan_usage(&self, ids: Option<&ExecutorId>) -> Result<()> {
        let ids: Vec<ExecutorId> = match ids {
            Some(id) => vec![id.clone()],
            None => self.executors.ids(),
        };
        for id in &ids {
            if self.executors.get(id).is_none() {
                return Err(KernelError::ExecutorNotFound(id.clone()));
            }
            if self.any_health(id).is_none() {
                // Reading usage is not a request to start the agent: a quiet probe is enough to
                // know whether it is installed.
                self.probe_executor(id, Probing::Shared, ProbeDepth::Quiet)
                    .await?;
            }
        }
        let changed = join_all(ids.iter().map(|id| self.read_plan_usage(id))).await;
        if changed.into_iter().any(|changed| changed) {
            self.publish_executors();
        }
        Ok(())
    }

    /// One executor's usage read; whether what is shown changed.
    async fn read_plan_usage(&self, id: &ExecutorId) -> bool {
        let Some(executor) = self.executors.get(id) else {
            return false;
        };
        let asked = self.any_health(id).is_some_and(|health| {
            matches!(
                health.installed,
                reno_proto::InstallStatus::Installed { .. }
            ) && !matches!(health.signed_in, reno_proto::SignedIn::No { .. })
        });
        if !asked {
            return false;
        }
        let now = self.db.now_ms();
        {
            let mut usage = self.health.usage_map();
            let entry = usage.entry(id.clone()).or_default();
            if entry
                .attempted_ms
                .is_some_and(|at| now.saturating_sub(at) < USAGE_MIN_INTERVAL_MS)
            {
                return false;
            }
            entry.attempted_ms = Some(now);
        }
        let reading = tokio::select! {
            biased;
            () = self.root.cancelled() => return false,
            reading = tokio::time::timeout(USAGE_DEADLINE, executor.plan_usage()) => reading.ok().flatten(),
        };
        let fetched = self.db.now_ms();
        let mut usage = self.health.usage_map();
        let entry = usage.entry(id.clone()).or_default();
        let next = match reading {
            Some(reading) if !reading.windows.is_empty() => Some(PlanUsage {
                plan_label: reading.plan_label,
                windows: reading.windows,
                fetched_at_ms: u64::try_from(fetched).unwrap_or(0),
            }),
            _ => entry.last.clone().filter(|last| {
                fetched.saturating_sub(i64::try_from(last.fetched_at_ms).unwrap_or(0))
                    < USAGE_KEEP_MS
            }),
        };
        let changed = entry.last != next;
        entry.last = next;
        changed
    }
}
