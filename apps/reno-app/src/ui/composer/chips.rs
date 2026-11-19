//! The foot's left side and the model chip's small popover: the "+" menu, the permission-mode
//! chip and the effort slider (ui-brief, "Composer, and the panels that take its place").

use gpui::{App, Context, FontWeight, SharedString, canvas, div, prelude::*, px};

use super::menu::{check, menu, menu_row};
use super::{Composer, Mode};
use crate::ui::icon::{Icon, icon};
use crate::ui::kit;
use crate::ui::theme::Theme;
use crate::vm::capabilities::PlusItem;
use crate::vm::{effort_slider, models, permission};

impl Composer {
    /// The "+" menu's rows on this box, from the store's capabilities.
    pub(super) fn plus_items(&self, cx: &App) -> Vec<PlusItem> {
        self.store
            .read(cx)
            .capabilities()
            .plus_items(self.takes_files(cx))
    }

    /// Whether an approval panel is open over this box's run.
    pub(super) fn panel_open(&self, cx: &App) -> bool {
        let Mode::Steer(run) = &self.mode else {
            return false;
        };
        self.store
            .read(cx)
            .state()
            .approvals_for(run)
            .next()
            .is_some()
    }

    /// The "+" button; `None` when it would have nothing to offer.
    pub(super) fn plus_button(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        if self.plus_items(cx).is_empty() {
            return None;
        }
        let bounds = self.plus_bounds.clone();
        Some(
            kit::icon_button(theme, "plus", Icon::Plus, 28.0)
                .relative()
                .child(
                    canvas(move |b, _, _| bounds.set(Some(b)), |_, _, _, _| {})
                        .absolute()
                        .size_full(),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    let open = !this.plus_open;
                    this.close_menus();
                    this.plus_open = open;
                    this.sync_overlay(cx);
                    cx.notify();
                })),
        )
    }

    pub(super) fn plus_menu(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let rows = self.plus_items(cx).into_iter().map(|item| {
            let (id, glyph, on) = match item {
                PlusItem::Files => ("plus-files", Icon::Clip, false),
                PlusItem::PlanMode => ("plus-plan-mode", Icon::List, self.plan_mode),
                PlusItem::Goal => ("plus-goal", Icon::Route, self.goal),
            };
            menu_row(theme, id.to_owned(), true)
                .child(icon(glyph, 14.0, theme.muted))
                .child(div().flex_1().child(item.label()))
                .when(on, |r| r.child(check(theme)))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.plus_open = false;
                    this.sync_overlay(cx);
                    match item {
                        PlusItem::Files => this.pick_files(window, cx),
                        PlusItem::PlanMode => this.plan_mode = !this.plan_mode,
                        PlusItem::Goal => this.goal = !this.goal,
                    }
                    cx.notify();
                }))
        });
        menu(theme, "plus-menu", 220.0).children(rows)
    }

    /// The permission-mode chip, when the connection has the harness's modes (not on the real
    /// daemon yet, ROAD-4). Under an approval panel it shrinks to its icon.
    pub(super) fn permission_chip(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        if !self.store.read(cx).capabilities().permission_modes || self.mode == Mode::Reason {
            return None;
        }
        let mode = self.permission_mode(cx);
        let small = self.panel_open(cx);
        let tone = if mode.warning {
            theme.warning
        } else {
            theme.muted
        };
        let bounds = self.permission_bounds.clone();
        let hover = theme.row_selected();
        Some(
            div()
                .id("permission-chip")
                .debug_selector(|| "permission-chip".to_owned())
                .relative()
                .flex_none()
                .h(px(28.0))
                .px(px(8.0))
                .flex()
                .items_center()
                .gap(px(6.0))
                .rounded(px(8.0))
                .text_size(px(12.5))
                .text_color(tone)
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .child(icon(
                    if mode.warning {
                        Icon::PhWarning
                    } else {
                        Icon::Lock
                    },
                    14.0,
                    tone,
                ))
                .when(!small, |c| {
                    c.child(
                        div()
                            .id("permission-chip-label")
                            .debug_selector(|| "permission-chip-label".to_owned())
                            .child(mode.title),
                    )
                    .child(icon(Icon::ChevDown, 12.0, theme.faint))
                })
                .child(
                    canvas(move |b, _, _| bounds.set(Some(b)), |_, _, _, _| {})
                        .absolute()
                        .size_full(),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    let open = !this.permission_open;
                    this.close_menus();
                    this.permission_open = open;
                    this.close_picker(cx);
                    this.sync_overlay(cx);
                    cx.notify();
                })),
        )
    }

    /// The mode in force: the one picked here, else the harness's first.
    fn permission_mode(&self, cx: &App) -> permission::Mode {
        permission::chosen(
            self.harness_for_modes(cx).as_ref(),
            self.permission.as_deref(),
        )
    }

    /// The harness whose modes the chip lists: the pick's, else the live run's; none on Auto.
    fn harness_for_modes(&self, cx: &App) -> Option<reno_proto::ExecutorId> {
        self.shown_pick(cx)
            .map(|p| p.executor)
            .or_else(|| self.live_run(cx).map(|r| r.executor.clone()))
    }

    pub(super) fn permission_menu(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let harness = self.harness_for_modes(cx);
        let current = self.permission_mode(cx);
        let rows = permission::modes(harness.as_ref()).iter().map(|&mode| {
            let tone = if mode.warning {
                theme.warning
            } else {
                theme.text
            };
            menu_row(theme, format!("permission-{}", mode.id), true)
                .items_start()
                .py(px(6.0))
                .gap(px(10.0))
                .child(icon(
                    if mode.warning {
                        Icon::PhWarning
                    } else {
                        Icon::Lock
                    },
                    15.0,
                    if mode.warning {
                        theme.warning
                    } else {
                        theme.muted
                    },
                ))
                .child(
                    div()
                        .flex_1()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(tone)
                                .child(mode.title),
                        )
                        .child(
                            div()
                                .text_size(px(12.0))
                                .text_color(theme.muted)
                                .child(mode.subtitle),
                        ),
                )
                .when(mode.id == current.id, |r| r.child(check(theme)))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.permission = Some(mode.id.to_owned());
                    this.permission_open = false;
                    this.sync_overlay(cx);
                    cx.notify();
                }))
        });
        menu(theme, "permission-menu", 400.0)
            .child(
                div()
                    .px(px(10.0))
                    .py(px(6.0))
                    .text_size(px(13.0))
                    .font_weight(FontWeight::MEDIUM)
                    .child(SharedString::from(format!(
                        "How should {} actions be approved?",
                        self.harness_name(harness.as_ref(), cx)
                    ))),
            )
            .children(rows)
    }

    fn harness_name(&self, harness: Option<&reno_proto::ExecutorId>, cx: &App) -> String {
        models::harnesses(self.store.read(cx).state())
            .into_iter()
            .find(|h| Some(&h.id) == harness)
            .map_or_else(|| "the agent".to_owned(), |h| h.name)
    }

    pub(super) fn close_menus(&mut self) {
        self.plus_open = false;
        self.permission_open = false;
        self.effort_open = false;
    }

    /// The effort popover: the level in force (warning tone), the model with "›" to the full
    /// picker, and the five steps. A step the model does not list cannot be picked.
    pub(super) fn effort_popover(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let shown = self.shown_pick(cx);
        let state = self.store.read(cx).state();
        let listed = shown.as_ref().and_then(|p| {
            let id = p.model.as_deref()?;
            models::model(state, &p.executor, id)
        });
        let (offers, default) = listed.as_ref().map_or((Vec::new(), None), |m| {
            (m.efforts.clone(), m.default_effort)
        });
        let chosen = shown.as_ref().and_then(|p| p.effort);
        let name = shown
            .as_ref()
            .map_or_else(|| "Auto".to_owned(), |p| models::chip_words(state, p).0);
        let live = self.live_run(cx).is_some();
        let editable = !live || self.live_configurable(cx);
        let opens_picker = self.chip_opens(cx);
        let steps = effort_slider::steps(&offers, chosen, default);
        let last = steps.len() - 1;
        let accent = theme.accent;
        let cells = steps.into_iter().enumerate().map(|(at, step)| {
            let id = format!("effort-step-{}", step.effort.as_str());
            let pickable = step.selectable && editable;
            let effort = step.effort;
            div()
                .id(SharedString::from(id.clone()))
                .debug_selector(move || id.clone())
                .flex_1()
                .h(px(22.0))
                .flex()
                .items_center()
                .when(pickable, |d| d.cursor_pointer())
                .child(
                    div()
                        .w_full()
                        .h(px(3.0))
                        .rounded(px(2.0))
                        .bg(if step.lit {
                            accent.opacity(0.35 + 0.65 * (at as f32 / last as f32))
                        } else {
                            theme.faint.opacity(0.25)
                        })
                        .when(!step.selectable, |d| d.opacity(0.4)),
                )
                .when(pickable, |d| {
                    d.on_click(cx.listener(move |this, _, _, cx| this.click_effort(effort, cx)))
                })
        });
        menu(theme, "effort-popover", 300.0)
            .p(px(12.0))
            .gap(px(8.0))
            .child(
                div()
                    .id("effort-title")
                    .debug_selector(|| "effort-title".to_owned())
                    .text_size(px(14.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.warning)
                    .child(effort_slider::title(chosen, default)),
            )
            .child(
                menu_row(theme, "effort-model".to_owned(), opens_picker)
                    .px(px(0.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(name)),
                    )
                    .when(opens_picker, |r| {
                        r.child(icon(Icon::PhCaretRight, 13.0, theme.faint))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.effort_open = false;
                                this.open_picker(window, cx);
                            }))
                    }),
            )
            .child(div().flex().gap(px(4.0)).children(cells))
    }
}
