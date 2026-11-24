//! The context menu of the kit: a panel of 30 pt rows with an optional note and a right-aligned
//! key hint, a danger tone, and the popup that hangs it from a point. The sidebar's thread menu is
//! this; anything else that needs a menu hangs off the same pieces.

use std::rc::Rc;

use gpui::{
    Anchor, AnyElement, App, Bounds, Div, MouseDownEvent, Pixels, Point, Role, SharedString,
    Stateful, Window, anchored, deferred, div, prelude::*, px,
};

use super::kit;
use super::theme::Theme;

/// What a popup calls when a click lands outside it.
pub(crate) type Dismiss = Rc<dyn Fn(&mut Window, &mut App)>;

pub(crate) const CONTEXT: &str = "PopupMenu";

pub(crate) fn visible_target(bounds: Bounds<Pixels>, window: &Window) -> Option<Bounds<Pixels>> {
    let visible = bounds.intersect(&window.content_mask().bounds);
    (visible.size.width > px(0.0) && visible.size.height > px(0.0)).then_some(visible)
}

#[derive(Default)]
pub(crate) struct MenuState {
    pub(crate) highlighted: Option<usize>,
}

impl MenuState {
    pub(crate) fn step(&mut self, len: usize, forward: bool) {
        if len == 0 {
            self.highlighted = None;
            return;
        }
        let current = self.highlighted.filter(|&ix| ix < len);
        self.highlighted = Some(match (current, forward) {
            (Some(ix), true) => (ix + 1) % len,
            (Some(ix), false) => ix.checked_sub(1).unwrap_or(len - 1),
            (None, true) => 0,
            (None, false) => len - 1,
        });
    }
}

/// The panel every popup sits on.
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
/// calls `dismiss`.
pub(crate) fn popup(at: Point<Pixels>, content: Stateful<Div>, dismiss: &Dismiss) -> AnyElement {
    let dismiss = Rc::clone(dismiss);
    deferred(
        anchored()
            .position(at)
            .anchor(Anchor::TopLeft)
            .snap_to_window_with_margin(px(8.0))
            .child(
                content
                    .occlude()
                    .on_mouse_down_out(move |_: &MouseDownEvent, window, cx| dismiss(window, cx)),
            ),
    )
    .with_priority(1)
    .into_any_element()
}

/// A hairline between groups of rows.
pub(crate) fn divider(theme: &Theme) -> Div {
    div().my(px(4.0)).mx(px(4.0)).h(px(1.0)).bg(theme.hairline)
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
