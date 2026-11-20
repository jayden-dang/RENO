//! ROAD-31 draft, the message queue: what the owner wrote while the turn was still going. The
//! list is the store's (`Store::queue`, the mocked contract) and is read at every render; this
//! file keeps only the view state around it (which message is being edited, whether C's list
//! is open). A is a card above the box, B numbered pills inside it, C a chip in the foot that
//! opens a list. All of them add, edit, reorder, remove and send now through the store.

use gpui::{
    AnyElement, Context, FocusHandle, FontWeight, Hsla, KeyDownEvent, MouseButton, SharedString,
    Window, canvas, div, prelude::*, px,
};
use reno_client::queue::QueuedMessage;
use reno_proto::RunId;

use super::place::typed;
use super::{Anchor, Composer, Mode};
use crate::ui::composer::menu::menu;
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, Tone};
use crate::ui::theme::Theme;
use crate::vm::draft31::{drop_place, queue_chip, queue_finding};

pub(in crate::ui::composer) struct QueueState {
    /// C's list is open.
    pub(in crate::ui::composer) open: bool,
    pub(in crate::ui::composer) chip: Anchor,
    /// A and C: the message being edited in its row, and what it reads now.
    editing: Option<(String, String)>,
    focus: FocusHandle,
    /// Why the last change was not taken.
    pub(in crate::ui::composer) error: Option<String>,
    /// B: a message pulled into the box to be edited, and the draft it displaced.
    box_edit: Option<BoxEdit>,
}

struct BoxEdit {
    id: String,
    stashed: String,
}

