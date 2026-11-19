//! The question panel, attached above the steer box while the run has an open question
//! (ui-brief, "Question panel"). It never takes the keyboard: its digit, Enter and Esc keys work
//! only after a click on it or ⌘J (the mis-press guard). With a daemon that cannot answer yet it
//! is read-only and says where to answer.

use gpui::{
    AnyElement, Context, Entity, FontWeight, IntoElement, KeyBinding, Subscription, Window,
    actions, div, prelude::*, px,
};
use reno_client::answers::{AnswerError, AnswerRequest};

use super::RunScreen;
use crate::summary;
use crate::ui::composer::{Composer, ReasonEvent};
use crate::ui::icon::{Icon, icon};
use crate::ui::kit;
use crate::ui::theme;
use crate::vm::question_panel::{self, Draft, PanelModel, Refusal};

actions!(
    run_question,
    [
        CloseQuestion,
        SendQuestion,
        Pick1,
        Pick2,
        Pick3,
        Pick4,
        Pick5,
        Pick6,
        Pick7,
        Pick8,
        Pick9
    ]
);

/// The key context of the question panel; its keys work only while it has the keyboard.
const CONTEXT: &str = "QuestionPanel";

const DIGITS: [&str; 9] = ["1", "2", "3", "4", "5", "6", "7", "8", "9"];

pub(crate) fn bind_keys(cx: &mut gpui::App) {
    cx.bind_keys([
        KeyBinding::new("escape", CloseQuestion, Some(CONTEXT)),
        KeyBinding::new("enter", SendQuestion, Some(CONTEXT)),
        KeyBinding::new("1", Pick1, Some(CONTEXT)),
        KeyBinding::new("2", Pick2, Some(CONTEXT)),
        KeyBinding::new("3", Pick3, Some(CONTEXT)),
        KeyBinding::new("4", Pick4, Some(CONTEXT)),
        KeyBinding::new("5", Pick5, Some(CONTEXT)),
        KeyBinding::new("6", Pick6, Some(CONTEXT)),
        KeyBinding::new("7", Pick7, Some(CONTEXT)),
        KeyBinding::new("8", Pick8, Some(CONTEXT)),
        KeyBinding::new("9", Pick9, Some(CONTEXT)),
    ]);
}

/// The open question the panel is for, and what the owner did with it so far.
pub(super) struct QuestionState {
    request: String,
    model: PanelModel,
    pub(super) draft: Draft,
    pub(super) refusal: Option<Refusal>,
    /// The "write your own response" line.
    pub(super) input: Entity<Composer>,
    /// Esc or the × closed the panel; the chip reopens it.
    closed: bool,
    _events: Subscription,
}

impl RunScreen {
    /// Keeps the panel's state in step with the run: made when a question opens, dropped when it
    /// is answered, declined or closed.
    fn sync_question(&mut self, cx: &mut Context<Self>) {
        let open = {
            let store = self.store.read(cx);
            let state = store.state();
            let status = state.run(&self.run).map(|r| r.status);
            let events: Vec<_> = state
                .transcript(&self.run)
                .map(|t| t.events.iter().map(|(_, e)| e.clone()).collect())
                .unwrap_or_default();
            status.and_then(|s| question_panel::open_question(s, &events))
        };
        let Some((request, questions)) = open else {
            self.question = None;
            return;
        };
        if self.question.as_ref().is_some_and(|q| q.request == request) {
            return;
        }
        let rules = self.store.read(cx).question_rules(&self.run, &request);
        let model = PanelModel::new(&questions, rules);
        let store = self.store.clone();
        let input = cx.new(|cx| Composer::line(store, "Or write your own response", cx));
        let events = cx.subscribe(&input, |this, _, event: &ReasonEvent, cx| match event {
            ReasonEvent::Submitted(_) => this.send_question(cx),
            ReasonEvent::Cancelled => this.pending_focus_panel = true,
        });
        self.question = Some(QuestionState {
            request,
            draft: Draft::new(&model),
            model,
            refusal: None,
            input,
            closed: false,
            _events: events,
        });
    }

