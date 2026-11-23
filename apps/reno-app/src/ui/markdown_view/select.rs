//! What selection and find need from painted text (TRANSCRIPT-2.1, 2.13, 3.5).
//!
//! The renderer lays text out with GPUI, which knows where a character is only after layout. A
//! [`Track`] wraps one laid-out text and, when it is painted, files it in the transcript's
//! [`Registry`] with a [`Map`] from laid-out bytes to source bytes, so a pointer position can be
//! turned into a source offset ([`Piece::source_at`]) and a selected source range into the laid
//! out range to paint ([`Map::layout_range`]). Highlights (the selection, find hits) are put on
//! a text's runs by [`highlight_runs`].

use std::cell::RefCell;
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, HighlightStyle,
    InspectorElementId, LayoutId, Pixels, Point, TextLayout, TextRun, Window, point, px,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::transcript::Key;
use crate::ui::theme::{Appearance, Theme};
use crate::vm::anchor::Atom;
use crate::vm::selection::{SelectUnit, Slot};

/// How a laid-out text's bytes relate to its source.
#[derive(Clone, Debug)]
pub(crate) enum Map {
    /// The text is source bytes `base..` (a line of an output starts partway into it); bytes
    /// past `len` (an ellipsis the view adds) belong to the end.
    Linear { base: usize, len: usize },
    /// The text has no source: selecting any of it selects all of it.
    Sourceless,
    /// Markdown runs: each laid-out range is one atom of the row.
    Atoms {
        atoms: Rc<Vec<Atom>>,
        segs: Vec<Seg>,
    },
}

/// A laid-out range that is atom number `atom` of the row.
#[derive(Clone, Debug)]
pub(crate) struct Seg {
    pub(crate) layout: Range<usize>,
    pub(crate) atom: usize,
}

impl Map {
    pub(crate) fn is_sourceless(&self) -> bool {
        matches!(self, Self::Sourceless)
    }

    /// The source byte for laid-out byte `at`.
    pub(crate) fn source_at(&self, at: usize) -> usize {
        match self {
            Self::Linear { base, len } => base + at.min(*len),
            Self::Sourceless => 0,
            Self::Atoms { atoms, segs } => {
                let seg = segs
                    .iter()
                    .rev()
                    .find(|s| s.layout.start <= at)
                    .or(segs.first());
                match seg.and_then(|s| Some((s, atoms.get(s.atom)?))) {
                    Some((seg, atom)) => {
                        if at >= seg.layout.end {
                            atom.src.end
                        } else {
                            atom.src_at(at - seg.layout.start)
                        }
                    }
                    None => 0,
                }
            }
        }
    }

    /// The source bytes a laid-out range is, for selecting it: a run that is not text (a
    /// formula, an image) is taken whole, and the markers around a run's text are left out.
    pub(crate) fn source_range(&self, layout: Range<usize>) -> Range<usize> {
        let Self::Atoms { atoms, segs } = self else {
            return self.source_at(layout.start)..self.source_at(layout.end);
        };
        let first = segs.iter().find(|s| s.layout.end > layout.start);
        let last = segs.iter().rev().find(|s| s.layout.start < layout.end);
        let first = first.and_then(|s| Some((s, atoms.get(s.atom)?)));
        let last = last.and_then(|s| Some((s, atoms.get(s.atom)?)));
        // A range that reaches an edge of the block takes the markers there with it.
        let at_front = layout.start <= segs.first().map_or(0, |s| s.layout.start);
        let at_back = segs.last().is_some_and(|s| layout.end >= s.layout.end);
        let start = first.map_or(0, |(seg, atom)| {
            if let (true, Some(block)) = (at_front, &atom.block) {
                block.start
            } else if atom.is_atomic() {
                atom.src.start
            } else {
                atom.src_at(layout.start.saturating_sub(seg.layout.start))
            }
        });
        let end = last.map_or(start, |(seg, atom)| {
            if let (true, Some(block)) = (at_back, &atom.block) {
                block.end
            } else if atom.is_atomic() {
                atom.src.end
            } else {
                atom.src_at(layout.end - seg.layout.start)
            }
        });
        start..end.max(start)
    }

