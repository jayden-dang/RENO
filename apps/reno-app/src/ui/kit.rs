//! Small shared elements: chips, status dots, executor glyphs, round buttons.
//!
//! Icons are the bundled SVGs from [`super::icon`], coloured with `text_color`.

use std::sync::Arc;
use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyElement, Div, ElementId, FontWeight, Hsla, ImageFormat, Role,
    SharedString, Stateful, Toggled, Transformation, div, percentage, prelude::*, px,
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

/// An executor's mark of `size` px, everywhere in the app: its coloured logo (a light twin on
/// dark themes), or a Phosphor glyph for an executor with no logo.
pub(crate) fn exec_glyph(theme: &Theme, executor_id: &str, size: f32) -> Div {
    div().flex_none().child(logo(theme, executor_id, size))
}

/// A square icon button with an accessible label (the element id).
pub(crate) fn icon_button(
    theme: &Theme,
    id: impl Into<SharedString>,
    glyph: Icon,
    size: f32,
) -> Stateful<Div> {
    let hover = theme.raised;
    let id: SharedString = id.into();
    let selector = id.clone();
    let label = id.clone();
    div()
        .id(id)
        .debug_selector(move || selector.to_string())
        .flex_none()
        .size(px(size))
        .border_1()
        .border_color(gpui::transparent_black())
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(8.0))
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .role(Role::Button)
        .aria_label(label)
        .focusable()
        .tab_stop(true)
        .focus_visible(focus_ring(theme))
        .child(icon(glyph, 15.0, theme.muted))
}

