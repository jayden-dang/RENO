//! Restarting `renod` when a newer one is installed than the one running: a warning-tinted pill
//! "Restart renod" next to "\u{25cf} renod" in the status bar. It opens a confirm that says what
//! happens to live runs, and, when renod did not come back, says so and offers another try. The
//! rules are in `vm::daemon_restart`. Nothing shows unless the daemon can restart
//! (`Capabilities::daemon_restart`) and a newer renod is installed.

use gpui::{
    AnyElement, Entity, FontWeight, IntoElement, Role, SharedString, Stateful, actions, div,
    prelude::*, px,
};
use reno_client::daemon_desk::DaemonVersions;

use crate::app::RootView;
use crate::ui::bar::{Anchor_, panel, track};
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, Tone};
use crate::ui::theme::Theme;
use crate::vm::daemon_restart::{self, Phase};

actions!(restart, [Dismiss]);

/// The key context the root view carries while the panel is open, so Escape reaches [`Dismiss`]
/// whatever has the focus.
pub(crate) const OPEN_CONTEXT: &str = "RestartOpen";

/// Binds Escape for the open panel.
pub(crate) fn bind_keys(cx: &mut gpui::App) {
    cx.bind_keys([gpui::KeyBinding::new("escape", Dismiss, Some(OPEN_CONTEXT))]);
}

/// What the restart pill needs to draw.
pub(crate) struct RestartBar {
    pub(crate) phase: Phase,
    /// A newer renod is installed than the one running (or a restart is under way).
    pub(crate) offered: bool,
    /// The panel under the affordance is open: the question, or the failure.
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

/// The pill, for the status bar.
pub(crate) fn pill(root: &Entity<RootView>, theme: &Theme, rb: &RestartBar) -> Option<AnyElement> {
    if !rb.offered {
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

/// The panel that hangs from the pill: the question, or the failure. `None`
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
    let go_tone = Tone::Primary;
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
        return None;
    };
    Some(
        panel(theme, "restart-panel", 340.0)
            .p(px(14.0))
            .gap(px(10.0))
            .child(head)
            .children(rows),
    )
}
