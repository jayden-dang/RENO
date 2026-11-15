//! Inbox: what needs the human, longest-waiting first, then what was decided.
//!
//! Live through `approvals.watch`: a card leaves when the daemon reports the new list, never on
//! click. Approve and Deny are the only actions a held action has on the wire
//! (`approval.decide` carries no message), so the cards offer exactly those two.

use std::rc::Rc;

use gpui::{
    AnyElement, Context, Entity, EventEmitter, FontWeight, IntoElement, Render, SharedString,
    Subscription, Window, div, prelude::*, px,
};
use reno_proto::ApprovalDecision;

use super::ScreenEvent;
use crate::store::Store;
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO, Tone};
use crate::ui::page;
use crate::ui::theme::{self, Theme};
use crate::vm::inbox::{self, HeldRow, HistoryRow, Verdict};

pub(crate) struct InboxScreen {
    store: Entity<Store>,
    _store: Subscription,
}

impl EventEmitter<ScreenEvent> for InboxScreen {}

impl InboxScreen {
    pub(crate) fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        Self {
            _store: cx.observe(&store, |_, _, cx| cx.notify()),
            store,
        }
    }

    /// Called when the screen is shown: the history has no watch, so it is read now.
    pub(crate) fn shown(&self, cx: &mut Context<Self>) {
        self.store.update(cx, |store, cx| store.load_history(cx));
    }

    pub(crate) fn view(&self, cx: &gpui::App) -> inbox::Inbox {
        let store = self.store.read(cx);
        inbox::build(store.state(), store.now_ms())
    }

    pub(crate) fn decide(
        &mut self,
        row: &HeldRow,
        decision: ApprovalDecision,
        cx: &mut Context<Self>,
    ) {
        let id = row.approval.id.clone();
        self.store
            .update(cx, |store, cx| store.decide(id, decision, cx));
    }

    fn held_card(&self, row: &HeldRow, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.store.read(cx).is_deciding(&row.approval.id);
        let id = row.approval.id.to_string();
        let deny = {
            let row = row.clone();
            cx.listener(move |this, _, _, cx| this.decide(&row, ApprovalDecision::Deny, cx))
        };
        let approve = {
            let row = row.clone();
            cx.listener(move |this, _, _, cx| {
                this.decide(&row, ApprovalDecision::ApproveOnce, cx);
            })
        };
        let open = {
            let run = row.run.clone();
            cx.listener(move |_, _, _, cx| cx.emit(ScreenEvent::OpenRun(run.clone())))
        };
        let deny_button = kit::pill_button(theme, format!("deny-{id}"), "Deny", Tone::Danger)
            .when(busy, |b| b.opacity(0.4))
            .when(!busy, |b| b.on_click(deny));
        let approve_button = kit::pill_button(
            theme,
            format!("approve-{id}"),
            "Approve once",
            Tone::Primary,
        )
        .when(busy, |b| b.opacity(0.4))
        .when(!busy, |b| b.on_click(approve));

        let who = row.bot.clone().unwrap_or_else(|| "A run".to_owned());
        let mut chips = vec![kit::tag(theme.warning, format!("waited {}", row.waited))];
        chips.push(kit::chip(theme, row.action));
        chips.push(kit::chip(theme, row.tool.clone()));
        if row.network {
            chips.push(kit::tag(theme.danger, "network"));
        }
        for path in &row.paths {
            chips.push(kit::chip(theme, path.clone()));
        }

        div()
            .id(SharedString::from(format!("held-{id}")))
            .w_full()
            .flex()
            .gap(px(14.0))
            .p(px(16.0))
            .rounded(px(18.0))
            .bg(theme.surface)
            .border_1()
            .border_color(theme.hairline)
            .child(
                div()
                    .flex_none()
                    .size(px(40.0))
                    .rounded(px(20.0))
                    .bg(theme.raised.opacity(0.5))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(kit::exec_glyph(theme, &row.executor, 17.0)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .text_size(px(14.0))
                            .child(div().font_weight(FontWeight::SEMIBOLD).child(who))
                            .child(div().text_color(theme.muted).child(format!(
                                "\u{b7} {}",
                                crate::summary::executor_name(&row.executor)
                            )))
                            .when_some(row.repo.clone(), |d, repo| {
                                d.child(
                                    div()
                                        .text_color(theme.muted)
                                        .child(format!("\u{b7} {repo}")),
                                )
                            })
                            .child(div().flex_1())
                            .child(
                                div()
                                    .id(SharedString::from(format!("open-run-{id}")))
                                    .text_size(px(12.5))
                                    .text_color(theme.muted)
                                    .cursor_pointer()
                                    .hover({
                                        let text = theme.text;
                                        move |s| s.text_color(text)
                                    })
                                    .on_click(open)
                                    .child("Open run"),
                            ),
                    )
                    .child(
                        div()
                            .w_full()
                            .px(px(14.0))
                            .py(px(9.0))
                            .rounded(px(10.0))
                            .bg(theme.shell)
                            .font_family(MONO)
                            .text_size(px(13.0))
                            .line_height(px(20.0))
                            .child(SharedString::from(row.line.clone())),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap(px(8.0))
                            .children(chips)
                            .child(div().flex_1())
                            .child(deny_button)
                            .child(approve_button),
                    ),
            )
            .into_any_element()
    }

    fn history_row(
        row: &HistoryRow,
        last: bool,
        index: usize,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (glyph, tone, word) = match row.verdict {
            Verdict::Approved => (Icon::Check, theme.success, "approved"),
            Verdict::Denied => (Icon::X, theme.danger, "denied"),
            Verdict::Expired => (Icon::Clock, theme.faint, "expired"),
        };
        let open = {
            let run = row.run.clone();
            cx.listener(move |_, _, _, cx| cx.emit(ScreenEvent::OpenRun(run.clone())))
        };
        page::table_row(theme, last)
            .id(SharedString::from(format!("history-{index}")))
            .cursor_pointer()
            .hover({
                let hover = theme.raised.opacity(0.4);
                move |s| s.bg(hover)
            })
            .on_click(open)
            .child(
                div()
                    .flex_none()
                    .w(px(96.0))
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .text_color(tone)
                    .child(icon(glyph, 14.0, tone))
                    .child(word),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(MONO)
                    .text_size(px(12.5))
                    .child(SharedString::from(row.line.clone())),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(130.0))
                    .truncate()
                    .text_color(theme.muted)
                    .child(row.bot.clone().unwrap_or_default()),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(190.0))
                    .truncate()
                    .text_color(theme.muted)
                    .child(format!("by {}", row.by)),
            )
            .child(
                div()
                    .flex_none()
                    .w(px(80.0))
                    .text_color(theme.faint)
                    .child(row.when.clone()),
            )
            .into_any_element()
    }
}

