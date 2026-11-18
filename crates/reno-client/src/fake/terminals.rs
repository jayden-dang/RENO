//! The fake daemon's Terminal tab (RP2): `term.open`, `term.attach`, `term.write`, `term.resize`,
//! `term.close`, `term.status` and `term.list`, over terminals that are an echo. No PTY, no
//! shell, no sandbox.
//!
//! - Which tree a run gets is the Files tab's ([`super::files`]): a removed worktree is
//!   `WorkspaceUnavailable`, a run with none is `inPlace`, a run the fake does not know is
//!   `NotFound`.
//! - What is written to a terminal comes back as output, as typed. A line that says `exit` ends
//!   the terminal with code 0.
//! - A terminal opened on a run that is still going is `sandboxed`, as the daemon's is, and keeps
//!   it; an `inPlace` one never is.
//! - `term.attach` replays what is held after `afterSeq`, then follows. A terminal keeps only the
//!   last [`HELD`] items: an attach from before them gets a `Gap` first, as the daemon's does.
//! - [`FakeDaemon::term_output`] makes a terminal print what a test wants, and
//!   [`FakeDaemon::set_term_busy`] says that a program holds its foreground.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};

use futures::stream;
use reno_proto::{
    ErrorCode, ErrorObject, RunId, Seq, TERM_MAX_DIM, TERM_MAX_INPUT_BYTES, TERM_MAX_TERMINALS,
    TermAttach, TermAttachParams, TermClose, TermDone, TermEvent, TermId, TermInfo, TermList,
    TermListParams, TermListResult, TermOpen, TermOpenParams, TermOpened, TermRef, TermResize,
    TermResizeParams, TermStatus, TermStatusResult, TermWrite, TermWriteParams, decode_bytes,
    encode_bytes,
};
use reno_rpc::{Router, SubItem};

use super::files::place;
use super::{FakeDaemon, Shared, injected};

/// How many output items a fake terminal holds for replay.
pub const HELD: usize = 256;

/// One fake terminal.
pub(super) struct FakeTerm {
    run: RunId,
    cwd: String,
    sandboxed: bool,
    in_place: bool,
    opened_at_ms: u64,
    busy: bool,
    exited: bool,
    last_seq: u64,
    held: VecDeque<TermEvent>,
    line: Vec<u8>,
    live: tokio::sync::broadcast::Sender<TermEvent>,
}

impl FakeTerm {
    fn push(&mut self, event: TermEvent) {
        self.held.push_back(event.clone());
        while self.held.len() > HELD {
            self.held.pop_front();
        }
        let _ = self.live.send(event);
    }

    fn print(&mut self, bytes: &[u8]) {
        self.last_seq += 1;
        let seq = self.last_seq;
        self.push(TermEvent::Data {
            seq,
            data: encode_bytes(bytes),
        });
    }

    fn end(&mut self, code: i32) {
        self.last_seq += 1;
        let seq = self.last_seq;
        self.exited = true;
        self.push(TermEvent::Exit {
            seq,
            code: Some(code),
            signal: None,
        });
    }
}

type Terms = Mutex<HashMap<TermId, FakeTerm>>;

fn lock(terms: &Terms) -> std::sync::MutexGuard<'_, HashMap<TermId, FakeTerm>> {
    terms.lock().unwrap_or_else(PoisonError::into_inner)
}

fn not_found(id: &TermId) -> ErrorObject {
    ErrorObject::new(ErrorCode::NotFound, format!("terminal {id} not found"))
}

/// The tree of a run, and whether a terminal opened now would be limited.
fn open(s: &Shared, p: &TermOpenParams) -> Result<TermOpened, ErrorObject> {
    let place = place(s, &p.run_id)?;
    let live = s.read(|w| {
        w.run(&p.run_id)
            .is_some_and(|run| !run.summary.status.is_terminal())
    });
    let sandboxed = live && !place.in_place;
    let mut terms = lock(&s.terms);
    if terms.len() >= TERM_MAX_TERMINALS {
        return Err(ErrorObject::new(
            ErrorCode::TerminalLimit,
            format!("the daemon holds {TERM_MAX_TERMINALS} terminals already; close one"),
        ));
    }
    let n = s.term_counter.fetch_add(1, Ordering::SeqCst) + 1;
    let id = TermId::from_suffix(&format!("fake{n}"))
        .map_err(|e| ErrorObject::new(ErrorCode::InternalError, e.to_string()))?;
    let cwd = format!("/fake/worktrees/{}", p.run_id.as_str());
    let mut term = FakeTerm {
        run: p.run_id.clone(),
        cwd: cwd.clone(),
        sandboxed,
        in_place: place.in_place,
        opened_at_ms: n,
        busy: false,
        exited: false,
        last_seq: 0,
        held: VecDeque::new(),
        line: Vec::new(),
        live: tokio::sync::broadcast::channel(256).0,
    };
    if let Some(command) = &p.command {
        term.print(format!("$ {command}\r\n").as_bytes());
    }
    terms.insert(id.clone(), term);
    Ok(TermOpened {
        id,
        cwd,
        shell: "zsh".to_owned(),
        sandboxed,
        in_place: place.in_place,
    })
}

fn write(s: &Shared, p: &TermWriteParams) -> Result<TermDone, ErrorObject> {
    let bytes = decode_bytes(&p.data)
        .ok_or_else(|| ErrorObject::new(ErrorCode::InvalidParams, "not valid base64"))?;
    if bytes.len() > TERM_MAX_INPUT_BYTES {
        return Err(ErrorObject::new(
            ErrorCode::InvalidParams,
            "input too large",
        ));
    }
    let mut terms = lock(&s.terms);
    let term = terms.get_mut(&p.id).ok_or_else(|| not_found(&p.id))?;
    if term.exited {
        return Err(ErrorObject::new(
            ErrorCode::Conflict,
            format!("terminal {} has exited", p.id),
        ));
    }
    term.print(&bytes);
    for &byte in &bytes {
        if byte == b'\r' || byte == b'\n' {
            let done = term.line == b"exit";
            term.line.clear();
            if done {
                term.end(0);
                break;
            }
        } else {
            term.line.push(byte);
        }
    }
    Ok(TermDone {})
}

