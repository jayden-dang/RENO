//! Where a new task runs: the project, the branch and the checkout, chosen from the mocked
//! places contract (`Store::place_desk`). The new-thread heading is the sentence ("What should we
//! build in [reno] on [main] in [a new worktree]?", `screens/home.rs`); its underlined words
//! open the pickers drawn here, in the composer's popover style and anchored to the word they
//! change. The choice itself (project, branch, checkout) is view state until Send, when it goes
//! to `task.run` (the project as `cwd`) and is bound to the started run through the places seam.

use std::time::Duration;

use gpui::{
    AnyElement, Context, FocusHandle, FontWeight, KeyDownEvent, MouseButton, SharedString, Task,
    Window, div, prelude::*, px,
};
use reno_client::places::{Checkout, CloneError, Project};

use super::completions::Anchor;

/// Room kept above a picker: the status bar and its gutter.
const POPOVER_TOP: f32 = 56.0;
use super::{Composer, Mode};
use crate::ui::composer::menu::{check, menu, menu_row};
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO};
use crate::ui::theme::Theme;
use crate::vm::places::{branches_foot, checkout_label};

/// The branch list shows this many; the foot says how many there are.
const BRANCHES_SHOWN: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlaceMenu {
    Project,
    Branch,
    Checkout,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::ui::composer) enum CloneState {
    /// The URL being typed.
    Typing(String),
    Cloning(String),
    Failed {
        url: String,
        why: String,
    },
}

pub(in crate::ui::composer) struct PlaceState {
    /// The owner's choices; `None` is "the default" (the first project, its default branch).
    pub(in crate::ui::composer) project: Option<String>,
    pub(in crate::ui::composer) branch: Option<String>,
    pub(in crate::ui::composer) checkout: Checkout,
    pub(in crate::ui::composer) menu: Option<PlaceMenu>,
    pub(in crate::ui::composer) query: String,
    pub(in crate::ui::composer) cursor: usize,
    /// The open picker's own keyboard (its search); the box gets it back when it closes.
    pub(in crate::ui::composer) focus: FocusHandle,
    pub(in crate::ui::composer) clone: Option<CloneState>,
    clone_task: Option<Task<()>>,
    /// Where the sentence's three words were last painted: project, branch, checkout.
    pub(in crate::ui::composer) triggers: [Anchor; 3],
    /// The chosen project's folder, for the route preview and the send.
    pub(in crate::ui::composer) cwd: Option<std::path::PathBuf>,
}

impl PlaceState {
    pub(in crate::ui::composer) fn new(cx: &mut Context<Composer>) -> Self {
        Self {
            project: None,
            branch: None,
            checkout: Checkout::NewWorktree,
            menu: None,
            query: String::new(),
            cursor: 0,
            focus: cx.focus_handle(),
            clone: None,
            clone_task: None,
            triggers: Default::default(),
            cwd: None,
        }
    }

    fn trigger(&self, menu: PlaceMenu) -> &Anchor {
        match menu {
            PlaceMenu::Project => &self.triggers[0],
            PlaceMenu::Branch => &self.triggers[1],
            PlaceMenu::Checkout => &self.triggers[2],
        }
    }
}

/// Where the next message goes: the three choices, resolved against the contract.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PlaceView {
    pub(crate) project: Project,
    pub(crate) branch: String,
    pub(crate) checkout: Checkout,
}

/// What a row of a picker does when chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::ui::composer) enum Action {
    Project(String),
    CloneFromUrl,
    Branch(String),
    Checkout(Checkout),
}

pub(in crate::ui::composer) struct Row {
    action: Action,
    label: String,
    detail: String,
    group: &'static str,
    checked: bool,
}

