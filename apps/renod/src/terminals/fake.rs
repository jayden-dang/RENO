//! A PTY that is not one (ARCH-16): shells that live in memory, so no test opens a real shell.
//!
//! [`FakePty::echo`] makes every shell echo its input back, and end when it is sent `exit`;
//! [`FakePty::silent`] makes them say nothing until a test calls [`FakeProcess::emit`]. Either
//! way a test sees the [`SpawnSpec`] each shell was started with (the sandbox limits among it),
//! what was written to it, and every resize.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Condvar, Mutex, PoisonError};

use super::pty::{
    ExitInfo, PtyChild, PtyControl, PtyKiller, PtyParts, PtySpawner, SpawnError, SpawnSpec,
};

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How many output chunks and exits a fake shell buffers.
const FAKE_QUEUE: usize = 4096;

/// A spawner of in-memory shells.
#[derive(Debug, Clone)]
pub struct FakePty {
    inner: Arc<FakeInner>,
}

#[derive(Debug)]
struct FakeInner {
    echo: bool,
    refuse_sandbox: Mutex<Option<String>>,
    refuse_spawn: Mutex<Option<String>>,
    started: Mutex<Vec<FakeProcess>>,
}

impl FakePty {
    fn new(echo: bool) -> Self {
        Self {
            inner: Arc::new(FakeInner {
                echo,
                refuse_sandbox: Mutex::new(None),
                refuse_spawn: Mutex::new(None),
                started: Mutex::new(Vec::new()),
            }),
        }
    }

    /// Shells that echo their input and end on a line that says `exit`.
    pub fn echo() -> Self {
        Self::new(true)
    }

    /// Shells that say nothing until [`FakeProcess::emit`].
    pub fn silent() -> Self {
        Self::new(false)
    }

    /// From now on a shell that needs the sandbox is refused with `why`, as when
    /// `sandbox-exec` is missing. `None` lifts it.
    pub fn refuse_sandbox(&self, why: Option<&str>) {
        *lock(&self.inner.refuse_sandbox) = why.map(str::to_owned);
    }

    /// From now on every start fails with `why`. `None` lifts it.
    pub fn refuse_spawn(&self, why: Option<&str>) {
        *lock(&self.inner.refuse_spawn) = why.map(str::to_owned);
    }

    /// The shells started so far, oldest first.
    pub fn processes(&self) -> Vec<FakeProcess> {
        lock(&self.inner.started).clone()
    }

    /// How many shells were started (refused ones are not).
    pub fn started(&self) -> usize {
        lock(&self.inner.started).len()
    }
}

impl PtySpawner for FakePty {
    fn spawn(&self, spec: SpawnSpec) -> Result<PtyParts, SpawnError> {
        if spec.limits.is_some()
            && let Some(why) = lock(&self.inner.refuse_sandbox).clone()
        {
            return Err(SpawnError::SandboxUnavailable(why));
        }
        if let Some(why) = lock(&self.inner.refuse_spawn).clone() {
            return Err(SpawnError::Failed(why));
        }
        let (out_tx, out_rx) = sync_channel(FAKE_QUEUE);
        let (exit_tx, exit_rx) = sync_channel(1);
        let n = u32::try_from(lock(&self.inner.started).len()).unwrap_or(0);
        let state = Arc::new(ProcState {
            pid: 10_000 + n * 2,
            spec,
            echo: self.inner.echo,
            written: Mutex::new(Vec::new()),
            line: Mutex::new(Vec::new()),
            resizes: Mutex::new(Vec::new()),
            ops: Mutex::new(Vec::new()),
            stalled: (Mutex::new(false), Condvar::new()),
            ignores_hangup: AtomicBool::new(false),
            out: Mutex::new(Some(out_tx)),
            exit: Mutex::new(Some(exit_tx)),
            busy: AtomicBool::new(false),
            hung_up: AtomicBool::new(false),
            killed: AtomicBool::new(false),
        });
        lock(&self.inner.started).push(FakeProcess(Arc::clone(&state)));
        Ok(PtyParts {
            pid: Some(state.pid),
            reader: Box::new(ChanReader {
                rx: out_rx,
                pending: Vec::new(),
            }),
            writer: Box::new(FakeWriter(Arc::clone(&state))),
            control: Arc::new(FakeControl(Arc::clone(&state))),
            child: Box::new(FakeChild(exit_rx)),
            killer: Arc::new(FakeKiller(state)),
        })
    }
}

/// One thing done to a fake shell, in the order it was done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Bytes written to its input.
    Write(Vec<u8>),
    /// A resize, `(cols, rows)`.
    Resize(u16, u16),
}

/// One fake shell, as a test sees it.
#[derive(Debug, Clone)]
pub struct FakeProcess(Arc<ProcState>);

#[derive(Debug)]
struct ProcState {
    pid: u32,
    spec: SpawnSpec,
    echo: bool,
    written: Mutex<Vec<u8>>,
    line: Mutex<Vec<u8>>,
    resizes: Mutex<Vec<(u16, u16)>>,
    ops: Mutex<Vec<Op>>,
    stalled: (Mutex<bool>, Condvar),
    ignores_hangup: AtomicBool,
    out: Mutex<Option<SyncSender<Vec<u8>>>>,
    exit: Mutex<Option<SyncSender<ExitInfo>>>,
    busy: AtomicBool,
    hung_up: AtomicBool,
    killed: AtomicBool,
}

