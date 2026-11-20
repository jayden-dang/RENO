//! The sidebar: "Start a task", then "Threads" grouped by project (one folder per repository
//! with its threads, each a status dot or spinner, a title, a text badge when it waits for you,
//! and an age), and a compact Settings row as its footer (app-shell brief, "Sidebar"). It sits
//! on `shell`, under the status bar. A closed folder still lists the threads that wait for you.

use std::rc::Rc;

use gpui::{AnyElement, App, Div, FontWeight, Role, SharedString, Window, div, prelude::*, px};
use reno_client::ConnectionStatus;
use reno_proto::RunId;

mod rows;
mod state;

use super::icon::{Icon, icon};
use super::kit;
use super::theme::Theme;
use crate::nav::Nav;
use crate::store::Store;

use crate::vm::projects::{Badge, Dot, Folder, FolderKey};
use crate::vm::settings_nav::{self, Pip};
pub(crate) use rows::{ActHandler, SidebarDrag, View, overlays};
pub(crate) use state::{Act, MAX_WIDTH, MIN_WIDTH, State};

/// Width of the sidebar column, padding included.
pub(crate) const WIDTH: f32 = 248.0;
/// Where the status bar's first control starts: clear of the traffic lights.
pub(crate) const MARK_INSET: f32 = 84.0;
/// A sidebar row (a thread, Settings) and a project header.
const ROW: f32 = 28.0;

pub(crate) type NavHandler = Rc<dyn Fn(Nav, &mut Window, &mut App)>;
pub(crate) type RunHandler = Rc<dyn Fn(RunId, &mut Window, &mut App)>;
pub(crate) type FolderHandler = Rc<dyn Fn(FolderKey, &mut Window, &mut App)>;

pub(crate) struct SidebarHandlers {
    pub(crate) navigate: NavHandler,
    pub(crate) recent_run: RunHandler,
    /// A project's "+": the new-thread state with that project as the working folder.
    pub(crate) new_thread: FolderHandler,
    /// Opens or closes a project folder.
    pub(crate) toggle_folder: FolderHandler,
    /// The folder's shield: the project's Allowlist and History.
    pub(crate) open_project: FolderHandler,
}

/// The daemon's state in a word, for Home's chip.
pub(crate) fn daemon_label(status: ConnectionStatus) -> &'static str {
    match status {
        ConnectionStatus::Connected => "renod",
        ConnectionStatus::Connecting => "connecting",
        ConnectionStatus::Disconnected => "offline",
    }
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

/// A thread row's badge: a tinted word, never a saturated disc. "Needs input" is warning,
/// "Needs approval" success (brief, "Sidebar").
pub(crate) fn badge_tag(theme: &Theme, badge: Badge) -> Div {
    kit::tag(
        match badge {
            Badge::NeedsInput => theme.warning,
            Badge::NeedsApproval => theme.success,
        },
        badge.label(),
    )
    .h(px(18.0))
    .px(px(6.0))
    .text_size(px(11.0))
    .whitespace_nowrap()
}

fn folder_view(
    theme: &Theme,
    folder: &Folder,
    selected: Option<&RunId>,
    handlers: &SidebarHandlers,
    view: &View<'_>,
) -> Div {
    let toggle = Rc::clone(&handlers.toggle_folder);
    let key = folder.key.clone();
    let slug = folder.name.replace(' ', "-");
    let group = SharedString::from(format!("folder-group-{slug}"));
    let hover = theme.row_selected().opacity(0.6);
    let head = div()
        .id(SharedString::from(format!("folder-{slug}")))
        .debug_selector({
            let slug = slug.clone();
            move || format!("folder-{slug}")
        })
        .flex_1()
        .min_w_0()
        .h(px(ROW))
        .pl(px(4.0))
        .pr(px(6.0))
        .flex()
        .items_center()
        .gap(px(6.0))
        .rounded(px(7.0))
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
        );
    // Dim until the header is pointed at: the project's Allowlist and History, and a new thread.
    let shield = {
        let open = Rc::clone(&handlers.open_project);
        let key = folder.key.clone();
        kit::icon_button(theme, format!("project-{slug}"), Icon::Shield, 24.0)
            .opacity(0.4)
            .group_hover(group.clone(), |s| s.opacity(1.0))
            .on_click(move |_, window, cx| open(key.clone(), window, cx))
    };
    // "No project" has no folder to work in, so it has no "+".
    let plus = matches!(folder.key, FolderKey::Repo(_)).then(|| {
        let new_thread = Rc::clone(&handlers.new_thread);
        let key = folder.key.clone();
        kit::icon_button(theme, format!("new-thread-{slug}"), Icon::Plus, 24.0)
            .opacity(0.4)
            .group_hover(group.clone(), |s| s.opacity(1.0))
            .on_click(move |_, window, cx| new_thread(key.clone(), window, cx))
    });
    div()
        .flex()
        .flex_col()
        .gap(px(1.0))
        .child(
            div()
                .group(group)
                .flex()
                .items_center()
                .child(head)
                .child(shield)
                .children(plus),
        )
        .children(
            folder
                .rows
                .iter()
                .map(|row| rows::folder_row(theme, view, row, selected, handlers)),
        )
        .children(rows::folder_extras(theme, view, folder))
}

