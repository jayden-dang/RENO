//! The status bar, unified with the window title bar (app-shell brief, "Status bar"): 40 pt at
//! the top, full width, on `shell` with no edge. Left, after the traffic lights: the sidebar
//! toggle, back and forward, the "…" menu and the current-thread chip. Right: "N live", ⌘K, the
//! daemon's status, the agents-updates island and the one panel toggle.
//!
//! The bar only draws what the root view tells it and calls the root view's methods; the rules
//! (history, live count, which agents have an update) are in `vm/shell.rs` and `vm/executors.rs`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::{
    Anchor, AnyElement, Bounds, Entity, FontWeight, MouseDownEvent, Pixels, Role, SharedString,
    WindowControlArea, anchored, canvas, deferred, div, point, prelude::*, px,
};
use reno_client::ConnectionStatus;

use super::draft38::restart::{self, RestartBar};
use super::icon::{Icon, icon};
use super::kit::{self, MONO, Tone};
use super::sidebar::{MARK_INSET, daemon_label};
use super::theme::Theme;
use crate::actions::{OpenPalette, TogglePanel, ToggleTheme};
use crate::app::RootView;
use crate::nav::Nav;
use crate::vm::executors::{AgentUpdate, UpdatePhase};
use crate::vm::shell::{BAR_HEIGHT, CHIP_MAX, CONTROL, Chip};

/// One agent logo in the updates island, ring included.
const LOGO_BOX: f32 = 22.0;
/// Logos the island stacks before it stops.
const LOGOS: usize = 3;

/// Where a bar control sits, so its popover can hang from it.
pub(crate) type Anchor_ = Rc<Cell<Option<Bounds<Pixels>>>>;

/// What the ROAD-38 draft adds to the bar: the switch's label, the restart affordance and Skip on
/// the agents-updates list. Absent without `--draft38`.
pub(crate) struct DraftBar {
    /// The switch's note, taken when the bar is drawn.
    pub(crate) label: RefCell<Option<AnyElement>>,
    pub(crate) restart: RestartBar,
}

/// What the bar shows.
pub(crate) struct Bar<'a> {
    pub(crate) theme: &'a Theme,
    pub(crate) chip: Chip,
    /// The chip opens the run's project page when it is a run.
    pub(crate) chip_opens_project: bool,
    pub(crate) can_back: bool,
    pub(crate) can_forward: bool,
    pub(crate) live: usize,
    pub(crate) connection: ConnectionStatus,
    pub(crate) updates: &'a [(AgentUpdate, UpdatePhase)],
    pub(crate) updates_open: bool,
    pub(crate) more_open: bool,
    /// The run screen is showing: the panel toggle and the "Toggle side panel" item exist.
    pub(crate) on_run: bool,
    pub(crate) more_at: Anchor_,
    pub(crate) updates_at: Anchor_,
    pub(crate) draft: Option<DraftBar>,
}

/// Reports where its parent was painted into `at`.
pub(crate) fn track(at: &Anchor_) -> impl IntoElement {
    let at = at.clone();
    canvas(move |bounds, _, _| at.set(Some(bounds)), |_, _, _, _| {})
        .absolute()
        .size_full()
}

fn nav_button(
    theme: &Theme,
    id: &'static str,
    glyph: Icon,
    enabled: bool,
) -> gpui::Stateful<gpui::Div> {
    kit::icon_button(theme, id, glyph, CONTROL).when(!enabled, |b| b.opacity(0.3).cursor_default())
}

