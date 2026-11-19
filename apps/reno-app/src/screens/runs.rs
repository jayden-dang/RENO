//! All runs: one lane per live run, opened from the bar's "N live" and from ⌘K (app-shell brief,
//! "Side panel and All runs"). A lane has a status stripe at its edge, the executor, the title, a
//! live one-line tail, an "asks" badge when the run waits for you, and its state and age, which
//! never wrap. Choosing a lane opens the run.

use std::rc::Rc;

use gpui::{
    AnyElement, Context, Entity, EventEmitter, FontWeight, IntoElement, Render, Role, SharedString,
    Subscription, Window, div, prelude::*, px,
};

use super::ScreenEvent;
use crate::store::Store;
use crate::ui::kit::{self, MONO, Tone};
use crate::ui::page;
use crate::ui::theme::{self, Theme};
use crate::vm::finished::{self, More};
use crate::vm::lanes::{self, Lane};

/// The lane's fixed columns: the title, the state in words and the age.
const TITLE_W: f32 = 230.0;
const STATUS_W: f32 = 96.0;
const TIME_W: f32 = 44.0;

pub(crate) struct RunsScreen {
    store: Entity<Store>,
    _store: Subscription,
}

impl EventEmitter<ScreenEvent> for RunsScreen {}

impl RunsScreen {
    pub(crate) fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        Self {
            _store: cx.observe(&store, |_, _, cx| cx.notify()),
            store,
        }
    }

    /// Called when the screen is shown.
    pub(crate) fn shown(&self, cx: &mut Context<Self>) {
        self.store
            .update(cx, |store, cx| store.load_finished_page(cx));
    }

    #[cfg(test)]
    pub(crate) fn lanes(&self, cx: &gpui::App) -> Vec<Lane> {
        let store = self.store.read(cx);
        lanes::lanes(store.state(), store.now_ms())
    }

    fn lane(lane: &Lane, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let run = lane.run.clone();
        let stripe = kit::run_color(theme, lane.status, None);
        let hover = theme.raised.opacity(0.4);
        let spoken = match lane.asks {
            Some(asks) => format!("{}, {}", lane.title, asks.label()),
            None => format!("{}, {}", lane.title, lane.status_label),
        };
        div()
            .id(SharedString::from(format!("lane-{}", lane.run)))
            .debug_selector({
                let id = format!("lane-{}", lane.run);
                move || id.clone()
            })
            .flex()
            .items_center()
            .gap(px(10.0))
            .h(px(36.0))
            .pr(px(12.0))
            .rounded(px(7.0))
            .bg(theme.surface)
            .border_1()
            .border_color(theme.hairline)
            .overflow_hidden()
            .cursor_pointer()
            .role(Role::Button)
            .aria_label(SharedString::from(spoken))
            .focusable()
            .tab_stop(true)
            .focus_visible(kit::focus_ring(theme))
            .hover(move |s| s.bg(hover))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(ScreenEvent::OpenRun(run.clone()))))
            .child(
                div()
                    .id(SharedString::from(format!("lane-stripe-{}", lane.run)))
                    .debug_selector({
                        let id = format!("lane-stripe-{}", lane.run);
                        move || id.clone()
                    })
                    .w(px(3.0))
                    .h_full()
                    .flex_none()
                    .bg(stripe),
            )
            .child(kit::exec_glyph(theme, &lane.executor, 14.0))
            .child(
                div()
                    .w(px(TITLE_W))
                    .flex_none()
                    .truncate()
                    .text_size(px(13.0))
                    .font_weight(FontWeight::MEDIUM)
                    .child(lane.title.clone()),
            )
            .child(
                div()
                    .id(SharedString::from(format!("lane-tail-{}", lane.run)))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .whitespace_nowrap()
                    .font_family(MONO)
                    .text_size(px(12.0))
                    .text_color(theme.muted)
                    .child(lane.tail.clone().unwrap_or_default()),
            )
            .children(lane.asks.map(|asks| {
                let tone = match asks {
                    crate::vm::projects::Badge::NeedsInput => theme.warning,
                    crate::vm::projects::Badge::NeedsApproval => theme.success,
                };
                kit::tag(tone, asks.label()).whitespace_nowrap()
            }))
            .child(
                // The badge already says a waiting state; the column keeps its width.
                div()
                    .w(px(STATUS_W))
                    .flex_none()
                    .whitespace_nowrap()
                    .text_size(px(12.5))
                    .text_color(kit::run_color(theme, lane.status, None))
                    .children(lane.status_word),
            )
            .child(
                div()
                    .w(px(TIME_W))
                    .flex_none()
                    .flex()
                    .justify_end()
                    .whitespace_nowrap()
                    .font_family(MONO)
                    .text_size(px(12.0))
                    .text_color(theme.faint)
                    .child(lane.time.clone()),
            )
            .into_any_element()
    }
}

