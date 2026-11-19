//! The composer: one multi-line text input used on Home (a new task), on the Run screen (steer a
//! run, or stop it) and under a held action (the reason for a denial).
//!
//! The text, caret, selection, undo history and IME composition live in [`TextModel`]. The
//! painted text and its scrolling are in `text_area`. This file owns what needs GPUI beyond that:
//! key actions, the platform input handler (IME, `NSTextInputClient` on macOS), the mouse, the
//! files attached to the draft, the route preview, and the chrome around the text.
//!
//! Text: typing, Backspace/Delete, caret and selection by keyboard and mouse (grapheme-aware),
//! Option+arrows and Option+Backspace by word, Cmd+arrows by line and text, Up/Down by visual
//! row, double and triple click, select all, cut, copy, paste, undo and redo, IME composition
//! (marked text underlined, candidate window placed at the marked text), Enter to send,
//! Shift+Enter for a newline, auto-grow up to [`MAX_ROWS`] and then scroll.
//!
//! Home: class chips; files attached by paste, drop or the paperclip, shown as chips that can be
//! taken off; a route preview from `route.preview` (debounced, one request at a time) with a
//! "why?" list of the executors passed over and why.
//!
//! Run screen: while the run is live, an empty draft shows Stop and a draft shows Steer; a
//! finished run disables the box and says why.
//!
//! [`Mode::Reason`] is the small field under a held action on the Run screen, where the human
//! denies it: no chips, no send button, one row; Enter and Escape are reported as a
//! [`ReasonEvent`] for the card to act on.

use std::cell::Cell;
use std::ops::Range;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    App, Bounds, ClipboardEntry, ClipboardItem, Context, CursorStyle, Entity, EntityInputHandler,
    EventEmitter, ExternalPaths, FocusHandle, Focusable, FontWeight, IntoElement, KeyBinding,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PathPromptOptions, Pixels, Point,
    Render, ScrollEvent, SharedString, Subscription, Task, UTF16Selection, Window, actions, canvas,
    div, prelude::*, px,
};
use reno_client::ConnectionStatus;
use reno_proto::{ExecutorId, RoutingDecision, RunId, RunStatus, TaskRunParams};

mod chips;
mod menu;
mod picker;
mod rings;

use super::icon::{Icon, icon};
use super::kit;
use super::overlay;
use super::text_area::{TextArea, TextLayout};
use super::text_model::{EnterOutcome, TextModel};
use super::theme::{self, Theme};
use super::toast;
use crate::attach;
use crate::store::{Store, StoreEvent};
use crate::summary;
use crate::vm::models;
use crate::vm::picker::Tab;
use crate::vm::routing::{self, Class, Preview};
use crate::vm::threads;
use menu::{check, menu, menu_row};

actions!(
    composer,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        WordLeft,
        WordRight,
        SelectWordLeft,
        SelectWordRight,
        DeleteWordBack,
        DeleteWordForward,
        DeleteLineBack,
        DocStart,
        DocEnd,
        SelectDocStart,
        SelectDocEnd,
        SelectAll,
        Home,
        End,
        SelectHome,
        SelectEnd,
        Paste,
        Copy,
        Cut,
        Undo,
        Redo,
        Submit,
        Cancel,
        Newline,
        ShowCharacterPalette,
    ]
);

/// The key context the bindings below are scoped to.
const CONTEXT: &str = "Composer";
/// Home's text area is at least this many rows tall; the others start at one.
const HOME_ROWS: usize = 2;
/// How long the text must stand still before the route is asked for again.
pub(crate) const PREVIEW_DEBOUNCE: Duration = Duration::from_millis(400);
/// How long "Sent" stays on a steer box.
const SENT_FOR: Duration = Duration::from_millis(2_500);

pub(crate) fn bind_keys(cx: &mut App) {
    let ctx = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("up", Up, ctx),
        KeyBinding::new("down", Down, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("shift-up", SelectUp, ctx),
        KeyBinding::new("shift-down", SelectDown, ctx),
        KeyBinding::new("alt-left", WordLeft, ctx),
        KeyBinding::new("alt-right", WordRight, ctx),
        KeyBinding::new("alt-shift-left", SelectWordLeft, ctx),
        KeyBinding::new("alt-shift-right", SelectWordRight, ctx),
        KeyBinding::new("alt-backspace", DeleteWordBack, ctx),
        KeyBinding::new("alt-delete", DeleteWordForward, ctx),
        KeyBinding::new("cmd-backspace", DeleteLineBack, ctx),
        KeyBinding::new("cmd-up", DocStart, ctx),
        KeyBinding::new("cmd-down", DocEnd, ctx),
        KeyBinding::new("cmd-shift-up", SelectDocStart, ctx),
        KeyBinding::new("cmd-shift-down", SelectDocEnd, ctx),
        KeyBinding::new("cmd-a", SelectAll, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("end", End, ctx),
        KeyBinding::new("cmd-left", Home, ctx),
        KeyBinding::new("cmd-right", End, ctx),
        KeyBinding::new("ctrl-a", Home, ctx),
        KeyBinding::new("ctrl-e", End, ctx),
        KeyBinding::new("shift-home", SelectHome, ctx),
        KeyBinding::new("shift-end", SelectEnd, ctx),
        KeyBinding::new("cmd-shift-left", SelectHome, ctx),
        KeyBinding::new("cmd-shift-right", SelectEnd, ctx),
        KeyBinding::new("cmd-v", Paste, ctx),
        KeyBinding::new("cmd-c", Copy, ctx),
        KeyBinding::new("cmd-x", Cut, ctx),
        KeyBinding::new("cmd-z", Undo, ctx),
        KeyBinding::new("cmd-shift-z", Redo, ctx),
        KeyBinding::new("enter", Submit, ctx),
        KeyBinding::new("escape", Cancel, ctx),
        KeyBinding::new("shift-enter", Newline, ctx),
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, ctx),
    ]);
}

/// What the composer is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Home: a new task, with class chips and a route preview.
    Home,
    /// The Run screen: steers this run while it is live.
    Steer(RunId),
    /// A held action's reason for a denial: Enter and Escape are emitted as [`ReasonEvent`]s.
    Reason,
}

/// What a [`Mode::Reason`] composer reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ReasonEvent {
    /// Enter: deny, with this reason (`None` when it was left blank).
    Submitted(Option<String>),
    /// Escape: do not deny after all.
    Cancelled,
}

impl EventEmitter<ReasonEvent> for Composer {}

/// The picker's Account row asks for the Executors screen.
pub(crate) struct OpenExecutors;

impl EventEmitter<OpenExecutors> for Composer {}

/// What a steer box can do for a run in this state.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SteerState {
    /// The run is live and takes messages: Steer, or Stop with nothing typed.
    Steering,
    /// The run is not over but cannot take a message now. Stop still works.
    Waiting(String),
    /// The run is over: what is sent starts a new run in the same project (ui-brief
    /// 2026-11-17), and the reason says how it ended.
    Ended(String),
}

/// What the round button at the box's foot does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DockAction {
    /// Send what is typed (dim while nothing is).
    Send,
    /// Stop the run: the box is empty and the run can be stopped.
    Stop,
}

/// The send / stop morph: Stop (a square) while the run can be stopped and the box has nothing to
/// send; Send as soon as there is text.
pub(crate) fn dock_action(stoppable: bool, has_text: bool) -> DockAction {
    if stoppable && !has_text {
        DockAction::Stop
    } else {
        DockAction::Send
    }
}

/// The steer box's state for `run` (`None` before the run is known).
pub(crate) fn steer_state(run: Option<&reno_proto::RunSummary>) -> SteerState {
    let Some(run) = run else {
        return SteerState::Waiting("Loading the run\u{2026}".to_owned());
    };
    // A person's stop is final; the daemon only takes a moment to settle it as cancelled.
    if summary::stopped_by_person(run) {
        return ended("was stopped");
    }
    match run.status {
        RunStatus::Succeeded => match summary::reported_problem(run) {
            Some(summary::Problem::Failed) => ended("reported a failure"),
            Some(summary::Problem::Blocked) => ended("reported it is blocked"),
            None => ended("passed"),
        },
        RunStatus::Failed => ended("failed"),
        RunStatus::Cancelled => ended("was stopped"),
        RunStatus::Interrupted => SteerState::Waiting(
            "This run was interrupted. It can take a message once it resumes.".to_owned(),
        ),
        _ => SteerState::Steering,
    }
}

