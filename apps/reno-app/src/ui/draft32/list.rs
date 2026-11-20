//! The thread list of the draft: rows in each variant's shape, the pinned section (A) and strip
//! (C), B's filter header, the archived list, and "Show N more". The keep-as-today parts (the
//! Start a task button, the project headers, the Settings footer) stay in `sidebar.rs`.

use std::collections::HashSet;
use std::rc::Rc;

use gpui::{
    AnyElement, Context, Div, Hsla, MouseButton, MouseDownEvent, Render, Role, SharedString,
    Stateful, Window, div, prelude::*, px,
};
use reno_client::thread_desk::ThreadMarks;
use reno_proto::{RunId, ThreadId};

use super::super::icon::{Icon, icon};
use super::super::kit;
use super::super::sidebar::{SidebarHandlers, badge_tag, run_dot};
use super::super::theme::Theme;
use super::menu::ActHandler;
use super::state::{Act, State};
use crate::store::Store;
use crate::vm::draft32::{self, Variant};
use crate::vm::projects::{self, Dot, Edit, Folder, Listing, ThreadRow};

/// What the sidebar draws in the draft: the variant, the list as the variant arranges it, and the
/// state of what is open.
pub(crate) struct Draft<'a> {
    pub(crate) variant: Variant,
    /// The mocked controls exist.
    pub(crate) edit: bool,
    pub(crate) listing: Listing,
    pub(crate) state: &'a State,
    pub(crate) store: &'a Store,
    pub(crate) act: ActHandler,
}

impl<'a> Draft<'a> {
    pub(crate) fn new(
        variant: Variant,
        store: &'a Store,
        toggled: &HashSet<projects::FolderKey>,
        state: &'a State,
        act: ActHandler,
    ) -> Self {
        let marks = store.thread_marks().unwrap_or_default();
        let listing = listing(store, toggled, state, variant, &marks);
        Self {
            variant,
            edit: store.capabilities().thread_edit,
            listing,
            state,
            store,
            act,
        }
    }

    /// The row of `thread`, wherever the list has it.
    pub(crate) fn row_of(&self, thread: &ThreadId) -> Option<&ThreadRow> {
        let l = &self.listing;
        l.folders
            .iter()
            .flat_map(|f| f.rows.iter())
            .chain(&l.pinned)
            .chain(&l.archived)
            .find(|r| r.thread.as_ref() == Some(thread))
    }
}