fn chip_view(theme: &Theme, chip: &Chip) -> gpui::Div {
    div()
        .flex()
        .items_center()
        .gap(px(8.0))
        .h(px(CONTROL))
        .pl(px(5.0))
        .pr(px(12.0))
        .max_w(px(CHIP_MAX))
        .min_w_0()
        .rounded(px(7.0))
        .bg(theme.surface)
        .border_1()
        .border_color(theme.hairline)
        .text_size(px(13.0))
        .font_weight(FontWeight::MEDIUM)
        .child(
            div()
                .flex_none()
                .size(px(18.0))
                .rounded(px(5.0))
                .bg(theme.accent.opacity(0.22))
                .text_color(theme.accent)
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(11.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child(chip.initial.clone()),
        )
        .child(div().min_w_0().truncate().child(chip.title.clone()))
}

pub(crate) fn status_bar(root: &Entity<RootView>, bar: &Bar<'_>) -> AnyElement {
    let theme = bar.theme;
    let on = |f: fn(&mut RootView, &mut gpui::Window, &mut gpui::Context<RootView>)| {
        let root = root.clone();
        move |_: &gpui::ClickEvent, window: &mut gpui::Window, cx: &mut gpui::App| {
            root.update(cx, |root, cx| f(root, window, cx));
        }
    };
    let dot = match bar.connection {
        ConnectionStatus::Connected => theme.success,
        ConnectionStatus::Connecting => theme.warning,
        ConnectionStatus::Disconnected => theme.danger,
    };
    let chip = chip_view(theme, &bar.chip);
    let chip = if bar.chip_opens_project {
        chip.id("bar-chip")
            .debug_selector(|| "bar-chip".to_owned())
            .cursor_pointer()
            .role(Role::Button)
            .aria_label(SharedString::from(format!(
                "{}, open project",
                bar.chip.title
            )))
            .focusable()
            .tab_stop(true)
            .focus_visible(kit::focus_ring(theme))
            .on_click(on(|root, window, cx| root.open_current_project(window, cx)))
            .into_any_element()
    } else {
        chip.id("bar-chip")
            .debug_selector(|| "bar-chip".to_owned())
            .into_any_element()
    };

    // C: the renod chip opens the restart card; the others have their own affordance.
    let chip_restart = bar
        .draft
        .as_ref()
        .map(|d| &d.restart)
        .filter(|r| r.variant == crate::vm::draft38::Variant::C && r.offered);
    let restarting = bar
        .draft
        .as_ref()
        .is_some_and(|d| d.restart.phase == crate::vm::daemon_restart::Phase::Restarting);
    let daemon_dot = if chip_restart.is_some() || restarting {
        theme.warning
    } else {
        dot
    };
    let daemon_chip = kit::chip(theme, daemon_label(bar.connection))
        .id("bar-daemon")
        .debug_selector(|| "bar-daemon".to_owned())
        .flex_row_reverse()
        .child(kit::dot(daemon_dot, 7.0));
    let daemon_chip = match chip_restart {
        Some(rb) => daemon_chip
            .relative()
            .cursor_pointer()
            .border_color(theme.warning.opacity(0.5))
            .role(Role::Button)
            .aria_label("renod, a newer version is installed")
            .focusable()
            .tab_stop(true)
            .focus_visible(kit::focus_ring(theme))
            .child(track(&rb.at))
            .on_click(on(|root, _, cx| root.restart_press(cx)))
            .into_any_element(),
        None => daemon_chip.into_any_element(),
    };
    let restart_pill = bar
        .draft
        .as_ref()
        .and_then(|d| restart::pill(root, theme, &d.restart));
    let draft_label = bar.draft.as_ref().and_then(|d| d.label.borrow_mut().take());

    let island = (!bar.updates.is_empty()).then(|| {
        let logos = bar
            .updates
            .iter()
            .take(LOGOS)
            .enumerate()
            .map(|(i, (u, _))| {
                // A ring in the bar's colour keeps neighbours apart; they overlap by a third.
                div()
                    .flex_none()
                    .when(i > 0, |l| l.ml(px(-LOGO_BOX / 3.0)))
                    .size(px(LOGO_BOX))
                    .rounded(px(LOGO_BOX / 2.0))
                    .bg(theme.surface)
                    .border_2()
                    .border_color(theme.shell)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(kit::exec_glyph(theme, &u.id, 14.0))
            });
        div()
            .id("bar-updates")
            .debug_selector(|| "bar-updates".to_owned())
            .relative()
            .cursor_pointer()
            .flex()
            .items_center()
            .gap(px(8.0))
            .h(px(CONTROL))
            .pl(px(5.0))
            .pr(px(10.0))
            .rounded(px(CONTROL / 2.0))
            .bg(theme.surface)
            .border_1()
            .border_color(if bar.updates_open {
                theme.accent
            } else {
                theme.hairline
            })
            .role(Role::Button)
            .aria_label("Agent updates")
            .focusable()
            .tab_stop(true)
            .focus_visible(kit::focus_ring(theme))
            .child(track(&bar.updates_at))
            .child(div().flex().items_center().children(logos))
            .child(icon(Icon::PhDownload, 15.0, theme.accent))
            .on_click(on(|root, _, cx| root.toggle_updates(cx)))
    });

    div()
        .id("status-bar")
        .debug_selector(|| "status-bar".to_owned())
        .h(px(BAR_HEIGHT))
        .w_full()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(6.0))
        .pl(px(MARK_INSET))
        .pr(px(12.0))
        .bg(theme.shell)
        .window_control_area(WindowControlArea::Drag)
        .child(
            kit::icon_button(theme, "toggle-sidebar", Icon::Sidebar, CONTROL)
                .on_click(on(|root, _, cx| root.toggle_sidebar(cx))),
        )
        .child(
            nav_button(theme, "nav-back", Icon::Back, bar.can_back).when(bar.can_back, |b| {
                b.on_click(on(|root, window, cx| root.step_history(true, window, cx)))
            }),
        )
        .child(
            nav_button(theme, "nav-forward", Icon::Fwd, bar.can_forward)
                .when(bar.can_forward, |b| {
                    b.on_click(on(|root, window, cx| root.step_history(false, window, cx)))
                }),
        )
        .child(
            kit::icon_button(theme, "bar-more", Icon::More, CONTROL)
                .relative()
                .child(track(&bar.more_at))
                .on_click(on(|root, _, cx| root.toggle_more(cx))),
        )
        .child(chip)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .justify_center()
                .children(draft_label),
        )
        .child(
            kit::icon_chip(theme, Icon::Play, format!("{} live", bar.live))
                .id("bar-live")
                .debug_selector(|| "bar-live".to_owned())
                .whitespace_nowrap()
                .cursor_pointer()
                .role(Role::Button)
                .aria_label("All runs")
                .focusable()
                .tab_stop(true)
                .focus_visible(kit::focus_ring(theme))
                .on_click(on(|root, window, cx| root.navigate(Nav::Runs, window, cx))),
        )
        .child(
            kit::icon_chip(theme, Icon::Search, "\u{2318}K")
                .id("bar-palette")
                .debug_selector(|| "bar-palette".to_owned())
                .cursor_pointer()
                .role(Role::Button)
                .aria_label("Command palette")
                .focusable()
                .tab_stop(true)
                .focus_visible(kit::focus_ring(theme))
                .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenPalette), cx)),
        )
        .child(daemon_chip)
        .children(restart_pill)
        .children(island)
        .when(bar.on_run, |b| {
            b.child(
                kit::icon_button(theme, "panel-toggle", Icon::Sidebar, CONTROL)
                    .on_click(|_, window, cx| window.dispatch_action(Box::new(TogglePanel), cx)),
            )
        })
        .into_any_element()
}

