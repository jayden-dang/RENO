//! The thread list of the sidebar: rows with their "\u{2026}" and hover details, the Pinned
//! section, the Archived row, "Show N more", the resize edge, and the popups (the row menu and
//! the close confirm). The parts that stay as the app shell has them (the Start a task button,
//! the project headers, the Settings footer) are in `sidebar.rs`.

use std::collections::HashSet;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Context, Div, FontWeight, Hsla, MouseButton, MouseDownEvent, Render, Role,
    SharedString, Stateful, Window, div, prelude::*, px,
};
use reno_client::thread_desk::ThreadMarks;
use reno_proto::{RunId, ThreadId};

use super::state::{Act, State};
use super::{SidebarHandlers, badge_tag, run_dot};
use crate::store::Store;
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, Tone};
use crate::ui::menu;
use crate::ui::theme::Theme;
use crate::vm::changes::Loaded;
use crate::vm::projects::{self, Dot, Edit, Folder, Listing, ThreadRow};
use crate::vm::threads::{self, Entry, Item};

pub(crate) type ActHandler = Rc<dyn Fn(Act, &mut Window, &mut App)>;

/// What the sidebar draws: the list (pinned and archived threads taken out of their projects),
/// the state of what is open, and what a click calls.
pub(crate) struct View<'a> {
    /// Renaming, pinning and archiving exist.
    pub(crate) edit: bool,
    /// "Copy link" is in the row menu (the ROAD-38 draft is on).
    pub(crate) links: bool,
    pub(crate) listing: Listing,
    pub(crate) state: &'a State,
    pub(crate) store: &'a Store,
    pub(crate) act: ActHandler,
}

impl<'a> View<'a> {
    pub(crate) fn new(
        store: &'a Store,
        toggled: &HashSet<projects::FolderKey>,
        state: &'a State,
        act: ActHandler,
    ) -> Self {
        let marks: ThreadMarks = store.thread_marks().unwrap_or_default();
        let runs_state = store.state();
        let runs = runs_state.runs();
        let edit = Edit {
            marks: &marks,
            expanded: &state.expanded,
        };
        let listing = projects::listing(
            &runs,
            |dir| store.root_of(dir),
            &projects::Inputs {
                seen: store.seen(),
                toggled,
                looking_at: store.looking_at(),
                now_ms: store.now_ms(),
                thread_of: &|run| store.thread_of(run),
                edit: Some(&edit),
            },
        );
        Self {
            edit: store.capabilities().thread_edit,
            links: false,
            listing,
            state,
            store,
            act,
        }
    }
}

impl View<'_> {
    /// Offers "Copy link" in the row menu (the ROAD-38 draft).
    pub(crate) fn with_links(mut self, links: bool) -> Self {
        self.links = links;
        self
    }
}

/// What a sidebar drag carries; nothing is drawn under the pointer.
#[derive(Clone, Copy)]
pub(crate) struct SidebarDrag;

pub(crate) struct NoLabel;

impl Render for NoLabel {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// A 22 pt icon button for a row's action.
fn mini(theme: &Theme, id: String, glyph: Icon, ink: Hsla, label: &str) -> Stateful<Div> {
    let hover = theme.raised;
    let selector = id.clone();
    div()
        .id(SharedString::from(id))
        .debug_selector(move || selector.clone())
        .flex_none()
        .size(px(22.0))
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(6.0))
        .cursor_pointer()
        .hover(move |s| s.bg(hover))
        .role(Role::Button)
        .aria_label(SharedString::from(label.to_owned()))
        .child(icon(glyph, 14.0, ink))
}

fn header(theme: &Theme, text: &'static str) -> Div {
    div()
        .px(px(10.0))
        .pt(px(16.0))
        .pb(px(6.0))
        .text_size(px(11.5))
        .text_color(theme.faint)
        .child(text)
}