impl RunsScreen {
    /// A finished run: the same height as a lane, no stripe, a muted state word and age.
    fn finished_row(row: &finished::Row, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let run = row.run.clone();
        let id = format!("finished-{}", row.run);
        let hover = theme.raised.opacity(0.4);
        div()
            .id(SharedString::from(id.clone()))
            .debug_selector(move || id.clone())
            .flex()
            .items_center()
            .gap(px(10.0))
            .h(px(36.0))
            .px(px(13.0))
            .rounded(px(7.0))
            .cursor_pointer()
            .role(Role::Button)
            .aria_label(SharedString::from(format!(
                "{}, {}",
                row.title, row.status_label
            )))
            .focusable()
            .tab_stop(true)
            .focus_visible(kit::focus_ring(theme))
            .hover(move |s| s.bg(hover))
            .on_click(cx.listener(move |_, _, _, cx| cx.emit(ScreenEvent::OpenRun(run.clone()))))
            .child(kit::exec_glyph(theme, &row.executor, 14.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.0))
                    .text_color(theme.muted)
                    .child(row.title.clone()),
            )
            .child(
                div()
                    .w(px(STATUS_W))
                    .flex_none()
                    .whitespace_nowrap()
                    .text_size(px(12.5))
                    .text_color(theme.faint)
                    .child(row.status_label),
            )
            .child(
                div()
                    .w(px(TIME_W))
                    .flex_none()
                    .flex()
                    .justify_end()
                    .whitespace_nowrap()
                    .font_family(MONO)
                    .text_size(px(12.0))
                    .text_color(theme.faint)
                    .child(row.time.clone()),
            )
            .into_any_element()
    }
}

impl Render for RunsScreen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let (banner, lanes, ended) = {
            let store = self.store.read(cx);
            (
                crate::vm::banner(store.state()),
                lanes::lanes(store.state(), store.now_ms()),
                finished::build(store.state(), store.is_paging(), store.now_ms()),
            )
        };
        let retry: page::Action = {
            let store = self.store.clone();
            Rc::new(move |_, cx| store.update(cx, |s, _| s.retry_now()))
        };
        let detail = match lanes.len() {
            0 => "Nothing is running.".to_owned(),
            1 => "1 live".to_owned(),
            n => format!("{n} live"),
        };
        let asking = lanes.iter().filter(|l| l.asks.is_some()).count();
        let detail = if asking > 0 {
            format!("{detail}, {asking} waiting for you")
        } else {
            detail
        };
        let empty = lanes.is_empty() && ended.rows.is_empty() && banner.is_none();

        let body = page::content(page::WIDE)
            .when_some(banner.as_ref(), |c, b| {
                c.child(page::banner(&theme, b, Some(retry)))
            })
            .child(page::lede(&theme, "All runs", detail))
            .when(empty, |c| {
                c.child(
                    div()
                        .id("no-live-runs")
                        .debug_selector(|| "no-live-runs".to_owned())
                        .text_size(px(13.0))
                        .text_color(theme.faint)
                        .child("No live runs. Start a task from the sidebar."),
                )
            })
            .child(
                div()
                    .id("lanes")
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .children(lanes.iter().map(|l| Self::lane(l, &theme, cx))),
            )
            .when(!ended.rows.is_empty(), |c| {
                c.child(
                    div()
                        .id("finished")
                        .flex()
                        .flex_col()
                        .gap(px(2.0))
                        .child(
                            div()
                                .pt(px(10.0))
                                .pb(px(4.0))
                                .text_size(px(11.5))
                                .text_color(theme.faint)
                                .child("Finished"),
                        )
                        .children(ended.rows.iter().map(|r| Self::finished_row(r, &theme, cx))),
                )
            })
            .when(ended.more != More::None, |c| {
                let loading = ended.more == More::Loading;
                c.child(
                    kit::pill_button_enabled(
                        &theme,
                        "show-older",
                        if loading {
                            "Loading\u{2026}"
                        } else {
                            "Show older"
                        },
                        Tone::Quiet,
                        !loading,
                    )
                    .when(!loading, |b| {
                        b.on_click(cx.listener(|this, _, _, cx| this.shown(cx)))
                    }),
                )
            });

        div()
            .id("runs")
            .size_full()
            .flex()
            .flex_col()
            .child(page::column("runs-scroll").child(body))
    }
}
