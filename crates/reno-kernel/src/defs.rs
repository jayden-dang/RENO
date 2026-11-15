//! The definitions state: the `Definitions` port, the validated set the kernel holds, and the
//! task that keeps it current (ARCHITECTURE 3.1, 3.2, 9; ARCH-7).
//!
//! # The port
//!
//! [`Definitions`] is how the kernel reaches `~/.reno`. It does two things: hand over a
//! **snapshot** of the definition files (their names and text, read raw), and say when
//! something may have changed. The kernel does the rest: it parses and validates the snapshot
//! (`reno_proto::defs`, pure) and stores versions through `reno-store`. The port reads files;
//! it never decides what is valid.
//!
//! # The flow of a load
//!
//! 1. Start: the watch begins, then the files are read, in that order, so an edit between the
//!    two is caught by the watch. Until the first load the kernel holds the **last good set**:
//!    the active versions in the store.
//! 2. Each signal from the watch: drain the signals that piled up, read a new snapshot, and
//!    validate it as a whole.
//! 3. **Valid**: compare each definition's hash with the store's active version. Anything new,
//!    changed or gone is applied in one transaction (new versions, retirements, one event
//!    each); then, and only then, the new set is published. An unchanged snapshot costs no
//!    store write at all.
//! 4. **Invalid**, or unreadable: nothing is stored. The previous set stays published, with the
//!    errors beside it, until the files validate again.
//!
//! Publishing after storing means a version a run pins is always in the store (the store checks
//! the pin too).

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use reno_proto::defs::{
    DefError, DefFile, DefKind, DefSet, DefSummary, DefsSnapshot, ValidationContext, load_bodies,
    load_set,
};
use reno_store::{DefinitionRecord, NewDefinition};
use tokio::sync::{mpsc, watch};

use crate::error::{KernelError, Result};
use crate::shared::Shared;
use crate::watch::Watch;

/// The principal named on definition events that come from files.
const FILE_PRINCIPAL: &str = "definitions:files";

/// What a read of the definitions directory found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DefSnapshot {
    /// The definition files that could be read.
    pub files: Vec<DefFile>,
    /// Files that could not be read (not UTF-8, unreadable, too large to read) or that the
    /// layout does not allow. Each is an error the set cannot be valid with.
    pub problems: Vec<DefError>,
}

/// The `Definitions` port: the definitions repository, implemented outside the kernel.
#[async_trait]
pub trait Definitions: Send + Sync + std::fmt::Debug {
    /// Reads every definition file now.
    async fn snapshot(&self) -> Result<DefSnapshot>;

    /// Starts watching and returns the signals. Each message means "something may have
    /// changed": the kernel reads a fresh snapshot. The port debounces and coalesces, so the
    /// channel is small and bounded; the kernel drains it before each read. The receiver ends
    /// when the port stops watching.
    fn watch(&self) -> Result<mpsc::Receiver<()>>;
}

/// What the kernel needs to load definitions.
#[derive(Debug, Clone)]
pub struct DefsSetup {
    /// Where the files come from.
    pub port: Arc<dyn Definitions>,
    /// What validation may assume about the world: the executors a bot may name.
    pub context: ValidationContext,
    /// How `fs` triggers watch files. Without it they cannot fire, and are shown as failed.
    pub watch: Option<Arc<dyn Watch>>,
}

impl DefsSetup {
    /// Definitions from `port`, with no file watching for triggers.
    pub fn new(port: Arc<dyn Definitions>, context: ValidationContext) -> Self {
        Self {
            port,
            context,
            watch: None,
        }
    }

    /// The same, with `watch` serving the `fs` triggers.
    #[must_use]
    pub fn with_watch(mut self, watch: Arc<dyn Watch>) -> Self {
        self.watch = Some(watch);
        self
    }
}

/// One stored version of a definition that is part of the current set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredVersion {
    /// The version, from 1.
    pub version: u32,
    /// The content hash.
    pub hash: String,
    /// When this version was first stored, in milliseconds since the epoch.
    pub created_at_ms: u64,
}