/// Reports where its parent was painted, so a menu can hang under the row.
fn track(v: &View<'_>, thread: &ThreadId) -> impl IntoElement {
    let rows = v.state.rows.clone();
    let thread = thread.clone();
    gpui::canvas(
        move |bounds, _, _| {
            rows.borrow_mut().insert(thread.clone(), bounds);
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_full()
}

/// One thread row. `archived` rows offer "Unarchive" and open as usual.
pub(crate) fn row(
    theme: &Theme,
    v: &View<'_>,
    row: &ThreadRow,
    selected: Option<&RunId>,
    handlers: &SidebarHandlers,
    archived: bool,
) -> AnyElement {
    let thread = row.thread.clone();
    let open = Rc::clone(&handlers.recent_run);
    let run_id = row.run.clone();
    let is_selected = selected.is_some_and(|run| row.members.contains(run));
    let hover_fill = theme.row_selected().opacity(0.6);
    // A run's title may carry its bot and repository ("builder \u{b7} atlas-api: ..."); the
    // folder already says the repository.
    let title = crate::summary::parse_title(&row.title).what.to_owned();
    let spoken = match row.badge {
        Some(badge) => format!("{title}, {}", badge.label()),
        None => title.clone(),
    };
    let info = thread.as_ref().and_then(|t| v.store.state().thread(t));
    let hovered = thread.as_ref().is_some_and(|t| {
        v.state.hover.as_ref() == Some(t) || v.state.menu.as_ref().is_some_and(|m| &m.thread == t)
    });
    let renaming = thread.as_ref().and_then(|t| v.state.rename_of(t)).cloned();
    let ink = if is_selected {
        theme.text
    } else if archived {
        theme.faint
    } else {
        theme.muted
    };

    let lead = if row.dot == Dot::Running {
        div().flex_none().size(px(11.0)).child(kit::spinner(
            SharedString::from(format!("row-spin-{}", row.run)),
            11.0,
            theme.accent,
        ))
    } else {
        run_dot(theme, row.dot, 7.0)
    };

    let title_el = match &renaming {
        Some(field) => div()
            .flex_1()
            .min_w_0()
            .h(px(22.0))
            .px(px(6.0))
            .flex()
            .items_center()
            .rounded(px(5.0))
            .border_1()
            .border_color(theme.accent)
            .bg(theme.canvas)
            .text_color(theme.text)
            .child(field.clone())
            .into_any_element(),
        None => div()
            .flex_1()
            .min_w_0()
            .truncate()
            .child(title.clone())
            .into_any_element(),
    };

    // What sits at the right end of the line: the age, or the row's "\u{2026}" while it is pointed
    // at, or "Unarchive" in the archive.
    let slot: AnyElement = match &thread {
        Some(t) if archived && v.edit => {
            let (act, t) = (Rc::clone(&v.act), t.clone());
            let raised = theme.raised;
            div()
                .id(SharedString::from(format!("unarchive-{}", row.run)))
                .debug_selector({
                    let run = row.run.clone();
                    move || format!("unarchive-{run}")
                })
                .flex_none()
                .px(px(6.0))
                .h(px(20.0))
                .flex()
                .items_center()
                .rounded(px(5.0))
                .text_size(px(11.5))
                .text_color(theme.muted)
                .cursor_pointer()
                .hover(move |s| s.bg(raised))
                .child("Unarchive")
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    act(Act::Unarchive(t.clone()), window, cx);
                })
                .into_any_element()
        }
        Some(t) if hovered && !archived => {
            let (act, t) = (Rc::clone(&v.act), t.clone());
            mini(
                theme,
                format!("more-{}", row.run),
                Icon::More,
                theme.muted,
                "More",
            )
            .on_click(move |event, window, cx| {
                cx.stop_propagation();
                let at = event.position();
                act(
                    Act::Menu(
                        t.clone(),
                        Some(gpui::point(at.x - px(8.0), at.y + px(12.0))),
                    ),
                    window,
                    cx,
                );
            })
            .into_any_element()
        }
        _ => div()
            .flex_none()
            .whitespace_nowrap()
            .font_family(kit::MONO)
            .text_size(px(11.0))
            .text_color(theme.faint)
            .child(row.age.clone())
            .into_any_element(),
    };

    let badge = row.badge.map(|b| {
        let id = format!("badge-{}", row.run);
        badge_tag(theme, b)
            .id(SharedString::from(id.clone()))
            .debug_selector(move || id.clone())
    });

    let mut el = div()
        .id(SharedString::from(format!("recent-{}", row.run)))
        .debug_selector({
            let run = row.run.clone();
            move || format!("recent-{run}")
        })
        .relative()
        .h(px(28.0))
        .ml(px(14.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .gap(px(9.0))
        .rounded(px(7.0))
        .text_size(px(13.0))
        .text_color(ink)
        .cursor_pointer()
        .role(Role::Button)
        .aria_label(SharedString::from(spoken))
        .when(is_selected, |r| r.bg(theme.row_selected()))
        .when(!is_selected, |r| r.hover(move |s| s.bg(hover_fill)))
        .child(lead)
        .child(title_el)
        .children(badge)
        .child(slot);
    if renaming.is_none() {
        el = el
            .focusable()
            .tab_stop(true)
            .focus_visible(kit::focus_ring(theme))
            .on_click(move |_, window, cx| open(run_id.clone(), window, cx));
    }
    if let Some(t) = &thread {
        el = el.child(track(v, t));
        let (act, th) = (Rc::clone(&v.act), t.clone());
        el = el.on_hover(move |on, window, cx| act(Act::Hover(th.clone(), *on), window, cx));
        let (act, th) = (Rc::clone(&v.act), t.clone());
        el = el.on_mouse_down(
            MouseButton::Right,
            move |event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                act(Act::Menu(th.clone(), Some(event.position)), window, cx);
            },
        );
    }
    // Resting on the row says the full title and where it lives.
    if renaming.is_none() {
        el = el.tooltip(kit::tip(threads::hover_text(row, info, &title)));
    }

    let failed = thread
        .as_ref()
        .zip(v.state.failure.as_ref())
        .filter(|(t, f)| *t == &f.thread)
        .map(|(_, f)| f.message.clone());
    div()
        .flex()
        .flex_col()
        .child(el)
        .children(failed.map(|message| {
            div()
                .id(SharedString::from(format!("failed-{}", row.run)))
                .debug_selector({
                    let run = row.run.clone();
                    move || format!("failed-{run}")
                })
                .ml(px(14.0))
                .px(px(10.0))
                .pl(px(26.0))
                .py(px(2.0))
                .text_size(px(11.5))
                .text_color(theme.danger)
                .child(message)
        }))
        .into_any_element()
}

/// "Show 6 more" / "Show less" under a project's threads.
pub(crate) fn folder_extras(theme: &Theme, v: &View<'_>, folder: &Folder) -> Option<AnyElement> {
    let label = threads::more_label(folder.hidden, folder.expanded)?;
    let slug = folder.name.replace(' ', "-");
    let (act, key) = (Rc::clone(&v.act), folder.key.clone());
    let hover = theme.muted;
    Some(
        div()
            .id(SharedString::from(format!("more-{slug}")))
            .debug_selector(move || format!("more-{slug}"))
            .ml(px(14.0))
            .px(px(10.0))
            .h(px(26.0))
            .flex()
            .items_center()
            .gap(px(6.0))
            .rounded(px(7.0))
            .text_size(px(12.0))
            .text_color(theme.faint)
            .cursor_pointer()
            .hover(move |s| s.text_color(hover))
            .role(Role::Button)
            .child(icon(
                if folder.expanded {
                    Icon::ChevUp
                } else {
                    Icon::ChevDown
                },
                11.0,
                theme.faint,
            ))
            .child(label)
            .on_click(move |_, window, cx| act(Act::ToggleMore(key.clone()), window, cx))
            .into_any_element(),
    )
}

/// The "Archived \u{b7} 4" row, and the archive under it when it is open.
fn archived_row(
    theme: &Theme,
    v: &View<'_>,
    selected: Option<&RunId>,
    handlers: &SidebarHandlers,
) -> Option<Div> {
    let count = v.listing.archived.len();
    if count == 0 || !v.edit {
        return None;
    }
    let open = v.state.archived_open;
    let act = Rc::clone(&v.act);
    let hover = theme.row_selected().opacity(0.6);
    let head = div()
        .id("archived-row")
        .debug_selector(|| "archived-row".to_owned())
        .h(px(28.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .rounded(px(7.0))
        .text_size(px(13.0))
        .text_color(theme.muted)
        .cursor_pointer()
        .role(Role::Button)
        .hover(move |s| s.bg(hover))
        .child(icon(
            if open { Icon::ChevDown } else { Icon::Fwd },
            12.0,
            theme.faint,
        ))
        .child(threads::archived_label(count))
        .on_click(move |_, window, cx| act(Act::ToggleArchived, window, cx));
    Some(
        div()
            .flex()
            .flex_col()
            .gap(px(1.0))
            .pt(px(8.0))
            .child(head)
            .when(open, |c| {
                c.children(
                    v.listing
                        .archived
                        .iter()
                        .map(|r| row(theme, v, r, selected, handlers, true)),
                )
            }),
    )
}

/// Everything under "Start a task" down to the footer: the Pinned section, "Threads" with its
/// projects, and the archive.
pub(crate) fn section(
    theme: &Theme,
    v: &View<'_>,
    selected: Option<&RunId>,
    handlers: &SidebarHandlers,
    folder_view: impl Fn(&Folder) -> Div,
) -> Div {
    let mut col = div().flex().flex_col();
    if !v.listing.pinned.is_empty() {
        col = col.child(header(theme, "Pinned")).child(
            div()
                .id("pinned-section")
                .debug_selector(|| "pinned-section".to_owned())
                .flex()
                .flex_col()
                .gap(px(1.0))
                .children(
                    v.listing
                        .pinned
                        .iter()
                        .map(|r| row(theme, v, r, selected, handlers, false)),
                ),
        );
    }
    col = col.child(header(theme, "Threads"));
    col.child(
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .pb(px(10.0))
            .children(v.listing.folders.iter().map(&folder_view))
            .when(v.listing.folders.is_empty(), |list| {
                list.child(
                    div()
                        .id("no-threads")
                        .px(px(10.0))
                        .py(px(8.0))
                        .text_size(px(12.5))
                        .text_color(theme.faint)
                        .child("No threads yet"),
                )
            })
            .children(archived_row(theme, v, selected, handlers)),
    )
}

/// The row of a folder.
pub(crate) fn folder_row(
    theme: &Theme,
    v: &View<'_>,
    r: &ThreadRow,
    selected: Option<&RunId>,
    handlers: &SidebarHandlers,
) -> AnyElement {
    row(theme, v, r, selected, handlers, false)
}

/// The sidebar's drag handle, on its right edge.
pub(crate) fn resize_handle(theme: &Theme, active: bool) -> Stateful<Div> {
    let accent = theme.accent;
    div()
        .id("sidebar-resize")
        .debug_selector(|| "sidebar-resize".to_owned())
        .group("sidebar-resize")
        .absolute()
        .top_0()
        .bottom_0()
        .right(px(-3.0))
        .w(px(7.0))
        .flex()
        .justify_center()
        .cursor_col_resize()
        .on_drag(SidebarDrag, |_, _, _, cx| cx.new(|_| NoLabel))
        .child(
            div()
                .w(px(2.0))
                .h_full()
                .rounded(px(1.0))
                .when(active, move |d| d.bg(accent))
                .group_hover("sidebar-resize", move |s| s.bg(accent.opacity(0.7))),
        )
}

// ---- the popups -----------------------------------------------------------------------------

/// Every popup that is open, ready to hang from the root view.
pub(crate) fn overlays(theme: &Theme, v: &View<'_>) -> Vec<AnyElement> {
    let mut out = Vec::new();
    let dismiss: menu::Dismiss = {
        let act = Rc::clone(&v.act);
        Rc::new(move |window, cx| act(Act::Dismiss, window, cx))
    };
    if let Some(m) = &v.state.menu {
        let facts = State::facts(v.store, &m.thread, v.links);
        let entries = if m.copy {
            threads::copy_entries(facts)
        } else {
            threads::menu(facts)
        };
        if !entries.is_empty() {
            out.push(menu::popup(
                m.at,
                thread_menu(theme, &m.thread, &entries, m.copy, &v.act),
                &dismiss,
            ));
        }
    }
    if let Some(confirm) = &v.state.confirm {
        out.push(menu::popup(
            confirm.at,
            menu::panel(theme, "sidebar-confirm", 288.0)
                .p(px(12.0))
                .child(confirm_body(theme, v, confirm)),
            &dismiss,
        ));
    }
    out
}

/// The menu of a thread: `entries` as rows. Copy opens its sub-list in place (`copy`), with a way
/// back as its first row.
fn thread_menu(
    theme: &Theme,
    thread: &ThreadId,
    entries: &[Entry],
    copy: bool,
    act: &ActHandler,
) -> Stateful<Div> {
    let mut m = menu::panel(theme, "sidebar-menu", 232.0);
    if copy {
        let back = Rc::clone(act);
        m = m.child(
            menu::row(
                theme,
                "sidebar-menu-back".to_owned(),
                "Copy",
                None,
                None,
                true,
                false,
            )
            .font_weight(FontWeight::MEDIUM)
            .child(icon(Icon::Back, 13.0, theme.muted))
            .on_click(move |_, window, cx| back(Act::CopyList(false), window, cx)),
        );
    }
    for entry in entries {
        match entry {
            Entry::Divider => m = m.child(menu::divider(theme)),
            Entry::Item {
                item,
                label,
                hint,
                enabled,
                note,
                danger,
            } => {
                let id = format!("sidebar-menu-{}", label.to_lowercase().replace(' ', "-"));
                let mut line = menu::row(theme, id, label, *note, *hint, *enabled, *danger);
                if *item == Item::Copy {
                    line = line.child(icon(Icon::PhCaretRight, 12.0, theme.faint));
                }
                if *enabled {
                    let (act, thread, item) = (Rc::clone(act), thread.clone(), *item);
                    line = line.on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        act(Act::Do(thread.clone(), item), window, cx);
                    });
                }
                m = m.child(line);
            }
        }
    }
    m
}

/// The close question and its two buttons. It is always asked; the words come from
/// `vm::threads::close_question`, and a live run says so and cannot be closed.
fn confirm_body(theme: &Theme, v: &View<'_>, confirm: &super::state::Confirm) -> Div {
    let store = v.store;
    let info = store.state().thread(&confirm.thread);
    let held = store
        .changes(&confirm.run)
        .and_then(|c| match &c.workspace {
            Loaded::Ready(Some(w)) => w.changes,
            _ => None,
        });
    let question = info
        .map(|i| threads::close_question(i, held.as_ref()))
        .unwrap_or_default();
    let busy = info.is_some_and(|i| i.status == reno_proto::ThreadStatus::Active);
    let (ok, cancel) = (Rc::clone(&v.act), Rc::clone(&v.act));
    let mut confirm_button = kit::line_button(
        theme,
        "sidebar-close-confirm",
        None,
        "Close thread",
        Tone::Danger,
        !busy,
    );
    if !busy {
        confirm_button =
            confirm_button.on_click(move |_, window, cx| ok(Act::ConfirmClose, window, cx));
    }
    div()
        .flex()
        .flex_col()
        .gap(px(8.0))
        .child(
            div()
                .text_size(px(13.0))
                .font_weight(FontWeight::SEMIBOLD)
                .child("Close this thread?"),
        )
        .child(
            div()
                .id("sidebar-close-question")
                .debug_selector(|| "sidebar-close-question".to_owned())
                .text_size(px(12.5))
                .line_height(px(18.0))
                .text_color(theme.muted)
                .child(question),
        )
        .child(
            div().flex().gap(px(8.0)).child(confirm_button).child(
                kit::line_button(
                    theme,
                    "sidebar-close-cancel",
                    None,
                    if busy { "OK" } else { "Cancel" },
                    Tone::Quiet,
                    true,
                )
                .on_click(move |_, window, cx| cancel(Act::Dismiss, window, cx)),
            ),
        )
}
