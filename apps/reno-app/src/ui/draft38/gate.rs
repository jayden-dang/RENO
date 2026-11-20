//! The connecting gate: what the window shows from its first frame until the daemon has
//! answered. The first paint never waits on the daemon; the gate is drawn over a shell that is
//! already there, says what the app is doing, and crossfades away.
//!
//! - A: the centred mark and one line, "Connecting to renod\u{2026}", over the whole window.
//! - B: no splash. The real shell is there with skeleton rows in the sidebar and a thin progress
//!   line under the status bar.
//! - C: the mark and a live checklist: Starting renod, Loading threads, Reading agents.
//!
//! Every gate sits under the status bar, so the bar and the switch's label in it stay.

use std::time::Duration;

use gpui::{
    Animation, AnimationExt as _, AnyElement, FontWeight, IntoElement, div, prelude::*, px,
    relative,
};

use crate::ui::icon::{Icon, icon};
use crate::ui::kit;
use crate::ui::motion;
use crate::ui::theme::Theme;
use crate::vm::draft38::Variant;
use crate::vm::shell::BAR_HEIGHT;

/// How far the app has got, for the words the gate says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Progress {
    /// The daemon answered the hello.
    pub(crate) started: bool,
    pub(crate) threads: bool,
    pub(crate) agents: bool,
}

/// Where the gate is: up, or fading into the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Up,
    Fading,
}

/// A and C's splash, over the window under the status bar. `None` for B, which has none.
pub(crate) fn splash(
    variant: Variant,
    theme: &Theme,
    progress: Progress,
    phase: Phase,
) -> Option<AnyElement> {
    let body = match variant {
        Variant::A => centred_line(theme, progress),
        Variant::C => live_list(theme, progress),
        Variant::B => return None,
    };
    let inner = div()
        .relative()
        .size_full()
        .bg(theme.shell)
        .flex()
        .items_center()
        .justify_center()
        .child(body);
    let inner = match phase {
        Phase::Fading => motion::splash_out("gate-out", inner).into_any_element(),
        _ => inner.into_any_element(),
    };
    Some(
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
            .into_any_element(),
    )
}

/// A's one line says what the app is doing: first reaching renod, then reading what it has.
fn centred_line(theme: &Theme, progress: Progress) -> AnyElement {
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
                .child(if progress.started {
                    "Loading your threads\u{2026}"
                } else {
                    "Connecting to renod\u{2026}"
                }),
        )
        .into_any_element()
}

fn live_list(theme: &Theme, progress: Progress) -> AnyElement {
    let steps = [
        ("gate-starting", "Starting renod", progress.started),
        ("gate-threads", "Loading threads", progress.threads),
        ("gate-agents", "Reading agents", progress.agents),
    ];
    let current = steps.iter().position(|(_, _, done)| !done);
    div()
        .flex()
        .flex_col()
        .items_start()
        .gap(px(18.0))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(12.0))
                .child(icon(Icon::Mark, 32.0, theme.text))
                .child(
                    div()
                        .text_size(px(15.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child("Starting Reno"),
                ),
        )
        .child(
            div()
                .id("gate-list")
                .debug_selector(|| "gate-list".to_owned())
                .flex()
                .flex_col()
                .gap(px(8.0))
                .font_family(kit::MONO)
                .text_size(px(12.0))
                .children(steps.iter().enumerate().map(|(at, (id, label, done))| {
                    let active = current == Some(at);
                    div()
                        .id(*id)
                        .debug_selector(move || (*id).to_owned())
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .text_color(if *done {
                            theme.text
                        } else if active {
                            theme.muted
                        } else {
                            theme.faint
                        })
                        .child(div().w(px(14.0)).flex().justify_center().child(if *done {
                            icon(Icon::PhCheck, 13.0, theme.success).into_any_element()
                        } else if active {
                            kit::spinner(*id, 12.0, theme.accent).into_any_element()
                        } else {
                            kit::dot(theme.faint, 5.0).into_any_element()
                        }))
                        .child(if *done {
                            (*label).to_owned()
                        } else {
                            format!("{label}\u{2026}")
                        })
                })),
        )
        .into_any_element()
}

/// B's thin progress line, under the status bar: a segment that keeps running along it.
pub(crate) fn progress_line(theme: &Theme, phase: Phase) -> AnyElement {
    let track = div()
        .id("gate-progress")
        .debug_selector(|| "gate-progress".to_owned())
        .absolute()
        .top(px(BAR_HEIGHT - 1.0))
        .left_0()
        .right_0()
        .h(px(2.0))
        .overflow_hidden()
        .child(
            div()
                .absolute()
                .top_0()
                .h(px(2.0))
                .w(relative(0.3))
                .rounded_full()
                .bg(theme.accent)
                .with_animation(
                    "gate-progress-run",
                    Animation::new(Duration::from_millis(1100)).repeat(),
                    |segment, t| segment.left(relative(t * 0.7)),
                ),
        );
    match phase {
        Phase::Fading => motion::fade_quick("gate-progress-out", track).into_any_element(),
        _ => track.into_any_element(),
    }
}

/// B's skeleton rows for the sidebar's list while the gate is up.
pub(crate) fn skeleton_rows(theme: &Theme) -> AnyElement {
    let widths = [0.62, 0.8, 0.5, 0.72, 0.58, 0.68];
    div()
        .id("gate-skeleton")
        .debug_selector(|| "gate-skeleton".to_owned())
        .flex()
        .flex_col()
        .gap(px(6.0))
        .pt(px(8.0))
        .children(widths.iter().enumerate().map(|(at, w)| {
            div()
                .h(px(28.0))
                .px(px(10.0))
                .flex()
                .items_center()
                .gap(px(10.0))
                .child(
                    div()
                        .flex_none()
                        .size(px(8.0))
                        .rounded_full()
                        .bg(theme.raised),
                )
                .child(
                    div()
                        .h(px(10.0))
                        .w(relative(*w))
                        .rounded(px(5.0))
                        .bg(theme.raised),
                )
                .with_animation(
                    format!("gate-skeleton-{at}"),
                    Animation::new(Duration::from_millis(1400 + 90 * at as u64)).repeat(),
                    |row, t| {
                        row.opacity(0.45 + 0.55 * (0.5 - 0.5 * (t * std::f32::consts::TAU).cos()))
                    },
                )
        }))
        .into_any_element()
}