/// The definitions in force, and the problems with the files, as last published.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CurrentDefs {
    set: DefSet,
    versions: BTreeMap<(DefKind, String), StoredVersion>,
    errors: Vec<DefError>,
}

impl CurrentDefs {
    /// The validated definitions in force.
    pub fn set(&self) -> &DefSet {
        &self.set
    }

    /// Why the files on disk are not in force; empty when they are.
    pub fn errors(&self) -> &[DefError] {
        &self.errors
    }

    /// The stored version of a definition in the set.
    pub fn version_of(&self, kind: DefKind, name: &str) -> Option<&StoredVersion> {
        self.versions.get(&(kind, name.to_owned()))
    }

    /// The set as the wire shows it, optionally of one kind.
    pub fn snapshot(&self, kind: Option<DefKind>) -> DefsSnapshot {
        let defs = self
            .set
            .entries()
            .into_iter()
            .filter(|e| kind.is_none_or(|k| k == e.kind))
            .filter_map(|e| {
                let stored = self.versions.get(&(e.kind, e.name.to_owned()))?;
                Some(DefSummary {
                    kind: e.kind,
                    name: e.name.to_owned(),
                    version: stored.version,
                    hash: stored.hash.clone(),
                    created_at_ms: stored.created_at_ms,
                })
            })
            .collect();
        DefsSnapshot {
            defs,
            errors: self.errors.clone(),
        }
    }

    fn with_errors(&self, errors: Vec<DefError>) -> Self {
        Self {
            errors,
            ..self.clone()
        }
    }
}

type Versions = BTreeMap<(DefKind, String), StoredVersion>;

fn versions_of(records: &[DefinitionRecord]) -> Versions {
    records
        .iter()
        .map(|r| {
            (
                (r.kind, r.name.clone()),
                StoredVersion {
                    version: r.version,
                    hash: r.hash.clone(),
                    created_at_ms: u64::try_from(r.created_at).unwrap_or(0),
                },
            )
        })
        .collect()
}

/// The error shown when the machinery around the files fails, not the files themselves.
fn machinery_error(message: String) -> DefError {
    DefError::new("(definitions)", "", message)
}

/// Owns the loading of definitions. One task runs [`Loader::run`]; nothing else touches `known`.
struct Loader {
    shared: Arc<Shared>,
    port: Arc<dyn Definitions>,
    ctx: ValidationContext,
    /// The active versions in the store: what a snapshot is compared with.
    known: Versions,
}

/// Starts loading definitions: restores the last good set from the store, begins watching, does
/// the first load, and spawns the task that follows the watch. Returns after the first load.
pub(crate) async fn start(shared: &Arc<Shared>, setup: DefsSetup) -> Result<()> {
    let DefsSetup {
        port,
        context,
        watch: _,
    } = setup;
    let stored = shared.db.current_definitions().await?;
    let known = versions_of(&stored);

    // The last good set: what the store says is active, if it still validates.
    let bodies: Vec<_> = stored
        .into_iter()
        .map(|r| (r.kind, r.name, r.body))
        .collect();
    match load_bodies(&bodies, &context) {
        Ok(set) => {
            let versions = known
                .iter()
                .filter(|((kind, name), _)| in_set(&set, *kind, name))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
            shared.defs_tx.send_replace(Arc::new(CurrentDefs {
                set,
                versions,
                errors: Vec::new(),
            }));
        }
        Err(errors) => {
            for error in &errors {
                tracing::warn!(%error, "the stored definitions no longer validate");
            }
        }
    }

    // Watch first, then read: an edit between the two is caught by the watch.
    let changes = match port.watch() {
        Ok(changes) => Some(changes),
        Err(error) => {
            tracing::warn!(%error, "definitions are not being watched; edits need a restart");
            None
        }
    };
    let mut loader = Loader {
        shared: Arc::clone(shared),
        port,
        ctx: context,
        known,
    };
    loader.reload().await;
    if let Some(changes) = changes {
        shared.spawn(loader.run(changes));
    }
    Ok(())
}

