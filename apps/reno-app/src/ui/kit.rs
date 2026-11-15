//! Small shared elements: chips, status dots, executor glyphs, round buttons.
//!
//! No icon set is bundled in Phase 0, so icons are text glyphs from the system fallback fonts.

use gpui::{Div, FontWeight, Hsla, SharedString, Stateful, div, hsla, prelude::*, px};
use reno_proto::RunStatus;

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

/// A one-glyph mark for an executor, in its own colour.
pub(crate) fn exec_glyph(theme: &Theme, executor_id: &str, size: f32) -> Div {
    let (glyph, color) = match executor_id {
        "claude-code" => ("✻", hsla(0.04, 0.62, 0.62, 1.0)),
        "codex" => ("⬡", theme.text),
        _ => ("◇", theme.muted),
    };
    div()
        .flex_none()
        .w(px(size + 4.0))
        .flex()
        .justify_center()
        .text_size(px(size))
        .line_height(px(size + 4.0))
        .text_color(color)
        .child(glyph)
}

/// A round icon button on a square: a glyph and an accessible label (the element id).
pub(crate) fn icon_button(
    theme: &Theme,
    id: &'static str,
    glyph: &'static str,
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
        .text_size(px(14.0))
        .text_color(theme.muted)
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .child(glyph)
}
