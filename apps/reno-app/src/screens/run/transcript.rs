//! The transcript: one function per row kind and the cards inside them. State and the rebuild
//! from the store live in `run.rs`; this only draws.
//!
//! The look is the reference app's: agent text bare on the canvas, the user's words in a neutral
//! raised bubble, tool activity as one muted line each, and a finished turn folded behind
//! "Worked for Ns".

use std::rc::Rc;

use reno_markdown::BlockTree;

use super::select::Spot;
use super::*;
use crate::ui::markdown_view::lru::Lru;
use crate::ui::markdown_view::{Host, SelectHost, Streaming, select as pick, visual::visual_view};
use crate::ui::motion;
use crate::vm::find;
use crate::vm::selection::{self, Slot};

impl RunScreen {
    /// One list item: its row, with the space below it, the subagent chips that hang on it and,
    /// under the last item of a live run, "Thinking".
    pub(super) fn render_row(&mut self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme::current(cx);
        let Some(item) = self.items.items.get(ix).copied() else {
            return div().into_any_element();
        };
        if let Some(v) = item.visual {
            return self.visual_item(ix, v, item.decor.last, &theme, cx);
        }
        // A reply's markdown is parsed when its item is first drawn, not when the row arrives.
        if let Some(Row::Prose { key, text }) = items::row_of(&self.rows, 0, &item) {
            let (key, len) = (*key, text.len());
            let live = item.decor.last && self.is_live(cx);
            let tree = Self::parse_reply(&mut self.markdown, &mut self.live_reply, key, text, live);
            self.probe_files(key, len, &tree, cx);
        }
        let Some(row) = items::row_of(&self.rows, 0, &item) else {
            return div().into_any_element();
        };
        let top = item.inner.is_none().then_some(ix);
        let body = self.row_body(row, top, &theme, cx);
        // "Thinking" under the last row while the agent works between steps.
        let thinking = item.decor.last && self.is_thinking(cx);
        // A row inside an open fold sits a step closer to its neighbours than a top-level row.
        let open_head = matches!(row, Row::Worked { key, .. } if self.open.contains(key));
        let gap = if item.inner.is_some() || open_head {
            12.0
        } else if item.decor.last {
            26.0
        } else {
            16.0
        };
        div()
            .w_full()
            .when(ix == 0, |d| d.pt(px(22.0)))
            .pb(px(if item.decor.last { 26.0 } else { gap }))
            .child(body)
            .children(self.activity_chips(&theme, item.decor.prose_at, cx))
            .when(thinking, |d| d.child(self.thinking_line(&theme, cx)))
            .into_any_element()
    }

    /// Whether the run is live: its last reply may still be arriving.
    pub(super) fn is_live(&self, cx: &Context<Self>) -> bool {
        self.summary(cx)
            .is_some_and(|r| r.status == RunStatus::Running)
    }

    /// The reply's tree for `text`, parsed from the last one when it only grew. `live` is the
    /// reply still arriving: it stays parsed however many others are drawn after it. Takes the
    /// two fields, not `self`: `text` is borrowed from the rows.
    pub(super) fn parse_reply(
        markdown: &mut Lru<Key, Streaming>,
        live_reply: &mut Option<Key>,
        key: Key,
        text: &str,
        live: bool,
    ) -> BlockTree {
        let mut stream = markdown.remove(&key).unwrap_or_default();
        stream.set_text(text);
        let tree = stream.tree().clone();
        markdown.insert(key, stream, text.len());
        if live {
            if let Some(before) = live_reply.replace(key).filter(|before| *before != key) {
                markdown.unpin(&before);
            }
            markdown.pin(key);
        } else if *live_reply == Some(key) {
            *live_reply = None;
            markdown.unpin(&key);
        }
        tree
    }

    /// The run a row belongs to: the thread's `n`th, in the high bits of its key.
    fn run_of(&self, key: Key) -> RunId {
        usize::try_from(key.0 >> 40)
            .ok()
            .and_then(|n| self.runs.get(n))
            .unwrap_or(&self.run)
            .clone()
    }

