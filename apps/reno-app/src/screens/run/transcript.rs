//! The transcript: one function per row kind and the cards inside them. State and the rebuild
//! from the store live in `run.rs`; this only draws.

use super::*;

impl RunScreen {
    pub(super) fn render_row(
        &self,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = theme::current(cx);
        let Some(row) = self.rows.get(ix) else {
            return div().into_any_element();
        };
        let body = match row {
            Row::User { text, steer, .. } => user_bubble(&theme, text, *steer),
            Row::Prose { key, .. } => match self.markdown.get(key) {
                Some(stream) => div()
                    .text_size(px(15.0))
                    .line_height(px(24.0))
                    .child(markdown_view::blocks(stream.tree(), &theme, window))
                    .into_any_element(),
                None => div().into_any_element(),
            },
            Row::Reasoning { key, text } => self.reasoning(&theme, *key, text, cx),
            Row::Tools { key, calls } => self.tools(&theme, *key, calls, cx),
            Row::Question {
                questions,
                resolution,
                ..
            } => question_card(&theme, questions, resolution),
            Row::Approval { key, approval } => self.approval_card(&theme, *key, approval, cx),
            Row::Asked { line, .. } => event_line(&theme, Icon::Shield, theme.muted, line),
            Row::ModelChanged { model, .. } => {
                let name = {
                    let store = self.store.read(cx);
                    let executor = store.state().run(&self.run).map(|r| r.executor.clone());
                    executor.map_or_else(
                        || model.clone(),
                        |e| crate::vm::models::model_name(store.state(), &e, model),
                    )
                };
                event_line(
                    &theme,
                    Icon::Route,
                    theme.muted,
                    &format!("Switched to {name}"),
                )
            }
            Row::Routing { key, view } => self.routing_card(&theme, *key, view, cx),
            Row::Done {
                stop,
                result,
                diagnostic,
                reason,
                ..
            } => div()
                .child(done_line(
                    &theme,
                    *stop,
                    result.as_deref(),
                    diagnostic.as_deref(),
                    reason.as_ref(),
                ))
                // Only the thread's latest run says what the branch holds.
                .children(
                    (self
                        .rows
                        .iter()
                        .rposition(|r| matches!(r, Row::Done { .. }))
                        == Some(ix))
                    .then(|| self.result_line(&theme, cx))
                    .flatten(),
                )
                .into_any_element(),
            Row::Divider {
                executor,
                model,
                at_ms,
                ..
            } => self.divider(&theme, executor, model.as_deref(), *at_ms, cx),
        };
        div()
            .w_full()
            .when(ix == 0, |d| d.pt(px(22.0)))
            .pb(px(if ix + 1 == self.rows.len() {
                26.0
            } else {
                16.0
            }))
            .child(body)
            .into_any_element()
    }

    /// Where a follow-up run begins: "New run · Claude Code · Sonnet" and when it started, between
    /// two hairlines.
    fn divider(
        &self,
        theme: &Theme,
        executor: &ExecutorId,
        model: Option<&str>,
        at_ms: u64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let model = model
            .map(|id| crate::vm::models::model_name(self.store.read(cx).state(), executor, id));
        let mut text = format!(
            "New run \u{b7} {}",
            summary::executor_name(executor.as_str())
        );
        if let Some(model) = model {
            text.push_str(&format!(" \u{b7} {model}"));
        }
        let started = chrono::DateTime::from_timestamp_millis(i64::try_from(at_ms).unwrap_or(0))
            .map(|t| {
                t.with_timezone(&chrono::Local)
                    .format("%-d %b %H:%M")
                    .to_string()
            })
            .unwrap_or_default();
        let line = || div().flex_1().h(px(1.0)).bg(theme.hairline);
        div()
            .id("run-divider")
            .debug_selector(|| "run-divider".to_owned())
            .flex()
            .items_center()
            .gap(px(10.0))
            .text_size(px(12.0))
            .text_color(theme.faint)
            .child(line())
            .child(SharedString::from(text))
            .child(div().font_family(MONO).child(SharedString::from(started)))
            .child(line())
            .into_any_element()
    }