    /// The laid-out range that `sel` (bytes of the source) covers; for a text with no source,
    /// `all` says whether it is selected, and the range is all of the text.
    pub(crate) fn layout_range(
        &self,
        sel: &Range<usize>,
        text_len: usize,
        sourceless_selected: bool,
    ) -> Option<Range<usize>> {
        match self {
            Self::Linear { base, len } => {
                let (from, to) = (sel.start.max(*base), sel.end.min(base + len));
                if from >= to {
                    // The ellipsis after a cut text is selected with the text before it.
                    return None;
                }
                let end = if to == base + len {
                    text_len
                } else {
                    to - base
                };
                Some(from - base..end)
            }
            Self::Sourceless => sourceless_selected.then_some(0..text_len),
            Self::Atoms { atoms, segs } => {
                let mut found: Option<Range<usize>> = None;
                for seg in segs {
                    let Some(atom) = atoms.get(seg.atom) else {
                        continue;
                    };
                    let part = if atom.src.is_empty() {
                        (sel.start <= atom.src.start && atom.src.start < sel.end)
                            .then(|| seg.layout.clone())
                    } else {
                        let (from, to) = (sel.start.max(atom.src.start), sel.end.min(atom.src.end));
                        (from < to).then(|| {
                            if atom.is_atomic() {
                                seg.layout.clone()
                            } else {
                                seg.layout.start + atom.text_at(from)
                                    ..seg.layout.start + atom.text_at(to)
                            }
                        })
                    };
                    if let Some(part) = part.filter(|p| p.start < p.end) {
                        found = Some(match found {
                            Some(f) => f.start.min(part.start)..f.end.max(part.end),
                            None => part,
                        });
                    }
                }
                found
            }
        }
    }
}

/// One painted text.
#[derive(Clone)]
pub(crate) struct Piece {
    pub(crate) bounds: Bounds<Pixels>,
    pub(crate) layout: TextLayout,
    pub(crate) key: Key,
    pub(crate) slot: Slot,
    pub(crate) map: Map,
}

impl Piece {
    /// The source byte under `point`, or the nearest one when it is outside the text.
    pub(crate) fn source_at(&self, point: Point<Pixels>) -> usize {
        let at = match self.layout.index_for_position(point) {
            Ok(at) | Err(at) => at,
        };
        self.map.source_at(at)
    }

    /// The source bytes of the word or drawn line under `point`; none for any other unit.
    pub(crate) fn unit_at(&self, point: Point<Pixels>, unit: SelectUnit) -> Option<Range<usize>> {
        let text = self.layout.text();
        let range = match unit {
            SelectUnit::Word => {
                let (Ok(at) | Err(at)) = self.layout.index_for_position(point);
                word_at(&text, at)
            }
            SelectUnit::Line => self.line_at(point, &text),
            SelectUnit::Char | SelectUnit::Block => return None,
        };
        Some(self.map.source_range(range))
    }

    /// The row of the text that `point` is on, as bytes of the laid-out text, without the
    /// spaces a wrapped row ends in.
    fn line_at(&self, over: Point<Pixels>, text: &str) -> Range<usize> {
        let bounds = self.layout.bounds();
        let y = over.y.clamp(bounds.top(), bounds.bottom() - px(1.0));
        let index = |x| {
            let (Ok(at) | Err(at)) = self.layout.index_for_position(point(x, y));
            at.min(text.len())
        };
        let start = index(bounds.left());
        let end = index(bounds.right() + px(1.0)).max(start);
        start..start + text.get(start..end).unwrap_or_default().trim_end().len()
    }
}

