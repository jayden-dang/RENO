//! Small shared elements: chips, status dots, executor glyphs, round buttons.
//!
//! Icons are the bundled SVGs from [`super::icon`], coloured with `text_color`.

use gpui::{Div, FontWeight, Hsla, SharedString, Stateful, div, hsla, prelude::*, px};
use reno_proto::RunStatus;

use super::icon::{Icon, icon};
use super::theme::Theme;

/// The monospace family bundled in `assets/fonts`.
pub(crate) const MONO: &str = "Geist Mono";
/// The UI family bundled in `assets/fonts`.
pub(crate) const SANS: &str = "Geist";

/// A pill with a hairline border: context chips, corner chips, suggestions.
pub(crate) fn chip(theme: &Theme, label: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .items_center()
        .gap(px(6.0))
        .h(px(28.0))
        .px(px(11.0))
        .rounded(px(999.0))
        .bg(theme.surface)
        .border_1()
        .border_color(theme.hairline)
        .text_size(px(12.5))
        .text_color(theme.text)
        .child(label.into())
}

/// A [`chip`] that starts with a muted icon.
pub(crate) fn icon_chip(theme: &Theme, glyph: Icon, label: impl Into<SharedString>) -> Div {
    chip(theme, "")
        .child(icon(glyph, 13.0, theme.muted))
        .child(label.into())
}

/// A small tinted label: the age chip, the "network" chip.
pub(crate) fn tag(tone: Hsla, label: impl Into<SharedString>) -> Div {
    div()
        .flex_none()
        .px(px(8.0))
        .h(px(20.0))
        .flex()
        .items_center()
        .rounded(px(999.0))
        .bg(tone.opacity(0.16))
        .text_size(px(12.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(tone)
        .child(label.into())
}

pub(crate) fn dot(color: Hsla, size: f32) -> Div {
    div()
        .flex_none()
        .size(px(size))
        .rounded(px(size / 2.0))
        .bg(color)
}

/// The state colour of a run: warning held, accent running, success passed, danger failed,
/// faint idle (UI brief, semantic colours).
pub(crate) fn status_color(theme: &Theme, status: RunStatus) -> Hsla {
    match status {
        RunStatus::WaitingApproval | RunStatus::WaitingInput => theme.warning,
        RunStatus::Leased | RunStatus::Running | RunStatus::Evaluating | RunStatus::Queued => {
            theme.accent
        }
        RunStatus::Succeeded => theme.success,
        RunStatus::Failed => theme.danger,
        RunStatus::Interrupted | RunStatus::Cancelled => theme.faint,
    }
}

/// The mark for an executor, in its own colour: Claude's burst is orange.
pub(crate) fn exec_icon(theme: &Theme, executor_id: &str) -> (Icon, Hsla) {
    match executor_id {
        "claude-code" => (Icon::XClaude, hsla(0.04, 0.62, 0.62, 1.0)),
        "codex" => (Icon::XCodex, theme.text),
        "grok" => (Icon::XGrok, theme.text),
        "opencode" => (Icon::XOpencode, theme.muted),
        _ => (Icon::Cpu, theme.muted),
    }
}

/// An executor mark of `size` px.
pub(crate) fn exec_glyph(theme: &Theme, executor_id: &str, size: f32) -> Div {
    let (glyph, color) = exec_icon(theme, executor_id);
    div()
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .child(icon(glyph, size, color))
}

/// A square icon button with an accessible label (the element id).
pub(crate) fn icon_button(
    theme: &Theme,
    id: &'static str,
    glyph: Icon,
    size: f32,
) -> Stateful<Div> {
    let hover = theme.raised;
    div()
        .id(id)
        .flex_none()
        .size(px(size))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(8.0))
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(icon(glyph, 15.0, theme.muted))
}

/// The send button: a `text`-coloured circle with a `canvas` arrow (UI brief amendment 6). With
/// nothing to send it greys out. The caller adds `on_click` when `enabled`.
pub(crate) fn send_button(theme: &Theme, enabled: bool) -> Stateful<Div> {
    let (fill, arrow) = if enabled {
        (theme.text, theme.canvas)
    } else {
        (theme.raised, theme.faint)
    };
    div()
        .id("send")
        .flex_none()
        .size(px(34.0))
        .rounded(px(17.0))
        .flex()
        .items_center()
        .justify_center()
        .bg(fill)
        .when(enabled, |s| s.cursor_pointer())
        .child(icon(Icon::Up, 17.0, arrow))
}
