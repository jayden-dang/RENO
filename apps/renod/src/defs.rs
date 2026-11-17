//! The `Definitions` port over the file system (ARCHITECTURE 3.1, 3.2, 9): `~/.reno` as a git
//! repository of TOML files.
//!
//! - [`bootstrap`] creates the directory on first start, with an example bot and the safe
//!   policy, and runs `git init` when git is installed.
//! - [`FsDefinitions`] reads the files raw and watches them with `notify`, debounced. The
//!   kernel validates what it reads.
//!
//! # The watch is event-driven (ARCH-10)
//!
//! The platform's watcher calls back on its own thread for every file event. The callback only
//! does a `try_send` into a channel of capacity 1, so a flood is one pending signal. One task
//! then waits for quiet: every signal re-arms a deadline, and when the deadline passes with no
//! further signal the kernel is told once. Nothing wakes on an interval.
//!
//! A rescan notice (the platform dropped events and named a directory) and a watcher error both
//! count whatever their path: the files that changed are unknown, and a read looks at them all.
//!
//! # Starting the watch
//!
//! `watch` returns at once; the platform stream is started on a blocking thread, because with
//! `fseventsd` busy a start has taken half a second and the daemon should not boot that slowly.
//! When the stream is up a reload is signalled, so an edit made while it was starting is read.
//!
//! # When the watch cannot start
//!
//! The platform has a finite pool of watch streams shared by every process on the machine (see
//! [`watcher`](crate::watcher)), so a start can fail for a while and work later. Then the daemon
//! keeps serving the last good definitions, says so ([`FsDefinitions::watch_problem`], shown in
//! `daemon.status`) and logs one warning, and tries again after a growing delay
//! (`reno_kernel::WatchBackoff`). That is a retry-on-error timer, armed only while a start is
//! failing and ended by the first start that works; it never asks whether files changed
//! (ARCH-10). When the watch starts, a reload is signalled, because edits made meanwhile were
//! missed.
//!
//! Only events at `policy.toml`, `bots/` and `triggers/` count. Whatever else lives in the
//! directory (a `.git/`, a scratch folder) never reloads the definitions, and neither do file
//! reads (`Access` events), so loading them does not reload them either.
//!
//! # Layout of what is read
//!
//! `bots/*.toml`, `triggers/*.toml` and `policy.toml` under the root, no deeper. Anything else,
//! including a file with another extension, is ignored. Each directory is read up to
//! [`MAX_FILES_PER_KIND`] files, and each file up to [`MAX_FILE_BYTES`] bytes.

use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use futures::future::BoxFuture;
use notify::EventKind;
use reno_kernel::{DefSnapshot, Definitions, KernelError, WatchBackoff};
use reno_proto::defs::{
    DefError, DefFile, DefKind, MAX_FILE_BYTES, MAX_FILES_PER_KIND, POLICY_NAME, defaults,
};
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::watcher::{NotifyWatchers, WatcherFactory, WatcherGuard};

/// How long the files must be quiet before a change is reported.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(250);

/// How long `git init` may take before it is given up on.
const GIT_DEADLINE: Duration = Duration::from_secs(10);

/// The environment variable that overrides the definitions directory.
pub const DEFS_DIR_ENV: &str = "RENO_DEFS_DIR";

/// The definitions directory, `~/.reno`, or `None` when `HOME` is not set and nothing overrides
/// it. [`DEFS_DIR_ENV`] wins.
pub fn default_defs_dir() -> Option<PathBuf> {
    defs_dir_from(|key| std::env::var_os(key))
}

fn defs_dir_from(env: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    let non_empty = |key| env(key).filter(|v| !v.is_empty()).map(PathBuf::from);
    non_empty(DEFS_DIR_ENV).or_else(|| non_empty("HOME").map(|home| home.join(".reno")))
}

// ---- first start --------------------------------------------------------------------------

/// What `git init` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GitOutcome {
    /// The directory is now a git repository.
    Initialized,
    /// The directory already is one (it has a `.git`), so `git init` was not run.
    AlreadyARepository,
    /// `git` is not installed (or not on `PATH`).
    Unavailable,
    /// `git init` ran and failed, or did not finish in time.
    Failed(String),
}

