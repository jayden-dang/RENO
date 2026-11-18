//! `fs.watch`: the changes of one served tree, as settled bursts, only while someone listens.
//!
//! One subscription is one platform watcher (an FSEvents stream, which is scarce: see
//! [`crate::watcher`]) plus the stream that reads it. Nothing runs between subscriptions: the
//! watcher starts when the subscription does and is dropped with it, whether the client
//! unsubscribed, disconnected, or fell behind.
//!
//! - **Coalescing.** The watcher's callback, on the platform's thread, only files a path into a
//!   small pending set and wakes the stream. The stream waits for quiet (every new event re-arms
//!   the deadline, up to a cap, so a constant churn still reports) and then yields one item:
//!   the directories touched, whether git's own state moved, and whether to rescan everything.
//!   More than [`FS_WATCH_MAX_DIRS`] directories is `overflow`: the set never grows past it.
//! - **A watcher that cannot start** (the pool is used up) is the one timer: the first item says
//!   `degraded`, then the start is retried after a doubling delay with a ceiling
//!   ([`WatchBackoff`], ARCH-10), and the first start that works yields an `overflow` item, since
//!   what changed meanwhile is unknown.
//! - **Git.** For a linked worktree the index and `HEAD` live outside the tree, so the
//!   repository's git directory is watched too, and only its `index`, `HEAD`, `packed-refs` and
//!   `refs` count (as `git`). Objects, logs and lock files do not.
//!
//! There is no queue to bound here: the pending set is bounded, and the socket's own bounded
//! queue (`reno-rpc`) turns a slow reader into `sub.gap`.

use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::Stream;
use futures::stream;
use notify::EventKind;
use reno_kernel::WatchBackoff;
use reno_proto::{FS_WATCH_MAX_DIRS, FsChange};
use tokio::sync::Notify;
use tokio::time::Instant;

use super::sandbox::GIT_DIR;
use crate::watcher::{WatcherFactory, WatcherGuard};

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
    /// Not ours: a read, or a path outside what is served.
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
    if inside
        .components()
        .next()
        .is_some_and(|c| matches!(c, Component::Normal(name) if name == GIT_DIR))
    {
        return Place::Elsewhere;
    }
    let parent = inside.parent().unwrap_or(Path::new(""));
    let dir: Vec<String> = parent
        .components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    Place::Tree(dir.join("/"))
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

#[derive(Debug, Default)]
struct Shared {
    pending: Mutex<Pending>,
    wake: Notify,
}

/// One of the daemon's open watches; releases its place when dropped.
#[derive(Debug)]
pub(crate) struct Slot(Arc<AtomicUsize>);

impl Slot {
    /// Takes a place, if fewer than `cap` are taken.
    pub(crate) fn take(open: &Arc<AtomicUsize>, cap: usize) -> Option<Self> {
        open.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| (n < cap).then_some(n + 1))
            .ok()?;
        Some(Self(Arc::clone(open)))
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Watching {
    factory: Arc<dyn WatcherFactory>,
    plan: Plan,
    timing: Timing,
    shared: Arc<Shared>,
    guard: Option<WatcherGuard>,
    failures: u32,
    /// An item to yield before anything else.
    queued: Option<FsChange>,
    _slot: Slot,
}

impl Watching {
    fn arm(&mut self) -> Result<(), String> {
        let shared = Arc::clone(&self.shared);
        let plan = self.plan.clone();
        let warned = AtomicBool::new(false);
        let guard = self.factory.start(
            &self.plan.roots(),
            Box::new(move |result: notify::Result<notify::Event>| {
                let verdict = classify(&plan, &result);
                if let Err(error) = &result
                    && !warned.swap(true, Ordering::Relaxed)
                {
                    tracing::warn!(%error, "a file watcher of the Files tab reported an error");
                }
                if verdict != Verdict::Ignore {
                    shared
                        .pending
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .absorb(verdict);
                    shared.wake.notify_one();
                }
            }),
        )?;
        self.guard = Some(guard);
        Ok(())
    }