impl ProcState {
    fn emit(&self, bytes: &[u8]) {
        if let Some(out) = lock(&self.out).as_ref() {
            let _ = out.send(bytes.to_vec());
        }
    }

    fn end(&self, info: ExitInfo) {
        // Output first ends (EOF), then the wait returns, as on a real PTY.
        lock(&self.out).take();
        if let Some(exit) = lock(&self.exit).take() {
            let _ = exit.send(info);
        }
    }
}

impl FakeProcess {
    /// What the shell was started with.
    pub fn spec(&self) -> SpawnSpec {
        self.0.spec.clone()
    }

    /// Everything written to the shell's input so far.
    pub fn written(&self) -> Vec<u8> {
        lock(&self.0.written).clone()
    }

    /// Every resize, in order.
    pub fn resizes(&self) -> Vec<(u16, u16)> {
        lock(&self.0.resizes).clone()
    }

    /// Everything done to the shell, in order.
    pub fn ops(&self) -> Vec<Op> {
        lock(&self.0.ops).clone()
    }

    /// The shell stops reading its input: writes block until `stall(false)`.
    pub fn stall(&self, on: bool) {
        *lock(&self.0.stalled.0) = on;
        self.0.stalled.1.notify_all();
    }

    /// The shell does not end when the terminal is hung up on (only a kill ends it).
    pub fn ignore_hangup(&self, on: bool) {
        self.0.ignores_hangup.store(on, Ordering::SeqCst);
    }

    /// The shell prints `bytes`.
    pub fn emit(&self, bytes: &[u8]) {
        self.0.emit(bytes);
    }

    /// The shell exits with `code`.
    pub fn exit(&self, code: i32) {
        self.0.end(ExitInfo {
            code: Some(code),
            signal: None,
        });
    }

    /// A program other than the shell is, or is not, in the foreground.
    pub fn set_busy(&self, busy: bool) {
        self.0.busy.store(busy, Ordering::SeqCst);
    }

    /// The terminal was hung up on the shell.
    pub fn hung_up(&self) -> bool {
        self.0.hung_up.load(Ordering::SeqCst)
    }

    /// The shell was killed outright.
    pub fn killed(&self) -> bool {
        self.0.killed.load(Ordering::SeqCst)
    }
}

struct ChanReader {
    rx: Receiver<Vec<u8>>,
    pending: Vec<u8>,
}

impl Read for ChanReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pending.is_empty() {
            match self.rx.recv() {
                Ok(chunk) => self.pending = chunk,
                Err(_) => return Ok(0),
            }
        }
        let n = buf.len().min(self.pending.len());
        buf[..n].copy_from_slice(&self.pending[..n]);
        self.pending.drain(..n);
        Ok(n)
    }
}

struct FakeWriter(Arc<ProcState>);

impl Write for FakeWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        {
            let mut stalled = lock(&self.0.stalled.0);
            while *stalled {
                stalled = self
                    .0
                    .stalled
                    .1
                    .wait(stalled)
                    .unwrap_or_else(PoisonError::into_inner);
            }
        }
        lock(&self.0.ops).push(Op::Write(buf.to_vec()));
        lock(&self.0.written).extend_from_slice(buf);
        if self.0.echo {
            self.0.emit(buf);
            let mut line = lock(&self.0.line);
            for &byte in buf {
                if byte == b'\n' || byte == b'\r' {
                    let done = line.as_slice() == b"exit";
                    line.clear();
                    if done {
                        drop(line);
                        self.0.end(ExitInfo {
                            code: Some(0),
                            signal: None,
                        });
                        return Ok(buf.len());
                    }
                } else {
                    line.push(byte);
                }
            }
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct FakeControl(Arc<ProcState>);

impl PtyControl for FakeControl {
    fn resize(&self, cols: u16, rows: u16) -> io::Result<()> {
        lock(&self.0.ops).push(Op::Resize(cols, rows));
        lock(&self.0.resizes).push((cols, rows));
        Ok(())
    }

    fn foreground_pid(&self) -> Option<u32> {
        Some(if self.0.busy.load(Ordering::SeqCst) {
            self.0.pid + 1
        } else {
            self.0.pid
        })
    }

    fn release(&self) {}
}

struct FakeChild(Receiver<ExitInfo>);

impl PtyChild for FakeChild {
    fn wait(&mut self) -> ExitInfo {
        self.0.recv().unwrap_or_default()
    }
}

struct FakeKiller(Arc<ProcState>);

impl PtyKiller for FakeKiller {
    fn hangup(&self) {
        self.0.hung_up.store(true, Ordering::SeqCst);
        if self.0.ignores_hangup.load(Ordering::SeqCst) {
            return;
        }
        self.0.end(ExitInfo {
            code: None,
            signal: Some("Hangup".to_owned()),
        });
    }

    fn kill(&self) {
        self.0.killed.store(true, Ordering::SeqCst);
        self.0.end(ExitInfo {
            code: None,
            signal: Some("Killed".to_owned()),
        });
    }
}