fn listing(
    store: &Store,
    toggled: &HashSet<projects::FolderKey>,
    state: &State,
    variant: Variant,
    marks: &ThreadMarks,
) -> Listing {
    let runs_state = store.state();
    let runs = runs_state.runs();
    let edit = Edit {
        marks,
        expanded: &state.expanded,
        pinned_apart: variant.pinned_apart(),
    };
    projects::listing(
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
    )
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

/// A 22 pt icon button for a row's actions.
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

/// Reports where its parent was painted, so a menu or the hooks can find the row.
fn track(d: &Draft<'_>, thread: &ThreadId) -> impl IntoElement {
    let rows = d.state.rows.clone();
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

/// One thread row, in the variant's shape. `archived` rows offer "Unarchive" and open as usual;
/// the project is in A's hover details and B's second line.
fn row(
    theme: &Theme,
    d: &Draft<'_>,
    row: &ThreadRow,
    selected: Option<&RunId>,
    handlers: &SidebarHandlers,
    archived: bool,
) -> AnyElement {
    let variant = d.variant;
    let thread = row.thread.clone();
    let open = Rc::clone(&handlers.recent_run);
    let run_id = row.run.clone();
    let is_selected = selected.is_some_and(|run| row.members.contains(run));
    let hover_fill = theme.row_selected().opacity(0.6);
    let title = crate::summary::parse_title(&row.title).what.to_owned();
    let spoken = match row.badge {
        Some(badge) => format!("{title}, {}", badge.label()),
        None => title.clone(),
    };
    let info = thread.as_ref().and_then(|t| d.store.state().thread(t));
    let hovered = thread.as_ref().is_some_and(|t| {
        d.state.hover.as_ref() == Some(t) || d.state.menu.as_ref().is_some_and(|m| &m.thread == t)
    });
    let renaming = thread
        .as_ref()
        .and_then(|t| d.state.rename_of(t, false))
        .cloned();
    let tall = variant == Variant::B;
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

    // What sits at the right end of the line.
    let slot: AnyElement = match (&thread, archived, variant) {
        (Some(t), true, _) if d.edit => {
            let (act, t) = (Rc::clone(&d.act), t.clone());
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
                .hover({
                    let raised = theme.raised;
                    move |s| s.bg(raised)
                })
                .child("Unarchive")
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    act(Act::Unarchive(t.clone()), window, cx);
                })
                .into_any_element()
        }
        (Some(t), false, Variant::A) if hovered => more_button(theme, d, row, t).into_any_element(),
        (Some(t), false, Variant::B) if hovered => {
            let mut icons = div().flex_none().flex().items_center().gap(px(1.0));
            if d.edit {
                let (act, tt) = (Rc::clone(&d.act), t.clone());
                let pin = mini(
                    theme,
                    format!("pin-{}", row.run),
                    Icon::Pin,
                    if row.pinned {
                        theme.accent
                    } else {
                        theme.faint
                    },
                    if row.pinned { "Unpin" } else { "Pin" },
                )
                .tooltip(kit::tip(if row.pinned { "Unpin" } else { "Pin" }))
                .on_click({
                    let item = if row.pinned {
                        crate::vm::draft32::Item::Unpin
                    } else {
                        crate::vm::draft32::Item::Pin
                    };
                    move |_, window, cx| {
                        cx.stop_propagation();
                        act(Act::Do(tt.clone(), item), window, cx);
                    }
                });
                let (act, tt) = (Rc::clone(&d.act), t.clone());
                let archive = mini(
                    theme,
                    format!("archive-{}", row.run),
                    Icon::Inbox,
                    theme.faint,
                    "Archive",
                )
                .tooltip(kit::tip("Archive"))
                .on_click(move |_, window, cx| {
                    cx.stop_propagation();
                    act(
                        Act::Do(tt.clone(), crate::vm::draft32::Item::Archive),
                        window,
                        cx,
                    );
                });
                icons = icons.child(pin).child(archive);
            }
            icons
                .child(more_button(theme, d, row, t))
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

    // B draws the pin where it can always be seen; the others say it with the Pinned section.
    let pin_glyph = (variant == Variant::B && row.pinned && !archived && !(hovered && d.edit))
        .then(|| icon(Icon::Pin, 12.0, theme.accent));
    let badge = row.badge.map(|b| {
        let id = format!("badge-{}", row.run);
        badge_tag(theme, b)
            .id(SharedString::from(id.clone()))
            .debug_selector(move || id.clone())
    });

    let (first_badge, second_badge) = if tall { (None, badge) } else { (badge, None) };
    let first_line = div()
        .w_full()
        .flex()
        .items_center()
        .gap(px(9.0))
        .child(lead)
        .child(title_el)
        .children(pin_glyph)
        .children(first_badge)
        .child(slot);

    let body: AnyElement = if tall {
        let sub = draft32::subline(row, info, archived, row.badge.is_some());
        div()
            .w_full()
            .min_w_0()
            .flex()
            .flex_col()
            .gap(px(1.0))
            .child(first_line)
            .child(
                div()
                    .pl(px(16.0))
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .font_family(kit::MONO)
                            .text_size(px(11.0))
                            .text_color(theme.faint)
                            .child(sub),
                    )
                    .children(second_badge),
            )
            .into_any_element()
    } else {
        first_line.into_any_element()
    };

    let mut el = div()
        .id(SharedString::from(format!("recent-{}", row.run)))
        .debug_selector({
            let run = row.run.clone();
            move || format!("recent-{run}")
        })
        .relative()
        .min_h(px(if tall { 40.0 } else { 28.0 }))
        .ml(px(14.0))
        .px(px(10.0))
        .py(px(if tall { 3.0 } else { 0.0 }))
        .flex()
        .items_center()
        .rounded(px(7.0))
        .text_size(px(13.0))
        .text_color(ink)
        .cursor_pointer()
        .role(Role::Button)
        .aria_label(SharedString::from(spoken))
        .when(is_selected, |r| r.bg(theme.row_selected()))
        .when(!is_selected, |r| r.hover(move |s| s.bg(hover_fill)))
        .child(body);
    if renaming.is_none() {
        el = el
            .focusable()
            .tab_stop(true)
            .focus_visible(kit::focus_ring(theme))
            .on_click(move |_, window, cx| open(run_id.clone(), window, cx));
    }
    if let Some(t) = &thread {
        el = el.child(track(d, t));
        let (act, th) = (Rc::clone(&d.act), t.clone());
        el = el.on_hover(move |on, window, cx| act(Act::Hover(th.clone(), *on), window, cx));
        let (act, th) = (Rc::clone(&d.act), t.clone());
        el = el.on_mouse_down(
            MouseButton::Right,
            move |event: &MouseDownEvent, window, cx| {
                cx.stop_propagation();
                if variant == Variant::C {
                    act(Act::Card(th.clone()), window, cx);
                } else {
                    act(Act::Menu(th.clone(), Some(event.position)), window, cx);
                }
            },
        );
    }
    // A shows the full title and where it lives on a rest; B says it in the row; C opens a card.
    if variant == Variant::A && renaming.is_none() {
        let details = draft32::details(row, info, d.store.now_ms());
        let text = std::iter::once(title)
            .chain(
                details
                    .iter()
                    .filter(|(label, _)| ["Project", "Branch", "Harness", "Status"].contains(label))
                    .map(|(_, v)| v.clone()),
            )
            .collect::<Vec<_>>()
            .join(" \u{b7} ");
        el = el.tooltip(kit::tip(text));
    }

    let failed = thread
        .as_ref()
        .zip(d.state.failure.as_ref())
        .filter(|(t, f)| *t == &f.thread && variant != Variant::C)
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

/// The "…" at a row's right end: A's only control, B's last.
fn more_button(theme: &Theme, d: &Draft<'_>, row: &ThreadRow, thread: &ThreadId) -> Stateful<Div> {
    let (act, thread) = (Rc::clone(&d.act), thread.clone());
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
                thread.clone(),
                Some(gpui::point(at.x - px(8.0), at.y + px(12.0))),
            ),
            window,
            cx,
        );
    })
}

