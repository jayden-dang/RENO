//! The popups of the draft (the row menu, B's filter menu, the close confirm), C's details card,
//! and C's archived page. All of them hang from the root view, so nothing is clipped by the
//! sidebar and nothing lands on the status bar.

use std::rc::Rc;

use gpui::{
    AnyElement, Div, FontWeight, MouseDownEvent, SharedString, Stateful, Window, div, point,
    prelude::*, px,
};

use super::super::icon::{Icon, icon};
use super::super::kit::{self, Tone};
use super::super::theme::Theme;
use super::list::Draft;
use super::menu;
use super::state::{Act, CardState, Confirm, State};
use crate::vm::changes::Loaded;
use crate::vm::draft32::{self, Item, Variant};
use crate::vm::threads;

/// The top of the area a popup may use: under the status bar.
const TOP: f32 = 48.0;

/// Every popup that is open, ready to hang from the root view.
pub(crate) fn overlays(theme: &Theme, d: &Draft<'_>, window: &Window) -> Vec<AnyElement> {
    let mut out = Vec::new();
    let st = d.state;
    if let Some(menu_state) = &st.menu {
        let facts = State::facts(d.store, &menu_state.thread);
        let entries = if menu_state.copy {
            draft32::copy_entries(facts)
        } else {
            draft32::menu(d.variant, facts)
        };
        if !entries.is_empty() {
            out.push(menu::popup(
                menu_state.at,
                menu::thread_menu(theme, &menu_state.thread, &entries, menu_state.copy, &d.act),
                &d.act,
            ));
        }
    }
    if let Some(at) = st.filter_menu {
        out.push(menu::popup(at, filter_menu(theme, d), &d.act));
    }
    if let Some(confirm) = &st.confirm
        && d.variant != Variant::C
    {
        out.push(menu::popup(
            confirm.at,
            confirm_panel(theme, d, confirm, "draft32-confirm", 288.0),
            &d.act,
        ));
    }
    if let Some(card) = &st.card
        && d.variant == Variant::C
        && let Some(popup) = card_popup(theme, d, card, window)
    {
        out.push(popup);
    }
    out
}

fn filter_menu(theme: &Theme, d: &Draft<'_>) -> Stateful<Div> {
    let archived = d.state.filter_archived;
    let mut menu = menu::panel(theme, "draft32-filter-menu", 196.0);
    for (id, label, on, to) in [
        (
            "draft32-filter-active",
            "Active".to_owned(),
            !archived,
            false,
        ),
        (
            "draft32-filter-archived",
            draft32::archived_label(d.listing.archived.len()),
            archived,
            true,
        ),
    ] {
        let act = Rc::clone(&d.act);
        menu = menu.child(
            menu::row(theme, id.to_owned(), &label, None, None, true, false)
                .child(
                    div()
                        .w(px(14.0))
                        .children(on.then(|| icon(Icon::Check, 14.0, theme.accent))),
                )
                .on_click(move |_, window, cx| act(Act::Filter(to), window, cx)),
        );
    }
    menu
}

/// The question and its two buttons, on whatever surface asks it (A and B: a small popup, C: the
/// card). It is always asked; the words come from `vm::threads::close_question`.
fn confirm_body(theme: &Theme, d: &Draft<'_>, confirm: &Confirm) -> Div {
    let store = d.store;
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
    let (ok, cancel) = (Rc::clone(&d.act), Rc::clone(&d.act));
    let mut confirm_button = kit::line_button(
        theme,
        "draft32-close-confirm",
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
                .id("draft32-close-question")
                .debug_selector(|| "draft32-close-question".to_owned())
                .text_size(px(12.5))
                .line_height(px(18.0))
                .text_color(theme.muted)
                .child(question),
        )
        .child(
            div().flex().gap(px(8.0)).child(confirm_button).child(
                kit::line_button(
                    theme,
                    "draft32-close-cancel",
                    None,
                    if busy { "OK" } else { "Cancel" },
                    Tone::Quiet,
                    true,
                )
                .on_click(move |_, window, cx| cancel(Act::Dismiss, window, cx)),
            ),
        )
}

