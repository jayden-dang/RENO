//! The Terminal service (RP2): shells on PTYs that the daemon holds for a person, behind
//! `term.open`, `term.attach`, `term.write`, `term.resize`, `term.close`, `term.status` and
//! `term.list`.
//!
//! Like Files, this is a person-driven service, not run state, so it lives in the composition
//! root and not in the kernel (ARCH-7). The handlers decide which tree a terminal starts in and
//! whether the run is live; everything here works on a directory and a flag. The daemon only
//! moves bytes: the app owns the emulator.
//!
//! # Modules
//!
//! - [`pty`]: the [`PtySpawner`] port and [`RealPty`]. The only code that starts a shell.
//! - [`fake`]: [`FakePty`], shells in memory, for tests (ARCH-16).
//! - [`sandbox`]: the D7 seatbelt profile and the [`Limits`] it is made from.
//! - `env`: the shell and its environment.
//! - [`error`]: [`TermError`] and its stable codes (ARCH-14).
//!
//! # How a terminal runs
//!
//! One blocking thread reads the PTY and hands chunks to the session's pump task over a bounded
//! channel, so a slow consumer slows the reader and nothing grows. The pump batches output on a
//! deadline (the first chunk after a quiet spell goes out at once, a stream waits out
//! [`Timing::batch`] between emissions: nothing polls, ARCH-10), numbers it, and appends it to a
//! ring of [`TERM_REPLAY_BYTES`]. `term.attach` is a cursor over that ring: it is woken by a
//! watch of the newest sequence number and copies from the ring on demand, so a subscriber owns
//! no queue of its own, a slow one finds its place gone and is told so with a `Gap` (ARCH-12),
//! and a detached viewer comes back with `after_seq` and replays.
//!
//! Input goes through a per-terminal lane: a bounded queue and one task that applies writes and
//! resizes in order, each on a blocking thread, so a shell that does not read its input stalls
//! its own lane and nothing else. Every task is owned by one `TaskTracker` (ARCH-11).
//!
//! # What this is not
//!
//! - **Not redacted.** A terminal is the person's own shell: its output is shown as it is. The
//!   run's secrets are not in its environment, and the environment drops `RENO_*`; what the
//!   person types or prints is theirs.
//! - **Not persistent.** Terminals die with the daemon (a restart, a reinstall): the shells are
//!   its children and `shutdown` ends them. A viewer shows "the daemon restarted; this shell
//!   ended" and reopens. Scrollback is lost.
//! - **Not a way to lift the sandbox.** A shell opened under limits keeps them; a viewer reopens.
//!
//! # Where the shell starts
//!
//! In the directory the Files tab serves for the run: the thread's worktree, or for a run with
//! none its own folder (`in_place`, which is the person's real folder and not sandboxed, because
//! the guard does not watch it). Terminals are keyed by that directory, so every run of a thread
//! sees them, and removing the worktree closes them ([`Terminals::close_dir`]).

pub mod error;
pub mod fake;
pub mod pty;
pub mod sandbox;

mod env;
mod session;

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::Stream;
use reno_proto::{
    TERM_EXITED_TTL_SECS, TERM_MAX_DIM, TERM_MAX_INPUT_BYTES, TERM_MAX_TERMINALS, TermEvent,
    TermId, TermInfo, TermOpened, TermStatusResult, decode_bytes,
};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

pub use error::TermError;
pub use fake::{FakeProcess, FakePty, Op};
pub use pty::{PtySpawner, RealPty, SpawnError, SpawnSpec};
pub use sandbox::{Limits, LimitsError};
use session::{Cmd, Session};

/// How long things take. The defaults are for the daemon; tests shorten them.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// The shortest gap between two output emissions of one terminal while output keeps coming.
    pub batch: Duration,
    /// How long a closed terminal's shell has to end after it is hung up on before it is killed.
    pub kill_grace: Duration,
    /// How long after the shell ended the output may keep flowing (a background job that holds
    /// the terminal open) before the terminal reports its exit anyway.
    pub eof_grace: Duration,
    /// How long an exited terminal stays before it is reaped.
    pub exited_ttl: Duration,
}

impl Default for Timing {
    fn default() -> Self {
        Self {
            batch: Duration::from_millis(8),
            kill_grace: Duration::from_secs(3),
            eof_grace: Duration::from_secs(1),
            exited_ttl: Duration::from_secs(TERM_EXITED_TTL_SECS),
        }
    }
}