    /// An agent-posted visual, titled with its caption (TRANSCRIPT-12.1).
    fn visual_item(
        &self,
        ix: usize,
        v: usize,
        last: bool,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let Some(visual) = self.visuals.get(v) else {
            return div().into_any_element();
        };
        let run = self
            .items
            .items
            .get(ix)
            .and_then(|i| self.rows.get(i.top))
            .map_or_else(|| self.run.clone(), |row| self.run_of(row.key()));
        let client = self.store.read(cx).client();
        let host = Host::transcript(Some(run), client, Some(self.file_links(cx)));
        div()
            .w_full()
            .pb(px(if last { 26.0 } else { 16.0 }))
            .child(visual_view(visual, host, theme))
            .into_any_element()
    }

    /// Whether the run is live and between steps (see [`convo::thinking`]).
    fn is_thinking(&self, cx: &Context<Self>) -> bool {
        let live = self
            .summary(cx)
            .is_some_and(|r| r.status == RunStatus::Running);
        convo::thinking(&self.rows, live)
    }

    /// The shimmer's opacity now; fixed while motion is reduced.
    pub(super) fn shimmer(cx: &mut Context<Self>) -> f32 {
        let phase = motion::pulse_delta(&motion::PULSE, cx.entity_id(), cx);
        convo::shimmer_opacity(phase, cx.reduce_motion())
    }

    fn thinking_line(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let opacity = Self::shimmer(cx);
        div()
            .id("thinking")
            .debug_selector(|| "thinking".to_owned())
            .pt(px(14.0))
            .text_size(px(13.0))
            .text_color(theme.muted)
            .opacity(opacity)
            .child("Thinking")
            .into_any_element()
    }