    /// "Routed to Codex: Claude Code was signed out", with the "why" collapsed.
    pub(super) fn routing_card(
        &self,
        theme: &Theme,
        key: Key,
        view: &RoutingView,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.open.contains(&key);
        let chosen = view.chosen.clone();
        let head = div()
            .id("routing-why")
            .debug_selector(|| "routing-why".to_owned())
            .flex()
            .items_center()
            .gap(px(8.0))
            .text_size(px(13.5))
            .when(view.has_detail(), |d| {
                d.cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(key, cx)))
            })
            .child(icon(Icon::Route, 14.0, theme.faint))
            .when_some(chosen.clone(), |d, who| {
                d.child(kit::exec_glyph(theme, &who.id, 14.0))
            })
            .child(
                div()
                    .flex_none()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(view.headline.clone())),
            )
            .when_some(
                chosen.and_then(|c| c.secondary().map(str::to_owned)),
                |d, id| {
                    d.child(
                        div()
                            .flex_none()
                            .text_color(theme.faint)
                            .child(SharedString::from(id)),
                    )
                },
            )
            .when_some(view.because.clone(), |d, because| {
                d.child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_color(theme.muted)
                        .child(SharedString::from(format!("\u{b7} {because}"))),
                )
            })
            .child(div().flex_1())
            .when(view.has_detail(), |d| {
                d.child(icon(
                    if open { Icon::ChevDown } else { Icon::Fwd },
                    14.0,
                    theme.muted,
                ))
            });

        let label = |text: &'static str| {
            div()
                .flex_none()
                .w(px(86.0))
                .text_color(theme.faint)
                .child(text)
        };
        let named = |name: String, id: Option<String>, then: String| {
            div()
                .flex()
                .flex_wrap()
                .gap(px(6.0))
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child(SharedString::from(name)),
                )
                .when_some(id, |d, id| {
                    d.child(div().text_color(theme.faint).child(SharedString::from(id)))
                })
                .child(SharedString::from(then))
        };
        let detail = div()
            .pt(px(8.0))
            .pl(px(22.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .text_size(px(13.0))
            .text_color(theme.muted)
            .child(
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(label("Rule"))
                    .child(SharedString::from(view.rule.clone())),
            )
            .when(view.order.len() > 1, |d| {
                let order = view
                    .order
                    .iter()
                    .map(|n| n.name.clone())
                    .collect::<Vec<_>>()
                    .join(" \u{2192} ");
                d.child(
                    div()
                        .flex()
                        .gap(px(8.0))
                        .child(label("Order"))
                        .child(SharedString::from(order)),
                )
            })
            .children(view.rejected.iter().enumerate().map(|(i, r)| {
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(label(if i == 0 { "Passed over" } else { "" }))
                    .child(named(
                        r.who.name.clone(),
                        r.who.secondary().map(str::to_owned),
                        r.reason.clone(),
                    ))
            }))
            .children(view.walk.iter().enumerate().map(|(i, step)| {
                let to = step
                    .to
                    .as_ref()
                    .map_or_else(|| "nothing left".to_owned(), |t| t.name.clone());
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(label(if i == 0 { "Fallback" } else { "" }))
                    .child(named(
                        format!("{} \u{2192} {to}", step.from.name),
                        None,
                        step.reason.clone(),
                    ))
            }));

        div()
            .id("routing-card")
            .debug_selector(|| "routing-card".to_owned())
            .w_full()
            .px(px(14.0))
            .py(px(10.0))
            .rounded(px(12.0))
            .bg(theme.surface)
            .border_1()
            .border_color(theme.hairline)
            .child(head)
            .when(open, |d| d.child(detail))
            .into_any_element()
    }

    pub(super) fn reasoning(
        &self,
        theme: &Theme,
        key: Key,
        text: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.open.contains(&key);
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                disclosure(
                    theme,
                    SharedString::from(format!("reasoning-{}", key.0)),
                    open,
                    "Reasoning".to_owned(),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.toggle(key, cx))),
            )
            .when(open, |d| {
                d.child(
                    div()
                        .pl(px(24.0))
                        .text_size(px(13.5))
                        .line_height(px(21.0))
                        .text_color(theme.muted)
                        .child(SharedString::from(text.to_owned())),
                )
            })
            .into_any_element()
    }

    pub(super) fn tools(
        &self,
        theme: &Theme,
        key: Key,
        calls: &[ToolEntry],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.open.contains(&key);
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                disclosure(
                    theme,
                    SharedString::from(format!("tools-{}", key.0)),
                    open,
                    model::group_title(calls),
                )
                .on_click(cx.listener(move |this, _, _, cx| this.toggle(key, cx))),
            )
            .when(open, |d| {
                d.child(
                    div()
                        .ml(px(24.0))
                        .rounded(px(10.0))
                        .border_1()
                        .border_color(theme.hairline)
                        .bg(theme.surface)
                        .flex()
                        .flex_col()
                        .children(
                            calls
                                .iter()
                                .map(|call| self.tool_entry(theme, key, call, calls.len(), cx)),
                        ),
                )
            })
            .into_any_element()
    }

    /// A held action, answered where the agent asked (ui-brief 2026-11-17): who and what, the
    /// exact command, why it waits, Approve once ▾ (how far the answer reaches), and a reason
    /// field with Deny under it.
    pub(super) fn approval_card(
        &self,
        theme: &Theme,
        key: Key,
        approval: &Approval,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.store.read(cx);
        let state = store.state();
        let busy = store.is_deciding(&approval.id);
        let keeps_rules = state.supports(reno_proto::FEATURE_RULES);
        let summary = state.run(&approval.run_id);
        let bot = summary.and_then(|r| summary::run_bot(r));
        let who = bot.unwrap_or("The run").to_owned();
        let executor = summary.map_or("", |r| r.executor.as_str());
        let age = summary::age_label(store.now_ms().saturating_sub(approval.requested_at_ms));
        let command = summary::subject_line(&approval.subject);
        let word = summary::action_word(&approval.subject);
        // "Always in porch": only when the daemon keeps project rules and the run is in one.
        let project = state
            .supports(reno_proto::FEATURE_PROJECT_RULES)
            .then(|| summary.and_then(|r| r.cwd.as_deref()))
            .flatten()
            .and_then(|cwd| store.root_of(cwd))
            .and_then(|root| root.file_name().map(|n| n.to_string_lossy().into_owned()));
        let choices = rules::approve_choices(bot, project.as_deref());
        let menu_open = self.menu_open.as_ref() == Some(&approval.id);

        let id = approval.id.clone();
        let approve = {
            let id = id.clone();
            cx.listener(move |this, _, _, cx| this.approve(id.clone(), None, cx))
        };
        let toggle_menu = {
            let id = id.clone();
            cx.listener(move |this, _, _, cx| {
                this.menu_open = if this.menu_open.as_ref() == Some(&id) {
                    None
                } else {
                    Some(id.clone())
                };
                cx.notify();
            })
        };
        let deny = {
            let id = id.clone();
            cx.listener(move |this, _, _, cx| {
                let message = this
                    .reasons
                    .get(&id)
                    .and_then(|(input, _)| input.read(cx).typed_reason());
                this.deny(id.clone(), message, cx);
            })
        };

        let approve_button = kit::pill_button(theme, "approve", "Approve once", Tone::Primary)
            .when(busy, |b| b.opacity(0.4))
            .when(!busy, |b| b.on_click(approve));
        let more_button = keeps_rules.then(|| {
            kit::icon_button(
                theme,
                "approve-more",
                if menu_open {
                    Icon::ChevUp
                } else {
                    Icon::ChevDown
                },
                28.0,
            )
            .debug_selector(|| "approve-more".to_owned())
            .when(!busy, |b| b.on_click(toggle_menu))
        });
        let deny_button = kit::pill_button(theme, "deny", "Deny", Tone::Danger)
            .when(busy, |b| b.opacity(0.4))
            .when(!busy, |b| b.on_click(deny));
        // What a kept answer holds, in words: exactly this action, for whom.
        let keeps = rules::hint(
            &approval.subject,
            if bot.is_some() {
                RememberScope::Bot
            } else {
                RememberScope::Run
            },
            bot,
        );
        let menu =
            (menu_open && keeps_rules).then(|| {
                div()
                    .flex()
                    .flex_wrap()
                    .justify_end()
                    .gap(px(6.0))
                    .child(
                        div()
                            .w_full()
                            .text_size(px(12.0))
                            .text_color(theme.muted)
                            .child(keeps),
                    )
                    .children(choices.into_iter().enumerate().skip(1).map(
                        |(ix, (scope, label))| {
                            let id = id.clone();
                            kit::pill_button(
                                theme,
                                format!("approve-scope-{ix}"),
                                label,
                                Tone::Quiet,
                            )
                            .debug_selector(move || format!("approve-scope-{ix}"))
                            .when(!busy, |b| {
                                b.on_click(cx.listener(move |this, _, _, cx| {
                                    this.approve(id.clone(), scope, cx);
                                }))
                            })
                        },
                    ))
            });
        // A daemon without `approval.message` would drop the words: then there is no field.
        let reason = self
            .reasons
            .get(&approval.id)
            .filter(|_| self.shows_reason(&approval.id, cx))
            .map(|(input, _)| input.clone());

        div()
            .id(SharedString::from(format!("approval-card-{}", key.0)))
            .w_full()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .p(px(16.0))
            .rounded(px(18.0))
            .bg(theme.surface)
            .border_1()
            .border_color(theme.hairline)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(px(13.5))
                    .child(kit::exec_glyph(theme, executor, 14.0))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(who))
                    .child(div().text_color(theme.muted).child("wants to run"))
                    .child(div().flex_1())
                    .child(
                        div()
                            .text_size(px(12.5))
                            .text_color(theme.warning)
                            .child(format!("held {age}")),
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
                    .child(command),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(kit::tag(theme.warning, format!("{word} needs a human")))
                    .child(div().flex_1())
                    .child(approve_button)
                    .children(more_button),
            )
            .children(menu)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(div().flex_1().min_w_0().children(reason))
                    .child(deny_button),
            )
            .into_any_element()
    }

    /// Open the Executors screen, or check this executor again.
    pub(super) fn executor_actions(
        &self,
        row: &executor_facts::ExecutorRow,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let checking = row
            .id
            .parse::<ExecutorId>()
            .is_ok_and(|id| self.store.read(cx).is_checking(Some(&id)));
        let check = {
            let id = row.id.parse::<ExecutorId>().ok();
            cx.listener(move |this, _, _, cx| {
                if let Some(id) = id.clone() {
                    this.store
                        .update(cx, |store, cx| store.refresh_executors(Some(id), cx));
                }
            })
        };
        let open = cx.listener(|_, _, _, cx| cx.emit(ScreenEvent::OpenExecutors));
        let check_button = kit::pill_button(
            theme,
            "run-check-executor",
            if checking {
                "Checking\u{2026}"
            } else {
                "Check again"
            },
            Tone::Quiet,
        );
        let check_button = if checking {
            check_button.opacity(0.4).cursor_default()
        } else {
            check_button.on_click(check)
        };
        div()
            .pt(px(10.0))
            .flex()
            .flex_wrap()
            .gap(px(8.0))
            .child(
                kit::pill_button(theme, "run-open-executors", "Open Executors", Tone::Quiet)
                    .on_click(open),
            )
            .child(check_button)
            .into_any_element()
    }
}

