//! One terminal: an emulator folding the shell's output, the selection and scrollback over it,
//! the writes and resizes that go back, the attach stream with its reconnects, and the element
//! that paints the grid.
//!
//! The daemon only moves bytes (ARCH-13); everything about what they mean is here. A view
//! attaches only while it is the tab in front: hidden, it drops the stream and keeps its
//! emulator and `last_seq`, and showing it again attaches from there. Which keys, colours and
//! timings apply is `vm::terminal`; this file holds the state and draws.

use std::time::Instant;

use futures::StreamExt as _;
use gpui::{
    App, Bounds, ClipboardItem, Context, Element, ElementId, ElementInputHandler, Entity,
    EntityInputHandler, EventEmitter, FocusHandle, Focusable, GlobalElementId, Hsla,
    InspectorElementId, InteractiveElement as _, IntoElement, KeyDownEvent, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, ParentElement as _, Pixels, Render,
    ScrollDelta, ScrollEvent, ShapedLine, SharedString, Style, Styled as _, Task, TextRun,
    TouchPhase, UTF16Selection, Window, div, fill, font, outline, point, px, relative, size,
};
use gpui_tokio::Tokio;
use reno_client::ClientError;
use reno_proto::{ErrorCode, RunId, TermEvent, TermId, TermOpenParams, TermOpened};
use reno_term::emulator::{
    CellColor, CellSnapshot, CursorShape, CursorSnapshot, Emulator, GridPoint, MouseTracking,
    SelectionType, Side,
};

use crate::store::Store;
use crate::ui::kit::MONO;
use crate::ui::theme::{self, Theme};
use crate::vm::terminal::{
    self as vm, BLINK, BLINK_IDLE, DRAG_THRESHOLD, EDGE_SCROLL_TICK, FONT_SIZE, KeyRoute,
    LINE_HEIGHT, MIN_CONTRAST, MOUSE_LEFT, MOUSE_MIDDLE, MOUSE_MOTION, MOUSE_NONE, MOUSE_RIGHT,
    MOUSE_WHEEL_DOWN, MOUSE_WHEEL_UP, OpenFail, PADDING, RESIZE_DEBOUNCE,
};

/// What a shell is, from `term.open` or `term.list`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Info {
    pub(crate) cwd: String,
    pub(crate) shell: String,
    pub(crate) sandboxed: bool,
    pub(crate) in_place: bool,
}

impl From<&TermOpened> for Info {
    fn from(o: &TermOpened) -> Self {
        Self {
            cwd: o.cwd.clone(),
            shell: o.shell.clone(),
            sandboxed: o.sandboxed,
            in_place: o.in_place,
        }
    }
}

/// How a view starts: a new shell, or one the daemon already holds (a restored tab).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Start {
    Open,
    Resume(TermId),
}

/// Where the shell is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Waiting for the grid to be measured, then for `term.open` (or for `term.list`).
    Opening,
    Live,
    /// The shell ended; its output stays.
    Ended {
        code: Option<i32>,
        signal: Option<String>,
    },
    /// The daemon no longer holds it: it ended and was forgotten, or the daemon restarted.
    Gone,
    /// `term.open` was refused.
    Failed(OpenFail),
}

/// What the screen hears from a view.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ViewEvent {
    /// The shell exists: its id is what is saved.
    Opened(TermId),
    /// The phase or the title changed.
    Changed,
}

/// Where the grid was painted on the last frame, in window coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Geometry {
    pub(crate) bounds: Bounds<Pixels>,
    pub(crate) origin: gpui::Point<Pixels>,
    pub(crate) cell_w: f32,
    pub(crate) line_h: f32,
    pub(crate) cols: u16,
    pub(crate) rows: u16,
}

/// A left-button gesture that may become a selection. A press alone does not select: pointer
/// travel past the threshold promotes it, so the click that focuses the terminal leaves no
/// one-cell selection (and no clipboard surprise) behind.
#[derive(Clone, Copy)]
struct Drag {
    origin: gpui::Point<Pixels>,
    position: gpui::Point<Pixels>,
    armed: bool,
}

/// What the attach task tells the view.
enum Msg {
    Started,
    Item(TermEvent),
    Failed(ClientError),
    Ended,
}

enum Flow {
    Continue,
    Stop,
}

/// The bytes and sizes sent to the daemon, recorded for tests.
#[cfg(test)]
#[derive(Default, Debug)]
pub(crate) struct Taps {
    pub(crate) writes: Vec<Vec<u8>>,
    pub(crate) resizes: Vec<(u16, u16)>,
    pub(crate) attaches: Vec<Option<u64>>,
}

pub(crate) struct TerminalView {
    store: Entity<Store>,
    run: RunId,
    start: Start,
    phase: Phase,
    id: Option<TermId>,
    info: Option<Info>,
    emu: Emulator,
    /// The last output item the emulator holds; an attach resumes after it.
    last_seq: u64,
    /// The view is the tab in front.
    visible: bool,
    /// An open or a list is on its way.
    requested: bool,
    attach: Option<Task<()>>,
    attached_at: Instant,
    /// The current attach replays what a viewer missed, or what the daemon still holds.
    resuming: bool,
    input: vm::InputQueue,
    sent_size: Option<(u16, u16)>,
    resize_task: Option<Task<()>>,
    sync_task: Option<Task<()>>,
    geometry: Option<Geometry>,
    drag: Option<Drag>,
    edge_task: Option<Task<()>>,
    report_button: Option<u8>,
    report_cell: Option<(usize, usize)>,
    wheel_owed: f32,
    scroll_remainder: f32,
    focus: FocusHandle,
    was_focused: bool,
    pub(crate) focus_pending: bool,
    blink_on: bool,
    blink_wanted: bool,
    blink_task: Option<Task<()>>,
    marked: Option<String>,
    #[cfg(test)]
    pub(crate) taps: Taps,
}

impl EventEmitter<ViewEvent> for TerminalView {}