/// The word, or the run of spaces or punctuation, that byte `at` of `text` is in.
pub(crate) fn word_at(text: &str, at: usize) -> Range<usize> {
    let mut found = 0..0;
    for (start, word) in text.split_word_bound_indices() {
        found = start..start + word.len();
        if at < found.end {
            break;
        }
    }
    found
}

/// The texts painted this frame, filled as they are painted and emptied before the next draw.
pub(crate) type Registry = Rc<RefCell<Vec<Piece>>>;

/// Wraps a laid-out text and files it in the registry when it is painted.
pub(crate) struct Track {
    child: AnyElement,
    piece: Option<Piece>,
    registry: Registry,
}

impl Track {
    pub(crate) fn new(
        child: AnyElement,
        layout: TextLayout,
        (key, slot, map): (Key, Slot, Map),
        registry: Registry,
    ) -> Self {
        Self {
            child,
            piece: Some(Piece {
                bounds: Bounds::default(),
                layout,
                key,
                slot,
                map,
            }),
            registry,
        }
    }
}

impl Element for Track {
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
        if let Some(mut piece) = self.piece.take() {
            piece.bounds = piece.layout.bounds();
            self.registry.borrow_mut().push(piece);
        }
    }
}

/// How the selection is painted: the accent at low strength. Every colour is a theme token.
pub(crate) fn selection_style(theme: &Theme) -> HighlightStyle {
    let dark = theme.appearance == Appearance::Dark;
    HighlightStyle {
        background_color: Some(theme.accent.opacity(if dark { 0.32 } else { 0.26 })),
        ..HighlightStyle::default()
    }
}

/// Puts `layers` over the text of `runs` (which cover it from byte zero): within a layer's range
/// each property it sets replaces the run's. Later layers win over earlier ones.
pub(crate) fn highlight_runs(
    runs: Vec<TextRun>,
    layers: &[(Range<usize>, HighlightStyle)],
) -> Vec<TextRun> {
    if layers.is_empty() {
        return runs;
    }
    let mut cuts: Vec<usize> = layers.iter().flat_map(|(r, _)| [r.start, r.end]).collect();
    cuts.sort_unstable();
    cuts.dedup();
    let mut out = Vec::with_capacity(runs.len() + cuts.len());
    let mut at = 0;
    for run in runs {
        let end = at + run.len;
        let mut from = at;
        let inside = cuts.iter().copied().filter(|c| *c > at && *c < end);
        for cut in inside.chain(std::iter::once(end)) {
            let mut piece = run.clone();
            piece.len = cut - from;
            for (range, style) in layers {
                if range.start <= from && cut <= range.end {
                    if let Some(c) = style.color {
                        piece.color = c;
                    }
                    if let Some(w) = style.font_weight {
                        piece.font.weight = w;
                    }
                    if let Some(s) = style.font_style {
                        piece.font.style = s;
                    }
                    if let Some(b) = style.background_color {
                        piece.background_color = Some(b);
                    }
                    if let Some(u) = style.underline {
                        piece.underline = Some(u);
                    }
                    if let Some(s) = style.strikethrough {
                        piece.strikethrough = Some(s);
                    }
                }
            }
            if piece.len > 0 {
                out.push(piece);
            }
            from = cut;
        }
        at = end;
    }
    out
}

