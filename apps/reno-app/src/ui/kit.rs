//! Small shared elements: chips, status dots, executor glyphs, round buttons.
//!
//! Icons are the bundled SVGs from [`super::icon`], coloured with `text_color`.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyElement, Div, ElementId, FontWeight, Hsla, SharedString,
    Stateful, Transformation, div, hsla, percentage, prelude::*, px,
};
use reno_proto::RunStatus;

use super::icon::{Icon, icon, logo_path};
use super::theme::Theme;
use crate::summary::Problem;

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
///
/// A run whose agent reported a problem takes the problem's colour instead: a failure is danger
/// and a block is warning, whatever the green "passed" would have been.
pub(crate) fn run_color(theme: &Theme, status: RunStatus, problem: Option<Problem>) -> Hsla {
    match problem {
        Some(Problem::Failed) => return theme.danger,
        Some(Problem::Blocked) => return theme.warning,
        None => {}
    }
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
        "grok" | "acp.grok" => (Icon::XGrok, theme.text),
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
    id: impl Into<SharedString>,
    glyph: Icon,
    size: f32,
) -> Stateful<Div> {
    let hover = theme.raised;
    div()
        .id(id.into())
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

/// How a [`pill_button`] looks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Tone {
    /// The one primary action of a card: accent fill.
    Primary,
    /// Refusing: a danger outline.
    Danger,
    /// Everything else: a hairline outline.
    Quiet,
}

/// The hover fill of a pill button: none while it is disabled, so an inert button does not look
/// pressable, and none for the filled primary.
pub(crate) fn hover_fill(theme: &Theme, tone: Tone, enabled: bool) -> Option<Hsla> {
    if !enabled {
        return None;
    }
    match tone {
        Tone::Primary => None,
        Tone::Danger => Some(theme.danger.opacity(0.1)),
        Tone::Quiet => Some(theme.raised),
    }
}

/// A pill button. The id doubles as the accessible label and the test selector. The caller adds
/// `on_click` (and dims it with [`Stateful::opacity`] when busy).
pub(crate) fn pill_button(
    theme: &Theme,
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    tone: Tone,
) -> Stateful<Div> {
    pill_button_enabled(theme, id, label, tone, true)
}

/// A [`pill_button`] that can be disabled: dimmed, with the default cursor and no hover fill. The
/// caller still adds `on_click` only when enabled.
pub(crate) fn pill_button_enabled(
    theme: &Theme,
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    tone: Tone,
    enabled: bool,
) -> Stateful<Div> {
    pill(theme, id, None, label, tone, enabled, (30.0, 14.0, 13.5))
}

/// A [`pill_button`] for a 40 px list line: 26 px tall, an optional Phosphor icon before the
/// label. The id is the accessible label and the test selector. The caller adds `on_click`.
pub(crate) fn line_button(
    theme: &Theme,
    id: impl Into<SharedString>,
    glyph: Option<Icon>,
    label: impl Into<SharedString>,
    tone: Tone,
    enabled: bool,
) -> Stateful<Div> {
    pill(theme, id, glyph, label, tone, enabled, (26.0, 10.0, 12.5))
}

/// The pill: `(height, horizontal padding, text size)` in px.
fn pill(
    theme: &Theme,
    id: impl Into<SharedString>,
    glyph: Option<Icon>,
    label: impl Into<SharedString>,
    tone: Tone,
    enabled: bool,
    (height, pad, text): (f32, f32, f32),
) -> Stateful<Div> {
    let label: SharedString = label.into();
    let id: SharedString = id.into();
    let selector = id.clone();
    let ink = match tone {
        Tone::Primary => gpui::white(),
        Tone::Danger => theme.danger,
        Tone::Quiet => theme.text,
    };
    let base = div()
        .id(id)
        .debug_selector(move || selector.to_string())
        .flex_none()
        .h(px(height))
        .px(px(pad))
        .flex()
        .items_center()
        .gap(px(6.0))
        .rounded(px(999.0))
        .text_size(px(text))
        .font_weight(FontWeight::MEDIUM)
        .children(glyph.map(|g| icon(g, 13.0, ink)))
        .child(label)
        .when(enabled, |b| b.cursor_pointer())
        .when(!enabled, |b| b.cursor_default().opacity(0.4));
    let hover = hover_fill(theme, tone, enabled);
    let base = match tone {
        Tone::Primary => base.bg(theme.accent).text_color(gpui::white()),
        Tone::Danger => base
            .border_1()
            .border_color(theme.danger.opacity(0.5))
            .text_color(theme.danger),
        Tone::Quiet => base
            .border_1()
            .border_color(theme.hairline)
            .text_color(theme.text),
    };
    match hover {
        Some(fill) => base.hover(move |s| s.bg(fill)),
        None => base,
    }
}

