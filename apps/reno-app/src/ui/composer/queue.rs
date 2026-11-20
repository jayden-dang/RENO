//! The message queue: what the owner wrote while the turn was still going, as a card above the
//! box in the run-activity tray's language. The list is the store's (`Store::queue`, the mocked
//! contract) and is read at every render; this file keeps only the view state around it (which
//! message is being edited). Rows add, edit, reorder (drag the handle), remove and send now,
//! all through the store.

use gpui::{
    AnyElement, Context, FocusHandle, Hsla, KeyDownEvent, SharedString, Window, div, prelude::*, px,
};
use reno_client::queue::QueuedMessage;
use reno_proto::RunId;

use super::places::typed;
use super::{Composer, Mode, SteerState};
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, Tone};
use crate::ui::theme::Theme;
use crate::vm::queue::{drop_place, queue_finding};

pub(in crate::ui::composer) struct QueueState {
    /// The message being edited in its row, and what it reads now.
    editing: Option<(String, String)>,
    focus: FocusHandle,
    /// Why the last change was not taken.
    pub(in crate::ui::composer) error: Option<String>,
}

impl QueueState {
    pub(in crate::ui::composer) fn new(cx: &mut Context<Composer>) -> Self {
        Self {
            editing: None,
            focus: cx.focus_handle(),
            error: None,
        }
    }
}

/// A queued message being dragged to a new place.
#[derive(Clone)]
struct Drag {
    id: String,
    text: String,
}

struct DragGhost(String);

impl gpui::Render for DragGhost {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = crate::ui::theme::current(cx);
        div()
            .max_w(px(320.0))
            .px(px(10.0))
            .py(px(6.0))
            .rounded(px(8.0))
            .bg(theme.surface)
            .border_1()
            .border_color(theme.accent.opacity(0.6))
            .shadow_md()
            .text_size(px(12.5))
            .text_color(theme.text)
            .truncate()
            .child(SharedString::from(self.0.clone()))
    }
}

impl Composer {
    /// The run this box steers, when the queue is on for it.
    pub(in crate::ui::composer) fn queue_run(&self, cx: &gpui::App) -> Option<RunId> {
        let Mode::Steer(run) = &self.mode else {
            return None;
        };
        self.store.read(cx).queue(run)?;
        Some(run.clone())
    }

    /// What waits, in delivery order; empty when the queue is off.
    pub(in crate::ui::composer) fn queue_items(&self, cx: &gpui::App) -> Vec<QueuedMessage> {
        self.queue_run(cx)
            .and_then(|run| self.store.read(cx).queue(&run))
            .unwrap_or_default()
    }

    /// Whether the box can queue a message now (a live run with the queue on).
    pub(in crate::ui::composer) fn queue_can_add(&self, cx: &gpui::App) -> bool {
        self.queue_run(cx).is_some()
            && self.model.can_submit()
            && matches!(self.steer_state(cx), Some(SteerState::Steering))
    }

    /// The line above the box: why the last change was refused.
    pub(in crate::ui::composer) fn queue_note(
        &self,
        theme: &Theme,
    ) -> Option<(SharedString, Hsla)> {
        self.queue
            .error
            .as_ref()
            .map(|error| (error.clone().into(), theme.warning))
    }

    // ---- changes, all through the store ------------------------------------------------------

    /// ⌥ Enter and the Queue button: the draft joins the queue and the box empties.
    pub(in crate::ui::composer) fn queue_add(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(run) = self.queue_run(cx) else {
            return;
        };
        let text = self.model.text().trim().to_owned();
        if text.is_empty() {
            return;
        }
        let result = self
            .store
            .update(cx, |store, cx| store.queue_add(&run, &text, cx));
        match result {
            Ok(_) => {
                self.queue.error = None;
                self.model.set_text("");
                self.text_changed(window, cx);
            }
            Err(error) => self.queue.error = Some(error.to_string()),
        }
        cx.notify();
    }