fn ended(how: &str) -> SteerState {
    SteerState::Ended(format!("This run {how}."))
}

/// The task a message to an ended run starts: the same folder and bot, a fresh session. With a
/// `thread` it continues that thread (its worktree and branch); without, a new one starts.
pub(crate) fn follow_up(
    run: &reno_proto::RunSummary,
    prompt: &str,
    thread: Option<reno_proto::ThreadId>,
) -> TaskRunParams {
    let bot = run.bot.as_ref().map(|pin| pin.name.clone());
    TaskRunParams {
        model_options: Default::default(),
        effort: run.effort,
        prompt: prompt.to_owned(),
        // The harness and model the run ended on (a model switched mid-run included), unless the
        // box picks others.
        executor: Some(run.executor.clone()),
        bot,
        cwd: run.cwd.clone(),
        thread_id: thread,
        model: run.model.clone(),
        in_place: false,
        attachments: Vec::new(),
    }
}

/// The note above an ended run's box: what a message sent now does. On a daemon with threads it
/// continues the thread, unless the thread's worktree is gone.
pub(crate) fn ended_note(why: &str, project: &str, threads: bool, gone: bool) -> String {
    match (threads, gone) {
        (true, true) => format!(
            "This thread is closed and its worktree is gone. What you send starts a new thread in {project}."
        ),
        (true, false) => format!("{why} What you send continues this thread in {project}."),
        (false, _) => format!("{why} What you send starts a new run in {project}."),
    }
}

/// What the route preview was last asked for. A new request goes out only when this changes.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PreviewKey {
    task: TaskRunParams,
    /// The probes the answer depends on: a fresh probe can change the decision.
    probes: Vec<(ExecutorId, Option<u64>)>,
}

/// A draft that has been sent and may have to come back.
struct Sent {
    ticket: u64,
    text: String,
    files: Vec<attach::Item>,
}

pub(crate) struct Composer {
    store: Entity<Store>,
    mode: Mode,
    focus_handle: FocusHandle,
    model: TextModel,
    class: Class,
    files: attach::Draft,
    why_open: bool,
    preview: Preview,
    preview_key: Option<PreviewKey>,
    debounce: Option<Task<()>>,
    request: Option<Task<()>>,
    generation: u64,
    layout: Option<TextLayout>,
    /// How far the text is scrolled, from its top.
    scroll: Pixels,
    /// The next paint scrolls the caret into view.
    reveal: bool,
    /// The x a run of Up/Down moves keeps.
    goal_x: Option<Pixels>,
    selecting: bool,
    next_ticket: u64,
    sent: Option<Sent>,
    /// The daemon refused a message to the thread: the note under the box says why, until the
    /// next send or edit.
    refusal: Option<threads::Refusal>,
    /// The thread is gone (closed, worktree removed): the next message starts a new one.
    thread_gone: bool,
    /// "Sent" shows until this task ends.
    sent_note: Option<Task<()>>,
    /// A [`Mode::Reason`] field's own placeholder and hint, when it is not a denial's reason.
    labels: Option<(SharedString, SharedString)>,
    /// A [`Mode::Reason`] line drawn with no box of its own (the question panel's "write your
    /// own response").
    bare: bool,
    /// The harness · model chosen for the next task (`None`: Auto). On a live run's steer box
    /// it is unused: a live run's model is changed on the daemon (`run.set_model`).
    pick: Option<models::Pick>,
    /// Whether the harness · model picker is open, the tab it views, what is typed in its search,
    /// and the row the keyboard is on.
    picker_open: bool,
    picker_tab: Tab,
    picker_query: String,
    picker_cursor: Option<usize>,
    /// The keyboard moved the tab: its name shows, as it does on hover.
    picker_keyboard: bool,
    /// The picker's own focus, so typing goes to its search and not to the text.
    picker_focus: FocusHandle,
    /// The "+" menu, the permission menu and the effort popover are open, and where the "+" and
    /// the permission chip were last painted.
    plus_open: bool,
    permission_open: bool,
    effort_open: bool,
    plus_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    permission_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The permission mode picked here (`None`: the harness's first) and the "+" menu's toggles.
    /// Until the daemon takes them (ROAD-4) they live in the box only, and show on the fake.
    permission: Option<String>,
    plan_mode: bool,
    goal: bool,
    /// The context ring's card on the run's foot is open.
    context_card_open: bool,
    /// The Account row shows its plan meters.
    usage_card_open: bool,
    /// The Context window row's sizes are showing.
    picker_window_open: bool,
    /// Where the context ring was last painted, for its card.
    ring_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// The picker closed: the next frame puts the keyboard back in the text.
    refocus: bool,
    /// Whether the class menu is open.
    class_open: bool,
    /// Where the model chip and the class button were last painted, for their menus to attach
    /// to.
    chip_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    class_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Whether the open picker was for a live run (switch it) or a next task. When the run
    /// ends or starts under it, the picker closes rather than change meaning under the cursor.
    picker_for_live: bool,
    /// Home: the folder a new thread works in (a project's "+"), or none for the daemon's own.
    folder: Option<PathBuf>,
    _store: Subscription,
    _events: Subscription,
}

impl Composer {
    pub(crate) fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        Self::with_mode(store, Mode::Home, cx)
    }

    fn with_mode(store: Entity<Store>, mode: Mode, cx: &mut Context<Self>) -> Self {
        let mut composer = Self {
            _store: cx.observe(&store, |this, _, cx| {
                if this.picker_open && this.live_run(cx).is_some() != this.picker_for_live {
                    this.close_picker(cx);
                }
                this.refresh_preview(cx);
                cx.notify();
            }),
            _events: cx.subscribe(&store, |this, _, event: &StoreEvent, cx| match event {
                StoreEvent::SendFailed(ticket) => this.take_draft_back(*ticket, cx),
                StoreEvent::SendRefused { ticket, why } if *ticket == this.next_ticket => {
                    this.refuse(why, cx);
                }
                _ => {}
            }),
            store,
            mode,
            focus_handle: cx.focus_handle(),
            model: TextModel::new(),
            class: Class::default(),
            files: attach::Draft::default(),
            why_open: false,
            preview: Preview::Idle,
            preview_key: None,
            debounce: None,
            request: None,
            generation: 0,
            layout: None,
            scroll: px(0.0),
            reveal: false,
            goal_x: None,
            selecting: false,
            next_ticket: 0,
            sent: None,
            refusal: None,
            thread_gone: false,
            sent_note: None,
            labels: None,
            bare: false,
            pick: None,
            picker_open: false,
            picker_tab: Tab::Favourites,
            picker_query: String::new(),
            picker_cursor: None,
            picker_keyboard: false,
            picker_focus: cx.focus_handle(),
            refocus: false,
            context_card_open: false,
            plus_open: false,
            permission_open: false,
            effort_open: false,
            plus_bounds: Rc::default(),
            permission_bounds: Rc::default(),
            permission: None,
            plan_mode: false,
            goal: false,
            usage_card_open: false,
            picker_window_open: false,
            ring_bounds: Rc::default(),
            class_open: false,
            chip_bounds: Rc::default(),
            class_bounds: Rc::default(),
            picker_for_live: false,
            folder: None,
        };
        // A composer that goes away with a menu open takes its signal with it.
        let owner = cx.entity_id();
        cx.on_release(move |_, cx| overlay::release(cx, owner))
            .detach();
        composer.refresh_preview(cx);
        composer
    }

    /// Tells the app-wide overlay signal whether any of this composer's menus, pickers or cards
    /// is open. Called wherever one opens or closes, so the two always agree.
    pub(super) fn sync_overlay(&self, cx: &mut Context<Self>) {
        let open = self.picker_open
            || self.class_open
            || self.context_card_open
            || self.plus_open
            || self.permission_open
            || self.effort_open;
        let owner = cx.entity_id();
        overlay::set(cx, owner, open);
    }

    /// The composer is hidden (its screen is not showing): its menus, pickers and cards close and
    /// the signal is released, so nothing invisible holds an overlay open.
    pub(crate) fn hide(&mut self, cx: &mut Context<Self>) {
        self.class_open = false;
        self.context_card_open = false;
        self.close_menus();
        self.close_picker(cx);
        self.sync_overlay(cx);
        cx.notify();
    }