impl QueueState {
    pub(in crate::ui::composer) fn new(cx: &mut Context<Composer>) -> Self {
        Self {
            open: false,
            chip: Anchor::default(),
            editing: None,
            focus: cx.focus_handle(),
            error: None,
            box_edit: None,
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
    pub(in crate::ui::composer) fn road31_queue_run(&self, cx: &gpui::App) -> Option<RunId> {
        self.road31(cx)?;
        let Mode::Steer(run) = &self.mode else {
            return None;
        };
        self.store.read(cx).queue(run)?;
        Some(run.clone())
    }

    /// What waits, in delivery order; empty when the queue is off.
    pub(in crate::ui::composer) fn road31_queue_items(&self, cx: &gpui::App) -> Vec<QueuedMessage> {
        self.road31_queue_run(cx)
            .and_then(|run| self.store.read(cx).queue(&run))
            .unwrap_or_default()
    }

    /// Whether the box can queue a message now (a live run with the queue on).
    pub(in crate::ui::composer) fn road31_can_queue(&self, cx: &gpui::App) -> bool {
        self.road31_queue_run(cx).is_some()
            && self.model.can_submit()
            && matches!(
                self.steer_state(cx),
                Some(super::super::SteerState::Steering)
            )
    }

    /// The line above the box: the queue's finding (C) or why a change was refused.
    pub(in crate::ui::composer) fn road31_queue_note(
        &self,
        theme: &Theme,
    ) -> Option<(SharedString, Hsla)> {
        if let Some(error) = &self.road31.queue.error {
            return Some((error.clone().into(), theme.warning));
        }
        None
    }

    /// C: the finding in words, with the "2 queued" chip that opens the list at its end.
    pub(in crate::ui::composer) fn road31_queue_line(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let finding = queue_finding(self.road31_queue_items(cx).len())?;
        let chip = self.road31_queue_chip(theme, cx)?;
        Some(
            div()
                .id("queue-line")
                .debug_selector(|| "queue-line".to_owned())
                .px(px(8.0))
                .pb(px(6.0))
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(
                    div()
                        .id("queue-finding")
                        .debug_selector(|| "queue-finding".to_owned())
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(12.5))
                        .text_color(theme.muted)
                        .child(SharedString::from(finding)),
                )
                .child(chip)
                .into_any_element(),
        )
    }

    // ---- changes, all through the store ------------------------------------------------------

    /// ⌥ Enter and the Queue button: the draft joins the queue and the box empties.
    pub(in crate::ui::composer) fn road31_queue_add(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(run) = self.road31_queue_run(cx) else {
            return;
        };
        let text = self.model.text().trim().to_owned();
        if text.is_empty() {
            return;
        }
        if let Some(edit) = self.road31.queue.box_edit.take() {
            // B: a pill pulled into the box is saved back to its place, and the draft returns.
            let result = self
                .store
                .update(cx, |store, cx| store.queue_edit(&run, &edit.id, &text, cx));
            self.road31.queue.error = result.err().map(|e| e.to_string());
            self.model.set_text(&edit.stashed);
            self.text_changed(window, cx);
            return;
        }
        let result = self
            .store
            .update(cx, |store, cx| store.queue_add(&run, &text, cx));
        match result {
            Ok(_) => {
                self.road31.queue.error = None;
                self.model.set_text("");
                self.text_changed(window, cx);
            }
            Err(error) => self.road31.queue.error = Some(error.to_string()),
        }
        cx.notify();
    }

    pub(in crate::ui::composer) fn road31_queue_remove(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(run) = self.road31_queue_run(cx) {
            let result = self
                .store
                .update(cx, |store, cx| store.queue_remove(&run, id, cx));
            self.road31.queue.error = result.err().map(|e| e.to_string());
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    pub(in crate::ui::composer) fn road31_queue_send_now(
        &mut self,
        id: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(run) = self.road31_queue_run(cx) {
            let result = self
                .store
                .update(cx, |store, cx| store.queue_send_now(&run, id, cx));
            self.road31.queue.error = result.err().map(|e| e.to_string());
        }
        window.focus(&self.focus_handle, cx);
        cx.notify();
    }

    pub(in crate::ui::composer) fn road31_queue_move(
        &mut self,
        id: &str,
        to: usize,
        cx: &mut Context<Self>,
    ) {
        if let Some(run) = self.road31_queue_run(cx) {
            let len = self.road31_queue_items(cx).len();
            let to = drop_place(len, to);
            let result = self
                .store
                .update(cx, |store, cx| store.queue_move(&run, id, to, cx));
            self.road31.queue.error = result.err().map(|e| e.to_string());
        }
        cx.notify();
    }

    // ---- editing a message -------------------------------------------------------------------

    /// ✎ (A, C): the row turns into a field.
    pub(in crate::ui::composer) fn road31_edit_in_row(
        &mut self,
        message: &QueuedMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.road31.queue.editing = Some((message.id.clone(), message.text.clone()));
        window.focus(&self.road31.queue.focus, cx);
        cx.notify();
    }

    pub(in crate::ui::composer) fn road31_edit_done(
        &mut self,
        save: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some((id, text)) = self.road31.queue.editing.take() {
            if save && let Some(run) = self.road31_queue_run(cx) {
                let result = self
                    .store
                    .update(cx, |store, cx| store.queue_edit(&run, &id, &text, cx));
                self.road31.queue.error = result.err().map(|e| e.to_string());
            }
            window.focus(&self.focus_handle, cx);
            cx.notify();
        }
    }

    pub(in crate::ui::composer) fn road31_edit_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = &event.keystroke;
        match key.key.as_str() {
            "enter" => self.road31_edit_done(true, window, cx),
            "escape" => self.road31_edit_done(false, window, cx),
            "backspace" => {
                if let Some((_, text)) = self.road31.queue.editing.as_mut() {
                    text.pop();
                }
                cx.notify();
            }
            _ => {
                if let Some(typed) = typed(key)
                    && let Some((_, text)) = self.road31.queue.editing.as_mut()
                {
                    text.push_str(&typed);
                    cx.notify();
                }
            }
        }
    }

    /// B: a pill is pulled into the box; the draft that was there waits, and comes back after.
    pub(in crate::ui::composer) fn road31_edit_in_box(
        &mut self,
        message: &QueuedMessage,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let stashed = match self.road31.queue.box_edit.take() {
            // Pulling a second pill keeps the first draft, not the first pill's text.
            Some(edit) => edit.stashed,
            None => self.model.text().to_owned(),
        };
        self.road31.queue.box_edit = Some(BoxEdit {
            id: message.id.clone(),
            stashed,
        });
        self.model.set_text(&message.text);
        window.focus(&self.focus_handle, cx);
        self.text_changed(window, cx);
    }

    /// Esc while a pill is in the box: it goes back unchanged and the draft returns.
    pub(in crate::ui::composer) fn road31_cancel_box_edit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(edit) = self.road31.queue.box_edit.take() else {
            return false;
        };
        self.model.set_text(&edit.stashed);
        self.text_changed(window, cx);
        true
    }

    /// Enter in the box while a pill is pulled in: saves it in place.
    pub(in crate::ui::composer) fn road31_submit_box_edit(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.road31.queue.box_edit.is_none() {
            return false;
        }
        self.road31_queue_add(window, cx);
        true
    }

    /// The id of the pill being edited in the box (for the pill's look).
    pub(in crate::ui::composer) fn road31_box_editing(&self) -> Option<&str> {
        self.road31.queue.box_edit.as_ref().map(|e| e.id.as_str())
    }

    pub(in crate::ui::composer) fn road31_open_queue(&mut self, cx: &mut Context<Self>) {
        self.road31.queue.open = !self.road31.queue.open;
        self.sync_overlay(cx);
        cx.notify();
    }

    // ---- drawing -----------------------------------------------------------------------------

    /// A small button in a row: no focus taken, the box gets the keyboard back.
    pub(in crate::ui::composer) fn road31_mini(
        &self,
        theme: &Theme,
        id: String,
        glyph: Icon,
    ) -> gpui::Stateful<gpui::Div> {
        kit::icon_button(theme, id, glyph, 24.0)
    }

    /// One message as a row of A's card and C's list.
    pub(in crate::ui::composer) fn road31_queue_row(
        &self,
        theme: &Theme,
        at: usize,
        message: &QueuedMessage,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = message.id.clone();
        let editing = self
            .road31
            .queue
            .editing
            .clone()
            .filter(|(eid, _)| *eid == id);
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
                .track_focus(&self.road31.queue.focus)
                .on_key_down(
                    cx.listener(|this, event, window, cx| this.road31_edit_key(event, window, cx)),
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
                this.road31_queue_move(&dragged.id, at, cx);
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
                self.road31_mini(theme, format!("queue-edit-btn-{at}"), Icon::Pencil)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.road31_edit_in_row(&edit_msg, window, cx);
                    })),
            )
            .child(
                self.road31_mini(theme, format!("queue-remove-{at}"), Icon::X)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.road31_queue_remove(&remove_id, window, cx);
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
                    this.road31_queue_send_now(&send_id, window, cx);
                })),
            )
            .into_any_element()
    }

    /// A: the card above the box, in the run-activity tray's language.
    pub(in crate::ui::composer) fn road31_queue_card(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let items = self.road31_queue_items(cx);
        let finding = queue_finding(items.len())?;
        let rows: Vec<AnyElement> = items
            .iter()
            .enumerate()
            .map(|(at, m)| self.road31_queue_row(theme, at, m, cx))
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

    /// B: numbered pills inside the box, above the text.
    pub(in crate::ui::composer) fn road31_queue_pills(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let items = self.road31_queue_items(cx);
        let finding = queue_finding(items.len())?;
        let editing = self.road31_box_editing().map(str::to_owned);
        let pills: Vec<AnyElement> = items
            .iter()
            .enumerate()
            .map(|(at, message)| {
                let group = SharedString::from(format!("queue-pill-{at}"));
                let drag = Drag {
                    id: message.id.clone(),
                    text: message.text.clone(),
                };
                let (pull, remove_id, send_id) =
                    (message.clone(), message.id.clone(), message.id.clone());
                let on = editing.as_deref() == Some(message.id.as_str());
                div()
                    .id(group.clone())
                    .debug_selector(move || format!("queue-pill-{at}"))
                    .group(group.clone())
                    .max_w(px(300.0))
                    .h(px(26.0))
                    .pl(px(4.0))
                    .pr(px(4.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .rounded(px(999.0))
                    .bg(theme.raised)
                    .border_1()
                    .border_color(if on {
                        theme.accent.opacity(0.7)
                    } else {
                        theme.hairline
                    })
                    .text_size(px(12.0))
                    .cursor_pointer()
                    .drag_over::<Drag>({
                        let line = theme.accent;
                        move |style, _, _, _| style.border_color(line)
                    })
                    .on_drop(cx.listener(move |this, dragged: &Drag, _, cx| {
                        this.road31_queue_move(&dragged.id, at, cx);
                    }))
                    .on_drag(drag, |dragged: &Drag, _, _, cx| {
                        cx.new(|_| DragGhost(dragged.text.clone()))
                    })
                    .on_mouse_down(MouseButton::Left, |_, window, _| window.prevent_default())
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.road31_edit_in_box(&pull, window, cx);
                    }))
                    .child(
                        div()
                            .flex_none()
                            .size(px(18.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(9.0))
                            .bg(theme.accent.opacity(0.18))
                            .text_color(theme.accent)
                            .font_weight(FontWeight::MEDIUM)
                            .child(SharedString::from((at + 1).to_string())),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .child(SharedString::from(message.text.clone())),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("queue-pill-send-{at}")))
                            .debug_selector(move || format!("queue-pill-send-{at}"))
                            .flex_none()
                            .px(px(6.0))
                            .rounded(px(999.0))
                            .text_color(theme.muted)
                            .opacity(0.0)
                            .group_hover(group, |s| s.opacity(1.0))
                            .hover(|s| s.bg(theme.row_selected()).text_color(theme.text))
                            .child("Send now")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.road31_queue_send_now(&send_id, window, cx);
                            })),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("queue-pill-remove-{at}")))
                            .debug_selector(move || format!("queue-pill-remove-{at}"))
                            .flex_none()
                            .size(px(18.0))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(9.0))
                            .hover(|s| s.bg(theme.row_selected()))
                            .child(icon(Icon::X, 11.0, theme.muted))
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.road31_queue_remove(&remove_id, window, cx);
                            })),
                    )
                    .into_any_element()
            })
            .collect();
        Some(
            div()
                .id("queue-pills")
                .debug_selector(|| "queue-pills".to_owned())
                .px(px(14.0))
                .pt(px(12.0))
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    div()
                        .id("queue-finding")
                        .debug_selector(|| "queue-finding".to_owned())
                        .px(px(4.0))
                        .text_size(px(12.0))
                        .text_color(theme.muted)
                        .child(SharedString::from(finding)),
                )
                .child(div().flex().flex_wrap().gap(px(6.0)).children(pills))
                .into_any_element(),
        )
    }

    /// C: the "2 queued" chip in the foot.
    pub(in crate::ui::composer) fn road31_queue_chip(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let n = self.road31_queue_items(cx).len();
        if n == 0 {
            return None;
        }
        let anchor = self.road31.queue.chip.clone();
        let on = self.road31.queue.open;
        Some(
            kit::icon_chip(theme, Icon::List, queue_chip(n))
                .id("queue-chip")
                .debug_selector(|| "queue-chip".to_owned())
                .relative()
                .flex_none()
                .cursor_pointer()
                .when(on, |c| c.border_color(theme.accent.opacity(0.6)))
                .child(icon(Icon::ChevUp, 12.0, theme.muted))
                .child(
                    canvas(move |b, _, _| anchor.set(Some(b)), |_, _, _, _| {})
                        .absolute()
                        .size_full(),
                )
                .on_click(cx.listener(|this, _, _, cx| this.road31_open_queue(cx)))
                .into_any_element(),
        )
    }

    /// C: the list the chip opens, with the finding as its header.
    pub(in crate::ui::composer) fn road31_queue_menu(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Deferred> {
        if !self.road31.queue.open {
            return None;
        }
        let items = self.road31_queue_items(cx);
        let finding = queue_finding(items.len())?;
        let rows: Vec<AnyElement> = items
            .iter()
            .enumerate()
            .map(|(at, m)| self.road31_queue_row(theme, at, m, cx))
            .collect();
        let at = self.road31.queue.chip.clone();
        let content = menu(theme, "queue-menu", 480.0)
            .px(px(12.0))
            .child(
                div()
                    .h(px(30.0))
                    .px(px(2.0))
                    .flex()
                    .items_center()
                    .text_size(px(12.5))
                    .text_color(theme.muted)
                    .child(SharedString::from(finding)),
            )
            .children(rows);
        self.popover(
            &at,
            content,
            |this, cx| {
                this.road31.queue.open = false;
                this.sync_overlay(cx);
                cx.notify();
            },
            Some(false),
            cx,
        )
    }

    /// The Queue button, beside the send button, while there is something to queue.
    pub(in crate::ui::composer) fn road31_queue_button(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.road31_can_queue(cx) {
            return None;
        }
        Some(
            kit::line_button(theme, "queue-add", None, "Queue", Tone::Quiet, true)
                .on_click(cx.listener(|this, _, window, cx| {
                    this.road31_queue_add(window, cx);
                    window.focus(&this.focus_handle, cx);
                }))
                .into_any_element(),
        )
    }
}