/// A popover hung under the control painted at `at`, closed by a click anywhere else.
pub(crate) fn popover(
    root: &Entity<RootView>,
    at: &Anchor_,
    corner: Anchor,
    content: gpui::Stateful<gpui::Div>,
) -> Option<AnyElement> {
    let bounds = at.get()?;
    let (trigger, root) = (at.clone(), root.clone());
    let (attach, anchor) = match corner {
        Anchor::TopRight => (Anchor::BottomRight, Anchor::TopRight),
        _ => (Anchor::BottomLeft, Anchor::TopLeft),
    };
    Some(
        deferred(
            anchored()
                .attach(bounds, attach)
                .anchor(anchor)
                .offset(point(px(0.0), px(6.0)))
                .snap_to_window_with_margin(px(8.0))
                .child(content.occlude().on_mouse_down_out(
                    move |event: &MouseDownEvent, _, cx| {
                        // The trigger toggles; do not close and reopen.
                        if trigger.get().is_some_and(|b| b.contains(&event.position)) {
                            return;
                        }
                        root.update(cx, RootView::close_menus);
                    },
                )),
        )
        .with_priority(2)
        .into_any_element(),
    )
}

pub(crate) fn panel(theme: &Theme, id: &'static str, width: f32) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .w(px(width))
        .rounded(px(10.0))
        .bg(theme.surface)
        .border_1()
        .border_color(theme.hairline)
        .shadow_lg()
        .p(px(6.0))
        .flex()
        .flex_col()
        .gap(px(2.0))
}

