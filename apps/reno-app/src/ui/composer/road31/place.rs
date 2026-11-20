//! ROAD-31 draft, where a new task runs: the project, the branch and the checkout, chosen from
//! the mocked places contract (`Store::place_desk`). Variant A draws three chips that each open
//! a popover; B one "Run in" breadcrumb that opens a two-column sheet; C the heading as a
//! sentence whose underlined words open the same popovers. The choice itself (project, branch,
//! checkout) is view state until Send, when it goes to `task.run` (the project as `cwd`) and is
//! bound to the started run through the places seam.

use std::time::Duration;

use gpui::{
    AnyElement, Context, FocusHandle, FontWeight, KeyDownEvent, MouseButton, SharedString, Task,
    Window, canvas, div, prelude::*, px,
};
use reno_client::places::{Checkout, CloneError, Project};

use super::{Anchor, Composer, Mode};
use crate::ui::composer::menu::{check, menu, menu_row};
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO};
use crate::ui::theme::Theme;
use crate::vm::draft31::{Variant, branches_foot, checkout_label, checkout_phrase, place_finding};

/// The branch list shows this many; the foot says how many there are.
const BRANCHES_SHOWN: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PlaceMenu {
    Project,
    Branch,
    Checkout,
    /// The command sheet's one picker: projects, branches and the checkout.
    Sheet,
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
    /// Where the three triggers were last painted: project, branch, checkout.
    pub(in crate::ui::composer) triggers: [Anchor; 3],
    /// The breadcrumb's.
    pub(in crate::ui::composer) crumb: Anchor,
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
            crumb: Anchor::default(),
        }
    }

    fn trigger(&self, menu: PlaceMenu) -> Option<&Anchor> {
        match menu {
            PlaceMenu::Project => Some(&self.triggers[0]),
            PlaceMenu::Branch => Some(&self.triggers[1]),
            PlaceMenu::Checkout => Some(&self.triggers[2]),
            PlaceMenu::Sheet => None,
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
    Browse,
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
    pub(in crate::ui::composer) fn road31_place_view(&self, cx: &gpui::App) -> Option<PlaceView> {
        if self.mode != Mode::Home || self.road31(cx).is_none() {
            return None;
        }
        let store = self.store.read(cx);
        let desk = store.place_desk()?;
        let projects = desk.projects();
        let wanted = self.road31.place.project.as_deref();
        let project = wanted
            .and_then(|name| projects.iter().find(|p| p.name == name))
            .or_else(|| {
                self.folder
                    .as_ref()
                    .and_then(|f| projects.iter().find(|p| &p.path == f))
            })
            .or_else(|| projects.first())?
            .clone();
        let branch = self.road31.place.branch.clone().unwrap_or_else(|| {
            desk.branches(&project.name, "", 1)
                .branches
                .first()
                .map_or_else(|| "main".to_owned(), |b| b.name.clone())
        });
        Some(PlaceView {
            project,
            branch,
            checkout: self.road31.place.checkout.clone(),
        })
    }

    /// The Home box's folder follows the chosen project (the route preview and the send ask
    /// with it).
    pub(in crate::ui::composer) fn road31_sync_place(&mut self, cx: &mut Context<Self>) {
        let cwd = self.road31_place_view(cx).map(|p| p.project.path);
        if self.road31.cwd != cwd {
            self.road31.cwd = cwd;
            self.refresh_preview(cx);
        }
    }

    // ---- opening and closing -----------------------------------------------------------------

    pub(in crate::ui::composer) fn road31_open_menu(
        &mut self,
        menu: PlaceMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.road31_place_view(cx).is_none() {
            return;
        }
        // The command sheet has one picker for all three.
        let menu = if self.road31(cx) == Some(Variant::B) {
            PlaceMenu::Sheet
        } else {
            menu
        };
        self.close_menus();
        self.class_open = false;
        self.close_picker(cx);
        self.road31.queue.open = false;
        let place = &mut self.road31.place;
        place.menu = Some(menu);
        place.query.clear();
        place.cursor = 0;
        place.clone = None;
        self.sync_overlay(cx);
        window.focus(&self.road31.place.focus, cx);
        cx.notify();
    }

    pub(in crate::ui::composer) fn road31_close_menu(&mut self, cx: &mut Context<Self>) {
        let place = &mut self.road31.place;
        place.menu = None;
        place.query.clear();
        place.clone = None;
        place.clone_task = None;
        // The box gets the keyboard back on the next frame.
        self.refocus = true;
        self.sync_overlay(cx);
        cx.notify();
    }

    pub(in crate::ui::composer) fn road31_toggle_menu(
        &mut self,
        menu: PlaceMenu,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.road31.place.menu.is_some() {
            self.road31_close_menu(cx);
        } else {
            self.road31_open_menu(menu, window, cx);
        }
    }

    // ---- the rows ----------------------------------------------------------------------------

    pub(in crate::ui::composer) fn road31_project_rows(
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
    pub(in crate::ui::composer) fn road31_branch_rows(
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

    pub(in crate::ui::composer) fn road31_checkout_rows(
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
    pub(in crate::ui::composer) fn road31_rows(&self, menu: PlaceMenu, cx: &gpui::App) -> Vec<Row> {
        let Some(view) = self.road31_place_view(cx) else {
            return Vec::new();
        };
        let query = &self.road31.place.query;
        match menu {
            PlaceMenu::Project => {
                let mut rows = self.road31_project_rows(&view.project.name, query, cx);
                rows.push(Row {
                    action: Action::Browse,
                    label: "Browse\u{2026}".to_owned(),
                    detail: String::new(),
                    group: "",
                    checked: false,
                });
                rows.push(Row {
                    action: Action::CloneFromUrl,
                    label: "Clone from URL\u{2026}".to_owned(),
                    detail: String::new(),
                    group: "",
                    checked: false,
                });
                rows
            }
            PlaceMenu::Branch | PlaceMenu::Sheet => self.road31_branch_rows(&view, query, cx).0,
            PlaceMenu::Checkout => self.road31_checkout_rows(&view, cx),
        }
    }

    // ---- choosing ----------------------------------------------------------------------------

    pub(in crate::ui::composer) fn road31_pick(
        &mut self,
        action: Action,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let sheet = self.road31.place.menu == Some(PlaceMenu::Sheet);
        let mut close = !sheet;
        match action {
            Action::Project(name) => {
                let place = &mut self.road31.place;
                place.project = Some(name);
                place.branch = None;
                place.checkout = Checkout::NewWorktree;
                place.query.clear();
            }
            Action::Branch(name) => self.road31.place.branch = Some(name),
            Action::Checkout(checkout) => {
                if let Checkout::Worktree(name) = &checkout
                    && let Some(view) = self.road31_place_view(cx)
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
                    self.road31.place.branch = Some(branch);
                }
                self.road31.place.checkout = checkout;
            }
            Action::Browse => {
                crate::ui::toast::push(
                    cx,
                    crate::ui::toast::Tone::NotYet,
                    "Browse",
                    "Browsing opens the system folder dialog; it is not part of this draft",
                );
                close = false;
            }
            Action::CloneFromUrl => {
                self.road31.place.clone = Some(CloneState::Typing(String::new()));
                close = false;
            }
        }
        if close {
            self.road31_close_menu(cx);
        } else {
            self.road31.place.cursor = 0;
            window.focus(&self.road31.place.focus, cx);
        }
        self.road31_sync_place(cx);
        cx.notify();
    }

    // ---- cloning -----------------------------------------------------------------------------

    pub(in crate::ui::composer) fn road31_start_clone(&mut self, cx: &mut Context<Self>) {
        let Some(CloneState::Typing(url) | CloneState::Failed { url, .. }) =
            self.road31.place.clone.clone()
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
        self.road31.place.clone = Some(CloneState::Cloning(url.clone()));
        self.road31.place.clone_task = Some(cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(ms))
                .await;
            this.update(cx, |this, cx| this.road31_finish_clone(&url, cx))
                .ok();
        }));
        cx.notify();
    }

    pub(in crate::ui::composer) fn road31_finish_clone(
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
                let place = &mut self.road31.place;
                place.project = Some(project.name);
                place.branch = None;
                place.checkout = Checkout::NewWorktree;
                self.road31_close_menu(cx);
                self.road31_sync_place(cx);
            }
            Err(error) => {
                self.road31.place.clone = Some(CloneState::Failed {
                    url: url.to_owned(),
                    why: error.to_string(),
                });
            }
        }
        cx.notify();
    }

    /// `RENO_DRAFT31_OPEN=clone-ok|clone-fail`: the project picker with a clone under way.
    pub(in crate::ui::composer) fn road31_demo_clone(
        &mut self,
        url: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.road31_open_menu(PlaceMenu::Project, window, cx);
        if self.road31.place.menu.is_some() {
            self.road31.place.clone = Some(CloneState::Typing(url.to_owned()));
            self.road31_start_clone(cx);
        }
    }

    // ---- the keyboard of an open picker ------------------------------------------------------

    pub(in crate::ui::composer) fn road31_place_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(menu) = self.road31.place.menu else {
            return;
        };
        let key = &event.keystroke;
        // The clone's URL field takes the typing while it is open.
        if let Some(CloneState::Typing(url) | CloneState::Failed { url, .. }) =
            self.road31.place.clone.clone()
        {
            match key.key.as_str() {
                "escape" => self.road31.place.clone = None,
                "enter" => self.road31_start_clone(cx),
                "backspace" => {
                    let mut url = url;
                    url.pop();
                    self.road31.place.clone = Some(CloneState::Typing(url));
                }
                _ => {
                    if let Some(text) = typed(key) {
                        self.road31.place.clone = Some(CloneState::Typing(format!("{url}{text}")));
                    }
                }
            }
            cx.notify();
            return;
        }
        if matches!(self.road31.place.clone, Some(CloneState::Cloning(_))) {
            if key.key == "escape" {
                self.road31.place.clone = None;
                self.road31.place.clone_task = None;
                cx.notify();
            }
            return;
        }
        let rows = self.road31_rows(menu, cx);
        let searchable = menu != PlaceMenu::Checkout;
        match key.key.as_str() {
            "escape" => return self.road31_close_menu(cx),
            "down" | "up" if !rows.is_empty() => {
                let n = rows.len() as isize;
                let step = if key.key == "down" { 1 } else { -1 };
                self.road31.place.cursor =
                    (self.road31.place.cursor as isize + step).rem_euclid(n) as usize;
            }
            "enter" => {
                if let Some(row) = rows.into_iter().nth(self.road31.place.cursor) {
                    self.road31_pick(row.action, window, cx);
                }
                return;
            }
            "backspace" if searchable => {
                self.road31.place.query.pop();
                self.road31.place.cursor = 0;
            }
            _ => {
                if searchable && let Some(text) = typed(key) {
                    self.road31.place.query.push_str(&text);
                    self.road31.place.cursor = 0;
                }
            }
        }
        cx.notify();
    }

    // ---- drawing: the triggers ---------------------------------------------------------------

    pub(in crate::ui::composer) fn road31_trigger(
        &self,
        id: &'static str,
        menu: PlaceMenu,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let anchor = self.road31.place.trigger(menu).cloned().unwrap_or_default();
        div()
            .id(id)
            .debug_selector(|| id.to_owned())
            .relative()
            .child(
                canvas(move |b, _, _| anchor.set(Some(b)), |_, _, _, _| {})
                    .absolute()
                    .size_full(),
            )
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, window, cx| {
                this.road31_toggle_menu(menu, window, cx);
            }))
    }

    /// Variant A: three chips, each opening its popover, and the finding in words under them.
    pub(in crate::ui::composer) fn road31_chip_strip(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let view = self.road31_place_view(cx)?;
        let open = self.road31.place.menu;
        let chip = |id: &'static str,
                    menu: PlaceMenu,
                    glyph: Icon,
                    label: String,
                    cx: &mut Context<Self>| {
            let on = open == Some(menu);
            self.road31_trigger(id, menu, cx).child(
                kit::icon_chip(theme, glyph, label)
                    .when(on, |c| c.border_color(theme.accent.opacity(0.6)))
                    .child(icon(Icon::ChevDown, 12.0, theme.muted)),
            )
        };
        Some(
            div()
                .id("place-strip")
                .debug_selector(|| "place-strip".to_owned())
                .w_full()
                .pb(px(10.0))
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(chip(
                            "place-project",
                            PlaceMenu::Project,
                            Icon::Folder,
                            view.project.name.clone(),
                            cx,
                        ))
                        .child(chip(
                            "place-branch",
                            PlaceMenu::Branch,
                            Icon::Branch,
                            view.branch.clone(),
                            cx,
                        ))
                        .child(chip(
                            "place-checkout",
                            PlaceMenu::Checkout,
                            Icon::Route,
                            checkout_label(&view.checkout),
                            cx,
                        )),
                )
                .child(
                    div()
                        .id("place-finding")
                        .debug_selector(|| "place-finding".to_owned())
                        .px(px(4.0))
                        .text_size(px(12.5))
                        .text_color(theme.muted)
                        .child(SharedString::from(place_finding(
                            &view.project.name,
                            &view.branch,
                            &view.checkout,
                        ))),
                )
                .into_any_element(),
        )
    }

    /// Variant C: the heading is the sentence; its underlined words open the pickers.
    pub(in crate::ui::composer) fn road31_sentence(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let view = self.road31_place_view(cx)?;
        let open = self.road31.place.menu;
        let word = |id: &'static str, menu: PlaceMenu, text: String, cx: &mut Context<Self>| {
            let on = open == Some(menu);
            self.road31_trigger(id, menu, cx)
                .text_color(if on { theme.accent } else { theme.text })
                .underline()
                .hover(|s| s.text_color(theme.accent))
                .child(SharedString::from(text))
        };
        let plain = |text: &'static str| div().child(text);
        Some(
            div()
                .id("place-sentence")
                .debug_selector(|| "place-sentence".to_owned())
                .w_full()
                .pb(px(18.0))
                .flex()
                .flex_wrap()
                .justify_center()
                .items_baseline()
                .gap_x(px(7.0))
                .text_size(px(24.0))
                .line_height(px(34.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child(plain("What should we build in"))
                .child(word(
                    "place-project",
                    PlaceMenu::Project,
                    view.project.name.clone(),
                    cx,
                ))
                .child(plain("on"))
                .child(word(
                    "place-branch",
                    PlaceMenu::Branch,
                    view.branch.clone(),
                    cx,
                ))
                .child(
                    div().child(SharedString::from(
                        checkout_phrase(&view.checkout)
                            .split_once(' ')
                            .map_or(String::new(), |(a, _)| a.to_owned()),
                    )),
                )
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .child(word(
                            "place-checkout",
                            PlaceMenu::Checkout,
                            checkout_phrase(&view.checkout)
                                .split_once(' ')
                                .map_or_else(String::new, |(_, rest)| rest.to_owned()),
                            cx,
                        ))
                        .child(plain("?")),
                )
                .into_any_element(),
        )
    }

    /// Variant B: the "Run in" breadcrumb at the top of the box, and the finding beside it.
    pub(in crate::ui::composer) fn road31_crumb(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let view = self.road31_place_view(cx)?;
        let crumb = self.road31.place.crumb.clone();
        let on = self.road31.place.menu == Some(PlaceMenu::Sheet);
        let sep = || icon(Icon::Fwd, 11.0, theme.faint);
        Some(
            div()
                .id("place-crumb-row")
                .px(px(14.0))
                .pt(px(10.0))
                .flex()
                .items_center()
                .gap(px(10.0))
                .text_size(px(12.5))
                .child(
                    div()
                        .id("place-crumb")
                        .debug_selector(|| "place-crumb".to_owned())
                        .relative()
                        .flex_none()
                        .h(px(26.0))
                        .px(px(8.0))
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .rounded(px(8.0))
                        .border_1()
                        .border_color(if on {
                            theme.accent.opacity(0.6)
                        } else {
                            theme.hairline
                        })
                        .cursor_pointer()
                        .hover(|s| s.bg(theme.row_selected()))
                        .text_color(theme.text)
                        .child(
                            canvas(move |b, _, _| crumb.set(Some(b)), |_, _, _, _| {})
                                .absolute()
                                .size_full(),
                        )
                        .child(div().text_color(theme.faint).child("Run in"))
                        .child(SharedString::from(view.project.name.clone()))
                        .child(sep())
                        .child(SharedString::from(view.branch.clone()))
                        .child(sep())
                        .child(SharedString::from(
                            checkout_label(&view.checkout).to_lowercase(),
                        ))
                        .child(icon(Icon::ChevDown, 12.0, theme.muted))
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.road31_toggle_menu(PlaceMenu::Sheet, window, cx);
                        })),
                )
                .child(
                    div()
                        .id("place-finding")
                        .debug_selector(|| "place-finding".to_owned())
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_color(theme.muted)
                        .child(SharedString::from(place_finding(
                            &view.project.name,
                            &view.branch,
                            &view.checkout,
                        ))),
                )
                .into_any_element(),
        )
    }

    // ---- drawing: the pickers ----------------------------------------------------------------

    /// The open picker, attached to its trigger (A, C) or docked to the box (B).
    pub(in crate::ui::composer) fn road31_place_menu(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Deferred> {
        let menu = self.road31.place.menu?;
        let view = self.road31_place_view(cx)?;
        let close: fn(&mut Self, &mut Context<Self>) = |this, cx| this.road31_close_menu(cx);
        match menu {
            PlaceMenu::Sheet => {
                let at = self.road31.box_bounds.clone();
                let width = at.get()?.size.width;
                let content = self.road31_sheet(theme, &view, width, cx);
                self.popover(&at, content, close, Some(false), cx)
            }
            _ => {
                let at = self.road31.place.trigger(menu)?.clone();
                let content = self.road31_menu(theme, menu, &view, cx);
                self.popover(&at, content, close, Some(false), cx)
            }
        }
    }

    pub(in crate::ui::composer) fn road31_search(
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
    pub(in crate::ui::composer) fn road31_row(
        &self,
        theme: &Theme,
        at: usize,
        row: &Row,
        glyph: Icon,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let action = row.action.clone();
        let under = self.road31.place.cursor == at;
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
                this.road31_pick(action.clone(), window, cx);
            }))
    }

    pub(in crate::ui::composer) fn road31_group_header(
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
    pub(in crate::ui::composer) fn road31_rows_list(
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
                    list = list.child(Self::road31_group_header(theme, group));
                }
            }
            list = list.child(self.road31_row(theme, at, row, glyph(row), cx));
        }
        list
    }

    pub(in crate::ui::composer) fn road31_menu_shell(
        &self,
        theme: &Theme,
        id: &'static str,
        width: f32,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let focus = self.road31.place.focus.clone();
        let grab = self.road31.place.focus.clone();
        menu(theme, id, width)
            .p(px(0.0))
            .overflow_hidden()
            .track_focus(&focus)
            .on_key_down(
                cx.listener(|this, event, window, cx| this.road31_place_key(event, window, cx)),
            )
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                window.focus(&grab, cx)
            })
    }

    pub(in crate::ui::composer) fn road31_menu(
        &self,
        theme: &Theme,
        menu: PlaceMenu,
        view: &PlaceView,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let query = self.road31.place.query.clone();
        match menu {
            PlaceMenu::Project => {
                if let Some(clone) = self.road31.place.clone.clone() {
                    return self.road31_clone_panel(theme, &clone, cx);
                }
                let rows = self.road31_rows(PlaceMenu::Project, cx);
                let list = self.road31_rows_list(
                    theme,
                    &rows,
                    |row| match row.action {
                        Action::Project(_) => Icon::Folder,
                        Action::Browse => Icon::Search,
                        _ => Icon::Link,
                    },
                    cx,
                );
                self.road31_menu_shell(theme, "place-menu-project", 360.0, cx)
                    .child(self.road31_search(theme, "Search projects\u{2026}", &query))
                    .child(
                        div()
                            .id("place-list")
                            .max_h(px(230.0))
                            .p(px(6.0))
                            .overflow_y_scroll()
                            .child(list),
                    )
            }
            PlaceMenu::Branch => {
                let (rows, foot) = self.road31_branch_rows(view, &query, cx);
                let empty = rows.is_empty();
                let list = self.road31_rows_list(theme, &rows, |_| Icon::Branch, cx);
                self.road31_menu_shell(theme, "place-menu-branch", 340.0, cx)
                    .child(self.road31_search(theme, "Search branches\u{2026}", &query))
                    .child(
                        div()
                            .id("place-list")
                            .max_h(px(210.0))
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
                let rows = self.road31_checkout_rows(view, cx);
                let list = self.road31_rows_list(
                    theme,
                    &rows,
                    |row| match row.action {
                        Action::Checkout(Checkout::NewWorktree) => Icon::Plus,
                        Action::Checkout(Checkout::ThisCheckout) => Icon::Folder,
                        _ => Icon::Branch,
                    },
                    cx,
                );
                self.road31_menu_shell(theme, "place-menu-checkout", 380.0, cx)
                    .child(div().p(px(6.0)).child(list))
            }
        }
    }

    /// The clone's own panel: the URL, and how it is going.
    pub(in crate::ui::composer) fn road31_clone_panel(
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
        self.road31_menu_shell(theme, "place-menu-clone", 380.0, cx)
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

    /// Variant B's sheet: projects and branches side by side, the checkout under them.
    pub(in crate::ui::composer) fn road31_sheet(
        &self,
        theme: &Theme,
        view: &PlaceView,
        width: gpui::Pixels,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let query = self.road31.place.query.clone();
        let projects = self.road31_project_rows(&view.project.name, "", cx);
        let (branches, foot) = self.road31_branch_rows(view, &query, cx);
        let worktrees = self
            .store
            .read(cx)
            .place_desk()
            .map(|d| d.worktrees(&view.project.name))
            .unwrap_or_default();
        let existing = matches!(view.checkout, Checkout::Worktree(_));

        let left = projects
            .iter()
            .enumerate()
            .fold(div().flex().flex_col(), |col, (at, row)| {
                let Action::Project(name) = row.action.clone() else {
                    return col;
                };
                col.child(
                    menu_row(theme, format!("sheet-project-{at}"), true)
                        .child(icon(Icon::Folder, 14.0, theme.muted))
                        .child(
                            div()
                                .flex_1()
                                .truncate()
                                .child(SharedString::from(row.label.clone())),
                        )
                        .when(row.checked, |r| r.child(check(theme)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.road31_pick(Action::Project(name.clone()), window, cx);
                        })),
                )
            });
        let right = self.road31_rows_list(theme, &branches, |_| Icon::Branch, cx);
        let segment = |id: &'static str,
                       label: &'static str,
                       on: bool,
                       to: Checkout,
                       cx: &mut Context<Self>| {
            kit::segment(theme, id, label, on).on_click(cx.listener(move |this, _, window, cx| {
                this.road31_pick(Action::Checkout(to.clone()), window, cx);
            }))
        };
        let first_worktree = worktrees.first().map_or(Checkout::NewWorktree, |w| {
            Checkout::Worktree(w.name.clone())
        });
        let controls = kit::segmented(theme)
            .child(segment(
                "sheet-new",
                "New worktree",
                view.checkout == Checkout::NewWorktree,
                Checkout::NewWorktree,
                cx,
            ))
            .child(segment(
                "sheet-this",
                "This checkout",
                view.checkout == Checkout::ThisCheckout,
                Checkout::ThisCheckout,
                cx,
            ))
            .child(segment(
                "sheet-existing",
                "Existing worktree",
                existing,
                first_worktree,
                cx,
            ));
        let chosen = worktrees
            .iter()
            .map(|w| {
                let name = w.name.clone();
                let on = view.checkout == Checkout::Worktree(name.clone());
                kit::chip(theme, format!("{} \u{b7} {}", w.name, w.branch))
                    .id(SharedString::from(format!("sheet-wt-{}", w.name)))
                    .cursor_pointer()
                    .when(on, |c| c.border_color(theme.accent.opacity(0.6)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.road31_pick(
                            Action::Checkout(Checkout::Worktree(name.clone())),
                            window,
                            cx,
                        );
                    }))
            })
            .collect::<Vec<_>>();

        self.road31_menu_shell(theme, "place-sheet", f32::from(width), cx)
            .rounded(px(18.0))
            .child(
                div()
                    .flex()
                    .child(
                        div()
                            .w(px(210.0))
                            .flex_none()
                            .border_r_1()
                            .border_color(theme.hairline)
                            .child(
                                div()
                                    .h(px(34.0))
                                    .px(px(14.0))
                                    .flex()
                                    .items_center()
                                    .border_b_1()
                                    .border_color(theme.hairline)
                                    .text_size(px(11.5))
                                    .text_color(theme.faint)
                                    .child("PROJECTS"),
                            )
                            .child(div().p(px(6.0)).child(left)),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(self.road31_search(theme, "Search branches\u{2026}", &query))
                            .child(
                                div()
                                    .id("place-list")
                                    .max_h(px(230.0))
                                    .p(px(6.0))
                                    .overflow_y_scroll()
                                    .child(right),
                            )
                            .child(
                                div()
                                    .id("place-foot")
                                    .debug_selector(|| "place-foot".to_owned())
                                    .px(px(14.0))
                                    .py(px(6.0))
                                    .text_size(px(11.5))
                                    .text_color(theme.faint)
                                    .child(SharedString::from(foot)),
                            ),
                    ),
            )
            .child(
                div()
                    .px(px(14.0))
                    .py(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(10.0))
                    .border_t_1()
                    .border_color(theme.hairline)
                    .child(
                        div()
                            .text_size(px(11.5))
                            .text_color(theme.faint)
                            .child("CHECKOUT"),
                    )
                    .child(controls)
                    .children(existing.then(|| div().flex().gap(px(6.0)).children(chosen))),
            )
    }
}

/// A key that types: its text, when it has no command modifier.
pub(in crate::ui::composer) fn typed(key: &gpui::Keystroke) -> Option<String> {
    let m = key.modifiers;
    let text = key.key_char.as_deref()?;
    (!(m.platform || m.control || m.alt) && !text.chars().any(char::is_control))
        .then(|| text.to_owned())
}