fn confirm_panel(
    theme: &Theme,
    d: &Draft<'_>,
    confirm: &Confirm,
    id: &'static str,
    width: f32,
) -> Stateful<Div> {
    menu::panel(theme, id, width)
        .p(px(12.0))
        .child(confirm_body(theme, d, confirm))
}

/// C's details card, beside the sidebar, level with the row that opened it.
fn card_popup(
    theme: &Theme,
    d: &Draft<'_>,
    card: &CardState,
    window: &Window,
) -> Option<AnyElement> {
    let row = d.row_of(&card.thread)?;
    let store = d.store;
    let info = store.state().thread(&card.thread);
    let facts = State::facts(store, &card.thread);
    let act = &d.act;
    let renaming = d.state.rename_of(&card.thread, true).cloned();
    let confirm = d.state.confirm.as_ref().filter(|c| c.thread == card.thread);
    let title = crate::summary::parse_title(&row.title).what.to_owned();

    let title_el = match renaming {
        Some(field) => div()
            .h(px(26.0))
            .px(px(8.0))
            .flex()
            .items_center()
            .rounded(px(6.0))
            .border_1()
            .border_color(theme.accent)
            .bg(theme.canvas)
            .text_size(px(14.0))
            .font_weight(FontWeight::SEMIBOLD)
            .child(field)
            .into_any_element(),
        None => div()
            .id("draft32-card-title")
            .debug_selector(|| "draft32-card-title".to_owned())
            .text_size(px(14.0))
            .line_height(px(19.0))
            .font_weight(FontWeight::SEMIBOLD)
            .child(title)
            .into_any_element(),
    };

    let meta = draft32::details(row, info, store.now_ms());
    let meta_rows = meta.into_iter().map(|(label, value)| {
        div()
            .flex()
            .items_center()
            .gap(px(10.0))
            .h(px(20.0))
            .child(
                div()
                    .flex_none()
                    .w(px(88.0))
                    .text_size(px(12.0))
                    .text_color(theme.faint)
                    .child(label),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(kit::MONO)
                    .text_size(px(12.0))
                    .child(value),
            )
    });

    let failure = d
        .state
        .failure
        .as_ref()
        .filter(|f| f.thread == card.thread)
        .map(|f| {
            div()
                .id("draft32-card-failure")
                .debug_selector(|| "draft32-card-failure".to_owned())
                .text_size(px(12.0))
                .text_color(theme.danger)
                .child(f.message.clone())
        });

    let button = |id: &'static str, label: &'static str, tone: Tone, enabled: bool, item: Item| {
        let (act, thread) = (Rc::clone(act), card.thread.clone());
        let mut b = kit::line_button(theme, id, None, label, tone, enabled);
        if enabled {
            b = b.on_click(move |_, window, cx| act(Act::Do(thread.clone(), item), window, cx));
        }
        b
    };
    let actions = match confirm {
        Some(confirm) => confirm_body(theme, d, confirm).into_any_element(),
        None => div()
            .flex()
            .flex_wrap()
            .gap(px(6.0))
            .when(facts.edit, |row| {
                row.child(button(
                    "draft32-card-rename",
                    "Rename",
                    Tone::Quiet,
                    true,
                    Item::Rename,
                ))
                .child(if facts.pinned {
                    button(
                        "draft32-card-pin",
                        "Unpin",
                        Tone::Primary,
                        true,
                        Item::Unpin,
                    )
                } else {
                    button("draft32-card-pin", "Pin", Tone::Primary, true, Item::Pin)
                })
                .child(button(
                    "draft32-card-archive",
                    "Archive",
                    Tone::Quiet,
                    true,
                    Item::Archive,
                ))
            })
            .child(button(
                "draft32-card-copy-path",
                "Copy path",
                Tone::Quiet,
                facts.has_path,
                Item::CopyPath,
            ))
            .child(button(
                "draft32-card-close",
                "Close thread\u{2026}",
                Tone::Danger,
                true,
                Item::Close,
            ))
            .into_any_element(),
    };

    let hover = Rc::clone(act);
    let dismiss = Rc::clone(act);
    let content = menu::panel(theme, "draft32-card", 332.0)
        .p(px(14.0))
        .gap(px(10.0))
        .on_hover(move |on, window, cx| hover(Act::CardHover(*on), window, cx))
        .child(title_el)
        .child(div().flex().flex_col().children(meta_rows))
        .children(failure)
        .child(actions);
    // Beside the sidebar, level with the pointer, and never up under the status bar.
    let x = px(d.state.width + 8.0);
    let y = (card.y - px(24.0)).max(px(TOP));
    let _ = window;
    let popup =
        gpui::deferred(
            gpui::anchored()
                .position(point(x, y))
                .anchor(gpui::Anchor::TopLeft)
                .snap_to_window_with_margin(px(8.0))
                .child(content.occlude().on_mouse_down_out(
                    move |_: &MouseDownEvent, window, cx| {
                        dismiss(Act::Dismiss, window, cx);
                    },
                )),
        )
        .with_priority(1);
    Some(popup.into_any_element())
}

