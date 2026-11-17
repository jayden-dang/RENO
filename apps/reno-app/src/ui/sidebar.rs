//! The sidebar: nav rows, the "ORCHESTRATION" group, recent runs from state, and a footer with
//! the daemon status dot. It sits on `shell`. Only Home is functional; the shell turns the other
//! rows into "coming in a later phase" toasts.

use std::rc::Rc;

use gpui::{App, Div, FontWeight, SharedString, Window, div, prelude::*, px};
use reno_client::ConnectionStatus;
use reno_proto::RunId;

use super::icon::{Icon, icon};
use super::kit;
use super::theme::Theme;
use crate::nav::Nav;
use crate::store::Store;
use crate::summary;

/// Width of the sidebar column, padding included.
pub(crate) const WIDTH: f32 = 248.0;
/// Height of the titlebar band the traffic lights sit in.
pub(crate) const TITLEBAR: f32 = 40.0;
/// How many runs "RECENT RUNS" lists.
const RECENT: usize = 6;

pub(crate) type NavHandler = Rc<dyn Fn(Nav, &mut Window, &mut App)>;
pub(crate) type ActionHandler = Rc<dyn Fn(&mut Window, &mut App)>;
pub(crate) type RunHandler = Rc<dyn Fn(RunId, &mut Window, &mut App)>;

pub(crate) struct SidebarHandlers {
    pub(crate) navigate: NavHandler,
    pub(crate) hide: ActionHandler,
    pub(crate) recent_run: RunHandler,
    pub(crate) settings: ActionHandler,
}

/// The counts shown beside rows, so the view and the tests agree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RowBadge {
    pub(crate) text: String,
    /// Hot (warning colour): something needs you.
    pub(crate) hot: bool,
}

pub(crate) fn badge(nav: Nav, counts: &summary::Attention) -> Option<RowBadge> {
    let (text, hot) = match nav {
        Nav::Inbox if counts.need > 0 => (counts.need.to_string(), true),
        Nav::Runs if counts.running > 0 => (format!("{} live", counts.running), false),
        Nav::Bots if counts.bots > 0 => (counts.bots.to_string(), false),
        _ => return None,
    };
    Some(RowBadge { text, hot })
}

pub(crate) fn daemon_label(status: ConnectionStatus) -> &'static str {
    match status {
        ConnectionStatus::Connected => "renod",
        ConnectionStatus::Connecting => "connecting",
        ConnectionStatus::Disconnected => "offline",
    }
}