impl Focusable for TerminalView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl TerminalView {
    pub(crate) fn new(
        store: Entity<Store>,
        run: RunId,
        start: Start,
        cx: &mut Context<Self>,
    ) -> Self {
        let resume = match &start {
            Start::Open => None,
            Start::Resume(id) => Some(id.clone()),
        };
        Self {
            store,
            run,
            id: resume,
            start,
            phase: Phase::Opening,
            info: None,
            emu: Emulator::new(80, 24),
            last_seq: 0,
            visible: false,
            requested: false,
            attach: None,
            attached_at: Instant::now(),
            resuming: false,
            input: vm::InputQueue::default(),
            sent_size: None,
            resize_task: None,
            sync_task: None,
            geometry: None,
            drag: None,
            edge_task: None,
            report_button: None,
            report_cell: None,
            wheel_owed: 0.0,
            scroll_remainder: 0.0,
            focus: cx.focus_handle(),
            was_focused: false,
            focus_pending: false,
            blink_on: true,
            blink_wanted: false,
            blink_task: None,
            marked: None,
            #[cfg(test)]
            taps: Taps::default(),
        }
    }

    // --- what the screen reads ----------------------------------------------------------------

    #[cfg(test)]
    pub(crate) fn id(&self) -> Option<&TermId> {
        self.id.as_ref()
    }

    pub(crate) fn phase(&self) -> &Phase {
        &self.phase
    }

    pub(crate) fn info(&self) -> Option<&Info> {
        self.info.as_ref()
    }

    /// What a program titled the shell (OSC 0 or 2).
    pub(crate) fn title(&self) -> Option<&str> {
        self.emu.title()
    }

    #[cfg(test)]
    pub(crate) fn emulator(&self) -> &Emulator {
        &self.emu
    }

    #[cfg(test)]
    pub(crate) fn last_seq(&self) -> u64 {
        self.last_seq
    }

    pub(crate) fn is_attached(&self) -> bool {
        self.attach.is_some()
    }

    #[cfg(test)]
    pub(crate) fn is_focused(&self, window: &Window) -> bool {
        self.focus.is_focused(window)
    }

    #[cfg(test)]
    pub(crate) fn blink_on(&self) -> bool {
        self.blink_on
    }

    #[cfg(test)]
    pub(crate) fn blink_running(&self) -> bool {
        self.blink_task.is_some()
    }

    #[cfg(test)]
    pub(crate) fn geometry(&self) -> Option<Geometry> {
        self.geometry
    }

    // --- showing and hiding -------------------------------------------------------------------