/// How `git init` is run: a function, so tests can say git is missing.
pub type GitInit = fn(PathBuf) -> BoxFuture<'static, GitOutcome>;

/// Runs `git init` in `root`.
pub fn git_init(root: PathBuf) -> BoxFuture<'static, GitOutcome> {
    Box::pin(async move {
        let mut command = tokio::process::Command::new("git");
        command
            .arg("init")
            .arg("--quiet")
            .current_dir(&root)
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true);
        let output = match timeout(GIT_DEADLINE, command.output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) if error.kind() == io::ErrorKind::NotFound => {
                return GitOutcome::Unavailable;
            }
            Ok(Err(error)) => return GitOutcome::Failed(error.to_string()),
            Err(_) => return GitOutcome::Failed(format!("did not finish in {GIT_DEADLINE:?}")),
        };
        if output.status.success() {
            GitOutcome::Initialized
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            GitOutcome::Failed(stderr.lines().next().unwrap_or("no message").to_owned())
        }
    })
}

/// What [`bootstrap`] found and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bootstrapped {
    /// Whether the example files were written now. When not, the directory was left as it is.
    pub created: bool,
    /// What happened to git; `None` when nothing was created.
    pub git: Option<GitOutcome>,
}

/// First start: when `root` holds no definitions yet, that is none of `bots/`, `triggers/` and
/// `policy.toml`, creates them with an example bot and the safe policy, then runs `git` (see
/// [`git_init`]) there unless `root` is already a repository. The directory itself is created,
/// private (0700), if it is missing; an existing one keeps its mode and its contents. A file is
/// never overwritten. As soon as any of the three exists, `root` is left alone: a user who has
/// deleted the example has not asked for it back.
///
/// The result says what happened, and each case is logged: a missing git is a warning that says
/// the definitions have no history and how to fix that, not an error, because the daemon works
/// without it.
pub async fn bootstrap(root: &Path, git: GitInit) -> io::Result<Bootstrapped> {
    let owned = root.to_owned();
    let created = tokio::task::spawn_blocking(move || scaffold(&owned))
        .await
        .map_err(io::Error::other)??;
    if !created {
        return Ok(Bootstrapped {
            created: false,
            git: None,
        });
    }
    tracing::info!(dir = %root.display(), "created an example bot and the default policy");
    let outcome = if root.join(".git").exists() {
        GitOutcome::AlreadyARepository
    } else {
        git(root.to_owned()).await
    };
    match &outcome {
        GitOutcome::AlreadyARepository => {}
        GitOutcome::Initialized => {
            tracing::info!(dir = %root.display(), "initialised a git repository for the definitions");
        }
        GitOutcome::Unavailable => tracing::warn!(
            dir = %root.display(),
            "git was not found, so {} is not a git repository and changes to the definitions will \
             have no history; install git and run `git init` there",
            root.display()
        ),
        GitOutcome::Failed(reason) => tracing::warn!(
            dir = %root.display(),
            %reason,
            "`git init` failed, so the definitions are not in a git repository; run it by hand"
        ),
    }
    Ok(Bootstrapped {
        created: true,
        git: Some(outcome),
    })
}

/// Creates the directory if needed, and the example files. `false`, changing nothing, when any
/// definition file or directory exists.
fn scaffold(root: &Path) -> io::Result<bool> {
    if ["bots", "triggers", "policy.toml"]
        .iter()
        .any(|part| root.join(part).symlink_metadata().is_ok())
    {
        return Ok(false);
    }
    if root.exists() {
        if !root.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is not a directory", root.display()),
            ));
        }
    } else {
        crate::paths::ensure_private_dir(root)?;
    }
    for dir in ["bots", "triggers"] {
        fs::create_dir(root.join(dir))?;
    }
    for (path, text) in [
        ("bots/example.toml", defaults::EXAMPLE_BOT),
        ("policy.toml", defaults::POLICY),
    ] {
        write_new(&root.join(path), text)?;
    }
    Ok(true)
}

fn write_new(path: &Path, text: &str) -> io::Result<()> {
    use std::io::Write;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?
        .write_all(text.as_bytes())
}

// ---- the port -----------------------------------------------------------------------------