    /// One row. `top` is its item's index in the list; a row inside a fold has none.
    fn row_body(
        &self,
        row: &Row,
        top: Option<usize>,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match row {
            Row::User {
                key,
                text,
                steer,
                at_ms,
            } => self.user_message(theme, *key, text, *steer, *at_ms, cx),
            Row::Prose { key, text } => {
                let Some(stream) = self.markdown.peek(key) else {
                    return div().into_any_element();
                };
                let item = top.and_then(|ix| self.items.items.get(ix));
                let actions = item
                    .filter(|item| item.decor.final_message)
                    .map(|_| self.message_actions(theme, *key, text));
                let streaming = item.is_some_and(|i| i.decor.last) && self.is_live(cx);
                // The reply is drawn for this run's worktree, with its find hits painted over the
                // words and its texts tracked for selection.
                let client = self.store.read(cx).client();
                let host =
                    Host::transcript(Some(self.run_of(*key)), client, Some(self.file_links(cx)))
                        .while_streaming(streaming)
                        .marked(self.find_marks(*key))
                        .selectable(Some(Rc::new(SelectHost {
                            registry: self.select.registry.clone(),
                            key: *key,
                            atoms: stream.atoms(),
                            selected: self.selected_in_prose(*key, text.len()),
                            style: pick::selection_style(theme),
                        })));
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_size(px(15.0))
                            .line_height(px(24.0))
                            .child(markdown_view::blocks_with(stream.tree(), theme, host)),
                    )
                    .children(actions)
                    .into_any_element()
            }
            Row::Reasoning { key, text } => self.reasoning(theme, *key, text, cx),
            Row::Tools { key, calls } => self.steps(theme, *key, calls, cx),
            Row::Question {
                questions,
                resolution,
                ..
            } => match convo::question_view(questions, resolution) {
                convo::QuestionView::Waiting(line) => {
                    event_line(theme, Icon::Question, theme.warning, &line)
                }
                convo::QuestionView::Answered(pairs) => answered_bubble(theme, &pairs),
                convo::QuestionView::Closed(line) => closed_line(theme, &line),
            },
            // The question itself is the approval panel's, in the composer's place; here it is
            // only a line, so the transcript still shows where the run stopped.
            Row::Approval { approval, .. } => event_line(
                theme,
                Icon::Shield,
                theme.warning,
                &format!(
                    "Waiting for your approval \u{b7} {}",
                    summary::subject_line(&approval.subject)
                ),
            ),
            Row::Asked { line, .. } => event_line(theme, Icon::Shield, theme.muted, line),
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
                    theme,
                    Icon::Route,
                    theme.muted,
                    &format!("Switched to {name}"),
                )
            }
            Row::Routing { key, view } => self.routing_card(theme, *key, view, cx),
            Row::Worked { key, secs, .. } => self.worked(theme, *key, *secs, cx),
            Row::Done {
                stop,
                result,
                diagnostic,
                reason,
                ..
            } => div()
                .child(done_line(
                    theme,
                    *stop,
                    result.as_deref(),
                    diagnostic.as_deref(),
                    reason.as_ref(),
                ))
                // Only the thread's latest run says what the branch holds.
                .children(
                    top.filter(|&ix| self.items.is_last_done(ix))
                        .and_then(|_| self.result_line(theme, cx)),
                )
                .into_any_element(),
            Row::Divider {
                executor,
                model,
                at_ms,
                ..
            } => self.divider(theme, executor, model.as_deref(), *at_ms, cx),
        }
    }

    /// "Worked for 3m 12s ›": a disclosure that opens the turn's interim text and steps in place.
    /// This is the fold's head only; while it is open its rows are list items of their own, right
    /// after it.
    fn worked(
        &self,
        theme: &Theme,
        key: Key,
        secs: Option<u64>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = self.open.contains(&key);
        let text = theme.text;
        let label = convo::worked_label(secs);
        div()
            .id(SharedString::from(format!("worked-{}", key.0)))
            .debug_selector({
                let id = format!("worked-{}", key.0);
                move || id.clone()
            })
            .flex()
            .items_center()
            .gap(px(4.0))
            .text_size(px(13.0))
            .text_color(theme.muted)
            .cursor_pointer()
            .hover(move |s| s.text_color(text))
            .child(self.text_piece(
                theme,
                Spot {
                    key,
                    slot: Slot::HEAD,
                    base: 0,
                    len: 0,
                    total: None,
                },
                &label,
                &[],
            ))
            .child(icon(
                if open { Icon::ChevDown } else { Icon::Fwd },
                13.0,
                theme.muted,
            ))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle(key, cx)))
            .into_any_element()
    }

    /// A user message: a neutral raised bubble on the right; the time and Copy show on hover, and
    /// a long one collapses behind "Show more".
    fn user_message(
        &self,
        theme: &Theme,
        key: Key,
        text: &str,
        steer: bool,
        at_ms: Option<u64>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shown = convo::user_text(text, self.open.contains(&key));
        let kept = selection::kept_len(&shown);
        let group = SharedString::from(format!("user-{}", key.0));
        let copy_text = text.to_owned();
        let toggle = shown.toggle.map(|label| {
            let selector = if self.open.contains(&key) {
                "user-less"
            } else {
                "user-more"
            };
            div()
                .id(selector)
                .debug_selector(move || selector.to_owned())
                .pt(px(6.0))
                .flex()
                .items_center()
                .gap(px(4.0))
                .text_size(px(13.0))
                .text_color(theme.muted)
                .cursor_pointer()
                .child(label)
                .child(icon(
                    if label == "Show more" {
                        Icon::ChevDown
                    } else {
                        Icon::ChevUp
                    },
                    12.0,
                    theme.muted,
                ))
                .on_click(cx.listener(move |this, _, _, cx| this.toggle(key, cx)))
        });
        let time = at_ms.map(|at| convo::clock_label(at, &chrono::Local));
        // The matches in the words shown (the cut part of a long message has none painted).
        let hits = self.find_highlights(key, find::Part::Whole);
        div()
            .group(group.clone())
            .w_full()
            .flex()
            .flex_col()
            .items_end()
            .gap(px(4.0))
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
                    .px(px(16.0))
                    .py(px(10.0))
                    .rounded(px(18.0))
                    .bg(theme.raised)
                    .text_size(px(15.0))
                    .line_height(px(23.0))
                    .child(self.text_piece(
                        theme,
                        Spot {
                            key,
                            slot: Slot::WHOLE,
                            base: 0,
                            len: kept,
                            total: Some(kept),
                        },
                        &shown.shown,
                        &hits,
                    ))
                    .children(toggle),
            )
            .child(
                div()
                    .h(px(22.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .opacity(0.0)
                    .group_hover(group, |s| s.opacity(1.0))
                    .children(time.map(|t| {
                        div()
                            .font_family(MONO)
                            .text_size(px(12.0))
                            .text_color(theme.faint)
                            .child(SharedString::from(t))
                    }))
                    .child(
                        kit::icon_button(theme, format!("user-copy-{}", key.0), Icon::Copy, 22.0)
                            .on_click(move |_, _, cx| {
                                output::copy(cx, copy_text.clone(), "Message copied");
                            }),
                    ),
            )
            .into_any_element()
    }

    /// The quiet icons under the final agent message of a finished turn.
    fn message_actions(&self, theme: &Theme, key: Key, text: &str) -> AnyElement {
        let text = text.to_owned();
        div()
            .flex()
            .items_center()
            .gap(px(2.0))
            .child(
                kit::icon_button(theme, format!("message-copy-{}", key.0), Icon::Copy, 24.0)
                    .on_click(move |_, _, cx| {
                        output::copy(cx, text.clone(), "Message copied");
                    }),
            )
            .into_any_element()
    }

    /// One muted line for each step of a tool group.
    pub(super) fn steps(
        &self,
        theme: &Theme,
        key: Key,
        calls: &[ToolEntry],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let lines = convo::step_lines(calls);
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .children(
                lines
                    .iter()
                    .enumerate()
                    .map(|(ln, line)| self.step_row(theme, (key, ln), line, calls, cx)),
            )
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

    /// "Routed to Codex \u{b7} Claude Code was signed out": one muted row; the "why" opens under it.
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
            .text_size(px(13.0))
            .text_color(theme.muted)
            .when(view.has_detail(), |d| {
                d.cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle(key, cx)))
            })
            .child(icon(Icon::Route, 14.0, theme.faint))
            .when_some(chosen, |d, who| {
                d.child(kit::exec_glyph(theme, &who.id, 14.0))
            })
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(convo::routing_line(view))),
            )
            .when(view.has_detail(), |d| {
                d.child(icon(
                    if open { Icon::ChevDown } else { Icon::Fwd },
                    13.0,
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
        let hits = self.find_highlights(key, find::Part::Whole);
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                disclosure(
                    theme,
                    SharedString::from(format!("reasoning-{}", key.0)),
                    open,
                    self.text_piece(
                        theme,
                        Spot {
                            key,
                            slot: Slot::HEAD,
                            base: 0,
                            len: 0,
                            total: None,
                        },
                        "Reasoning",
                        &[],
                    ),
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
                        .child(self.text_piece(
                            theme,
                            Spot {
                                key,
                                slot: Slot::WHOLE,
                                base: 0,
                                len: text.len(),
                                total: Some(text.len()),
                            },
                            text,
                            &hits,
                        )),
                )
            })
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

/// An answered question: the question muted above the answer, in the user's bubble.
fn answered_bubble(theme: &Theme, pairs: &[(String, Option<String>)]) -> AnyElement {
    div()
        .id("question-answered")
        .debug_selector(|| "question-answered".to_owned())
        .w_full()
        .flex()
        .flex_col()
        .items_end()
        .gap(px(10.0))
        .children(pairs.iter().map(|(question, answer)| {
            div()
                .max_w(px(560.0))
                .flex()
                .flex_col()
                .items_end()
                .gap(px(4.0))
                .child(
                    div()
                        .text_size(px(13.0))
                        .text_color(theme.muted)
                        .child(SharedString::from(question.clone())),
                )
                .child(
                    div()
                        .px(px(16.0))
                        .py(px(10.0))
                        .rounded(px(18.0))
                        .bg(theme.raised)
                        .text_size(px(15.0))
                        .line_height(px(23.0))
                        .child(SharedString::from(
                            answer.clone().unwrap_or_else(|| "Answered".to_owned()),
                        )),
                )
        }))
        .into_any_element()
}

/// A question that can no longer be answered: one muted line where it was asked.
fn closed_line(theme: &Theme, line: &str) -> AnyElement {
    div()
        .id("question-closed")
        .debug_selector(|| "question-closed".to_owned())
        .flex()
        .items_center()
        .gap(px(9.0))
        .text_size(px(13.0))
        .text_color(theme.muted)
        .child(icon(Icon::Question, 14.0, theme.faint))
        .child(SharedString::from(line.to_owned()))
        .into_any_element()
}

/// The clickable head of a collapsible row: a chevron and a label.
pub(super) fn disclosure(
    theme: &Theme,
    id: SharedString,
    open: bool,
    label: impl IntoElement,
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