/// The footer's Settings row: a compact 28 pt rounded row, the gear (marked while the daemon is
/// not connected), the word, and how many executors can't take runs, as a tinted count.
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
        .child(icon(Icon::PhGear, 15.0, theme.muted))
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
        .h(px(ROW))
        .px(px(10.0))
        .flex()
        .items_center()
        .gap(px(10.0))
        .rounded(px(7.0))
        .text_size(px(13.0))
        .text_color(if selected { theme.text } else { theme.muted })
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
            // A tinted count, not a saturated disc: how many agents cannot take runs.
            r.child(
                div()
                    .id("settings-count")
                    .debug_selector(|| "settings-count".to_owned())
                    .flex_none()
                    .min_w(px(18.0))
                    .h(px(18.0))
                    .px(px(5.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(5.0))
                    .bg(theme.danger.opacity(0.18))
                    .text_color(theme.danger)
                    .font_family(kit::MONO)
                    .text_size(px(11.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(needing.to_string()),
            )
        })
}

/// What the ROAD-38 draft puts in the sidebar: B's restart banner above everything, and B's
/// skeleton rows in place of the thread list while its connecting gate is up.
#[derive(Default)]
pub(crate) struct DraftSlots {
    pub(crate) banner: Option<AnyElement>,
    pub(crate) skeleton: Option<AnyElement>,
}

/// The sidebar column. `active` marks the current destination; `needing` is how many executors
/// can't take runs.
// The draft's slots made the eighth argument; the winner folds them into `View`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn sidebar(
    theme: &Theme,
    store: &Store,
    active: Nav,
    needing: usize,
    handlers: &SidebarHandlers,
    view: &View<'_>,
    width: f32,
    slots: DraftSlots,
) -> impl IntoElement {
    let selected_run = store.looking_at().cloned();

    let start = {
        let go = Rc::clone(&handlers.navigate);
        let on = active == Nav::Home;
        let hover = theme.row_selected();
        div()
            .id("new-task")
            .debug_selector(|| "new-task".to_owned())
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
            .aria_label("Start a task")
            .focusable()
            .tab_stop(true)
            .focus_visible(kit::focus_ring(theme))
            .when(on, |r| r.bg(theme.row_selected()))
            .when(!on, |r| r.hover(move |s| s.bg(hover.opacity(0.6))))
            .on_click(move |_, window, cx| go(Nav::Home, window, cx))
            .child(icon(Icon::Plus, 16.0, theme.text))
            .child(div().flex_1().child("Start a task"))
    };

    let list = match slots.skeleton {
        Some(skeleton) => skeleton,
        None => rows::section(theme, view, selected_run.as_ref(), handlers, |folder| {
            folder_view(theme, folder, selected_run.as_ref(), handlers, view)
        })
        .into_any_element(),
    };

    let foot = div()
        .flex_none()
        .px(px(10.0))
        .pt(px(4.0))
        .pb(px(10.0))
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
        .relative()
        .w(px(width))
        .h_full()
        .flex_none()
        .flex()
        .flex_col()
        .bg(theme.shell)
        .child(
            div()
                .id("sidebar-scroll")
                .flex_1()
                .min_h_0()
                .overflow_y_scroll()
                .track_scroll(&view.state.scroll)
                .px(px(10.0))
                .children(slots.banner)
                .child(start)
                .child(list),
        )
        .child(foot)
        .child(rows::resize_handle(theme, view.state.resizing))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daemon_labels() {
        assert_eq!(daemon_label(ConnectionStatus::Connected), "renod");
        assert_eq!(daemon_label(ConnectionStatus::Disconnected), "offline");
        assert_eq!(daemon_label(ConnectionStatus::Connecting), "connecting");
    }
}