/// `layers` as sorted, non-overlapping highlights for `StyledText::with_highlights`: where
/// layers meet, each property a later one sets wins.
pub(crate) fn flatten_layers(
    layers: &[(Range<usize>, HighlightStyle)],
) -> Vec<(Range<usize>, HighlightStyle)> {
    let mut cuts: Vec<usize> = layers
        .iter()
        .filter(|(r, _)| r.start < r.end)
        .flat_map(|(r, _)| [r.start, r.end])
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    cuts.windows(2)
        .filter_map(|w| {
            let mut style: Option<HighlightStyle> = None;
            for (range, layer) in layers {
                if range.start <= w[0] && w[1] <= range.end {
                    style = Some(style.map_or(*layer, |acc| acc.highlight(*layer)));
                }
            }
            style.map(|style| (w[0]..w[1], style))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::anchor;
    use gpui::{Hsla, font, rgb};

    fn run(len: usize) -> TextRun {
        TextRun {
            len,
            font: font("Geist"),
            color: Hsla::from(rgb(0xffffff)),
            background_color: None,
            underline: None,
            strikethrough: None,
        }
    }

    fn bg(color: u32) -> HighlightStyle {
        HighlightStyle {
            background_color: Some(Hsla::from(rgb(color))),
            ..HighlightStyle::default()
        }
    }

    #[test]
    fn a_highlight_splits_the_runs_it_crosses_and_keeps_their_total_length() {
        let runs = vec![run(5), run(5)];
        let out = highlight_runs(runs, &[(3..7, bg(0xff0000))]);
        let lens: Vec<usize> = out.iter().map(|r| r.len).collect();
        assert_eq!(lens, [3, 2, 2, 3]);
        let marked: Vec<bool> = out.iter().map(|r| r.background_color.is_some()).collect();
        assert_eq!(marked, [false, true, true, false]);
    }

    #[test]
    fn a_later_layer_wins_where_two_overlap() {
        let mut find = bg(0x00ff00);
        find.color = Some(Hsla::from(rgb(0x000000)));
        let out = highlight_runs(vec![run(10)], &[(0..8, bg(0xff0000)), (4..6, find)]);
        let at = |i: usize| out[i].background_color;
        assert_eq!(out.iter().map(|r| r.len).sum::<usize>(), 10);
        assert_eq!(at(0), Some(Hsla::from(rgb(0xff0000))));
        assert_eq!(at(1), Some(Hsla::from(rgb(0x00ff00))));
        assert_eq!(out[1].color, Hsla::from(rgb(0x000000)));
        assert_eq!(out[3].background_color, None);
    }

    #[test]
    fn flattened_layers_do_not_overlap_and_the_later_wins() {
        let out = flatten_layers(&[(0..8, bg(0xff0000)), (4..6, bg(0x00ff00))]);
        let spans: Vec<_> = out.iter().map(|(r, _)| r.clone()).collect();
        assert_eq!(spans, [0..4, 4..6, 6..8]);
        assert_eq!(out[1].1.background_color, Some(Hsla::from(rgb(0x00ff00))));
        assert_eq!(out[2].1.background_color, Some(Hsla::from(rgb(0xff0000))));
    }

    #[test]
    fn no_layers_leave_the_runs_alone() {
        assert_eq!(highlight_runs(vec![run(4)], &[]).len(), 1);
    }

    fn atoms_map(src: &str) -> (Map, usize) {
        let tree = reno_markdown::parse_full(src);
        let atoms = Rc::new(anchor::atoms(src, &tree));
        let mut segs = Vec::new();
        let mut at = 0;
        for (i, a) in atoms.iter().enumerate() {
            segs.push(Seg {
                layout: at..at + a.text.len(),
                atom: i,
            });
            at += a.text.len();
        }
        (Map::Atoms { atoms, segs }, at)
    }

    #[test]
    fn painted_positions_map_to_source_through_markup() {
        // Drawn: "a bold and it".
        let (map, len) = atoms_map("a **bold** and *it*");
        assert_eq!(len, 13);
        assert_eq!(map.source_at(0), 0);
        assert_eq!(map.source_at(3), 5); // b-o in "bold": the 'o'
        assert_eq!(map.source_at(len), 18);
    }

    #[test]
    fn a_selected_source_range_maps_to_the_drawn_range() {
        let (map, _) = atoms_map("a **bold** and *it*");
        // From inside the markers to inside "and": drawn "bold an".
        assert_eq!(map.layout_range(&(2..13), 13, false), Some(2..9));
        assert_eq!(map.layout_range(&(19..19), 13, false), None);
    }

    #[test]
    fn a_text_with_no_source_is_selected_whole_or_not_at_all() {
        let map = Map::Sourceless;
        assert_eq!(map.layout_range(&(0..0), 9, true), Some(0..9));
        assert_eq!(map.layout_range(&(0..0), 9, false), None);
    }

    #[test]
    fn a_line_of_an_output_maps_with_its_base_and_an_ellipsis_belongs_to_the_end() {
        let map = Map::Linear { base: 10, len: 5 };
        assert_eq!(map.source_at(2), 12);
        assert_eq!(map.source_at(9), 15);
        assert_eq!(map.layout_range(&(12..40), 6, false), Some(2..6));
        assert_eq!(map.layout_range(&(0..8), 6, false), None);
    }

    #[test]
    fn a_word_is_the_letters_around_the_click_and_punctuation_is_not_part_of_it() {
        let text = "see the transcript, now";
        assert_eq!(word_at(text, 10), 8..18);
        assert_eq!(word_at(text, 8), 8..18);
        assert_eq!(word_at(text, 18), 18..19, "the comma");
        assert_eq!(word_at(text, 3), 3..4, "the space");
        assert_eq!(
            word_at(text, text.len()),
            20..23,
            "past the end is the last word"
        );
        assert_eq!(word_at("héllo wörld", 9), 7..13);
        assert_eq!(word_at("", 0), 0..0);
    }

    #[test]
    fn a_drawn_range_maps_to_the_source_without_the_markers_around_it() {
        // Drawn: "a bold and it"; "bold" is source 4..8 and its closing markers end at 10.
        let (map, _) = atoms_map("a **bold** and *it*");
        assert_eq!(map.source_range(2..6), 4..8);
        assert_eq!(map.source_range(7..10), 11..14);
        assert_eq!(
            map.source_range(0..13),
            0..19,
            "the last run's closing marker is the block's"
        );
    }

    #[test]
    fn a_range_from_the_first_drawn_byte_of_a_block_starts_at_its_source() {
        // Drawn: "Bold text".
        let (map, len) = atoms_map("**Bold** text");
        assert_eq!(map.source_range(0..len), 0..13);
        assert_eq!(
            map.source_range(0..4),
            0..6,
            "the opening marker, not the closing"
        );
        assert_eq!(
            map.source_range(1..4),
            3..6,
            "inside the first run it stays"
        );
    }

    #[test]
    fn a_range_to_the_last_drawn_byte_of_a_block_ends_at_its_source() {
        // Drawn: "see it".
        let (map, len) = atoms_map("see *it*");
        assert_eq!(
            map.source_range(4..len),
            5..8,
            "the closing marker, not the opening"
        );
        assert_eq!(map.source_range(0..len), 0..8);
        assert_eq!(map.source_range(4..5), 5..6, "short of the end it stays");
    }

    #[test]
    fn a_block_edge_extends_over_the_blank_lines_around_the_block_only_up_to_its_text() {
        let (map, len) = atoms_map("**Bold**\n\nplain *it*\n");
        assert_eq!(map.source_range(0..len), 0..8 + 2 + 10);
    }

    #[test]
    fn a_range_touching_a_formula_takes_the_whole_formula() {
        let (map, _) = atoms_map("p $x^2$ q");
        // Drawn: "p x^2 q"; the word "x" is part of the formula.
        assert_eq!(map.source_range(2..3), 2..7);
        assert_eq!(map.source_range(0..1), 0..1);
    }

    #[test]
    fn a_range_of_a_plain_text_maps_by_its_base() {
        let map = Map::Linear { base: 10, len: 8 };
        assert_eq!(map.source_range(2..5), 12..15);
        assert_eq!(
            map.source_range(2..30),
            12..18,
            "the ellipsis belongs to the end"
        );
    }
}
