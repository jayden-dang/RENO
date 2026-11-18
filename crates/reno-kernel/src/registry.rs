//! The executor registry: the `Executor` port as the kernel holds it.
//!
//! The set takes inserts and removes while the daemon runs (an agent added from the ACP
//! registry, or removed). It is a cheap handle: clones share one map. The lock is held only to
//! read or change the map, never across an `.await`, and `get` hands out an owned `Arc`, so a
//! removed executor stays alive for whoever already holds it (a run in flight keeps its
//! executor; nothing is orphaned by the map forgetting it).

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, PoisonError, RwLock};

use reno_harness::exec::Executor;
use reno_proto::ExecutorId;

/// Executors by id. Built by the daemon (and by tests), then handed to the kernel.
#[derive(Clone, Default)]
pub struct ExecutorRegistry {
    executors: Arc<RwLock<HashMap<ExecutorId, Arc<dyn Executor>>>>,
}

impl fmt::Debug for ExecutorRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut ids: Vec<String> = self.ids().iter().map(|id| id.to_string()).collect();
        ids.sort_unstable();
        f.debug_struct("ExecutorRegistry")
            .field("executors", &ids)
            .finish()
    }
}

impl ExecutorRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    fn read(&self) -> std::sync::RwLockReadGuard<'_, HashMap<ExecutorId, Arc<dyn Executor>>> {
        self.executors
            .read()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, HashMap<ExecutorId, Arc<dyn Executor>>> {
        self.executors
            .write()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Register `executor` under the id of its descriptor, replacing any earlier one.
    #[must_use]
    pub fn with(self, executor: Arc<dyn Executor>) -> Self {
        self.insert(executor.descriptor().id, executor);
        self
    }

    /// Register `executor` under `id`, replacing any earlier one. (The daemon's start-up and the
    /// tests; an executor added while the daemon runs goes through [`Self::insert_new`].)
    pub fn insert(&self, id: ExecutorId, executor: Arc<dyn Executor>) {
        self.write().insert(id, executor);
    }

    /// Register `executor` under `id` only if nothing is: the check and the insert are one step.
    /// `false` (and nothing changed) when `id` is taken, so an added agent never replaces an
    /// executor that is registered, whichever came first.
    pub fn insert_new(&self, id: ExecutorId, executor: Arc<dyn Executor>) -> bool {
        match self.write().entry(id) {
            std::collections::hash_map::Entry::Occupied(_) => false,
            std::collections::hash_map::Entry::Vacant(slot) => {
                slot.insert(executor);
                true
            }
        }
    }

    /// Forget `id`, returning its executor. Runs that already hold it keep it.
    pub fn remove(&self, id: &ExecutorId) -> Option<Arc<dyn Executor>> {
        self.write().remove(id)
    }

    /// The executor named `id`.
    pub fn get(&self, id: &ExecutorId) -> Option<Arc<dyn Executor>> {
        self.read().get(id).cloned()
    }

    /// The registered ids, in no particular order.
    pub fn ids(&self) -> Vec<ExecutorId> {
        self.read().keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use reno_harness::exec::MockExecutor;

    use super::*;

    fn mock(id: &str) -> Arc<dyn Executor> {
        Arc::new(MockExecutor::new(id.parse().unwrap()))
    }

    #[test]
    fn an_insert_of_a_taken_id_changes_nothing() {
        let registry = ExecutorRegistry::new().with(mock("codex"));
        let first = registry.get(&"codex".parse().unwrap()).unwrap();
        assert!(!registry.insert_new("codex".parse().unwrap(), mock("codex")));
        assert!(Arc::ptr_eq(
            &first,
            &registry.get(&"codex".parse().unwrap()).unwrap()
        ));
        assert!(registry.insert_new("acp.new".parse().unwrap(), mock("acp.new")));
    }

    #[test]
    fn clones_share_the_set_and_a_removed_executor_stays_alive_for_its_holder() {
        let registry = ExecutorRegistry::new().with(mock("acp.a"));
        let other = registry.clone();
        let held = other.get(&"acp.a".parse().unwrap()).unwrap();
        assert!(registry.remove(&"acp.a".parse().unwrap()).is_some());
        assert!(other.get(&"acp.a".parse().unwrap()).is_none());
        assert_eq!(held.descriptor().id.as_str(), "acp.a");
        assert!(other.ids().is_empty());
    }
}