/// The definitions in a directory on disk.
#[derive(Debug)]
pub struct FsDefinitions {
    root: PathBuf,
    debounce: Duration,
    watchers: Arc<dyn WatcherFactory>,
    retry: WatchBackoff,
    /// Whether `watch` was called.
    watching: AtomicBool,
    /// Where the running watcher lives, once one has started. Dropping it stops its stream.
    state: Arc<WatchState>,
    tasks: TaskTracker,
    stop: CancellationToken,
}

/// What the start of the watch leaves behind; shared with the task that retries it.
#[derive(Debug, Default)]
struct WatchState {
    watcher: Mutex<Option<WatcherGuard>>,
    /// Why the watch is not running, while its start is being retried.
    problem: Mutex<Option<String>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Drop for FsDefinitions {
    /// A port dropped without `shutdown` still lets go of its stream, and stops its tasks.
    fn drop(&mut self) {
        self.stop.cancel();
        lock(&self.state.watcher).take();
    }
}

impl FsDefinitions {
    /// The definitions in `root`, which must exist. The path is resolved once, so the watcher's
    /// paths (which the platform reports resolved) compare with it.
    pub fn open(root: &Path, debounce: Duration) -> io::Result<Self> {
        Ok(Self {
            root: fs::canonicalize(root)?,
            debounce,
            watchers: Arc::new(NotifyWatchers),
            retry: WatchBackoff::default(),
            watching: AtomicBool::new(false),
            state: Arc::default(),
            tasks: TaskTracker::new(),
            stop: CancellationToken::new(),
        })
    }

    /// The same, starting its watcher with `watchers`.
    #[must_use]
    pub fn with_watchers(mut self, watchers: Arc<dyn WatcherFactory>) -> Self {
        self.watchers = watchers;
        self
    }

    /// The same, waiting as `retry` says between starts of a watch that failed to start.
    #[must_use]
    pub fn with_retry(mut self, retry: WatchBackoff) -> Self {
        self.retry = retry;
        self
    }

    /// The resolved directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Why edits are not being noticed: the watch could not start and is being retried. `None`
    /// when it is running, or was never asked for.
    pub fn watch_problem(&self) -> Option<String> {
        lock(&self.state.problem).clone()
    }