fn info(id: &TermId, term: &FakeTerm) -> TermInfo {
    TermInfo {
        id: id.clone(),
        cwd: term.cwd.clone(),
        shell: "zsh".to_owned(),
        sandboxed: term.sandboxed,
        in_place: term.in_place,
        exited: term.exited,
        opened_at_ms: term.opened_at_ms,
    }
}

/// What `term.attach` follows.
struct Follow {
    queue: VecDeque<TermEvent>,
    rx: tokio::sync::broadcast::Receiver<TermEvent>,
    last: u64,
    done: bool,
}

pub(super) fn add(router: Router, shared: &Arc<Shared>) -> Router {
    macro_rules! method {
        ($router:expr, $ty:ty, $name:literal, $params:ty, |$s:ident, $p:ident| $body:expr) => {{
            let s = Arc::clone(shared);
            $router.method::<$ty, _, _>(move |$p: $params, _| {
                let $s = Arc::clone(&s);
                async move {
                    injected(&$s, $name)?;
                    $body
                }
            })
        }};
    }
    let router = method!(router, TermOpen, "term.open", TermOpenParams, |s, p| open(
        &s, &p
    ));
    let router = method!(router, TermWrite, "term.write", TermWriteParams, |s, p| {
        write(&s, &p)
    });
    let router = method!(
        router,
        TermResize,
        "term.resize",
        TermResizeParams,
        |s, p| {
            let _ = (p.cols.min(TERM_MAX_DIM), p.rows.min(TERM_MAX_DIM));
            lock(&s.terms)
                .contains_key(&p.id)
                .then_some(TermDone {})
                .ok_or_else(|| not_found(&p.id))
        }
    );
    let router = method!(router, TermClose, "term.close", TermRef, |s, p| {
        let mut terms = lock(&s.terms);
        let mut term = terms.remove(&p.id).ok_or_else(|| not_found(&p.id))?;
        if !term.exited {
            term.end(1);
        }
        Ok(TermDone {})
    });
    let router = method!(router, TermStatus, "term.status", TermRef, |s, p| {
        let terms = lock(&s.terms);
        let term = terms.get(&p.id).ok_or_else(|| not_found(&p.id))?;
        Ok(TermStatusResult {
            busy: term.busy && !term.exited,
            exited: term.exited,
            sandboxed: term.sandboxed,
        })
    });
    let router = method!(router, TermList, "term.list", TermListParams, |s, p| {
        place(&s, &p.run_id)?;
        let terms = lock(&s.terms);
        let mut found: Vec<TermInfo> = terms
            .iter()
            .filter(|(_, term)| term.run == p.run_id)
            .map(|(id, term)| info(id, term))
            .collect();
        found.sort_by_key(|term| term.opened_at_ms);
        Ok(TermListResult { terminals: found })
    });
    let s = Arc::clone(shared);
    router.subscription::<TermAttach, _, _, _>(move |p: TermAttachParams, _| {
        let s = Arc::clone(&s);
        async move {
            injected(&s, "term.attach")?;
            let follow = {
                let terms = lock(&s.terms);
                let term = terms.get(&p.id).ok_or_else(|| not_found(&p.id))?;
                let after = p.after_seq.unwrap_or(0).min(term.last_seq);
                let mut queue = VecDeque::new();
                let oldest = term.held.front().map(TermEvent::seq);
                if let Some(oldest) = oldest
                    && after + 1 < oldest
                {
                    queue.push_back(TermEvent::Gap {
                        lost_from: after + 1,
                        seq: oldest,
                    });
                }
                queue.extend(term.held.iter().filter(|e| e.seq() > after).cloned());
                Follow {
                    queue,
                    last: term.last_seq,
                    done: term.exited,
                    rx: term.live.subscribe(),
                }
            };
            Ok(stream::unfold(follow, |mut f| async move {
                loop {
                    if let Some(event) = f.queue.pop_front() {
                        if matches!(event, TermEvent::Exit { .. }) {
                            f.done = true;
                            f.queue.clear();
                        }
                        let item = SubItem::seq(Seq(event.seq()), event);
                        return Some((item, f));
                    }
                    if f.done {
                        return None;
                    }
                    match f.rx.recv().await {
                        Ok(event) if event.seq() > f.last => {
                            f.last = event.seq();
                            f.queue.push_back(event);
                        }
                        Ok(_) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => return None,
                    }
                }
            }))
        }
    })
}

impl FakeDaemon {
    /// A terminal prints `text`, as a shell would.
    pub fn term_output(&self, id: &TermId, text: &str) {
        if let Some(term) = lock(&self.shared.terms).get_mut(id) {
            term.print(text.as_bytes());
        }
    }

    /// A program holds (or no longer holds) a terminal's foreground: `term.status` says `busy`.
    pub fn set_term_busy(&self, id: &TermId, busy: bool) {
        if let Some(term) = lock(&self.shared.terms).get_mut(id) {
            term.busy = busy;
        }
    }

    /// The terminal's shell exits with `code`.
    pub fn term_exit(&self, id: &TermId, code: i32) {
        if let Some(term) = lock(&self.shared.terms).get_mut(id)
            && !term.exited
        {
            term.end(code);
        }
    }

    /// How many terminals the fake holds.
    pub fn terminals(&self) -> usize {
        lock(&self.shared.terms).len()
    }
}