/// What `term.open` was asked for, once the handler has decided where and how.
#[derive(Debug, Clone)]
pub struct OpenRequest {
    /// The directory the shell starts in.
    pub dir: PathBuf,
    /// The run has no worktree: `dir` is its own folder. Never sandboxed.
    pub in_place: bool,
    /// The run is live, so the shell must run under the sandbox (unless `in_place`).
    pub live: bool,
    /// The width, in cells.
    pub cols: u16,
    /// The height, in cells.
    pub rows: u16,
    /// A command to run in the shell once it starts.
    pub command: Option<String>,
}

/// The Terminal service.
#[derive(Debug, Clone)]
pub struct Terminals {
    inner: Arc<Inner>,
}

#[derive(Debug)]
pub(crate) struct Inner {
    spawner: Option<Arc<dyn PtySpawner>>,
    base_env: Vec<(OsString, OsString)>,
    sessions: Mutex<HashMap<TermId, Arc<Session>>>,
    /// Terminals being started: counted against the cap before they are in `sessions`.
    opening: AtomicUsize,
    tracker: TaskTracker,
    stop: CancellationToken,
    timing: Timing,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Terminals {
    /// A service that starts shells with `spawner`, in `base_env` minus Reno's own variables. With
    /// no spawner every method answers [`TermError::Disabled`].
    pub fn new(
        spawner: Option<Arc<dyn PtySpawner>>,
        base_env: Vec<(OsString, OsString)>,
        timing: Timing,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                spawner,
                base_env,
                sessions: Mutex::new(HashMap::new()),
                opening: AtomicUsize::new(0),
                tracker: TaskTracker::new(),
                stop: CancellationToken::new(),
                timing,
            }),
        }
    }

    /// The daemon can start shells.
    pub fn enabled(&self) -> bool {
        self.inner.spawner.is_some()
    }

    /// `term.open`.
    pub async fn open(&self, request: OpenRequest) -> Result<TermOpened, TermError> {
        let inner = &self.inner;
        let spawner = inner.spawner.clone().ok_or(TermError::Disabled)?;
        // Connections stay open while the daemon shuts down: no shell is started into it.
        if inner.stop.is_cancelled() {
            return Err(TermError::ShuttingDown);
        }
        // The shell starts in the real path: what a tab shows, and what a sandbox profile names.
        let real = tokio::fs::canonicalize(&request.dir).await;
        let real = match real {
            Ok(real)
                if tokio::fs::metadata(&real)
                    .await
                    .is_ok_and(|meta| meta.is_dir()) =>
            {
                real
            }
            _ => {
                return Err(TermError::FolderUnavailable(format!(
                    "{} is not a folder any more",
                    request.dir.display()
                )));
            }
        };
        // The sandbox is decided before anything is started: a terminal on a live run either has
        // its limits or is not opened.
        let limits = if request.live && !request.in_place {
            let dir = request.dir.clone();
            let made = tokio::task::spawn_blocking(move || Limits::for_worktree(&dir))
                .await
                .map_err(|error| TermError::Spawn(error.to_string()))?;
            Some(made.map_err(|error| TermError::SandboxUnavailable(error.to_string()))?)
        } else {
            None
        };
        let slot = inner.reserve()?;

        let shell = env::select_shell(&inner.base_env);
        let shell_name = env::shell_name(&shell);
        let id = new_id();
        let mut script = None;
        let mut environment = env::child_env(inner.base_env.clone());
        let mut bootstrap = None;
        if let Some(command) = &request.command {
            let (file, input) = write_action_script(&shell_name, command)
                .map_err(|error| TermError::Spawn(format!("the action script: {error}")))?;
            environment.push((ACTION_SCRIPT_VAR.into(), file.path().into()));
            script = Some(file);
            bootstrap = Some(input);
        }
        let (cols, rows) = clamp(request.cols, request.rows);
        let spec = SpawnSpec {
            program: shell,
            args: vec!["-l".into()],
            cwd: real.clone(),
            env: environment,
            cols,
            rows,
            limits: limits.clone(),
        };
        let parts = tokio::task::spawn_blocking(move || spawner.spawn(spec))
            .await
            .map_err(|error| TermError::Spawn(error.to_string()))?
            .map_err(|error| match error {
                SpawnError::SandboxUnavailable(why) => TermError::SandboxUnavailable(why),
                SpawnError::Failed(why) => TermError::Spawn(why),
            })?;

        let sandboxed = limits.is_some();
        let session = Session::start(
            inner,
            session::Start {
                id: id.clone(),
                dir: request.dir,
                cwd: real.display().to_string(),
                shell: shell_name.clone(),
                sandboxed,
                in_place: request.in_place,
                script,
            },
            parts,
        )
        .map_err(|error| TermError::Spawn(format!("its threads: {error}")))?;
        lock(&inner.sessions).insert(id.clone(), Arc::clone(&session));
        drop(slot);
        if let Some(input) = bootstrap {
            // The lane is empty and bounded; this is its first entry.
            session.send(Cmd::Write(input.into_bytes()), &id)?;
        }
        Ok(TermOpened {
            id,
            cwd: session.cwd.clone(),
            shell: shell_name,
            sandboxed,
            in_place: request.in_place,
        })
    }

    fn session(&self, id: &TermId) -> Result<Arc<Session>, TermError> {
        if self.inner.spawner.is_none() {
            return Err(TermError::Disabled);
        }
        lock(&self.inner.sessions)
            .get(id)
            .cloned()
            .ok_or_else(|| TermError::NotFound(id.clone()))
    }

    /// `term.attach`: output after `after_seq`, then live, ending after the exit.
    pub fn attach(
        &self,
        id: &TermId,
        after_seq: Option<u64>,
    ) -> Result<impl Stream<Item = TermEvent> + Send + 'static + use<>, TermError> {
        Ok(session::attach(self.session(id)?, after_seq))
    }

    /// `term.write`: `data` (base64) queued for the shell, in order.
    pub fn write(&self, id: &TermId, data: &str) -> Result<(), TermError> {
        let bytes =
            decode_bytes(data).ok_or_else(|| TermError::BadInput("not valid base64".into()))?;
        if bytes.len() > TERM_MAX_INPUT_BYTES {
            return Err(TermError::BadInput(format!(
                "{} bytes is over the limit of {TERM_MAX_INPUT_BYTES}",
                bytes.len()
            )));
        }
        let session = self.session(id)?;
        session.send(Cmd::Write(bytes), id)
    }

    /// `term.resize`, in the same order as the writes. Ignored for an exited shell.
    pub fn resize(&self, id: &TermId, cols: u16, rows: u16) -> Result<(), TermError> {
        let session = self.session(id)?;
        let (cols, rows) = clamp(cols, rows);
        match session.send(Cmd::Resize { cols, rows }, id) {
            Err(TermError::Exited(_)) => Ok(()),
            other => other,
        }
    }

    /// `term.status`.
    pub fn status(&self, id: &TermId) -> Result<TermStatusResult, TermError> {
        Ok(self.session(id)?.status())
    }

    /// `term.list`: the terminals in `dir`, oldest first.
    pub fn list(&self, dir: &std::path::Path) -> Vec<TermInfo> {
        let mut found: Vec<TermInfo> = lock(&self.inner.sessions)
            .values()
            .filter(|session| session.dir == dir)
            .map(|session| session.info())
            .collect();
        found.sort_by(|a, b| (a.opened_at_ms, a.id.as_str()).cmp(&(b.opened_at_ms, b.id.as_str())));
        found
    }

    /// `term.close`: ends the shell and forgets the terminal.
    pub fn close(&self, id: &TermId) -> Result<(), TermError> {
        let session = lock(&self.inner.sessions)
            .remove(id)
            .ok_or_else(|| TermError::NotFound(id.clone()))?;
        self.inner.end(&session);
        Ok(())
    }

    /// Closes every terminal in `dir` (its worktree is going away) and says which.
    pub fn close_dir(&self, dir: &std::path::Path) -> Vec<TermId> {
        let gone: Vec<Arc<Session>> = {
            let mut sessions = lock(&self.inner.sessions);
            let ids: Vec<TermId> = sessions
                .values()
                .filter(|session| session.dir == dir)
                .map(|session| session.id.clone())
                .collect();
            ids.iter().filter_map(|id| sessions.remove(id)).collect()
        };
        let mut ids: Vec<TermId> = gone.iter().map(|session| session.id.clone()).collect();
        ids.sort();
        for session in &gone {
            self.inner.end(session);
        }
        ids
    }

    /// Every directory that has a terminal, exited ones included.
    pub fn dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = lock(&self.inner.sessions)
            .values()
            .map(|session| session.dir.clone())
            .collect();
        dirs.sort();
        dirs.dedup();
        dirs
    }

    /// How many terminals the daemon holds, exited ones not yet reaped included.
    pub fn count(&self) -> usize {
        lock(&self.inner.sessions).len()
    }

    /// The daemon is stopping: every shell ends and every task is waited for (a shell that holds
    /// its terminal open past a short deadline is left to the OS).
    pub async fn shutdown(&self) {
        let inner = &self.inner;
        inner.stop.cancel();
        let all: Vec<Arc<Session>> = lock(&inner.sessions).drain().map(|(_, s)| s).collect();
        for session in &all {
            session.close_input();
            session.killer.hangup();
            session.killer.kill();
        }
        drop(all);
        inner.tracker.close();
        if tokio::time::timeout(Duration::from_secs(2), inner.tracker.wait())
            .await
            .is_err()
        {
            tracing::warn!("a terminal's shell kept its terminal open at shutdown; leaving it");
        }
    }
}

