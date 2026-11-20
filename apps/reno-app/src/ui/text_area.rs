//! The composer's painted text: shapes wrapped lines, paints the selection, the text and the
//! caret, scrolls when the text is taller than the box, and installs the platform input handler
//! (IME: `NSTextInputClient` on macOS).
//!
//! [`TextLayout`] is what the last paint measured. The composer uses it to turn a mouse position
//! into a byte offset, to move the caret by visual rows, and to tell the IME where the marked
//! text is. All of those are in window coordinates and include the scroll offset, so a long
//! draft that has scrolled still clicks and composes in the right place.

use std::ops::Range;

use gpui::{
    App, AvailableSpace, Bounds, ContentMask, Element, ElementId, ElementInputHandler, Entity,
    GlobalElementId, Hsla, InspectorElementId, LayoutId, PaintQuad, Pixels, Point, SharedString,
    Style, TextAlign, TextRun, TextStyle, UnderlineStyle, Window, WrappedLine, fill, point, px,
    relative, size,
};

use super::composer::Composer;
use super::theme::Theme;

/// The text area grows to this many rows, then scrolls.
pub(crate) const MAX_ROWS: usize = 10;

/// The last painted text, kept for hit testing, vertical moves and IME geometry.
pub(crate) struct TextLayout {
    lines: Vec<WrappedLine>,
    starts: Vec<usize>,
    /// Where the text begins in the window: the box's origin moved up by the scroll offset.
    origin: Point<Pixels>,
    /// The visible box.
    viewport: Bounds<Pixels>,
    line_height: Pixels,
    /// The text this layout was shaped from.
    text: String,
}

impl TextLayout {
    /// The layout matches `text` (nothing has changed since it was painted).
    pub(crate) fn is_for(&self, text: &str) -> bool {
        self.text == text
    }

    pub(crate) fn line_height(&self) -> Pixels {
        self.line_height
    }

    /// The height of all the text.
    pub(crate) fn content_height(&self) -> Pixels {
        self.lines
            .iter()
            .map(|l| l.size(self.line_height).height)
            .fold(px(0.0), |a, b| a + b)
    }

    /// How far the text can scroll.
    pub(crate) fn max_scroll(&self) -> Pixels {
        (self.content_height() - self.viewport.size.height).max(px(0.0))
    }

    /// The byte offset under a window position.
    pub(crate) fn index_for_position(&self, position: Point<Pixels>) -> usize {
        let local = position - self.origin;
        if local.y < px(0.0) {
            return 0;
        }
        let mut top = px(0.0);
        for (line, start) in self.lines.iter().zip(&self.starts) {
            let height = line.size(self.line_height).height;
            if local.y < top + height {
                let hit = line
                    .closest_index_for_position(point(local.x, local.y - top), self.line_height);
                // A faint completion after the text is not text: a click on it lands at the end.
                return (start + hit.unwrap_or_else(|end| end)).min(self.text.len());
            }
            top += height;
        }
        self.text.len()
    }

    /// Where a byte offset sits, relative to the start of the text.
    pub(crate) fn point_for_index(&self, index: usize) -> Option<Point<Pixels>> {
        let mut top = px(0.0);
        for (line, start) in self.lines.iter().zip(&self.starts) {
            if index <= start + line.len() {
                let local = line.position_for_index(index - start, self.line_height)?;
                return Some(point(local.x, top + local.y));
            }
            top += line.size(self.line_height).height;
        }
        None
    }

    /// Where a byte offset sits in the window.
    pub(crate) fn window_point_for_index(&self, index: usize) -> Option<Point<Pixels>> {
        self.point_for_index(index).map(|p| self.origin + p)
    }

    /// The window position a vertical move from `from` lands on, `rows` visual rows away (negative
    /// is up), keeping the horizontal position `x`. `None` when that is past the first or last row.
    pub(crate) fn index_for_row_move(
        &self,
        from: usize,
        rows: i32,
        x: Option<Pixels>,
    ) -> Option<(usize, Pixels)> {
        let at = self.point_for_index(from)?;
        let x = x.unwrap_or(at.x);
        let y = at.y + self.line_height * rows as f32;
        if y < px(0.0) || y >= self.content_height() {
            return None;
        }
        let target = self.origin + point(x, y + self.line_height * 0.5);
        Some((self.index_for_position(target), x))
    }
}

/// Byte offset where each logical line starts, given each line's byte length. Lines are split at
/// `\n`, which the lines themselves do not include.
pub(crate) fn line_starts(lengths: impl IntoIterator<Item = usize>) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut next = 0;
    for len in lengths {
        starts.push(next);
        next += len + 1;
    }
    starts
}

/// The painted text area.
pub(crate) struct TextArea {
    pub(crate) composer: Entity<Composer>,
    pub(crate) theme: Theme,
    pub(crate) placeholder: SharedString,
    /// The area is at least this many rows tall.
    pub(crate) min_rows: usize,
    /// Text drawn in the faint colour: the box takes no input.
    pub(crate) disabled: bool,
}