    /// The tab came to the front (or left it). Attaches while it shows and detaches when it
    /// does not: the shell keeps running, the emulator keeps what it has.
    pub(crate) fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        if self.visible == visible {
            return;
        }
        self.visible = visible;
        if visible {
            self.begin(cx);
        } else {
            self.attach = None;
            self.resize_task = None;
            self.edge_task = None;
            self.blink_task = None;
            self.blink_wanted = false;
            self.drag = None;
        }
    }

    fn begin(&mut self, cx: &mut Context<Self>) {
        match self.phase {
            Phase::Opening => {
                if self.requested {
                    return;
                }
                match self.start.clone() {
                    Start::Open if self.id.is_none() => self.try_open(cx),
                    Start::Open | Start::Resume(_) => self.resume(cx),
                }
            }
            Phase::Live => self.attach_stream(cx),
            Phase::Ended { .. } | Phase::Gone | Phase::Failed(_) => {}
        }
    }

    fn client(&self, cx: &App) -> Option<reno_client::Client> {
        self.store.read(cx).term_client()
    }

    fn fail(&mut self, fail: OpenFail, cx: &mut Context<Self>) {
        self.phase = Phase::Failed(fail);
        self.requested = false;
        cx.emit(ViewEvent::Changed);
        cx.notify();
    }

    /// `term.open`, as soon as the grid has been measured.
    fn try_open(&mut self, cx: &mut Context<Self>) {
        let Some(geometry) = self.geometry else {
            // The first paint measures the grid and comes back here.
            return;
        };
        let Some(client) = self.client(cx) else {
            self.fail(
                OpenFail::Other("The daemon is not connected.".to_owned()),
                cx,
            );
            return;
        };
        self.requested = true;
        let (cols, rows) = (geometry.cols, geometry.rows);
        let params = TermOpenParams {
            run_id: self.run.clone(),
            cols,
            rows,
            command: None,
        };
        let closer = client.clone();
        let task = Tokio::spawn(cx, async move { client.term_open(params).await });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let opened = match result {
                Ok(Ok(opened)) => opened,
                Ok(Err(error)) => {
                    this.update(cx, |view, cx| {
                        view.fail(OpenFail::of(error.code(), &remote_message(&error)), cx);
                    })
                    .ok();
                    return;
                }
                Err(error) => {
                    this.update(cx, |view, cx| {
                        view.fail(OpenFail::Other(error.to_string()), cx);
                    })
                    .ok();
                    return;
                }
            };
            let id = opened.id.clone();
            let kept = this.update(cx, |view, cx| view.opened(&opened, (cols, rows), cx));
            if kept.is_err() {
                // The tab was closed while the shell was starting: nobody will use it.
                Tokio::spawn(cx, async move { closer.term_close(id).await }).detach();
            }
        })
        .detach();
    }

    fn opened(&mut self, opened: &TermOpened, asked: (u16, u16), cx: &mut Context<Self>) {
        self.requested = false;
        self.id = Some(opened.id.clone());
        self.info = Some(Info::from(opened));
        self.phase = Phase::Live;
        self.sent_size = Some(asked);
        cx.emit(ViewEvent::Opened(opened.id.clone()));
        cx.emit(ViewEvent::Changed);
        // The grid may have changed while the open was in flight.
        self.schedule_resize(cx);
        self.pump(cx);
        if self.visible {
            self.attach_stream(cx);
        }
        cx.notify();
    }

    /// A restored tab: ask the daemon whether the shell is still there.
    fn resume(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.id.clone() else {
            return;
        };
        let Some(client) = self.client(cx) else {
            self.fail(
                OpenFail::Other("The daemon is not connected.".to_owned()),
                cx,
            );
            return;
        };
        self.requested = true;
        let run = self.run.clone();
        let task = Tokio::spawn(cx, async move { client.term_list(run).await });
        cx.spawn(async move |this, cx| {
            let listed = task.await;
            this.update(cx, |view, cx| {
                view.requested = false;
                match listed {
                    Ok(Ok(all)) => match all.into_iter().find(|t| t.id == id) {
                        Some(found) => {
                            view.info = Some(Info {
                                cwd: found.cwd,
                                shell: found.shell,
                                sandboxed: found.sandboxed,
                                in_place: found.in_place,
                            });
                            view.resuming = true;
                            view.phase = Phase::Live;
                            cx.emit(ViewEvent::Changed);
                            if view.visible {
                                view.attach_stream(cx);
                            }
                        }
                        None => view.gone(cx),
                    },
                    Ok(Err(error)) if error.code() == Some(ErrorCode::NotFound) => view.gone(cx),
                    Ok(Err(error)) => {
                        view.fail(OpenFail::Other(remote_message(&error)), cx);
                    }
                    Err(error) => view.fail(OpenFail::Other(error.to_string()), cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn gone(&mut self, cx: &mut Context<Self>) {
        self.phase = Phase::Gone;
        self.attach = None;
        cx.emit(ViewEvent::Changed);
        cx.notify();
    }

    // --- the attach stream --------------------------------------------------------------------

    fn after_seq(&self) -> Option<u64> {
        (self.last_seq > 0).then_some(self.last_seq)
    }

    /// Follows the shell's output from where this emulator left off, reconnecting with a
    /// growing wait when the stream ends without the shell's exit. The client is asked for on
    /// each attempt: the connection may have been replaced.
    fn attach_stream(&mut self, cx: &mut Context<Self>) {
        if self.attach.is_some() || self.id.is_none() || !self.visible {
            return;
        }
        if !matches!(self.phase, Phase::Live) {
            return;
        }
        self.attach = Some(cx.spawn(async move |this, cx| {
            let mut attempt: u32 = 0;
            loop {
                let Ok(plan) = this.update(cx, |view, cx| {
                    view.id
                        .clone()
                        .map(|id| (id, view.after_seq(), view.client(cx)))
                }) else {
                    return;
                };
                let Some((id, after, client)) = plan else {
                    return;
                };
                if let Some(client) = client {
                    #[cfg(test)]
                    this.update(cx, |view, _| view.taps.attaches.push(after))
                        .ok();
                    let (tx, mut rx) = futures::channel::mpsc::unbounded::<Msg>();
                    let pump = Tokio::spawn(cx, async move {
                        match client.term_attach(id, after).await {
                            Err(error) => {
                                tx.unbounded_send(Msg::Failed(error)).ok();
                            }
                            Ok(mut stream) => {
                                tx.unbounded_send(Msg::Started).ok();
                                while let Some(item) = stream.next().await {
                                    let msg = match item {
                                        Ok(event) => Msg::Item(event),
                                        Err(error) => {
                                            tx.unbounded_send(Msg::Failed(error)).ok();
                                            return;
                                        }
                                    };
                                    if tx.unbounded_send(msg).is_err() {
                                        return;
                                    }
                                }
                                tx.unbounded_send(Msg::Ended).ok();
                            }
                        }
                    });
                    while let Some(msg) = rx.next().await {
                        let item = matches!(msg, Msg::Item(_));
                        match this.update(cx, |view, cx| view.on_attach(msg, cx)) {
                            Ok(Flow::Continue) => {
                                if item {
                                    attempt = 0;
                                }
                            }
                            Ok(Flow::Stop) | Err(_) => return,
                        }
                    }
                    drop(pump);
                }
                cx.background_executor().timer(vm::backoff(attempt)).await;
                attempt = attempt.saturating_add(1);
            }
        }));
    }

    fn on_attach(&mut self, msg: Msg, cx: &mut Context<Self>) -> Flow {
        match msg {
            Msg::Started => {
                self.attached_at = Instant::now();
                self.resuming = self.resuming || self.last_seq > 0;
                Flow::Continue
            }
            Msg::Item(TermEvent::Data { seq, data }) => {
                self.last_seq = seq;
                if let Some(bytes) = reno_proto::decode_bytes(&data) {
                    self.feed(&bytes, cx);
                }
                Flow::Continue
            }
            Msg::Item(TermEvent::Gap { seq, .. }) => {
                self.last_seq = seq.saturating_sub(1);
                self.emu.feed(
                    b"\r\n\x1b[90m[some output was lost while this terminal was not shown]\x1b[0m\r\n",
                );
                cx.notify();
                Flow::Continue
            }
            Msg::Item(TermEvent::Exit { seq, code, signal }) => {
                self.last_seq = seq;
                self.marked = None;
                self.emu.feed(&vm::exit_message(code, signal.as_deref()));
                self.phase = Phase::Ended { code, signal };
                self.attach = None;
                cx.emit(ViewEvent::Changed);
                cx.notify();
                Flow::Stop
            }
            Msg::Failed(error) if error.code() == Some(ErrorCode::NotFound) => {
                self.gone(cx);
                Flow::Stop
            }
            Msg::Failed(_) | Msg::Ended => Flow::Continue,
        }
    }

    /// Folds output into the grid. What the emulator answers (device status and attribute
    /// queries) goes back through the same queue as typing, in order with it, unless the output
    /// is a replay: those queries were answered when they were live.
    fn feed(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        let before = self.title().map(str::to_owned);
        let replies = self.emu.feed(bytes);
        self.after_feed(replies, before, cx);
    }

    fn after_feed(&mut self, replies: Vec<u8>, before: Option<String>, cx: &mut Context<Self>) {
        let live = vm::replies_allowed(self.attached_at.elapsed(), self.resuming);
        if !replies.is_empty() && live {
            self.input.push(&replies);
            self.pump(cx);
        }
        if let Some(deadline) = self.emu.sync_deadline() {
            let wait = deadline.saturating_duration_since(Instant::now());
            self.sync_task = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(wait).await;
                this.update(cx, |view, cx| {
                    let replies = view.emu.finish_sync();
                    view.after_feed(replies, None, cx);
                })
                .ok();
            }));
        }
        if self.title().map(str::to_owned) != before {
            cx.emit(ViewEvent::Changed);
        }
        cx.notify();
    }

    // --- input --------------------------------------------------------------------------------

    /// Sends bytes as the person typed them: scrolled back, the view snaps to the live bottom
    /// first, and the cursor holds solid for a moment.
    fn queue_input(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if !matches!(self.phase, Phase::Live | Phase::Opening) {
            return;
        }
        if self.emu.display_offset() > 0 || self.scroll_remainder != 0.0 {
            self.emu.scroll_to_bottom();
            self.scroll_remainder = 0.0;
        }
        self.input.push(bytes);
        self.pump(cx);
        self.restart_blink(cx);
    }

    /// Sends bytes the program asked for (focus, pointer and wheel reports): in order with the
    /// typing, without a keystroke's side effects.
    fn send_report(&mut self, bytes: &[u8], cx: &mut Context<Self>) {
        if !matches!(self.phase, Phase::Live) {
            return;
        }
        self.input.push(bytes);
        self.pump(cx);
    }

    /// Starts the next `term.write` unless one is in flight; its completion pumps again, so a
    /// burst drains as fast as the daemon acknowledges it.
    fn pump(&mut self, cx: &mut Context<Self>) {
        let Some(id) = self.id.clone() else {
            return;
        };
        if !matches!(self.phase, Phase::Live) {
            return;
        }
        let Some(client) = self.client(cx) else {
            return;
        };
        let Some(bytes) = self.input.start() else {
            return;
        };
        #[cfg(test)]
        self.taps.writes.push(bytes.clone());
        let task = Tokio::spawn(cx, async move { client.term_write(id, &bytes).await });
        cx.spawn(async move |this, cx| {
            // A failed write is a shell that ended or a connection that dropped; the attach
            // stream says which. Either way the next write may start.
            let _ = task.await;
            this.update(cx, |view, cx| {
                view.input.finish();
                view.pump(cx);
            })
            .ok();
        })
        .detach();
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) else {
            return;
        };
        let bytes = vm::paste_bytes(&text, self.emu.bracketed_paste_mode());
        self.queue_input(&bytes, cx);
    }

    /// Copies the selection; whether anything was copied.
    fn copy(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(text) = self.emu.selection_text() else {
            return false;
        };
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        true
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let ks = &event.keystroke;
        let mods = &ks.modifiers;
        if ks.key == "v" && mods.platform {
            self.paste(cx);
            cx.stop_propagation();
            return;
        }
        // Only swallowed when it copied: Cmd-C with nothing selected is nobody's. Ctrl-C is
        // the shell's interrupt and never reaches the copy.
        if ks.key == "c" && mods.platform && !mods.control && self.copy(cx) {
            cx.stop_propagation();
            return;
        }
        match vm::route_key(
            &ks.key,
            ks.key_char.as_deref(),
            mods,
            self.emu.app_cursor_mode(),
        ) {
            KeyRoute::Bytes(bytes) => {
                self.queue_input(&bytes, cx);
                cx.stop_propagation();
            }
            // A held key repeats here, not through the platform text input, which would offer
            // its accent picker instead.
            KeyRoute::Text if event.is_held => {
                if let Some(text) = ks.key_char.clone() {
                    self.queue_input(text.as_bytes(), cx);
                    cx.stop_propagation();
                }
            }
            // The platform's text input delivers it, after any input method composed it.
            KeyRoute::Text => {}
            KeyRoute::NotOurs => {}
        }
    }

    // --- blink and focus ----------------------------------------------------------------------

    fn restart_blink(&mut self, cx: &mut Context<Self>) {
        self.blink_on = true;
        self.blink_task = None;
        if self.blink_wanted {
            self.blink_task = Some(Self::blink_loop(cx));
        }
    }

    /// The cursor blinks every half second and rests solid after a quiet spell, so an idle
    /// terminal schedules no frames; the next key starts it again.
    fn blink_loop(cx: &mut Context<Self>) -> Task<()> {
        let started = Instant::now();
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(BLINK).await;
                let keep = this.update(cx, |view, cx| {
                    if !view.blink_wanted || started.elapsed() >= BLINK_IDLE {
                        if !view.blink_on {
                            view.blink_on = true;
                            cx.notify();
                        }
                        return false;
                    }
                    view.blink_on = !view.blink_on;
                    cx.notify();
                    true
                });
                if !matches!(keep, Ok(true)) {
                    this.update(cx, |view, _| view.blink_task = None).ok();
                    return;
                }
            }
        })
    }

    /// Render-time bookkeeping for focus: report changes to programs that asked (1004) and run
    /// the blink only while a focused, blinking cursor is on screen in an active window.
    fn sync_focus(&mut self, focused: bool, window_active: bool, cx: &mut Context<Self>) {
        if focused != self.was_focused {
            self.was_focused = focused;
            if self.emu.focus_reporting() && matches!(self.phase, Phase::Live) {
                self.send_report(vm::focus_report(focused), cx);
            }
        }
        let wanted = focused
            && window_active
            && matches!(self.phase, Phase::Live)
            && self.emu.cursor_style().1
            && self.emu.cursor().is_some();
        if wanted != self.blink_wanted {
            self.blink_wanted = wanted;
            self.blink_on = true;
            self.blink_task = wanted.then(|| Self::blink_loop(cx));
        }
    }

    // --- the grid -----------------------------------------------------------------------------

    /// The element reports where the grid is, once per frame. The emulator is resized to match
    /// at once; the daemon hears of it after the debounce.
    fn on_grid(&mut self, geometry: Geometry, cx: &mut Context<Self>) {
        self.geometry = Some(geometry);
        let waiting = !self.requested && matches!(self.phase, Phase::Opening);
        if usize::from(geometry.cols) != self.emu.cols()
            || usize::from(geometry.rows) != self.emu.rows()
        {
            self.emu.resize(geometry.cols, geometry.rows);
            self.schedule_resize(cx);
        }
        if waiting && self.visible {
            // The grid is measured: a shell that waited for it can open.
            let view = cx.entity();
            cx.defer(move |cx| view.update(cx, |view, cx| view.begin(cx)));
        }
    }

    /// Tells the daemon the grid's size once it has held still for the debounce.
    fn schedule_resize(&mut self, cx: &mut Context<Self>) {
        if self.id.is_none() || !matches!(self.phase, Phase::Live) {
            return;
        }
        self.resize_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RESIZE_DEBOUNCE).await;
            let send = this
                .update(cx, |view, cx| {
                    let size = (view.emu.cols() as u16, view.emu.rows() as u16);
                    if view.sent_size == Some(size) {
                        return None;
                    }
                    let id = view.id.clone()?;
                    let client = view.client(cx)?;
                    view.sent_size = Some(size);
                    #[cfg(test)]
                    view.taps.resizes.push(size);
                    Some((client, id, size))
                })
                .ok()
                .flatten();
            if let Some((client, id, (cols, rows))) = send {
                let _ = Tokio::spawn(cx, async move { client.term_resize(id, cols, rows).await })
                    .await;
            }
        }));
    }

    fn snapshot(&self) -> Snapshot {
        let cursor_style = self.emu.cursor_style();
        Snapshot {
            lines: self.emu.lines(),
            cursor: self.emu.cursor(),
            shape: cursor_style.0,
            cursor_on: self.blink_on || !self.blink_wanted,
            marked: self.marked.clone(),
            live: matches!(self.phase, Phase::Live),
        }
    }

    fn point_at(&self, position: gpui::Point<Pixels>) -> Option<(GridPoint, Side)> {
        let g = self.geometry?;
        let hit = vm::cell_at(
            f32::from(position.x - g.origin.x),
            f32::from(position.y - g.origin.y),
            g.cell_w,
            g.line_h,
            usize::from(g.cols),
            usize::from(g.rows),
        );
        Some((self.emu.grid_point(hit.row, hit.col), hit.side))
    }

    // --- pointer ------------------------------------------------------------------------------

    /// The program's pointer mode, unless Shift is held: Shift always selects, so a terminal
    /// running a mouse-aware program still lets you copy from it.
    fn reporting(&self, mods: &gpui::Modifiers) -> Option<MouseTracking> {
        if mods.shift || !matches!(self.phase, Phase::Live) {
            return None;
        }
        self.emu.mouse_tracking()
    }

    fn report_mouse(
        &mut self,
        button: u8,
        position: gpui::Point<Pixels>,
        release: bool,
        mods: &gpui::Modifiers,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.reporting(mods).is_none() {
            return false;
        }
        self.send_mouse(button, position, release, mods, cx);
        true
    }

    fn send_mouse(
        &mut self,
        button: u8,
        position: gpui::Point<Pixels>,
        release: bool,
        mods: &gpui::Modifiers,
        cx: &mut Context<Self>,
    ) {
        let Some(g) = self.geometry else {
            return;
        };
        let hit = vm::cell_at(
            f32::from(position.x - g.origin.x),
            f32::from(position.y - g.origin.y),
            g.cell_w,
            g.line_h,
            usize::from(g.cols),
            usize::from(g.rows),
        );
        let motion = (MOUSE_MOTION..MOUSE_WHEEL_UP).contains(&button);
        if motion && self.report_cell == Some((hit.row, hit.col)) {
            return;
        }
        self.report_cell = Some((hit.row, hit.col));
        if let Some(bytes) = vm::mouse_report(
            button,
            hit.col,
            hit.row,
            release,
            mods,
            self.emu.sgr_mouse(),
        ) {
            self.send_report(&bytes, cx);
        }
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // The panel takes focus on a click; the terminal keeps it.
        cx.stop_propagation();
        window.focus(&self.focus, cx);
        if self.report_mouse(MOUSE_LEFT, event.position, false, &event.modifiers, cx) {
            self.report_button = Some(MOUSE_LEFT);
            return;
        }
        let Some((point, side)) = self.point_at(event.position) else {
            return;
        };
        let ty = match event.click_count {
            0 => return,
            1 => SelectionType::Simple,
            2 => SelectionType::Semantic,
            _ => SelectionType::Lines,
        };
        if ty == SelectionType::Simple {
            // Shift-click extends a selection instead of replacing it.
            if event.modifiers.shift && self.emu.has_selection() {
                self.emu.update_selection(point, side);
                self.drag = Some(Drag {
                    origin: event.position,
                    position: event.position,
                    armed: true,
                });
                cx.notify();
                return;
            }
            self.emu.clear_selection();
            self.drag = Some(Drag {
                origin: event.position,
                position: event.position,
                armed: false,
            });
        } else {
            // A word or a line is complete on the press; the drag extends it by that unit.
            self.emu.start_selection(ty, point, side);
            self.drag = Some(Drag {
                origin: event.position,
                position: event.position,
                armed: true,
            });
        }
        cx.notify();
    }

    fn on_mouse_move(&mut self, event: &MouseMoveEvent, _: &mut Window, cx: &mut Context<Self>) {
        let tracking = self.reporting(&event.modifiers);
        let report = match tracking {
            Some(MouseTracking::Motion) => true,
            Some(MouseTracking::Drag) => self.report_button.is_some(),
            _ => false,
        };
        if report {
            let button = self.report_button.unwrap_or(MOUSE_NONE) + MOUSE_MOTION;
            self.report_mouse(button, event.position, false, &event.modifiers, cx);
            return;
        }
        if tracking.is_some() && self.report_button.is_some() {
            return;
        }
        if !event.dragging() {
            return;
        }
        let Some(mut drag) = self.drag else {
            return;
        };
        drag.position = event.position;
        self.drag = Some(drag);
        if !drag.armed {
            let dx = f32::from(event.position.x - drag.origin.x);
            let dy = f32::from(event.position.y - drag.origin.y);
            if dx.hypot(dy) < DRAG_THRESHOLD {
                return;
            }
            // Anchored at the press, not where the threshold tripped.
            let Some((anchor, side)) = self.point_at(drag.origin) else {
                return;
            };
            self.emu
                .start_selection(SelectionType::Simple, anchor, side);
            self.drag = Some(Drag {
                armed: true,
                ..drag
            });
        }
        if let Some((point, side)) = self.point_at(event.position) {
            self.emu.update_selection(point, side);
            cx.notify();
            self.schedule_edge_scroll(cx);
        }
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, _: &mut Window, cx: &mut Context<Self>) {
        self.drag = None;
        self.edge_task = None;
        if let Some(button) = self.report_button.take() {
            // Whatever the program was told is held, it is told is released.
            self.send_mouse(button, event.position, true, &event.modifiers, cx);
            self.report_cell = None;
        }
    }

    fn on_other_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.stop_propagation();
        window.focus(&self.focus, cx);
        let button = match event.button {
            MouseButton::Middle => MOUSE_MIDDLE,
            MouseButton::Right => MOUSE_RIGHT,
            _ => return,
        };
        if self.report_mouse(button, event.position, false, &event.modifiers, cx) {
            self.report_button = Some(button);
        }
    }

    fn edge_lines(&self) -> i32 {
        match (self.drag, self.geometry) {
            (Some(drag), Some(g)) if drag.armed => vm::selection_scroll_lines(
                f32::from(g.origin.y),
                g.line_h,
                usize::from(g.rows),
                f32::from(drag.position.y),
            ),
            _ => 0,
        }
    }

    /// A selection dragged past the grid's edge scrolls the history and follows it.
    fn schedule_edge_scroll(&mut self, cx: &mut Context<Self>) {
        if self.edge_task.is_some() || self.edge_lines() == 0 {
            return;
        }
        self.edge_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(EDGE_SCROLL_TICK).await;
            this.update(cx, |view, cx| {
                view.edge_task = None;
                let lines = view.edge_lines();
                if lines == 0 {
                    return;
                }
                view.emu.scroll(lines);
                view.scroll_remainder = 0.0;
                if let Some(drag) = view.drag
                    && let Some((point, side)) = view.point_at(drag.position)
                {
                    view.emu.update_selection(point, side);
                }
                cx.notify();
                view.schedule_edge_scroll(cx);
            })
            .ok();
        }));
    }

    /// Wheel movement in rows (positive is up): to the program as wheel reports when it tracks
    /// the mouse, as arrow keys to a full-screen program that does not (less, man), else through
    /// the scrollback, by the fraction, flush to the live bottom and to the history's top.
    fn wheel(
        &mut self,
        rows: f32,
        position: gpui::Point<Pixels>,
        mods: &gpui::Modifiers,
        cx: &mut Context<Self>,
    ) {
        let reporting = self.reporting(mods).is_some();
        let arrows = matches!(self.phase, Phase::Live)
            && self.emu.alt_screen()
            && self.emu.alternate_scroll()
            && !mods.shift;
        if !(reporting || arrows) {
            let (offset, rest) = vm::scroll_target(
                self.emu.display_offset(),
                self.scroll_remainder,
                rows,
                self.emu.history_lines(),
            );
            self.emu.scroll_to_offset(offset);
            self.scroll_remainder = rest;
            cx.notify();
            return;
        }
        let (step, rest) = vm::whole_steps(self.wheel_owed + rows);
        self.wheel_owed = rest;
        if step == 0 {
            return;
        }
        if reporting {
            let button = if step > 0 {
                MOUSE_WHEEL_UP
            } else {
                MOUSE_WHEEL_DOWN
            };
            for _ in 0..step.unsigned_abs() {
                self.send_mouse(button, position, false, mods, cx);
            }
        } else {
            self.send_report(&vm::wheel_arrows(step, self.emu.app_cursor_mode()), cx);
        }
    }

    fn on_scroll(&mut self, event: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let rows = match event.delta {
            ScrollDelta::Lines(delta) => delta.y,
            ScrollDelta::Pixels(delta) => {
                let line_h = self.geometry.map_or(LINE_HEIGHT, |g| g.line_h);
                vm::rows_of_pixels(f32::from(delta.y), line_h)
            }
        };
        // A fresh gesture owes the program nothing from the last one.
        if event.touch_phase == TouchPhase::Started {
            self.wheel_owed = 0.0;
        }
        self.wheel(rows, event.position, &event.modifiers, cx);
        cx.stop_propagation();
    }
}