impl Inner {
    /// Takes a slot under the cap, making room by reaping the oldest exited terminal when the
    /// daemon is full.
    fn reserve(self: &Arc<Self>) -> Result<Slot, TermError> {
        let mut sessions = lock(&self.sessions);
        let held = sessions.len() + self.opening.load(Ordering::SeqCst);
        if held >= TERM_MAX_TERMINALS {
            let oldest_exited = sessions
                .values()
                .filter(|session| session.exited())
                .min_by_key(|session| session.opened_at_ms)
                .map(|session| session.id.clone());
            match oldest_exited.and_then(|id| sessions.remove(&id)) {
                Some(gone) => self.end(&gone),
                None => return Err(TermError::TooMany(TERM_MAX_TERMINALS)),
            }
        }
        self.opening.fetch_add(1, Ordering::SeqCst);
        Ok(Slot(Arc::clone(self)))
    }

    /// Ends a terminal that was removed from the map: its shell is hung up on, and killed when it
    /// does not end within the grace.
    fn end(self: &Arc<Self>, session: &Arc<Session>) {
        session.reap.cancel();
        if session.exited() {
            return;
        }
        session.killer.hangup();
        let (inner, session) = (Arc::clone(self), Arc::clone(session));
        self.tracker.spawn(async move {
            tokio::select! {
                () = tokio::time::sleep(inner.timing.kill_grace) => {
                    if !session.exited() {
                        tracing::debug!(terminal = %session.id, "a closed terminal's shell ignored the hangup; killing it");
                        session.killer.kill();
                    }
                }
                () = inner.stop.cancelled() => {}
            }
        });
    }

