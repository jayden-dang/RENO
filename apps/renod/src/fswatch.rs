//! The `Watch` port over the file system, for `fs` triggers (ARCHITECTURE 7, ARCH-10).
//!
//! Each watch is one `notify` watcher plus one debounce task. The watcher's callback runs on the
//! platform's thread and only does a `try_send` into a channel of capacity 1, so a flood is one
//! pending signal. The task waits for quiet: every signal re-arms a deadline, and when it passes
//! with no further signal the kernel is told once, with `debounce` taken from the definition.
//! Nothing wakes on an interval.
//!
//! A watch the platform refuses to start (it gives the whole machine only a few streams) is an
//! error from [`FsWatch::watch`]; the scheduler shows the trigger as failed and starts it again
//! after a growing delay. That timer exists only while a start is failing (ARCH-10).
//!
//! # Paths
//!
//! A path is absolute or starts with `~/`. A path with no glob characters is watched as it is
//! (a directory covers everything under it, a file only itself). One with glob characters
//! (`*`, `?`, `[`, `{`) is watched from its longest literal directory prefix, recursively, and
//! an event counts only when its path matches the glob. A prefix that does not exist yet is
//! replaced by its nearest existing ancestor, so a directory created later is seen.
//!
//! The platform reports resolved paths (on macOS `/var` is `/private/var`), so each prefix is
//! resolved once, as `FsDefinitions::open` does, and the pattern is rebuilt from the resolved
//! prefix.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use globset::{Glob, GlobMatcher};
use notify::EventKind;
use reno_kernel::{KernelError, Watch, WatchSpec, WatchStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::watcher::{NotifyWatchers, WatcherFactory, WatcherGuard};

/// The `Watch` port: `notify`, debounced per definition.
#[derive(Debug)]
pub struct FsWatch {
    tasks: TaskTracker,
    watchers: Arc<dyn WatcherFactory>,
}

impl Default for FsWatch {
    fn default() -> Self {
        Self::with_watchers(Arc::new(NotifyWatchers))
    }
}

impl FsWatch {
    /// A port with no watches yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// A port that starts its watchers with `watchers`.
    pub fn with_watchers(watchers: Arc<dyn WatcherFactory>) -> Self {
        Self {
            tasks: TaskTracker::new(),
            watchers,
        }
    }

    /// Waits for the debounce tasks of watches that were dropped. Call it after the kernel has
    /// shut down, which drops them.
    pub async fn shutdown(&self) {
        self.tasks.close();
        self.tasks.wait().await;
    }
}

/// What a watch holds while it runs: dropping it stops the watcher and its debounce task.
struct Guard {
    _watcher: WatcherGuard,
    stop: CancellationToken,
}

impl Drop for Guard {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

/// One path of a definition, ready to match events.
struct Target {
    /// Where to point the watcher: an existing path.
    root: PathBuf,
    /// What an event path must match.
    matcher: Matcher,
}

enum Matcher {
    /// The path itself, or anything under it.
    Under(PathBuf),
    Glob(Box<GlobMatcher>),
}

impl Matcher {
    fn matches(&self, path: &Path) -> bool {
        match self {
            Self::Under(root) => path.starts_with(root),
            Self::Glob(glob) => glob.is_match(path),
        }
    }
}

fn has_glob(component: &str) -> bool {
    component.contains(['*', '?', '[', '{'])
}

fn expand_home(path: &str) -> Result<PathBuf, String> {
    match path.strip_prefix("~/") {
        Some(rest) => {
            let home = std::env::var_os("HOME")
                .filter(|home| !home.is_empty())
                .ok_or_else(|| format!("{path:?} starts with `~/` but HOME is not set"))?;
            Ok(PathBuf::from(home).join(rest))
        }
        None => Ok(PathBuf::from(path)),
    }
}

/// The nearest ancestor of `path` (or `path`) that exists, resolved.
fn resolve_existing(path: &Path) -> Result<(PathBuf, PathBuf), String> {
    let mut existing = path;
    loop {
        match std::fs::canonicalize(existing) {
            Ok(resolved) => {
                let rest = path.strip_prefix(existing).unwrap_or(Path::new(""));
                return Ok((resolved, rest.to_path_buf()));
            }
            Err(_) => {
                existing = existing
                    .parent()
                    .ok_or_else(|| format!("no part of {} exists", path.display()))?;
            }
        }
    }
}

fn target_of(text: &str) -> Result<Target, String> {
    let path = expand_home(text)?;
    // The literal directory prefix, and what follows it.
    let mut literal = PathBuf::new();
    let mut rest = PathBuf::new();
    let mut in_glob = false;
    for component in path.components() {
        let name = component.as_os_str().to_string_lossy();
        if in_glob || (matches!(component, Component::Normal(_)) && has_glob(&name)) {
            in_glob = true;
            rest.push(component);
        } else {
            literal.push(component);
        }
    }
    let (root, missing) = resolve_existing(&literal)?;
    if in_glob {
        let pattern = root.join(&missing).join(&rest);
        let glob = Glob::new(&pattern.to_string_lossy())
            .map_err(|e| format!("{text:?} is not a valid glob: {}", e.kind()))?
            .compile_matcher();
        Ok(Target {
            root,
            matcher: Matcher::Glob(Box::new(glob)),
        })
    } else {
        Ok(Target {
            matcher: Matcher::Under(root.join(&missing)),
            root,
        })
    }
}

/// What one watcher callback means for a trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// Not ours: a read, or a path that matches none of the definition's paths.
    Ignore,
    /// A change at one of the paths.
    Changed,
    /// The platform dropped events (FSEvents `MustScanSubDirs`, from an overloaded daemon or a
    /// full buffer) and sent a notice for a directory instead. Which files changed is unknown, so
    /// the whole watched tree may have. The notice's path is a directory that matches no file
    /// glob, so it cannot be judged by path.
    Rescan,
    /// The watcher itself failed; treated as a rescan.
    Failed,
}

/// Judges one callback. A pure function of the event and the targets.
fn classify(result: &notify::Result<notify::Event>, targets: &[Target]) -> Verdict {
    let Ok(event) = result else {
        return Verdict::Failed;
    };
    if event.need_rescan() {
        return Verdict::Rescan;
    }
    let relevant = !matches!(event.kind, EventKind::Access(_))
        && (event.paths.is_empty()
            || event
                .paths
                .iter()
                .any(|p| targets.iter().any(|t| t.matcher.matches(p))));
    if relevant {
        Verdict::Changed
    } else {
        Verdict::Ignore
    }
}

/// True the first time it is called for `warned`, false after.
fn first_failure(warned: &AtomicBool) -> bool {
    !warned.swap(true, Ordering::Relaxed)
}

impl Watch for FsWatch {
    fn watch(&self, spec: &WatchSpec) -> reno_kernel::Result<WatchStream> {
        let fail = |message: String| {
            KernelError::Definitions(format!("trigger {}: {message}", spec.trigger))
        };
        let targets: Vec<Target> = spec
            .paths
            .iter()
            .map(|path| target_of(path))
            .collect::<Result<_, _>>()
            .map_err(fail)?;

        let (raw_tx, raw_rx) = mpsc::channel(1);
        let (out_tx, out_rx) = mpsc::channel(1);
        let roots: Vec<PathBuf> = targets.iter().map(|t| t.root.clone()).collect();
        let name = spec.trigger.clone();
        let warned = AtomicBool::new(false);
        let watcher = self
            .watchers
            .start(
                &roots,
                Box::new(move |result: notify::Result<notify::Event>| {
                    let verdict = classify(&result, &targets);
                    if verdict == Verdict::Failed
                        && let Err(error) = &result
                        && first_failure(&warned)
                    {
                        // The watcher lost track of something, so the paths may have changed.
                        // Say so once: a failing watcher can repeat itself without end.
                        tracing::warn!(trigger = %name, %error, "a file watcher reported an error");
                    }
                    if verdict != Verdict::Ignore {
                        // A full channel already holds a pending signal.
                        let _ = raw_tx.try_send(());
                    }
                }),
            )
            .map_err(fail)?;

        let stop = CancellationToken::new();
        self.tasks
            .spawn(debounce(raw_rx, out_tx, spec.debounce, stop.clone()));
        Ok(WatchStream::new(
            out_rx,
            Guard {
                _watcher: watcher,
                stop,
            },
        ))
    }
}

/// Turns bursts of signals into one: waits for `quiet` with no signal, re-arming the deadline
/// on every signal, then reports once. Ends when stopped or when the watcher is gone.
async fn debounce(
    mut raw: mpsc::Receiver<()>,
    out: mpsc::Sender<()>,
    quiet: Duration,
    stop: CancellationToken,
) {
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            signal = raw.recv() => if signal.is_none() { return },
        }
        let deadline = tokio::time::sleep(quiet);
        tokio::pin!(deadline);
        loop {
            tokio::select! {
                biased;
                () = stop.cancelled() => return,
                signal = raw.recv() => {
                    if signal.is_none() {
                        return;
                    }
                    deadline.as_mut().reset(tokio::time::Instant::now() + quiet);
                }
                () = &mut deadline => break,
            }
        }
        let _ = out.try_send(());
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::fs;

    use super::*;

    fn spec(paths: Vec<String>) -> WatchSpec {
        WatchSpec {
            trigger: "inbox".into(),
            paths,
            debounce: Duration::from_millis(100),
        }
    }

    /// A hang detector, not an expected latency: a wait for a change returns the moment the event
    /// arrives. The platform's event daemon (`fseventsd` on macOS) is shared by the whole
    /// machine and can lag by seconds when it is busy, so a short bound here only measures the
    /// load. The waits that expect silence stay short: they only have to outlast the debounce.
    const FAILSAFE: Duration = Duration::from_secs(60);

    /// Real files and the real watcher.
    async fn changed(stream: &mut WatchStream, within: Duration) -> bool {
        tokio::time::timeout(within, stream.changes.recv())
            .await
            .is_ok_and(|signal| signal.is_some())
    }

    /// Starts a real watch, waiting for the platform while it refuses: the machine has only a few
    /// watch streams for every process on it, and these tests are about what a working watch
    /// reports. (The daemon's own answer to a refusal is the scheduler's retry.)
    async fn watch_for_real(port: &FsWatch, spec: &WatchSpec) -> WatchStream {
        tokio::time::timeout(FAILSAFE, async {
            loop {
                match port.watch(spec) {
                    Ok(stream) => return stream,
                    Err(error) if error.to_string().contains("FSEvent stream") => {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    Err(error) => panic!("{error}"),
                }
            }
        })
        .await
        .expect("the platform never allowed a watch")
    }

    // ---- which events count (pure) ---------------------------------------------------------

    use notify::event::{AccessKind, CreateKind, Flag};

    fn event(kind: EventKind, paths: &[&Path]) -> notify::Event {
        paths.iter().fold(notify::Event::new(kind), |event, path| {
            event.add_path(path.to_path_buf())
        })
    }

    /// What FSEvents sends when it dropped events: kind `Other`, the rescan flag, the directory.
    fn rescan(path: &Path) -> notify::Event {
        event(EventKind::Other, &[path]).set_flag(Flag::Rescan)
    }

    fn glob_target(root: &Path, pattern: &str) -> Target {
        Target {
            root: root.to_path_buf(),
            matcher: Matcher::Glob(Box::new(
                Glob::new(&root.join(pattern).to_string_lossy())
                    .unwrap()
                    .compile_matcher(),
            )),
        }
    }

    #[test]
    fn a_rescan_notice_means_the_tree_may_have_changed_whatever_its_path() {
        let root = Path::new("/w/inbox");
        let targets = [glob_target(root, "*.txt")];
        let created = EventKind::Create(CreateKind::File);

        // A per-file event counts only when it matches the glob.
        assert_eq!(
            classify(&Ok(event(created, &[&root.join("a.txt")])), &targets),
            Verdict::Changed
        );
        assert_eq!(
            classify(&Ok(event(created, &[&root.join("notes.log")])), &targets),
            Verdict::Ignore
        );
        assert_eq!(
            classify(
                &Ok(event(
                    EventKind::Access(AccessKind::Any),
                    &[&root.join("a.txt")]
                )),
                &targets
            ),
            Verdict::Ignore
        );

        // A rescan notice names the watched root, a subdirectory, an ancestor, or nothing: none
        // of which match `*.txt`, and all of which mean "look again".
        for path in [
            root,
            Path::new("/w/inbox/sub"),
            Path::new("/w"),
            Path::new("/"),
        ] {
            assert_eq!(
                classify(&Ok(rescan(path)), &targets),
                Verdict::Rescan,
                "{path:?}"
            );
        }
        let pathless = notify::Event::new(EventKind::Other).set_flag(Flag::Rescan);
        assert_eq!(classify(&Ok(pathless), &targets), Verdict::Rescan);

        // Even flagged as a read: a rescan outranks the kind.
        let odd = event(EventKind::Access(AccessKind::Any), &[root]).set_flag(Flag::Rescan);
        assert_eq!(classify(&Ok(odd), &targets), Verdict::Rescan);
    }

    #[test]
    fn a_watcher_error_is_a_rescan_and_is_logged_only_once() {
        let targets = [glob_target(Path::new("/w"), "*.txt")];
        let error = || Err(notify::Error::generic("event buffer overflowed"));
        assert_eq!(classify(&error(), &targets), Verdict::Failed);

        let warned = AtomicBool::new(false);
        assert!(first_failure(&warned), "the first error is logged");
        assert!(!first_failure(&warned), "the next ones are not");
        assert!(!first_failure(&warned));
    }

    #[tokio::test(start_paused = true)]
    async fn many_signals_are_one_debounced_signal() {
        let (raw_tx, raw_rx) = mpsc::channel(1);
        let (out_tx, mut out_rx) = mpsc::channel(1);
        let stop = CancellationToken::new();
        let task = tokio::spawn(debounce(
            raw_rx,
            out_tx,
            Duration::from_millis(100),
            stop.clone(),
        ));
        // A burst of per-file signals and a rescan notice together are still one signal.
        for _ in 0..10 {
            let _ = raw_tx.try_send(());
            tokio::task::yield_now().await;
            tokio::time::advance(Duration::from_millis(20)).await;
        }
        tokio::time::advance(Duration::from_millis(101)).await;
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(out_rx.try_recv().is_ok());
        assert!(out_rx.try_recv().is_err(), "one burst, one signal");
        stop.cancel();
        task.await.unwrap();
    }

    // ---- real files and the real watcher ---------------------------------------------------
    //
    // These share one lock so the suite's own file churn does not overload the platform's event
    // daemon, and they accept any signal: under load the daemon sends a rescan notice instead of
    // per-file events, which is the same "look again" as far as a trigger is concerned.

    #[tokio::test]
    async fn a_matching_file_change_is_reported_after_it_settles() {
        let _serial = crate::test_serial().await;
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        fs::create_dir(&inbox).unwrap();
        let port = FsWatch::new();
        let pattern = format!("{}/*.txt", dir.path().join("inbox").display());
        let mut stream = watch_for_real(&port, &spec(vec![pattern])).await;

        // A burst of writes is signalled (the debounce itself is tested above with no files).
        for n in 0..5 {
            fs::write(inbox.join(format!("a{n}.txt")), "x").unwrap();
        }
        assert!(changed(&mut stream, FAILSAFE).await);

        drop(stream);
        port.shutdown().await;
    }

    #[tokio::test]
    async fn a_plain_directory_covers_what_is_created_under_it_later() {
        let _serial = crate::test_serial().await;
        let dir = tempfile::tempdir().unwrap();
        let port = FsWatch::new();
        let later = dir.path().join("not-yet");
        let mut stream = watch_for_real(&port, &spec(vec![later.display().to_string()])).await;
        fs::create_dir(&later).unwrap();
        fs::write(later.join("f"), "x").unwrap();
        assert!(changed(&mut stream, FAILSAFE).await);
        drop(stream);
        port.shutdown().await;
    }

    #[tokio::test]
    async fn paths_that_cannot_be_watched_are_an_error() {
        let port = FsWatch::new();
        assert!(
            port.watch(&spec(vec!["/".into(), "no-such".into()]))
                .is_err()
        );
        let bad = port.watch(&spec(vec!["/tmp/[".into()]));
        assert!(bad.is_err());
    }
}
