//! Two small elements the renderers need and GPUI has no div for.
//!
//! - [`Picture`]: a decoded image painted into its box, scaled down to the column (never up),
//!   keeping its aspect ratio. Used for display math and diagrams, which are already pixels.
//! - [`Overlays`]: pictures laid over reserved gaps in a paragraph's text. Inline math reserves
//!   its width with non-breaking spaces in the text run; once the text is laid out this finds the
//!   gap and paints the formula in it, its baseline on the line's baseline.

use std::ops::Range;
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, LayoutId,
    Pixels, Style, TextLayout, Window, point, px, relative, size,
};

use super::math::Typeset;

/// An image painted into its box.
#[derive(Clone)]
pub(crate) struct Picture {
    pub(crate) image: Arc<gpui::RenderImage>,
    /// Natural size in logical pixels: the widest the box gets.
    pub(crate) width: f32,
    pub(crate) height: f32,
}

impl Element for Picture {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.max_size.width = px(self.width).into();
        style.aspect_ratio = Some(self.width / self.height.max(1.0));
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _: &mut (),
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        _cx: &mut App,
    ) {
        // A picture that cannot be painted leaves its box empty; nothing here may fail a frame.
        window
            .paint_image(
                bounds,
                bounds,
                gpui::Corners::default(),
                self.image.clone(),
                0,
                false,
            )
            .ok();
    }
}

/// A formula to paint over `range` of the text.
pub(crate) struct Overlay {
    pub(crate) range: Range<usize>,
    pub(crate) math: Rc<Typeset>,
}

/// Text with formulas painted over its reserved gaps.
pub(crate) struct Overlays {
    child: AnyElement,
    layout: TextLayout,
    items: Vec<Overlay>,
}

impl Overlays {
    /// `child` draws the text laid out by `layout`; `items` are the gaps to fill.
    pub(crate) fn new(child: AnyElement, layout: TextLayout, items: Vec<Overlay>) -> Self {
        Self {
            child,
            layout,
            items,
        }
    }
}

/// Where a formula's picture goes: its top-left, given the gap's start and end positions in the
/// laid-out text, the line height, the line's ascent and descent, and the formula's width and its
/// extent `above` the baseline. The formula is centred in the gap and its baseline is the text's:
/// the line's top, plus the padding GPUI puts above the glyphs, plus the ascent, is the
/// baseline; the picture's `above` rises from it.
pub(crate) fn place(
    start: gpui::Point<Pixels>,
    end: gpui::Point<Pixels>,
    line_height: Pixels,
    (ascent, descent): (Pixels, Pixels),
    (width, above): (f32, f32),
) -> gpui::Point<Pixels> {
    let gap = if (end.y - start.y).abs() < px(1.0) {
        end.x - start.x
    } else {
        px(width)
    };
    let padding_top = (line_height - ascent - descent) / 2.0;
    let baseline = start.y + padding_top + ascent;
    point(
        start.x + ((gap - px(width)) / 2.0).max(px(0.0)),
        baseline - px(above),
    )
}

impl Element for Overlays {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        (self.child.request_layout(window, cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.prepaint(window, cx);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
        let line_height = self.layout.line_height();
        for item in &self.items {
            let (Some(start), Some(end)) = (
                self.layout.position_for_index(item.range.start),
                self.layout.position_for_index(item.range.end),
            ) else {
                continue;
            };
            let Some(line) = self.layout.line_layout_for_index(item.range.start) else {
                continue;
            };
            let at = place(
                start,
                end,
                line_height,
                (line.unwrapped_layout.ascent, line.unwrapped_layout.descent),
                (item.math.width, item.math.above),
            );
            let bounds = Bounds::new(at, size(px(item.math.width), px(item.math.height())));
            window
                .paint_image(
                    bounds,
                    bounds,
                    gpui::Corners::default(),
                    item.math.image.clone(),
                    0,
                    false,
                )
                .ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_formula_sits_on_the_baseline_centred_in_its_gap() {
        // A 24 px line, glyphs 15 px tall (12 ascent, 3 descent): 4.5 px above them, so the
        // baseline is 16.5 px below the line's top. A formula 10 px wide rising 11 px fits.
        let at = place(
            point(px(100.0), px(48.0)),
            point(px(120.0), px(48.0)),
            px(24.0),
            (px(12.0), px(3.0)),
            (10.0, 11.0),
        );
        assert_eq!(at.x, px(105.0), "centred in a 20 px gap");
        assert_eq!(
            at.y,
            px(48.0 + 4.5 + 12.0 - 11.0),
            "its baseline is the text's"
        );
    }

    #[test]
    fn a_gap_that_wrapped_puts_the_formula_at_its_start() {
        let at = place(
            point(px(300.0), px(24.0)),
            point(px(20.0), px(48.0)),
            px(24.0),
            (px(12.0), px(3.0)),
            (10.0, 11.0),
        );
        assert_eq!(at.x, px(300.0));
    }

    #[test]
    fn a_formula_wider_than_its_gap_starts_at_the_gap() {
        let at = place(
            point(px(10.0), px(0.0)),
            point(px(15.0), px(0.0)),
            px(20.0),
            (px(10.0), px(2.0)),
            (30.0, 8.0),
        );
        assert_eq!(at.x, px(10.0));
    }
}