/// "Show 6 more" / "Show less" under a project's threads.
pub(crate) fn folder_extras(theme: &Theme, d: &Draft<'_>, folder: &Folder) -> Option<AnyElement> {
    let label = draft32::more_label(folder.hidden, folder.expanded)?;
    let slug = folder.name.replace(' ', "-");
    let (act, key) = (Rc::clone(&d.act), folder.key.clone());
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

/// A's "Archived \u{b7} 4" row, and the archive under it when it is open.
fn archived_row(
    theme: &Theme,
    d: &Draft<'_>,
    selected: Option<&RunId>,
    handlers: &SidebarHandlers,
) -> Option<Div> {
    let count = d.listing.archived.len();
    if count == 0 || !d.edit {
        return None;
    }
    let open = d.state.archived_open;
    let act = Rc::clone(&d.act);
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
        .child(draft32::archived_label(count))
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
                    d.listing
                        .archived
                        .iter()
                        .map(|r| row(theme, d, r, selected, handlers, true)),
                )
            }),
    )
}

/// C's strip of pinned threads: small chips, each opening its thread.
fn chips(
    theme: &Theme,
    d: &Draft<'_>,
    selected: Option<&RunId>,
    handlers: &SidebarHandlers,
) -> Option<Stateful<Div>> {
    if d.listing.pinned.is_empty() {
        return None;
    }
    Some(
        div()
            .id("pinned-strip")
            .debug_selector(|| "pinned-strip".to_owned())
            .pt(px(12.0))
            .px(px(6.0))
            .flex()
            .flex_wrap()
            .gap(px(4.0))
            .children(d.listing.pinned.iter().map(|r| {
                let open = Rc::clone(&handlers.recent_run);
                let run = r.run.clone();
                let title = crate::summary::parse_title(&r.title).what.to_owned();
                let on = selected.is_some_and(|s| r.members.contains(s));
                let raised = theme.raised;
                let mut chip = div()
                    .id(SharedString::from(format!("chip-{}", r.run)))
                    .debug_selector({
                        let run = r.run.clone();
                        move || format!("chip-{run}")
                    })
                    .h(px(24.0))
                    .max_w(px(102.0))
                    .px(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .rounded(px(12.0))
                    .border_1()
                    .border_color(theme.hairline)
                    .text_size(px(12.0))
                    .text_color(if on { theme.text } else { theme.muted })
                    .cursor_pointer()
                    .role(Role::Button)
                    .aria_label(SharedString::from(title.clone()))
                    .when(on, |c| c.bg(theme.row_selected()))
                    .when(!on, |c| c.hover(move |s| s.bg(raised)))
                    .child(icon(Icon::Pin, 11.0, theme.faint))
                    .child(div().min_w_0().truncate().child(title))
                    .on_click(move |_, window, cx| open(run.clone(), window, cx));
                if let Some(t) = &r.thread {
                    let (act, th) = (Rc::clone(&d.act), t.clone());
                    chip = chip.on_hover(move |on, window, cx| {
                        act(Act::Hover(th.clone(), *on), window, cx);
                    });
                    let (act, th) = (Rc::clone(&d.act), t.clone());
                    chip = chip.on_mouse_down(MouseButton::Right, move |_, window, cx| {
                        cx.stop_propagation();
                        act(Act::Card(th.clone()), window, cx);
                    });
                    chip = chip.child(track(d, t));
                }
                chip
            })),
    )
}

/// B's header: "Threads \u{25be}" with the filter menu, and what the archive holds in words.
fn filter_header(theme: &Theme, d: &Draft<'_>) -> Div {
    let archived = d.state.filter_archived;
    let act = Rc::clone(&d.act);
    let count = d.listing.archived.len();
    let hover = theme.muted;
    div()
        .px(px(10.0))
        .pt(px(16.0))
        .pb(px(6.0))
        .flex()
        .items_center()
        .justify_between()
        .child(
            div()
                .id("draft32-filter")
                .debug_selector(|| "draft32-filter".to_owned())
                .flex()
                .items_center()
                .gap(px(4.0))
                .text_size(px(11.5))
                .text_color(theme.faint)
                .cursor_pointer()
                .role(Role::Button)
                .hover(move |s| s.text_color(hover))
                .child(if archived {
                    draft32::archived_label(count)
                } else {
                    "Threads".to_owned()
                })
                .child(icon(Icon::ChevDown, 10.0, theme.faint))
                .on_click(move |event, window, cx| {
                    let at = event.position();
                    act(
                        Act::FilterMenu(Some(gpui::point(at.x - px(8.0), at.y + px(14.0)))),
                        window,
                        cx,
                    );
                }),
        )
        .when(d.edit && count > 0 && !archived, |h| {
            h.child(
                div()
                    .id("archived-count")
                    .debug_selector(|| "archived-count".to_owned())
                    .text_size(px(11.5))
                    .text_color(theme.faint)
                    .child(format!("{count} archived")),
            )
        })
}

/// Everything under "Start a task" down to the footer: the variant's list.
pub(crate) fn section(
    theme: &Theme,
    d: &Draft<'_>,
    selected: Option<&RunId>,
    handlers: &SidebarHandlers,
    folder_view: impl Fn(&Folder) -> Div,
) -> Div {
    let mut col = div().flex().flex_col();
    match d.variant {
        Variant::A => {
            if !d.listing.pinned.is_empty() {
                col = col.child(header(theme, "Pinned")).child(
                    div()
                        .id("pinned-section")
                        .debug_selector(|| "pinned-section".to_owned())
                        .flex()
                        .flex_col()
                        .gap(px(1.0))
                        .children(
                            d.listing
                                .pinned
                                .iter()
                                .map(|r| row(theme, d, r, selected, handlers, false)),
                        ),
                );
            }
            col = col.child(header(theme, "Threads"));
        }
        Variant::B => {
            col = col.child(filter_header(theme, d));
        }
        Variant::C => {
            col = col
                .children(chips(theme, d, selected, handlers))
                .child(header(theme, "Threads"));
        }
    }
    let list = if d.variant == Variant::B && d.state.filter_archived {
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .pb(px(10.0))
            .children(
                d.listing
                    .archived
                    .iter()
                    .map(|r| row(theme, d, r, selected, handlers, true)),
            )
            .when(d.listing.archived.is_empty(), |l| {
                l.child(
                    div()
                        .px(px(10.0))
                        .py(px(8.0))
                        .text_size(px(12.5))
                        .text_color(theme.faint)
                        .child("Nothing is archived"),
                )
            })
    } else {
        div()
            .flex()
            .flex_col()
            .gap(px(2.0))
            .pb(px(10.0))
            .children(d.listing.folders.iter().map(&folder_view))
            .when(d.listing.folders.is_empty(), |list| {
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
            .when(d.variant == Variant::A, |l| {
                l.children(archived_row(theme, d, selected, handlers))
            })
    };
    col.child(list)
}

/// The row of a folder in the draft's shape.
pub(crate) fn folder_row(
    theme: &Theme,
    d: &Draft<'_>,
    r: &ThreadRow,
    selected: Option<&RunId>,
    handlers: &SidebarHandlers,
) -> AnyElement {
    row(theme, d, r, selected, handlers, false)
}

/// The width of the sidebar's drag handle, painted on its right edge.
pub(crate) fn resize_handle(theme: &Theme, active: bool, act: &ActHandler) -> Stateful<Div> {
    let accent = theme.accent;
    let _ = act;
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