fn nav_row(
    theme: &Theme,
    nav: Nav,
    active: Nav,
    counts: &summary::Attention,
    on_nav: &NavHandler,
) -> impl IntoElement {
    let on_nav = Rc::clone(on_nav);
    let selected = nav == active;
    let hover = theme.row_selected();
    let row = div()
        .id(SharedString::from(format!("nav-{}", nav.label())))
        .debug_selector(move || format!("nav-{}", nav.label()))
        .h(px(34.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .gap(px(10.0))
        .rounded(px(9.0))
        .text_size(px(14.0))
        .text_color(if selected { theme.text } else { theme.muted })
        .cursor_pointer()
        .when(selected, |r| r.bg(theme.row_selected()))
        .when(!selected, |r| r.hover(move |s| s.bg(hover.opacity(0.6))))
        .on_click(move |_, window, cx| on_nav(nav, window, cx))
        .child(icon(
            nav.icon(),
            16.0,
            if selected { theme.text } else { theme.muted },
        ))
        .child(div().flex_1().child(nav.label()));
    match badge(nav, counts) {
        Some(b) => row.child(
            div()
                .text_size(px(12.5))
                .text_color(if b.hot { theme.warning } else { theme.faint })
                .when(b.hot, |t| t.font_weight(FontWeight::SEMIBOLD))
                .child(b.text),
        ),
        None => row,
    }
}

fn group_label(theme: &Theme, text: &'static str) -> Div {
    div()
        .px(px(10.0))
        .pt(px(20.0))
        .pb(px(6.0))
        .text_size(px(11.5))
        .text_color(theme.faint)
        .child(text)
}

/// The sidebar column. `active` marks the current destination.
pub(crate) fn sidebar(
    theme: &Theme,
    store: &Store,
    active: Nav,
    handlers: &SidebarHandlers,
) -> impl IntoElement {
    let state = store.state();
    let counts = summary::attention(state);
    let daemon_text = daemon_label(store.connection());
    let daemon_color = match store.connection() {
        ConnectionStatus::Connected => theme.success,
        ConnectionStatus::Connecting => theme.warning,
        ConnectionStatus::Disconnected => theme.danger,
    };

    let hide = Rc::clone(&handlers.hide);
    let new_task = Rc::clone(&handlers.navigate);
    let top = div()
        .h(px(TITLEBAR))
        .flex_none()
        .flex()
        .items_center()
        .justify_end()
        .gap(px(2.0))
        .pr(px(10.0))
        .child(
            kit::icon_button(theme, "hide-sidebar", Icon::Sidebar, 28.0)
                .on_click(move |_, window, cx| hide(window, cx)),
        )
        .child(
            kit::icon_button(theme, "new-task", Icon::Plus, 28.0)
                .debug_selector(|| "new-task".to_owned())
                .on_click(move |_, window, cx| new_task(Nav::Home, window, cx)),
        );

    let main = div()
        .flex()
        .flex_col()
        .gap(px(2.0))
        .children(Nav::MAIN.map(|nav| nav_row(theme, nav, active, &counts, &handlers.navigate)))
        .child(group_label(theme, "ORCHESTRATION"))
        .children(
            Nav::ORCHESTRATION.map(|nav| nav_row(theme, nav, active, &counts, &handlers.navigate)),
        )
        .child(group_label(theme, "RECENT RUNS"));

    let recent =
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .children(state.runs().into_iter().take(RECENT).map(|run| {
                let open = Rc::clone(&handlers.recent_run);
                let run_id = run.run_id.clone();
                div()
                    .id(SharedString::from(format!("recent-{}", run.run_id)))
                    .h(px(32.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(9.0))
                    .rounded(px(9.0))
                    .text_size(px(13.5))
                    .text_color(theme.muted)
                    .cursor_pointer()
                    .hover({
                        let hover = theme.row_selected().opacity(0.6);
                        move |s| s.bg(hover)
                    })
                    .on_click(move |_, window, cx| open(run_id.clone(), window, cx))
                    .debug_selector(|| format!("recent-{}", run.run_id))
                    .child(kit::dot(kit::status_color(theme, run.status), 7.0))
                    .child(kit::exec_glyph(theme, run.executor.as_str(), 12.0))
                    .child(div().flex_1().min_w_0().truncate().child(run.title.clone()))
            }));

    let settings = Rc::clone(&handlers.settings);
    let foot = div()
        .flex_none()
        .h(px(52.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .gap(px(10.0))
        .border_t_1()
        .border_color(theme.hairline)
        .child(
            div()
                .size(px(24.0))
                .rounded(px(12.0))
                .bg(theme.raised)
                .flex()
                .items_center()
                .justify_center()
                .text_size(px(12.0))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.text)
                .child("L"),
        )
        .child(
            div()
                .flex_1()
                .text_size(px(14.0))
                .text_color(theme.text)
                .child("Local"),
        )
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.0))
                .text_size(px(13.0))
                .text_color(theme.muted)
                .child(kit::dot(daemon_color, 7.0))
                .child(daemon_text),
        )
        .child(
            kit::icon_button(theme, "settings", Icon::Settings, 28.0)
                .on_click(move |_, window, cx| settings(window, cx)),
        );

    div()
        .id("sidebar")
        .w(px(WIDTH))
        .h_full()
        .flex_none()
        .flex()
        .flex_col()
        .bg(theme.shell)
        .child(top)
        .child(
            div()
                .id("sidebar-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .px(px(10.0))
                .child(main)
                .child(recent),
        )
        .child(foot)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn badges_follow_the_counts() {
        let counts = summary::Attention {
            need: 2,
            running: 3,
            bots: 5,
        };
        assert_eq!(
            badge(Nav::Inbox, &counts),
            Some(RowBadge {
                text: "2".into(),
                hot: true
            })
        );
        assert_eq!(
            badge(Nav::Runs, &counts),
            Some(RowBadge {
                text: "3 live".into(),
                hot: false
            })
        );
        assert_eq!(badge(Nav::Bots, &counts).map(|b| b.text), Some("5".into()));
        assert_eq!(badge(Nav::Flows, &counts), None);
        let none = summary::Attention::default();
        assert_eq!(badge(Nav::Inbox, &none), None);
        assert_eq!(badge(Nav::Runs, &none), None);
    }

    #[test]
    fn daemon_labels() {
        assert_eq!(daemon_label(ConnectionStatus::Connected), "renod");
        assert_eq!(daemon_label(ConnectionStatus::Disconnected), "offline");
        assert_eq!(daemon_label(ConnectionStatus::Connecting), "connecting");
    }
}