impl Composer {
    /// The place, when the chooser is on and this is a new-thread box.
    pub(crate) fn place_view(&self, cx: &gpui::App) -> Option<PlaceView> {
        if self.mode != Mode::Home {
            return None;
        }
        let store = self.store.read(cx);
        let desk = store.place_desk()?;
        let projects = desk.projects();
        let wanted = self.places.project.as_deref();
        let from_folder = || {
            self.folder.as_ref().map(|folder| {
                projects
                    .iter()
                    .find(|p| &p.path == folder)
                    .cloned()
                    // A project the chooser does not list is still the folder the task runs in.
                    .unwrap_or_else(|| Project {
                        name: folder.file_name().map_or_else(
                            || "project".to_owned(),
                            |n| n.to_string_lossy().into_owned(),
                        ),
                        path: folder.clone(),
                    })
            })
        };
        let project = wanted
            .and_then(|name| projects.iter().find(|p| p.name == name).cloned())
            .or_else(from_folder)
            .or_else(|| projects.first().cloned())?;
        let branch = self.places.branch.clone().unwrap_or_else(|| {
            desk.branches(&project.name, "", 1)
                .branches
                .first()
                .map_or_else(|| "main".to_owned(), |b| b.name.clone())
        });
        Some(PlaceView {
            project,
            branch,
            checkout: self.places.checkout.clone(),
        })
    }

    /// The Home box's folder follows the chosen project (the route preview and the send ask
    /// with it).
    pub(in crate::ui::composer) fn place_sync_place(&mut self, cx: &mut Context<Self>) {
        let cwd = self.place_view(cx).map(|p| p.project.path);
        if self.places.cwd != cwd {
            self.places.cwd = cwd;
            self.refresh_preview(cx);
        }
    }

    // ---- opening and closing -----------------------------------------------------------------

    pub(in crate::ui::composer) fn place_open_menu(
        &mut self,
        menu: PlaceMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.place_view(cx).is_none() {
            return;
        }
        self.close_menus();
        self.class_open = false;
        self.close_picker(cx);
        let place = &mut self.places;
        place.menu = Some(menu);
        place.query.clear();
        place.cursor = 0;
        place.clone = None;
        self.sync_overlay(cx);
        window.focus(&self.places.focus, cx);
        cx.notify();
    }

    pub(in crate::ui::composer) fn place_close_menu(&mut self, cx: &mut Context<Self>) {
        let place = &mut self.places;
        place.menu = None;
        place.query.clear();
        place.clone = None;
        place.clone_task = None;
        // The box gets the keyboard back on the next frame.
        self.refocus = true;
        self.sync_overlay(cx);
        cx.notify();
    }

    pub(in crate::ui::composer) fn place_toggle_menu(
        &mut self,
        menu: PlaceMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.places.menu.is_some() {
            self.place_close_menu(cx);
        } else {
            self.place_open_menu(menu, window, cx);
        }
    }

    // ---- the rows ----------------------------------------------------------------------------

    pub(in crate::ui::composer) fn place_project_rows(
        &self,
        current: &str,
        query: &str,
        cx: &gpui::App,
    ) -> Vec<Row> {
        let store = self.store.read(cx);
        let Some(desk) = store.place_desk() else {
            return Vec::new();
        };
        let query = query.to_lowercase();
        let keep = |p: &Project| p.name.to_lowercase().contains(&query);
        let recents = desk.recent_projects();
        let row = |p: &Project, group| Row {
            action: Action::Project(p.name.clone()),
            label: p.name.clone(),
            detail: p.path.display().to_string(),
            group,
            checked: p.name == current,
        };
        let mut rows: Vec<Row> = recents
            .iter()
            .filter(|p| keep(p))
            .map(|p| row(p, "Recent"))
            .collect();
        rows.extend(
            desk.projects()
                .iter()
                .filter(|p| keep(p) && !recents.iter().any(|r| r.name == p.name))
                .map(|p| row(p, "All projects")),
        );
        rows
    }