/// The popovers: the "…" menu and the agents-updates list.
pub(crate) fn popovers(root: &Entity<RootView>, bar: &Bar<'_>) -> Vec<AnyElement> {
    let theme = bar.theme;
    let mut out = Vec::new();
    if bar.more_open {
        let item =
            |id: &'static str,
             label: &'static str,
             run: fn(&mut RootView, &mut gpui::Window, &mut gpui::Context<RootView>)| {
                let root = root.clone();
                let hover = theme.raised;
                div()
                    .id(id)
                    .debug_selector(move || id.to_owned())
                    .h(px(28.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .rounded(px(6.0))
                    .text_size(px(13.0))
                    .cursor_pointer()
                    .role(Role::Button)
                    .aria_label(label)
                    .hover(move |s| s.bg(hover))
                    .child(label)
                    .on_click(move |_, window, cx| {
                        root.update(cx, |root, cx| {
                            root.close_menus(cx);
                            run(root, window, cx);
                        });
                    })
            };
        let menu = panel(theme, "more-menu", 210.0)
            .child(item("more-palette", "Command palette", |_, w, cx| {
                w.dispatch_action(Box::new(OpenPalette), cx);
            }))
            .child(item("more-theme", "Toggle theme", |_, w, cx| {
                w.dispatch_action(Box::new(ToggleTheme), cx);
            }))
            .when(bar.on_run, |m| {
                m.child(item("more-panel", "Toggle side panel", |_, w, cx| {
                    w.dispatch_action(Box::new(TogglePanel), cx);
                }))
            })
            .child(item("more-triggers", "Triggers", |root, w, cx| {
                root.navigate(Nav::Triggers, w, cx);
            }));
        out.extend(popover(root, &bar.more_at, Anchor::TopLeft, menu));
    }
    if let Some(draft) = &bar.draft
        && let Some(content) = restart::content(root, theme, &draft.restart)
    {
        let corner = if draft.restart.variant == crate::vm::draft38::Variant::B {
            Anchor::TopLeft
        } else {
            Anchor::TopRight
        };
        out.extend(popover(root, &draft.restart.at, corner, content));
    }
    if bar.updates_open && !bar.updates.is_empty() {
        let skippable = bar.draft.is_some();
        let any_available = bar
            .updates
            .iter()
            .any(|(_, phase)| *phase == UpdatePhase::Available);
        let rows = bar.updates.iter().map(|(u, phase)| {
            let id = u.id.clone();
            let root = root.clone();
            let skip_id = u.id.clone();
            let skip_root = root.clone();
            let skip_name = u.name.clone();
            let right = match phase {
                UpdatePhase::Available => div()
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .when(skippable, |d| {
                        d.child(
                            kit::line_button(
                                theme,
                                SharedString::from(format!("skip-{}", u.id)),
                                None,
                                "Skip this version",
                                Tone::Quiet,
                                true,
                            )
                            .aria_label(SharedString::from(format!(
                                "Skip this version of {skip_name}"
                            )))
                            .on_click(move |_, _, cx| {
                                let id = skip_id.clone();
                                skip_root.update(cx, |root, cx| root.skip_update(&id, cx));
                            }),
                        )
                    })
                    .child(
                        kit::line_button(
                            theme,
                            SharedString::from(format!("update-{}", u.id)),
                            None,
                            "Update",
                            Tone::Primary,
                            true,
                        )
                        .on_click(move |_, _, cx| {
                            let id = id.clone();
                            root.update(cx, |root, cx| root.update_agent(&id, cx));
                        }),
                    )
                    .into_any_element(),
                UpdatePhase::Updating => div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(px(12.0))
                    .text_color(theme.muted)
                    .child(kit::spinner(
                        SharedString::from(format!("upd-spin-{}", u.id)),
                        12.0,
                        theme.accent,
                    ))
                    .child("Updating")
                    .into_any_element(),
                UpdatePhase::UpToDate => div()
                    .text_size(px(12.0))
                    .text_color(theme.success)
                    .child("Up to date")
                    .into_any_element(),
            };
            let row_id = format!("update-row-{}", u.id);
            div()
                .id(SharedString::from(row_id.clone()))
                .debug_selector(move || row_id.clone())
                .flex()
                .items_center()
                .gap(px(10.0))
                .px(px(8.0))
                .h(px(40.0))
                .child(kit::exec_glyph(theme, &u.id, 18.0))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .truncate()
                                .text_size(px(13.0))
                                .font_weight(FontWeight::MEDIUM)
                                .child(u.name.clone()),
                        )
                        .child(
                            div()
                                .whitespace_nowrap()
                                .font_family(MONO)
                                .text_size(px(11.5))
                                .text_color(theme.muted)
                                .child(format!("{} \u{2192} {}", u.from, u.to)),
                        ),
                )
                .child(right)
        });
        let all = root.clone();
        let menu = panel(theme, "updates-menu", if skippable { 440.0 } else { 360.0 })
            .child(
                div()
                    .flex()
                    .items_center()
                    .px(px(8.0))
                    .h(px(34.0))
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(13.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Agent updates"),
                    )
                    .when(any_available, |h| {
                        h.child(
                            kit::line_button(
                                theme,
                                "update-all",
                                None,
                                "Update all",
                                Tone::Primary,
                                true,
                            )
                            .on_click(move |_, _, cx| {
                                all.update(cx, |root, cx| root.update_all_agents(cx));
                            }),
                        )
                    }),
            )
            .children(rows);
        out.extend(popover(root, &bar.updates_at, Anchor::TopRight, menu));
    }
    out
}