/// A user message: right-aligned bubble.
pub(super) fn user_bubble(theme: &Theme, text: &str, steer: bool) -> AnyElement {
    div()
        .w_full()
        .flex()
        .flex_col()
        .items_end()
        .gap(px(5.0))
        .when(steer, |d| {
            d.child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.faint)
                    .child("You \u{b7} steer"),
            )
        })
        .child(
            div()
                .max_w(px(560.0))
                .px(px(18.0))
                .py(px(12.0))
                .rounded(px(20.0))
                .bg(theme.accent.opacity(0.14))
                .text_size(px(15.0))
                .line_height(px(23.0))
                .child(SharedString::from(text.to_owned())),
        )
        .into_any_element()
}

/// The clickable head of a collapsible row: a chevron and a label.
pub(super) fn disclosure(
    theme: &Theme,
    id: SharedString,
    open: bool,
    label: String,
) -> gpui::Stateful<gpui::Div> {
    let selector = id.to_string();
    div()
        .id(id)
        .debug_selector(move || selector.clone())
        .flex()
        .items_center()
        .gap(px(8.0))
        .text_size(px(14.5))
        .text_color(theme.muted)
        .cursor_pointer()
        .hover({
            let text = theme.text;
            move |s| s.text_color(text)
        })
        .child(icon(
            if open { Icon::ChevDown } else { Icon::Fwd },
            14.0,
            theme.muted,
        ))
        .child(label)
}

