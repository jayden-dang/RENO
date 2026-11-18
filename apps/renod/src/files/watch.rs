//! `fs.watch`: the changes of one served tree, as settled bursts, only while someone listens.
//!
//! # One watcher per tree, shared
//!
//! The platform's pool of file-watch streams is small and shared by every process on the machine
//! (see [`crate::watcher`]: about sixteen were free when measured). So the daemon holds **one
//! watcher per served root**, however many subscriptions look at it (two windows, or two runs of
//! one thread, share a worktree), and at most [`Hub`]'s `cap` roots at once. A watcher is started
//! by the first subscription for a root and dropped with the last (ARCH-10): nothing is armed
//! for a run nobody is looking at.
//!
//! The watcher's callback, on the platform's thread, classifies the event and sends one
//! [`Verdict`] into a bounded broadcast channel. Each subscription reads its own receiver.
//! A subscription that falls behind loses the oldest verdicts and is told so by the channel;
//! that is turned into an `overflow` item (a gap notice, ARCH-12) instead of any unbounded
//! buffer.
//!
//! # What a subscription yields
//!
//! - **Coalescing.** A subscription waits for quiet (every new verdict re-arms the deadline, up
//!   to a cap, so a constant churn still reports) and yields one item: the directories touched,
//!   whether git's own state moved, and whether to rescan everything. More than
//!   [`FS_WATCH_MAX_DIRS`] directories is `overflow`; the pending set never grows past it.
//! - **A watcher that cannot start** (the pool is used up) is the one timer: the first item says
//!   `degraded`, then the start is retried after a doubling delay with a ceiling
//!   ([`WatchBackoff`], ARCH-10), and the first start that works yields an `overflow` item, since
//!   what changed meanwhile is unknown.
//! - **Git.** For a linked worktree the index and `HEAD` live outside the tree, so the
//!   repository's git directory is watched too, and only its `index`, `HEAD`, `packed-refs` and
//!   `refs` count (as `git`). Objects, logs and lock files do not.
//! - **Noise.** Events under a top-level directory git ignores (`target/`, `node_modules/`...)
//!   are dropped, and so is anything inside a `node_modules` at any depth; a build would
//!   otherwise report every second. An ignored directory nested deeper still reports, and a
//!   large burst there is the `overflow` item.

use std::collections::{BTreeSet, HashMap};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::Duration;

use futures::Stream;
use futures::stream;
use notify::EventKind;
use reno_kernel::WatchBackoff;
use reno_proto::{FS_WATCH_MAX_DIRS, FsChange};
use tokio::sync::broadcast;
use tokio::time::Instant;

use super::sandbox::GIT_DIR;
use crate::watcher::{WatcherFactory, WatcherGuard};

/// Verdicts a subscription may fall behind by before it is told to rescan.
const VERDICT_QUEUE: usize = 256;

/// How the watch waits and retries.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    /// How long the tree must be quiet before a burst is reported.
    pub(crate) quiet: Duration,
    /// The longest a burst is held back, however busy the tree stays.
    pub(crate) max_wait: Duration,
    /// How soon a failed start is tried again.
    pub(crate) retry: WatchBackoff,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            quiet: Duration::from_millis(100),
            max_wait: Duration::from_secs(1),
            retry: WatchBackoff::default(),
        }
    }
}

/// What to watch.
#[derive(Debug, Clone)]
pub(crate) struct Plan {
    /// The served root, real path.
    pub(crate) root: PathBuf,
    /// The repository's git directory, when the root is in one.
    pub(crate) git_dir: Option<PathBuf>,
    /// Top-level names under the root whose events are noise (ignored build output).
    pub(crate) noise: Vec<String>,
}

impl Plan {
    /// The paths the platform watcher is pointed at.
    fn roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.root.clone()];
        if let Some(git) = &self.git_dir
            && !git.starts_with(&self.root)
        {
            roots.push(git.clone());
        }
        roots
    }
}

/// What one watcher callback means.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Not ours: a read, noise, or a path outside what is served.
    Ignore,
    /// Entries changed in these directories, and/or git's state moved.
    Touched { dirs: Vec<String>, git: bool },
    /// Events were lost, or the watcher failed: everything may have changed.
    Rescan,
}