    async fn next(&mut self) -> FsChange {
        loop {
            if let Some(item) = self.queued.take() {
                return item;
            }
            if self.guard.is_none() {
                tokio::time::sleep(self.timing.retry.delay(self.failures)).await;
                match self.arm() {
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
            self.shared.wake.notified().await;
            // Wait for quiet: each event re-arms the deadline, never past `max_wait`.
            let started = Instant::now();
            let mut deadline = started + self.timing.quiet;
            loop {
                tokio::select! {
                    biased;
                    () = self.shared.wake.notified() => {
                        deadline = (Instant::now() + self.timing.quiet)
                            .min(started + self.timing.max_wait);
                    }
                    () = tokio::time::sleep_until(deadline) => break,
                }
            }
            let taken = self
                .shared
                .pending
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take();
            if let Some(item) = taken {
                return item;
            }
        }
    }
}

/// Starts watching `plan` and returns the stream of changes. The first start is tried now; when it
/// fails the stream's first item is `degraded` and it keeps trying. Dropping the stream stops the
/// watcher and frees `slot`.
pub(crate) fn start(
    factory: Arc<dyn WatcherFactory>,
    plan: Plan,
    timing: Timing,
    slot: Slot,
) -> impl Stream<Item = FsChange> + Send + 'static {
    let mut watching = Watching {
        factory,
        plan,
        timing,
        shared: Arc::default(),
        guard: None,
        failures: 0,
        queued: None,
        _slot: slot,
    };
    if let Err(error) = watching.arm() {
        tracing::warn!(%error, "the Files tab cannot watch yet; retrying with a growing delay");
        watching.failures = 1;
        watching.queued = Some(FsChange {
            degraded: true,
            ..FsChange::default()
        });
    }
    stream::unfold(watching, |mut watching| async move {
        let item = watching.next().await;
        Some((item, watching))
    })
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
        let read = Ok(Event::new(EventKind::Access(AccessKind::Any))
            .add_path(PathBuf::from("/w/tree/a")));
        assert_eq!(classify(&plan, &read), Verdict::Ignore);
        let lost = Ok(Event::new(EventKind::Modify(ModifyKind::Any))
            .set_flag(notify::event::Flag::Rescan));
        assert_eq!(classify(&plan, &lost), Verdict::Rescan);
        assert_eq!(
            classify(&plan, &Err(notify::Error::generic("boom"))),
            Verdict::Rescan
        );
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

    fn slot() -> Slot {
        Slot::take(&Arc::default(), 4).unwrap()
    }

    #[tokio::test(start_paused = true)]
    async fn a_burst_is_one_item_with_every_directory_once_and_sorted() {
        let fake = Arc::new(Captured::default());
        let mut changes = Box::pin(start(fake.clone(), plan(), timing(), slot()));
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
        let mut changes = Box::pin(start(fake.clone(), plan(), timing(), slot()));
        let begun = Instant::now();
        let first = tokio::spawn({
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
        first.abort();
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_yielded_for_events_that_are_not_ours() {
        let fake = Arc::new(Captured::default());
        let mut changes = Box::pin(start(fake.clone(), plan(), timing(), slot()));
        fake.raise(created("/elsewhere/x").unwrap());
        fake.raise(created("/w/tree/.git/HEAD").unwrap());
        let waited = tokio::time::timeout(Duration::from_secs(60), changes.next()).await;
        assert!(waited.is_err(), "no item for events that are not ours");
    }

    /// Starts its first watchers refused, as a platform out of streams does.
    #[tokio::test(start_paused = true)]
    async fn a_failing_start_says_degraded_once_retries_with_backoff_and_then_rescans() {
        let inner = Counting::new(Captured::default());
        let failing = Arc::new(FailingFirst::new(inner, 3));
        let started = Instant::now();
        let mut changes = Box::pin(start(failing.clone(), plan(), timing(), slot()));
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
    async fn dropping_the_stream_stops_the_watcher_and_frees_the_slot() {
        let counting = Arc::new(Counting::new(Captured::default()));
        let open = Arc::new(AtomicUsize::new(0));
        let taken = Slot::take(&open, 1).unwrap();
        assert!(Slot::take(&open, 1).is_none(), "the cap holds");
        let changes = Box::pin(start(counting.clone(), plan(), timing(), taken));
        assert_eq!(counting.live(), 1);
        drop(changes);
        assert_eq!(counting.live(), 0);
        assert_eq!(open.load(Ordering::SeqCst), 0);
        assert!(Slot::take(&open, 1).is_some());
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
        let _ = start(roots.clone(), plan(), timing(), slot());
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
        };
        assert_eq!(inside.roots(), [PathBuf::from("/w/tree")]);
    }
}