/// The tone of the card's leading "?": warning while the question waits, then the tone of how
/// it ended.
pub(super) fn question_icon_tone(theme: &Theme, resolution: &Resolution) -> gpui::Hsla {
    match resolution {
        Resolution::Pending => theme.warning,
        Resolution::Declined | Resolution::Unanswered => theme.faint,
        Resolution::Answered(_) => theme.success,
    }
}

/// The question card. Pending, it is the live ask; resolved, it is the record of how it ended:
/// a status tag, the options dimmed (the chosen one lit) and no promise of a later phase.
pub(super) fn question_card(
    theme: &Theme,
    questions: &[reno_proto::Question],
    resolution: &Resolution,
) -> AnyElement {
    let tag = match resolution {
        Resolution::Pending => None,
        Resolution::Declined => Some((theme.faint, "Declined", "question-tag-declined")),
        Resolution::Unanswered => Some((theme.faint, "Not answered", "question-tag-unanswered")),
        Resolution::Answered(_) => Some((theme.success, "Answered", "question-tag-answered")),
    };
    let answer_of = |i: usize| match resolution {
        Resolution::Answered(answers) => answers.get(i).cloned().flatten(),
        _ => None,
    };
    let card = div()
        .w_full()
        .p(px(16.0))
        .rounded(px(18.0))
        .bg(theme.surface)
        .border_1()
        .border_color(theme.hairline)
        .flex()
        .flex_col()
        .gap(px(12.0))
        .children(questions.iter().enumerate().map(|(i, q)| {
            let answer = answer_of(i);
            let chosen = |o: &str| {
                answer
                    .as_deref()
                    .is_some_and(|a| a.split(", ").any(|part| part == o))
            };
            let free_text = answer
                .as_ref()
                .filter(|_| !q.options.iter().any(|o| chosen(o)));
            let header_tag = tag.filter(|_| i == 0).map(|(tone, label, selector)| {
                div()
                    .ml_auto()
                    .debug_selector(move || selector.to_owned())
                    .child(kit::tag(tone, label))
            });
            div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(icon(
                            Icon::Question,
                            15.0,
                            question_icon_tone(theme, resolution),
                        ))
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .child(q.prompt.clone()),
                        )
                        .children(header_tag),
                )
                .child(
                    div()
                        .flex()
                        .flex_wrap()
                        .gap(px(8.0))
                        .children(q.options.iter().map(|o| {
                            let chip = kit::chip(theme, o.clone());
                            match resolution {
                                Resolution::Pending => chip,
                                _ if chosen(o) => chip
                                    .border_color(theme.success.opacity(0.7))
                                    .bg(theme.success.opacity(0.14))
                                    .font_weight(FontWeight::MEDIUM)
                                    .debug_selector(|| "question-chosen-chip".to_owned()),
                                _ => chip
                                    .opacity(0.5)
                                    .text_color(theme.faint)
                                    .debug_selector(|| "question-dimmed-chip".to_owned()),
                            }
                        })),
                )
                .children(free_text.map(|text| {
                    div()
                        .text_size(px(13.5))
                        .text_color(theme.text)
                        .child(SharedString::from(format!("\u{201c}{text}\u{201d}")))
                }))
        }));
    match resolution {
        Resolution::Pending => card.child(
            div()
                .debug_selector(|| "question-pending-note".to_owned())
                .text_size(px(12.5))
                .text_color(theme.faint)
                .child("Answering from the app arrives in a later phase."),
        ),
        Resolution::Declined => card.child(
            div()
                .debug_selector(|| "question-declined-line".to_owned())
                .text_size(px(12.5))
                .text_color(theme.faint)
                .child("Nobody answered in time."),
        ),
        Resolution::Unanswered => card.child(
            div()
                .text_size(px(12.5))
                .text_color(theme.faint)
                .child("The run went on without an answer."),
        ),
        Resolution::Answered(_) => card,
    }
    .into_any_element()
}