    pub(in crate::ui::composer) fn queue_remove(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(run) = self.queue_run(cx) {
            let result = self
                .store
                .update(cx, |store, cx| store.queue_remove(&run, id, cx));
            self.queue.error = result.err().map(|e| e.to_string());
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    pub(in crate::ui::composer) fn queue_send_now(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(run) = self.queue_run(cx) {
            let result = self
                .store
                .update(cx, |store, cx| store.queue_send_now(&run, id, cx));
            self.queue.error = result.err().map(|e| e.to_string());
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    pub(in crate::ui::composer) fn queue_move(
        &mut self,
        id: &str,
        to: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(run) = self.queue_run(cx) {
            let len = self.queue_items(cx).len();
            let to = drop_place(len, to);
            let result = self
                .store
                .update(cx, |store, cx| store.queue_move(&run, id, to, cx));
            self.queue.error = result.err().map(|e| e.to_string());
        }
        cx.notify();
    }

    // ---- editing a message -------------------------------------------------------------------

    /// ✎ (A, C): the row turns into a field.
    pub(in crate::ui::composer) fn queue_edit_in_row(
        &mut self,
        message: &QueuedMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.queue.editing = Some((message.id.clone(), message.text.clone()));
        window.focus(&self.queue.focus, cx);
        cx.notify();
    }

    pub(in crate::ui::composer) fn queue_edit_done(
        &mut self,
        save: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((id, text)) = self.queue.editing.take() {
            if save && let Some(run) = self.queue_run(cx) {
                let result = self
                    .store
                    .update(cx, |store, cx| store.queue_edit(&run, &id, &text, cx));
                self.queue.error = result.err().map(|e| e.to_string());
            }
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    pub(in crate::ui::composer) fn queue_edit_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = &event.keystroke;
        match key.key.as_str() {
            "enter" => self.queue_edit_done(true, window, cx),
            "escape" => self.queue_edit_done(false, window, cx),
            "backspace" => {
                if let Some((_, text)) = self.queue.editing.as_mut() {
                    text.pop();
                }
                cx.notify();
            }
            _ => {
                if let Some(typed) = typed(key)
                    && let Some((_, text)) = self.queue.editing.as_mut()
                {
                    text.push_str(&typed);
                    cx.notify();
                }
            }
        }
    }

    // ---- drawing -----------------------------------------------------------------------------

    /// A small button in a row: no focus taken, the box gets the keyboard back.
    pub(in crate::ui::composer) fn queue_mini(
        &self,
        theme: &Theme,
        id: String,
        glyph: Icon,
    ) -> gpui::Stateful<gpui::Div> {
        kit::icon_button(theme, id, glyph, 24.0)
    }

    /// One message as a row of A's card and C's list.
    pub(in crate::ui::composer) fn queue_row(
        &self,
        theme: &Theme,
        at: usize,
        message: &QueuedMessage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = message.id.clone();
        let editing = self.queue.editing.clone().filter(|(eid, _)| *eid == id);
        let drag = Drag {
            id: id.clone(),
            text: message.text.clone(),
        };
        let body: AnyElement = match editing {
            Some((_, text)) => div()
                .id(SharedString::from(format!("queue-edit-{at}")))
                .debug_selector(move || format!("queue-edit-{at}"))
                .flex_1()
                .min_w_0()
                .h(px(26.0))
                .px(px(8.0))
                .flex()
                .items_center()
                .rounded(px(7.0))
                .border_1()
                .border_color(theme.accent.opacity(0.6))
                .track_focus(&self.queue.focus)
                .on_key_down(
                    cx.listener(|this, event, window, cx| this.queue_edit_key(event, window, cx)),
                )
                .child(div().truncate().child(SharedString::from(text)))
                .child(div().flex_none().w(px(1.5)).h(px(14.0)).bg(theme.text))
                .into_any_element(),
            None => div()
                .id(SharedString::from(format!("queue-text-{at}")))
                .debug_selector(move || format!("queue-text-{at}"))
                .flex_1()
                .min_w_0()
                .truncate()
                .text_color(theme.text)
                .child(SharedString::from(message.text.clone()))
                .into_any_element(),
        };
        let (edit_msg, remove_id, send_id) = (message.clone(), id.clone(), id.clone());
        div()
            .id(SharedString::from(format!("queue-row-{at}")))
            .debug_selector(move || format!("queue-row-{at}"))
            .h(px(34.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .text_size(px(13.0))
            .rounded(px(7.0))
            .drag_over::<Drag>({
                let line = theme.accent;
                move |style, _, _, _| style.border_t_2().border_color(line)
            })
            .on_drop(cx.listener(move |this, dragged: &Drag, _, cx| {
                this.queue_move(&dragged.id, at, cx);
            }))
            .child(
                div()
                    .id(SharedString::from(format!("queue-handle-{at}")))
                    .debug_selector(move || format!("queue-handle-{at}"))
                    .flex_none()
                    .w(px(14.0))
                    .cursor_grab()
                    .text_color(theme.faint)
                    .text_size(px(12.0))
                    .child("\u{22ee}\u{22ee}")
                    .on_drag(drag, |dragged: &Drag, _, _, cx| {
                        cx.new(|_| DragGhost(dragged.text.clone()))
                    }),
            )
            .child(body)
            .child(
                self.queue_mini(theme, format!("queue-edit-btn-{at}"), Icon::Pencil)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.queue_edit_in_row(&edit_msg, window, cx);
                    })),
            )
            .child(
                self.queue_mini(theme, format!("queue-remove-{at}"), Icon::X)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.queue_remove(&remove_id, window, cx);
                    })),
            )
            .child(
                kit::line_button(
                    theme,
                    format!("queue-send-now-{at}"),
                    None,
                    "Send now",
                    Tone::Quiet,
                    true,
                )
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.queue_send_now(&send_id, window, cx);
                })),
            )
            .into_any_element()
    }

    /// A: the card above the box, in the run-activity tray's language.
    pub(in crate::ui::composer) fn queue_card(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let items = self.queue_items(cx);
        let finding = queue_finding(items.len())?;
        let rows: Vec<AnyElement> = items
            .iter()
            .enumerate()
            .map(|(at, m)| self.queue_row(theme, at, m, cx))
            .collect();
        Some(
            div()
                .id("queue-card")
                .debug_selector(|| "queue-card".to_owned())
                .mb(px(8.0))
                .px(px(14.0))
                .py(px(4.0))
                .flex()
                .flex_col()
                .rounded(px(12.0))
                .bg(theme.surface)
                .border_1()
                .border_color(theme.hairline)
                .child(
                    div()
                        .h(px(30.0))
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(icon(Icon::List, 14.0, theme.muted))
                        .child(
                            div()
                                .id("queue-finding")
                                .debug_selector(|| "queue-finding".to_owned())
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .text_size(px(13.0))
                                .child(SharedString::from(finding)),
                        )
                        .child(kit::tag(
                            theme.muted,
                            format!("Queued \u{b7} {}", items.len()),
                        )),
                )
                .child(div().h(px(1.0)).bg(theme.hairline).mb(px(2.0)))
                .children(rows)
                .into_any_element(),
        )
    }

    /// The Queue button, beside the send button, while there is something to queue.
    pub(in crate::ui::composer) fn queue_button(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.queue_can_add(cx) {
            return None;
        }
        Some(
            kit::line_button(theme, "queue-add", None, "Queue", Tone::Quiet, true)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.queue_add(window, cx);
                    window.focus(&this.focus_handle, cx);
                }))
                .into_any_element(),
        )
    }
}

