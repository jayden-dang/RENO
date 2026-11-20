//! The sidebar's interaction state, owned by the root view: which menu, confirm or rename is open,
//! which projects list every thread, whether the archive is open, the width. Every click in the
//! sidebar is an [`Act`]; [`State::apply`] is the one place they land, so a test can drive them
//! without a mouse.

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

use crate::app::RootView;
use crate::store::Store;
use crate::ui::field::{Field, FieldEvent, Look};
use crate::ui::toast::{self, Tone};
use crate::vm::projects::FolderKey;
use crate::vm::threads::{Facts, Item};

/// The least and most the sidebar can be dragged to (its default is [`super::WIDTH`]).
pub(crate) const MIN_WIDTH: f32 = 200.0;
pub(crate) const MAX_WIDTH: f32 = 380.0;

/// What a click, key or timer in the sidebar asks for.
#[derive(Clone, Debug)]
pub(crate) enum Act {
    /// The pointer entered or left a thread's row.
    Hover(ThreadId, bool),
    /// Open the row's menu: at the pointer, or (`None`) under the row.
    Menu(ThreadId, Option<Point<Pixels>>),
    /// The menu's Copy sub-list, and back from it.
    CopyList(bool),
    /// An entry of the menu on a thread.
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
    /// The "Archived \u{b7} 4" row.
    ToggleArchived,
    /// Scroll the sidebar to its end (the `archived` and `more` start hooks).
    ScrollEnd,
    /// The sidebar's edge was dragged to this width, and let go.
    Resize(f32),
    ResizeEnd,
}

struct Rename {
    thread: ThreadId,
    field: Entity<Field>,
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

pub(crate) struct Failure {
    pub(crate) thread: ThreadId,
    pub(crate) message: String,
}

pub(crate) type RowBounds = Rc<RefCell<HashMap<ThreadId, Bounds<Pixels>>>>;

pub(crate) struct State {
    pub(crate) menu: Option<MenuState>,
    rename: Option<Rename>,
    pub(crate) confirm: Option<Confirm>,
    pub(crate) failure: Option<Failure>,
    pub(crate) hover: Option<ThreadId>,
    /// The archive is open under "Archived \u{b7} 4".
    pub(crate) archived_open: bool,
    pub(crate) expanded: HashSet<FolderKey>,
    pub(crate) width: f32,
    pub(crate) resizing: bool,
    /// Where each thread's row was last painted.
    pub(crate) rows: RowBounds,
    pub(crate) scroll: gpui::ScrollHandle,
}

impl State {
    pub(crate) fn new(width: f32) -> Self {
        Self {
            menu: None,
            rename: None,
            confirm: None,
            failure: None,
            hover: None,
            archived_open: false,
            expanded: HashSet::new(),
            width: width.clamp(MIN_WIDTH, MAX_WIDTH),
            resizing: false,
            rows: Rc::default(),
            scroll: gpui::ScrollHandle::new(),
        }
    }

    /// Something of the sidebar is open: Escape means it.
    pub(crate) fn any_open(&self) -> bool {
        self.menu.is_some()
            || self.rename.is_some()
            || self.confirm.is_some()
            || self.failure.is_some()
    }

    /// The rename field, when one is open for `thread`.
    pub(crate) fn rename_of(&self, thread: &ThreadId) -> Option<&Entity<Field>> {
        self.rename
            .as_ref()
            .filter(|r| &r.thread == thread)
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
        self.failure = None;
    }

    /// Under the row, a little in from its left edge: where its menu and confirm hang.
    fn under(&self, thread: &ThreadId) -> Option<Point<Pixels>> {
        self.row_at(thread)
            .map(|b| point(b.left() + px(34.0), b.bottom() - px(2.0)))
    }

    /// The thread on screen in the run screen, which the keys act on when no menu is open.
    fn current_thread(&self, store: &Entity<Store>, cx: &App) -> Option<ThreadId> {
        if let Some(menu) = &self.menu {
            return Some(menu.thread.clone());
        }
        let store = store.read(cx);
        store.looking_at().and_then(|run| store.thread_of(run))
    }