    /// The branch rows and the foot ("Showing 20 of 34").
    pub(in crate::ui::composer) fn place_branch_rows(
        &self,
        view: &PlaceView,
        query: &str,
        cx: &gpui::App,
    ) -> (Vec<Row>, String) {
        let store = self.store.read(cx);
        let Some(desk) = store.place_desk() else {
            return (Vec::new(), String::new());
        };
        let page = desk.branches(&view.project.name, query, BRANCHES_SHOWN);
        let foot = branches_foot(page.branches.len(), page.total);
        let rows = page
            .branches
            .into_iter()
            .map(|b| Row {
                checked: b.name == view.branch,
                detail: match (&b.worktree, b.default) {
                    (Some(w), _) => format!("in worktree {w}"),
                    (None, true) => "default".to_owned(),
                    (None, false) => String::new(),
                },
                label: b.name.clone(),
                action: Action::Branch(b.name),
                group: "",
            })
            .collect();
        (rows, foot)
    }

    pub(in crate::ui::composer) fn place_checkout_rows(
        &self,
        view: &PlaceView,
        cx: &gpui::App,
    ) -> Vec<Row> {
        let store = self.store.read(cx);
        let Some(desk) = store.place_desk() else {
            return Vec::new();
        };
        let mut rows = vec![
            Row {
                action: Action::Checkout(Checkout::NewWorktree),
                label: checkout_label(&Checkout::NewWorktree),
                detail: format!("A fresh branch off {}", view.branch),
                group: "",
                checked: view.checkout == Checkout::NewWorktree,
            },
            Row {
                action: Action::Checkout(Checkout::ThisCheckout),
                label: checkout_label(&Checkout::ThisCheckout),
                detail: format!("Work on {} in place", view.branch),
                group: "",
                checked: view.checkout == Checkout::ThisCheckout,
            },
        ];
        rows.extend(desk.worktrees(&view.project.name).into_iter().map(|w| {
            let checked = view.checkout == Checkout::Worktree(w.name.clone());
            Row {
                label: w.name.clone(),
                detail: w.branch,
                action: Action::Checkout(Checkout::Worktree(w.name)),
                group: "Existing worktrees",
                checked,
            }
        }));
        rows
    }

    /// The rows the keyboard moves over in the open picker.
    pub(in crate::ui::composer) fn place_rows(&self, menu: PlaceMenu, cx: &gpui::App) -> Vec<Row> {
        let Some(view) = self.place_view(cx) else {
            return Vec::new();
        };
        let query = &self.places.query;
        match menu {
            PlaceMenu::Project => {
                let mut rows = self.place_project_rows(&view.project.name, query, cx);
                rows.push(Row {
                    action: Action::CloneFromUrl,
                    label: "Clone from URL\u{2026}".to_owned(),
                    detail: String::new(),
                    group: "",
                    checked: false,
                });
                rows
            }
            PlaceMenu::Branch => self.place_branch_rows(&view, query, cx).0,
            PlaceMenu::Checkout => self.place_checkout_rows(&view, cx),
        }
    }

    // ---- choosing ----------------------------------------------------------------------------

    pub(in crate::ui::composer) fn place_pick(
        &mut self,
        action: Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mut close = true;
        match action {
            Action::Project(name) => {
                let place = &mut self.places;
                place.project = Some(name);
                place.branch = None;
                place.checkout = Checkout::NewWorktree;
                place.query.clear();
            }
            Action::Branch(name) => self.places.branch = Some(name),
            Action::Checkout(checkout) => {
                if let Checkout::Worktree(name) = &checkout
                    && let Some(view) = self.place_view(cx)
                    && let Some(branch) = self
                        .store
                        .read(cx)
                        .place_desk()
                        .and_then(|d| {
                            d.worktrees(&view.project.name)
                                .into_iter()
                                .find(|w| &w.name == name)
                        })
                        .map(|w| w.branch)
                {
                    self.places.branch = Some(branch);
                }
                self.places.checkout = checkout;
            }
            Action::CloneFromUrl => {
                self.places.clone = Some(CloneState::Typing(String::new()));
                close = false;
            }
        }
        if close {
            self.place_close_menu(cx);
        } else {
            self.places.cursor = 0;
            window.focus(&self.places.focus, cx);
        }
        self.place_sync_place(cx);
        cx.notify();
    }

