//! The ONE menu component of the draft: a panel of 30 pt rows with an optional note and a
//! right-aligned key hint, a danger tone, and the popup that hangs it from a point. A's row menu,
//! B's "…" menu and B's filter menu are all this; C's card is not a menu.

use std::rc::Rc;

use gpui::{
    Anchor, AnyElement, App, Div, FontWeight, MouseDownEvent, Pixels, Point, Role, SharedString,
    Stateful, Window, anchored, deferred, div, prelude::*, px,
};
use reno_proto::ThreadId;

use super::super::icon::{Icon, icon};
use super::super::kit;
use super::super::theme::Theme;
use super::state::Act;
use crate::vm::draft32::{Entry, Item};

pub(crate) type ActHandler = Rc<dyn Fn(Act, &mut Window, &mut App)>;

/// The panel every popup of the draft sits on.
pub(crate) fn panel(theme: &Theme, id: &'static str, width: f32) -> Stateful<Div> {
    div()
        .id(id)
        .debug_selector(move || id.to_owned())
        .w(px(width))
        .p(px(6.0))
        .flex()
        .flex_col()
        .rounded(px(12.0))
        .bg(theme.surface)
        .border_1()
        .border_color(theme.hairline)
        .shadow_lg()
        .text_size(px(13.0))
        .text_color(theme.text)
}

/// Hangs `content` from `at` (its top-left corner), kept inside the window. A click outside it
/// dismisses what is open.
pub(crate) fn popup(at: Point<Pixels>, content: Stateful<Div>, act: &ActHandler) -> AnyElement {
    let act = Rc::clone(act);
    deferred(
        anchored()
            .position(at)
            .anchor(Anchor::TopLeft)
            .snap_to_window_with_margin(px(8.0))
            .child(
                content
                    .occlude()
                    .on_mouse_down_out(move |_: &MouseDownEvent, window, cx| {
                        act(Act::Dismiss, window, cx);
                    }),
            ),
    )
    .with_priority(1)
    .into_any_element()
}

/// One 30 pt row: label, a note after it, a hint at the right. `on_click` is added when enabled.
pub(crate) fn row(
    theme: &Theme,
    id: String,
    label: &str,
    note: Option<&str>,
    hint: Option<&str>,
    enabled: bool,
    danger: bool,
) -> Stateful<Div> {
    let hover = theme.row_selected();
    let selector = id.clone();
    let ink = if !enabled {
        theme.faint
    } else if danger {
        theme.danger
    } else {
        theme.text
    };
    div()
        .id(SharedString::from(id))
        .debug_selector(move || selector.clone())
        .w_full()
        .h(px(30.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .rounded(px(7.0))
        .text_color(ink)
        .role(Role::Button)
        .aria_label(SharedString::from(label.to_owned()))
        .when(enabled, |r| r.cursor_pointer().hover(move |s| s.bg(hover)))
        .child(
            div()
                .flex_none()
                .whitespace_nowrap()
                .child(SharedString::from(label.to_owned())),
        )
        .children(note.map(|note| {
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(11.5))
                .text_color(theme.faint)
                .child(SharedString::from(note.to_owned()))
        }))
        .when(note.is_none(), |r| r.child(div().flex_1()))
        .children(hint.map(|hint| {
            div()
                .flex_none()
                .font_family(kit::MONO)
                .text_size(px(11.5))
                .text_color(theme.faint)
                .child(SharedString::from(hint.to_owned()))
        }))
}

/// The menu of a thread: `entries` as rows. Copy opens its sub-list in place (`copy`), with a
/// way back as its first row.
pub(crate) fn thread_menu(
    theme: &Theme,
    thread: &ThreadId,
    entries: &[Entry],
    copy: bool,
    act: &ActHandler,
) -> Stateful<Div> {
    let mut menu = panel(theme, "draft32-menu", 232.0);
    if copy {
        let back = Rc::clone(act);
        menu = menu.child(
            row(
                theme,
                "draft32-menu-back".to_owned(),
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
            Entry::Divider => {
                menu = menu.child(div().my(px(4.0)).mx(px(4.0)).h(px(1.0)).bg(theme.hairline));
            }
            Entry::Item {
                item,
                label,
                hint,
                enabled,
                note,
                danger,
            } => {
                let id = format!("draft32-menu-{}", label.to_lowercase().replace(' ', "-"));
                let is_copy = *item == Item::Copy;
                let mut line = row(theme, id, label, *note, *hint, *enabled, *danger);
                if is_copy {
                    line = line.child(icon(Icon::PhCaretRight, 12.0, theme.faint));
                }
                if *enabled {
                    let (act, thread, item) = (Rc::clone(act), thread.clone(), *item);
                    line = line.on_click(move |_, window, cx| {
                        cx.stop_propagation();
                        act(Act::Do(thread.clone(), item), window, cx);
                    });
                }
                menu = menu.child(line);
            }
        }
    }
    menu
}
