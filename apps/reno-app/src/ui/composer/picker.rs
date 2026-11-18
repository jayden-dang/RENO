//! The harness · model picker of the composer: the chip, its popover and the effort control.

use gpui::{App, Context, FontWeight, SharedString, canvas, div, prelude::*, px};
use reno_proto::ExecutorId;

use super::menu::{check, menu, menu_row};
use super::{Composer, Mode};
use crate::ui::icon::{Icon, icon};
use crate::ui::kit;
use crate::ui::theme::Theme;
use crate::vm::models;
use crate::vm::routing::{self, Preview};

impl Composer {
    /// Whether the chip opens the picker: on Home and on an ended run (the next task's harness
    /// and model), and on a live run whose harness switches models.
    pub(super) fn chip_opens(&self, cx: &App) -> bool {
        match &self.mode {
            Mode::Home => true,
            Mode::Reason => false,
            Mode::Steer(_) => match self.live_run(cx) {
                None => true,
                Some(run) => {
                    let state = self.store.read(cx).state();
                    state.supports(reno_proto::FEATURE_RUN_MODEL)
                        && models::harness(state, &run.executor).is_some_and(|h| h.switches)
                }
            },
        }
    }

    /// What the chip and the effort dots show: the pick, else on a run the run's own harness,
    /// model (as its harness last reported it) and effort. `None` is Auto on Home.
    pub(super) fn shown_pick(&self, cx: &App) -> Option<models::Pick> {
        if let Some(pick) = &self.pick {
            return Some(pick.clone());
        }
        let Mode::Steer(run) = &self.mode else {
            return None;
        };
        let state = self.store.read(cx).state();
        state.run(run).map(|r| models::Pick {
            executor: r.executor.clone(),
            model: models::in_use(state, run),
            effort: r.effort,
        })
    }