/// The visible keyboard focus: the accent ring (a border colour, so nothing shifts). Mouse
/// clicks do not show it.
pub(crate) fn focus_ring(
    theme: &Theme,
) -> impl FnOnce(gpui::StyleRefinement) -> gpui::StyleRefinement {
    let ring = theme.accent;
    move |s| s.border_1().border_color(ring)
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
    let spoken = label.clone();
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
        .when(!enabled, |b| b.cursor_default().opacity(0.4))
        .role(Role::Button)
        .aria_label(spoken)
        .when(enabled, |b| {
            // Keyboard focus is a ring; on the accent fill it is the text colour.
            let ring = if tone == Tone::Primary {
                theme.text
            } else {
                theme.accent
            };
            b.focusable()
                .tab_stop(true)
                .focus_visible(move |s| s.border_1().border_color(ring))
        });
    let hover = hover_fill(theme, tone, enabled);
    let base = match tone {
        Tone::Primary => base
            .border_1()
            .border_color(gpui::transparent_black())
            .bg(theme.accent)
            .text_color(gpui::white()),
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
        .role(Role::Switch)
        .aria_toggled(if on { Toggled::True } else { Toggled::False })
        .focusable()
        .tab_stop(true)
        .focus_visible(focus_ring(theme))
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
        .role(Role::CheckBox)
        .aria_toggled(if checked {
            Toggled::True
        } else {
            Toggled::False
        })
        .focusable()
        .tab_stop(true)
        .focus_visible(focus_ring(theme))
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

/// One segment of a [`segmented`] filter: a button that is pressed or not. The id is the
/// accessible label and the test selector; the caller adds `on_click`.
pub(crate) fn segment(
    theme: &Theme,
    id: impl Into<SharedString>,
    label: impl Into<SharedString>,
    pressed: bool,
) -> Stateful<Div> {
    let id: SharedString = id.into();
    let selector = id.clone();
    let label: SharedString = label.into();
    let hover = theme.raised;
    div()
        .id(id)
        .debug_selector(move || selector.to_string())
        .flex_none()
        .h(px(26.0))
        .px(px(11.0))
        .flex()
        .items_center()
        .rounded(px(999.0))
        .border_1()
        .border_color(gpui::transparent_black())
        .text_size(px(12.5))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .when(pressed, |d| d.bg(theme.raised).text_color(theme.text))
        .when(!pressed, |d| {
            d.text_color(theme.muted).hover(move |s| s.bg(hover))
        })
        .role(Role::Button)
        .aria_label(label.clone())
        .aria_toggled(if pressed {
            Toggled::True
        } else {
            Toggled::False
        })
        .focusable()
        .tab_stop(true)
        .focus_visible(focus_ring(theme))
        .child(label)
}

/// The segmented filter: a pill group of [`segment`]s.
pub(crate) fn segmented(theme: &Theme) -> Div {
    div()
        .flex_none()
        .flex()
        .items_center()
        .gap(px(2.0))
        .p(px(2.0))
        .rounded(px(999.0))
        .bg(theme.surface)
        .border_1()
        .border_color(theme.hairline)
}

/// A numbered step card of an opened registry line: number, title and detail, and one control
/// at the right. A step that is not yet its turn is dimmed.
pub(crate) fn step_card(
    theme: &Theme,
    id: impl Into<SharedString>,
    number: u8,
    title: &'static str,
    detail: impl Into<SharedString>,
    control: Option<AnyElement>,
    enabled: bool,
) -> Stateful<Div> {
    let id: SharedString = id.into();
    let selector = id.clone();
    div()
        .id(id)
        .debug_selector(move || selector.to_string())
        .w_full()
        .flex()
        .items_start()
        .gap(px(12.0))
        .p(px(12.0))
        .rounded(px(10.0))
        .bg(theme.surface)
        .border_1()
        .border_color(theme.hairline)
        .child(
            div()
                .flex_none()
                .size(px(22.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(11.0))
                .bg(theme.accent.opacity(0.18))
                .text_size(px(12.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.accent)
                .child(format!("{number}")),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(3.0))
                .child(
                    div()
                        .text_size(px(13.0))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(if enabled { theme.text } else { theme.muted })
                        .child(title),
                )
                .child(
                    div()
                        .text_size(px(12.5))
                        .line_height(px(18.0))
                        .text_color(theme.muted)
                        .child(detail.into()),
                ),
        )
        .children(control)
}

/// The mark of a registry agent: the registry's own one-ink SVG, drawn in the text colour, or a
/// letter tile when it has none (or it does not draw).
pub(crate) fn registry_icon(theme: &Theme, svg: Option<&str>, name: &str, size: f32) -> AnyElement {
    let letter = name
        .chars()
        .find(|c| c.is_alphanumeric())
        .map_or_else(|| "?".to_owned(), |c| c.to_uppercase().collect());
    let tile = {
        let (bg, ink) = (theme.raised, theme.muted);
        move || {
            div()
                .size(px(size))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.0))
                .bg(bg)
                .text_size(px(size * 0.6))
                .font_weight(FontWeight::MEDIUM)
                .text_color(ink)
                .child(letter.clone())
                .into_any_element()
        }
    };
    let frame = div().flex_none().size(px(size));
    match svg.filter(|s| s.contains("<svg")) {
        Some(svg) => {
            let image = recolored_svg(svg, theme.muted);
            frame
                .child(
                    gpui::img(Arc::new(gpui::Image::from_bytes(
                        ImageFormat::Svg,
                        image.into_bytes(),
                    )))
                    .size(px(size))
                    .flex_none()
                    .with_fallback(tile),
                )
                .into_any_element()
        }
        None => frame.child(tile()).into_any_element(),
    }
}

/// The registry's icons paint with `currentColor`, which an image does not inherit: it is
/// replaced with the colour they should have.
pub(crate) fn recolored_svg(svg: &str, ink: Hsla) -> String {
    let rgba = gpui::Rgba::from(ink);
    let byte = |c: f32| (c.clamp(0.0, 1.0) * 255.0).round() as u8;
    let hex = format!(
        "#{:02x}{:02x}{:02x}",
        byte(rgba.r),
        byte(rgba.g),
        byte(rgba.b)
    );
    svg.replace("currentColor", &hex)
}

#[cfg(test)]
mod registry_icon_tests {
    use super::*;
    use crate::ui::theme::Appearance;

    #[test]
    fn a_registry_icon_is_drawn_in_the_theme_colour_not_black() {
        let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\"><path fill=\"currentColor\" d=\"M0 0\"/></svg>";
        let dark = Theme::for_appearance(Appearance::Dark);
        let light = Theme::for_appearance(Appearance::Light);
        let on_dark = recolored_svg(svg, dark.muted);
        let on_light = recolored_svg(svg, light.muted);
        assert!(!on_dark.contains("currentColor") && on_dark.contains("fill=\"#"));
        assert_ne!(on_dark, on_light, "each theme gets its own ink");
    }
}
