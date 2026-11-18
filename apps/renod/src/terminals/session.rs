//! One terminal: its replay ring, its pump, its input lane and its attach cursor.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::Stream;
use futures::stream;
use reno_proto::{
    TERM_MAX_CHUNK_BYTES, TERM_REPLAY_BYTES, TermEvent, TermId, TermInfo, TermStatusResult,
    encode_bytes,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::pty::{ExitInfo, PtyChild, PtyControl, PtyKiller, PtyParts};
use super::{Inner, TermError};

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Commands the input lane applies in order.
#[derive(Debug)]
pub(super) enum Cmd {
    Write(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

/// Commands one terminal queues before a write is refused as backed up.
const LANE_CAPACITY: usize = 32;
/// Raw chunks between the reader thread and the pump.
const RAW_CAPACITY: usize = 16;
/// How many output events one refill of an attach cursor copies out of the ring.
const REFILL_EVENTS: usize = 32;
/// How many bytes of output one refill copies.
const REFILL_BYTES: usize = 256 * 1024;

enum Item {
    Data { seq: u64, bytes: Vec<u8> },
    Exit { seq: u64, info: ExitInfo },
}

impl Item {
    fn seq(&self) -> u64 {
        match self {
            Self::Data { seq, .. } | Self::Exit { seq, .. } => *seq,
        }
    }

    fn event(&self) -> TermEvent {
        match self {
            Self::Data { seq, bytes } => TermEvent::Data {
                seq: *seq,
                data: encode_bytes(bytes),
            },
            Self::Exit { seq, info } => TermEvent::Exit {
                seq: *seq,
                code: info.code,
                signal: info.signal.clone(),
            },
        }
    }
}

struct State {
    ring: VecDeque<Item>,
    ring_bytes: usize,
    last_seq: u64,
    exited: bool,
}

/// What `Session::start` needs besides the PTY.
pub(super) struct Start {
    pub(super) id: TermId,
    pub(super) dir: PathBuf,
    pub(super) cwd: String,
    pub(super) shell: String,
    pub(super) sandboxed: bool,
    pub(super) in_place: bool,
    pub(super) script: Option<tempfile::NamedTempFile>,
}

pub(super) struct Session {
    pub(super) id: TermId,
    /// The directory the terminal is keyed by: what the Files tab serves for its run.
    pub(super) dir: PathBuf,
    pub(super) cwd: String,
    shell: String,
    sandboxed: bool,
    in_place: bool,
    pub(super) opened_at_ms: u64,
    shell_pid: Option<u32>,
    state: Mutex<State>,
    latest: watch::Sender<u64>,
    lane: Mutex<Option<mpsc::Sender<Cmd>>>,
    control: Arc<dyn PtyControl>,
    pub(super) killer: Arc<dyn PtyKiller>,
    /// Cancelled when the terminal is closed or reaped, and when the daemon stops.
    pub(super) reap: CancellationToken,
    /// Cancelled when the daemon stops.
    stopped: CancellationToken,
    script: Mutex<Option<tempfile::NamedTempFile>>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("dir", &self.dir)
            .field("sandboxed", &self.sandboxed)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Starts the tasks of a terminal whose shell is already running.
    ///
    /// The three threads a terminal needs (its reader, the wait for its shell, its input lane)
    /// are dedicated OS threads, not the runtime's blocking pool: each parks for as long as the
    /// shell lives, and the pool is small and shared with the store. Each is tracked by a task
    /// that ends when its thread does, so shutdown waits for them (ARCH-11).
    pub(super) fn start(
        inner: &Arc<Inner>,
        start: Start,
        parts: PtyParts,
    ) -> std::io::Result<Arc<Self>> {
        let PtyParts {
            pid,
            reader,
            writer,
            control,
            child,
            killer,
        } = parts;
        let (latest, _) = watch::channel(0);
        let (lane_tx, lane_rx) = mpsc::channel(LANE_CAPACITY);
        let session = Arc::new(Self {
            id: start.id,
            dir: start.dir,
            cwd: start.cwd,
            shell: start.shell,
            sandboxed: start.sandboxed,
            in_place: start.in_place,
            opened_at_ms: now_ms(),
            shell_pid: pid,
            state: Mutex::new(State {
                ring: VecDeque::new(),
                ring_bytes: 0,
                last_seq: 0,
                exited: false,
            }),
            latest,
            lane: Mutex::new(Some(lane_tx)),
            control: Arc::clone(&control),
            killer,
            reap: inner.stop.child_token(),
            stopped: inner.stop.clone(),
            script: Mutex::new(start.script),
        });

        let (raw_tx, raw_rx) = mpsc::channel(RAW_CAPACITY);
        let (exit_tx, exit_rx) = oneshot::channel();
        let started = spawn_thread(inner, "term-read", move || read_loop(reader, &raw_tx))
            .and_then(|()| spawn_thread(inner, "term-wait", move || wait_for(child, exit_tx)))
            .and_then(|()| {
                spawn_thread(inner, "term-input", move || {
                    input_loop(lane_rx, writer, &*control);
                })
            });
        if let Err(error) = started {
            // No terminal without its threads: the shell that was started is ended.
            session.killer.hangup();
            session.killer.kill();
            return Err(error);
        }
        inner.tracker.spawn(pump(
            Arc::clone(inner),
            Arc::clone(&session),
            raw_rx,
            exit_rx,
        ));
        Ok(session)
    }

    /// Stops taking input: the lane's thread ends once what is queued is applied.
    pub(super) fn close_input(&self) {
        lock(&self.lane).take();
    }

    pub(super) fn exited(&self) -> bool {
        lock(&self.state).exited
    }

    /// Queues `cmd` for the shell.
    pub(super) fn send(&self, cmd: Cmd, id: &TermId) -> Result<(), TermError> {
        let lane = lock(&self.lane);
        let Some(lane) = lane.as_ref() else {
            return Err(TermError::Exited(id.clone()));
        };
        lane.try_send(cmd).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => TermError::Backlog(id.clone()),
            mpsc::error::TrySendError::Closed(_) => TermError::Exited(id.clone()),
        })
    }

    pub(super) fn status(&self) -> TermStatusResult {
        let exited = self.exited();
        let busy = !exited
            && match (self.control.foreground_pid(), self.shell_pid) {
                (Some(foreground), Some(shell)) => foreground != shell,
                _ => false,
            };
        TermStatusResult {
            busy,
            exited,
            sandboxed: self.sandboxed,
        }
    }

    pub(super) fn info(&self) -> TermInfo {
        TermInfo {
            id: self.id.clone(),
            cwd: self.cwd.clone(),
            shell: self.shell.clone(),
            sandboxed: self.sandboxed,
            in_place: self.in_place,
            exited: self.exited(),
            opened_at_ms: self.opened_at_ms,
        }
    }

    /// Appends output, in pieces no longer than a `Data` event may be.
    fn push_data(&self, bytes: Vec<u8>) {
        let mut state = lock(&self.state);
        for piece in bytes.chunks(TERM_MAX_CHUNK_BYTES) {
            state.last_seq += 1;
            let seq = state.last_seq;
            state.ring_bytes += piece.len();
            state.ring.push_back(Item::Data {
                seq,
                bytes: piece.to_vec(),
            });
        }
        // Never the last item: a ring holds at least what was just pushed.
        while state.ring_bytes > TERM_REPLAY_BYTES && state.ring.len() > 1 {
            if let Some(Item::Data { bytes, .. }) = state.ring.pop_front() {
                state.ring_bytes -= bytes.len();
            }
        }
        let last = state.last_seq;
        self.latest.send_replace(last);
    }

    fn push_exit(&self, info: ExitInfo) {
        {
            let mut state = lock(&self.state);
            state.last_seq += 1;
            let seq = state.last_seq;
            state.ring.push_back(Item::Exit { seq, info });
            state.exited = true;
            let last = state.last_seq;
            self.latest.send_replace(last);
        }
        // The shell is gone: no more input, no script to keep, no terminal to hold.
        lock(&self.lane).take();
        lock(&self.script).take();
        self.control.release();
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The blocking reader: PTY bytes into the bounded channel until EOF, an error, or the pump
/// going away.
fn read_loop(mut reader: Box<dyn Read + Send>, tx: &mpsc::Sender<Vec<u8>>) {
    let mut buf = vec![0_u8; TERM_MAX_CHUNK_BYTES];
    loop {
        match reader.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if tx.blocking_send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        }
    }
}

fn wait_for(mut child: Box<dyn PtyChild>, tx: oneshot::Sender<ExitInfo>) {
    let _ = tx.send(child.wait());
}

/// Runs `work` on a thread of its own, and a tracked task that ends when it does.
fn spawn_thread(
    inner: &Arc<Inner>,
    name: &str,
    work: impl FnOnce() + Send + 'static,
) -> std::io::Result<()> {
    let (done_tx, done_rx) = oneshot::channel::<()>();
    std::thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || {
            work();
            let _ = done_tx.send(());
        })?;
    inner.tracker.spawn(async move {
        let _ = done_rx.await;
    });
    Ok(())
}

/// Turns raw chunks into numbered events, and the shell's end into `Exit`.
///
/// Output that follows a quiet spell goes out the moment it is read; only a continuous stream
/// waits out `batch` between emissions, on a deadline. Nothing here polls.
async fn pump(
    inner: Arc<Inner>,
    session: Arc<Session>,
    mut raw: mpsc::Receiver<Vec<u8>>,
    mut exit_rx: oneshot::Receiver<ExitInfo>,
) {
    use tokio::time::{Instant, sleep_until};
    let batch = inner.timing.batch;
    let mut buffer: Vec<u8> = Vec::new();
    let mut flush_at: Option<Instant> = None;
    let mut last_emit = Instant::now()
        .checked_sub(batch)
        .unwrap_or_else(Instant::now);
    let mut raw_open = true;
    let mut exit: Option<ExitInfo> = None;
    let mut give_up_at: Option<Instant> = None;

    while raw_open || exit.is_none() {
        let flush_deadline = flush_at.unwrap_or_else(Instant::now);
        let give_up_deadline = give_up_at.unwrap_or_else(Instant::now);
        tokio::select! {
            biased;
            () = inner.stop.cancelled() => return,
            chunk = raw.recv(), if raw_open => match chunk {
                Some(chunk) => {
                    buffer.extend_from_slice(&chunk);
                    let now = Instant::now();
                    if now >= last_emit + batch || buffer.len() >= TERM_MAX_CHUNK_BYTES {
                        last_emit = now;
                        flush_at = None;
                        session.push_data(std::mem::take(&mut buffer));
                    } else if flush_at.is_none() {
                        flush_at = Some(last_emit + batch);
                    }
                }
                None => raw_open = false,
            },
            ended = &mut exit_rx, if exit.is_none() => {
                exit = Some(ended.unwrap_or_default());
                if raw_open {
                    // Whatever the shell left running may keep the terminal open: wait for its
                    // last output, but not for ever.
                    give_up_at = Some(Instant::now() + inner.timing.eof_grace);
                }
            }
            () = sleep_until(flush_deadline), if flush_at.is_some() => {
                last_emit = Instant::now();
                flush_at = None;
                session.push_data(std::mem::take(&mut buffer));
            }
            () = sleep_until(give_up_deadline), if give_up_at.is_some() && raw_open => {
                raw_open = false;
            }
        }
    }
    if !buffer.is_empty() {
        session.push_data(buffer);
    }
    session.push_exit(exit.unwrap_or_default());

    // An exited terminal stays for a while so a viewer can show its last output, then goes: one
    // deadline, not a sweep.
    tokio::select! {
        () = tokio::time::sleep(inner.timing.exited_ttl) => inner.forget(&session),
        () = session.reap.cancelled() => {}
    }
}

/// Applies one terminal's input and resizes, in the order they were accepted. A write blocks for
/// as long as the shell does not read, which stalls this terminal's lane and nothing else; it
/// fails once the shell is gone. The loop ends when the queue is closed.
fn input_loop(
    mut rx: mpsc::Receiver<Cmd>,
    mut writer: Box<dyn Write + Send>,
    control: &dyn PtyControl,
) {
    while let Some(cmd) = rx.blocking_recv() {
        let applied = match cmd {
            Cmd::Write(bytes) => writer.write_all(&bytes).and_then(|()| writer.flush()),
            Cmd::Resize { cols, rows } => control.resize(cols, rows),
        };
        if let Err(error) = applied {
            tracing::debug!(%error, "a terminal's input or resize failed");
        }
    }
}

/// The cursor of one `term.attach`: where it has read to, and what it has copied but not yet
/// handed out.
struct Cursor {
    session: Arc<Session>,
    seen: u64,
    rx: watch::Receiver<u64>,
    queue: VecDeque<TermEvent>,
    done: bool,
}

impl Cursor {
    /// Copies what the ring holds after `seen`, with a `Gap` first when the ring no longer
    /// reaches back to it.
    fn refill(&mut self) {
        // Marked seen before the ring is read: a push after this wakes the next `changed()`.
        let _ = self.rx.borrow_and_update();
        let state = lock(&self.session.state);
        let Some(oldest) = state.ring.front().map(Item::seq) else {
            return;
        };
        if self.seen + 1 < oldest {
            self.queue.push_back(TermEvent::Gap {
                lost_from: self.seen + 1,
                seq: oldest,
            });
            self.seen = oldest - 1;
        }
        let skip = usize::try_from(self.seen + 1 - oldest).unwrap_or(usize::MAX);
        let mut bytes = 0;
        for item in state.ring.iter().skip(skip) {
            if let Item::Data { bytes: data, .. } = item {
                bytes += data.len();
            }
            self.queue.push_back(item.event());
            self.seen = item.seq();
            if self.queue.len() >= REFILL_EVENTS || bytes >= REFILL_BYTES {
                break;
            }
        }
    }
}

/// `term.attach`'s stream: replay from `after`, then live, ending after the exit.
pub(super) fn attach(
    session: Arc<Session>,
    after: Option<u64>,
) -> impl Stream<Item = TermEvent> + Send + 'static {
    let last = lock(&session.state).last_seq;
    let cursor = Cursor {
        seen: after.unwrap_or(0).min(last),
        rx: session.latest.subscribe(),
        session,
        queue: VecDeque::new(),
        done: false,
    };
    stream::unfold(cursor, |mut cursor| async move {
        loop {
            if let Some(event) = cursor.queue.pop_front() {
                if matches!(event, TermEvent::Exit { .. }) {
                    cursor.done = true;
                }
                return Some((event, cursor));
            }
            if cursor.done {
                return None;
            }
            cursor.refill();
            if !cursor.queue.is_empty() {
                continue;
            }
            let stopped = cursor.session.stopped.clone();
            tokio::select! {
                changed = cursor.rx.changed() => {
                    if changed.is_err() {
                        return None;
                    }
                }
                // The daemon is stopping, and no `Exit` will come. A closed terminal ends
                // through its own `Exit`.
                () = stopped.cancelled() => return None,
            }
        }
    })
}