    pub(super) fn model_chip(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let opens = self.chip_opens(cx);
        let shown = self.shown_pick(cx);
        let state = self.store.read(cx).state();
        let mut chip = div()
            .id("model-chip")
            .debug_selector(|| "model-chip".to_owned())
            .relative()
            .h(px(28.0))
            .px(px(10.0))
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(6.0))
            .rounded(px(8.0))
            .bg(theme.row_selected())
            .text_size(px(12.5))
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.text);
        match &shown {
            None => {
                chip = chip
                    .child(icon(Icon::Route, 13.0, theme.muted))
                    .child(div().flex_none().child("Auto"));
                if matches!(self.preview, Preview::Unavailable(_)) {
                    chip = chip.child(
                        div()
                            .id("route-unavailable")
                            .debug_selector(|| "route-unavailable".to_owned())
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::NORMAL)
                            .text_color(theme.faint)
                            .child("route unavailable"),
                    );
                }
                let routed = match &self.preview {
                    Preview::Ready(decision) => routing::view(decision).chosen,
                    _ => None,
                };
                if let Some(chosen) = routed {
                    chip = chip.child(
                        div()
                            .id("route-name")
                            .debug_selector(|| "route-name".to_owned())
                            .min_w_0()
                            .truncate()
                            .font_weight(FontWeight::NORMAL)
                            .text_color(theme.faint)
                            .child(SharedString::from(chosen.name)),
                    );
                }
            }
            Some(pick) => {
                let (words, extra) = models::chip_words(state, pick);
                chip = chip
                    .child(kit::exec_glyph(theme, pick.executor.as_str(), 13.0))
                    .child(div().min_w_0().truncate().child(SharedString::from(words)))
                    .when_some(extra, |c, extra| {
                        c.child(
                            div()
                                .flex_none()
                                .font_weight(FontWeight::NORMAL)
                                .text_color(theme.faint)
                                .child(extra),
                        )
                    });
            }
        }
        // Where the chip is, for the popover to attach to and to tell its own clicks apart.
        let bounds = self.chip_bounds.clone();
        chip = chip.child(
            canvas(move |b, _, _| bounds.set(Some(b)), |_, _, _, _| {})
                .absolute()
                .size_full(),
        );
        if !opens {
            return chip.when(self.live_run(cx).is_some(), |c| {
                c.child(icon(Icon::Lock, 11.0, theme.faint))
            });
        }
        let hover = theme.raised;
        chip.cursor_pointer()
            .hover(move |s| s.bg(hover))
            .child(icon(
                if self.picker_open {
                    Icon::ChevUp
                } else {
                    Icon::ChevDown
                },
                12.0,
                theme.faint,
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                this.picker_open = !this.picker_open;
                this.picker_for_live = this.live_run(cx).is_some();
                this.picker_drill = None;
                this.class_open = false;
                cx.notify();
            }))
    }

    pub(super) fn close_picker(&mut self, cx: &mut Context<Self>) {
        if self.picker_open {
            self.picker_open = false;
            self.picker_drill = None;
            cx.notify();
        }
    }

    /// Choose a model: on a live run the daemon switches it; otherwise it is the next task's,
    /// keeping the effort chosen before when this model takes it too.
    pub(super) fn choose_model(
        &mut self,
        executor: ExecutorId,
        model: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.close_picker(cx);
        if let (Some(run), Some(model)) =
            (self.live_run(cx).map(|r| r.run_id.clone()), model.clone())
        {
            self.store
                .update(cx, |store, cx| store.set_run_model(run, model, cx));
            cx.notify();
            return;
        }
        let levels = {
            let state = self.store.read(cx).state();
            model
                .as_deref()
                .and_then(|m| models::model(state, &executor, m))
                .map(|m| m.efforts)
                .unwrap_or_default()
        };
        let effort = self
            .shown_pick(cx)
            .and_then(|p| p.effort)
            .filter(|e| levels.contains(e));
        self.pick = Some(models::Pick {
            executor,
            model,
            effort,
        });
        self.refresh_preview(cx);
        cx.notify();
    }

    /// A click on an effort dot: that level, or the model's default when it was the chosen one.
    pub(super) fn click_effort(&mut self, clicked: reno_proto::Effort, cx: &mut Context<Self>) {
        if self.live_run(cx).is_some() {
            return;
        }
        let Some(mut pick) = self.shown_pick(cx) else {
            return;
        };
        pick.effort = models::next_effort(pick.effort, clicked);
        self.pick = Some(pick);
        self.refresh_preview(cx);
        cx.notify();
    }

    /// The effort dots beside the chip: one per level the shown model lists. None when it lists
    /// none. On a live run they are dimmed and do nothing: effort is set when a run starts.
    pub(super) fn effort_dots(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Stateful<gpui::Div>> {
        let shown = self.shown_pick(cx)?;
        let state = self.store.read(cx).state();
        let model = models::model(state, &shown.executor, shown.model.as_deref()?)?;
        if model.efforts.is_empty() {
            return None;
        }
        let editable = self.live_run(cx).is_none();
        let word = shown.effort.map_or("default", |e| e.as_str());
        let dots = models::effort_dots(&model.efforts, shown.effort, model.default_effort);
        let (accent, ring) = (theme.accent, theme.text.opacity(0.28));
        Some(
            div()
                .id("effort-dots")
                .debug_selector(|| "effort-dots".to_owned())
                .flex_none()
                .h(px(28.0))
                .pl(px(4.0))
                .pr(px(2.0))
                .flex()
                .items_center()
                .gap(px(1.0))
                .when(!editable, |d| d.opacity(0.5))
                .children(dots.into_iter().map(|dot| {
                    let effort = dot.effort;
                    let id = format!("effort-dot-{}", effort.as_str());
                    let mark = div()
                        .size(px(8.0))
                        .rounded_full()
                        .border_1()
                        .when(dot.filled, |m| m.bg(accent).border_color(accent))
                        .when(!dot.filled, |m| {
                            m.border_color(if dot.default { accent } else { ring })
                        });
                    div()
                        .id(SharedString::from(id.clone()))
                        .debug_selector(move || id.clone())
                        .size(px(14.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(mark)
                        .when(editable, |d| {
                            d.cursor_pointer()
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.click_effort(effort, cx);
                                }))
                        })
                }))
                .child(
                    div()
                        .id("effort-word")
                        .debug_selector(|| "effort-word".to_owned())
                        .ml(px(4.0))
                        .font_family(kit::MONO)
                        .text_size(px(11.0))
                        .text_color(theme.muted)
                        .child(word),
                ),
        )
    }

    /// The picker: Auto (Home only), then each harness's first models, "More …" drilling in
    /// place to all of one harness's models. A live run lists only its own harness.
    pub(super) fn picker(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let state = self.store.read(cx).state();
        let live = self.live_run(cx).map(|r| r.executor.clone());
        let shown = self.shown_pick(cx);
        let home = self.mode == Mode::Home;
        let chosen = |h: &ExecutorId, m: Option<&str>| {
            shown
                .as_ref()
                .is_some_and(|p| &p.executor == h && p.model.as_deref() == m)
                && (self.pick.is_some() || live.is_some())
        };
        let harnesses: Vec<_> = models::harnesses(state)
            .into_iter()
            .filter(|h| live.as_ref().is_none_or(|l| &h.id == l))
            .collect();
        let model_row = |h: &models::HarnessChoice, m: &models::ModelChoice, two_lines: bool| {
            let (id, model) = (h.id.clone(), m.id.clone());
            let row = menu_row(theme, format!("pick-model-{}", m.id), h.usable)
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .child(SharedString::from(m.name.clone())),
                )
                .when(chosen(&h.id, Some(&m.id)), |r| r.child(check(theme)))
                .when(h.usable, |r| {
                    r.on_click(cx.listener(move |this, _, _, cx| {
                        this.choose_model(id.clone(), Some(model.clone()), cx);
                    }))
                });
            match (two_lines, &m.description) {
                (true, Some(about)) => row.flex_wrap().py(px(5.0)).child(
                    div()
                        .w_full()
                        .text_size(px(12.0))
                        .text_color(theme.muted)
                        .child(SharedString::from(about.clone())),
                ),
                _ => row,
            }
        };
        let default_row = |h: &models::HarnessChoice| {
            let id = h.id.clone();
            menu_row(theme, format!("pick-harness-{}", h.id), h.usable)
                .child(div().flex_1().child("Default model"))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.faint)
                        .child(SharedString::from(format!("{} decides", h.name))),
                )
                .when(chosen(&h.id, None), |r| r.child(check(theme)))
                .when(h.usable, |r| {
                    r.on_click(cx.listener(move |this, _, _, cx| {
                        this.choose_model(id.clone(), None, cx);
                    }))
                })
        };

        let panel = menu(theme, "model-picker", 296.0);
        // "More …": every model of one harness, with what the CLI says each is for.
        if let Some(h) = self
            .picker_drill
            .as_ref()
            .and_then(|id| harnesses.iter().find(|h| &h.id == id))
        {
            return panel
                .child(
                    menu_row(theme, "picker-back".to_owned(), true)
                        .text_color(theme.muted)
                        .child(icon(Icon::Back, 12.0, theme.muted))
                        .child(SharedString::from(format!(
                            "{} \u{b7} {} models",
                            h.name,
                            h.models.len()
                        )))
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.picker_drill = None;
                            cx.notify();
                        })),
                )
                .when(live.is_none(), |p| p.child(default_row(h)))
                .child(
                    div()
                        .id("picker-scroll")
                        .max_h(px(320.0))
                        .overflow_y_scroll()
                        .children(h.models.iter().map(|m| model_row(h, m, true))),
                );
        }

        let title = if live.is_some() {
            "Model \u{b7} this run"
        } else if home {
            "Model \u{b7} new task"
        } else {
            "Model \u{b7} next run"
        };
        let auto = home.then(|| {
            let now = match &self.preview {
                Preview::Ready(d) => routing::view(d)
                    .chosen
                    .map_or("Reno picks".to_owned(), |c| {
                        format!("Reno picks \u{2014} now {}", c.name)
                    }),
                Preview::Unavailable(_) => "Route unavailable".to_owned(),
                _ => "Reno picks".to_owned(),
            };
            menu_row(theme, "pick-harness-auto".to_owned(), true)
                .flex_wrap()
                .py(px(5.0))
                .child(icon(Icon::Route, 13.0, theme.muted))
                .child(div().flex_1().child("Auto"))
                .when(self.pick.is_none(), |r| r.child(check(theme)))
                .child(
                    div()
                        .w_full()
                        .flex()
                        .flex_wrap()
                        .gap(px(4.0))
                        .text_size(px(12.0))
                        .text_color(theme.muted)
                        .child(SharedString::from(now))
                        .child(
                            div()
                                .id("why")
                                .debug_selector(|| "why".to_owned())
                                .text_color(theme.warning)
                                .border_b_1()
                                .border_color(theme.warning.opacity(0.6))
                                .cursor_pointer()
                                .on_click(cx.listener(|this, _, _, cx| {
                                    cx.stop_propagation();
                                    this.why_open = !this.why_open;
                                    this.close_picker(cx);
                                }))
                                .child("why?"),
                        ),
                )
                .on_click(cx.listener(|this, _, _, cx| {
                    this.pick = None;
                    this.close_picker(cx);
                    this.refresh_preview(cx);
                }))
        });
        let sections = harnesses.iter().map(|h| {
            let status: Option<(String, gpui::Hsla)> = if !h.usable {
                Some(("signed out".to_owned(), theme.warning))
            } else if !h.live {
                Some((format!("{}'s own list", h.name), theme.faint))
            } else {
                None
            };
            let current = shown
                .as_ref()
                .filter(|p| p.executor == h.id)
                .and_then(|p| p.model.as_deref());
            let (listed, hidden) = models::folded(&h.models, current);
            let id = h.id.clone();
            div()
                .flex()
                .flex_col()
                .child(div().h(px(1.0)).mx(px(6.0)).my(px(5.0)).bg(theme.hairline))
                .child(
                    div()
                        .id(SharedString::from(format!("harness-head-{}", h.id)))
                        .debug_selector({
                            let id = format!("harness-head-{}", h.id);
                            move || id.clone()
                        })
                        .flex()
                        .items_center()
                        .gap(px(7.0))
                        .px(px(10.0))
                        .pt(px(4.0))
                        .pb(px(2.0))
                        .text_size(px(12.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.muted)
                        .child(kit::exec_glyph(theme, h.id.as_str(), 13.0))
                        .child(div().flex_1().child(SharedString::from(h.name.clone())))
                        .when_some(status, |d, (words, tone)| {
                            d.child(
                                div()
                                    .font_weight(FontWeight::NORMAL)
                                    .text_color(tone)
                                    .child(SharedString::from(words)),
                            )
                        }),
                )
                .when(h.models.is_empty() && live.is_none(), |d| {
                    d.child(default_row(h))
                })
                .children(listed.into_iter().map(|m| model_row(h, m, false)))
                .when(hidden > 0, |d| {
                    d.child(
                        menu_row(theme, format!("more-{}", h.id), true)
                            .text_color(theme.muted)
                            .child(
                                div()
                                    .flex_1()
                                    .child(SharedString::from(format!("More {} models", h.name))),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(theme.faint)
                                    .child(SharedString::from(hidden.to_string())),
                            )
                            .child(icon(Icon::Fwd, 12.0, theme.faint))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.picker_drill = Some(id.clone());
                                cx.notify();
                            })),
                    )
                })
        });
        let switch_note = live
            .as_ref()
            .and_then(|l| harnesses.iter().find(|h| &h.id == l))
            .is_some_and(|h| h.switches)
            .then(|| {
                div()
                    .px(px(10.0))
                    .pt(px(4.0))
                    .pb(px(4.0))
                    .text_size(px(12.0))
                    .text_color(theme.muted)
                    .child("Switching keeps the conversation; it applies from its next step.")
            });
        panel
            .child(
                div()
                    .px(px(10.0))
                    .pt(px(4.0))
                    .pb(px(2.0))
                    .text_size(px(12.0))
                    .text_color(theme.muted)
                    .child(title),
            )
            .children(auto)
            .child(
                div()
                    .id("picker-list")
                    .max_h(px(340.0))
                    .overflow_y_scroll()
                    .children(sections),
            )
            .children(switch_note)
    }
}