    /// A one-line field like the reason field, with its own placeholder and hint: Enter emits
    /// [`ReasonEvent::Submitted`], Escape clears it.
    pub(crate) fn field(
        store: Entity<Store>,
        placeholder: impl Into<SharedString>,
        hint: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut field = Self::with_mode(store, Mode::Reason, cx);
        field.labels = Some((placeholder.into(), hint.into()));
        field
    }

    /// A one-line field with no box or hint of its own: Enter emits [`ReasonEvent::Submitted`],
    /// Escape emits [`ReasonEvent::Cancelled`].
    pub(crate) fn line(
        store: Entity<Store>,
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut line = Self::field(store, placeholder, "", cx);
        line.bare = true;
        line
    }

    /// Empties the field.
    pub(crate) fn clear(&mut self, cx: &mut Context<Self>) {
        self.model.set_text("");
        cx.notify();
    }

    /// A composer that steers `run`.
    pub(crate) fn steer(store: Entity<Store>, run: RunId, cx: &mut Context<Self>) -> Self {
        Self::with_mode(store, Mode::Steer(run), cx)
    }

    pub(crate) fn class(&self) -> Class {
        self.class
    }

    #[cfg(test)]
    pub(crate) fn text(&self) -> &str {
        self.model.text()
    }

    /// The tab the picker views.
    #[cfg(test)]
    pub(crate) fn picker_tab(&self) -> &Tab {
        &self.picker_tab
    }

    /// The harness · model chosen for the next task; `None` is Auto.
    #[cfg(test)]
    pub(crate) fn pick(&self) -> Option<&models::Pick> {
        self.pick.as_ref()
    }

    /// A task's params with the chosen harness and model: a pick names the harness (so there is
    /// no fallback to another) and wins over a bot's own list.
    fn with_pick(&self, mut params: TaskRunParams) -> TaskRunParams {
        if let Some(pick) = &self.pick {
            params.executor = Some(pick.executor.clone());
            params.model.clone_from(&pick.model);
            params.effort = pick.effort;
            params.model_options.clone_from(&pick.options);
        }
        params
    }

    /// A new task's params: the class's bot, the chosen harness and model, and the folder the
    /// thread is for. The send and the route preview both ask with these, so they agree.
    fn home_params(
        &self,
        prompt: &str,
        attachments: &[reno_proto::Attachment],
        state: &reno_client::State,
    ) -> TaskRunParams {
        let mut params =
            self.with_pick(routing::task_params(prompt, self.class, attachments, state));
        params.cwd.clone_from(&self.folder);
        params
    }

    /// Home: the folder new threads work in. Asks for a fresh route preview for it.
    pub(crate) fn set_folder(&mut self, folder: Option<PathBuf>, cx: &mut Context<Self>) {
        if self.folder != folder {
            self.folder = folder;
            self.refresh_preview(cx);
            cx.notify();
        }
    }

    /// What is typed in a one-line field, trimmed; `None` when blank.
    pub(crate) fn typed_reason(&self) -> Option<String> {
        let reason = self.model.text().trim();
        (!reason.is_empty()).then(|| reason.to_owned())
    }

    #[cfg(test)]
    pub(crate) fn placeholder_text(&self, cx: &App) -> SharedString {
        self.placeholder(cx)
    }

    /// `RENO_DEMO_MENU`: opens a menu for a screenshot.
    pub(crate) fn open_demo_menu(&mut self, menu: &str, cx: &mut Context<Self>) {
        match menu {
            "plus" => self.plus_open = true,
            "permission" => self.permission_open = true,
            "effort" => self.effort_open = true,
            _ => return,
        }
        self.sync_overlay(cx);
        cx.notify();
    }

    /// Whether anything is typed (a draft the owner would lose sight of).
    pub(crate) fn has_text(&self) -> bool {
        !self.model.text().is_empty()
    }

    #[cfg(test)]
    pub(crate) fn preview(&self) -> &Preview {
        &self.preview
    }

    #[cfg(test)]
    pub(crate) fn files(&self) -> &attach::Draft {
        &self.files
    }

    #[cfg(test)]
    pub(crate) fn why_is_open(&self) -> bool {
        self.why_open
    }

    #[cfg(test)]
    pub(crate) fn is_sent_noted(&self) -> bool {
        self.sent_note.is_some()
    }

    /// Replaces the draft (the suggestion chips) and focuses the input.
    pub(crate) fn fill(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.model.set_text(text);
        window.focus(&self.focus_handle, cx);
        self.text_changed(window, cx);
    }

    pub(crate) fn set_class(&mut self, class: Class, cx: &mut Context<Self>) {
        if self.class != class {
            self.class = class;
            self.refresh_preview(cx);
            cx.notify();
        }
    }

    // ---- what the text area reads ----------------------------------------------------------

    pub(crate) fn text_for_layout(&self) -> &str {
        self.model.text()
    }

    pub(crate) fn marked_range(&self) -> Option<Range<usize>> {
        self.model.marked()
    }

    pub(crate) fn selection(&self) -> Range<usize> {
        self.model.selection()
    }

    pub(crate) fn caret(&self) -> usize {
        self.model.caret()
    }

    pub(crate) fn is_blank(&self) -> bool {
        self.model.is_empty()
    }

    pub(crate) fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    pub(crate) fn focus_handle_ref(&self) -> &FocusHandle {
        &self.focus_handle
    }

    pub(crate) fn scroll_offset(&self) -> Pixels {
        self.scroll
    }

    pub(crate) fn wants_caret_visible(&self) -> bool {
        self.reveal
    }

    /// The text area painted: keep what it measured.
    pub(crate) fn painted(&mut self, layout: TextLayout, scroll: Pixels) {
        self.layout = Some(layout);
        self.scroll = scroll;
        self.reveal = false;
    }

    // ---- state -----------------------------------------------------------------------------

    fn connected(&self, cx: &App) -> bool {
        self.store.read(cx).connection() == ConnectionStatus::Connected
    }

    fn steer_state(&self, cx: &App) -> Option<SteerState> {
        match &self.mode {
            Mode::Steer(run) => Some(steer_state(self.store.read(cx).state().run(run))),
            _ => None,
        }
    }

    /// Whether the box takes text: always; an ended run's box starts a new run.
    #[allow(clippy::unused_self)]
    fn input_enabled(&self, _: &App) -> bool {
        true
    }

    /// Whether what is typed can go: a steer needs a run that takes messages; an ended run's
    /// message starts a new run, so it can always go.
    fn target_live(&self, cx: &App) -> bool {
        !matches!(self.steer_state(cx), Some(SteerState::Waiting(_)))
    }

    /// The project name an ended run's follow-up goes to: its repository's folder name.
    fn project_name(&self, cx: &App) -> String {
        let Mode::Steer(run) = &self.mode else {
            return String::new();
        };
        let store = self.store.read(cx);
        store
            .state()
            .run(run)
            .and_then(|r| r.cwd.as_deref())
            .and_then(|cwd| store.root_of(cwd))
            .and_then(|root| root.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| "scratch".to_owned())
    }

    pub(crate) fn can_send(&self, cx: &App) -> bool {
        self.model.can_submit()
            && self.connected(cx)
            && self.target_live(cx)
            && !self.store.read(cx).is_sending()
    }

