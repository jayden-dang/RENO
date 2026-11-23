//! A vertical scrollbar for a GPUI `list`, which has none of its own.

use std::cell::Cell;
use std::rc::Rc;

use gpui::{
    App, DragMoveEvent, ListOffset, ListState, MouseButton, Render, RenderOnce, Window, div, point,
    prelude::*, px,
};

use super::theme::Theme;

/// How wide the scrollbar's track is; the transcript's gutter leaves this much room for it.
pub(crate) const WIDTH: f32 = 6.0;
/// The shortest thumb, so a very long transcript still has something to take hold of.
const GRIP: f32 = 24.0;
const THUMB_WIDTH: f32 = 3.0;

/// Where the thumb rests on a track as long as the view (`viewport`) and how long it is, for a
/// view scrolled `offset` into `content`; both measured along the track in points. Content that
/// fits in the view has a thumb that fills the track.
pub(crate) fn thumb_geometry(viewport: f32, content: f32, offset: f32) -> (f32, f32) {
    if viewport <= 0.0 || content <= viewport {
        return (0.0, viewport.max(0.0));
    }
    let len = (viewport * viewport / content).clamp(GRIP.min(viewport), viewport);
    let share = (offset / (content - viewport)).clamp(0.0, 1.0);
    (share * (viewport - len), len)
}

/// The scroll offset that puts the thumb's top `top` points down the track: the inverse of
/// [`thumb_geometry`], clamped to the ends.
pub(crate) fn offset_for_thumb_top(viewport: f32, content: f32, top: f32) -> f32 {
    let (_, len) = thumb_geometry(viewport, content, 0.0);
    let travel = viewport - len;
    if travel <= 0.0 {
        return 0.0;
    }
    (top / travel).clamp(0.0, 1.0) * (content - viewport)
}

/// The scrollbar of `list`: a track with a thumb the owner drags. It draws nothing while the list
/// fits its view.
pub(crate) fn scrollbar(list: &ListState, theme: &Theme) -> Scrollbar {
    Scrollbar {
        list: list.clone(),
        thumb: theme.raised,
        hovered: theme.faint,
        moved: None,
    }
}

type Moved = Rc<dyn Fn(&mut Window, &mut App)>;

#[derive(IntoElement)]
pub(crate) struct Scrollbar {
    list: ListState,
    thumb: gpui::Hsla,
    hovered: gpui::Hsla,
    moved: Option<Moved>,
}

impl Scrollbar {
    /// Runs after a drag moved the list, and once more when it ends: the list moved without a
    /// wheel event, so whoever keeps where it rests needs to know.
    pub(crate) fn on_moved(mut self, moved: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.moved = Some(Rc::new(moved));
        self
    }
}

/// What a drag carries: how far down the thumb the pointer took hold. Set when the drag starts.
struct ThumbDrag {
    grab: Cell<f32>,
}

/// The drag's preview, which is nothing: the thumb itself follows the pointer.
struct Ghost;

impl Render for Ghost {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        div()
    }
}

fn end_drag(list: &ListState, moved: Option<&Moved>, window: &mut Window, cx: &mut App) {
    if list.is_scrollbar_dragging() {
        list.scrollbar_drag_ended();
        if let Some(moved) = moved {
            moved(window, cx);
        }
    }
}