/// A one-line event: a muted icon and a sentence.
pub(super) fn event_line(theme: &Theme, glyph: Icon, tone: gpui::Hsla, text: &str) -> AnyElement {
    div()
        .flex()
        .items_center()
        .gap(px(9.0))
        .text_size(px(13.5))
        .text_color(theme.muted)
        .child(icon(glyph, 15.0, tone))
        .child(SharedString::from(text.to_owned()))
        .into_any_element()
}

/// The closing status line.
pub(super) fn done_line(
    theme: &Theme,
    stop: StopReason,
    result: Option<&str>,
    diagnostic: Option<&str>,
    reason: Option<&reno_proto::StopDetail>,
) -> AnyElement {
    let (glyph, tone, head) = match stop {
        StopReason::Completed => (Icon::Check, theme.success, "Finished"),
        StopReason::Interrupted => (Icon::Stop, theme.faint, "Stopped"),
        StopReason::Error => (Icon::X, theme.danger, "Failed"),
        StopReason::Refusal => (Icon::X, theme.warning, "The model declined"),
        StopReason::MaxTokens => (Icon::X, theme.warning, "Hit the output limit"),
    };
    // The typed reason says it in words (a budget stop reads as the limit, not as JSON); the
    // daemon's own diagnostic and the result are what is left.
    let said = reason.map(ToString::to_string);
    let detail = said.as_deref().or(diagnostic).or(result);
    div()
        .flex()
        .items_start()
        .gap(px(9.0))
        .text_size(px(14.0))
        .child(div().pt(px(2.0)).child(icon(glyph, 15.0, tone)))
        .child(
            div()
                .text_color(tone)
                .font_weight(FontWeight::MEDIUM)
                .child(head),
        )
        .when_some(detail, |d, text| {
            d.child(
                div()
                    .min_w_0()
                    .text_color(theme.muted)
                    .child(SharedString::from(format!("\u{b7} {text}"))),
            )
        })
        .into_any_element()
}
