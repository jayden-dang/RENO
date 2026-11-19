//! The harness · model picker of the composer: the chip, and the popover with its glyph rail,
//! search, list and the Account and Effort rows (ui-brief 2026-11-18, "v2 — Rail").

use gpui::{
    App, Context, FontWeight, Hsla, KeyDownEvent, MouseButton, SharedString, Window, canvas, div,
    prelude::*, px,
};
use reno_proto::{Effort, ExecutorId, MODEL_OPTION_CONTEXT_WINDOW};

use super::menu::{Tip, check, menu, menu_row};
use super::{Composer, Mode, OpenExecutors};
use crate::ui::icon::{Icon, icon};
use crate::ui::kit;
use crate::ui::theme::Theme;
use crate::vm::context;
use crate::vm::models;
use crate::vm::models::{Account, HarnessChoice, Pick};
use crate::vm::picker::{self, Listing, Row, Tab, TabView};
use crate::vm::routing::{self, Preview};

/// The popover's width.
const WIDTH: f32 = 344.0;
/// The list's tallest height; longer lists scroll.
const LIST_MAX: f32 = 248.0;

/// What the open picker shows, worked out once per frame and per key.
pub(super) struct View {
    pub(super) harnesses: Vec<HarnessChoice>,
    /// The harness of the live run this box steers: the only one it lists.
    pub(super) live: Option<ExecutorId>,
    pub(super) shown: Option<Pick>,
    pub(super) tabs: Vec<TabView>,
    pub(super) tab: Tab,
    pub(super) listing: Listing,
}

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
        state.run(run).map(|r| {
            // What the harness reported is resolved to the model it lists, so the chip, the
            // effort levels, the window row and the check mark all find it ("opus-5.5" and
            // "claude-opus-5-5[1m]" are Opus 5.5, the latter at 1M).
            let reported = models::in_use(state, run);
            let mut options = r.model_options.clone();
            let model = match reported
                .as_deref()
                .and_then(|m| models::resolve(state, &r.executor, m))
            {
                Some((listed, implied)) => {
                    options.extend(implied);
                    Some(listed.id)
                }
                None => reported,
            };
            models::Pick {
                executor: r.executor.clone(),
                model,
                effort: r.effort,
                options,
            }
        })
    }

    /// What the picker shows now: the rail, the viewed tab and its rows for the query.
    pub(super) fn picker_view(&self, cx: &App) -> View {
        let store = self.store.read(cx);
        let live = self.live_run(cx).map(|r| r.executor.clone());
        let harnesses: Vec<_> = models::harnesses(store.state())
            .into_iter()
            .filter(|h| live.as_ref().is_none_or(|l| &h.id == l))
            // Settings › General › In the composer; a run keeps its agent.
            .filter(|h| live.is_some() || store.composer_prefs().offered(h.id.as_str()))
            .collect();
        let shown = self.shown_pick(cx);
        let tabs = picker::tabs(&harnesses, live.as_ref(), self.mode == Mode::Home);
        let tab = tabs
            .iter()
            .find(|t| t.tab == self.picker_tab)
            .or(tabs.first())
            .map_or(Tab::Favourites, |t| t.tab.clone());
        let prefs = store.composer_prefs();
        let listing = picker::listing(&self.picker_query, &tab, &harnesses, |h, m| {
            prefs.is_favourite(h, m)
        });
        View {
            harnesses,
            live,
            shown,
            tabs,
            tab,
            listing,
        }
    }

    pub(super) fn model_chip(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
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
                                .id("chip-effort")
                                .debug_selector(|| "chip-effort".to_owned())
                                .flex_none()
                                .font_weight(FontWeight::NORMAL)
                                .text_color(theme.faint)
                                .child(SharedString::from(extra)),
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
        // The chip opens the small effort popover, also where the model cannot be switched: the
        // effort of a live run can still change, and "Model ›" is there only when it can open
        // the picker.
        let hover = theme.raised;
        chip.cursor_pointer()
            .hover(move |s| s.bg(hover))
            .child(icon(
                if self.picker_open || self.effort_open {
                    Icon::ChevUp
                } else {
                    Icon::ChevDown
                },
                12.0,
                theme.faint,
            ))
            .on_click(cx.listener(|this, _, _, cx| {
                if this.picker_open {
                    this.close_picker(cx);
                } else {
                    let open = !this.effort_open;
                    this.close_menus();
                    this.class_open = false;
                    this.effort_open = open;
                    this.sync_overlay(cx);
                    cx.notify();
                }
            }))
    }

    /// Opens the picker on the tab of what is shown, with its search empty and the keyboard in
    /// it.
    pub(super) fn open_picker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (tab, live) = {
            let live = self.live_run(cx).map(|r| r.executor.clone());
            let harnesses = models::harnesses(self.store.read(cx).state());
            let tab = picker::opening_tab(
                self.shown_pick(cx).as_ref(),
                &harnesses,
                live.as_ref(),
                self.mode == Mode::Home,
            );
            (tab, live.is_some())
        };
        if let Tab::Harness(id) = &tab {
            // The picker is being looked at: read the account's usage again (the daemon reads
            // at most once per 30 s; nothing polls, ARCH-10).
            let id = id.clone();
            self.store
                .update(cx, |store, cx| store.refresh_usage(id, cx));
        }
        self.close_menus();
        self.picker_open = true;
        self.picker_window_open = false;
        self.picker_for_live = live;
        self.picker_tab = tab;
        self.picker_query.clear();
        self.picker_cursor = None;
        self.picker_keyboard = false;
        self.class_open = false;
        self.sync_overlay(cx);
        window.focus(&self.picker_focus, cx);
        cx.notify();
    }

    pub(super) fn close_picker(&mut self, cx: &mut Context<Self>) {
        if self.picker_open {
            self.picker_open = false;
            self.usage_card_open = false;
            self.picker_window_open = false;
            // The next frame gives the keyboard back to the text.
            self.refocus = true;
            self.sync_overlay(cx);
            cx.notify();
        }
    }

    /// Choose a model: on a live run the daemon switches it; otherwise it is the next task's. The
    /// picker stays open. The effort becomes the one remembered for this model, else the one
    /// chosen before when this model takes it too.
    pub(super) fn choose_model(
        &mut self,
        executor: ExecutorId,
        model: Option<String>,
        cx: &mut Context<Self>,
    ) {
        if let (Some(run), Some(model)) =
            (self.live_run(cx).map(|r| r.run_id.clone()), model.clone())
        {
            self.store
                .update(cx, |store, cx| store.set_run_model(run, model, cx));
            cx.notify();
            return;
        }
        let (effort, options) = {
            let store = self.store.read(cx);
            let listed = model
                .as_deref()
                .and_then(|m| models::model(store.state(), &executor, m));
            let options = listed.as_ref().map_or_else(Default::default, |m| {
                context::options_for_model(
                    m,
                    model
                        .as_deref()
                        .and_then(|id| store.composer_prefs().options_for(&executor, id)),
                )
            });
            let remembered = model
                .as_deref()
                .and_then(|m| store.composer_prefs().effort_for(&executor, m));
            let carried = self.shown_pick(cx).and_then(|p| p.effort);
            let effort = picker::effort_on_pick(
                remembered,
                carried,
                &listed.map(|m| m.efforts).unwrap_or_default(),
            );
            (effort, options)
        };
        self.pick = Some(Pick {
            executor,
            model,
            effort,
            options,
        });
        self.picker_window_open = false;
        self.refresh_preview(cx);
        cx.notify();
    }

    /// Choose Auto: Reno picks.
    fn choose_auto(&mut self, cx: &mut Context<Self>) {
        self.pick = None;
        self.refresh_preview(cx);
        cx.notify();
    }

    /// A click on an effort cell: that level, or the model's default when it was the chosen one.
    /// What is chosen is remembered for the model; on a live run the daemon changes it.
    pub(super) fn click_effort(&mut self, clicked: Effort, cx: &mut Context<Self>) {
        let Some(mut pick) = self.shown_pick(cx) else {
            return;
        };
        pick.effort = picker::next_effort(pick.effort, clicked);
        if let Some(run) = self.live_run(cx).map(|r| r.run_id.clone()) {
            // A live run: the daemon changes it (or stores the change for its next launch) and
            // says when it applies.
            let change = pick
                .effort
                .map_or(reno_proto::EffortChange::Clear, |effort| {
                    reno_proto::EffortChange::Set { effort }
                });
            self.store.update(cx, |store, cx| {
                store.configure_run(run, Some(change), Default::default(), cx);
            });
            return;
        }
        if let Some(model) = pick.model.clone() {
            let (executor, effort) = (pick.executor.clone(), pick.effort);
            self.store.update(cx, |store, cx| {
                store.update_composer_prefs(|p| p.remember_effort(&executor, &model, effort), cx);
            });
        }
        self.pick = Some(pick);
        self.refresh_preview(cx);
        cx.notify();
    }

    /// A click on a size of the context window: the next task's, remembered for the model; on a
    /// live run the daemon changes it.
    fn choose_option(&mut self, option: &str, choice: &str, cx: &mut Context<Self>) {
        if let Some(run) = self.live_run(cx).map(|r| r.run_id.clone()) {
            let options = [(option.to_owned(), choice.to_owned())].into();
            self.store
                .update(cx, |store, cx| store.configure_run(run, None, options, cx));
            self.picker_window_open = false;
            cx.notify();
            return;
        }
        let Some(mut pick) = self.shown_pick(cx) else {
            return;
        };
        let Some(model) = pick.model.clone() else {
            return;
        };
        pick.options.insert(option.to_owned(), choice.to_owned());
        let (executor, option, choice) =
            (pick.executor.clone(), option.to_owned(), choice.to_owned());
        self.store.update(cx, |store, cx| {
            store.update_composer_prefs(
                |p| p.remember_option(&executor, &model, &option, &choice),
                cx,
            );
        });
        self.pick = Some(pick);
        self.picker_window_open = false;
        self.refresh_preview(cx);
        cx.notify();
    }

    fn toggle_star(&mut self, executor: &ExecutorId, model: &str, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| {
            store.update_composer_prefs(
                |prefs| {
                    prefs.toggle_favourite(executor, model);
                },
                cx,
            );
        });
        cx.notify();
    }

    fn show_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.usage_card_open = false;
        self.picker_tab = tab;
        self.picker_query.clear();
        self.picker_cursor = None;
        cx.notify();
    }

    /// Whether the viewed tab has a search field (Auto has none).
    fn searchable(tab: &Tab) -> bool {
        !matches!(tab, Tab::Auto)
    }

    /// The picker's keyboard: type to search, Up and Down to move over the rows, Enter to pick,
    /// Tab and Shift-Tab to change the tab, Escape to close.
    fn on_picker_key(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        let key = &event.keystroke;
        let view = self.picker_view(cx);
        let rows: Vec<&Row> = view
            .listing
            .rows
            .iter()
            .chain(&view.listing.others)
            .collect();
        match key.key.as_str() {
            "escape" => return self.close_picker(cx),
            "tab" => {
                let at = view
                    .tabs
                    .iter()
                    .position(|t| t.tab == view.tab)
                    .unwrap_or(0);
                let n = view.tabs.len().max(1);
                let next = if key.modifiers.shift {
                    at + n - 1
                } else {
                    at + 1
                } % n;
                self.picker_keyboard = true;
                if let Some(t) = view.tabs.get(next) {
                    self.show_tab(t.tab.clone(), cx);
                }
                return;
            }
            "down" | "up" if !rows.is_empty() => {
                let n = rows.len();
                let down = key.key == "down";
                self.picker_cursor = Some(match self.picker_cursor {
                    None if down => 0,
                    None => n - 1,
                    Some(at) if down => (at + 1) % n,
                    Some(at) => (at + n - 1) % n,
                });
            }
            "enter" => {
                if let Some(row) = self.picker_cursor.and_then(|at| rows.get(at))
                    && row.usable
                {
                    let (harness, model) = (row.harness.clone(), row.model.id.clone());
                    self.choose_model(harness, Some(model), cx);
                }
            }
            "backspace" if Self::searchable(&view.tab) => {
                self.picker_query.pop();
                self.picker_cursor = None;
            }
            _ => {
                let m = key.modifiers;
                if let Some(text) = key.key_char.as_deref()
                    && Self::searchable(&view.tab)
                    && !(m.platform || m.control || m.alt)
                    && !text.chars().any(char::is_control)
                {
                    self.picker_query.push_str(text);
                    self.picker_cursor = None;
                }
            }
        }
        cx.notify();
    }

    /// One model row: its harness's glyph, the name, a check when chosen and a star. `named`
    /// puts the harness's name after the model's (Favourites and other harnesses' matches).
    fn model_row(
        &self,
        theme: &Theme,
        row: &Row,
        chosen: bool,
        named: bool,
        highlighted: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let key = format!("{}/{}", row.harness, row.model.id);
        let selector = format!("pick-model-{}", row.model.id);
        let star_selector = format!("star-{}", row.model.id);
        let group = SharedString::from(format!("row-{key}"));
        let (hover, starred) = (theme.row_selected(), row.starred);
        let (harness, model) = (row.harness.clone(), row.model.id.clone());
        let (star_harness, star_model) = (row.harness.clone(), row.model.id.clone());
        let star = div()
            .id(SharedString::from(format!("star-{key}")))
            .debug_selector(move || star_selector.clone())
            .flex_none()
            .size(px(22.0))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(6.0))
            .cursor_pointer()
            .when(!starred && !highlighted, |s| {
                s.opacity(0.0)
                    .group_hover(group.clone(), |s| s.opacity(1.0))
            })
            .child(if starred {
                icon(Icon::StarFill, 13.0, theme.warning)
            } else {
                icon(Icon::Star, 13.0, theme.faint)
            })
            .on_click(cx.listener(move |this, _, _, cx| {
                cx.stop_propagation();
                this.toggle_star(&star_harness, &star_model, cx);
            }));
        div()
            .id(SharedString::from(key))
            .debug_selector(move || selector.clone())
            .group(group)
            .w_full()
            .h(px(32.0))
            .flex_none()
            .px(px(8.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(7.0))
            .when(highlighted, |r| r.bg(hover))
            .when(row.usable, |r| {
                r.cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.choose_model(harness.clone(), Some(model.clone()), cx);
                    }))
            })
            .when(!row.usable, |r| r.text_color(theme.faint))
            .when_some(row.model.description.clone(), |r, about| {
                r.tooltip(move |_, cx| {
                    let about = SharedString::from(about.clone());
                    cx.new(|_| Tip(about)).into()
                })
            })
            .child(kit::exec_glyph(theme, row.harness.as_str(), 15.0))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(row.model.name.clone())),
            )
            .when(named, |r| {
                r.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.0))
                        .text_color(theme.faint)
                        .child(SharedString::from(row.harness_name.clone())),
                )
            })
            .when(!named, |r| r.child(div().flex_1()))
            .when(chosen, |r| r.child(check(theme)))
            .child(star)
    }

    /// The row that picks a harness's default model, for a harness that listed none.
    fn default_row(
        &self,
        theme: &Theme,
        harness: &HarnessChoice,
        chosen: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let id = harness.id.clone();
        menu_row(
            theme,
            format!("pick-harness-{}", harness.id),
            harness.usable,
        )
        .child(kit::exec_glyph(theme, harness.id.as_str(), 15.0))
        .child(div().min_w_0().truncate().child("Default model"))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(12.0))
                .text_color(theme.faint)
                .child(SharedString::from(format!(
                    "\u{b7} {} reports no list",
                    harness.name
                ))),
        )
        .when(chosen, |r| r.child(check(theme)))
        .when(harness.usable, |r| {
            r.on_click(cx.listener(move |this, _, _, cx| {
                this.choose_model(id.clone(), None, cx);
            }))
        })
    }

    /// The Auto tab: Auto's row, what Reno would pick now, and "why?".
    fn auto_row(&self, theme: &Theme, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
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
            .on_click(cx.listener(|this, _, _, cx| this.choose_auto(cx)))
    }

    /// The glyph tabs: names appear on hover, and on the tab the keyboard is on.
    fn rail(&self, theme: &Theme, view: &View, cx: &mut Context<Self>) -> gpui::Div {
        let tabs = view.tabs.iter().map(|t| {
            let active = t.tab == view.tab;
            let key = t.tab.key();
            let selector = format!("picker-tab-{key}");
            let group = SharedString::from(format!("tab-{key}"));
            let tab = t.tab.clone();
            let glyph = match &t.tab {
                Tab::Favourites => icon(Icon::Star, 16.0, theme.muted).into_any_element(),
                Tab::Auto => icon(Icon::Route, 16.0, theme.muted).into_any_element(),
                Tab::Harness(id) => div()
                    .when(t.empty, |d| d.opacity(0.4))
                    .child(kit::logo(theme, id.as_str(), 16.0))
                    .into_any_element(),
            };
            let (hover, name_shown) = (theme.row_selected(), active && self.picker_keyboard);
            div()
                .id(SharedString::from(selector.clone()))
                .debug_selector(move || selector.clone())
                .group(group.clone())
                .relative()
                .flex_none()
                .size(px(32.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(7.0))
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .child(glyph)
                .when(active, |d| {
                    d.child(
                        div()
                            .absolute()
                            .bottom(px(-4.0))
                            .left(px(6.0))
                            .right(px(6.0))
                            .h(px(2.0))
                            .rounded(px(1.0))
                            .bg(theme.accent),
                    )
                })
                .when(t.signed_out, |d| {
                    d.child(
                        kit::dot(theme.warning, 6.0)
                            .debug_selector({
                                let id = format!("picker-tab-warn-{key}");
                                move || id.clone()
                            })
                            .absolute()
                            .top(px(3.0))
                            .right(px(3.0)),
                    )
                })
                .child(
                    div()
                        .id(SharedString::from(format!("picker-tab-name-{key}")))
                        .absolute()
                        .top(px(36.0))
                        .left_0()
                        .px(px(7.0))
                        .py(px(3.0))
                        .rounded(px(6.0))
                        .bg(theme.raised)
                        .border_1()
                        .border_color(theme.hairline)
                        .text_size(px(11.5))
                        .whitespace_nowrap()
                        .text_color(theme.text)
                        .opacity(if name_shown { 1.0 } else { 0.0 })
                        .group_hover(group, |s| s.opacity(1.0))
                        .child(SharedString::from(t.name.clone())),
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.picker_keyboard = false;
                    this.show_tab(tab.clone(), cx);
                }))
        });
        div()
            .flex_none()
            .h(px(40.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .gap(px(2.0))
            .border_b_1()
            .border_color(theme.hairline)
            .children(tabs)
    }

    /// The search field, scoped to the viewed tab. The caret is a bar after the text.
    fn search_field(
        &self,
        theme: &Theme,
        view: &View,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let placeholder = match &view.tab {
            Tab::Favourites => "Search favourites\u{2026}".to_owned(),
            Tab::Auto => String::new(),
            Tab::Harness(id) => {
                let name = view
                    .harnesses
                    .iter()
                    .find(|h| &h.id == id)
                    .map_or_else(|| id.to_string(), |h| h.name.clone());
                format!("Search {name} models\u{2026}")
            }
        };
        div()
            .id("picker-search")
            .debug_selector(|| "picker-search".to_owned())
            .flex_none()
            .h(px(38.0))
            .px(px(14.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .text_size(px(13.0))
            .child(icon(Icon::Search, 13.0, theme.faint))
            .child(if self.picker_query.is_empty() {
                div()
                    .flex_1()
                    .truncate()
                    .text_color(theme.faint)
                    .child(SharedString::from(placeholder))
            } else {
                div()
                    .flex_1()
                    .truncate()
                    .text_color(theme.text)
                    .child(SharedString::from(self.picker_query.clone()))
            })
            .on_click(cx.listener(|this, _, window, cx| {
                window.focus(&this.picker_focus, cx);
            }))
    }

    /// The list under the rail for the viewed tab.
    fn picker_list(
        &self,
        theme: &Theme,
        view: &View,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let chosen = |h: &ExecutorId, m: Option<&str>| {
            view.shown
                .as_ref()
                .is_some_and(|p| &p.executor == h && p.model.as_deref() == m)
                && (self.pick.is_some() || view.live.is_some())
        };
        let list = div()
            .id("picker-list")
            .debug_selector(|| "picker-list".to_owned())
            .max_h(px(LIST_MAX))
            .p(px(6.0))
            .flex()
            .flex_col()
            .overflow_y_scroll();
        if view.tab == Tab::Auto {
            return list.child(self.auto_row(theme, cx));
        }
        let named = view.tab == Tab::Favourites;
        let note = |text: String| {
            div()
                .px(px(10.0))
                .py(px(14.0))
                .text_size(px(12.5))
                .text_color(theme.faint)
                .child(SharedString::from(text))
        };
        let mut list = list;
        let mut at = 0;
        if let Tab::Harness(id) = &view.tab
            && let Some(h) = view.harnesses.iter().find(|h| &h.id == id)
            && h.models.is_empty()
            && view.live.is_none()
            && self.picker_query.is_empty()
        {
            list = list.child(self.default_row(theme, h, chosen(&h.id, None), cx));
        }
        for row in &view.listing.rows {
            let is_chosen = chosen(&row.harness, Some(&row.model.id));
            list = list.child(self.model_row(
                theme,
                row,
                is_chosen,
                named,
                self.picker_cursor == Some(at),
                cx,
            ));
            at += 1;
        }
        if view.listing.rows.is_empty() && view.listing.others.is_empty() {
            let text = match (&view.tab, self.picker_query.is_empty()) {
                (Tab::Favourites, true) => "Star a model in any harness to keep it here.",
                (Tab::Harness(_), true) if view.live.is_some() => "No models listed.",
                (Tab::Harness(_), true) => "",
                _ => "No models match.",
            };
            if !text.is_empty() {
                list = list.child(note(text.to_owned()));
            }
        }
        if !view.listing.others.is_empty() {
            list = list.child(
                div()
                    .px(px(10.0))
                    .pt(px(8.0))
                    .pb(px(2.0))
                    .text_size(px(11.5))
                    .text_color(theme.faint)
                    .child("In other harnesses"),
            );
            for row in &view.listing.others {
                let is_chosen = chosen(&row.harness, Some(&row.model.id));
                list = list.child(self.model_row(
                    theme,
                    row,
                    is_chosen,
                    true,
                    self.picker_cursor == Some(at),
                    cx,
                ));
                at += 1;
            }
        }
        list
    }

    /// The harness the footer speaks about: the viewed tab's, else the shown pick's.
    pub(super) fn footer_harness(view: &View) -> Option<&HarnessChoice> {
        let id = match &view.tab {
            Tab::Harness(id) => Some(id),
            _ => view.shown.as_ref().map(|p| &p.executor),
        }?;
        view.harnesses.iter().find(|h| &h.id == id)
    }

    /// The Account row: who is signed in to the harness, and a way to the Executors screen.
    fn account_row(
        &self,
        theme: &Theme,
        harness: &HarnessChoice,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let (words, tone): (String, Hsla) = match &harness.account {
            Account::SignedIn(Some(who)) => (who.clone(), theme.muted),
            Account::SignedIn(None) => ("signed in".to_owned(), theme.muted),
            Account::SignedOut => ("signed out".to_owned(), theme.warning),
            Account::NotReported => ("not reported".to_owned(), theme.faint),
        };
        let row = menu_row(theme, "picker-account".to_owned(), true)
            .child(div().flex_none().text_color(theme.muted).child("Account"))
            .child(
                div()
                    .id("picker-account-value")
                    .debug_selector(|| "picker-account-value".to_owned())
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_right()
                    .text_size(px(12.0))
                    .text_color(tone)
                    .child(SharedString::from(words)),
            )
            .children(self.usage_ring(theme, harness, cx))
            .child(icon(Icon::Fwd, 12.0, theme.faint))
            .on_click(cx.listener(|this, _, _, cx| {
                this.close_picker(cx);
                cx.emit(OpenExecutors);
            }));
        let meters = harness
            .usage
            .as_ref()
            .filter(|_| self.usage_card_open && matches!(harness.account, Account::SignedIn(_)))
            .map(|usage| {
                let now = self.store.read(cx).now_ms();
                div()
                    .id("picker-usage-meters")
                    .debug_selector(|| "picker-usage-meters".to_owned())
                    .px(px(10.0))
                    .pb(px(6.0))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .text_size(px(12.0))
                    .text_color(theme.muted)
                    .children(usage.plan_label.clone().map(|plan| {
                        div()
                            .text_color(theme.faint)
                            .child(SharedString::from(plan))
                    }))
                    .children(usage.windows.iter().map(|w| {
                        div().child(SharedString::from(context::window_line(
                            w,
                            now,
                            &chrono::Local,
                        )))
                    }))
            });
        div().flex().flex_col().child(row).children(meters)
    }

    /// Whether a live run's effort and options can be changed: the daemon has `run.configure`.
    /// Where the executor does not apply a change live, the daemon stores it for the next launch.
    pub(super) fn live_configurable(&self, cx: &App) -> bool {
        self.store
            .read(cx)
            .state()
            .supports(reno_proto::FEATURE_RUN_CONFIGURE)
    }

    /// The Context window row, when the shown model offers a choice of sizes: the size chosen
    /// (a click lists the sizes), or on a live run the live fill, with when a change applies
    /// under it. An option the harness does not let Reno set shows its size and nothing to
    /// click.
    fn context_row(&self, theme: &Theme, view: &View, cx: &mut Context<Self>) -> Option<gpui::Div> {
        let shown = view.shown.as_ref()?;
        let state = self.store.read(cx).state();
        let model = models::model(state, &shown.executor, shown.model.as_deref()?)?;
        let option = context::window_option(&model)?;
        let chosen = shown.options.get(&option.id).map(String::as_str);
        let label = context::chosen_label(option, chosen);
        let live = self.live_run(cx);
        let configurable = live.is_none() || self.live_configurable(cx);
        let interactive = option.settable && configurable;
        let harness = view.harnesses.iter().find(|h| h.id == shown.executor);
        let value = live
            .and_then(|run| context::live_words(models::context_usage(state, &run.run_id)))
            .unwrap_or_else(|| label.clone());
        let open = self.picker_window_open && interactive;
        let name = harness.map_or_else(String::new, |h| h.name.clone());
        let row = menu_row(theme, "picker-context".to_owned(), interactive)
            .child(
                div()
                    .flex_none()
                    .text_color(theme.muted)
                    .child("Context window"),
            )
            .child(
                div()
                    .id("picker-context-value")
                    .debug_selector(|| "picker-context-value".to_owned())
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_right()
                    .text_size(px(12.0))
                    .text_color(theme.text)
                    .child(SharedString::from(value)),
            )
            .when(!option.settable, |r| {
                let words = if shown.executor.as_str() == "acp.grok" {
                    "Set in Grok's own settings (/context-window)".to_owned()
                } else {
                    format!("Set in {name}'s own settings")
                };
                r.tooltip(move |_, cx| {
                    let words = SharedString::from(words.clone());
                    cx.new(|_| Tip(words)).into()
                })
            })
            .when(interactive, |r| {
                r.child(icon(
                    if open { Icon::ChevDown } else { Icon::Fwd },
                    12.0,
                    theme.faint,
                ))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.picker_window_open = !this.picker_window_open;
                    cx.notify();
                }))
            });
        let mut section = div().flex().flex_col().child(row);
        if let Some(run) = live
            && option.settable
        {
            let current = context::chosen_choice(option, chosen);
            let when = if configurable {
                let timing = self
                    .store
                    .read(cx)
                    .config_applied(&run.run_id)
                    .options
                    .unwrap_or_else(|| {
                        context::window_timing(harness.and_then(|h| h.live_options), current)
                    });
                context::timing_words(timing)
            } else {
                "next run"
            };
            section = section.child(
                div()
                    .id("context-applies")
                    .debug_selector(|| "context-applies".to_owned())
                    .px(px(10.0))
                    .pb(px(4.0))
                    .text_size(px(11.5))
                    .text_color(theme.faint)
                    .child(SharedString::from(format!("{label} \u{b7} {when}"))),
            );
        }
        if open {
            let current = context::chosen_choice(option, chosen).to_owned();
            section = section.children(option.choices.iter().map(|choice| {
                let (id, name) = (choice.id.clone(), choice.label.clone());
                let selected = choice.id == current;
                menu_row(theme, format!("context-choice-{}", choice.id), true)
                    .pl(px(22.0))
                    .child(div().flex_1().child(SharedString::from(name)))
                    .when(selected, |r| r.child(check(theme)))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.choose_option(MODEL_OPTION_CONTEXT_WINDOW, &id, cx);
                    }))
            }));
        }
        Some(section)
    }

    /// EFFORT and its track: one cell per level the shown model lists.
    fn effort_section(
        &self,
        theme: &Theme,
        view: &View,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let live = view.live.is_some();
        // Changeable unless it is a live run whose daemon cannot take a change (`run.configure`).
        let editable = !live || self.live_configurable(cx);
        let state = self.store.read(cx).state();
        let shown = view.shown.as_ref();
        let model = shown.and_then(|p| {
            let id = p.model.as_deref()?;
            Some((id, models::model(state, &p.executor, id)))
        });
        let (levels, default) = model
            .as_ref()
            .and_then(|(_, m)| m.as_ref())
            .map_or((Vec::new(), None), |m| {
                (m.efforts.clone(), m.default_effort)
            });
        let chosen = shown.and_then(|p| p.effort);
        let word = if levels.is_empty() {
            String::new()
        } else {
            let mut w = picker::effort_words(chosen, default);
            if live && !editable {
                w.push_str(" \u{b7} next run");
            }
            w
        };
        let body: gpui::AnyElement =
            match (&model, levels.is_empty()) {
                (None, _) => div()
                    .text_size(px(12.0))
                    .text_color(theme.faint)
                    .child("pick a model to set it")
                    .into_any_element(),
                (Some((id, listed)), true) => {
                    let name = listed.as_ref().map_or_else(
                        || {
                            shown.map_or_else(
                                || (*id).to_owned(),
                                |p| models::model_name(state, &p.executor, id),
                            )
                        },
                        |m| m.name.clone(),
                    );
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.faint)
                        .child(SharedString::from(format!("{name} has no levels")))
                        .into_any_element()
                }
                (Some(_), false) => {
                    let cells = picker::effort_cells(&levels, chosen, default);
                    div()
                        .flex()
                        .gap(px(3.0))
                        .when(!editable, |d| d.opacity(0.5))
                        .children(cells.into_iter().map(|cell| {
                            let effort = cell.effort;
                            let id = format!("effort-cell-{}", effort.as_str());
                            let selector = id.clone();
                            let (accent, faint) = (theme.accent, theme.faint);
                            div()
                                .id(SharedString::from(id))
                                .debug_selector(move || selector.clone())
                                .flex_1()
                                .h(px(26.0))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(px(5.0))
                                .border_1()
                                .font_family(kit::MONO)
                                .text_size(px(11.0))
                                .when(cell.filled, |c| {
                                    c.bg(accent.opacity(0.18 + 0.62 * cell.strength))
                                        .border_color(accent.opacity(0.0))
                                        .text_color(theme.text)
                                })
                                .when(!cell.filled, |c| {
                                    c.bg(theme.row_selected())
                                        .text_color(theme.muted)
                                        .border_color(if cell.default {
                                            accent
                                        } else {
                                            faint.opacity(0.0)
                                        })
                                })
                                .when(editable, |c| {
                                    c.cursor_pointer().on_click(cx.listener(
                                        move |this, _, _, cx| this.click_effort(effort, cx),
                                    ))
                                })
                                .child(effort_word(effort))
                        }))
                        .into_any_element()
                }
            };
        // Under the track on a live run: when a change takes hold, as the daemon last answered,
        // else as the executor says it applies one.
        let applies = (live && editable && !levels.is_empty())
            .then(|| {
                let run = self.live_run(cx)?;
                let timing = self
                    .store
                    .read(cx)
                    .config_applied(&run.run_id)
                    .effort
                    .or_else(|| {
                        view.harnesses
                            .iter()
                            .find(|h| h.id == run.executor)
                            .and_then(|h| h.live_effort)
                    })
                    .unwrap_or(reno_proto::ConfigTiming::AfterRestart);
                Some(
                    div()
                        .id("effort-applies")
                        .debug_selector(|| "effort-applies".to_owned())
                        .text_size(px(11.5))
                        .text_color(theme.faint)
                        .child(SharedString::from(context::timing_words(timing))),
                )
            })
            .flatten();
        div()
            .id("picker-effort")
            .debug_selector(|| "picker-effort".to_owned())
            .px(px(10.0))
            .pt(px(6.0))
            .pb(px(8.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(px(10.0))
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme.faint)
                            .child("EFFORT"),
                    )
                    .child(
                        div()
                            .id("effort-word")
                            .debug_selector(|| "effort-word".to_owned())
                            .text_size(px(12.0))
                            .text_color(theme.text)
                            .child(SharedString::from(word)),
                    ),
            )
            .child(body)
            .children(applies)
    }

    /// The picker's popover content: rail, search, list and the footer.
    pub(super) fn picker(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let view = self.picker_view(cx);
        let switch_note = view
            .live
            .as_ref()
            .and_then(|l| view.harnesses.iter().find(|h| &h.id == l))
            .is_some_and(|h| h.switches)
            .then(|| {
                div()
                    .px(px(16.0))
                    .pb(px(6.0))
                    .text_size(px(12.0))
                    .text_color(theme.muted)
                    .child("Switching keeps the conversation; it applies from its next step.")
            });
        let account = Self::footer_harness(&view).map(|h| self.account_row(theme, h, cx));
        menu(theme, "model-picker", WIDTH)
            .p_0()
            .key_context("ModelPicker")
            .track_focus(&self.picker_focus)
            .on_key_down(cx.listener(Self::on_picker_key))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| window.focus(&this.picker_focus, cx)),
            )
            .child(self.rail(theme, &view, cx))
            .when(Self::searchable(&view.tab), |p| {
                p.child(self.search_field(theme, &view, cx))
            })
            .child(self.picker_list(theme, &view, cx))
            .children(switch_note)
            .child(
                div()
                    .flex_none()
                    .border_t_1()
                    .border_color(theme.hairline)
                    .p(px(4.0))
                    .flex()
                    .flex_col()
                    .children(account)
                    .children(self.context_row(theme, &view, cx))
                    .child(self.effort_section(theme, &view, cx)),
            )
    }
}

/// The short word a cell shows: `med` for medium, `xhigh` and `max` as the harness says them.
fn effort_word(effort: Effort) -> &'static str {
    match effort {
        Effort::Minimal => "min",
        Effort::Medium => "med",
        other => other.as_str(),
    }
}
