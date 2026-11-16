//! The attention pill, Home's signature: "3 need you · 3 running · 8 bots".
//!
//! Built on [`running_pill`]: the counts are pulsing count pills (the pulse stops under reduced
//! motion). Clicking the pill expands a stack of approval cards above it. Each card shows the
//! exact command, with Deny then Approve, which call `Store::decide` (once; `More…` opens the
//! Inbox, where the answer can reach this run, this bot or always). A card is not removed
//! locally: it leaves when the daemon reports the new approval list.

use gpui::{
    AnyElement, Context, Entity, FontWeight, IntoElement, Render, SharedString, Subscription,
    Window, div, prelude::*, px,
};
use reno_client::ConnectionStatus;
use reno_proto::{Approval, ApprovalDecision};

use super::icon::{Icon, icon};
use super::kit::{self, MONO};
use super::running_pill::running_pill;
use super::theme::{self, Theme};
use crate::actions::GoInbox;
use crate::store::Store;
use crate::summary;

pub(crate) struct AttentionDock {
    store: Entity<Store>,
    expanded: bool,
    _store: Subscription,
}

impl AttentionDock {
    pub(crate) fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        Self {
            _store: cx.observe(&store, |_, _, cx| cx.notify()),
            store,
            // Held actions are why the pill exists: it opens on them.
            expanded: true,
        }
    }

    pub(crate) fn toggle(&mut self, cx: &mut Context<Self>) {
        self.expanded = !self.expanded;
        cx.notify();
    }

    pub(crate) fn decide(
        &mut self,
        approval: &Approval,
        decision: ApprovalDecision,
        cx: &mut Context<Self>,
    ) {
        let id = approval.id.clone();
        self.store
            .update(cx, |store, cx| store.decide(id, decision, cx));
    }

    fn card(&self, approval: &Approval, theme: &Theme, cx: &mut Context<Self>) -> impl IntoElement {
        let store = self.store.read(cx);
        let state = store.state();
        let busy = store.is_deciding(&approval.id);
        let bot = summary::approval_bot(state, approval).unwrap_or("A run");
        let executor = state
            .run(&approval.run_id)
            .map_or("", |run| run.executor.as_str());
        let repo = state
            .run(&approval.run_id)
            .and_then(|run| summary::parse_title(&run.title).repo.map(str::to_owned));
        let what = match (summary::action_word(&approval.subject), repo) {
            ("push", Some(repo)) => format!("wants to push to {repo}"),
            ("network request", _) => "wants network access".to_owned(),
            (word, _) => format!("wants to run a {word}"),
        };
        let age = summary::age_label(store.now_ms().saturating_sub(approval.requested_at_ms));
        let command = summary::subject_line(&approval.subject);
        let is_network = approval.subject.action == reno_proto::ActionClass::Network;

        let id = approval.id.to_string();
        let deny = approval.clone();
        let approve = approval.clone();
        let deny_button = kit::pill_button(theme, format!("deny-{id}"), "Deny", kit::Tone::Danger)
            .when(busy, |b| b.opacity(0.4))
            .when(!busy, |b| {
                b.on_click(cx.listener(move |this, _, _, cx| {
                    this.decide(&deny, ApprovalDecision::Deny, cx);
                }))
            });
        let approve_button = kit::pill_button(
            theme,
            format!("approve-{id}"),
            "Approve",
            kit::Tone::Primary,
        )
        .when(busy, |b| b.opacity(0.4))
        .when(!busy, |b| {
            b.on_click(cx.listener(move |this, _, _, cx| {
                this.decide(&approve, ApprovalDecision::ApproveOnce, cx);
            }))
        });

        // Deny and Approve answer once. A wider answer (this run, this bot, always) is chosen on
        // the Inbox card, which this opens, rather than repeated here.
        let more_button = kit::pill_button(
            theme,
            format!("more-{id}"),
            "More\u{2026}",
            kit::Tone::Quiet,
        )
        .on_click(|_, window, cx| window.dispatch_action(Box::new(GoInbox), cx));

        div()
            .id(SharedString::from(format!("approval-{id}")))
            .w_full()
            .flex()
            .items_center()
            .gap(px(14.0))
            .px(px(14.0))
            .py(px(12.0))
            .rounded(px(18.0))
            .bg(theme.surface)
            .border_1()
            .border_color(theme.hairline)
            .child(
                div()
                    .flex_none()
                    .size(px(36.0))
                    .rounded(px(18.0))
                    .bg(theme.raised.opacity(0.5))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(kit::exec_glyph(theme, executor, 15.0)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(5.0))
                    .child(
                        div().text_size(px(14.0)).text_color(theme.text).child(
                            div()
                                .flex()
                                .gap(px(5.0))
                                .child(
                                    div()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .child(bot.to_owned()),
                                )
                                .child(div().text_color(theme.muted).child(what)),
                        ),
                    )
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap(px(8.0))
                            .child(kit::tag(theme.warning, age))
                            .when(is_network, |row| {
                                row.child(kit::tag(theme.danger, "network"))
                            })
                            .child(
                                div()
                                    .min_w_0()
                                    .font_family(MONO)
                                    .text_size(px(12.5))
                                    .line_height(px(20.0))
                                    .text_color(theme.text)
                                    .child(command),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .flex()
                    .gap(px(8.0))
                    .child(more_button)
                    .child(deny_button)
                    .child(approve_button),
            )
    }
}