impl Render for InboxScreen {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let view = self.view(cx);
        let retry: page::Action = {
            let store = self.store.clone();
            Rc::new(move |_, cx| store.update(cx, |s, _| s.retry_now()))
        };

        let held: Vec<AnyElement> = view
            .held
            .rows()
            .iter()
            .map(|row| self.held_card(row, &theme, cx))
            .collect();
        let history_rows = view.history.rows();
        let history: Vec<AnyElement> = history_rows
            .iter()
            .enumerate()
            .map(|(i, row)| Self::history_row(row, i + 1 == history_rows.len(), i, &theme, cx))
            .collect();

        let body = page::content(page::NARROW)
            .when_some(view.banner.as_ref(), |c, b| {
                c.child(page::banner(&theme, b, Some(retry)))
            })
            .child(page::lede(&theme, view.lede.clone(), view.detail.clone()))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(page::section_label(&theme, "HELD ACTIONS"))
                    .when_some(
                        page::note(
                            &theme,
                            &view.held,
                            "Nothing is held. Runs are not waiting on you.",
                        ),
                        ParentElement::child,
                    )
                    .children(held),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(10.0))
                    .child(page::section_label(&theme, "DECIDED"))
                    .when_some(
                        page::note(&theme, &view.history, "No decisions yet."),
                        ParentElement::child,
                    )
                    .when(!history.is_empty(), |d| {
                        d.child(page::table_frame(&theme, page::table()).children(history))
                    }),
            );

        div()
            .id("inbox")
            .size_full()
            .flex()
            .flex_col()
            .child(page::header(&theme, "Inbox", div()))
            .child(page::column("inbox-scroll").child(body))
    }
}
