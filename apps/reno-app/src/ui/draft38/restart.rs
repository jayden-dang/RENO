//! Restarting `renod` when a newer one is installed than the one running, as A, B and C draw it.
//! The rules are in `vm::daemon_restart`; the three only differ in where the affordance sits.
//!
//! - A: a warning-tinted pill "Restart renod" next to "\u{25cf} renod" in the status bar; it opens
//!   the confirm.
//! - B: a one-line banner at the top of the sidebar, "renod 0.4.2 is ready \u{b7} Restart".
//! - C: inside the renod pill's popover, a card "A newer renod is installed (0.4.1 \u{2192} 0.4.2)"
//!   with Restart, which then asks.
//!
//! Every variant asks before it restarts, saying what happens to live runs, and says plainly when
//! renod did not come back.

use gpui::{
    AnyElement, Entity, FontWeight, IntoElement, Role, SharedString, Stateful, div, prelude::*, px,
};
use reno_client::daemon_desk::DaemonVersions;

use crate::app::RootView;
use crate::ui::bar::{Anchor_, panel, track};
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, Tone};
use crate::ui::theme::Theme;
use crate::vm::daemon_restart::{self, Phase};
use crate::vm::draft38::Variant;

/// What the restart surfaces need to draw.
pub(crate) struct RestartBar {
    pub(crate) variant: Variant,
    pub(crate) phase: Phase,
    /// A newer renod is installed than the one running (or a restart is under way).
    pub(crate) offered: bool,
    /// The panel under the affordance is open: C's card, the question, or the failure.
    pub(crate) open: bool,
    pub(crate) versions: Option<DaemonVersions>,
    /// Runs live now.
    pub(crate) live: usize,
    /// Where the surface sits, so the panel hangs from it.
    pub(crate) at: Anchor_,
}

impl RestartBar {
    /// Whether the panel under the affordance is open.
    pub(crate) fn panel_open(&self) -> bool {
        self.offered && self.open && self.phase != Phase::Restarting
    }
}

fn tone(theme: &Theme, phase: &Phase) -> gpui::Hsla {
    match phase {
        Phase::Failed(_) => theme.danger,
        Phase::Restarting => theme.muted,
        _ => theme.warning,
    }
}

fn on_press(
    root: &Entity<RootView>,
    f: fn(&mut RootView, &mut gpui::Context<RootView>),
) -> impl Fn(&gpui::ClickEvent, &mut gpui::Window, &mut gpui::App) + 'static {
    let root = root.clone();
    move |_, _, cx| {
        root.update(cx, f);
    }
}