    /// Enter: send non-blank text to the daemon. A blank draft does nothing; with no daemon the
    /// draft is kept and a notice says why.
    pub(crate) fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Enter belongs to the IME while it is composing.
        if self.model.marked().is_some() {
            return;
        }
        if self.mode == Mode::Reason {
            // A blank reason is fine: the denial goes without one.
            let reason = self.model.text().trim().to_owned();
            cx.emit(ReasonEvent::Submitted(
                (!reason.is_empty()).then_some(reason),
            ));
            return;
        }
        if !self.model.can_submit() || !self.target_live(cx) || self.store.read(cx).is_sending() {
            return;
        }
        if !self.connected(cx) {
            toast::push(
                cx,
                toast::Tone::Error,
                "Daemon not running",
                "The task was not sent.",
            );
            return;
        }
        let draft = self.model.text().to_owned();
        let files = self.files.take();
        let wire = attach::wire(&files);
        let EnterOutcome::Submitted(prompt) = self.model.enter(false) else {
            self.files.restore(files);
            return;
        };
        self.next_ticket += 1;
        self.refusal = None;
        let ticket = self.next_ticket;
        self.sent = Some(Sent {
            ticket,
            text: draft,
            files,
        });
        match self.mode.clone() {
            Mode::Home => {
                let params = self.home_params(&prompt, &wire, self.store.read(cx).state());
                self.store
                    .update(cx, |store, cx| store.submit_task(params, ticket, cx));
            }
            Mode::Steer(run) => {
                let thread = self.continuing(cx);
                let ended = self
                    .store
                    .read(cx)
                    .state()
                    .run(&run)
                    .filter(|_| matches!(self.steer_state(cx), Some(SteerState::Ended(_))))
                    .map(|summary| self.with_pick(follow_up(summary, &prompt, thread)));
                match ended {
                    Some(params) => self
                        .store
                        .update(cx, |store, cx| store.submit_task(params, ticket, cx)),
                    None => {
                        self.store
                            .update(cx, |store, cx| store.steer(run, prompt, ticket, cx));
                        self.show_sent(cx);
                    }
                }
            }
            Mode::Reason => {}
        }
        self.scroll = px(0.0);
        self.text_changed(window, cx);
    }

    #[cfg(test)]
    pub(crate) fn refusal_note(&self) -> Option<threads::Refusal> {
        self.refusal
    }

    /// The note above the box, as the tests read it.
    #[cfg(test)]
    pub(crate) fn note_for_test(&self, cx: &App) -> String {
        match self.steer_state(cx) {
            Some(SteerState::Ended(why)) => ended_note(
                &why,
                &self.project_name(cx),
                self.store.read(cx).supports_threads(),
                self.thread_is_gone(cx),
            ),
            _ => String::new(),
        }
    }

    /// The thread a message to this ended run continues: the run's own, while it has a worktree
    /// to continue (or never had one) and the daemon has not said otherwise.
    fn continuing(&self, cx: &App) -> Option<reno_proto::ThreadId> {
        let Mode::Steer(run) = &self.mode else {
            return None;
        };
        if self.thread_gone {
            return None;
        }
        threads::continues(self.store.read(cx).state(), run)
    }

    /// Whether the daemon has threads and this run's is one that cannot be continued.
    fn thread_is_gone(&self, cx: &App) -> bool {
        let Mode::Steer(run) = &self.mode else {
            return false;
        };
        let state = self.store.read(cx).state();
        state.supports(reno_proto::FEATURE_THREADS)
            && (self.thread_gone
                || state
                    .thread_of(run)
                    .and_then(|t| state.thread(t))
                    .is_some_and(threads::worktree_gone))
    }

    /// The daemon refused a send in words: a thread that is busy or gone says so under the box.
    /// The draft is already back (`SendFailed` came first).
    fn refuse(&mut self, why: &str, cx: &mut Context<Self>) {
        let Some(refusal) = threads::refusal(why) else {
            return;
        };
        if refusal == threads::Refusal::Gone {
            self.thread_gone = true;
        }
        self.refusal = Some(refusal);
        cx.notify();
    }

    /// "Sent" on a steer box, for a moment.
    fn show_sent(&mut self, cx: &mut Context<Self>) {
        self.sent_note = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SENT_FOR).await;
            this.update(cx, |this, cx| {
                this.sent_note = None;
                cx.notify();
            })
            .ok();
        }));
    }

    /// The daemon refused what was sent: the draft comes back, in front of anything typed since.
    fn take_draft_back(&mut self, ticket: u64, cx: &mut Context<Self>) {
        if self.sent.as_ref().is_none_or(|s| s.ticket != ticket) {
            return;
        }
        let Some(sent) = self.sent.take() else {
            return;
        };
        self.sent_note = None;
        if self.model.is_empty() {
            self.model.set_text(&sent.text);
        } else {
            let later = self.model.text().to_owned();
            self.model.set_text(&format!("{}\n\n{later}", sent.text));
        }
        let mut files = sent.files;
        files.extend(self.files.take());
        self.files.restore(files);
        self.reveal = true;
        self.refresh_preview(cx);
        cx.notify();
    }

    // ---- harness · model --------------------------------------------------------------------

    /// The live run this box steers, while it can still take a message (not ended).
    fn live_run<'a>(&self, cx: &'a App) -> Option<&'a reno_proto::RunSummary> {
        let Mode::Steer(run) = &self.mode else {
            return None;
        };
        if matches!(self.steer_state(cx), Some(SteerState::Ended(_))) {
            return None;
        }
        self.store.read(cx).state().run(run)
    }

    /// The class button: the four classes behind one quiet button.
    fn class_button(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let bounds = self.class_bounds.clone();
        let (hover, text) = (theme.row_selected(), theme.text);
        div()
            .id("class-menu")
            .debug_selector(|| "class-menu".to_owned())
            .relative()
            .flex_none()
            .h(px(28.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .gap(px(4.0))
            .rounded(px(8.0))
            .text_size(px(12.5))
            .text_color(theme.muted)
            .cursor_pointer()
            .hover(move |s| s.bg(hover).text_color(text))
            .child(self.class.label())
            .child(icon(Icon::ChevDown, 12.0, theme.faint))
            .child(
                canvas(move |b, _, _| bounds.set(Some(b)), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .on_click(cx.listener(|this, _, _, cx| {
                this.class_open = !this.class_open;
                this.close_picker(cx);
                this.sync_overlay(cx);
                cx.notify();
            }))
    }

    fn class_menu(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let rows = Class::ALL.map(|class| {
            menu_row(theme, format!("class-{}", class.label()), true)
                .child(class.label())
                .when(class == self.class, |r| r.child(check(theme)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.class_open = false;
                    this.sync_overlay(cx);
                    this.set_class(class, cx);
                }))
        });
        menu(theme, "class-list", 168.0).children(rows)
    }

    // ---- attachments ----------------------------------------------------------------------

    /// Whether this composer sends files: only a new task does.
    fn takes_files(&self, cx: &App) -> bool {
        self.mode == Mode::Home
            && self
                .store
                .read(cx)
                .state()
                .supports(reno_proto::FEATURE_ATTACHMENTS)
    }

    /// Paste, drop and the picker all end here.
    pub(crate) fn add_attachments(&mut self, paths: &[PathBuf], cx: &mut Context<Self>) {
        if self.mode != Mode::Home {
            toast::push(
                cx,
                toast::Tone::NotYet,
                "Files",
                "Only a new task takes files. A steer is text.",
            );
            return;
        }
        if !self
            .store
            .read(cx)
            .state()
            .supports(reno_proto::FEATURE_ATTACHMENTS)
        {
            toast::push(
                cx,
                toast::Tone::NotYet,
                "Files",
                "This daemon is too old to take attachments. Update it to attach files.",
            );
            return;
        }
        let refused = self.files.add_paths(paths);
        if !refused.is_empty() {
            toast::push(
                cx,
                toast::Tone::Error,
                "Could not attach",
                refused.join(" "),
            );
        }
        cx.notify();
    }

    fn remove_attachment(&mut self, index: usize, cx: &mut Context<Self>) {
        self.files.remove(index);
        cx.notify();
    }

    /// The paperclip: the system file picker.
    fn pick_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.takes_files(cx) {
            self.add_attachments(&[], cx);
            return;
        }
        let picked = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Attach".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            if let Ok(Ok(Some(paths))) = picked.await {
                this.update(cx, |this, cx| this.add_attachments(&paths, cx))
                    .ok();
            }
        })
        .detach();
    }

    // ---- route preview ---------------------------------------------------------------------

    /// Asks for the route again if what it depends on changed, after the text has stood still.
    /// Home only. Not while the IME is composing; not on every keystroke (the request waits for
    /// [`PREVIEW_DEBOUNCE`] of quiet and a newer request replaces an older one).
    fn refresh_preview(&mut self, cx: &mut Context<Self>) {
        if self.mode != Mode::Home || self.model.marked().is_some() {
            return;
        }
        if !self.connected(cx) {
            self.debounce = None;
            self.request = None;
            self.preview_key = None;
            self.preview = if self.store.read(cx).connection() == ConnectionStatus::Connecting {
                Preview::Loading
            } else {
                Preview::Unavailable("the daemon is not running".to_owned())
            };
            return;
        }
        let key = {
            let state = self.store.read(cx).state();
            PreviewKey {
                task: self.home_params(self.model.text().trim(), &[], state),
                probes: state
                    .executors()
                    .iter()
                    .map(|e| {
                        (
                            e.descriptor.id.clone(),
                            e.health.as_ref().map(|h| h.checked_at_ms),
                        )
                    })
                    .collect(),
            }
        };
        if self.preview_key.as_ref() == Some(&key) {
            return;
        }
        let first = self.preview_key.is_none();
        self.preview_key = Some(key.clone());
        if matches!(self.preview, Preview::Idle | Preview::Unavailable(_)) {
            self.preview = Preview::Loading;
        }
        self.debounce = Some(cx.spawn(async move |this, cx| {
            if !first {
                cx.background_executor().timer(PREVIEW_DEBOUNCE).await;
            }
            this.update(cx, |this, cx| this.fetch_preview(key, cx)).ok();
        }));
    }

    fn fetch_preview(&mut self, key: PreviewKey, cx: &mut Context<Self>) {
        self.generation += 1;
        let generation = self.generation;
        let call = self.store.read(cx).preview_route(key.task, cx);
        // Replacing the task drops the one before it, which cancels its request.
        self.request = Some(cx.spawn(async move |this, cx| {
            let answer = call.await;
            this.update(cx, |this, cx| this.finish_preview(generation, answer, cx))
                .ok();
        }));
    }

    fn finish_preview(
        &mut self,
        generation: u64,
        answer: Result<RoutingDecision, String>,
        cx: &mut Context<Self>,
    ) {
        if generation != self.generation {
            return;
        }
        self.preview = match answer {
            Ok(decision) => Preview::Ready(decision),
            Err(reason) => Preview::Unavailable(reason),
        };
        cx.notify();
    }

    // ---- edits and moves ---------------------------------------------------------------------

    fn selection_changed(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.reveal = true;
        window.notify_text_selection_changed(&self.focus_handle);
        cx.notify();
    }

    fn text_changed(&mut self, window: &Window, cx: &mut Context<Self>) {
        self.goal_x = None;
        self.reveal = true;
        window.notify_text_input_changed(&self.focus_handle);
        self.refresh_preview(cx);
        cx.notify();
    }

    fn on_submit(&mut self, _: &Submit, window: &mut Window, cx: &mut Context<Self>) {
        self.submit(window, cx);
    }

    fn on_cancel(&mut self, _: &Cancel, _: &mut Window, cx: &mut Context<Self>) {
        if self.picker_open || self.class_open {
            self.class_open = false;
            self.context_card_open = false;
            self.close_picker(cx);
            self.sync_overlay(cx);
            cx.notify();
        } else if self.mode == Mode::Reason {
            self.model.set_text("");
            cx.emit(ReasonEvent::Cancelled);
            cx.notify();
        } else {
            // Escape is somebody else's outside a reason input.
            cx.propagate();
        }
    }

    fn on_newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.marked().is_none() && self.input_enabled(cx) {
            self.model.enter(true);
            self.text_changed(window, cx);
        }
    }

    /// An edit that may do nothing: rings the bell when it did not, and does nothing at all in
    /// a box that is disabled.
    fn edit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        change: impl FnOnce(&mut TextModel) -> bool,
    ) {
        if !self.input_enabled(cx) {
            return;
        }
        if change(&mut self.model) {
            self.text_changed(window, cx);
        } else {
            window.play_system_bell();
        }
    }

    fn on_backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        self.edit(window, cx, TextModel::backspace);
    }

    fn on_delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        self.edit(window, cx, TextModel::delete);
    }

    fn on_delete_word_back(
        &mut self,
        _: &DeleteWordBack,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.edit(window, cx, TextModel::delete_word_back);
    }

    fn on_delete_word_forward(
        &mut self,
        _: &DeleteWordForward,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.edit(window, cx, TextModel::delete_word_forward);
    }

    fn on_delete_line_back(
        &mut self,
        _: &DeleteLineBack,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.edit(window, cx, TextModel::delete_line_back);
    }

    fn on_undo(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        self.edit(window, cx, TextModel::undo);
    }

    fn on_redo(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        self.edit(window, cx, TextModel::redo);
    }

    fn step(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        mv: impl FnOnce(&mut TextModel),
    ) {
        mv(&mut self.model);
        self.selection_changed(window, cx);
    }

    fn on_left(&mut self, _: &Left, window: &mut Window, cx: &mut Context<Self>) {
        self.step(window, cx, |m| m.move_left(false));
    }

    fn on_right(&mut self, _: &Right, window: &mut Window, cx: &mut Context<Self>) {
        self.step(window, cx, |m| m.move_right(false));
    }

    fn on_select_left(&mut self, _: &SelectLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.step(window, cx, |m| m.move_left(true));
    }

    fn on_select_right(&mut self, _: &SelectRight, window: &mut Window, cx: &mut Context<Self>) {
        self.step(window, cx, |m| m.move_right(true));
    }

    fn on_word_left(&mut self, _: &WordLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.step(window, cx, |m| m.move_word_left(false));
    }

    fn on_word_right(&mut self, _: &WordRight, window: &mut Window, cx: &mut Context<Self>) {
        self.step(window, cx, |m| m.move_word_right(false));
    }

    fn on_select_word_left(
        &mut self,
        _: &SelectWordLeft,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step(window, cx, |m| m.move_word_left(true));
    }

    fn on_select_word_right(
        &mut self,
        _: &SelectWordRight,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step(window, cx, |m| m.move_word_right(true));
    }

    fn on_doc_start(&mut self, _: &DocStart, window: &mut Window, cx: &mut Context<Self>) {
        self.step(window, cx, |m| m.move_doc_start(false));
    }

    fn on_doc_end(&mut self, _: &DocEnd, window: &mut Window, cx: &mut Context<Self>) {
        self.step(window, cx, |m| m.move_doc_end(false));
    }

    fn on_select_doc_start(
        &mut self,
        _: &SelectDocStart,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.step(window, cx, |m| m.move_doc_start(true));
    }

    fn on_select_doc_end(&mut self, _: &SelectDocEnd, window: &mut Window, cx: &mut Context<Self>) {
        self.step(window, cx, |m| m.move_doc_end(true));
    }

    fn on_select_all(&mut self, _: &SelectAll, window: &mut Window, cx: &mut Context<Self>) {
        self.step(window, cx, TextModel::select_all);
    }

    fn on_home(&mut self, _: &Home, window: &mut Window, cx: &mut Context<Self>) {
        self.step(window, cx, |m| m.move_home(false));
    }

    fn on_end(&mut self, _: &End, window: &mut Window, cx: &mut Context<Self>) {
        self.step(window, cx, |m| m.move_end(false));
    }

    fn on_select_home(&mut self, _: &SelectHome, window: &mut Window, cx: &mut Context<Self>) {
        self.step(window, cx, |m| m.move_home(true));
    }

    fn on_select_end(&mut self, _: &SelectEnd, window: &mut Window, cx: &mut Context<Self>) {
        self.step(window, cx, |m| m.move_end(true));
    }

    /// Up and Down move by visual row, keeping the x they started at, once the text has been
    /// painted; past the first or last row they go to the start or the end of the text.
    fn vertical(&mut self, rows: i32, extend: bool, window: &mut Window, cx: &mut Context<Self>) {
        let goal = self.goal_x;
        let target = self
            .layout
            .as_ref()
            .filter(|l| l.is_for(self.model.text()) && self.model.marked().is_none())
            .and_then(|l| l.index_for_row_move(self.model.caret(), rows, goal));
        match target {
            Some((index, x)) => {
                self.model.move_to(index, extend);
                self.selection_changed(window, cx);
                self.goal_x = Some(x);
            }
            None => {
                if rows < 0 {
                    self.model.move_up(extend);
                } else {
                    self.model.move_down(extend);
                }
                self.selection_changed(window, cx);
            }
        }
    }

    fn on_up(&mut self, _: &Up, window: &mut Window, cx: &mut Context<Self>) {
        self.vertical(-1, false, window, cx);
    }

    fn on_down(&mut self, _: &Down, window: &mut Window, cx: &mut Context<Self>) {
        self.vertical(1, false, window, cx);
    }

    fn on_select_up(&mut self, _: &SelectUp, window: &mut Window, cx: &mut Context<Self>) {
        self.vertical(-1, true, window, cx);
    }

    fn on_select_down(&mut self, _: &SelectDown, window: &mut Window, cx: &mut Context<Self>) {
        self.vertical(1, true, window, cx);
    }

    /// Paste: files (a Finder copy) attach, text inserts, and an image with no text (a
    /// screenshot) is saved and attached.
    fn on_paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if !self.input_enabled(cx) {
            return;
        }
        let Some(item) = cx.read_from_clipboard() else {
            return;
        };
        // Files come before text: a Finder copy carries the file's name as text too. A steer
        // does not take files, but `add_attachments` is the one that says so.
        if let Some(paths) = pasted_paths(&item) {
            self.add_attachments(&paths, cx);
            return;
        }
        if let Some(text) = item.text() {
            self.model.insert(&text);
            self.text_changed(window, cx);
        } else if self.takes_files(cx)
            && let Some(image) = pasted_image(&item)
        {
            match attach::save_image(&attach::paste_dir(), image.0, image.1) {
                Ok(path) => self.add_attachments(&[path], cx),
                Err(e) => toast::push(
                    cx,
                    toast::Tone::Error,
                    "Could not attach",
                    format!("The pasted image could not be saved: {e}."),
                ),
            }
        }
    }

    fn on_copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(selected) = self.model.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(selected.to_owned()));
        }
    }

    fn on_cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if !self.input_enabled(cx) {
            return;
        }
        if let Some(selected) = self.model.cut() {
            cx.write_to_clipboard(ClipboardItem::new_string(selected));
            self.text_changed(window, cx);
        }
    }

    fn on_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        window.request_virtual_keyboard();
        let index = self.index_for_position(event.position);
        self.selecting = false;
        match event.click_count {
            0 | 1 => {
                self.selecting = true;
                self.model.move_to(index, event.modifiers.shift);
            }
            2 => self.model.select_word_at(index),
            _ => self.model.select_line_at(index),
        }
        self.selection_changed(window, cx);
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selecting {
            let index = self.index_for_position(event.position);
            self.model.move_to(index, true);
            self.selection_changed(window, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }

    /// The wheel scrolls a draft taller than the box.
    fn on_scroll(&mut self, event: &ScrollEvent, _: &mut Window, cx: &mut Context<Self>) {
        let Some(layout) = &self.layout else {
            return;
        };
        let max = layout.max_scroll();
        if max <= px(0.0) {
            return;
        }
        let delta = event.delta.pixel_delta(layout.line_height()).y;
        self.scroll = (self.scroll - delta).clamp(px(0.0), max);
        cx.notify();
        cx.stop_propagation();
    }

    /// The byte offset under a window position, from the last painted layout.
    fn index_for_position(&self, position: Point<Pixels>) -> usize {
        match &self.layout {
            Some(layout) if layout.is_for(self.model.text()) => layout.index_for_position(position),
            _ => self.model.text().len(),
        }
    }
}