impl Composer {
    /// `RENO_COMPOSER_OPEN=queue-edit`: the first waiting message turns into its field.
    pub(in crate::ui::composer) fn queue_demo_edit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(first) = self.queue_items(cx).first().cloned() {
            self.queue_edit_in_row(&first, window, cx);
        }
    }
}

#[cfg(test)]
impl Composer {
    /// ⌥ Enter, as a test presses it.
    pub(crate) fn queue_add_for_test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.queue_add(window, cx);
    }

    /// Why the last queue change was refused, as the box says it.
    pub(crate) fn queue_error_for_test(&self) -> Option<String> {
        self.queue.error.clone()
    }

    /// The card's finding sentence.
    pub(crate) fn queue_finding_for_test(&self, cx: &gpui::App) -> Option<String> {
        queue_finding(self.queue_items(cx).len())
    }

    /// The texts that wait, in order.
    pub(crate) fn queue_texts_for_test(&self, cx: &gpui::App) -> Vec<String> {
        self.queue_items(cx).into_iter().map(|m| m.text).collect()
    }

    /// ✎, ×, and Send now, as a test presses them (by place in the queue).
    pub(crate) fn queue_remove_for_test(
        &mut self,
        at: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(m) = self.queue_items(cx).get(at).cloned() {
            self.queue_remove(&m.id, window, cx);
        }
    }

    pub(crate) fn queue_send_now_for_test(
        &mut self,
        at: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(m) = self.queue_items(cx).get(at).cloned() {
            self.queue_send_now(&m.id, window, cx);
        }
    }

    /// ✎ then typing `text` then Enter: the message at `at` is rewritten in its row.
    pub(crate) fn queue_edit_for_test(
        &mut self,
        at: usize,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(m) = self.queue_items(cx).get(at).cloned() {
            self.queue_edit_in_row(&m, window, cx);
            self.queue.editing = Some((m.id, text.to_owned()));
            self.queue_edit_done(true, window, cx);
        }
    }
}