/// A's pill, for the status bar.
pub(crate) fn pill(root: &Entity<RootView>, theme: &Theme, rb: &RestartBar) -> Option<AnyElement> {
    if rb.variant != Variant::A || !rb.offered {
        return None;
    }
    let colour = tone(theme, &rb.phase);
    let busy = rb.phase == Phase::Restarting;
    let label = daemon_restart::pill_label(&rb.phase);
    let pill = div()
        .id("restart-pill")
        .debug_selector(|| "restart-pill".to_owned())
        .relative()
        .flex_none()
        .h(px(28.0))
        .px(px(11.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .rounded(px(999.0))
        .bg(colour.opacity(0.14))
        .border_1()
        .border_color(colour.opacity(0.5))
        .text_size(px(12.5))
        .font_weight(FontWeight::MEDIUM)
        .text_color(if busy { theme.muted } else { colour })
        .whitespace_nowrap()
        .role(Role::Button)
        .aria_label(label)
        .child(track(&rb.at))
        .child(if busy {
            kit::spinner("restart-spin", 12.0, theme.muted).into_any_element()
        } else {
            icon(Icon::PhRefresh, 13.0, colour).into_any_element()
        })
        .child(label);
    Some(if busy {
        pill.cursor_default().into_any_element()
    } else {
        pill.cursor_pointer()
            .focusable()
            .tab_stop(true)
            .focus_visible(kit::focus_ring(theme))
            .on_click(on_press(root, RootView::restart_press))
            .into_any_element()
    })
}

/// B's banner, for the top of the sidebar.
pub(crate) fn banner(
    root: &Entity<RootView>,
    theme: &Theme,
    rb: &RestartBar,
) -> Option<AnyElement> {
    if rb.variant != Variant::B || !rb.offered {
        return None;
    }
    let colour = tone(theme, &rb.phase);
    let installed = rb
        .versions
        .as_ref()
        .map_or_else(String::new, |v| v.installed.clone());
    let (words, action): (SharedString, Option<&'static str>) = match &rb.phase {
        Phase::Restarting => ("Restarting renod\u{2026}".into(), None),
        Phase::Failed(_) => ("Restart failed".into(), Some("Details")),
        _ => (
            format!("renod {installed} is ready").into(),
            Some("Restart"),
        ),
    };
    let busy = rb.phase == Phase::Restarting;
    Some(
        div()
            .id("restart-banner")
            .debug_selector(|| "restart-banner".to_owned())
            .relative()
            .flex_none()
            .h(px(32.0))
            .mb(px(6.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(8.0))
            .bg(colour.opacity(0.12))
            .border_1()
            .border_color(colour.opacity(0.35))
            .text_size(px(12.5))
            .child(track(&rb.at))
            .child(if busy {
                kit::spinner("restart-spin", 12.0, theme.muted).into_any_element()
            } else {
                kit::dot(colour, 7.0).into_any_element()
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(theme.text)
                    .child(words),
            )
            .children(action.map(|label| {
                div()
                    .id("restart-banner-action")
                    .debug_selector(|| "restart-banner-action".to_owned())
                    .cursor_pointer()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme.text)
                    .underline()
                    .role(Role::Button)
                    .aria_label(label)
                    .focusable()
                    .tab_stop(true)
                    .focus_visible(kit::focus_ring(theme))
                    .on_click(on_press(root, RootView::restart_press))
                    .child(label)
            }))
            .into_any_element(),
    )
}

/// The panel that hangs from the affordance: C's card, the question, or the failure. `None`
/// while nothing is open.
pub(crate) fn content(
    root: &Entity<RootView>,
    theme: &Theme,
    rb: &RestartBar,
) -> Option<Stateful<gpui::Div>> {
    if !rb.panel_open() {
        return None;
    }
    let versions = rb.versions.clone()?;
    let ask = rb.phase == Phase::Asking;
    let failed = match &rb.phase {
        Phase::Failed(why) => Some(why.clone()),
        _ => None,
    };
    let go_tone = if rb.variant == Variant::A {
        Tone::Primary
    } else {
        Tone::Quiet
    };
    let title = |text: SharedString, danger: bool| {
        div()
            .text_size(px(13.5))
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(if danger { theme.danger } else { theme.text })
            .child(text)
    };
    let body = |text: SharedString| {
        div()
            .text_size(px(13.0))
            .line_height(px(19.0))
            .text_color(theme.muted)
            .child(text)
    };
    let buttons = div().flex().justify_end().gap(px(8.0));
    let (head, rows): (AnyElement, Vec<AnyElement>) = if let Some(why) = failed {
        (
            title("Restart failed".into(), true).into_any_element(),
            vec![
                body(why.into()).into_any_element(),
                body("The old renod is still what the app is talking to.".into())
                    .into_any_element(),
                buttons
                    .child(
                        kit::pill_button(theme, "restart-close", "Close", Tone::Quiet)
                            .on_click(on_press(root, RootView::restart_cancel)),
                    )
                    .child(
                        kit::pill_button(theme, "restart-retry", "Try again", go_tone)
                            .on_click(on_press(root, RootView::restart_go_again)),
                    )
                    .into_any_element(),
            ],
        )
    } else if ask {
        (
            title(daemon_restart::question(&versions).into(), false).into_any_element(),
            vec![
                body(daemon_restart::live_words(rb.live).into())
                    .id("restart-live-words")
                    .debug_selector(|| "restart-live-words".to_owned())
                    .into_any_element(),
                buttons
                    .child(
                        kit::pill_button(theme, "restart-cancel", "Not now", Tone::Quiet)
                            .on_click(on_press(root, RootView::restart_cancel)),
                    )
                    .child(
                        kit::pill_button(theme, "restart-confirm", "Restart", go_tone)
                            .on_click(on_press(root, RootView::restart_go)),
                    )
                    .into_any_element(),
            ],
        )
    } else {
        // C's card, before the question.
        (
            title("A newer renod is installed".into(), false).into_any_element(),
            vec![
                div()
                    .font_family(kit::MONO)
                    .text_size(px(12.0))
                    .text_color(theme.muted)
                    .child(format!(
                        "{} \u{2192} {}",
                        versions.running, versions.installed
                    ))
                    .into_any_element(),
                body(
                    format!(
                        "renod {} is running. {} is installed and starts when you restart.",
                        versions.running, versions.installed
                    )
                    .into(),
                )
                .into_any_element(),
                buttons
                    .child(
                        kit::pill_button(theme, "restart-ask", "Restart", Tone::Quiet)
                            .on_click(on_press(root, RootView::restart_ask)),
                    )
                    .into_any_element(),
            ],
        )
    };
    Some(
        panel(theme, "restart-panel", 340.0)
            .p(px(14.0))
            .gap(px(10.0))
            .child(head)
            .children(rows),
    )
}