/// Judges one callback. A pure function of the event and the plan.
pub(crate) fn classify(plan: &Plan, result: &notify::Result<notify::Event>) -> Verdict {
    let Ok(event) = result else {
        return Verdict::Rescan;
    };
    if event.need_rescan() {
        return Verdict::Rescan;
    }
    if matches!(event.kind, EventKind::Access(_)) {
        return Verdict::Ignore;
    }
    let mut dirs = Vec::new();
    let mut git = false;
    for path in &event.paths {
        match place(plan, path) {
            Place::Git => git = true,
            Place::Tree(dir) => dirs.push(dir),
            Place::Elsewhere => {}
        }
    }
    if dirs.is_empty() && !git {
        Verdict::Ignore
    } else {
        Verdict::Touched { dirs, git }
    }
}

enum Place {
    /// Something of git's that moves the status.
    Git,
    /// A path in the served tree: its directory, relative.
    Tree(String),
    Elsewhere,
}

fn place(plan: &Plan, path: &Path) -> Place {
    if let Some(git) = &plan.git_dir
        && let Ok(inside) = path.strip_prefix(git)
    {
        let first = inside.components().next();
        let moves_status = matches!(
            first,
            Some(Component::Normal(name))
                if name == "index" || name == "HEAD" || name == "packed-refs" || name == "refs"
        );
        return if moves_status {
            Place::Git
        } else {
            Place::Elsewhere
        };
    }
    let Ok(inside) = path.strip_prefix(&plan.root) else {
        return Place::Elsewhere;
    };
    let names: Vec<String> = inside
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    let first = names.first().map(String::as_str);
    if first == Some(GIT_DIR)
        || first.is_some_and(|name| plan.noise.iter().any(|n| n == name))
        || names.iter().any(|name| name == "node_modules")
    {
        return Place::Elsewhere;
    }
    let dirs = names.len().saturating_sub(1);
    Place::Tree(names[..dirs].join("/"))
}

/// What has been seen since the last item. Bounded by [`FS_WATCH_MAX_DIRS`].
#[derive(Debug, Default)]
struct Pending {
    dirs: BTreeSet<String>,
    git: bool,
    overflow: bool,
}

impl Pending {
    fn absorb(&mut self, verdict: Verdict) {
        match verdict {
            Verdict::Ignore => {}
            Verdict::Rescan => self.overflow = true,
            Verdict::Touched { dirs, git } => {
                self.git |= git;
                if self.overflow {
                    return;
                }
                self.dirs.extend(dirs);
                if self.dirs.len() > FS_WATCH_MAX_DIRS {
                    self.overflow = true;
                    self.dirs.clear();
                }
            }
        }
    }

    fn take(&mut self) -> Option<FsChange> {
        let taken = std::mem::take(self);
        if taken.dirs.is_empty() && !taken.git && !taken.overflow {
            return None;
        }
        Some(FsChange {
            dirs: taken.dirs.into_iter().collect(),
            git: taken.git,
            overflow: taken.overflow,
            degraded: false,
        })
    }
}

/// One place of the daemon's budget of watched roots; released when dropped.
#[derive(Debug)]
struct Slot(Arc<AtomicUsize>);

impl Slot {
    fn take(open: &Arc<AtomicUsize>, cap: usize) -> Option<Self> {
        open.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            (n < cap).then_some(n + 1)
        })
        .ok()?;
        Some(Self(Arc::clone(open)))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A running platform watcher for one root, with the channel its verdicts go to. Dropping the
/// last `Arc` stops the watcher (and then frees its place).
#[derive(Debug)]
struct RootWatch {
    tx: broadcast::Sender<Verdict>,
    _guard: WatcherGuard,
    _slot: Slot,
}

/// Why a watcher was not started.
#[derive(Debug)]
pub(crate) enum Refused {
    /// The daemon watches as many roots as it will.
    Capacity,
    /// The platform refused.
    Platform(String),
}

/// The watchers the daemon holds, one per root, shared.
#[derive(Debug)]
pub(crate) struct Hub {
    factory: Arc<dyn WatcherFactory>,
    open: Arc<AtomicUsize>,
    cap: usize,
    roots: Mutex<HashMap<PathBuf, Weak<RootWatch>>>,
}