    /// Stops watching and waits for the tasks. Safe to call twice.
    pub async fn shutdown(&self) {
        self.stop.cancel();
        lock(&self.state.watcher).take();
        self.tasks.close();
        self.tasks.wait().await;
    }
}

fn start_watcher(
    watchers: &dyn WatcherFactory,
    root: &Path,
    raw: mpsc::Sender<()>,
) -> Result<WatcherGuard, String> {
    let owned = root.to_owned();
    let warned = AtomicBool::new(false);
    watchers.start(
        &[root.to_owned()],
        Box::new(move |result: notify::Result<notify::Event>| {
            if let Err(error) = &result
                && !warned.swap(true, Ordering::Relaxed)
            {
                // Say so once: a failing watcher can repeat itself without end.
                tracing::warn!(%error, "the definitions watcher reported an error");
            }
            if signals_reload(&result, &owned) {
                // A full channel already holds a pending signal.
                let _ = raw.try_send(());
            }
        }),
    )
}

/// The first start of the watch, on a blocking thread (the platform call waits for its stream).
/// When it works a reload is signalled: the kernel read the files when `watch` returned, and an
/// edit made before the stream was up would otherwise be missed. When it fails, [`retry_start`]
/// takes over.
async fn first_start(
    watchers: Arc<dyn WatcherFactory>,
    root: PathBuf,
    backoff: WatchBackoff,
    state: Arc<WatchState>,
    raw: mpsc::Sender<()>,
    stop: CancellationToken,
) {
    let started = {
        let (watchers, root, raw) = (Arc::clone(&watchers), root.clone(), raw.clone());
        tokio::task::spawn_blocking(move || start_watcher(&*watchers, &root, raw))
            .await
            .unwrap_or_else(|error| Err(format!("the watch start did not finish: {error}")))
    };
    match started {
        Ok(guard) => {
            {
                let mut slot = lock(&state.watcher);
                // As in `retry_start`: a guard that arrives after `shutdown` is dropped.
                if stop.is_cancelled() {
                    return;
                }
                *slot = Some(guard);
            }
            let _ = raw.try_send(());
        }
        Err(error) => {
            tracing::warn!(
                %error,
                "cannot watch the definitions; keeping what is loaded and trying again"
            );
            *lock(&state.problem) = Some(error);
            retry_start(watchers, root, backoff, state, raw, stop).await;
        }
    }
}

/// Starts the watch again until it works or `stop` is cancelled. Each wait is a deadline that
/// exists only because the last start failed (ARCH-10).
async fn retry_start(
    watchers: Arc<dyn WatcherFactory>,
    root: PathBuf,
    backoff: WatchBackoff,
    state: Arc<WatchState>,
    raw: mpsc::Sender<()>,
    stop: CancellationToken,
) {
    let mut failures = 1u32;
    loop {
        tokio::select! {
            biased;
            () = stop.cancelled() => return,
            () = tokio::time::sleep(backoff.delay(failures)) => {}
        }
        match start_watcher(&*watchers, &root, raw.clone()) {
            Ok(guard) => {
                {
                    let mut slot = lock(&state.watcher);
                    // `shutdown` cancels before it takes the slot, so a guard stored here is
                    // always taken after; one that arrives late is dropped.
                    if stop.is_cancelled() {
                        return;
                    }
                    *slot = Some(guard);
                }
                lock(&state.problem).take();
                tracing::info!(
                    attempts = failures.saturating_add(1),
                    "the definitions watch started; edits are noticed again"
                );
                // Edits made while nothing watched were missed.
                let _ = raw.try_send(());
                return;
            }
            Err(error) => {
                failures = failures.saturating_add(1);
                *lock(&state.problem) = Some(error);
            }
        }
    }
}

#[async_trait]
impl Definitions for FsDefinitions {
    async fn snapshot(&self) -> reno_kernel::Result<DefSnapshot> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || read_snapshot(&root))
            .await
            .map_err(|e| KernelError::Definitions(e.to_string()))?
            .map_err(|e| KernelError::Definitions(format!("{}: {e}", self.root.display())))
    }

    fn watch(&self) -> reno_kernel::Result<mpsc::Receiver<()>> {
        if self.watching.swap(true, Ordering::SeqCst) {
            return Err(KernelError::Definitions(
                "the definitions are already being watched".into(),
            ));
        }
        let (raw_tx, raw_rx) = mpsc::channel(1);
        let (out_tx, out_rx) = mpsc::channel(1);
        self.tasks
            .spawn(debounce(raw_rx, out_tx, self.debounce, self.stop.clone()));

        // The first start runs off the boot path: with `fseventsd` busy it has taken half a
        // second, and the daemon serves the definitions it just read meanwhile.
        self.tasks.spawn(first_start(
            Arc::clone(&self.watchers),
            self.root.clone(),
            self.retry,
            Arc::clone(&self.state),
            raw_tx,
            self.stop.clone(),
        ));
        Ok(out_rx)
    }
}

/// Whether one watcher callback asks for a reload. A snapshot always reads the whole directory,
/// so a signal is a rescan. That is what the two cases with no usable path want: an error (the
/// watcher lost track of something) and a rescan notice (the platform dropped events and named a
/// directory instead, e.g. FSEvents `MustScanSubDirs` under load, possibly one that is not a
/// definition directory).
fn signals_reload(result: &notify::Result<notify::Event>, root: &Path) -> bool {
    match result {
        Err(_) => true,
        Ok(event) => event.need_rescan() || is_relevant(event, root),
    }
}

/// Whether an event can change what a snapshot reads: something at `policy.toml`, under `bots/`
/// or under `triggers/` (or the directories themselves), or the root. Anything else in the
/// directory, a scratch folder or `.git/`, is not ours, and neither are reads. An event with no
/// path, or one outside the root, is assumed to count: the watcher could not say what changed.
fn is_relevant(event: &notify::Event, root: &Path) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    event.paths.is_empty()
        || event
            .paths
            .iter()
            .any(|path| match path.strip_prefix(root) {
                Err(_) => true,
                Ok(relative) => relative.components().next().is_none_or(|first| {
                    ["policy.toml", "bots", "triggers"]
                        .map(OsStr::new)
                        .contains(&first.as_os_str())
                }),
            })
}

