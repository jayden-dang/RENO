//! The run screen's side panel: how it sits beside the chat, the tab commands and keys, and the
//! page each tab shows. Drawing the strip and the Home page is `panel::view`; the Session and
//! Review pages are in `session.rs` and `changes.rs`, because they read the store.

use std::rc::Rc;

use gpui::{
    AnyElement, App, Context, DragMoveEvent, IntoElement, KeyDownEvent, MouseButton, Render,
    Window, div, prelude::*, px,
};

use super::RunScreen;
use crate::panel::layout::{GUTTER, Layout, OVERLAY_BELOW, fit};
use crate::panel::saved::Saved;
use crate::panel::surface::{Counts, Surface};
use crate::panel::view::{self, Cmd, Emit, Prefs};
use crate::summary;
use crate::ui::theme::Theme;
use crate::ui::toast;
use crate::vm::changes::{Loaded, Pane};
use crate::vm::panel_home::{self, Home, HomeFacts, HomeKind};

/// A key command for the panel. The root view hears the keys (focus can be anywhere, or
/// nowhere, after a click) and hands them to the open run screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PanelAction {
    Toggle,
    NewTab,
    CloseTab,
    ReopenTab,
    NextTab,
    PrevTab,
}

/// What the resize gutter drags.
#[derive(Clone, Copy)]
struct ResizeDrag;

/// The label that follows the pointer while the gutter drags: nothing.
struct NoLabel;

impl Render for NoLabel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

impl RunScreen {
    // --- layout and visibility ---------------------------------------------------------------

    /// How the stage is shared now, from the window, the sidebar and what the person chose.
    pub(super) fn layout(&self, window: &Window, cx: &App) -> Layout {
        let window_w = f32::from(window.viewport_size().width);
        let stage_w = (window_w - view::stage_inset(cx)).max(0.0);
        let prefs = Prefs::get(cx);
        fit(
            window_w,
            stage_w,
            prefs.width,
            prefs.open.unwrap_or(window_w >= OVERLAY_BELOW),
        )
    }

    pub(crate) fn panel_is_open(&self, window: &Window, cx: &App) -> bool {
        self.layout(window, cx) != Layout::Hidden
    }

    fn set_panel_open(&self, open: bool, cx: &mut App) {
        Prefs {
            open: Some(open),
            ..Prefs::get(cx)
        }
        .set(cx);
    }

    /// The panel is going away: if it had the keyboard, the steer field takes it back, so the
    /// screen's keys keep working.
    fn give_back_focus(&self, window: &mut Window, cx: &mut Context<Self>) {
        if self.panel_focus.contains_focused(window, cx) {
            window.focus(&self.focus_handle(cx), cx);
        }
    }