fn remote_message(error: &ClientError) -> String {
    match error {
        ClientError::Remote(object) => object.message.clone(),
        other => other.to_string(),
    }
}

/// What the element paints, taken from the view once per frame.
struct Snapshot {
    lines: Vec<Vec<CellSnapshot>>,
    cursor: Option<CursorSnapshot>,
    shape: CursorShape,
    cursor_on: bool,
    marked: Option<String>,
    live: bool,
}

// --- text input (typing, input methods) ---------------------------------------------------------

impl EntityInputHandler for TerminalView {
    fn text_for_range(
        &mut self,
        range: std::ops::Range<usize>,
        adjusted: &mut Option<std::ops::Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let utf16: Vec<u16> = self.marked.as_ref()?.encode_utf16().collect();
        let range = range.start.min(utf16.len())..range.end.min(utf16.len());
        *adjusted = Some(range.clone());
        Some(String::from_utf16_lossy(&utf16[range]))
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = self
            .marked
            .as_ref()
            .map_or(0, |text| text.encode_utf16().count());
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        Some(0..self.marked.as_ref()?.encode_utf16().count())
    }

    /// The platform accepts the composition as typed (focus moving away mid-composition): it
    /// goes to the shell rather than vanishing.
    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = self.marked.take().filter(|t| !t.is_empty()) {
            self.queue_input(text.as_bytes(), cx);
        }
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<std::ops::Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = None;
        if !text.is_empty() {
            self.queue_input(text.as_bytes(), cx);
        }
        cx.notify();
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<std::ops::Range<usize>>,
        new_text: &str,
        _: Option<std::ops::Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked = (!new_text.is_empty()).then(|| new_text.to_owned());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: std::ops::Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        // The candidate window opens under the cursor cell.
        let g = self.geometry?;
        let cursor = self.emu.cursor()?;
        Some(Bounds::new(
            point(
                g.origin.x + px(g.cell_w * cursor.col as f32),
                g.origin.y + px(g.line_h * cursor.row as f32),
            ),
            size(px(g.cell_w), px(g.line_h)),
        ))
    }

    fn character_index_for_point(
        &mut self,
        _: gpui::Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

impl Render for TerminalView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if std::mem::take(&mut self.focus_pending) {
            window.focus(&self.focus, cx);
        }
        let focused = self.focus.is_focused(window);
        self.sync_focus(focused, window.is_window_active(), cx);
        let theme = theme::current(cx);
        div()
            .id("terminal-view")
            .debug_selector(|| "terminal-view".to_owned())
            .size_full()
            .relative()
            .bg(theme.canvas)
            .key_context("Terminal")
            .track_focus(&self.focus)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            // On the window, not the element: a drag that ends outside still ends the gesture.
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_other_mouse_down))
            .on_mouse_down(MouseButton::Right, cx.listener(Self::on_other_mouse_down))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_up(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Right, cx.listener(Self::on_mouse_up))
            .on_scroll(cx.listener(Self::on_scroll))
            .child(TerminalElement {
                view: cx.entity(),
                focused,
            })
    }
}

