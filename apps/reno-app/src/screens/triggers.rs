//! Triggers: each trigger's kind, schedule, next due, last firing and its run's outcome, and
//! whether it is backing off. "Fire now" is `trigger.fire`. Live through `triggers.watch`.

use std::rc::Rc;

use gpui::{
    AnyElement, Context, Entity, EventEmitter, FontWeight, IntoElement, Render, SharedString,
    Subscription, Window, div, prelude::*, px,
};
use reno_proto::TriggerStatus;

use super::ScreenEvent;
use crate::store::Store;
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO, Tone};
use crate::ui::page;
use crate::ui::theme::{self, Theme};
use crate::vm::triggers::{self, TriggerRow, TriggersView};

pub(crate) struct TriggersScreen {
    store: Entity<Store>,
    _store: Subscription,
}

impl EventEmitter<ScreenEvent> for TriggersScreen {}

impl TriggersScreen {
    pub(crate) fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        Self {
            _store: cx.observe(&store, |_, _, cx| cx.notify()),
            store,
        }
    }

    pub(crate) fn view(&self, cx: &gpui::App) -> TriggersView {
        let store = self.store.read(cx);
        triggers::build(store.state(), store.now_ms())
    }

    pub(crate) fn fire(&mut self, name: &str, cx: &mut Context<Self>) {
        let name = name.to_owned();
        self.store
            .update(cx, |store, cx| store.fire_trigger(name, cx));
    }

    fn kind_icon(kind: &str) -> Icon {
        match kind {
            "fs" => Icon::Folder,
            "manual" => Icon::Bolt,
            "every" => Icon::History,
            _ => Icon::Clock,
        }
    }

    fn row(
        &self,
        row: &TriggerRow,
        last: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let firing = self.store.read(cx).is_firing(&row.name);
        let name = row.name.clone();
        let fire = cx.listener(move |this, _, _, cx| this.fire(&name, cx));
        let live = row.can_fire && !firing;
        let button = kit::pill_button_enabled(
            theme,
            format!("fire-{}", row.name),
            if firing { "Firing…" } else { "Fire now" },
            Tone::Quiet,
            live,
        );
        let button = if live { button.on_click(fire) } else { button };

        let state_color = match row.status {
            TriggerStatus::BackedOff => theme.warning,
            TriggerStatus::Failed => theme.danger,
            TriggerStatus::Disabled | TriggerStatus::Done => theme.faint,
            _ => theme.muted,
        };
        let last_cell = {
            let mut cell = div().flex().flex_col().gap(px(2.0));
            cell = match &row.last_fired {
                Some(ago) => cell.child(div().text_color(theme.muted).child(ago.clone())),
                None => cell.child(div().text_color(theme.faint).child("never")),
            };
            if let Some(last) = &row.last_run {
                let run = last.run.clone();
                let status = match last.status {
                    Some(status) => {
                        kit::status_pill(theme, status, last.problem).into_any_element()
                    }
                    None => div()
                        .text_size(px(12.0))
                        .text_color(theme.faint)
                        .child("run unknown")
                        .into_any_element(),
                };
                cell = cell.child(
                    div()
                        .id(SharedString::from(format!("last-run-{}", row.name)))
                        .text_size(px(12.0))
                        .cursor_pointer()
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(ScreenEvent::OpenRun(run.clone()));
                        }))
                        .child(status),
                );
            }
            cell
        };

        page::table_row(theme, last)
            .id(SharedString::from(format!("trigger-{}", row.name)))
            .items_start()
            .child(
                div()
                    .map(|d| page::cell(d, Some(190.0)))
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(icon(Self::kind_icon(&row.kind), 14.0, theme.muted))
                            .child(
                                div()
                                    .font_weight(FontWeight::MEDIUM)
                                    .truncate()
                                    .child(row.name.clone()),
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(
                                kit::chip(theme, row.kind.clone())
                                    .h(px(20.0))
                                    .text_size(px(11.5)),
                            )
                            .child(
                                div()
                                    .text_size(px(12.0))
                                    .text_color(theme.faint)
                                    .child(format!("\u{2192} {}", row.bot)),
                            ),
                    ),
            )
            .child(
                div()
                    .map(|d| page::cell(d, None))
                    .font_family(MONO)
                    .text_size(px(12.0))
                    .line_height(px(18.0))
                    .flex()
                    .flex_col()
                    .child(if row.schedule.is_empty() {
                        SharedString::from("\u{2026}")
                    } else {
                        SharedString::from(row.schedule.clone())
                    })
                    .when_some(row.cwd.clone(), |d, cwd| {
                        d.child(
                            div()
                                .truncate()
                                .text_color(theme.faint)
                                .child(format!("in {cwd}")),
                        )
                    }),
            )
            .child(
                div()
                    .map(|d| page::cell(d, Some(110.0)))
                    .text_color(if row.status == TriggerStatus::BackedOff {
                        theme.warning
                    } else {
                        theme.muted
                    })
                    .child(row.next.clone().unwrap_or_else(|| "\u{2014}".to_owned())),
            )
            .child(page::cell(div(), Some(130.0)).child(last_cell))
            .child(
                div()
                    .map(|d| page::cell(d, Some(190.0)))
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .child(div().text_color(state_color).child(row.status_label))
                    .when_some(row.backoff.clone(), |d, text| {
                        d.child(
                            div()
                                .text_size(px(12.0))
                                .line_height(px(17.0))
                                .text_color(theme.warning)
                                .child(text),
                        )
                    })
                    .when_some(row.error.clone(), |d, text| {
                        d.child(
                            div()
                                .text_size(px(12.0))
                                .line_height(px(17.0))
                                .text_color(theme.danger)
                                .child(text),
                        )
                    }),
            )
            .child(div().flex_none().child(button))
            .into_any_element()
    }
}

impl Render for TriggersScreen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let view = self.view(cx);
        let retry: page::Action = {
            let store = self.store.clone();
            Rc::new(move |_, cx| store.update(cx, |s, _| s.retry_now()))
        };
        let detail = match view.backing_off {
            0 => "Each firing is deduplicated; missed occurrences merge into one catch-up run."
                .to_owned(),
            1 => "1 trigger is backing off after failing to start a run.".to_owned(),
            n => format!("{n} triggers are backing off after failing to start runs."),
        };
        let rows = view.rows.rows();
        let table_rows: Vec<AnyElement> = rows
            .iter()
            .enumerate()
            .map(|(i, r)| self.row(r, i + 1 == rows.len(), &theme, cx))
            .collect();

        let body = page::content(page::WIDE)
            .when_some(view.banner.as_ref(), |c, b| {
                c.child(page::banner(&theme, b, Some(retry)))
            })
            .child(page::lede(&theme, "Time and files can start work.", detail))
            .when_some(
                page::note(
                    &theme,
                    &view.rows,
                    "No triggers. Add one as a file in the definitions directory.",
                ),
                ParentElement::child,
            )
            .when(!table_rows.is_empty(), |c| {
                c.child(
                    page::table_frame(&theme, page::table())
                        .child(page::table_head(
                            &theme,
                            &[
                                ("Trigger", Some(190.0)),
                                ("Schedule", None),
                                ("Next", Some(110.0)),
                                ("Last", Some(130.0)),
                                ("State", Some(190.0)),
                                ("", Some(92.0)),
                            ],
                        ))
                        .children(table_rows),
                )
            });

        div()
            .id("triggers")
            .size_full()
            .flex()
            .flex_col()
            .child(page::header(&theme, "Triggers", div()))
            .child(page::column("triggers-scroll").child(body))
    }
}
