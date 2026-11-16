#![cfg(test)]
#![allow(clippy::unwrap_used, clippy::expect_used)]

//! Behaviour tests for the kernel: paused tokio time, a tempdir store, `MockExecutor`. No
//! network, no real harness, and no assertion that depends on the wall clock. The one real-time
//! wait is `Fx::idle`, which pauses a few real milliseconds for the store thread to settle; it
//! bounds how long a test waits, never what it asserts.
//!
//! # Time
//!
//! A paused tokio clock jumps to the next timer whenever the runtime is idle, and a store write
//! is answered by a real thread, so an armed timer could fire while a write is in flight. Each
//! test therefore runs a spinner task that keeps the runtime busy: time moves only when the test
//! says so, in `until` (1 ms per probe) or `tokio::time::advance`.

mod agent;
mod approvals;
mod budget;
mod defs;
mod health;
mod queue;
mod recovery;
mod resume;
mod routing;
mod runs;
mod secrets;
mod triggers;
mod workspaces;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures::StreamExt;
use reno_harness::exec::{MockExecutor, MockStep};
use reno_proto::{ExecEvent, ExecutorId, RunEventItem, RunId, RunStatus, Seq};
use reno_store::Store;
use tempfile::TempDir;
use tokio::task::JoinHandle;

use crate::{
    DefsSetup, ExecutorRegistry, Kernel, KernelConfig, KernelError, ManualClock, RunLink, Workspace,
};

pub(super) fn id(name: &str) -> ExecutorId {
    name.parse().unwrap()
}

/// Probe until it yields, moving paused time 1 ms per try.
pub(super) async fn until<T>(mut probe: impl AsyncFnMut() -> Option<T>) -> T {
    for _ in 0..30_000 {
        if let Some(found) = probe().await {
            return found;
        }
        tokio::time::advance(Duration::from_millis(1)).await;
    }
    panic!("condition never became true");
}

/// The clock of a new kernel: 2023-11-14T22:13:20Z.
pub(super) const START_MS: i64 = 1_700_000_000_000;

pub(super) struct Fx {
    dir: Option<TempDir>,
    pub(super) store: Store,
    pub(super) clock: Arc<ManualClock>,
    pub(super) kernel: Kernel,
    spinner: JoinHandle<()>,
}

impl Drop for Fx {
    fn drop(&mut self) {
        self.spinner.abort();
    }
}

/// Keeps the runtime from ever being idle, so paused time cannot auto-advance.
pub(super) fn spin() -> JoinHandle<()> {
    tokio::spawn(async {
        loop {
            tokio::task::yield_now().await;
        }
    })
}

pub(super) fn registry(mocks: &[MockExecutor]) -> ExecutorRegistry {
    mocks
        .iter()
        .fold(ExecutorRegistry::new(), |registry, mock| {
            registry.with(Arc::new(mock.clone()))
        })
}

pub(super) fn mock(steps: impl IntoIterator<Item = MockStep>) -> MockExecutor {
    MockExecutor::new(id("mock")).with_steps(steps)
}

pub(super) fn config(max_concurrent: usize) -> KernelConfig {
    KernelConfig {
        max_concurrent,
        ..KernelConfig::default()
    }
}