    pub(crate) fn toggle_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let open = self.panel_is_open(window, cx);
        self.set_panel_open(!open, cx);
        if open {
            self.give_back_focus(window, cx);
        }
        self.ensure_patches(cx);
        cx.notify();
    }

    pub(crate) fn panel_action(
        &mut self,
        action: PanelAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let open = self.panel_is_open(window, cx);
        match action {
            PanelAction::Toggle => self.toggle_panel(window, cx),
            PanelAction::NewTab => self.open_surface(Surface::Home, window, cx),
            PanelAction::CloseTab if open => {
                let active = self.panel.active().clone();
                self.panel_cmd(Cmd::Close(active), window, cx);
            }
            PanelAction::ReopenTab => {
                self.panel_touched = true;
                if self.panel.reopen_closed().is_some() {
                    self.persist_panel(cx);
                    self.set_panel_open(true, cx);
                    self.ensure_patches(cx);
                    cx.notify();
                }
            }
            PanelAction::NextTab | PanelAction::PrevTab if open => {
                self.panel.cycle(action == PanelAction::NextTab);
                self.persist_panel(cx);
                self.ensure_patches(cx);
                cx.notify();
            }
            PanelAction::CloseTab | PanelAction::NextTab | PanelAction::PrevTab => {}
        }
    }

    /// The tabs changed: remember them under the panel's key, and write soon.
    fn persist_panel(&self, cx: &mut Context<Self>) {
        Saved::put(self.panel_key.clone(), self.panel.snapshot(), cx);
    }

    // --- opening tabs ------------------------------------------------------------------------

    /// Shows the panel on `surface`, adding its tab when it has none.
    pub(crate) fn open_surface(
        &mut self,
        surface: Surface,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.panel_touched = true;
        self.set_panel_open(true, cx);
        let home = surface == Surface::Home;
        self.panel.open(surface);
        self.persist_panel(cx);
        if home {
            // The page's letters work once the panel has the keyboard.
            window.focus(&self.panel_focus, cx);
        }
        self.ensure_patches(cx);
        cx.notify();
    }

    /// A home row, or its letter. A row that is dimmed does nothing; the one that is built
    /// opens its tab, and the rest open a placeholder.
    fn open_kind(&mut self, kind: HomeKind, window: &mut Window, cx: &mut Context<Self>) {
        let enabled = self
            .home(cx)
            .and_then(|h| h.row(kind).map(|r| r.enabled))
            .unwrap_or(false);
        if !enabled {
            return;
        }
        // The page that was chosen from is replaced by what it opened.
        if *self.panel.active() == Surface::Home {
            self.panel.close(&Surface::Home);
        }
        self.open_surface(Surface::for_row(kind), window, cx);
    }

    pub(super) fn panel_cmd(&mut self, cmd: Cmd, window: &mut Window, cx: &mut Context<Self>) {
        match cmd {
            Cmd::Activate(surface) => {
                self.panel_touched = true;
                self.panel.activate(&surface);
                self.persist_panel(cx);
            }
            Cmd::Close(surface) => {
                self.panel_touched = true;
                self.panel.close(&surface);
                self.persist_panel(cx);
            }
            Cmd::NewTab => return self.open_surface(Surface::Home, window, cx),
            Cmd::Hide => {
                self.set_panel_open(false, cx);
                self.give_back_focus(window, cx);
            }
            Cmd::Reorder { from, to } => {
                self.panel.reorder(from, to);
                self.persist_panel(cx);
            }
            Cmd::Open(kind) => return self.open_kind(kind, window, cx),
            Cmd::Reopen(surface) => return self.open_surface(surface, window, cx),
            Cmd::AddAction => toast::not_yet(cx, "Actions"),
        }
        self.ensure_patches(cx);
        cx.notify();
    }

    fn emitter(&self, cx: &mut Context<Self>) -> Emit {
        let this = cx.entity();
        Rc::new(move |cmd, window, cx| {
            this.update(cx, |screen, cx| screen.panel_cmd(cmd, window, cx));
        })
    }

    // --- seeding ------------------------------------------------------------------------------

    /// Chooses the first tabs once what the run has is known: Review when it has a workspace,
    /// in front when it changed something. Never again, and not after the person has used the
    /// tabs, so it cannot undo what they did.
    pub(super) fn seed_panel(&mut self, cx: &mut Context<Self>) {
        if self.panel_seeded || self.panel_touched {
            return;
        }
        let pane = self.pane(cx);
        let facts = match pane {
            // Not answered yet.
            Pane::Hidden if self.store.read(cx).changes(&self.run).is_none() => return,
            Pane::Loading => return,
            Pane::Hidden | Pane::Unsupported => crate::panel::RunFacts::default(),
            Pane::Failed(_) | Pane::Removed { .. } | Pane::Empty { .. } => crate::panel::RunFacts {
                has_workspace: true,
                has_changes: false,
            },
            Pane::Files(_) => crate::panel::RunFacts {
                has_workspace: true,
                has_changes: true,
            },
        };
        self.panel_seeded = true;
        self.panel = crate::panel::PanelState::reset_for(facts);
        self.ensure_patches(cx);
    }

    // --- what the home needs --------------------------------------------------------------------

    /// The Home page's content for this run, `None` before the run is known.
    pub(super) fn home(&self, cx: &App) -> Option<Home> {
        let store = self.store.read(cx);
        let state = store.state();
        let run = state.run(&self.run)?;
        let pane = self.pane(cx);
        // What `workspace.get` said. A daemon that is not connected, or has not answered, leaves
        // it unread, which is neither "no repository" nor "same folder".
        let connected = store.connection() == reno_client::ConnectionStatus::Connected;
        let record = panel_home::record(
            connected,
            store.supports_changes(),
            store.changes(&self.run).and_then(|c| match &c.workspace {
                Loaded::Ready(w) => Some(w.is_some()),
                _ => None,
            }),
        );
        let project = crate::vm::projects::key_of(Some(run), |dir| store.root_of(dir));
        let project = match project {
            crate::vm::projects::FolderKey::NoProject => None,
            key => Some(key.name()),
        };
        let path = run.cwd.as_ref().map(|p| p.display().to_string());
        let model = self.model_label(cx).map(|(name, _)| name);
        let held = state.approvals_for(&self.run).next();
        let note = panel_home::finding_note(run, held, self.result.as_deref(), store.now_ms());
        let facts = HomeFacts {
            status: run.status,
            problem: summary::reported_problem(run),
            executor: run.executor.as_str(),
            project: project.as_deref(),
            path: path.as_deref(),
            record,
            model: model.as_deref(),
            bot: summary::run_bot(run),
            note: Some(&note),
        };
        Some(panel_home::home(&facts, &pane))
    }

    /// The model in use by name, and by name with its id, as the Session tab words it.
    pub(super) fn model_label(&self, cx: &App) -> Option<(String, String)> {
        let state = self.store.read(cx).state();
        let run = state.run(&self.run)?;
        let id =
            crate::vm::models::in_use(state, &run.run_id).or_else(|| self.facts.model.clone())?;
        let name = crate::vm::models::model_name(state, &run.executor, &id);
        let long = if name == id {
            id
        } else {
            format!("{name} \u{b7} {id}")
        };
        Some((name, long))
    }

    pub(super) fn counts(&self, cx: &App) -> Counts {
        Counts {
            changed_files: match self.pane(cx) {
                Pane::Files(o) => o.rows.len(),
                _ => 0,
            },
        }
    }

    // --- the stage -----------------------------------------------------------------------------

    /// Puts the panel canvas beside the chat (or over it), with the gutter between, and the keys
    /// that drive it.
    pub(super) fn with_panel(
        &mut self,
        stage: gpui::Stateful<gpui::Div>,
        theme: &Theme,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let layout = self.layout(window, cx);
        match layout {
            Layout::Hidden => stage.into_any_element(),
            Layout::Split { panel_w, .. } => {
                let canvas = self.panel_canvas(theme, panel_w, window, cx);
                stage
                    .on_drag_move(cx.listener(|_, e: &DragMoveEvent<ResizeDrag>, _, cx| {
                        // The gutter's middle is under the pointer; the panel is what is right
                        // of it. `fit` clamps whatever this says.
                        let width = f32::from(e.bounds.right() - e.event.position.x) - GUTTER / 2.0;
                        Prefs {
                            width: Some(width),
                            ..Prefs::get(cx)
                        }
                        .set(cx);
                        cx.notify();
                    }))
                    .child(self.gutter(theme))
                    .child(canvas)
                    .into_any_element()
            }
            Layout::Overlay { panel_w } => {
                let canvas = self.panel_canvas(theme, panel_w, window, cx);
                stage
                    .child(
                        div()
                            .absolute()
                            .top_0()
                            .right_0()
                            .bottom_0()
                            .occlude()
                            .shadow_lg()
                            .child(canvas),
                    )
                    .into_any_element()
            }
        }
    }

    fn gutter(&self, theme: &Theme) -> impl IntoElement {
        let accent = theme.accent;
        div()
            .id("panel-gutter")
            .debug_selector(|| "panel-gutter".to_owned())
            .group("panel-gutter")
            .flex_none()
            .w(px(GUTTER))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor_col_resize()
            .on_drag(ResizeDrag, |_, _, _, cx| cx.new(|_| NoLabel))
            .child(
                div()
                    .w(px(2.0))
                    .h(px(40.0))
                    .rounded(px(1.0))
                    .group_hover("panel-gutter", move |s| s.bg(accent)),
            )
    }

    fn panel_canvas(
        &mut self,
        theme: &Theme,
        width: f32,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let emit = self.emitter(cx);
        let counts = self.counts(cx);
        let body = match self.panel.active().clone() {
            Surface::Home => match self.home(cx) {
                Some(home) => {
                    let closed: Vec<Surface> =
                        self.panel.recently_closed(3).into_iter().cloned().collect();
                    view::home_page(theme, &home, &closed, &emit)
                }
                None => div().into_any_element(),
            },
            Surface::Session => self.session_body(theme, cx),
            Surface::Review => self.review_body(theme, cx),
            Surface::Planned(kind) => view::planned_page(theme, kind),
            Surface::History => view::planned_page(theme, HomeKind::History),
            Surface::Terminal(_) => view::planned_page(theme, HomeKind::Terminal),
            Surface::SideChat(_) => view::planned_page(theme, HomeKind::SideChat),
            Surface::File(path) => self.file_body(theme, &path, cx),
            Surface::Commit(_) => view::planned_page(theme, HomeKind::History),
        };
        div()
            .id("panel-canvas")
            .debug_selector(|| "panel-canvas".to_owned())
            .track_focus(&self.panel_focus)
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| window.focus(&this.panel_focus, cx)),
            )
            .on_key_down(cx.listener(Self::on_panel_key))
            .w(px(width))
            .flex_none()
            .h_full()
            .flex()
            .flex_col()
            .rounded(px(14.0))
            .bg(theme.canvas)
            .border_1()
            .border_color(theme.hairline)
            .overflow_hidden()
            .child(view::tab_strip(theme, &self.panel, &counts, &emit))
            .child(div().id("panel-body").flex_1().min_h_0().child(body))
            .into_any_element()
    }

    /// A letter opens its row while the home is in front. It only arrives here when the panel
    /// has the keyboard, so a text field never loses one; a key with no row goes on.
    fn on_panel_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let m = &event.keystroke.modifiers;
        if m.control || m.alt || m.platform || m.function {
            return;
        }
        if *self.panel.active() != Surface::Home {
            return;
        }
        let mut chars = event.keystroke.key.chars();
        let (Some(letter), None) = (chars.next(), chars.next()) else {
            return;
        };
        if let Some(kind) = panel_home::letter_to_kind(letter) {
            cx.stop_propagation();
            self.open_kind(kind, window, cx);
        }
    }
}
