//! A progress ring with its percent: the one indicator the composer foot's context window and
//! the picker's account usage both use, so they sit side by side as equals.
//!
//! Ported from zeron's `crates/ui/src/context_usage.rs` (`ring_chip`, `ring`): a faint full
//! track at 25% under an arc that starts at twelve o'clock.

use gpui::{Hsla, SharedString, Stateful, canvas, div, point, prelude::*, px};

use super::theme::Theme;
use crate::vm::context::Level;

/// The tone of a ring at `level`: muted, warning or danger.
pub(crate) fn tone(theme: &Theme, level: Level) -> Hsla {
    match level {
        Level::Muted => theme.muted,
        Level::Warning => theme.warning,
        Level::Danger => theme.danger,
    }
}

/// One ring indicator: ring and label, the same geometry for every ring. `open` holds the hover
/// wash while its card is up.
pub(crate) fn ring_chip(
    id: &'static str,
    fraction: f32,
    tone: Hsla,
    label: String,
    open: bool,
    theme: &Theme,
) -> Stateful<gpui::Div> {
    let wash = theme.row_selected();
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .flex_none()
        .flex()
        .items_center()
        .gap(px(5.0))
        .h(px(24.0))
        .px(px(6.0))
        .rounded(px(6.0))
        .text_size(px(11.0))
        .text_color(tone)
        .cursor_pointer()
        .when(open, |s| s.bg(wash))
        .hover(move |s| s.bg(wash))
        .child(ring(fraction, tone, theme))
        .child(SharedString::from(label))
}

/// The 16 px ring: a faint full track under a `fraction` arc from twelve o'clock.
pub(crate) fn ring(fraction: f32, color: Hsla, theme: &Theme) -> impl IntoElement {
    let track = theme.faint.opacity(0.25);
    canvas(
        |_, _, _| (),
        move |bounds, _, window, _| {
            let center = bounds.center();
            let mut arc = |fraction: f32, color| {
                if fraction <= 0.0 {
                    return;
                }
                let steps = (64.0 * fraction).ceil().max(2.0) as usize;
                let mut path = gpui::PathBuilder::stroke(px(1.8));
                for i in 0..=steps {
                    let angle = -std::f32::consts::FRAC_PI_2
                        + std::f32::consts::TAU * fraction * i as f32 / steps as f32;
                    let p = point(
                        center.x + px(6.0 * angle.cos()),
                        center.y + px(6.0 * angle.sin()),
                    );
                    if i == 0 {
                        path.move_to(p);
                    } else {
                        path.line_to(p);
                    }
                }
                if let Ok(path) = path.build() {
                    window.paint_path(path, color);
                }
            };
            arc(1.0, track);
            arc(fraction.clamp(0.0, 1.0), color);
        },
    )
    .size(px(16.0))
}