fn in_set(set: &DefSet, kind: DefKind, name: &str) -> bool {
    match kind {
        DefKind::Bot => set.bots().contains_key(name),
        DefKind::Trigger => set.triggers().contains_key(name),
        DefKind::Policy => set.policy().is_some(),
    }
}

impl Loader {
    async fn run(mut self, mut changes: mpsc::Receiver<()>) {
        loop {
            tokio::select! {
                biased;
                () = self.shared.root.cancelled() => break,
                signal = changes.recv() => {
                    if signal.is_none() {
                        tracing::warn!("the definitions watch ended; edits need a restart");
                        break;
                    }
                    // A burst is one reload: the snapshot read next sees all of it.
                    while changes.try_recv().is_ok() {}
                    self.reload().await;
                }
            }
        }
    }

    /// Reads a snapshot and either stores and publishes it, or publishes why it was refused.
    async fn reload(&mut self) {
        let snapshot = match self.port.snapshot().await {
            Ok(snapshot) => snapshot,
            Err(error) => {
                self.publish_errors(vec![machinery_error(format!(
                    "cannot read the definitions: {error}"
                ))]);
                return;
            }
        };
        let mut errors = snapshot.problems;
        match load_set(&snapshot.files, &self.ctx) {
            Ok(set) if errors.is_empty() => self.adopt(set).await,
            Ok(_) => self.publish_errors(errors),
            Err(found) => {
                errors.extend(found);
                self.publish_errors(errors);
            }
        }
    }

    /// Stores what changed in one transaction, then publishes the new set.
    async fn adopt(&mut self, set: DefSet) {
        let entries = set.entries();
        let unchanged = entries.len() == self.known.len()
            && entries.iter().all(|e| {
                self.known
                    .get(&(e.kind, e.name.to_owned()))
                    .is_some_and(|stored| stored.hash == e.hash)
            });
        if !unchanged {
            let desired: Vec<NewDefinition> = entries
                .iter()
                .map(|e| NewDefinition {
                    kind: e.kind,
                    name: e.name.to_owned(),
                    hash: e.hash.to_owned(),
                    body: e.body.clone(),
                })
                .collect();
            let stored = async {
                self.shared
                    .db
                    .apply_definitions(desired, FILE_PRINCIPAL)
                    .await?;
                self.shared.db.current_definitions().await
            }
            .await;
            match stored {
                Ok(records) => self.known = versions_of(&records),
                Err(error) => {
                    tracing::error!(%error, "could not store the definitions");
                    self.publish_errors(vec![machinery_error(format!(
                        "cannot store the definitions: {error}"
                    ))]);
                    return;
                }
            }
        }
        let versions: Versions = self
            .known
            .iter()
            .filter(|((kind, name), _)| in_set(&set, *kind, name))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let next = CurrentDefs {
            set,
            versions,
            errors: Vec::new(),
        };
        self.shared.defs_tx.send_if_modified(|current| {
            if **current == next {
                return false;
            }
            *current = Arc::new(next);
            true
        });
    }

    /// Keeps the set in force and shows `errors` beside it.
    fn publish_errors(&self, errors: Vec<DefError>) {
        for error in &errors {
            tracing::warn!(%error, "definitions not loaded; keeping the last good set");
        }
        self.shared.defs_tx.send_if_modified(|current| {
            if current.errors == errors {
                return false;
            }
            *current = Arc::new(current.with_errors(errors));
            true
        });
    }
}

impl Shared {
    /// The definitions in force.
    pub(crate) fn current_defs(&self) -> Arc<CurrentDefs> {
        self.defs_tx.borrow().clone()
    }
}

/// A receiver for the definitions in force.
pub(crate) fn subscribe(shared: &Shared) -> watch::Receiver<Arc<CurrentDefs>> {
    shared.defs_tx.subscribe()
}

/// The error for a definition that is not there.
pub(crate) fn not_found(kind: DefKind, name: &str, version: Option<u32>) -> KernelError {
    KernelError::DefinitionNotFound {
        kind,
        name: name.to_owned(),
        version,
    }
}