/// The files a clipboard item carries (a copy in Finder), if any.
fn pasted_paths(item: &ClipboardItem) -> Option<Vec<PathBuf>> {
    let paths: Vec<PathBuf> = item
        .entries()
        .iter()
        .filter_map(|e| match e {
            ClipboardEntry::ExternalPaths(p) => Some(p.paths().to_vec()),
            _ => None,
        })
        .flatten()
        .collect();
    (!paths.is_empty()).then_some(paths)
}

/// The image a clipboard item carries (a screenshot): its file extension and bytes.
fn pasted_image(item: &ClipboardItem) -> Option<(&'static str, &[u8])> {
    item.entries().iter().find_map(|e| match e {
        ClipboardEntry::Image(image) => Some((image.format.extension(), image.bytes.as_slice())),
        _ => None,
    })
}

impl EntityInputHandler for Composer {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.model.range_from_utf16(&range_utf16);
        actual_range.replace(self.model.range_to_utf16(&range));
        Some(self.model.text()[range].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.model.range_to_utf16(&self.model.selection()),
            reversed: self.model.reversed(),
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.model
            .marked()
            .map(|range| self.model.range_to_utf16(&range))
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model.unmark();
        self.text_changed(window, cx);
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.input_enabled(cx) {
            return;
        }
        self.model.replace_utf16(range_utf16.as_ref(), text);
        self.text_changed(window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.input_enabled(cx) {
            return;
        }
        self.model.replace_and_mark_utf16(
            range_utf16.as_ref(),
            new_text,
            new_selected_range_utf16.as_ref(),
        );
        self.text_changed(window, cx);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.layout.as_ref()?;
        if !layout.is_for(self.model.text()) {
            return None;
        }
        let range = self.model.range_from_utf16(&range_utf16);
        let start = layout.window_point_for_index(range.start)?;
        let end = layout.window_point_for_index(range.end)?;
        Some(Bounds::from_corners(
            start,
            gpui::point(end.x.max(start.x), end.y + layout.line_height()),
        ))
    }

    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        if !layout.is_for(self.model.text()) {
            return None;
        }
        Some(
            self.model
                .offset_to_utf16(layout.index_for_position(position)),
        )
    }
}

