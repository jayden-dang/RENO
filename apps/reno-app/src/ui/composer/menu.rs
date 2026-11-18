//! The menus a composer opens: the popover that anchors them and the panel and row they share.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    Anchor, Bounds, Context, MouseDownEvent, Pixels, SharedString, anchored, deferred, div, point,
    prelude::*, px,
};

use super::{Composer, Mode};
use crate::ui::icon::{Icon, icon};
use crate::ui::theme::Theme;

impl Composer {
    /// An open menu, attached to the element whose bounds are in `at`: below it on Home (the
    /// composer sits high), above it on a run; it flips when it would leave the window. A click
    /// outside it, but not on its own trigger (which toggles), closes it.
    pub(super) fn popover(
        &self,
        at: &Rc<Cell<Option<Bounds<Pixels>>>>,
        content: gpui::Stateful<gpui::Div>,
        close: fn(&mut Self, &mut Context<Self>),
        cx: &mut Context<Self>,
    ) -> Option<gpui::Deferred> {
        let bounds = at.get()?;
        let below = self.mode == Mode::Home;
        let (corner, anchor, dy) = if below {
            (Anchor::BottomRight, Anchor::TopRight, 6.0)
        } else {
            (Anchor::TopRight, Anchor::BottomRight, -6.0)
        };
        // A menu hangs from the side its trigger sits on: the class button and a run's chip are
        // at the left of the foot, Home's chip at the right.
        let left = Rc::ptr_eq(at, &self.class_bounds) || matches!(self.mode, Mode::Steer(_));
        let (corner, anchor) = if left {
            (
                if below {
                    Anchor::BottomLeft
                } else {
                    Anchor::TopLeft
                },
                if below {
                    Anchor::TopLeft
                } else {
                    Anchor::BottomLeft
                },
            )
        } else {
            (corner, anchor)
        };
        let trigger = at.clone();
        Some(
            deferred(
                anchored()
                    .attach(bounds, corner)
                    .anchor(anchor)
                    .offset(point(px(0.0), px(dy)))
                    .snap_to_window_with_margin(px(8.0))
                    .child(content.occlude().on_mouse_down_out(cx.listener(
                        move |this, event: &MouseDownEvent, _, cx| {
                            if trigger.get().is_some_and(|b| b.contains(&event.position)) {
                                return;
                            }
                            close(this, cx);
                        },
                    ))),
            )
            .with_priority(1),
        )
    }
}

/// A menu's panel: the surface every popover shares.
pub(super) fn menu(theme: &Theme, id: &'static str, width: f32) -> gpui::Stateful<gpui::Div> {
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

/// One row of a menu; a disabled one is faint and takes no hover.
pub(super) fn menu_row(theme: &Theme, id: String, enabled: bool) -> gpui::Stateful<gpui::Div> {
    let hover = theme.row_selected();
    let selector = id.clone();
    div()
        .id(SharedString::from(id))
        .debug_selector(move || selector.clone())
        .w_full()
        .min_h(px(30.0))
        .px(px(10.0))
        .flex()
        .items_center()
        .gap(px(8.0))
        .rounded(px(7.0))
        .when(enabled, |r| r.cursor_pointer().hover(move |s| s.bg(hover)))
        .when(!enabled, |r| r.text_color(theme.faint))
}

/// The chosen row's check mark.
pub(super) fn check(theme: &Theme) -> gpui::Svg {
    icon(Icon::Check, 14.0, theme.accent)
}
