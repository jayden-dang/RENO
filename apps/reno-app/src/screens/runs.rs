//! Runs: the live window from `runs.watch` plus older pages from `run.list`, narrowed by a
//! status filter. Choosing a row opens the run's transcript.

use std::rc::Rc;

use gpui::{
    AnyElement, Context, Entity, EventEmitter, FontWeight, IntoElement, Render, SharedString,
    Subscription, Window, div, prelude::*, px,
};

use super::ScreenEvent;
use crate::store::Store;
use crate::summary;
use crate::ui::kit::{self, Tone};
use crate::ui::page;
use crate::ui::theme::{self, Theme};
use crate::vm::runs::{self, More, RunFilter, RunRow, RunsView};

pub(crate) struct RunsScreen {
    store: Entity<Store>,
    filter: RunFilter,
    _store: Subscription,
}

impl EventEmitter<ScreenEvent> for RunsScreen {}

impl RunsScreen {
    pub(crate) fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        Self {
            _store: cx.observe(&store, |_, _, cx| cx.notify()),
            store,
            filter: RunFilter::All,
        }
    }

    /// Called when the screen is shown: reads the first page of the current filter.
    pub(crate) fn shown(&self, cx: &mut Context<Self>) {
        self.load_more(cx);
    }

    pub(crate) fn set_filter(&mut self, filter: RunFilter, cx: &mut Context<Self>) {
        self.filter = filter;
        cx.notify();
        self.load_more(cx);
    }

    /// Reads the next page (the first, when none was read) for the current filter.
    pub(crate) fn load_more(&self, cx: &mut Context<Self>) {
        let statuses = self.filter.statuses();
        self.store
            .update(cx, |store, cx| store.load_runs_page(statuses, cx));
    }

    pub(crate) fn view(&self, cx: &gpui::App) -> RunsView {
        let store = self.store.read(cx);
        runs::build(
            store.state(),
            self.filter,
            store.is_paging(self.filter.statuses()),
            store.now_ms(),
        )
    }

    fn chips(&self, view: &RunsView, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_wrap()
            .gap(px(8.0))
            .children(view.counts.iter().map(|(filter, n)| {
                let filter = *filter;
                let on = filter == view.filter;
                kit::chip(theme, format!("{} {n}", filter.label()))
                    .id(SharedString::from(format!("filter-{}", filter.label())))
                    .debug_selector(move || format!("filter-{}", filter.label()))
                    .cursor_pointer()
                    .when(on, |c| c.bg(theme.raised).font_weight(FontWeight::MEDIUM))
                    .on_click(cx.listener(move |this, _, _, cx| this.set_filter(filter, cx)))
            }))
            .into_any_element()
    }

    fn row(row: &RunRow, last: bool, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let run = row.run.clone();
        let open = cx.listener(move |_, _, _, cx| cx.emit(ScreenEvent::OpenRun(run.clone())));
        let sub = [row.repo.clone(), Some(row.run.to_string())]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" \u{b7} ");
        page::table_row(theme, last)
            .id(SharedString::from(format!("run-row-{}", row.run)))
            .debug_selector({
                let id = format!("run-row-{}", row.run);
                move || id.clone()
            })
            .cursor_pointer()
            .hover({
                let hover = theme.raised.opacity(0.4);
                move |s| s.bg(hover)
            })
            .on_click(open)
            .child(
                div()
                    .map(|d| page::cell(d, None))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(
                        div()
                            .truncate()
                            .font_weight(FontWeight::MEDIUM)
                            .child(row.what.clone()),
                    )
                    .child(
                        div()
                            .truncate()
                            .text_size(px(11.5))
                            .text_color(theme.faint)
                            .child(sub),
                    ),
            )
            .child(
                div()
                    .map(|d| page::cell(d, Some(110.0)))
                    .truncate()
                    .text_color(theme.muted)
                    .child(row.bot.clone().unwrap_or_default()),
            )
            .child(
                div()
                    .map(|d| page::cell(d, Some(130.0)))
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .overflow_hidden()
                    .child(kit::exec_glyph(theme, &row.executor, 14.0))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .child(summary::executor_name(&row.executor).to_owned()),
                    ),
            )
            .child(
                div()
                    .map(|d| page::cell(d, Some(120.0)))
                    .child(kit::status_pill(theme, row.status)),
            )
            .child(
                div()
                    .map(|d| page::cell(d, Some(90.0)))
                    .truncate()
                    .text_color(theme.muted)
                    .child(row.started.clone()),
            )
            .child(
                div()
                    .map(|d| page::cell(d, Some(70.0)))
                    .truncate()
                    .text_color(theme.muted)
                    .child(row.took.clone()),
            )
            .into_any_element()
    }
}

impl Render for RunsScreen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let view = self.view(cx);
        let retry: page::Action = {
            let store = self.store.clone();
            Rc::new(move |_, cx| store.update(cx, |s, _| s.retry_now()))
        };
        let total = view.counts.first().map_or(0, |(_, n)| *n);
        let detail = match total {
            0 => "No runs yet.".to_owned(),
            1 => "1 run known.".to_owned(),
            n => format!("{n} runs known."),
        };

        let rows = view.rows.rows();
        let table_rows: Vec<AnyElement> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| Self::row(r, i + 1 == rows.len(), &theme, cx))
            .collect();
        let empty = match view.filter {
            RunFilter::All => "No runs yet. Start one from Home.",
            _ => "No runs match this filter.",
        };

        let more = match view.more {
            More::None if !rows.is_empty() => Some(
                div()
                    .text_size(px(12.5))
                    .text_color(theme.faint)
                    .child("That is every run.")
                    .into_any_element(),
            ),
            More::Available => Some(
                kit::pill_button(&theme, "load-older", "Load older runs", Tone::Quiet)
                    .on_click(cx.listener(|this, _, _, cx| this.load_more(cx)))
                    .into_any_element(),
            ),
            More::Loading => Some(
                div()
                    .text_size(px(12.5))
                    .text_color(theme.muted)
                    .child("Loading older runs…")
                    .into_any_element(),
            ),
            More::None => None,
        };

        let body = page::content(page::WIDE)
            .when_some(view.banner.as_ref(), |c, b| {
                c.child(page::banner(&theme, b, Some(retry)))
            })
            .child(page::lede(&theme, "Runs", detail))
            .child(self.chips(&view, &theme, cx))
            .when_some(page::note(&theme, &view.rows, empty), ParentElement::child)
            .when(!table_rows.is_empty(), |c| {
                c.child(
                    page::table_frame(&theme, page::table())
                        .child(page::table_head(
                            &theme,
                            &[
                                ("Run", None),
                                ("Bot", Some(110.0)),
                                ("Executor", Some(130.0)),
                                ("State", Some(120.0)),
                                ("Started", Some(90.0)),
                                ("Took", Some(70.0)),
                            ],
                        ))
                        .children(table_rows),
                )
            })
            .when_some(more, ParentElement::child);

        div()
            .id("runs")
            .size_full()
            .flex()
            .flex_col()
            .child(page::header(&theme, "Runs", div()))
            .child(page::column("runs-scroll").child(body))
    }
}
