//! The draft's interaction state, owned by the root view: which menu, card, confirm or rename is
//! open, the hover that opens a card, the filters, the width. Every click in the sidebar is an
//! [`Act`]; [`State::apply`] is the one place they land, so a test can drive them without a mouse.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    App, AppContext as _, Bounds, ClipboardItem, Context, Entity, Focusable, Pixels, Point,
    Subscription, Window, point, px,
};
use reno_client::thread_desk::ThreadEditError;
use reno_proto::{RunId, ThreadId};

use super::super::field::{Field, FieldEvent, Look};
use super::super::toast::{self, Tone};
use crate::app::RootView;
use crate::store::Store;
use crate::vm::draft32::{Facts, Item, Variant};
use crate::vm::projects::FolderKey;

/// The sidebar's width when nothing was chosen, and the least and most it can be dragged to.
pub(crate) const DEFAULT_WIDTH: f32 = crate::ui::sidebar::WIDTH;
pub(crate) const MIN_WIDTH: f32 = 200.0;
pub(crate) const MAX_WIDTH: f32 = 380.0;
/// How long the pointer rests on a row before the details card opens, and how long it waits to
/// change to another row's once it is open.
const REST_MS: u64 = 400;
const SWITCH_MS: u64 = 150;
/// How long the card stays after the pointer leaves the row and the card.
const LEAVE_MS: u64 = 300;

/// What a click, key or timer in the sidebar asks for.
#[derive(Clone, Debug)]
pub(crate) enum Act {
    /// The pointer entered or left a thread's row.
    Hover(ThreadId, bool),
    /// The pointer entered or left the details card.
    CardHover(bool),
    /// Open the details card for a thread now (a right click).
    Card(ThreadId),
    /// Open the row's menu: at the pointer, or (`None`) under the row.
    Menu(ThreadId, Option<Point<Pixels>>),
    /// The menu's Copy sub-list, and back from it.
    CopyList(bool),
    /// An entry of a menu, or a card button, on a thread.
    Do(ThreadId, Item),
    Unarchive(ThreadId),
    RenameSubmit,
    ConfirmClose,
    /// Esc, or a click outside: closes what is open (a rename first).
    Dismiss,
    /// The rename field lost the focus to something else.
    Blur,
    /// "Show 6 more" / "Show less" on a project.
    ToggleMore(FolderKey),
    /// A's "Archived \u{b7} 4" row.
    ToggleArchived,
    /// B's "Threads \u{25be}" header menu, and its two choices.
    FilterMenu(Option<Point<Pixels>>),
    Filter(bool),
    /// C's archived page.
    Page(bool),
    /// Scroll the sidebar to its end (the `archived` and `more` hooks).
    ScrollEnd,
    /// The sidebar's edge was dragged to this width, and let go.
    Resize(f32),
    ResizeEnd,
}

struct Rename {
    thread: ThreadId,
    field: Entity<Field>,
    /// The field is in C's card, not in the row.
    in_card: bool,
    _subs: [Subscription; 2],
}

pub(crate) struct MenuState {
    pub(crate) thread: ThreadId,
    pub(crate) at: Point<Pixels>,
    pub(crate) copy: bool,
}

pub(crate) struct Confirm {
    pub(crate) thread: ThreadId,
    pub(crate) run: RunId,
    pub(crate) at: Point<Pixels>,
}

pub(crate) struct CardState {
    pub(crate) thread: ThreadId,
    /// Where the pointer was: the card's top edge follows it.
    pub(crate) y: Pixels,
}

pub(crate) struct Failure {
    pub(crate) thread: ThreadId,
    pub(crate) message: String,
}

pub(crate) type RowBounds = Rc<RefCell<HashMap<ThreadId, Bounds<Pixels>>>>;

pub(crate) struct State {
    shown: Option<Variant>,
    pub(crate) menu: Option<MenuState>,
    rename: Option<Rename>,
    pub(crate) confirm: Option<Confirm>,
    pub(crate) card: Option<CardState>,
    pub(crate) failure: Option<Failure>,
    pub(crate) hover: Option<ThreadId>,
    hover_gen: u64,
    card_hover: bool,
    /// A: the archive is open under "Archived \u{b7} 4".
    pub(crate) archived_open: bool,
    /// B: the list is the archive, not the active threads.
    pub(crate) filter_archived: bool,
    pub(crate) filter_menu: Option<Point<Pixels>>,
    pub(crate) expanded: HashSet<FolderKey>,
    pub(crate) width: f32,
    pub(crate) resizing: bool,
    /// C: the archived page replaces the screen.
    pub(crate) page: bool,
    /// Where each thread's row was last painted.
    pub(crate) rows: RowBounds,
    /// The sidebar's scroll, for the `more` hook.
    pub(crate) scroll: gpui::ScrollHandle,
}