impl Focusable for Composer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

// ---- drawing ------------------------------------------------------------------------------

impl Composer {
    fn placeholder(&self, cx: &App) -> SharedString {
        match &self.mode {
            Mode::Home => "What should Reno orchestrate?".into(),
            Mode::Reason => self.labels.as_ref().map_or_else(
                || "Or tell it what to do instead \u{2014} Deny sends your words".into(),
                |(placeholder, _)| placeholder.clone(),
            ),
            Mode::Steer(run) => {
                let store = self.store.read(cx);
                let state = store.state();
                let who = state.run(run).map_or_else(
                    || "the run".to_owned(),
                    |r| summary::executor_name(r.executor.as_str()),
                );
                match self.steer_state(cx) {
                    Some(SteerState::Waiting(_)) => {
                        format!("Work with {who} when it is running").into()
                    }
                    _ => format!("Work with {who}").into(),
                }
            }
        }
    }

    /// The chips of the attached files.
    fn attachment_row(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<gpui::Div> {
        if self.files.is_empty() {
            return None;
        }
        let chips = self.files.items().iter().enumerate().map(|(i, item)| {
            div()
                .id(SharedString::from(format!("attachment-{i}")))
                .debug_selector(move || format!("attachment-{i}"))
                .h(px(28.0))
                .max_w(px(260.0))
                .pl(px(10.0))
                .pr(px(4.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .rounded(px(999.0))
                .bg(theme.raised)
                .border_1()
                .border_color(theme.hairline)
                .text_size(px(12.5))
                .child(icon(Icon::File, 13.0, theme.muted))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from(item.name.clone())),
                )
                .child(
                    div()
                        .flex_none()
                        .text_color(theme.faint)
                        .child(SharedString::from(item.size_label())),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("remove-attachment-{i}")))
                        .debug_selector(move || format!("remove-attachment-{i}"))
                        .flex_none()
                        .size(px(20.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(10.0))
                        .cursor_pointer()
                        .hover({
                            let hover = theme.surface;
                            move |s| s.bg(hover)
                        })
                        .on_click(cx.listener(move |this, _, _, cx| this.remove_attachment(i, cx)))
                        .child(icon(Icon::X, 11.0, theme.muted)),
                )
        });
        Some(
            div()
                .flex()
                .flex_wrap()
                .gap(px(6.0))
                .px(px(18.0))
                .children(chips),
        )
    }