impl Hub {
    pub(crate) fn new(factory: Arc<dyn WatcherFactory>, cap: usize) -> Self {
        Self {
            factory,
            open: Arc::default(),
            cap,
            roots: Mutex::default(),
        }
    }

    /// How many roots are being watched.
    pub(crate) fn open(&self) -> usize {
        self.open.load(Ordering::SeqCst)
    }

    /// The watcher of `plan`'s root: the one that runs, or a new one.
    fn acquire(&self, plan: &Plan) -> Result<Arc<RootWatch>, Refused> {
        let mut roots = self.roots.lock().unwrap_or_else(PoisonError::into_inner);
        roots.retain(|_, watch| watch.strong_count() > 0);
        if let Some(running) = roots.get(&plan.root).and_then(Weak::upgrade) {
            return Ok(running);
        }
        let slot = Slot::take(&self.open, self.cap).ok_or(Refused::Capacity)?;
        let (tx, _) = broadcast::channel(VERDICT_QUEUE);
        let sender = tx.clone();
        let judged = plan.clone();
        let warned = AtomicBool::new(false);
        let guard = self
            .factory
            .start(
                &plan.roots(),
                Box::new(move |result: notify::Result<notify::Event>| {
                    let verdict = classify(&judged, &result);
                    if let Err(error) = &result
                        && !warned.swap(true, Ordering::Relaxed)
                    {
                        tracing::warn!(%error, "a file watcher of the Files tab reported an error");
                    }
                    if verdict != Verdict::Ignore {
                        // No receiver is not an error: nobody is looking right now.
                        let _ = sender.send(verdict);
                    }
                }),
            )
            .map_err(Refused::Platform)?;
        let watch = Arc::new(RootWatch {
            tx,
            _guard: guard,
            _slot: slot,
        });
        roots.insert(plan.root.clone(), Arc::downgrade(&watch));
        Ok(watch)
    }
}

struct Connection {
    /// Keeps the root's watcher alive.
    _watch: Arc<RootWatch>,
    verdicts: broadcast::Receiver<Verdict>,
}

struct Watching {
    hub: Arc<Hub>,
    plan: Plan,
    timing: Timing,
    connection: Option<Connection>,
    pending: Pending,
    failures: u32,
    /// An item to yield before anything else.
    queued: Option<FsChange>,
}

impl Watching {
    fn connect(&mut self) -> Result<(), Refused> {
        let watch = self.hub.acquire(&self.plan)?;
        self.connection = Some(Connection {
            verdicts: watch.tx.subscribe(),
            _watch: watch,
        });
        Ok(())
    }

    /// The next verdict, or `None` when the root's watcher is gone.
    async fn verdict(&mut self) -> Option<Verdict> {
        let connection = self.connection.as_mut()?;
        match connection.verdicts.recv().await {
            Ok(verdict) => Some(verdict),
            // This subscription was too slow: it lost verdicts, so it cannot know what changed.
            Err(broadcast::error::RecvError::Lagged(_)) => Some(Verdict::Rescan),
            Err(broadcast::error::RecvError::Closed) => None,
        }
    }

    async fn next(&mut self) -> FsChange {
        loop {
            if let Some(item) = self.queued.take() {
                return item;
            }
            if self.connection.is_none() {
                tokio::time::sleep(self.timing.retry.delay(self.failures)).await;
                match self.connect() {
                    Ok(()) => {
                        self.failures = 0;
                        // What changed while it was down is unknown.
                        return FsChange {
                            overflow: true,
                            git: true,
                            ..FsChange::default()
                        };
                    }
                    Err(_) => {
                        self.failures = self.failures.saturating_add(1);
                        continue;
                    }
                }
            }
            let Some(first) = self.verdict().await else {
                // The watcher went away under us: start again.
                self.connection = None;
                self.failures = 1;
                continue;
            };
            self.pending.absorb(first);
            // Wait for quiet: each verdict re-arms the deadline, never past `max_wait`.
            let started = Instant::now();
            let mut deadline = started + self.timing.quiet;
            loop {
                tokio::select! {
                    biased;
                    more = self.verdict() => {
                        let Some(more) = more else { break };
                        self.pending.absorb(more);
                        deadline = (Instant::now() + self.timing.quiet)
                            .min(started + self.timing.max_wait);
                    }
                    () = tokio::time::sleep_until(deadline) => break,
                }
            }
            if let Some(item) = self.pending.take() {
                return item;
            }
        }
    }
}