impl State {
    pub(crate) fn new(width: f32) -> Self {
        Self {
            shown: None,
            menu: None,
            rename: None,
            confirm: None,
            card: None,
            failure: None,
            hover: None,
            hover_gen: 0,
            card_hover: false,
            archived_open: false,
            filter_archived: false,
            filter_menu: None,
            expanded: HashSet::new(),
            width: width.clamp(MIN_WIDTH, MAX_WIDTH),
            resizing: false,
            page: false,
            rows: Rc::default(),
            scroll: gpui::ScrollHandle::new(),
        }
    }

    /// Switching variants starts from a quiet sidebar.
    pub(crate) fn sync(&mut self, variant: Variant) {
        if self.shown.is_some_and(|v| v != variant) {
            let (width, rows, scroll) = (self.width, self.rows.clone(), self.scroll.clone());
            *self = Self::new(width);
            self.rows = rows;
            self.scroll = scroll;
        }
        self.shown = Some(variant);
    }

    /// Something of the draft is open: Escape means it.
    pub(crate) fn any_open(&self) -> bool {
        self.menu.is_some()
            || self.rename.is_some()
            || self.confirm.is_some()
            || self.card.is_some()
            || self.failure.is_some()
            || self.filter_menu.is_some()
    }

    /// The rename field and where it is, when one is open for `thread`.
    pub(crate) fn rename_of(&self, thread: &ThreadId, in_card: bool) -> Option<&Entity<Field>> {
        self.rename
            .as_ref()
            .filter(|r| &r.thread == thread && r.in_card == in_card)
            .map(|r| &r.field)
    }

    /// The row bounds of `thread`, once painted.
    pub(crate) fn row_at(&self, thread: &ThreadId) -> Option<Bounds<Pixels>> {
        self.rows.borrow().get(thread).copied()
    }

    fn close_all(&mut self) {
        self.menu = None;
        self.rename = None;
        self.confirm = None;
        self.card = None;
        self.failure = None;
        self.filter_menu = None;
        self.card_hover = false;
    }

    /// The thread on screen in the run screen, which the keys act on when no row is chosen.
    fn current_thread(&self, store: &Entity<Store>, cx: &App) -> Option<ThreadId> {
        if let Some(menu) = &self.menu {
            return Some(menu.thread.clone());
        }
        if let Some(card) = &self.card {
            return Some(card.thread.clone());
        }
        let store = store.read(cx);
        store.looking_at().and_then(|run| store.thread_of(run))
    }

    /// What the menu needs to know about `thread`.
    pub(crate) fn facts(store: &Store, thread: &ThreadId) -> Facts {
        let info = store.state().thread(thread);
        let latest = info.and_then(|i| i.latest_run()).map(|r| r.run_id.clone());
        let marks = store.thread_marks();
        Facts {
            known: info.is_some(),
            edit: store.capabilities().thread_edit,
            pinned: marks.is_some_and(|m| m.pinned.contains(thread)),
            has_path: info.is_some_and(|i| i.worktree.is_some() || i.cwd.is_some()),
            has_session: latest
                .and_then(|run| store.state().run(&run).map(|r| r.session.is_some()))
                .unwrap_or(false),
        }
    }

    /// The text `item` copies.
    fn copy_text(store: &Store, thread: &ThreadId, item: Item) -> Option<String> {
        let info = store.state().thread(thread)?;
        match item {
            Item::CopyPath => info
                .worktree
                .as_ref()
                .or(info.cwd.as_ref())
                .map(|p| p.display().to_string()),
            Item::CopyThreadId => Some(thread.to_string()),
            Item::CopySession => {
                let run = info.latest_run()?.run_id.clone();
                store
                    .state()
                    .run(&run)
                    .and_then(|r| r.session.as_ref().map(|s| s.0.clone()))
            }
            _ => None,
        }
    }