    /// The "why?" list under the foot: the rule, the order, and each executor passed over.
    fn why_panel(&self, theme: &Theme, connected: bool) -> gpui::Div {
        let panel = div()
            .debug_selector(|| "why-panel".to_owned())
            .px(px(22.0))
            .pb(px(14.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .text_size(px(12.5))
            .text_color(theme.muted);
        match &self.preview {
            Preview::Idle | Preview::Loading => {
                panel.child("Asking the daemon where this would go\u{2026}")
            }
            Preview::Unavailable(reason) => panel.child(SharedString::from(
                preview_unavailable_text(reason, connected),
            )),
            Preview::Ready(decision) => {
                let view = routing::view(decision);
                let label = |text: &'static str| {
                    div()
                        .flex_none()
                        .w(px(84.0))
                        .text_color(theme.faint)
                        .child(text)
                };
                let mut panel = panel.child(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .child(label("Rule"))
                        .child(SharedString::from(view.rule.clone())),
                );
                if view.order.len() > 1 {
                    let order = view
                        .order
                        .iter()
                        .map(|n| n.name.clone())
                        .collect::<Vec<_>>()
                        .join(" \u{2192} ");
                    panel = panel.child(
                        div()
                            .flex()
                            .gap(px(8.0))
                            .child(label("Order"))
                            .child(SharedString::from(order)),
                    );
                }
                for (i, rejected) in view.rejected.iter().enumerate() {
                    panel = panel.child(
                        div()
                            .id(SharedString::from(format!("rejected-{i}")))
                            .debug_selector(move || format!("rejected-{i}"))
                            .flex()
                            .gap(px(8.0))
                            .child(label(if i == 0 { "Passed over" } else { "" }))
                            .child(
                                div()
                                    .flex()
                                    .flex_wrap()
                                    .gap(px(6.0))
                                    .child(
                                        div()
                                            .font_weight(FontWeight::MEDIUM)
                                            .text_color(theme.text)
                                            .child(SharedString::from(rejected.who.name.clone())),
                                    )
                                    .when_some(rejected.who.secondary(), |d, id| {
                                        d.child(
                                            div()
                                                .text_color(theme.faint)
                                                .child(SharedString::from(id.to_owned())),
                                        )
                                    })
                                    .child(SharedString::from(rejected.reason.clone())),
                            ),
                    );
                }
                if view.chosen.is_none() {
                    panel = panel.child(
                        div()
                            .text_color(theme.warning)
                            .child("Nothing can take this task right now. Fix an executor in Settings › Executors."),
                    );
                }
                panel
            }
        }
    }

    /// Whether this box's run can be stopped: anything not finished, a run waiting on you
    /// included.
    fn stoppable(&self, cx: &App) -> bool {
        let Mode::Steer(run) = &self.mode else {
            return false;
        };
        self.store
            .read(cx)
            .state()
            .run(run)
            .is_some_and(|r| !r.status.is_terminal() && !summary::stopped_by_person(r))
    }

    /// The round button at the foot's right: Stop (a square) while the run is live and the box is
    /// empty, else Send (an arrow, dim until there is text).
    fn action_button(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        if dock_action(self.stoppable(cx), self.model.can_submit()) == DockAction::Stop
            && let Mode::Steer(run) = &self.mode
        {
            let run = run.clone();
            return kit::stop_button(theme)
                .debug_selector(|| "composer-stop".to_owned())
                .on_click(cx.listener(move |this, _, _, cx| {
                    let run = run.clone();
                    this.store.update(cx, |store, cx| store.cancel_run(run, cx));
                }));
        }
        let can_send = self.can_send(cx);
        kit::send_button(theme, can_send)
            .debug_selector(|| "send".to_owned())
            .when(can_send, |s| {
                s.on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
            })
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        if std::mem::take(&mut self.refocus) {
            window.focus(&self.focus_handle, cx);
        }
        let focused = self.focus_handle.is_focused(window);
        let entity = cx.entity();
        let reason = self.mode == Mode::Reason;
        let bare = self.bare;
        let enabled = self.input_enabled(cx);
        let connected = self.connected(cx);
        let sending = self.store.read(cx).is_sending();

        let min_rows = if self.mode == Mode::Home {
            HOME_ROWS
        } else {
            1
        };
        let text_area = div()
            .id("composer-text")
            .w_full()
            .px(px(if reason { 14.0 } else { 22.0 }))
            .pt(px(if reason { 9.0 } else { 18.0 }))
            .cursor(if enabled {
                CursorStyle::IBeam
            } else {
                CursorStyle::Arrow
            })
            .font_weight(if reason {
                FontWeight::NORMAL
            } else {
                FontWeight::MEDIUM
            })
            .text_size(px(if reason { 13.5 } else { 15.0 }))
            .line_height(px(if reason { 20.0 } else { 23.0 }))
            .text_color(theme.text)
            .when(enabled, |d| {
                d.on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
                    .on_mouse_move(cx.listener(Self::on_mouse_move))
            })
            .on_scroll(cx.listener(Self::on_scroll))
            .child(TextArea {
                composer: entity,
                theme,
                placeholder: self.placeholder(cx),
                min_rows,
                disabled: !enabled,
            });

        let foot = match &self.mode {
            Mode::Home => div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .px(px(14.0))
                .pb(px(12.0))
                .children(self.plus_button(&theme, cx))
                .children(self.permission_chip(&theme, cx))
                .child(self.class_button(&theme, cx))
                .child(div().flex_1())
                .child(self.model_chip(&theme, cx))
                .child(div().w(px(4.0)))
                .child(self.action_button(&theme, cx)),
            Mode::Reason => div()
                .px(px(14.0))
                .pb(px(8.0))
                .text_size(px(12.0))
                .text_color(theme.faint)
                .child(self.labels.as_ref().map_or_else(
                    || "Enter to deny \u{b7} Esc to clear".into(),
                    |(_, hint)| hint.clone(),
                )),
            Mode::Steer(_) => div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .px(px(14.0))
                .pb(px(12.0))
                .children(self.plus_button(&theme, cx))
                .children(self.permission_chip(&theme, cx))
                .child(div().flex_1())
                .child(self.model_chip(&theme, cx))
                .children(self.context_ring(&theme, cx))
                .child(div().w(px(4.0)))
                .child(self.action_button(&theme, cx)),
        };
        // A run's note sits on one line above the box, so the foot never truncates.
        let note: Option<(SharedString, gpui::Hsla)> = match &self.mode {
            Mode::Steer(_) => match &self.steer_state(cx) {
                _ if self.refusal.is_some() => self
                    .refusal
                    .map(|refusal| (refusal.note().into(), theme.warning)),
                Some(SteerState::Ended(why)) => Some((
                    ended_note(
                        why,
                        &self.project_name(cx),
                        self.store.read(cx).supports_threads(),
                        self.thread_is_gone(cx),
                    )
                    .into(),
                    theme.muted,
                )),
                Some(SteerState::Waiting(why)) => Some((why.clone().into(), theme.muted)),
                _ if self.sent_note.is_some() => {
                    Some(("Sent \u{b7} delivered at the next step".into(), theme.faint))
                }
                _ if sending => Some(("Sending\u{2026}".into(), theme.faint)),
                _ => None,
            },
            _ => None,
        };
        let note = note.map(|(text, tone)| {
            div()
                .id("composer-note")
                .debug_selector(|| "composer-note".to_owned())
                .px(px(8.0))
                .pb(px(6.0))
                .truncate()
                .text_size(px(12.5))
                .text_color(tone)
                .child(text)
        });
        let picker = (self.picker_open && self.chip_opens(cx))
            .then(|| {
                let content = self.picker(&theme, cx);
                let at = self.chip_bounds.clone();
                self.popover(&at, content, Self::close_picker, None, cx)
            })
            .flatten();
        let small_menus = [
            (
                self.plus_open && !self.plus_items(cx).is_empty(),
                &self.plus_bounds,
                0,
            ),
            (
                self.permission_open && self.store.read(cx).capabilities().permission_modes,
                &self.permission_bounds,
                1,
            ),
        ]
        .into_iter()
        .filter(|(open, _, _)| *open)
        .filter_map(|(_, at, which)| {
            let content = if which == 0 {
                self.plus_menu(&theme, cx)
            } else {
                self.permission_menu(&theme, cx)
            };
            self.popover(
                at,
                content,
                |this, cx| {
                    this.close_menus();
                    this.sync_overlay(cx);
                    cx.notify();
                },
                None,
                cx,
            )
        })
        .collect::<Vec<_>>();
        let effort = self
            .effort_open
            .then(|| {
                let content = self.effort_popover(&theme, cx);
                let at = self.chip_bounds.clone();
                self.popover(
                    &at,
                    content,
                    |this, cx| {
                        this.effort_open = false;
                        this.sync_overlay(cx);
                        cx.notify();
                    },
                    None,
                    cx,
                )
            })
            .flatten();
        let cards = (self.context_card_open && self.mode != Mode::Home)
            .then(|| self.context_card(&theme, cx))
            .flatten();
        let classes = (self.class_open && self.mode == Mode::Home)
            .then(|| {
                let content = self.class_menu(&theme, cx);
                let at = self.class_bounds.clone();
                self.popover(
                    &at,
                    content,
                    |this, cx| {
                        this.class_open = false;
                        this.sync_overlay(cx);
                        cx.notify();
                    },
                    None,
                    cx,
                )
            })
            .flatten();

        let home = self.mode == Mode::Home;
        let boxed = div()
            .id("composer")
            .debug_selector(|| "composer".to_owned())
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_submit))
            .on_action(cx.listener(Self::on_cancel))
            .on_action(cx.listener(Self::on_newline))
            .on_action(cx.listener(Self::on_backspace))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_delete_word_back))
            .on_action(cx.listener(Self::on_delete_word_forward))
            .on_action(cx.listener(Self::on_delete_line_back))
            .on_action(cx.listener(Self::on_undo))
            .on_action(cx.listener(Self::on_redo))
            .on_action(cx.listener(Self::on_left))
            .on_action(cx.listener(Self::on_right))
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_select_left))
            .on_action(cx.listener(Self::on_select_right))
            .on_action(cx.listener(Self::on_select_up))
            .on_action(cx.listener(Self::on_select_down))
            .on_action(cx.listener(Self::on_word_left))
            .on_action(cx.listener(Self::on_word_right))
            .on_action(cx.listener(Self::on_select_word_left))
            .on_action(cx.listener(Self::on_select_word_right))
            .on_action(cx.listener(Self::on_doc_start))
            .on_action(cx.listener(Self::on_doc_end))
            .on_action(cx.listener(Self::on_select_doc_start))
            .on_action(cx.listener(Self::on_select_doc_end))
            .on_action(cx.listener(Self::on_select_all))
            .on_action(cx.listener(Self::on_home))
            .on_action(cx.listener(Self::on_end))
            .on_action(cx.listener(Self::on_select_home))
            .on_action(cx.listener(Self::on_select_end))
            .on_action(cx.listener(Self::on_paste))
            .on_action(cx.listener(Self::on_copy))
            .on_action(cx.listener(Self::on_cut))
            .on_action(cx.listener(Self::on_character_palette))
            // A drop reaches `add_attachments` in every mode, which says why a steer takes none.
            .on_drop(cx.listener(|this, paths: &ExternalPaths, _, cx| {
                this.add_attachments(paths.paths(), cx);
            }))
            .when(home, |d| {
                let accent = theme.accent;
                d.drag_over::<ExternalPaths>(move |style, _, _, _| {
                    style.border_color(accent.opacity(0.7))
                })
            })
            .w_full()
            .flex()
            .flex_col()
            .gap(px(if reason { 4.0 } else { 10.0 }))
            .rounded(px(if reason { 12.0 } else { 26.0 }))
            .when(!bare, |d| d.bg(theme.surface))
            .border_1()
            .border_color(if bare {
                gpui::transparent_black()
            } else if focused {
                theme.text.opacity(0.2)
            } else {
                theme.hairline
            })
            .opacity(if enabled { 1.0 } else { 0.72 })
            .child(text_area)
            .children(self.attachment_row(&theme, cx))
            .when(!bare, |d| d.child(foot))
            .when(home && self.why_open, |c| {
                c.child(self.why_panel(&theme, connected))
            })
            .children(classes);
        // The picker is a sibling of the box, not inside it: it has its own keyboard (a search
        // field), which the box's key bindings (Backspace, Enter, Escape) must not see.
        div()
            .w_full()
            .flex()
            .flex_col()
            .children(note)
            .child(boxed)
            .children(picker)
            .children(effort)
            .children(small_menus)
            .children(cards)
    }
}