    /// Whether the panel (or the line inside it) has the keyboard: it was clicked or jumped to.
    fn question_claimed(&self, window: &Window, cx: &gpui::App) -> bool {
        self.question.is_some() && self.question_focus.contains_focused(window, cx)
    }

    /// Picks option `ix` (0-based) on the current page; a single choice sends at once.
    fn pick_question(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(q) = self.question.as_mut() else {
            return;
        };
        if !self.store.read(cx).capabilities().answer_questions {
            return;
        }
        q.refusal = None;
        // A pick replaces what was typed.
        q.draft.type_text("");
        q.input.update(cx, |input, cx| input.clear(cx));
        q.draft.pick(&q.model, ix);
        if q.model.sends_on_click() {
            self.send_question(cx);
        }
        cx.notify();
    }

    fn send_question(&mut self, cx: &mut Context<Self>) {
        let Some(q) = self.question.as_mut() else {
            return;
        };
        if let Some(typed) = q.input.read(cx).typed_reason() {
            q.draft.type_text(&typed);
        }
        let answers = q.draft.answers(&q.model);
        if let Err(refusal) = question_panel::check(&q.model, &answers) {
            q.refusal = Some(refusal);
            cx.notify();
            return;
        }
        let request = AnswerRequest {
            question_id: q.request.clone(),
            answers,
        };
        self.submit_answer(request, cx);
    }

    /// Skip: declines the whole set.
    fn skip_question(&mut self, cx: &mut Context<Self>) {
        let Some(q) = self.question.as_ref() else {
            return;
        };
        let request = AnswerRequest {
            question_id: q.request.clone(),
            answers: Vec::new(),
        };
        self.submit_answer(request, cx);
    }

    fn submit_answer(&mut self, request: AnswerRequest, cx: &mut Context<Self>) {
        let run = self.run.clone();
        let result = self
            .store
            .update(cx, |store, cx| store.answer_question(&run, request, cx));
        match result {
            // The journal's answer arrives and the panel goes with the question.
            Ok(()) => {}
            Err(AnswerError::AnswerInvalid { allowed, .. }) => {
                if let Some(q) = self.question.as_mut() {
                    q.refusal = Some(Refusal::OptionsOnly { allowed });
                }
            }
            Err(AnswerError::QuestionNotPending) => self.question = None,
        }
        cx.notify();
    }

    fn close_question(&mut self, cx: &mut Context<Self>) {
        if let Some(q) = self.question.as_mut() {
            q.closed = true;
        }
        cx.notify();
    }

    /// ⌘J with a question waiting: reopens it if it was closed and gives it the keyboard.
    pub(super) fn jump_to_question(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(q) = self.question.as_mut() else {
            return false;
        };
        q.closed = false;
        window.focus(&self.question_focus, cx);
        cx.notify();
        true
    }

    /// The question's chip or panel, above the steer box.
    pub(super) fn question_dock(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.sync_question(cx);
        if std::mem::take(&mut self.pending_focus_panel) {
            window.focus(&self.question_focus, cx);
        }
        let closed = self.question.as_ref()?.closed;
        Some(if closed {
            self.question_chip(cx)
        } else {
            self.question_panel(window, cx)
        })
    }

