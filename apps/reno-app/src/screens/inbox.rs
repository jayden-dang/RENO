//! Inbox: what needs the human, longest-waiting first, then what was decided.
//!
//! Live through `approvals.watch`: a card leaves when the daemon reports the new list, never on
//! click. Approve and Deny are the only actions a held action has. Approve is one click. Deny
//! opens a small reason field under the card (optional: Enter denies, Escape cancels) when the
//! daemon takes a reason, and the harness is told it; on an older daemon Deny is one click.
//!
//! A card also offers how far the answer reaches (`Once` by default, `This run`, `This bot`,
//! `Always`) when the daemon keeps standing rules. A choice other than `Once` makes the next
//! Approve or Deny also keep a rule for exactly this action, and a line under the choice says
//! what that is. The choice belongs to the card and is forgotten with it.

use std::collections::HashMap;
use std::rc::Rc;

use gpui::{
    AnyElement, Context, Entity, EventEmitter, Focusable, FontWeight, IntoElement, Render,
    SharedString, Subscription, Window, div, prelude::*, px,
};
use reno_proto::{ApprovalDecision, ApprovalId, Remember, RememberScope};

use super::ScreenEvent;
use crate::store::Store;
use crate::ui::composer::{Composer, ReasonEvent};
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO, Tone};
use crate::ui::page;
use crate::ui::theme::{self, Theme};
use crate::vm::inbox::{self, HeldRow, HistoryRow, QuestionRow, Verdict};
use crate::vm::rules;

/// The reason field open under one card.
struct Reason {
    approval: ApprovalId,
    input: Entity<Composer>,
    _events: Subscription,
}

pub(crate) struct InboxScreen {
    store: Entity<Store>,
    reason: Option<Reason>,
    /// How far each card's answer reaches; a card not here is `Once`.
    scopes: HashMap<ApprovalId, RememberScope>,
    _store: Subscription,
}

impl EventEmitter<ScreenEvent> for InboxScreen {}