    /// What the menu needs to know about `thread`.
    pub(crate) fn facts(store: &Store, thread: &ThreadId, link: bool) -> Facts {
        let info = store.state().thread(thread);
        let latest = info.and_then(|i| i.latest_run()).map(|r| r.run_id.clone());
        Facts {
            known: info.is_some(),
            edit: store.capabilities().thread_edit,
            pinned: store
                .thread_marks()
                .is_some_and(|m| m.pinned.contains(thread)),
            has_path: info.is_some_and(|i| i.worktree.is_some() || i.cwd.is_some()),
            has_session: latest
                .and_then(|run| store.state().run(&run).map(|r| r.session.is_some()))
                .unwrap_or(false),
            link,
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
        title: &str,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
        let field = cx.new(|cx| {
            Field::new(
                "sidebar-rename",
                "Thread title",
                "Thread title",
                Look::RENAME,
                cx,
            )
        });
        field.update(cx, |field, cx| field.set_text(title, cx));
        let submit = cx.subscribe_in(&field, window, |root, _, event: &FieldEvent, window, cx| {
            if *event == FieldEvent::Submit {
                root.sidebar_act(Act::RenameSubmit, window, cx);
            }
        });
        let handle = field.read(cx).focus_handle(cx);
        let blur = cx.on_blur(&handle, window, |root, window, cx| {
            root.sidebar_act(Act::Blur, window, cx);
        });
        window.focus(&handle, cx);
        self.menu = None;
        self.failure = None;
        self.rename = Some(Rename {
            thread: thread.clone(),
            field,
            _subs: [submit, blur],
        });
    }

    /// Does what `act` asks. `true` when the composer should have the focus back.
    pub(crate) fn apply(
        &mut self,
        act: Act,
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
                false
            }
            Act::Menu(thread, at) => {
                if let Some(at) = at.or_else(|| self.under(&thread)) {
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
            Act::Do(thread, item) => self.apply_item(thread, item, store, window, cx),
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
                    let busy = store
                        .read(cx)
                        .state()
                        .thread(&confirm.thread)
                        .is_some_and(|i| i.status == reno_proto::ThreadStatus::Active);
                    if !busy {
                        let thread = confirm.thread;
                        store.update(cx, |store, cx| store.close_thread(thread, cx));
                    }
                }
                false
            }
            Act::Dismiss => {
                let had_rename = self.rename.is_some();
                if let Some(menu) = &mut self.menu
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
        store: &Entity<Store>,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) -> bool {
        match item {
            Item::Rename => {
                let title = {
                    let store = store.read(cx);
                    store
                        .thread_marks()
                        .and_then(|m| m.titles.get(&thread).cloned())
                        .or_else(|| Self::title_of(store, &thread))
                        .unwrap_or_default()
                };
                self.start_rename(&thread, &title, window, cx);
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
                if let Err(e) =
                    store.update(cx, |store, cx| store.thread_set_archived(&thread, true, cx))
                {
                    self.fail(&thread, &e);
                }
            }
            Item::Copy => {
                if let Some(menu) = &mut self.menu {
                    menu.copy = true;
                }
            }
            Item::CopyLink => {
                cx.write_to_clipboard(ClipboardItem::new_string(crate::vm::thread_link::link(
                    &thread,
                )));
                toast::push(cx, Tone::Info, "Copied", "Copied the link to this thread.");
                self.menu = None;
            }
            Item::CopyPath | Item::CopyThreadId | Item::CopySession => {
                let what = match item {
                    Item::CopyPath => "path",
                    Item::CopyThreadId => "thread ID",
                    _ => "harness session ID",
                };
                let text = Self::copy_text(store.read(cx), &thread, item);
                if let Some(text) = text {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                    toast::push(cx, Tone::Info, "Copied", format!("Copied the {what}."));
                }
                self.menu = None;
            }
            Item::Close => {
                let run = store
                    .read(cx)
                    .state()
                    .thread(&thread)
                    .and_then(|i| i.latest_run())
                    .map(|r| r.run_id.clone());
                if let Some(run) = run {
                    store.update(cx, |store, cx| store.load_changes(&run, cx));
                    let at = self
                        .menu
                        .as_ref()
                        .map(|m| m.at)
                        .or_else(|| self.under(&thread))
                        .unwrap_or_else(|| window.mouse_position());
                    self.menu = None;
                    self.confirm = Some(Confirm { thread, run, at });
                }
            }
        }
        false
    }

    /// The title a row shows for `thread` before any rename.
    fn title_of(store: &Store, thread: &ThreadId) -> Option<String> {
        let info = store.state().thread(thread)?;
        let first = info.runs.first()?.run_id.clone();
        let title = store.state().run(&first)?.title.clone();
        Some(crate::summary::parse_title(&title).what.to_owned())
    }

    /// ⌘⇧P and ⌘⇧A: the open menu's thread, else the one on screen.
    pub(crate) fn key_act(
        &mut self,
        archive: bool,
        store: &Entity<Store>,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
        if !store.read(cx).capabilities().thread_edit {
            return;
        }
        let Some(thread) = self.current_thread(store, cx) else {
            return;
        };
        let item = if archive {
            Item::Archive
        } else if Self::facts(store.read(cx), &thread, false).pinned {
            Item::Unpin
        } else {
            Item::Pin
        };
        self.apply_item(thread, item, store, window, cx);
    }

    /// Once the list has been laid out again, scroll to its end.
    fn scroll_end_soon(&self, window: &mut Window, cx: &mut Context<RootView>) {
        cx.spawn_in(window, async move |root, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(600))
                .await;
            root.update_in(cx, |root, window, cx| {
                root.sidebar_act(Act::ScrollEnd, window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// `RENO_SIDEBAR_OPEN=menu|copy|rename|archived|more|archive|resize|close`, on `thread` (the
    /// "fix the ACP v2 handshake" thread unless the hook names another): states for screenshots.
    pub(crate) fn hook(
        &mut self,
        what: &str,
        thread: ThreadId,
        store: &Entity<Store>,
        window: &mut Window,
        cx: &mut Context<RootView>,
    ) {
        match what {
            "menu" => {
                self.apply(Act::Menu(thread, None), store, window, cx);
            }
            "copy" => {
                self.apply(Act::Menu(thread, None), store, window, cx);
                self.apply(Act::CopyList(true), store, window, cx);
            }
            "rename" => {
                self.apply_item(thread, Item::Rename, store, window, cx);
            }
            "archived" => {
                self.archived_open = true;
                self.scroll_end_soon(window, cx);
            }
            "more" => self.scroll_end_soon(window, cx),
            "archive" => {
                self.hover = Some(thread.clone());
                self.apply_item(thread, Item::Archive, store, window, cx);
            }
            "resize" => {
                self.width = 300.0;
                self.resizing = true;
            }
            "close" => {
                self.hover = Some(thread.clone());
                self.apply_item(thread, Item::Close, store, window, cx);
            }
            _ => {}
        }
    }
}