    /// Forgets `session` if it is still the one registered under its id.
    fn forget(&self, session: &Arc<Session>) {
        let mut sessions = lock(&self.sessions);
        if sessions
            .get(&session.id)
            .is_some_and(|held| Arc::ptr_eq(held, session))
        {
            sessions.remove(&session.id);
        }
    }
}

/// A reserved place under the terminal cap, given back when dropped.
struct Slot(Arc<Inner>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.opening.fetch_sub(1, Ordering::SeqCst);
    }
}

/// The variable that names an action's script for the shell that sources it. Not `RENO_*`: none
/// of those reach a terminal.
const ACTION_SCRIPT_VAR: &str = "TERM_ACTION_SCRIPT";

fn clamp(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(1, TERM_MAX_DIM), rows.clamp(1, TERM_MAX_DIM))
}

/// `term_` and 16 hex digits of 8 random bytes.
fn new_id() -> TermId {
    let mut bytes = [0_u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        // The clock is a worse source, but an id only has to differ from the others held.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        bytes.copy_from_slice(&nanos.to_le_bytes()[..8]);
    }
    let suffix: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    TermId::from_suffix(&suffix).unwrap_or_else(|_| unreachable!("hex is a valid suffix"))
}

/// A private script holding `command`, and the line that makes the shell run it: sourcing a file
/// keeps a multi-line command out of the line editor.
fn write_action_script(
    shell_name: &str,
    command: &str,
) -> std::io::Result<(tempfile::NamedTempFile, String)> {
    use std::io::Write as _;
    let fish = shell_name == "fish";
    let mut file = tempfile::Builder::new()
        .prefix("reno-action-")
        .suffix(if fish { ".fish" } else { ".sh" })
        .tempfile()?;
    file.write_all(command.as_bytes())?;
    file.write_all(b"\n")?;
    file.flush()?;
    let input = if fish {
        format!("source \"${ACTION_SCRIPT_VAR}\"\r")
    } else {
        format!(". \"${ACTION_SCRIPT_VAR}\"\r")
    };
    Ok((file, input))
}

#[cfg(test)]
mod tests;