    // ---- cloning -----------------------------------------------------------------------------

    pub(in crate::ui::composer) fn place_start_clone(&mut self, cx: &mut Context<Self>) {
        let Some(CloneState::Typing(url) | CloneState::Failed { url, .. }) =
            self.places.clone.clone()
        else {
            return;
        };
        let Some(ms) = self
            .store
            .read(cx)
            .place_desk()
            .map(|d| d.clone_duration_ms(&url))
        else {
            return;
        };
        self.places.clone = Some(CloneState::Cloning(url.clone()));
        self.places.clone_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(ms))
                .await;
            this.update(cx, |this, cx| this.place_finish_clone(&url, cx))
                .ok();
        }));
        cx.notify();
    }

    pub(in crate::ui::composer) fn place_finish_clone(
        &mut self,
        url: &str,
        cx: &mut Context<Self>,
    ) {
        let result = self
            .store
            .read(cx)
            .place_desk()
            .map(|d| d.clone_project(url))
            .unwrap_or(Err(CloneError::NotFound));
        match result {
            Ok(project) => {
                let place = &mut self.places;
                place.project = Some(project.name);
                place.branch = None;
                place.checkout = Checkout::NewWorktree;
                self.place_close_menu(cx);
                self.place_sync_place(cx);
            }
            Err(error) => {
                self.places.clone = Some(CloneState::Failed {
                    url: url.to_owned(),
                    why: error.to_string(),
                });
            }
        }
        cx.notify();
    }

    /// `RENO_COMPOSER_OPEN=clone-ok|clone-fail`: the project picker with a clone under way.
    pub(in crate::ui::composer) fn place_demo_clone(
        &mut self,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.place_open_menu(PlaceMenu::Project, window, cx);
        if self.places.menu.is_some() {
            self.places.clone = Some(CloneState::Typing(url.to_owned()));
            self.place_start_clone(cx);
        }
    }

    // ---- the keyboard of an open picker ------------------------------------------------------

    pub(in crate::ui::composer) fn place_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.places.menu else {
            return;
        };
        let key = &event.keystroke;
        // The clone's URL field takes the typing while it is open.
        if let Some(CloneState::Typing(url) | CloneState::Failed { url, .. }) =
            self.places.clone.clone()
        {
            match key.key.as_str() {
                "escape" => self.places.clone = None,
                "enter" => self.place_start_clone(cx),
                "backspace" => {
                    let mut url = url;
                    url.pop();
                    self.places.clone = Some(CloneState::Typing(url));
                }
                _ => {
                    if let Some(text) = typed(key) {
                        self.places.clone = Some(CloneState::Typing(format!("{url}{text}")));
                    }
                }
            }
            cx.notify();
            return;
        }
        if matches!(self.places.clone, Some(CloneState::Cloning(_))) {
            if key.key == "escape" {
                self.places.clone = None;
                self.places.clone_task = None;
                cx.notify();
            }
            return;
        }
        let rows = self.place_rows(menu, cx);
        let searchable = menu != PlaceMenu::Checkout;
        match key.key.as_str() {
            "escape" => return self.place_close_menu(cx),
            "down" | "up" if !rows.is_empty() => {
                let n = rows.len() as isize;
                let step = if key.key == "down" { 1 } else { -1 };
                self.places.cursor = (self.places.cursor as isize + step).rem_euclid(n) as usize;
            }
            "enter" => {
                if let Some(row) = rows.into_iter().nth(self.places.cursor) {
                    self.place_pick(row.action, window, cx);
                }
                return;
            }
            "backspace" if searchable => {
                self.places.query.pop();
                self.places.cursor = 0;
            }
            _ => {
                if searchable && let Some(text) = typed(key) {
                    self.places.query.push_str(&text);
                    self.places.cursor = 0;
                }
            }
        }
        cx.notify();
    }

    // ---- what the heading's words reach ------------------------------------------------------

    /// Where the sentence's word for `menu` was last painted: the picker hangs from it.
    pub(crate) fn place_anchor(&self, menu: PlaceMenu) -> Anchor {
        self.places.trigger(menu).clone()
    }

    /// The picker that is open, for the word that opened it to show it.
    pub(crate) fn place_open(&self) -> Option<PlaceMenu> {
        self.places.menu
    }

    /// A word of the sentence was clicked: its picker opens, or closes if it was open.
    pub(crate) fn place_toggle(
        &mut self,
        menu: PlaceMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.place_toggle_menu(menu, window, cx);
    }

    // ---- drawing: the pickers ----------------------------------------------------------------

    /// The open picker, attached to the word or chip it changes.
    pub(in crate::ui::composer) fn place_menu(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Deferred> {
        let menu = self.places.menu?;
        let view = self.place_view(cx)?;
        let close: fn(&mut Self, &mut Context<Self>) = |this, cx| this.place_close_menu(cx);
        let at = self.places.trigger(menu).clone();
        let content = self.place_menu_content(theme, menu, &view, cx);
        self.popover(&at, content, close, Some(false), cx)
    }

    pub(in crate::ui::composer) fn place_search(
        &self,
        theme: &Theme,
        placeholder: &'static str,
        text: &str,
    ) -> gpui::Div {
        div()
            .flex_none()
            .h(px(34.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .border_b_1()
            .border_color(theme.hairline)
            .text_size(px(13.0))
            .child(icon(Icon::Search, 13.0, theme.faint))
            .child(if text.is_empty() {
                div()
                    .flex_1()
                    .truncate()
                    .text_color(theme.faint)
                    .child(placeholder)
            } else {
                div()
                    .flex_1()
                    .truncate()
                    .text_color(theme.text)
                    .child(SharedString::from(text.to_owned()))
            })
    }

    /// One row of a picker.
    pub(in crate::ui::composer) fn place_row(
        &self,
        theme: &Theme,
        at: usize,
        row: &Row,
        glyph: Icon,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let action = row.action.clone();
        let under = self.places.cursor == at;
        menu_row(theme, format!("place-row-{at}"), true)
            .when(under, |r| r.bg(theme.row_selected()))
            .child(icon(glyph, 14.0, theme.muted))
            .child(
                div()
                    .flex_none()
                    .max_w(px(200.0))
                    .truncate()
                    .child(SharedString::from(row.label.clone())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(11.5))
                    .text_color(theme.faint)
                    .child(SharedString::from(row.detail.clone())),
            )
            .when(row.checked, |r| r.child(check(theme)))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.place_pick(action.clone(), window, cx);
            }))
    }

    pub(in crate::ui::composer) fn place_group_header(
        theme: &Theme,
        text: &'static str,
    ) -> gpui::Div {
        div()
            .px(px(10.0))
            .pt(px(8.0))
            .pb(px(2.0))
            .text_size(px(11.5))
            .text_color(theme.faint)
            .child(text.to_uppercase())
    }

    /// The rows of a picker, with a header where the group changes.
    pub(in crate::ui::composer) fn place_rows_list(
        &self,
        theme: &Theme,
        rows: &[Row],
        glyph: impl Fn(&Row) -> Icon,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let mut list = div().flex().flex_col();
        let mut group = "";
        for (at, row) in rows.iter().enumerate() {
            if row.group != group {
                group = row.group;
                if !group.is_empty() {
                    list = list.child(Self::place_group_header(theme, group));
                }
            }
            list = list.child(self.place_row(theme, at, row, glyph(row), cx));
        }
        list
    }

    pub(in crate::ui::composer) fn place_menu_shell(
        &self,
        theme: &Theme,
        id: &'static str,
        width: f32,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let focus = self.places.focus.clone();
        let grab = self.places.focus.clone();
        menu(theme, id, width)
            .p(px(0.0))
            .overflow_hidden()
            .track_focus(&focus)
            .on_key_down(cx.listener(|this, event, window, cx| this.place_key(event, window, cx)))
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                window.focus(&grab, cx)
            })
    }

    pub(in crate::ui::composer) fn place_menu_content(
        &self,
        theme: &Theme,
        menu: PlaceMenu,
        view: &PlaceView,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let query = self.places.query.clone();
        // A picker rises from the word it changes: it never gets taller than the room between
        // the status bar and that word, so it can not flip down over the box.
        let room = self
            .places
            .trigger(menu)
            .get()
            .map_or(300.0, |b| f32::from(b.origin.y) - POPOVER_TOP - 8.0);
        match menu {
            PlaceMenu::Project => {
                if let Some(clone) = self.places.clone.clone() {
                    return self.place_clone_panel(theme, &clone, cx);
                }
                let rows = self.place_rows(PlaceMenu::Project, cx);
                let list = self.place_rows_list(
                    theme,
                    &rows,
                    |row| match row.action {
                        Action::Project(_) => Icon::Folder,
                        _ => Icon::Link,
                    },
                    cx,
                );
                self.place_menu_shell(theme, "place-menu-project", 360.0, cx)
                    .child(self.place_search(theme, "Search projects\u{2026}", &query))
                    .child(
                        div()
                            .id("place-list")
                            .max_h(px((room - 34.0).clamp(90.0, 260.0)))
                            .p(px(6.0))
                            .overflow_y_scroll()
                            .child(list),
                    )
            }
            PlaceMenu::Branch => {
                let (rows, foot) = self.place_branch_rows(view, &query, cx);
                let empty = rows.is_empty();
                let list = self.place_rows_list(theme, &rows, |_| Icon::Branch, cx);
                self.place_menu_shell(theme, "place-menu-branch", 340.0, cx)
                    .child(self.place_search(theme, "Search branches\u{2026}", &query))
                    .child(
                        div()
                            .id("place-list")
                            .max_h(px(whole_rows(room - 34.0 - 30.0, 8.0)))
                            .p(px(6.0))
                            .overflow_y_scroll()
                            .child(list),
                    )
                    .child(
                        div()
                            .id("place-foot")
                            .debug_selector(|| "place-foot".to_owned())
                            .flex_none()
                            .px(px(14.0))
                            .py(px(7.0))
                            .border_t_1()
                            .border_color(theme.hairline)
                            .text_size(px(11.5))
                            .text_color(theme.faint)
                            .child(SharedString::from(if empty {
                                "No branch matches".to_owned()
                            } else {
                                foot
                            })),
                    )
            }
            _ => {
                let rows = self.place_checkout_rows(view, cx);
                let list = self.place_rows_list(
                    theme,
                    &rows,
                    |row| match row.action {
                        Action::Checkout(Checkout::NewWorktree) => Icon::Plus,
                        Action::Checkout(Checkout::ThisCheckout) => Icon::Folder,
                        _ => Icon::Branch,
                    },
                    cx,
                );
                self.place_menu_shell(theme, "place-menu-checkout", 380.0, cx)
                    .child(
                        div()
                            .id("place-list")
                            .max_h(px(room.clamp(90.0, 260.0)))
                            .p(px(6.0))
                            .overflow_y_scroll()
                            .child(list),
                    )
            }
        }
    }

    /// The clone's own panel: the URL, and how it is going.
    pub(in crate::ui::composer) fn place_clone_panel(
        &self,
        theme: &Theme,
        clone: &CloneState,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let (url, status): (String, AnyElement) = match clone {
            CloneState::Typing(url) => (
                url.clone(),
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_color(theme.muted)
                    .child(kit::keycap(theme, "\u{23ce}", None))
                    .child("to clone, Esc to go back")
                    .into_any_element(),
            ),
            CloneState::Cloning(url) => (
                url.clone(),
                div()
                    .id("clone-progress")
                    .debug_selector(|| "clone-progress".to_owned())
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .text_color(theme.muted)
                    .child(kit::spinner("clone-spin", 13.0, theme.accent))
                    .child("Cloning\u{2026}")
                    .into_any_element(),
            ),
            CloneState::Failed { url, why } => (
                url.clone(),
                div()
                    .id("clone-failed")
                    .debug_selector(|| "clone-failed".to_owned())
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .text_color(theme.danger)
                            .child(icon(Icon::PhWarning, 13.0, theme.danger))
                            .child(SharedString::from(why.clone())),
                    )
                    .child(
                        div()
                            .text_color(theme.faint)
                            .child("Check the address, and that you can read the repository. Edit it to try again."),
                    )
                    .into_any_element(),
            ),
        };
        self.place_menu_shell(theme, "place-menu-clone", 380.0, cx)
            .child(
                div()
                    .px(px(14.0))
                    .pt(px(12.0))
                    .pb(px(6.0))
                    .text_size(px(12.5))
                    .font_weight(FontWeight::MEDIUM)
                    .child("Clone from URL"),
            )
            .child(
                div()
                    .id("clone-url")
                    .debug_selector(|| "clone-url".to_owned())
                    .mx(px(12.0))
                    .h(px(32.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .rounded(px(8.0))
                    .border_1()
                    .border_color(theme.hairline)
                    .font_family(MONO)
                    .text_size(px(12.0))
                    .child(icon(Icon::Link, 13.0, theme.faint))
                    .child(if url.is_empty() {
                        div()
                            .flex_1()
                            .truncate()
                            .text_color(theme.faint)
                            .child("https://github.com/owner/repo")
                    } else {
                        div().flex_1().truncate().child(SharedString::from(url))
                    }),
            )
            .child(
                div()
                    .px(px(14.0))
                    .py(px(10.0))
                    .text_size(px(12.0))
                    .child(status),
            )
    }
}