/// Turns bursts of signals into one: waits for `quiet` with no signal, re-arming the deadline on
/// every signal, then tells the kernel once. Ends when cancelled or when the watcher is gone.
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
                    deadline.as_mut().reset(Instant::now() + quiet);
                }
                () = &mut deadline => break,
            }
        }
        // The kernel drains this channel before each read; one pending signal is enough.
        let _ = out.try_send(());
    }
}

// ---- reading ------------------------------------------------------------------------------

/// Reads the definition files under `root`, sorted by kind and name.
fn read_snapshot(root: &Path) -> io::Result<DefSnapshot> {
    let mut snapshot = DefSnapshot::default();
    for (dir, kind) in [("bots", DefKind::Bot), ("triggers", DefKind::Trigger)] {
        read_dir_of(&root.join(dir), dir, kind, &mut snapshot)?;
    }
    let policy = root.join("policy.toml");
    match fs::metadata(&policy) {
        Ok(meta) if meta.is_file() => {
            read_file(&policy, DefKind::Policy, POLICY_NAME, &mut snapshot);
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => snapshot.problems.push(DefError::new(
            "policy.toml",
            "",
            format!("cannot read the file: {error}"),
        )),
    }
    snapshot
        .files
        .sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
    Ok(snapshot)
}

fn read_dir_of(
    dir: &Path,
    shown: &str,
    kind: DefKind,
    snapshot: &mut DefSnapshot,
) -> io::Result<()> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        // A directory that is not there holds no definitions. Not being able to list one that
        // is there is a failure of the read as a whole.
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let mut files = 0usize;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let Some(stem) = toml_stem(&path) else {
            continue;
        };
        let Some(stem) = stem else {
            snapshot.problems.push(DefError::new(
                format!("{shown}/{}", entry.file_name().to_string_lossy()),
                "",
                "the file name is not valid UTF-8",
            ));
            continue;
        };
        // `metadata` follows a symlink; a link to a directory, or to nothing, is skipped.
        if !fs::metadata(&path).is_ok_and(|meta| meta.is_file()) {
            continue;
        }
        files += 1;
        if files > MAX_FILES_PER_KIND {
            snapshot.problems.push(DefError::new(
                format!("{shown}/"),
                "",
                format!("holds more than {MAX_FILES_PER_KIND} files; the rest were not read"),
            ));
            break;
        }
        read_file(&path, kind, &stem, snapshot);
    }
    Ok(())
}

/// `Some(Some(stem))` for `<stem>.toml`, `Some(None)` for a `.toml` file whose stem is not
/// UTF-8, and `None` for any other file.
fn toml_stem(path: &Path) -> Option<Option<String>> {
    if path.extension()? != "toml" {
        return None;
    }
    Some(path.file_stem()?.to_str().map(str::to_owned))
}

/// Reads one file, at most one byte past the limit, so an oversized file is recognised without
/// being read whole. Anything that cannot be read becomes a problem.
fn read_file(path: &Path, kind: DefKind, name: &str, snapshot: &mut DefSnapshot) {
    let shown = kind.path_for(name);
    let limit = u64::try_from(MAX_FILE_BYTES)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let mut bytes = Vec::new();
    let read = fs::File::open(path).and_then(|file| file.take(limit).read_to_end(&mut bytes));
    if let Err(error) = read {
        snapshot.problems.push(DefError::new(
            shown,
            "",
            format!("cannot read the file: {error}"),
        ));
        return;
    }
    // An oversized file arrives as its first MAX_FILE_BYTES + 1 bytes, which may end inside a
    // character. Lossy decoding never shortens the text, so the loader still sees it as too
    // large and says so, instead of calling it invalid UTF-8.
    let text = if bytes.len() > MAX_FILE_BYTES {
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    } else {
        String::from_utf8(bytes)
    };
    match text {
        Ok(text) => snapshot.files.push(DefFile {
            kind,
            name: name.to_owned(),
            text,
        }),
        Err(_) => snapshot
            .problems
            .push(DefError::new(shown, "", "the file is not valid UTF-8")),
    }
}

#[cfg(test)]
mod tests;