impl RenderOnce for Scrollbar {
    fn render(self, _: &mut Window, _: &mut App) -> impl IntoElement {
        let Self {
            list,
            thumb,
            hovered,
            moved,
        } = self;
        let viewport = f32::from(list.viewport_bounds().size.height);
        let reach = f32::from(list.max_offset_for_scrollbar().y);
        if reach <= 0.0 || viewport <= 0.0 {
            return div().into_any_element();
        }
        let content = viewport + reach;
        let offset = (-f32::from(list.scroll_px_offset_for_scrollbar().y)).clamp(0.0, reach);
        let (top, len) = thumb_geometry(viewport, content, offset);
        let dragging = list.is_scrollbar_dragging();
        let drag_list = list.clone();
        let move_list = list.clone();
        let move_moved = moved.clone();
        let (up_list, up_moved) = (list.clone(), moved.clone());
        let (out_list, out_moved) = (list, moved);
        div()
            .id("transcript-scrollbar")
            .debug_selector(|| "transcript-scrollbar".to_owned())
            .group("transcript-scrollbar")
            .relative()
            .w(px(WIDTH))
            .h_full()
            .on_drag(
                ThumbDrag {
                    grab: Cell::new(0.0),
                },
                move |drag, at, _, cx| {
                    let y = f32::from(at.y);
                    let on_thumb = y >= top && y <= top + len;
                    drag.grab.set(if on_thumb { y - top } else { len / 2.0 });
                    drag_list.scrollbar_drag_started();
                    cx.new(|_| Ghost)
                },
            )
            .on_drag_move(move |e: &DragMoveEvent<ThumbDrag>, window, cx| {
                let viewport = f32::from(move_list.viewport_bounds().size.height);
                let content = viewport + f32::from(move_list.max_offset_for_scrollbar().y);
                let top = f32::from(e.event.position.y - e.bounds.top()) - e.drag(cx).grab.get();
                let to = offset_for_thumb_top(viewport, content, top);
                if to <= 0.0 {
                    // Rows not measured yet count for no height, so a pixel offset of 0 could
                    // land past them.
                    move_list.scroll_to(ListOffset::default());
                } else {
                    move_list.set_offset_from_scrollbar(point(px(0.0), px(-to)));
                }
                window.refresh();
                if let Some(moved) = &move_moved {
                    moved(window, cx);
                }
            })
            .on_mouse_up(MouseButton::Left, move |_, window, cx| {
                end_drag(&up_list, up_moved.as_ref(), window, cx);
            })
            .on_mouse_up_out(MouseButton::Left, move |_, window, cx| {
                end_drag(&out_list, out_moved.as_ref(), window, cx);
            })
            .child(
                div()
                    .id("transcript-scrollbar-thumb")
                    .debug_selector(|| "transcript-scrollbar-thumb".to_owned())
                    .absolute()
                    .top(px(top))
                    .left(px((WIDTH - THUMB_WIDTH) / 2.0))
                    .w(px(THUMB_WIDTH))
                    .h(px(len))
                    .rounded(px(THUMB_WIDTH / 2.0))
                    .bg(if dragging { hovered } else { thumb })
                    .group_hover("transcript-scrollbar", move |s| s.bg(hovered)),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::{offset_for_thumb_top, thumb_geometry};

    #[test]
    fn thumb_geometry_tracks_the_offset_and_the_content() {
        // A 400 pt view of 1600 pt: the thumb is a quarter of the track.
        assert_eq!(thumb_geometry(400.0, 1600.0, 0.0), (0.0, 100.0));
        assert_eq!(thumb_geometry(400.0, 1600.0, 600.0), (150.0, 100.0));
        assert_eq!(thumb_geometry(400.0, 1600.0, 1200.0), (300.0, 100.0));
        // More content, a shorter thumb; the thumb never shrinks below a grip.
        assert_eq!(thumb_geometry(400.0, 3200.0, 0.0).1, 50.0);
        assert_eq!(thumb_geometry(400.0, 400_000.0, 0.0).1, 24.0);
        // An offset past the end rests at the end.
        assert_eq!(thumb_geometry(400.0, 1600.0, 5000.0), (300.0, 100.0));
        assert_eq!(thumb_geometry(400.0, 1600.0, -20.0), (0.0, 100.0));
        // Content that fits fills the track.
        assert_eq!(thumb_geometry(400.0, 400.0, 0.0), (0.0, 400.0));
        assert_eq!(thumb_geometry(400.0, 120.0, 0.0), (0.0, 400.0));
        assert_eq!(thumb_geometry(0.0, 0.0, 0.0), (0.0, 0.0));
    }

    #[test]
    fn a_thumb_position_maps_back_to_the_offset_that_puts_it_there() {
        for offset in [0.0, 250.0, 600.0, 1200.0] {
            let (top, _) = thumb_geometry(400.0, 1600.0, offset);
            let back = offset_for_thumb_top(400.0, 1600.0, top);
            assert!((back - offset).abs() < 0.01, "{offset} came back {back}");
        }
        assert_eq!(offset_for_thumb_top(400.0, 1600.0, -30.0), 0.0);
        assert_eq!(offset_for_thumb_top(400.0, 1600.0, 900.0), 1200.0);
        assert_eq!(offset_for_thumb_top(400.0, 400.0, 10.0), 0.0);
    }
}