// --- the element --------------------------------------------------------------------------------

/// Paints the grid. The cell size comes from the real mono font each frame (the font probe);
/// the measured columns and rows go back to the view.
pub(crate) struct TerminalElement {
    view: Entity<TerminalView>,
    focused: bool,
}

pub(crate) struct Prepaint {
    background: Vec<PaintQuad>,
    selection: Vec<PaintQuad>,
    /// Per row: its top edge, and the shaped segments with the column each starts at.
    rows: Vec<(Pixels, Vec<(usize, ShapedLine)>)>,
    cell_w: Pixels,
    line_h: Pixels,
    cursor: Option<PaintQuad>,
    marked: Option<(PaintQuad, gpui::Point<Pixels>, ShapedLine)>,
}

/// Beam and underline cursor thickness.
const CURSOR_BAR: f32 = 2.0;
/// Selection corner rounding, capped for small fonts.
const SELECTION_RADIUS: f32 = 3.0;

/// The grid's font: ligatures off. A terminal is a fixed grid, and a contextual substitution
/// (`->`, `--`) collapses several cells into fewer glyphs, so the row would paint short while
/// the cursor, a quad at `cell_w * col`, stays on the true column.
fn mono() -> gpui::Font {
    let mut mono = font(MONO);
    mono.features = gpui::FontFeatures(std::sync::Arc::new(vec![
        ("liga".into(), 0),
        ("calt".into(), 0),
        ("dlig".into(), 0),
    ]));
    mono
}