    fn question_chip(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme::current(cx);
        div()
            .flex()
            .child(
                kit::chip(&theme, "Question waiting \u{b7} Open")
                    .id("question-chip")
                    .debug_selector(|| "question-chip".to_owned())
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.jump_to_question(window, cx);
                    })),
            )
            .into_any_element()
    }

    fn question_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let theme = theme::current(cx);
        let answerable = self.store.read(cx).capabilities().answer_questions;
        let agent = self.store.read(cx).state().run(&self.run).map_or_else(
            || "The agent".to_owned(),
            |r| summary::executor_name(r.executor.as_str()),
        );
        let claimed = self.question_claimed(window, cx);
        let Some(q) = self.question.as_ref() else {
            return div().into_any_element();
        };
        let page = q.draft.page.min(q.model.pages.len().saturating_sub(1));
        let Some(view) = q.model.pages.get(page).cloned() else {
            return div().into_any_element();
        };
        let counter = q.model.counter(page);
        let last = q.draft.on_last_page(&q.model);
        let has_answer =
            !q.draft.answers(&q.model).is_empty() || q.input.read(cx).typed_reason().is_some();
        let refusal = q.refusal.as_ref().map(|r| r.message(&agent));
        let input = q.input.clone();

        let header = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .text_size(px(13.5))
            .text_color(theme.muted)
            .child(icon(Icon::Question, 15.0, theme.muted))
            .child(
                div()
                    .id("question-header")
                    .debug_selector(|| "question-header".to_owned())
                    .child("Question"),
            )
            .children(counter.map(|c| {
                div()
                    .id("question-counter")
                    .debug_selector(|| "question-counter".to_owned())
                    .text_color(theme.faint)
                    .child(c)
            }))
            .child(div().flex_1())
            .child(
                kit::icon_button(&theme, "question-close", Icon::X, 26.0)
                    .on_click(cx.listener(|this, _, _, cx| this.close_question(cx))),
            );

        let prompt = div()
            .id("question-prompt")
            .debug_selector(|| "question-prompt".to_owned())
            .text_size(px(15.0))
            .font_weight(FontWeight::MEDIUM)
            .text_color(theme.text)
            .child(view.prompt.clone());

        let options =
            div()
                .flex()
                .flex_col()
                .gap(px(2.0))
                .children(view.options.iter().enumerate().map(|(ix, option)| {
                    let picked = q.draft.is_picked(ix);
                    let row = div()
                        .flex()
                        .items_center()
                        .gap(px(12.0))
                        .px(px(8.0))
                        .h(px(36.0))
                        .rounded(px(10.0));
                    if !answerable {
                        // Read-only: what it offered, as plain text. Nothing here answers.
                        return row
                            .text_size(px(14.0))
                            .text_color(theme.muted)
                            .child(option.label.clone())
                            .into_any_element();
                    }
                    let keycap = DIGITS.get(ix).map(|d| kit::keycap(&theme, d, None));
                    let hover = theme.raised;
                    row.id(gpui::SharedString::from(format!(
                        "question-option-{}",
                        ix + 1
                    )))
                    .debug_selector(move || format!("question-option-{}", ix + 1))
                    .cursor_pointer()
                    .when(picked, |d| d.bg(theme.raised))
                    .hover(move |s| s.bg(hover))
                    .text_size(px(14.0))
                    .text_color(theme.text)
                    .on_click(cx.listener(move |this, _, _, cx| this.pick_question(ix, cx)))
                    .children(keycap)
                    .child(option.label.clone())
                    .into_any_element()
                }));

        let write_own = div()
            .id("question-write-own")
            .debug_selector(|| "question-write-own".to_owned())
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(icon(Icon::Pencil, 14.0, theme.muted))
            .child(div().flex_1().child(input.clone()))
            .on_click({
                let input = input.clone();
                cx.listener(move |_, _, window, cx| {
                    let focus = input.read(cx).focus_handle_ref().clone();
                    window.focus(&focus, cx);
                })
            });

        let on_fill = theme.on_fill(theme.accent);
        let foot = div()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(write_own)
            .child(
                kit::pill_button(&theme, "question-skip", "Skip", kit::Tone::Quiet)
                    .on_click(cx.listener(|this, _, _, cx| this.skip_question(cx))),
            )
            .child(if last {
                kit::pill_button_enabled(
                    &theme,
                    "question-send",
                    "Send",
                    kit::Tone::Primary,
                    has_answer,
                )
                .child(kit::keycap(&theme, "\u{23ce}", Some(on_fill)))
                .when(has_answer, |b| {
                    b.on_click(cx.listener(|this, _, _, cx| this.send_question(cx)))
                })
            } else {
                kit::pill_button(&theme, "question-next", "Next", kit::Tone::Primary).on_click(
                    cx.listener(|this, _, _, cx| {
                        if let Some(q) = this.question.as_mut() {
                            q.refusal = None;
                            q.draft.next(&q.model);
                        }
                        cx.notify();
                    }),
                )
            });

        let readonly_note = (!answerable).then(|| {
            div()
                .id("question-readonly-note")
                .debug_selector(|| "question-readonly-note".to_owned())
                .text_size(px(13.5))
                .text_color(theme.muted)
                .child(format!("Answer it in {agent} for now"))
        });

        let pick = |ix: usize| {
            move |this: &mut RunScreen, window: &mut Window, cx: &mut Context<RunScreen>| {
                if !this.question_claimed(window, cx) {
                    return;
                }
                // Typing into the line is typing, not picking.
                let typing = this
                    .question
                    .as_ref()
                    .is_some_and(|q| q.input.read(cx).focus_handle_ref().is_focused(window));
                if typing {
                    cx.propagate();
                } else {
                    this.pick_question(ix, cx);
                }
            }
        };

        let mut panel = div()
            .id("question-panel")
            .debug_selector(|| "question-panel".to_owned())
            .key_context(CONTEXT)
            .track_focus(&self.question_focus)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    // A click on the panel is the owner's choice to answer it by keyboard, unless
                    // it went to the line, which takes the keyboard itself.
                    let typing = this
                        .question
                        .as_ref()
                        .is_some_and(|q| q.input.read(cx).focus_handle_ref().is_focused(window));
                    if !typing {
                        window.focus(&this.question_focus, cx);
                    }
                }),
            )
            .on_action(cx.listener(|this, _: &CloseQuestion, window, cx| {
                if this.question_claimed(window, cx) {
                    this.close_question(cx);
                }
            }))
            .on_action(cx.listener(|this, _: &SendQuestion, window, cx| {
                if !this.question_claimed(window, cx)
                    || !this.store.read(cx).capabilities().answer_questions
                {
                    return;
                }
                let last = this
                    .question
                    .as_ref()
                    .is_some_and(|q| q.draft.on_last_page(&q.model));
                if last {
                    this.send_question(cx);
                } else if let Some(q) = this.question.as_mut() {
                    q.draft.next(&q.model);
                    cx.notify();
                }
            }));
        macro_rules! digit {
            ($action:ident, $ix:expr) => {
                panel = panel.on_action(cx.listener(move |this, _: &$action, window, cx| {
                    pick($ix)(this, window, cx);
                }));
            };
        }
        digit!(Pick1, 0);
        digit!(Pick2, 1);
        digit!(Pick3, 2);
        digit!(Pick4, 3);
        digit!(Pick5, 4);
        digit!(Pick6, 5);
        digit!(Pick7, 6);
        digit!(Pick8, 7);
        digit!(Pick9, 8);

        panel
            .w_full()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .p(px(18.0))
            .rounded(px(26.0))
            .bg(theme.surface)
            .border_1()
            .border_color(if claimed {
                theme.text.opacity(0.2)
            } else {
                theme.hairline
            })
            .child(header)
            .child(prompt)
            .child(options)
            .children(refusal.map(|line| {
                div()
                    .id("question-refusal")
                    .debug_selector(|| "question-refusal".to_owned())
                    .text_size(px(13.0))
                    .text_color(theme.warning)
                    .child(line)
            }))
            .children(readonly_note)
            .when(answerable, |d| d.child(foot))
            .into_any_element()
    }
}