/// A picker list's height in whole 30 pt rows (and the 12 pt of padding), so no row shows cut in
/// half: as many as fit in `avail` points, between 3 and `most`.
fn whole_rows(avail: f32, most: f32) -> f32 {
    ((avail - 12.0) / 30.0).floor().clamp(3.0, most) * 30.0 + 12.0
}

/// A key that types: its text, when it has no command modifier.
pub(in crate::ui::composer) fn typed(key: &gpui::Keystroke) -> Option<String> {
    let m = key.modifiers;
    let text = key.key_char.as_deref()?;
    (!(m.platform || m.control || m.alt) && !text.chars().any(char::is_control))
        .then(|| text.to_owned())
}

#[cfg(test)]
impl Composer {
    /// Picks a project, then a branch and a checkout in it, as the pickers would.
    pub(crate) fn place_choose_for_test(
        &mut self,
        project: &str,
        branch: Option<&str>,
        checkout: Option<Checkout>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.place_pick(Action::Project(project.to_owned()), window, cx);
        if let Some(branch) = branch {
            self.place_pick(Action::Branch(branch.to_owned()), window, cx);
        }
        if let Some(checkout) = checkout {
            self.place_pick(Action::Checkout(checkout), window, cx);
        }
    }

    /// Types a URL into the project picker's clone field and presses Enter.
    pub(crate) fn place_clone_for_test(
        &mut self,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.place_demo_clone(url, window, cx);
    }

    /// The clone field: its URL, and why it failed, when it did.
    pub(crate) fn place_clone_state_for_test(&self) -> Option<(String, Option<String>)> {
        match self.places.clone.as_ref()? {
            CloneState::Typing(url) | CloneState::Cloning(url) => Some((url.clone(), None)),
            CloneState::Failed { url, why } => Some((url.clone(), Some(why.clone()))),
        }
    }
}