impl Element for TerminalElement {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let theme = theme::current(cx);
        let mono = mono();
        let font_size = px(FONT_SIZE);
        let font_id = window.text_system().resolve_font(&mono);
        let cell_w = window
            .text_system()
            .em_advance(font_id, font_size)
            .unwrap_or(px(FONT_SIZE * 0.6));
        let line_h = px(LINE_HEIGHT);
        let origin = point(bounds.left() + px(PADDING), bounds.top() + px(PADDING));
        let grid = vm::grid_for(
            f32::from(bounds.size.width),
            f32::from(bounds.size.height),
            f32::from(cell_w),
            f32::from(line_h),
            PADDING,
        );
        let snapshot = self.view.update(cx, |view, cx| {
            if let Some((cols, rows)) = grid {
                view.on_grid(
                    Geometry {
                        bounds,
                        origin,
                        cell_w: f32::from(cell_w),
                        line_h: f32::from(line_h),
                        cols,
                        rows,
                    },
                    cx,
                );
            }
            view.snapshot()
        });
        let mut out = Prepaint {
            background: Vec::new(),
            selection: Vec::new(),
            rows: Vec::with_capacity(snapshot.lines.len()),
            cell_w,
            line_h,
            cursor: None,
            marked: None,
        };
        let radius = px(SELECTION_RADIUS.min(f32::from(line_h) / 4.0));
        let wash = theme.accent.opacity(0.28);
        let row_count = snapshot.lines.len();
        for (row_ix, row) in snapshot.lines.iter().enumerate() {
            let y = origin.y + line_h * row_ix as f32;
            let mut sel_start: Option<usize> = None;
            for col in 0..=row.len() {
                let selected = row.get(col).is_some_and(|cell| cell.selected);
                match (sel_start, selected) {
                    (None, true) => sel_start = Some(col),
                    (Some(start), false) => {
                        out.selection.push(
                            fill(
                                Bounds::new(
                                    point(origin.x + cell_w * start as f32, y),
                                    size(cell_w * (col - start) as f32, line_h),
                                ),
                                wash,
                            )
                            .corner_radii(radius),
                        );
                        sel_start = None;
                    }
                    _ => {}
                }
            }
            // Consecutive non-default backgrounds merge into one quad. A run touching the
            // grid's edge carries its colour into the padding beside it, so a full-bleed
            // program background reaches the panel's edge.
            let edge = |start: usize, end: usize, color: Hsla| {
                let x0 = if start == 0 {
                    bounds.left()
                } else {
                    origin.x + cell_w * start as f32
                };
                let x1 = if end == row.len() {
                    bounds.right()
                } else {
                    origin.x + cell_w * end as f32
                };
                let y0 = if row_ix == 0 { bounds.top() } else { y };
                let y1 = if row_ix + 1 == row_count {
                    bounds.bottom()
                } else {
                    y + line_h
                };
                fill(Bounds::from_corners(point(x0, y0), point(x1, y1)), color)
            };
            let mut run: Option<(usize, Hsla)> = None;
            for (col, color) in row
                .iter()
                .map(|cell| cell.display_colors().1)
                .chain(std::iter::once(CellColor::Background))
                .enumerate()
            {
                let paint = match color {
                    CellColor::Background => None,
                    other => Some(vm::resolve_color(other, &theme)),
                };
                match (&run, paint) {
                    (None, Some(color)) => run = Some((col, color)),
                    (Some((start, current)), next) if next != Some(*current) => {
                        out.background.push(edge(*start, col, *current));
                        run = next.map(|color| (col, color));
                    }
                    _ => {}
                }
            }
            out.rows
                .push((y, shape_row(row, &theme, &mono, font_size, window)));
        }
        if let Some(c) = snapshot.cursor.filter(|_| snapshot.cursor_on && snapshot.live) {
            let cell = Bounds::new(
                point(
                    origin.x + cell_w * c.col as f32,
                    origin.y + line_h * c.row as f32,
                ),
                size(cell_w, line_h),
            );
            let bar = px(CURSOR_BAR);
            let ink = theme.text;
            out.cursor = Some(match (self.focused, snapshot.shape) {
                (false, _) | (true, CursorShape::HollowBlock) => {
                    outline(cell, ink, gpui::BorderStyle::Solid)
                }
                (true, CursorShape::Block) => fill(cell, ink.opacity(0.55)),
                (true, CursorShape::Beam) => fill(Bounds::new(cell.origin, size(bar, line_h)), ink),
                (true, CursorShape::Underline) => fill(
                    Bounds::new(point(cell.left(), cell.bottom() - bar), size(cell_w, bar)),
                    ink,
                ),
            });
        }
        // Input-method composition sits over the cursor on an opaque backdrop, underlined as
        // every platform marks input that is not yet confirmed.
        out.marked = snapshot
            .marked
            .as_deref()
            .filter(|text| !text.is_empty())
            .zip(snapshot.cursor)
            .map(|(text, c)| {
                let at = point(
                    origin.x + cell_w * c.col as f32,
                    origin.y + line_h * c.row as f32,
                );
                let shaped = window.text_system().shape_line(
                    SharedString::from(text.to_owned()),
                    font_size,
                    &[TextRun {
                        len: text.len(),
                        font: mono.clone(),
                        color: theme.text,
                        background_color: None,
                        underline: Some(gpui::UnderlineStyle {
                            color: Some(theme.text),
                            thickness: px(1.0),
                            wavy: false,
                        }),
                        strikethrough: None,
                    }],
                    None,
                );
                let backdrop = fill(
                    Bounds::new(at, size(shaped.width.max(cell_w), line_h)),
                    theme.canvas,
                );
                (backdrop, at, shaped)
            });
        out
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        prepaint: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let line_h = prepaint.line_h;
        let cell_w = prepaint.cell_w;
        let origin = point(bounds.left() + px(PADDING), bounds.top() + px(PADDING));
        window.with_content_mask(Some(gpui::ContentMask { bounds }), |window| {
            for quad in prepaint.background.drain(..) {
                window.paint_quad(quad);
            }
            // The wash tints a cell's own background and sits under the glyphs it highlights.
            for quad in prepaint.selection.drain(..) {
                window.paint_quad(quad);
            }
            for (y, segments) in &prepaint.rows {
                for (col, line) in segments {
                    let _ = line.paint(
                        point(origin.x + cell_w * *col as f32, *y),
                        line_h,
                        gpui::TextAlign::Left,
                        None,
                        window,
                        cx,
                    );
                }
            }
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
            if let Some((backdrop, at, text)) = prepaint.marked.take() {
                window.paint_quad(backdrop);
                let _ = text.paint(at, line_h, gpui::TextAlign::Left, None, window, cx);
            }
        });
        // Typed text and input-method composition enter here.
        if self.focused {
            let focus = self.view.read(cx).focus.clone();
            window.handle_input(
                &focus,
                ElementInputHandler::new(bounds, self.view.clone()),
                cx,
            );
        }
    }
}