/// Starts watching `plan` and returns the stream of changes. The first start is tried now:
/// [`Refused::Capacity`] is an error to the caller, and a platform refusal makes the stream's
/// first item `degraded` while it keeps trying. Dropping the stream releases its hold on the
/// root's watcher, which stops with the last hold.
pub(crate) fn start(
    hub: Arc<Hub>,
    plan: Plan,
    timing: Timing,
) -> Result<impl Stream<Item = FsChange> + Send + 'static, Refused> {
    let mut watching = Watching {
        hub,
        plan,
        timing,
        connection: None,
        pending: Pending::default(),
        failures: 0,
        queued: None,
    };
    match watching.connect() {
        Ok(()) => {}
        Err(Refused::Capacity) => return Err(Refused::Capacity),
        Err(Refused::Platform(error)) => {
            tracing::warn!(%error, "the Files tab cannot watch yet; retrying with a growing delay");
            watching.failures = 1;
            watching.queued = Some(FsChange {
                degraded: true,
                ..FsChange::default()
            });
        }
    }
    Ok(stream::unfold(watching, |mut watching| async move {
        let item = watching.next().await;
        Some((item, watching))
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Mutex as StdMutex;

    use futures::StreamExt;
    use notify::event::{AccessKind, CreateKind, ModifyKind};
    use notify::{Event, EventKind};

    use super::*;
    use crate::watcher::{Captured, Counting, FailingFirst};

    fn plan() -> Plan {
        Plan {
            root: PathBuf::from("/w/tree"),
            git_dir: Some(PathBuf::from("/w/repo.git/worktrees/t")),
            noise: vec!["target".into()],
        }
    }

    fn created(path: &str) -> notify::Result<Event> {
        Ok(Event::new(EventKind::Create(CreateKind::File)).add_path(PathBuf::from(path)))
    }

    #[test]
    fn a_path_is_filed_under_its_directory_and_git_and_the_rest_are_ignored() {
        let plan = plan();
        assert_eq!(
            classify(&plan, &created("/w/tree/src/a.rs")),
            Verdict::Touched {
                dirs: vec!["src".into()],
                git: false
            }
        );
        assert_eq!(
            classify(&plan, &created("/w/tree/top.txt")),
            Verdict::Touched {
                dirs: vec![String::new()],
                git: false
            }
        );
        assert_eq!(
            classify(&plan, &created("/w/tree/a/b/c.txt")),
            Verdict::Touched {
                dirs: vec!["a/b".into()],
                git: false
            }
        );
        // .git inside the tree, a git directory's objects and lock files, and strangers.
        assert_eq!(classify(&plan, &created("/w/tree/.git")), Verdict::Ignore);
        assert_eq!(
            classify(&plan, &created("/w/repo.git/worktrees/t/index.lock")),
            Verdict::Ignore
        );
        assert_eq!(
            classify(&plan, &created("/w/repo.git/worktrees/t/logs/HEAD")),
            Verdict::Ignore
        );
        assert_eq!(classify(&plan, &created("/elsewhere/x")), Verdict::Ignore);
        for moves in ["index", "HEAD", "refs/heads/x", "packed-refs"] {
            assert_eq!(
                classify(&plan, &created(&format!("/w/repo.git/worktrees/t/{moves}"))),
                Verdict::Touched {
                    dirs: vec![],
                    git: true
                },
                "{moves}"
            );
        }
        let read =
            Ok(Event::new(EventKind::Access(AccessKind::Any)).add_path(PathBuf::from("/w/tree/a")));
        assert_eq!(classify(&plan, &read), Verdict::Ignore);
        let lost =
            Ok(Event::new(EventKind::Modify(ModifyKind::Any))
                .set_flag(notify::event::Flag::Rescan));
        assert_eq!(classify(&plan, &lost), Verdict::Rescan);
        assert_eq!(
            classify(&plan, &Err(notify::Error::generic("boom"))),
            Verdict::Rescan
        );
    }

    #[test]
    fn build_output_is_noise_at_the_top_and_node_modules_at_any_depth() {
        let plan = plan();
        assert_eq!(
            classify(&plan, &created("/w/tree/target/debug/x.o")),
            Verdict::Ignore
        );
        assert_eq!(
            classify(&plan, &created("/w/tree/web/node_modules/a/b.js")),
            Verdict::Ignore
        );
        // Only a top-level ignored name is noise: `src/target` is a directory like any other.
        assert!(matches!(
            classify(&plan, &created("/w/tree/src/target/x")),
            Verdict::Touched { .. }
        ));
    }

    #[test]
    fn the_pending_set_is_bounded_and_overflow_is_final_until_taken() {
        let mut pending = Pending::default();
        for n in 0..=FS_WATCH_MAX_DIRS {
            pending.absorb(Verdict::Touched {
                dirs: vec![format!("d{n}")],
                git: false,
            });
        }
        assert!(pending.overflow);
        assert!(pending.dirs.is_empty());
        pending.absorb(Verdict::Touched {
            dirs: vec!["more".into()],
            git: true,
        });
        let item = pending.take().unwrap();
        assert!(item.overflow && item.git && item.dirs.is_empty());
        assert!(pending.take().is_none());
    }

    fn timing() -> Timing {
        Timing {
            quiet: Duration::from_millis(100),
            max_wait: Duration::from_millis(500),
            retry: WatchBackoff {
                initial: Duration::from_secs(1),
                max: Duration::from_secs(4),
            },
        }
    }

    fn hub(factory: Arc<dyn WatcherFactory>) -> Arc<Hub> {
        Arc::new(Hub::new(factory, 4))
    }

    fn open(hub: &Arc<Hub>, plan: Plan) -> std::pin::Pin<Box<impl Stream<Item = FsChange> + Send>> {
        Box::pin(start(Arc::clone(hub), plan, timing()).unwrap())
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_is_one_item_with_every_directory_once_and_sorted() {
        let fake = Arc::new(Captured::default());
        let hub = hub(fake.clone());
        let mut changes = open(&hub, plan());
        for path in ["/w/tree/b/x", "/w/tree/a/y", "/w/tree/b/z", "/w/tree/q"] {
            fake.raise(created(path).unwrap());
            tokio::time::advance(Duration::from_millis(40)).await;
        }
        let item = changes.next().await.unwrap();
        assert_eq!(item.dirs, ["", "a", "b"]);
        assert!(!item.git && !item.overflow && !item.degraded);
    }

    #[tokio::test(start_paused = true)]
    async fn constant_churn_still_reports_within_the_cap() {
        let fake = Arc::new(Captured::default());
        let hub = hub(fake.clone());
        let mut changes = open(&hub, plan());
        let begun = Instant::now();
        let churn = tokio::spawn({
            let fake = fake.clone();
            async move {
                // Never quiet for the 100 ms the watch waits for.
                for _ in 0..40 {
                    fake.raise(created("/w/tree/busy/f").unwrap());
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
        });
        let item = changes.next().await.unwrap();
        assert_eq!(item.dirs, ["busy"]);
        assert!(begun.elapsed() <= Duration::from_millis(600));
        churn.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_yielded_for_events_that_are_not_ours() {
        let fake = Arc::new(Captured::default());
        let hub = hub(fake.clone());
        let mut changes = open(&hub, plan());
        fake.raise(created("/elsewhere/x").unwrap());
        fake.raise(created("/w/tree/.git/HEAD").unwrap());
        fake.raise(created("/w/tree/target/debug/x").unwrap());
        let waited = tokio::time::timeout(Duration::from_secs(60), changes.next()).await;
        assert!(waited.is_err(), "no item for events that are not ours");
    }

    #[tokio::test(start_paused = true)]
    async fn a_subscriber_that_falls_behind_is_told_to_rescan_and_nothing_piles_up() {
        let fake = Arc::new(Captured::default());
        let hub = hub(fake.clone());
        let mut changes = open(&hub, plan());
        // Far more verdicts than the queue holds, none read yet.
        for n in 0..(VERDICT_QUEUE * 3) {
            fake.raise(created(&format!("/w/tree/d{}/f", n % 3)).unwrap());
        }
        let item = changes.next().await.unwrap();
        assert!(item.overflow, "{item:?}");
        assert!(item.dirs.is_empty() || item.dirs.len() <= 3);
    }

    #[tokio::test(start_paused = true)]
    async fn subscriptions_of_one_root_share_one_watcher_that_stops_with_the_last() {
        let counting = Arc::new(Counting::new(Captured::default()));
        let hub = hub(counting.clone());
        let first = open(&hub, plan());
        let second = open(&hub, plan());
        assert_eq!(counting.live(), 1, "one watcher for one root");
        assert_eq!(hub.open(), 1);
        drop(first);
        assert_eq!(counting.live(), 1, "the other still looks");
        drop(second);
        assert_eq!(counting.live(), 0);
        assert_eq!(hub.open(), 0);
        // A later subscription starts a new one.
        let _again = open(&hub, plan());
        assert_eq!(counting.live(), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn the_hub_holds_only_so_many_roots() {
        let hub = Arc::new(Hub::new(Arc::new(Captured::default()), 2));
        let at = |root: &str| Plan {
            root: PathBuf::from(root),
            git_dir: None,
            noise: vec![],
        };
        let _a = start(Arc::clone(&hub), at("/a"), timing()).unwrap();
        let _b = start(Arc::clone(&hub), at("/b"), timing()).unwrap();
        // A third root is refused; a second look at a watched root is not.
        assert!(matches!(
            start(Arc::clone(&hub), at("/c"), timing()),
            Err(Refused::Capacity)
        ));
        assert!(start(Arc::clone(&hub), at("/a"), timing()).is_ok());
        assert_eq!(hub.open(), 2);
    }

    /// Starts its first watchers refused, as a platform out of streams does.
    #[tokio::test(start_paused = true)]
    async fn a_failing_start_says_degraded_once_retries_with_backoff_and_then_rescans() {
        let inner = Counting::new(Captured::default());
        let failing = Arc::new(FailingFirst::new(inner, 3));
        let hub = hub(failing.clone());
        let started = Instant::now();
        let mut changes = open(&hub, plan());
        // The first start failed already; the stream says so at once.
        let first = changes.next().await.unwrap();
        assert!(first.degraded && first.dirs.is_empty());
        assert_eq!(failing.attempts(), 1);
        // Retries after 1 s and 2 s fail, the one after 4 s works: 7 s in all, and no item came
        // between.
        let recovered = changes.next().await.unwrap();
        assert!(recovered.overflow && recovered.git && !recovered.degraded);
        assert_eq!(failing.attempts(), 4);
        assert_eq!(started.elapsed(), Duration::from_secs(7));
    }

    #[tokio::test(start_paused = true)]
    async fn a_linked_worktree_watches_its_git_directory_too() {
        #[derive(Debug, Default)]
        struct Roots(StdMutex<Vec<Vec<PathBuf>>>);
        impl WatcherFactory for Roots {
            fn start(
                &self,
                roots: &[PathBuf],
                _: crate::watcher::Handler,
            ) -> Result<WatcherGuard, String> {
                self.0.lock().unwrap().push(roots.to_vec());
                Ok(WatcherGuard::new(()))
            }
        }
        let roots = Arc::new(Roots::default());
        let hub = hub(roots.clone());
        let _kept = open(&hub, plan());
        assert_eq!(
            roots.0.lock().unwrap()[0],
            [
                PathBuf::from("/w/tree"),
                PathBuf::from("/w/repo.git/worktrees/t")
            ]
        );
        // A git directory inside the tree is already covered.
        let inside = Plan {
            root: PathBuf::from("/w/tree"),
            git_dir: Some(PathBuf::from("/w/tree/.git")),
            noise: vec![],
        };
        assert_eq!(inside.roots(), [PathBuf::from("/w/tree")]);
    }
}
