//! The sidebar: Home, Runs and Triggers, then "Projects": one folder per repository with its
//! threads (a run of its own on a daemon without threads) and their status dots, and the Settings
//! row as its footer (ui-brief 2026-11-19, settings). It sits on `shell`. A closed folder still
//! lists the runs that wait for you.

use std::collections::HashSet;
use std::rc::Rc;

use gpui::{App, Div, FontWeight, Role, SharedString, Window, div, prelude::*, px};
use reno_client::ConnectionStatus;
use reno_proto::RunId;

use super::icon::{Icon, icon};
use super::kit;
use super::page::{self, CountTone};
use super::theme::Theme;
use crate::nav::Nav;
use crate::store::Store;
use crate::summary;
use crate::vm::projects::{self, Dot, Folder, FolderKey};
use crate::vm::settings_nav::{self, Pip};

/// Width of the sidebar column, padding included.
pub(crate) const WIDTH: f32 = 248.0;
/// Height of the titlebar band the traffic lights sit in.
pub(crate) const TITLEBAR: f32 = 40.0;

pub(crate) type NavHandler = Rc<dyn Fn(Nav, &mut Window, &mut App)>;
pub(crate) type ActionHandler = Rc<dyn Fn(&mut Window, &mut App)>;
pub(crate) type RunHandler = Rc<dyn Fn(RunId, &mut Window, &mut App)>;
pub(crate) type FolderHandler = Rc<dyn Fn(FolderKey, &mut Window, &mut App)>;

pub(crate) struct SidebarHandlers {
    pub(crate) navigate: NavHandler,
    pub(crate) hide: ActionHandler,
    pub(crate) recent_run: RunHandler,
    /// Opens or closes a project folder.
    pub(crate) toggle_folder: FolderHandler,
    /// The folder's shield: the project's Allowlist and History.
    pub(crate) open_project: FolderHandler,
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
        Nav::Runs if counts.running > 0 => (format!("{} live", counts.running), false),
        _ => return None,
    };
    Some(RowBadge { text, hot })
}

/// The daemon's state in a word, for Home's chip.
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

/// A run's dot as drawn: a filled circle, a ring for a stopped run, nothing for a seen one.
pub(crate) fn run_dot(theme: &Theme, dot: Dot, size: f32) -> Div {
    let fill = match dot {
        Dot::Running => Some(theme.accent),
        Dot::Waiting => Some(theme.warning),
        Dot::New => Some(theme.text),
        Dot::Failed => Some(theme.danger),
        Dot::Seen | Dot::Stopped => None,
    };
    let base = div().flex_none().size(px(size)).rounded(px(size / 2.0));
    match (fill, dot) {
        (Some(color), _) => base.bg(color),
        (None, Dot::Stopped) => base.border_1().border_color(theme.faint),
        (None, _) => base,
    }
}