pub(crate) struct Prepaint {
    lines: Vec<WrappedLine>,
    starts: Vec<usize>,
    placeholder: Vec<WrappedLine>,
    selection: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
    line_height: Pixels,
    origin: Point<Pixels>,
    scroll: Pixels,
}

fn run_for(style: &TextStyle, color: Hsla, len: usize) -> TextRun {
    TextRun {
        len,
        font: style.font(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}

/// What the draft adds to the text's look without touching the text: coloured words, and a faint
/// completion after the caret. Empty when the draft is off, and the text shapes as before.
#[derive(Clone, Debug, Default)]
pub(crate) struct Decor {
    /// Byte ranges of the text, in order, with their ink and an optional fill.
    pub(crate) spans: Vec<(Range<usize>, Hsla, Option<Hsla>)>,
    /// Painted after the end of the text in this ink; not part of the text.
    pub(crate) ghost: Option<(String, Hsla)>,
}

/// The runs of `text`: IME composition underlined, `decor`'s words coloured, its ghost last.
fn runs_for(
    style: &TextStyle,
    color: Hsla,
    text: &str,
    marked: Option<&Range<usize>>,
    decor: &Decor,
) -> Vec<TextRun> {
    let marked = marked.filter(|m| m.end <= text.len() && m.start < m.end);
    let mut cuts = vec![0, text.len()];
    for (range, _, _) in &decor.spans {
        cuts.extend([range.start.min(text.len()), range.end.min(text.len())]);
    }
    if let Some(m) = marked {
        cuts.extend([m.start, m.end]);
    }
    cuts.sort_unstable();
    cuts.dedup();
    let mut runs: Vec<TextRun> = cuts
        .windows(2)
        .map(|pair| {
            let (a, b) = (pair[0], pair[1]);
            let mut run = run_for(style, color, b - a);
            if let Some((_, ink, fill)) = decor
                .spans
                .iter()
                .find(|(range, _, _)| range.start <= a && b <= range.end)
            {
                run.color = *ink;
                run.background_color = *fill;
            }
            if marked.is_some_and(|m| m.start <= a && b <= m.end) {
                run.underline = Some(UnderlineStyle {
                    color: Some(run.color),
                    thickness: px(1.0),
                    wavy: false,
                });
            }
            run
        })
        .collect();
    if runs.is_empty() {
        runs.push(run_for(style, color, 0));
    }
    if let Some((ghost, ink)) = &decor.ghost {
        runs.push(run_for(style, *ink, ghost.len()));
    }
    runs
}

/// Shapes `text` (split at newlines, wrapped at `width`), underlining the IME `marked` range.
fn shape(
    window: &Window,
    style: &TextStyle,
    text: &SharedString,
    color: Hsla,
    marked: Option<&Range<usize>>,
    decor: &Decor,
    width: Option<Pixels>,
) -> Vec<WrappedLine> {
    let runs = runs_for(style, color, text, marked, decor);
    let shown = match &decor.ghost {
        Some((ghost, _)) => SharedString::from(format!("{text}{ghost}")),
        None => text.clone(),
    };
    let font_size = style.font_size.to_pixels(window.rem_size());
    window
        .text_system()
        .shape_text(shown, font_size, &runs, width, None)
        .map(|lines| lines.into_iter().collect())
        .unwrap_or_default()
}

fn row_count(lines: &[WrappedLine]) -> usize {
    lines
        .iter()
        .map(|l| l.wrap_boundaries().len() + 1)
        .sum::<usize>()
        .max(1)
}

/// Selection rectangles for `range`, relative to the text origin: one per visual row, and a
/// small stub where the selection takes in a newline.
fn selection_rects(
    lines: &[WrappedLine],
    starts: &[usize],
    line_height: Pixels,
    width: Pixels,
    range: &Range<usize>,
) -> Vec<Bounds<Pixels>> {
    let mut rects = Vec::new();
    let mut top = px(0.0);
    for (line, &start) in lines.iter().zip(starts) {
        let end = start + line.len();
        let height = line.size(line_height).height;
        let takes_newline = range.end > end && range.start <= end;
        if range.end > start && range.start <= end {
            let a = range.start.max(start) - start;
            let b = range.end.min(end) - start;
            if a < b || takes_newline {
                let pa = line.position_for_index(a, line_height).unwrap_or_default();
                let pb = line.position_for_index(b, line_height).unwrap_or_default();
                let row =
                    |p: Point<Pixels>| (f32::from(p.y) / f32::from(line_height)).round() as usize;
                for r in row(pa)..=row(pb) {
                    let x0 = if r == row(pa) { pa.x } else { px(0.0) };
                    let mut x1 = if r == row(pb) { pb.x } else { width };
                    if r == row(pb) && takes_newline {
                        x1 += px(6.0);
                    }
                    rects.push(Bounds::new(
                        point(x0, top + line_height * r),
                        size(x1 - x0, line_height),
                    ));
                }
            }
        }
        top += height;
    }
    rects
}

impl Element for TextArea {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let (text, marked, decor) = {
            let composer = self.composer.read(cx);
            (
                SharedString::from(composer.text_for_layout().to_owned()),
                composer.marked_range(),
                composer.decor(&self.theme, cx),
            )
        };
        let style = window.text_style();
        let line_height = window.line_height();
        let color = self.theme.text;
        let min_rows = self.min_rows;
        let mut layout = Style::default();
        layout.size.width = relative(1.0).into();
        let id = window.request_measured_layout(layout, move |known, available, window, _| {
            let width = known.width.or(match available.width {
                AvailableSpace::Definite(w) => Some(w),
                _ => None,
            });
            let lines = shape(window, &style, &text, color, marked.as_ref(), &decor, width);
            let rows = row_count(&lines).clamp(min_rows, MAX_ROWS);
            size(width.unwrap_or_default(), line_height * rows)
        });
        (id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let composer = self.composer.read(cx);
        let text = SharedString::from(composer.text_for_layout().to_owned());
        let selection = composer.selection();
        let caret = composer.caret();
        let marked = composer.marked_range();
        let decor = composer.decor(&self.theme, cx);
        let focused = composer.is_focused(window);
        let reveal = composer.wants_caret_visible();
        let mut scroll = composer.scroll_offset();
        let style = window.text_style();
        let line_height = window.line_height();
        let text_color = if self.disabled {
            self.theme.faint
        } else {
            self.theme.text
        };

        let lines = shape(
            window,
            &style,
            &text,
            text_color,
            marked.as_ref(),
            &decor,
            Some(bounds.size.width),
        );
        let placeholder = if text.is_empty() {
            shape(
                window,
                &style,
                &self.placeholder,
                self.theme.faint,
                None,
                &Decor::default(),
                Some(bounds.size.width),
            )
        } else {
            Vec::new()
        };
        let starts = line_starts(lines.iter().map(WrappedLine::len));

        // Where the caret is, from the top of the text.
        let caret_at = lines
            .iter()
            .zip(&starts)
            .scan(px(0.0), |top, (line, &start)| {
                let line_top = *top;
                *top += line.size(line_height).height;
                Some((line, start, line_top))
            })
            .find(|(line, start, _)| caret <= start + line.len())
            .and_then(|(line, start, top)| {
                line.position_for_index(caret - start, line_height)
                    .map(|p| point(p.x, top + p.y))
            })
            .unwrap_or_default();

        // Scroll to keep the caret in view after an edit or a move, and within the text.
        let content: Pixels = lines
            .iter()
            .map(|l| l.size(line_height).height)
            .fold(px(0.0), |a, b| a + b);
        let max_scroll = (content - bounds.size.height).max(px(0.0));
        if reveal {
            if caret_at.y < scroll {
                scroll = caret_at.y;
            } else if caret_at.y + line_height > scroll + bounds.size.height {
                scroll = caret_at.y + line_height - bounds.size.height;
            }
        }
        scroll = scroll.clamp(px(0.0), max_scroll);
        let origin = bounds.origin - point(px(0.0), scroll);

        let accent = self.theme.accent;
        let selection_quads =
            selection_rects(&lines, &starts, line_height, bounds.size.width, &selection)
                .into_iter()
                .map(|r| fill(Bounds::new(origin + r.origin, r.size), accent.opacity(0.28)))
                .collect();

        let cursor = (focused && selection.is_empty() && !self.disabled).then(|| {
            fill(
                Bounds::new(origin + caret_at, size(px(1.5), line_height)),
                self.theme.text,
            )
        });

        Prepaint {
            lines,
            starts,
            placeholder,
            selection: selection_quads,
            cursor,
            line_height,
            origin,
            scroll,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.composer.read(cx).focus_handle_ref().clone();
        if !self.disabled {
            window.handle_input(
                &focus_handle,
                ElementInputHandler::new(bounds, self.composer.clone()),
                cx,
            );
        }
        let line_height = prepaint.line_height;
        let origin = prepaint.origin;
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for quad in prepaint.selection.drain(..) {
                window.paint_quad(quad);
            }
            let mut top = px(0.0);
            let painted = if prepaint.lines.is_empty() || self.composer.read(cx).is_blank() {
                &prepaint.placeholder
            } else {
                &prepaint.lines
            };
            for line in painted {
                let _ = line.paint(
                    origin + point(px(0.0), top),
                    line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
                top += line.size(line_height).height;
            }
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        });

        let lines = std::mem::take(&mut prepaint.lines);
        let starts = std::mem::take(&mut prepaint.starts);
        let scroll = prepaint.scroll;
        self.composer.update(cx, |composer, _| {
            composer.painted(
                TextLayout {
                    text: composer.text_for_layout().to_owned(),
                    lines,
                    starts,
                    origin,
                    viewport: bounds,
                    line_height,
                },
                scroll,
            );
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_starts_skip_the_newline_between_lines() {
        assert_eq!(line_starts([3, 0, 5]), [0, 4, 5]);
        assert_eq!(line_starts([]), Vec::<usize>::new());
        assert_eq!(line_starts([0]), [0]);
    }
}