impl Render for AttentionDock {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let (counts, connection, approvals) = {
            let store = self.store.read(cx);
            (
                summary::attention(store.state()),
                store.connection(),
                store.state().approvals().to_vec(),
            )
        };
        let open = self.expanded && !approvals.is_empty();
        let cards: Vec<AnyElement> = if open {
            approvals
                .iter()
                .map(|a| self.card(a, &theme, cx).into_any_element())
                .collect()
        } else {
            Vec::new()
        };
        let offline = connection != ConnectionStatus::Connected;

        let plain = |text: String, color| div().text_color(color).child(text);
        let dotsep = || div().text_color(theme.faint).child("·");
        let mut summary_row = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .text_size(px(13.5))
            .text_color(theme.text);
        summary_row = if offline {
            summary_row.child(plain("daemon offline".to_owned(), theme.faint))
        } else {
            let need = if counts.need > 0 {
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_color(theme.warning)
                    .child(running_pill(
                        "attention-need",
                        counts.need,
                        theme.warning,
                        MONO,
                    ))
                    .child("need you")
            } else {
                div().text_color(theme.success).child("all clear")
            };
            let running = if counts.running > 0 {
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(running_pill(
                        "attention-running",
                        counts.running,
                        theme.accent,
                        MONO,
                    ))
                    .child("running")
            } else {
                div().child("0 running")
            };
            summary_row
                .child(need)
                .child(dotsep())
                .child(running)
                .child(dotsep())
                .child(div().child(format!("{} bots", counts.bots)))
        };

        let pill = div()
            .id("attention-pill")
            .flex()
            .items_center()
            .gap(px(12.0))
            .h(px(40.0))
            .px(px(18.0))
            .rounded(px(999.0))
            .bg(theme.surface)
            .border_1()
            .border_color(theme.hairline)
            .cursor_pointer()
            .on_click(cx.listener(|this, _, _, cx| this.toggle(cx)))
            .child(summary_row)
            .when(!approvals.is_empty(), |pill| {
                pill.child(icon(
                    if open { Icon::ChevDown } else { Icon::ChevUp },
                    14.0,
                    theme.muted,
                ))
            });

        div()
            .w_full()
            .flex()
            .flex_col()
            .items_center()
            .gap(px(10.0))
            .when(open, |dock| {
                dock.child(
                    div()
                        .id("approval-stack")
                        .w_full()
                        .max_w(px(660.0))
                        .flex()
                        .flex_col()
                        .gap(px(8.0))
                        .children(cards),
                )
            })
            .child(pill)
    }
}