/// The Auto-update switch: a 30x18 track with a 14 px knob, success green when on. A button with
/// the switch role as GPUI allows: the id is its accessible label and its test selector, and the
/// caller adds `on_click`.
pub(crate) fn switch(theme: &Theme, id: impl Into<SharedString>, on: bool) -> Stateful<Div> {
    let id: SharedString = id.into();
    let selector = id.clone();
    let track = if on {
        theme.success
    } else {
        theme.faint.opacity(0.4)
    };
    div()
        .id(id)
        .debug_selector(move || selector.to_string())
        .flex_none()
        .w(px(30.0))
        .h(px(18.0))
        .px(px(2.0))
        .flex()
        .items_center()
        .when(on, |d| d.justify_end())
        .rounded(px(9.0))
        .bg(track)
        .cursor_pointer()
        .child(div().size(px(14.0)).rounded(px(7.0)).bg(gpui::white()))
}

/// A check box with its label: "Only versions Reno has verified". The caller adds `on_click`.
pub(crate) fn checkbox(
    theme: &Theme,
    id: impl Into<SharedString>,
    checked: bool,
    label: impl Into<SharedString>,
) -> Stateful<Div> {
    let id: SharedString = id.into();
    let selector = id.clone();
    div()
        .id(id)
        .debug_selector(move || selector.to_string())
        .flex()
        .items_center()
        .gap(px(8.0))
        .cursor_pointer()
        .text_size(px(12.5))
        .text_color(theme.muted)
        .child(
            div()
                .flex_none()
                .size(px(15.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.0))
                .border_1()
                .border_color(if checked { theme.accent } else { theme.faint })
                .when(checked, |d| d.bg(theme.accent))
                .when(checked, |d| {
                    d.child(icon(Icon::PhCheck, 11.0, gpui::white()))
                }),
        )
        .child(label.into())
}

/// A turning ring: a job is running. Reduced motion stops it (GPUI snaps a repeating animation
/// to its first frame).
pub(crate) fn spinner(id: impl Into<ElementId>, size: f32, color: Hsla) -> impl IntoElement {
    icon(Icon::PhSpinner, size, color).with_animation(
        id,
        Animation::new(Duration::from_millis(900)).repeat(),
        |svg, delta| svg.with_transformation(Transformation::rotate(percentage(delta))),
    )
}

/// The mark of an executor on the Executors screen: its own coloured logo (a light twin on dark
/// themes for the dark-ink marks), Lightning for Fx and Flask for the mock, else a Phosphor Cpu.
pub(crate) fn logo(theme: &Theme, executor_id: &str, size: f32) -> AnyElement {
    let frame = div()
        .flex_none()
        .size(px(size))
        .flex()
        .items_center()
        .justify_center();
    if let Some(path) = logo_path(executor_id, theme.appearance.is_dark()) {
        return frame
            .child(gpui::img(path).size(px(size)).flex_none())
            .into_any_element();
    }
    let glyph = match executor_id {
        "acp.fx" => Icon::PhLightning,
        "mock" => Icon::PhFlask,
        _ => Icon::PhCpu,
    };
    frame
        .child(icon(glyph, size - 1.0, theme.muted))
        .into_any_element()
}

/// The status mark of a run: a dot and the state's words, in the state's colour.
pub(crate) fn status_pill(theme: &Theme, status: RunStatus, problem: Option<Problem>) -> Div {
    status_pill_worded(
        run_color(theme, status, problem),
        crate::summary::status_words(status, problem),
    )
}

/// A pill in `color` with other words, for a run whose state a person reads differently.
pub(crate) fn status_pill_worded(color: Hsla, words: &'static str) -> Div {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(7.0))
        .text_color(color)
        .child(dot(color, 7.0))
        .child(words)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::{Appearance, Theme};

    #[test]
    fn a_disabled_button_has_no_hover_fill() {
        let theme = Theme::for_appearance(Appearance::Dark);
        for tone in [Tone::Primary, Tone::Danger, Tone::Quiet] {
            assert_eq!(hover_fill(&theme, tone, false), None, "{tone:?}");
        }
        assert!(hover_fill(&theme, Tone::Quiet, true).is_some());
        assert!(hover_fill(&theme, Tone::Danger, true).is_some());
    }
}