    fn fail(&mut self, thread: &ThreadId, error: &ThreadEditError) {
        self.failure = Some(Failure {
            thread: thread.clone(),
            message: error.to_string(),
        });
    }

    fn start_rename(
        &mut self,
        thread: &ThreadId,
        in_card: bool,
        title: &str,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
        let field = cx.new(|cx| {
            Field::new(
                "draft32-rename",
                "Thread title",
                "Thread title",
                Look::BARE,
                cx,
            )
        });
        field.update(cx, |field, cx| field.set_text(title, cx));
        let submit = cx.subscribe_in(&field, window, |root, _, event: &FieldEvent, window, cx| {
            if *event == FieldEvent::Submit {
                root.draft32_act(Act::RenameSubmit, window, cx);
            }
        });
        let handle = field.read(cx).focus_handle(cx);
        let blur = cx.on_blur(&handle, window, |root, window, cx| {
            root.draft32_act(Act::Blur, window, cx);
        });
        window.focus(&handle, cx);
        self.menu = None;
        self.failure = None;
        self.rename = Some(Rename {
            thread: thread.clone(),
            field,
            in_card,
            _subs: [submit, blur],
        });
    }

    fn timer(&self, gen_: u64, ms: u64, window: &mut Window, cx: &mut Context<RootView>) {
        cx.spawn_in(window, async move |root, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(ms))
                .await;
            root.update_in(cx, |root, window, cx| root.draft32_timer(gen_, window, cx))
                .ok();
        })
        .detach();
    }

    /// The timer a hover started has run out.
    pub(crate) fn on_timer(&mut self, gen_: u64, window: &Window) {
        if gen_ != self.hover_gen {
            return;
        }
        if let Some(thread) = self.hover.clone() {
            if self.card.as_ref().is_none_or(|c| c.thread != thread) && self.rename.is_none() {
                self.confirm = None;
                self.failure = None;
                self.card = Some(CardState {
                    thread,
                    y: window.mouse_position().y,
                });
            }
        } else if !self.card_hover
            && self.rename.is_none()
            && self.confirm.is_none()
            && self.failure.is_none()
        {
            self.card = None;
        }
    }

    /// Does what `act` asks. `true` when the composer should have the focus back.
    pub(crate) fn apply(
        &mut self,
        act: Act,
        variant: Variant,
        store: &Entity<Store>,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) -> bool {
        match act {
            Act::Hover(thread, on) => {
                if on {
                    self.hover = Some(thread);
                } else if self.hover.as_ref() == Some(&thread) {
                    self.hover = None;
                }
                self.hover_gen += 1;
                if variant == Variant::C && self.menu.is_none() {
                    let ms = match (on, self.card.is_some()) {
                        (true, false) => REST_MS,
                        (true, true) => SWITCH_MS,
                        (false, _) => LEAVE_MS,
                    };
                    self.timer(self.hover_gen, ms, window, cx);
                }
                false
            }
            Act::CardHover(on) => {
                self.card_hover = on;
                self.hover_gen += 1;
                if !on {
                    self.timer(self.hover_gen, LEAVE_MS, window, cx);
                }
                false
            }
            Act::Card(thread) => {
                self.close_all();
                self.hover = Some(thread.clone());
                self.hover_gen += 1;
                self.card = Some(CardState {
                    thread,
                    y: window.mouse_position().y,
                });
                false
            }
            Act::Menu(thread, at) => {
                let at = at.or_else(|| {
                    self.row_at(&thread)
                        .map(|b| point(b.left() + px(34.0), b.bottom() - px(2.0)))
                });
                if let Some(at) = at {
                    self.close_all();
                    self.hover = Some(thread.clone());
                    self.menu = Some(MenuState {
                        thread,
                        at,
                        copy: false,
                    });
                }
                false
            }
            Act::CopyList(on) => {
                if let Some(menu) = &mut self.menu {
                    menu.copy = on;
                }
                false
            }
            Act::Do(thread, item) => self.apply_item(thread, item, variant, store, window, cx),
            Act::Unarchive(thread) => {
                let result = store.update(cx, |store, cx| {
                    store.thread_set_archived(&thread, false, cx)
                });
                if let Err(error) = result {
                    self.fail(&thread, &error);
                }
                false
            }
            Act::RenameSubmit => {
                let Some(rename) = &self.rename else {
                    return false;
                };
                let (thread, text) = (
                    rename.thread.clone(),
                    rename.field.read(cx).text().to_owned(),
                );
                match store.update(cx, |store, cx| store.thread_rename(&thread, &text, cx)) {
                    Ok(_) => {
                        self.rename = None;
                        self.failure = None;
                        true
                    }
                    Err(error) => {
                        self.fail(&thread, &error);
                        false
                    }
                }
            }
            Act::ConfirmClose => {
                if let Some(confirm) = self.confirm.take() {
                    self.card = None;
                    let busy = {
                        let store = store.read(cx);
                        store
                            .state()
                            .thread(&confirm.thread)
                            .is_some_and(|i| i.status == reno_proto::ThreadStatus::Active)
                    };
                    if !busy {
                        let thread = confirm.thread;
                        store.update(cx, |store, cx| store.close_thread(thread, cx));
                    }
                }
                false
            }
            Act::Dismiss => {
                let had_rename = self.rename.is_some();
                if self.filter_menu.is_some() {
                    self.filter_menu = None;
                } else if let Some(menu) = &mut self.menu
                    && menu.copy
                {
                    // Esc backs out one step at a time.
                    menu.copy = false;
                } else {
                    self.close_all();
                }
                had_rename
            }
            Act::Blur => {
                // Focus went somewhere on purpose: the rename is over, nothing is stolen back.
                self.rename = None;
                false
            }
            Act::ToggleMore(key) => {
                if !self.expanded.remove(&key) {
                    self.expanded.insert(key);
                }
                false
            }
            Act::ToggleArchived => {
                self.archived_open = !self.archived_open;
                false
            }
            Act::FilterMenu(at) => {
                self.close_all();
                self.filter_menu = at;
                false
            }
            Act::Filter(archived) => {
                self.filter_archived = archived;
                self.filter_menu = None;
                false
            }
            Act::Page(on) => {
                self.close_all();
                self.page = on;
                false
            }
            Act::ScrollEnd => {
                let max = self.scroll.max_offset();
                self.scroll.set_offset(point(px(0.0), -max.y));
                false
            }
            Act::Resize(width) => {
                self.width = width.clamp(MIN_WIDTH, MAX_WIDTH);
                self.resizing = true;
                false
            }
            Act::ResizeEnd => {
                if self.resizing {
                    self.resizing = false;
                    let width = self.width;
                    crate::prefs::update(cx, |prefs| prefs.sidebar_width = width);
                }
                false
            }
        }
    }

    fn apply_item(
        &mut self,
        thread: ThreadId,
        item: Item,
        variant: Variant,
        store: &Entity<Store>,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) -> bool {
        let in_card = variant == Variant::C;
        match item {
            Item::Rename => {
                let title = {
                    let store = store.read(cx);
                    let marks = store.thread_marks();
                    marks
                        .and_then(|m| m.titles.get(&thread).cloned())
                        .or_else(|| self.title_of(store, &thread))
                        .unwrap_or_default()
                };
                self.start_rename(&thread, in_card, &title, window, cx);
            }
            Item::Pin | Item::Unpin => {
                let pin = item == Item::Pin;
                self.menu = None;
                self.failure = None;
                if let Err(e) =
                    store.update(cx, |store, cx| store.thread_set_pinned(&thread, pin, cx))
                {
                    self.fail(&thread, &e);
                }
            }
            Item::Archive => {
                self.menu = None;
                self.failure = None;
                match store.update(cx, |store, cx| store.thread_set_archived(&thread, true, cx)) {
                    Ok(()) => {
                        self.card = None;
                    }
                    Err(e) => self.fail(&thread, &e),
                }
            }
            Item::Copy => {
                if let Some(menu) = &mut self.menu {
                    menu.copy = true;
                }
            }
            Item::CopyPath | Item::CopyThreadId | Item::CopySession => {
                let (text, what) = {
                    let store = store.read(cx);
                    (
                        Self::copy_text(store, &thread, item),
                        match item {
                            Item::CopyPath => "path",
                            Item::CopyThreadId => "thread ID",
                            _ => "harness session ID",
                        },
                    )
                };
                if let Some(text) = text {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                    toast::push(cx, Tone::Info, "Copied", format!("Copied the {what}."));
                }
                self.menu = None;
                self.card = None;
            }
            Item::Close => {
                let run = {
                    let store = store.read(cx);
                    store
                        .state()
                        .thread(&thread)
                        .and_then(|i| i.latest_run())
                        .map(|r| r.run_id.clone())
                };
                if let Some(run) = run {
                    store.update(cx, |store, cx| store.load_changes(&run, cx));
                    let at = self
                        .menu
                        .as_ref()
                        .map(|m| m.at)
                        .or_else(|| {
                            self.row_at(&thread)
                                .map(|b| point(b.left() + px(34.0), b.bottom() - px(2.0)))
                        })
                        .unwrap_or_else(|| window.mouse_position());
                    self.menu = None;
                    self.confirm = Some(Confirm { thread, run, at });
                }
            }
        }
        false
    }

    /// The title a row shows for `thread` before any rename.
    fn title_of(&self, store: &Store, thread: &ThreadId) -> Option<String> {
        let info = store.state().thread(thread)?;
        let first = info.runs.first()?.run_id.clone();
        let title = store.state().run(&first)?.title.clone();
        Some(crate::summary::parse_title(&title).what.to_owned())
    }

    /// ⌘⇧P and ⌘⇧A: the open menu's thread, else the one on screen.
    pub(crate) fn key_act(
        &mut self,
        archive: bool,
        variant: Variant,
        store: &Entity<Store>,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) -> bool {
        let Some(thread) = self.current_thread(store, cx) else {
            return false;
        };
        let item = if archive {
            Item::Archive
        } else if Self::facts(store.read(cx), &thread).pinned {
            Item::Unpin
        } else {
            Item::Pin
        };
        if !store.read(cx).capabilities().thread_edit {
            return false;
        }
        self.apply_item(thread, item, variant, store, window, cx)
    }

    /// Once the list has been laid out again, scroll to its end.
    fn scroll_end_soon(&self, window: &mut Window, cx: &mut Context<RootView>) {
        cx.spawn_in(window, async move |root, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(600))
                .await;
            root.update_in(cx, |root, window, cx| {
                root.draft32_act(Act::ScrollEnd, window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// `RENO_DRAFT32_OPEN=menu|rename|hover|archived|archive|resize|close`, on `thread` (the
    /// "fix the ACP v2 handshake" row unless the hook names another).
    pub(crate) fn hook(
        &mut self,
        what: &str,
        thread: ThreadId,
        variant: Variant,
        store: &Entity<Store>,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
        match what {
            "menu" if variant == Variant::C => {
                self.apply(Act::Card(thread.clone()), variant, store, window, cx);
                self.card = self.card.take().map(|c| CardState {
                    y: self.row_at(&thread).map_or(c.y, |b| b.top()),
                    ..c
                });
            }
            "menu" => {
                self.apply(Act::Menu(thread, None), variant, store, window, cx);
            }
            "rename" => {
                if variant == Variant::C {
                    self.apply(Act::Card(thread.clone()), variant, store, window, cx);
                    self.card = self.card.take().map(|c| CardState {
                        y: self.row_at(&thread).map_or(c.y, |b| b.top()),
                        ..c
                    });
                }
                self.apply_item(thread, Item::Rename, variant, store, window, cx);
            }
            "hover" => {
                if variant == Variant::C {
                    self.hook("menu", thread, variant, store, window, cx);
                } else {
                    self.hover = Some(thread);
                }
            }
            "archived" => {
                match variant {
                    Variant::A => self.archived_open = true,
                    Variant::B => self.filter_archived = true,
                    Variant::C => self.page = true,
                }
                self.scroll_end_soon(window, cx);
            }
            "archive" => {
                if variant == Variant::C {
                    self.hook("menu", thread.clone(), variant, store, window, cx);
                }
                self.hover = Some(thread.clone());
                self.apply_item(thread, Item::Archive, variant, store, window, cx);
            }
            "more" => self.scroll_end_soon(window, cx),
            "resize" => {
                self.width = 300.0;
                self.resizing = true;
            }
            "close" => {
                if variant == Variant::C {
                    self.hook("menu", thread.clone(), variant, store, window, cx);
                }
                self.hover = Some(thread.clone());
                self.apply_item(thread, Item::Close, variant, store, window, cx);
            }
            _ => {}
        }
    }
}