/// C's archived page: what is archived, with Open and Unarchive, in the canvas.
pub(crate) fn archived_page(
    theme: &Theme,
    d: &Draft<'_>,
    open: &super::super::sidebar::RunHandler,
) -> AnyElement {
    let rows = &d.listing.archived;
    let back = Rc::clone(&d.act);
    let list = div()
        .id("archived-page-list")
        .flex()
        .flex_col()
        .gap(px(2.0))
        .children(rows.iter().map(|r| {
            let (open, run) = (Rc::clone(open), r.run.clone());
            let unarchive = Rc::clone(&d.act);
            let thread = r.thread.clone();
            let id = format!("archived-page-{}", r.run);
            div()
                .id(SharedString::from(id.clone()))
                .debug_selector(move || id.clone())
                .h(px(44.0))
                .px(px(12.0))
                .flex()
                .items_center()
                .gap(px(12.0))
                .rounded(px(9.0))
                .hover({
                    let hover = theme.row_selected().opacity(0.6);
                    move |s| s.bg(hover)
                })
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .flex_col()
                        .child(
                            div()
                                .truncate()
                                .text_size(px(13.5))
                                .child(crate::summary::parse_title(&r.title).what.to_owned()),
                        )
                        .child(
                            div()
                                .font_family(kit::MONO)
                                .text_size(px(11.5))
                                .text_color(theme.faint)
                                .child(format!("{} \u{b7} {} ago", r.project, r.age)),
                        ),
                )
                .child(
                    kit::line_button(
                        theme,
                        SharedString::from(format!("archived-open-{}", r.run)),
                        None,
                        "Open",
                        Tone::Quiet,
                        true,
                    )
                    .on_click(move |_, window, cx| open(run.clone(), window, cx)),
                )
                .child(
                    kit::line_button(
                        theme,
                        SharedString::from(format!("archived-unarchive-{}", r.run)),
                        None,
                        "Unarchive",
                        Tone::Quiet,
                        true,
                    )
                    .on_click(move |_, window, cx| {
                        if let Some(t) = &thread {
                            unarchive(Act::Unarchive(t.clone()), window, cx);
                        }
                    }),
                )
        }));
    div()
        .id("archived-page")
        .debug_selector(|| "archived-page".to_owned())
        .size_full()
        .overflow_y_scroll()
        .child(
            div()
                .w_full()
                .max_w(px(680.0))
                .mx_auto()
                .px(px(24.0))
                .py(px(28.0))
                .flex()
                .flex_col()
                .gap(px(14.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .text_size(px(12.0))
                        .text_color(theme.faint)
                        .child(
                            kit::line_button(
                                theme,
                                "archived-page-back",
                                None,
                                "Back",
                                Tone::Quiet,
                                true,
                            )
                            .on_click(move |_, window, cx| back(Act::Page(false), window, cx)),
                        )
                        .child("Settings")
                        .child(icon(Icon::PhCaretRight, 11.0, theme.faint))
                        .child("Archived"),
                )
                .child(
                    div()
                        .text_size(px(20.0))
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(draft32::archived_label(rows.len())),
                )
                .child(
                    div().text_size(px(13.0)).text_color(theme.muted).child(
                        "Archived threads leave the sidebar. Unarchive one to bring it back.",
                    ),
                )
                .child(list)
                .when(rows.is_empty(), |p| {
                    p.child(
                        div()
                            .text_size(px(13.0))
                            .text_color(theme.faint)
                            .child("Nothing is archived."),
                    )
                }),
        )
        .into_any_element()
}
