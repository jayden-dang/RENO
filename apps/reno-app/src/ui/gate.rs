//! The connecting gate: what the window shows from its first frame until the daemon has
//! answered, one way or the other. The first paint never waits on the daemon: the gate is drawn
//! over a shell that is already there, says what the app is doing (the centred mark and one
//! line, "Connecting to renod\u{2026}"), and crossfades away the moment the daemon connects, or
//! fails to, so Home's offline notice and Retry are never hidden behind it.
//!
//! It sits under the status bar, so the bar stays.

use gpui::{AnyElement, IntoElement, div, prelude::*, px};

use crate::ui::icon::{Icon, icon};
use crate::ui::motion;
use crate::ui::theme::Theme;
use crate::vm::shell::BAR_HEIGHT;

/// Where the gate is: up, or fading into the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Up,
    Fading,
}

/// The gate, over the window under the status bar.
pub(crate) fn splash(theme: &Theme, phase: Phase) -> AnyElement {
    let inner = div()
        .relative()
        .size_full()
        .bg(theme.shell)
        .flex()
        .items_center()
        .justify_center()
        .child(
            div()
                .flex()
                .flex_col()
                .items_center()
                .gap(px(14.0))
                .child(icon(Icon::Mark, 44.0, theme.text))
                .child(
                    div()
                        .id("gate-line")
                        .debug_selector(|| "gate-line".to_owned())
                        .text_size(px(14.0))
                        .text_color(theme.muted)
                        .child("Connecting to renod\u{2026}"),
                ),
        );
    let inner = match phase {
        Phase::Fading => motion::splash_out("gate-out", inner).into_any_element(),
        Phase::Up => inner.into_any_element(),
    };
    div()
        .id("gate-splash")
        .debug_selector(|| "gate-splash".to_owned())
        .absolute()
        .top(px(BAR_HEIGHT))
        .left_0()
        .right_0()
        .bottom_0()
        .occlude()
        .child(inner)
        .into_any_element()
}