impl InboxScreen {
    pub(crate) fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        Self {
            _store: cx.observe(&store, |_, _, cx| cx.notify()),
            store,
            reason: None,
            scopes: HashMap::new(),
        }
    }

    /// What the card's choice says now: `None` is once.
    pub(crate) fn scope_of(&self, approval: &ApprovalId) -> Option<RememberScope> {
        self.scopes.get(approval).copied()
    }

    /// Choose how far the answer to `approval` reaches.
    pub(crate) fn choose_scope(
        &mut self,
        approval: &ApprovalId,
        scope: Option<RememberScope>,
        cx: &mut Context<Self>,
    ) {
        match scope {
            Some(scope) => self.scopes.insert(approval.clone(), scope),
            None => self.scopes.remove(approval),
        };
        cx.notify();
    }

    fn remember(&self, approval: &ApprovalId) -> Option<Remember> {
        self.scope_of(approval).map(|scope| Remember {
            scope,
            expires_in_secs: None,
        })
    }

    /// Deny: opens (or closes) the reason field under the card; without one to offer, denies.
    pub(crate) fn deny(&mut self, row: &HeldRow, window: &mut Window, cx: &mut Context<Self>) {
        let reasons = {
            let store = self.store.read(cx);
            store.state().supports(reno_proto::FEATURE_APPROVAL_MESSAGE)
        };
        if !reasons {
            self.decide(row, ApprovalDecision::Deny, cx);
            return;
        }
        if self
            .reason
            .as_ref()
            .is_some_and(|open| open.approval == row.approval.id)
        {
            self.reason = None;
            cx.notify();
            return;
        }
        let store = self.store.clone();
        let input = cx.new(|cx| Composer::reason(store, cx));
        let events = cx.subscribe_in(&input, window, Self::on_reason);
        window.focus(&input.focus_handle(cx), cx);
        self.reason = Some(Reason {
            approval: row.approval.id.clone(),
            input,
            _events: events,
        });
        cx.notify();
    }

    /// Enter in the reason field denies with it; Escape closes the field.
    fn on_reason(
        &mut self,
        _: &Entity<Composer>,
        event: &ReasonEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(open) = self.reason.take() else {
            return;
        };
        if let ReasonEvent::Submitted(message) = event {
            let remember = self.remember(&open.approval);
            let id = open.approval;
            let message = message.clone();
            self.store.update(cx, |store, cx| {
                store.decide_remembering(id, ApprovalDecision::Deny, message, remember, cx);
            });
        }
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn reason_input(&self) -> Option<&Entity<Composer>> {
        self.reason.as_ref().map(|open| &open.input)
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
        let remember = self.remember(&id);
        self.store.update(cx, |store, cx| {
            store.decide_remembering(id, decision, None, remember, cx);
        });
    }

    /// The choice of how far an answer reaches, and the line that says what it keeps. Nothing
    /// when the daemon keeps no rules.
    fn scope_choice(
        &self,
        row: &HeldRow,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self
            .store
            .read(cx)
            .state()
            .supports(reno_proto::FEATURE_RULES)
        {
            return None;
        }
        let id = row.approval.id.clone();
        let current = self.scope_of(&id);
        let chips = rules::choices(row.bot.is_some())
            .into_iter()
            .map(|(scope, label)| {
                let approval = id.clone();
                kit::choice_chip(
                    theme,
                    format!("scope-{label}-{id}", label = label.replace(' ', "-")),
                    label,
                    scope == current,
                )
                .on_click(
                    cx.listener(move |this, _, _, cx| this.choose_scope(&approval, scope, cx)),
                )
            });
        let hint =
            current.map(|scope| rules::hint(&row.approval.subject, scope, row.bot.as_deref()));
        Some(
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .gap(px(6.0))
                        .child(
                            div()
                                .mr(px(4.0))
                                .text_size(px(12.5))
                                .text_color(theme.muted)
                                .child("Applies to"),
                        )
                        .children(chips),
                )
                .when_some(hint, |d, hint| {
                    d.child(
                        div()
                            .id(SharedString::from(format!("scope-hint-{id}")))
                            .debug_selector({
                                let id = id.clone();
                                move || format!("scope-hint-{id}")
                            })
                            .text_size(px(12.5))
                            .line_height(px(18.0))
                            .text_color(theme.muted)
                            .child(hint),
                    )
                })
                .into_any_element(),
        )
    }

    fn held_card(&self, row: &HeldRow, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let busy = self.store.read(cx).is_deciding(&row.approval.id);
        let id = row.approval.id.to_string();
        let deny = {
            let row = row.clone();
            cx.listener(move |this, _, window, cx| this.deny(&row, window, cx))
        };
        let reason_input = self
            .reason
            .as_ref()
            .filter(|open| open.approval == row.approval.id)
            .map(|open| open.input.clone());
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
        let remembering = self.scope_of(&row.approval.id).is_some();
        let scope_choice = self.scope_choice(row, theme, cx);
        let deny_button = kit::pill_button(
            theme,
            format!("deny-{id}"),
            if remembering { "Deny & keep" } else { "Deny" },
            Tone::Danger,
        )
        .when(busy, |b| b.opacity(0.4))
        .when(!busy, |b| b.on_click(deny));
        let approve_button = kit::pill_button(
            theme,
            format!("approve-{id}"),
            if remembering {
                "Approve & keep"
            } else {
                "Approve once"
            },
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
                    .children(scope_choice)
                    .child(
                        // The chips wrap among themselves; the two decisions stay together on
                        // the right, so a narrow window never splits Deny from Approve.
                        div()
                            .flex()
                            .items_start()
                            .gap(px(8.0))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .flex()
                                    .flex_wrap()
                                    .items_center()
                                    .gap(px(8.0))
                                    .children(chips),
                            )
                            .child(
                                div()
                                    .flex_none()
                                    .flex()
                                    .items_center()
                                    .gap(px(8.0))
                                    .child(deny_button)
                                    .child(approve_button),
                            ),
                    )
                    .children(reason_input.map(|input| {
                        div()
                            .id(SharedString::from(format!("reason-{id}")))
                            .debug_selector({
                                let id = id.clone();
                                move || format!("reason-{id}")
                            })
                            .w_full()
                            .child(input)
                    })),
            )
            .into_any_element()
    }

    /// A run waiting on a harness question. The app cannot answer yet, so the card says what
    /// happens and opens the run, where the question is.
    fn question_card(row: &QuestionRow, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let id = row.run.to_string();
        let open = {
            let run = row.run.clone();
            cx.listener(move |_, _, _, cx| cx.emit(ScreenEvent::OpenRun(run.clone())))
        };
        let who = row.bot.clone().unwrap_or_else(|| "A run".to_owned());
        div()
            .id(SharedString::from(format!("question-{id}")))
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
                    .child(icon(Icon::Question, 17.0, theme.warning)),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
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
                            .child(kit::tag(theme.warning, format!("waited {}", row.waited)))
                            .child(div().flex_1())
                            .child(
                                div()
                                    .id(SharedString::from(format!("open-question-{id}")))
                                    .debug_selector({
                                        let id = id.clone();
                                        move || format!("open-question-{id}")
                                    })
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
                    .when(row.asked.is_empty(), |d| {
                        // The question is not loaded yet: say what the run is for.
                        d.child(
                            div()
                                .text_size(px(13.5))
                                .text_color(theme.text)
                                .child(SharedString::from(row.what.clone())),
                        )
                    })
                    .children(row.asked.iter().enumerate().map(|(n, asked)| {
                        div()
                            .id(SharedString::from(format!("asked-{id}-{n}")))
                            .debug_selector({
                                let id = id.clone();
                                move || format!("asked-{id}-{n}")
                            })
                            .flex()
                            .flex_col()
                            .gap(px(6.0))
                            .child(
                                div()
                                    .text_size(px(13.5))
                                    .text_color(theme.text)
                                    .child(SharedString::from(asked.prompt.clone())),
                            )
                            .when(!asked.options.is_empty(), |d| {
                                d.child(
                                    div().flex().flex_wrap().gap(px(6.0)).children(
                                        asked.options.iter().map(|option| {
                                            div()
                                                .px(px(8.0))
                                                .py(px(2.0))
                                                .rounded(px(10.0))
                                                .border_1()
                                                .border_color(theme.hairline)
                                                .text_size(px(12.0))
                                                .text_color(theme.faint)
                                                .child(SharedString::from(option.clone()))
                                        }),
                                    ),
                                )
                            })
                    }))
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(theme.faint)
                            .child(
                                "This run is waiting on a question. Answering from the app arrives in a later phase; it is declined when it expires.",
                            ),
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
            // The `by` cell says it all: "withdrawn — the run was cancelled".
            Verdict::Withdrawn => (Icon::Clock, theme.faint, ""),
        };
        let withdrawn = row.verdict == Verdict::Withdrawn;
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
                    .map(|d| page::cell(d, Some(96.0)))
                    // The verdict is one short word; it keeps its width so it never runs into
                    // the command beside it.
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .gap(px(7.0))
                    .text_color(tone)
                    .child(icon(glyph, 14.0, tone))
                    .child(word),
            )
            .child(
                div()
                    .map(|d| page::cell(d, None))
                    .flex()
                    .flex_col()
                    .gap(px(3.0))
                    .child(
                        div()
                            .truncate()
                            .font_family(MONO)
                            .text_size(px(12.5))
                            .child(SharedString::from(row.line.clone())),
                    )
                    .when_some(row.message.clone(), |d, message| {
                        d.child(
                            div()
                                .text_size(px(12.5))
                                .text_color(theme.muted)
                                .child(SharedString::from(format!("\u{201c}{message}\u{201d}"))),
                        )
                    }),
            )
            .child(
                div()
                    .map(|d| page::cell(d, Some(130.0)))
                    .truncate()
                    .text_color(theme.muted)
                    .child(row.bot.clone().unwrap_or_default()),
            )
            .child(
                div()
                    .map(|d| page::cell(d, Some(190.0)))
                    .truncate()
                    .text_color(if withdrawn { theme.faint } else { theme.muted })
                    .child(if withdrawn {
                        row.by.clone()
                    } else {
                        format!("by {}", row.by)
                    }),
            )
            .child(
                div()
                    .map(|d| page::cell(d, Some(80.0)))
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

        // A card that left takes its scope choice with it.
        let held_ids: Vec<ApprovalId> = view
            .held
            .rows()
            .iter()
            .map(|row| row.approval.id.clone())
            .collect();
        self.scopes.retain(|id, _| held_ids.contains(id));
        // A card that left (decided somewhere else) takes its reason field with it.
        if self.reason.as_ref().is_some_and(|open| {
            !view
                .held
                .rows()
                .iter()
                .any(|row| row.approval.id == open.approval)
        }) {
            self.reason = None;
        }
        let held: Vec<AnyElement> = view
            .held
            .rows()
            .iter()
            .map(|row| self.held_card(row, &theme, cx))
            .collect();
        let questions: Vec<AnyElement> = view
            .questions
            .iter()
            .map(|row| Self::question_card(row, &theme, cx))
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
            .when(!questions.is_empty(), |c| {
                c.child(
                    div()
                        .id("inbox-questions")
                        .debug_selector(|| "inbox-questions".to_owned())
                        .flex()
                        .flex_col()
                        .gap(px(10.0))
                        .child(page::section_label(&theme, "QUESTIONS"))
                        .children(questions),
                )
            })
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