/// Shapes one grid row into column-pinned segments.
///
/// A shaped line places glyphs by their font advances, which agree with the grid only while
/// every glyph is the mono font's. A glyph from a fallback font (box drawing, arrows, emoji,
/// CJK) has another advance and the rest of the line slides out of its columns. So runs of
/// ASCII shape together, and every other glyph is a segment of its own, pinned at its own
/// column. A wide glyph's spacer is skipped: the glyph covers both columns.
fn shape_row(
    row: &[CellSnapshot],
    theme: &Theme,
    mono: &gpui::Font,
    font_size: Pixels,
    window: &Window,
) -> Vec<(usize, ShapedLine)> {
    fn flush(
        segments: &mut Vec<(usize, ShapedLine)>,
        text: &mut String,
        runs: &mut Vec<TextRun>,
        at: usize,
        font_size: Pixels,
        window: &Window,
    ) {
        if text.is_empty() {
            return;
        }
        let shaped = window.text_system().shape_line(
            SharedString::from(std::mem::take(text)),
            font_size,
            runs,
            None,
        );
        segments.push((at, shaped));
        runs.clear();
    }

    let mut segments = Vec::new();
    let mut text = String::with_capacity(row.len());
    let mut runs: Vec<TextRun> = Vec::new();
    let mut at = 0usize;
    for (col, cell) in row.iter().enumerate() {
        if cell.wide_spacer {
            continue;
        }
        let ch = if cell.hidden { ' ' } else { cell.ch };
        let pinned = !ch.is_ascii() || cell.wide;
        if pinned {
            flush(&mut segments, &mut text, &mut runs, at, font_size, window);
        }
        if text.is_empty() {
            at = col;
        }
        let (fg, bg) = cell.display_colors();
        let mut color = vm::resolve_color(fg, theme);
        // The theme's own default pair is tuned; what a program coloured gets the floor.
        if !(fg == CellColor::Foreground && bg == CellColor::Background) {
            color = vm::ensure_contrast(color, vm::resolve_color(bg, theme), MIN_CONTRAST);
        }
        if cell.dim {
            color.a *= 0.6;
        }
        let mut cell_font = mono.clone();
        cell_font.weight = if cell.bold {
            gpui::FontWeight::BOLD
        } else {
            gpui::FontWeight::NORMAL
        };
        cell_font.style = if cell.italic {
            gpui::FontStyle::Italic
        } else {
            gpui::FontStyle::Normal
        };
        let underline = cell.underline.then_some(gpui::UnderlineStyle {
            color: Some(color),
            thickness: px(1.0),
            wavy: false,
        });
        let len = ch.len_utf8();
        text.push(ch);
        match runs.last_mut() {
            Some(last)
                if last.color == color && last.font == cell_font && last.underline == underline =>
            {
                last.len += len;
            }
            _ => runs.push(TextRun {
                len,
                font: cell_font,
                color,
                background_color: None,
                underline,
                strikethrough: None,
            }),
        }
        if pinned {
            flush(&mut segments, &mut text, &mut runs, at, font_size, window);
        }
    }
    flush(&mut segments, &mut text, &mut runs, at, font_size, window);
    segments
}