impl Fx {
    /// A kernel over a fresh store.
    pub(super) async fn new(config: KernelConfig, executors: ExecutorRegistry) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("reno.db")).unwrap();
        Self::over(dir, store, config, executors).await
    }

    /// A kernel over an existing store (recovery tests build the store first).
    pub(super) async fn over(
        dir: TempDir,
        store: Store,
        config: KernelConfig,
        executors: ExecutorRegistry,
    ) -> Self {
        Self::start_over(Some(dir), store, config, executors, None, START_MS).await
    }

    /// A kernel over a fresh store that loads definitions through `definitions`.
    pub(super) async fn with_definitions(
        config: KernelConfig,
        executors: ExecutorRegistry,
        definitions: DefsSetup,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("reno.db")).unwrap();
        Self::over_definitions(dir, store, config, executors, definitions).await
    }

    /// A kernel over an existing store that loads definitions through `definitions`.
    pub(super) async fn over_definitions(
        dir: TempDir,
        store: Store,
        config: KernelConfig,
        executors: ExecutorRegistry,
        definitions: DefsSetup,
    ) -> Self {
        Self::start_over(
            Some(dir),
            store,
            config,
            executors,
            Some(definitions),
            START_MS,
        )
        .await
    }

    /// A kernel over a fresh store, its clock at `now_ms`, loading definitions through
    /// `definitions`.
    pub(super) async fn with_definitions_at(
        now_ms: i64,
        config: KernelConfig,
        executors: ExecutorRegistry,
        definitions: DefsSetup,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("reno.db")).unwrap();
        Self::start_over(
            Some(dir),
            store,
            config,
            executors,
            Some(definitions),
            now_ms,
        )
        .await
    }

    /// A second kernel over the same store as `first`, with its own clock at `now_ms`: two
    /// schedulers on one database.
    pub(super) async fn sharing(
        first: &Fx,
        now_ms: i64,
        config: KernelConfig,
        executors: ExecutorRegistry,
        definitions: DefsSetup,
    ) -> Self {
        Self::start_over(
            None,
            first.store.clone(),
            config,
            executors,
            Some(definitions),
            now_ms,
        )
        .await
    }

    /// Like `over_definitions`, with the clock at `now_ms`: a restart some time later.
    pub(super) async fn restarted_at(
        now_ms: i64,
        dir: TempDir,
        store: Store,
        config: KernelConfig,
        executors: ExecutorRegistry,
        definitions: DefsSetup,
    ) -> Self {
        Self::start_over(
            Some(dir),
            store,
            config,
            executors,
            Some(definitions),
            now_ms,
        )
        .await
    }

    /// Gives back the store and its directory, to start another kernel over them.
    pub(super) fn into_store(mut self) -> (TempDir, Store) {
        let dir = self.dir.take().expect("the directory is still here");
        (dir, self.store.clone())
    }

    async fn start_over(
        dir: Option<TempDir>,
        store: Store,
        config: KernelConfig,
        executors: ExecutorRegistry,
        definitions: Option<DefsSetup>,
        now_ms: i64,
    ) -> Self {
        Self::start_over_with(
            dir,
            store,
            config,
            executors,
            definitions,
            None,
            None,
            now_ms,
        )
        .await
    }

    /// A kernel over a fresh store with `link` as its `RunLink` port.
    pub(super) async fn with_link(
        config: KernelConfig,
        executors: ExecutorRegistry,
        definitions: Option<DefsSetup>,
        link: Arc<dyn RunLink>,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("reno.db")).unwrap();
        Self::over_link(dir, store, config, executors, definitions, link, START_MS).await
    }

    /// A kernel over an existing store with `link` as its `RunLink` port, its clock at `now_ms`.
    pub(super) async fn over_link(
        dir: TempDir,
        store: Store,
        config: KernelConfig,
        executors: ExecutorRegistry,
        definitions: Option<DefsSetup>,
        link: Arc<dyn RunLink>,
        now_ms: i64,
    ) -> Self {
        Self::start_over_with(
            Some(dir),
            store,
            config,
            executors,
            definitions,
            None,
            Some(link),
            now_ms,
        )
        .await
    }

    /// A kernel over a fresh store with `workspace` as its `Workspace` port.
    pub(super) async fn with_workspace(
        config: KernelConfig,
        executors: ExecutorRegistry,
        definitions: Option<DefsSetup>,
        workspace: Arc<dyn Workspace>,
    ) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path().join("reno.db")).unwrap();
        Self::over_workspace(dir, store, config, executors, definitions, workspace).await
    }

    /// A kernel over an existing store with `workspace` as its `Workspace` port.
    pub(super) async fn over_workspace(
        dir: TempDir,
        store: Store,
        config: KernelConfig,
        executors: ExecutorRegistry,
        definitions: Option<DefsSetup>,
        workspace: Arc<dyn Workspace>,
    ) -> Self {
        Self::start_over_with(
            Some(dir),
            store,
            config,
            executors,
            definitions,
            Some(workspace),
            None,
            START_MS,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn start_over_with(
        dir: Option<TempDir>,
        store: Store,
        config: KernelConfig,
        executors: ExecutorRegistry,
        definitions: Option<DefsSetup>,
        workspace: Option<Arc<dyn Workspace>>,
        link: Option<Arc<dyn RunLink>>,
        now_ms: i64,
    ) -> Self {
        let spinner = spin();
        let clock = Arc::new(ManualClock::new(now_ms));
        let start = tokio::spawn(Kernel::start_linked(
            config,
            store.clone(),
            executors,
            clock.clone(),
            definitions,
            workspace,
            link,
        ));
        // Start-up may sleep on paused time (the orphan sweep), so drive it.
        until(async || start.is_finished().then_some(())).await;
        let kernel = start.await.unwrap().unwrap();
        Self {
            dir,
            store,
            clock,
            kernel,
            spinner,
        }
    }

    pub(super) async fn submit(&self, prompt: &str) -> RunId {
        self.kernel
            .submit(prompt, id("mock"), PathBuf::from("/work"), None)
            .await
            .unwrap()
    }

    pub(super) async fn status(&self, run: &RunId) -> RunStatus {
        self.kernel.get_run(run).await.unwrap().status
    }

    pub(super) async fn wait_status(&self, run: &RunId, want: RunStatus) {
        until(async || (self.status(run).await == want).then_some(())).await;
    }

    /// The run's journal from the file, which is what a late subscriber replays.
    pub(super) fn journal(&self, run: &RunId) -> Vec<(Seq, ExecEvent)> {
        self.store
            .journals()
            .replay(run, Seq(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    /// Every item of `run_events(run, from)` until it ends.
    pub(super) async fn events(
        &self,
        run: &RunId,
        from: Option<Seq>,
    ) -> Vec<Result<RunEventItem, KernelError>> {
        self.kernel
            .run_events(run, from)
            .await
            .unwrap()
            .collect()
            .await
    }

    /// Wait for the dispatcher to go quiet: no store write for several real milliseconds in a
    /// row. Returns the write count then.
    pub(super) async fn idle(&self) -> u64 {
        let mut last = u64::MAX;
        let mut stable = 0;
        until(async || {
            tokio::task::spawn_blocking(|| std::thread::sleep(Duration::from_millis(5)))
                .await
                .unwrap();
            let now = self.kernel.store_writes();
            stable = if now == last { stable + 1 } else { 0 };
            last = now;
            (stable >= 5).then_some(now)
        })
        .await
    }

    /// The run's journal once it ends in a `Done`: the state commits first, the `Done` follows.
    pub(super) async fn journal_with_done(&self, run: &RunId) -> Vec<(Seq, ExecEvent)> {
        until(async || {
            let journal = self.journal(run);
            matches!(journal.last(), Some((_, ExecEvent::Done { .. }))).then_some(journal)
        })
        .await
    }

    /// Every `(from, to)` status pair the store recorded for `run`, in order.
    pub(super) async fn transitions(&self, run: &RunId) -> Vec<(String, String)> {
        self.store
            .events_since(Seq(0), 100_000)
            .await
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "run.transitioned" && e.subject_id == run.as_str())
            .map(|e| {
                (
                    e.data["from"].as_str().unwrap().to_owned(),
                    e.data["to"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }
}

pub(super) fn last_done(journal: &[(Seq, ExecEvent)]) -> &ExecEvent {
    let (_, last) = journal.last().expect("journal is not empty");
    assert!(matches!(last, ExecEvent::Done { .. }), "{last:?}");
    last
}