fn folder_view(
    theme: &Theme,
    folder: &Folder,
    selected: Option<&RunId>,
    handlers: &SidebarHandlers,
) -> Div {
    let toggle = Rc::clone(&handlers.toggle_folder);
    let key = folder.key.clone();
    let slug = folder.name.replace(' ', "-");
    let hover = theme.row_selected().opacity(0.6);
    let head = div()
        .id(SharedString::from(format!("folder-{slug}")))
        .debug_selector({
            let slug = slug.clone();
            move || format!("folder-{slug}")
        })
        .h(px(30.0))
        .pl(px(4.0))
        .pr(px(10.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .rounded(px(9.0))
        .text_size(px(13.0))
        .font_weight(FontWeight::MEDIUM)
        .text_color(theme.text)
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .on_click(move |_, window, cx| toggle(key.clone(), window, cx))
        .child(icon(
            if folder.open {
                Icon::ChevDown
            } else {
                Icon::Fwd
            },
            12.0,
            theme.faint,
        ))
        .child(icon(Icon::PhFolder, 15.0, theme.muted))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .child(folder.name.clone()),
        )
        .when(folder.waiting > 0, |d| {
            d.child(
                div()
                    .text_size(px(12.0))
                    .text_color(theme.warning)
                    .child(format!("{} waiting", folder.waiting)),
            )
        });
    let shield = {
        let open = Rc::clone(&handlers.open_project);
        let key = folder.key.clone();
        let group = SharedString::from(format!("folder-group-{slug}"));
        kit::icon_button(theme, format!("project-{slug}"), Icon::Shield, 24.0)
            .debug_selector({
                let slug = slug.clone();
                move || format!("project-{slug}")
            })
            .opacity(0.45)
            .group_hover(group, |s| s.opacity(1.0))
            .on_click(move |_, window, cx| open(key.clone(), window, cx))
    };
    let head = div()
        .group(SharedString::from(format!("folder-group-{slug}")))
        .flex()
        .items_center()
        .child(head.flex_1())
        .child(shield);
    div()
        .flex()
        .flex_col()
        .gap(px(1.0))
        .child(head)
        .children(folder.rows.iter().map(|row| {
            let open = Rc::clone(&handlers.recent_run);
            let run_id = row.run.clone();
            let is_selected = selected.is_some_and(|run| row.members.contains(run));
            let hover = theme.row_selected().opacity(0.6);
            div()
                .id(SharedString::from(format!("recent-{}", row.run)))
                .debug_selector({
                    let run = row.run.clone();
                    move || format!("recent-{run}")
                })
                .h(px(30.0))
                .ml(px(14.0))
                .px(px(10.0))
                .flex()
                .items_center()
                .gap(px(9.0))
                .rounded(px(9.0))
                .text_size(px(13.0))
                .text_color(if is_selected { theme.text } else { theme.muted })
                .cursor_pointer()
                .when(is_selected, |r| r.bg(theme.row_selected()))
                .when(!is_selected, |r| r.hover(move |s| s.bg(hover)))
                .on_click(move |_, window, cx| open(run_id.clone(), window, cx))
                .child(run_dot(theme, row.dot, 7.0))
                .child(div().flex_1().min_w_0().truncate().child(row.title.clone()))
                .child(
                    div()
                        .font_family(kit::MONO)
                        .text_size(px(11.0))
                        .text_color(theme.faint)
                        .child(row.age.clone()),
                )
        }))
}

/// The footer's Settings row: the gear (marked while the daemon is not connected), the word, and
/// how many executors can't take runs.
pub(crate) fn settings_row(
    theme: &Theme,
    needing: usize,
    connection: ConnectionStatus,
    selected: bool,
    on_nav: &NavHandler,
) -> impl IntoElement {
    let on_nav = Rc::clone(on_nav);
    let hover = theme.row_selected();
    let pip = settings_nav::daemon_pip(connection).map(|pip| match pip {
        Pip::Warning => theme.warning,
        Pip::Danger => theme.danger,
    });
    let gear = div()
        .relative()
        .flex_none()
        .child(icon(Icon::PhGear, 16.0, theme.muted))
        .children(pip.map(|tone| {
            div()
                .id("settings-pip")
                .debug_selector(|| "settings-pip".to_owned())
                .absolute()
                .top(px(-2.0))
                .right(px(-3.0))
                .size(px(7.0))
                .rounded(px(3.5))
                .bg(tone)
                .border_1()
                .border_color(theme.shell)
        }));
    div()
        .id("nav-Settings")
        .debug_selector(|| "nav-Settings".to_owned())
        .flex_1()
        .h(px(34.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .gap(px(10.0))
        .rounded(px(9.0))
        .text_size(px(14.0))
        .text_color(theme.text)
        .cursor_pointer()
        .role(Role::Button)
        .aria_label(SharedString::from(settings_nav::settings_label(needing)))
        .focusable()
        .tab_stop(true)
        .focus_visible(kit::focus_ring(theme))
        .when(selected, |r| r.bg(theme.row_selected()))
        .when(!selected, |r| r.hover(move |s| s.bg(hover.opacity(0.6))))
        .on_click(move |_, window, cx| on_nav(Nav::Settings, window, cx))
        .child(gear)
        .child(div().flex_1().child("Settings"))
        .when(needing > 0, |r| {
            r.child(
                page::count(theme, needing, CountTone::Danger)
                    .id("settings-count")
                    .debug_selector(|| "settings-count".to_owned()),
            )
        })
}

/// The sidebar column. `active` marks the current destination; `needing` is how many executors
/// can't take runs.
pub(crate) fn sidebar(
    theme: &Theme,
    store: &Store,
    active: Nav,
    needing: usize,
    toggled: &HashSet<FolderKey>,
    handlers: &SidebarHandlers,
) -> impl IntoElement {
    let state = store.state();
    let counts = summary::attention(state);

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
        .children(Nav::ROWS.map(|nav| nav_row(theme, nav, active, &counts, &handlers.navigate)))
        .child(group_label(theme, "Projects"));

    let runs = state.runs();
    let folders = projects::folders(
        &runs,
        |dir| store.root_of(dir),
        &projects::Inputs {
            seen: store.seen(),
            toggled,
            looking_at: store.looking_at(),
            now_ms: store.now_ms(),
            thread_of: &|run| store.thread_of(run),
        },
    );
    let selected_run = store.looking_at().cloned();
    let recent = div().flex().flex_col().gap(px(2.0)).pb(px(10.0)).children(
        folders
            .iter()
            .map(|folder| folder_view(theme, folder, selected_run.as_ref(), handlers)),
    );

    let foot = div()
        .flex_none()
        .h(px(52.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .border_t_1()
        .border_color(theme.hairline)
        .child(settings_row(
            theme,
            needing,
            store.connection(),
            active == Nav::Settings,
            &handlers.navigate,
        ));

    div()
        .id("sidebar")
        .debug_selector(|| "sidebar".to_owned())
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
            questions: 1,
            running: 3,
            bots: 5,
        };
        assert_eq!(
            badge(Nav::Runs, &counts),
            Some(RowBadge {
                text: "3 live".into(),
                hot: false
            })
        );
        assert_eq!(badge(Nav::Home, &counts), None);
        let none = summary::Attention::default();
        assert_eq!(badge(Nav::Runs, &none), None);
    }

    #[test]
    fn daemon_labels() {
        assert_eq!(daemon_label(ConnectionStatus::Connected), "renod");
        assert_eq!(daemon_label(ConnectionStatus::Disconnected), "offline");
        assert_eq!(daemon_label(ConnectionStatus::Connecting), "connecting");
    }
}
