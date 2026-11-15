//! The executor registry: the `Executor` port as the kernel holds it.

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use reno_harness::exec::Executor;
use reno_proto::ExecutorId;

/// Executors by id. Built by the daemon (and by tests), then handed to the kernel.
#[derive(Clone, Default)]
pub struct ExecutorRegistry {
    executors: HashMap<ExecutorId, Arc<dyn Executor>>,
}

impl fmt::Debug for ExecutorRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut ids: Vec<_> = self.executors.keys().map(ExecutorId::as_str).collect();
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

    /// Register `executor` under the id of its descriptor, replacing any earlier one.
    #[must_use]
    pub fn with(mut self, executor: Arc<dyn Executor>) -> Self {
        self.insert(executor.descriptor().id, executor);
        self
    }

    /// Register `executor` under `id`, replacing any earlier one.
    pub fn insert(&mut self, id: ExecutorId, executor: Arc<dyn Executor>) {
        self.executors.insert(id, executor);
    }

    /// The executor named `id`.
    pub fn get(&self, id: &ExecutorId) -> Option<&Arc<dyn Executor>> {
        self.executors.get(id)
    }

    /// The registered ids, in no particular order.
    pub fn ids(&self) -> impl Iterator<Item = &ExecutorId> {
        self.executors.keys()
    }
}