/// The "why?" panel's line when no route preview came back. Only a connected daemon routes the
/// task on send; with none running, a send is refused.
fn preview_unavailable_text(reason: &str, connected: bool) -> String {
    if connected {
        format!(
            "The route could not be previewed: {reason}. The daemon still routes the task when you send it."
        )
    } else {
        format!("The route could not be previewed: {reason}.")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_round_button_is_stop_while_live_and_empty_and_send_with_text() {
        use super::{DockAction, dock_action};
        assert_eq!(dock_action(true, false), DockAction::Stop);
        assert_eq!(dock_action(true, true), DockAction::Send);
        assert_eq!(dock_action(false, false), DockAction::Send, "an ended run");
        assert_eq!(dock_action(false, true), DockAction::Send);
    }

    use super::*;

    #[test]
    fn the_unavailable_route_promises_a_send_only_while_connected() {
        let offline = preview_unavailable_text("the daemon is not running", false);
        assert!(
            offline.starts_with("The route could not be previewed"),
            "{offline}"
        );
        assert!(!offline.contains("still routes"), "{offline}");
        let online = preview_unavailable_text("boom", true);
        assert!(online.contains("The daemon still routes the task when you send it."));
    }

    #[test]
    fn a_passed_run_that_reported_a_problem_is_not_called_passed() {
        use reno_proto::{ReportStatus, RunReport};
        let reported = |status| {
            let mut r = run_in(RunStatus::Succeeded, None);
            r.report = Some(RunReport {
                status,
                summary: "x".into(),
            });
            steer_state(Some(&r))
        };
        assert_eq!(
            reported(ReportStatus::Failed),
            SteerState::Ended("This run reported a failure.".into())
        );
        assert_eq!(
            reported(ReportStatus::Blocked),
            SteerState::Ended("This run reported it is blocked.".into())
        );
        assert_eq!(
            reported(ReportStatus::Done),
            SteerState::Ended("This run passed.".into())
        );
    }

    fn run_in(status: RunStatus, stop: Option<reno_proto::StopDetail>) -> reno_proto::RunSummary {
        reno_proto::RunSummary {
            model_options: Default::default(),
            effort: None,
            run_id: reno_proto::RunId::from_suffix("a").unwrap(),
            task_id: reno_proto::TaskId::from_suffix("a").unwrap(),
            executor: "claude-code".parse().unwrap(),
            status,
            attempt: 1,
            title: "t".into(),
            prompt: None,
            model: None,
            stop_reason: None,
            bot: None,
            cwd: None,
            created_at_ms: 0,
            updated_at_ms: 0,
            stop,
            report: None,
            not_resumed: None,
            session: None,
        }
    }

    #[test]
    fn a_persons_stop_reads_as_stopped_and_other_interruptions_stay_resumable() {
        let by_user = reno_proto::StopDetail::Cancelled {
            by: reno_proto::CancelledBy::User,
        };
        let SteerState::Ended(why) =
            steer_state(Some(&run_in(RunStatus::Interrupted, Some(by_user))))
        else {
            panic!("a person's stop is over");
        };
        assert_eq!(why, "This run was stopped.");
        for stop in [
            reno_proto::StopDetail::Shutdown,
            reno_proto::StopDetail::Crashed,
        ] {
            let SteerState::Waiting(why) =
                steer_state(Some(&run_in(RunStatus::Interrupted, Some(stop))))
            else {
                panic!("a resumable interruption waits");
            };
            assert!(why.contains("once it resumes"), "{why}");
        }
    }

    #[test]
    fn a_live_run_takes_a_steer_a_finished_one_says_why_it_does_not() {
        assert_eq!(
            steer_state(Some(&run_in(RunStatus::Running, None))),
            SteerState::Steering
        );
        assert_eq!(
            steer_state(Some(&run_in(RunStatus::WaitingApproval, None))),
            SteerState::Steering
        );
        assert_eq!(
            steer_state(Some(&run_in(RunStatus::Queued, None))),
            SteerState::Steering
        );
        assert!(matches!(
            steer_state(Some(&run_in(RunStatus::Interrupted, None))),
            SteerState::Waiting(_)
        ));
        assert!(matches!(steer_state(None), SteerState::Waiting(_)));
        let SteerState::Ended(why) = steer_state(Some(&run_in(RunStatus::Succeeded, None))) else {
            panic!("a passed run is over");
        };
        assert!(why.starts_with("This run passed."), "{why}");
        let SteerState::Ended(why) = steer_state(Some(&run_in(RunStatus::Cancelled, None))) else {
            panic!("a stopped run is over");
        };
        assert!(why.starts_with("This run was stopped."), "{why}");
    }
}
