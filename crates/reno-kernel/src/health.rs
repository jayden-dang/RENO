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
use reno_proto::{
    ExecutorFrameCounts, ExecutorHealth, ExecutorId, ExecutorInfo, HealthProbe, ModelCatalog,
    NotInstalledReason, UnknownFrameKind,
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

struct Entry {
    health: ExecutorHealth,
    /// Probed by this process; only a live entry decides eligibility.
    live: bool,
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
        let mut ids: Vec<&ExecutorId> = self.executors.ids().collect();
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        let list = ids
            .into_iter()
            .filter_map(|id| {
                let mut descriptor = self.executors.get(id)?.descriptor();
                let health = health.get(id).cloned();
                // The models shown are the ones the health labels: the live list when a probe
                // got one, else the curated one.
                if let Some(found) = &health
                    && !found.models.models.is_empty()
                {
                    descriptor.caps.models.clone_from(&found.models.models);
                }
                Some(ExecutorInfo { descriptor, health })
            })
            .collect();
        self.health.tx.send_replace(Arc::new(list));
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
                        },
                    );
                }
            }
        }
        self.publish_executors();
        Ok(())
    }

    /// Probes `id` and publishes the result. `Err` only for an executor that is not registered.
    pub(crate) async fn probe_executor(&self, id: &ExecutorId, how: Probing) -> Result<()> {
        let executor = self
            .executors
            .get(id)
            .cloned()
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
        let probe = tokio::select! {
            biased;
            () = self.root.cancelled() => return Err(KernelError::ShuttingDown),
            probe = tokio::time::timeout(PROBE_DEADLINE, executor.probe()) => probe,
        };
        let checked_at = u64::try_from(self.db.now_ms()).unwrap_or(0);
        let health = match probe {
            Ok(probe) => ExecutorHealth::from_probe(probe, checked_at),
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
        self.health
            .entries()
            .insert(id.clone(), Entry { health, live: true });
        gate.finished.fetch_add(1, Ordering::SeqCst);
        self.publish_executors();
        Ok(())
    }

    /// Probes `ids` (every registered executor when `None`), each at most [`PROBE_CONCURRENCY`]
    /// at a time, and waits for all of them.
    pub(crate) async fn probe_executors(
        &self,
        ids: Option<&ExecutorId>,
        how: Probing,
    ) -> Result<()> {
        let ids: Vec<ExecutorId> = match ids {
            Some(id) => vec![id.clone()],
            None => self.executors.ids().cloned().collect(),
        };
        for outcome in join_all(ids.iter().map(|id| self.probe_executor(id, how))).await {
            outcome?;
        }
        Ok(())
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
            let outcome = shared.probe_executor(&id, Probing::Shared).await;
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
        if age >= RECHECK_FLOOR_MS && self.probe_executor(id, Probing::Shared).await.is_ok() {
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
            None => self.executors.ids().cloned().collect(),
        };
        ids.sort_by(|a, b| a.as_str().cmp(b.as_str()));
        ids.iter()
            .map(|id| {
                self.unknown_frames_of(id)
                    .ok_or_else(|| KernelError::ExecutorNotFound(id.clone()))
            })
            .collect()
    }

    /// The health a probe of this process found for `id`, or `None` when it has not probed it.
    pub(crate) fn live_health(&self, id: &ExecutorId) -> Option<ExecutorHealth> {
        self.health.live(id)
    }
}
