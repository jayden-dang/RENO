//! The `Watch` port over the file system, for `fs` triggers (ARCHITECTURE 7, ARCH-10).
//!
//! Each watch is one `notify` watcher plus one debounce task. The watcher's callback runs on the
//! platform's thread and only does a `try_send` into a channel of capacity 1, so a flood is one
//! pending signal. The task waits for quiet: every signal re-arms a deadline, and when it passes
//! with no further signal the kernel is told once, with `debounce` taken from the definition.
//! Nothing wakes on an interval.
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
use std::time::Duration;

use globset::{Glob, GlobMatcher};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use reno_kernel::{KernelError, Watch, WatchSpec, WatchStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

/// The `Watch` port: `notify`, debounced per definition.
#[derive(Debug, Default)]
pub struct FsWatch {
    tasks: TaskTracker,
}

impl FsWatch {
    /// A port with no watches yet.
    pub fn new() -> Self {
        Self::default()
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
    _watcher: RecommendedWatcher,
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
        let mut watcher =
            notify::recommended_watcher(move |event: notify::Result<notify::Event>| match event {
                Ok(event) => {
                    let relevant = !matches!(event.kind, EventKind::Access(_))
                        && (event.paths.is_empty()
                            || event
                                .paths
                                .iter()
                                .any(|p| targets.iter().any(|t| t.matcher.matches(p))));
                    if relevant {
                        // A full channel already holds a pending signal.
                        let _ = raw_tx.try_send(());
                    }
                }
                Err(error) => {
                    // The watcher lost track of something: say so, the paths may have changed.
                    tracing::warn!(trigger = %name, %error, "a file watcher reported an error");
                    let _ = raw_tx.try_send(());
                }
            })
            .map_err(|e| fail(format!("cannot create a watcher: {e}")))?;
        for root in &roots {
            watcher
                .watch(root, RecursiveMode::Recursive)
                .map_err(|e| fail(format!("cannot watch {}: {e}", root.display())))?;
        }

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

    /// Real files and the real watcher, so the waits are real and short.
    async fn changed(stream: &mut WatchStream, within: Duration) -> bool {
        tokio::time::timeout(within, stream.changes.recv())
            .await
            .is_ok_and(|signal| signal.is_some())
    }

    #[tokio::test]
    async fn a_matching_file_change_is_reported_once_after_it_settles() {
        let dir = tempfile::tempdir().unwrap();
        let inbox = dir.path().join("inbox");
        fs::create_dir(&inbox).unwrap();
        let port = FsWatch::new();
        let pattern = format!("{}/*.txt", dir.path().join("inbox").display());
        let mut stream = port.watch(&spec(vec![pattern])).unwrap();

        // A burst of writes is one signal.
        for n in 0..5 {
            fs::write(inbox.join(format!("a{n}.txt")), "x").unwrap();
        }
        assert!(changed(&mut stream, Duration::from_secs(10)).await);
        assert!(
            !changed(&mut stream, Duration::from_millis(400)).await,
            "one burst, one signal"
        );

        // A file that does not match the glob is not a change.
        fs::write(inbox.join("notes.log"), "x").unwrap();
        assert!(!changed(&mut stream, Duration::from_millis(500)).await);

        drop(stream);
        port.shutdown().await;
    }

    #[tokio::test]
    async fn a_plain_directory_covers_what_is_created_under_it_later() {
        let dir = tempfile::tempdir().unwrap();
        let port = FsWatch::new();
        let later = dir.path().join("not-yet");
        let mut stream = port
            .watch(&spec(vec![later.display().to_string()]))
            .unwrap();
        fs::create_dir(&later).unwrap();
        fs::write(later.join("f"), "x").unwrap();
        assert!(changed(&mut stream, Duration::from_secs(10)).await);
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
