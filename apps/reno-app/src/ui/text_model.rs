//! The composer's text and selection as plain data: no GPUI, so every edit is a unit test.
//!
//! Offsets are UTF-8 byte offsets on grapheme boundaries. The platform input handler speaks
//! UTF-16, so the conversions live here too. Vertical movement follows logical lines (split at
//! `\n`); soft-wrapped visual lines are not tracked by the model.

use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

/// What Enter did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum EnterOutcome {
    /// Plain Enter on non-blank text: the trimmed text, now removed from the model.
    Submitted(String),
    /// Shift+Enter: a newline was inserted.
    Newline,
    /// Plain Enter on blank text: nothing happened.
    Ignored,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct TextModel {
    text: String,
    selection: Range<usize>,
    /// The caret is at `selection.start` (the selection was made backwards).
    reversed: bool,
    /// IME composition range.
    marked: Option<Range<usize>>,
    /// Column kept across consecutive up/down moves, in graphemes.
    goal_column: Option<usize>,
}

impl TextModel {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    pub(crate) fn selection(&self) -> Range<usize> {
        self.selection.clone()
    }

    pub(crate) fn reversed(&self) -> bool {
        self.reversed
    }

    pub(crate) fn marked(&self) -> Option<Range<usize>> {
        self.marked.clone()
    }

    /// The moving end of the selection.
    pub(crate) fn caret(&self) -> usize {
        if self.reversed {
            self.selection.start
        } else {
            self.selection.end
        }
    }

    pub(crate) fn can_submit(&self) -> bool {
        !self.text.trim().is_empty()
    }

    pub(crate) fn selected_text(&self) -> Option<&str> {
        (!self.selection.is_empty()).then(|| &self.text[self.selection.clone()])
    }

    /// Replaces everything and puts the caret at the end.
    pub(crate) fn set_text(&mut self, text: &str) {
        self.text = text.to_owned();
        self.selection = self.text.len()..self.text.len();
        self.reversed = false;
        self.marked = None;
        self.goal_column = None;
    }

    fn set_caret(&mut self, offset: usize) {
        self.selection = offset..offset;
        self.reversed = false;
    }

    /// Replaces the range with `new` and puts the caret after it.
    fn replace(&mut self, range: Range<usize>, new: &str) {
        self.text.replace_range(range.clone(), new);
        self.set_caret(range.start + new.len());
        self.marked = None;
        self.goal_column = None;
    }

    /// Types `new` over the marked range, else the selection. Carriage returns are normalised.
    pub(crate) fn insert(&mut self, new: &str) {
        let new = new.replace("\r\n", "\n").replace('\r', "\n");
        let range = self
            .marked
            .clone()
            .unwrap_or_else(|| self.selection.clone());
        self.replace(range, &new);
    }

    /// Enter and Shift+Enter. Enter submits non-blank text; Shift+Enter always adds a newline.
    pub(crate) fn enter(&mut self, shift: bool) -> EnterOutcome {
        if shift {
            self.insert("\n");
            EnterOutcome::Newline
        } else {
            self.take_submission()
                .map_or(EnterOutcome::Ignored, EnterOutcome::Submitted)
        }
    }

    /// The trimmed text, clearing the model; `None` (and no change) when blank.
    pub(crate) fn take_submission(&mut self) -> Option<String> {
        if !self.can_submit() {
            return None;
        }
        let out = self.text.trim().to_owned();
        self.set_text("");
        Some(out)
    }

    /// Backspace: the selection, else one grapheme before the caret. False when nothing moved.
    pub(crate) fn backspace(&mut self) -> bool {
        if self.selection.is_empty() {
            let caret = self.caret();
            let prev = self.previous_boundary(caret);
            if prev == caret {
                return false;
            }
            self.replace(prev..caret, "");
        } else {
            self.replace(self.selection.clone(), "");
        }
        true
    }

    /// Delete: the selection, else one grapheme after the caret.
    pub(crate) fn delete(&mut self) -> bool {
        if self.selection.is_empty() {
            let caret = self.caret();
            let next = self.next_boundary(caret);
            if next == caret {
                return false;
            }
            self.replace(caret..next, "");
        } else {
            self.replace(self.selection.clone(), "");
        }
        true
    }

    /// Removes the selection and returns it, for cut.
    pub(crate) fn cut(&mut self) -> Option<String> {
        let selected = self.selected_text()?.to_owned();
        self.replace(self.selection.clone(), "");
        Some(selected)
    }

    /// Moves the caret to `offset`, or extends the selection to it.
    pub(crate) fn move_to(&mut self, offset: usize, extend: bool) {
        let offset = self.clamp_to_boundary(offset);
        self.goal_column = None;
        if !extend {
            self.set_caret(offset);
            return;
        }
        let anchor = if self.reversed {
            self.selection.end
        } else {
            self.selection.start
        };
        self.selection = anchor.min(offset)..anchor.max(offset);
        self.reversed = offset < anchor;
    }

    pub(crate) fn select_all(&mut self) {
        self.selection = 0..self.text.len();
        self.reversed = false;
        self.goal_column = None;
    }

    /// Left: collapses a selection to its start, else steps back one grapheme (extending keeps
    /// the anchor).
    pub(crate) fn move_left(&mut self, extend: bool) {
        if !extend && !self.selection.is_empty() {
            self.move_to(self.selection.start, false);
        } else {
            self.move_to(self.previous_boundary(self.caret()), extend);
        }
    }

    pub(crate) fn move_right(&mut self, extend: bool) {
        if !extend && !self.selection.is_empty() {
            self.move_to(self.selection.end, false);
        } else {
            self.move_to(self.next_boundary(self.caret()), extend);
        }
    }

    /// Start of the caret's logical line.
    pub(crate) fn move_home(&mut self, extend: bool) {
        let caret = self.caret();
        self.move_to(self.line_start(caret), extend);
    }

    /// End of the caret's logical line.
    pub(crate) fn move_end(&mut self, extend: bool) {
        let caret = self.caret();
        self.move_to(self.line_end(caret), extend);
    }

    pub(crate) fn move_up(&mut self, extend: bool) {
        self.move_vertical(false, extend);
    }

    pub(crate) fn move_down(&mut self, extend: bool) {
        self.move_vertical(true, extend);
    }

    /// Up and down keep a goal column. On the first line, up goes to the start; on the last,
    /// down goes to the end.
    fn move_vertical(&mut self, down: bool, extend: bool) {
        let caret = self.caret();
        let start = self.line_start(caret);
        let column = self
            .goal_column
            .unwrap_or_else(|| self.text[start..caret].graphemes(true).count());
        let target = if down {
            let end = self.line_end(caret);
            if end == self.text.len() {
                None
            } else {
                Some(end + 1)
            }
        } else if start == 0 {
            None
        } else {
            Some(self.line_start(start - 1))
        };
        let Some(line_start) = target else {
            let edge = if down { self.text.len() } else { 0 };
            self.move_to(edge, extend);
            self.goal_column = Some(column);
            return;
        };
        let line_end = self.line_end(line_start);
        let offset = self.text[line_start..line_end]
            .grapheme_indices(true)
            .nth(column)
            .map_or(line_end, |(i, _)| line_start + i);
        self.move_to(offset, extend);
        self.goal_column = Some(column);
    }

    fn line_start(&self, offset: usize) -> usize {
        self.text[..offset].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self, offset: usize) -> usize {
        self.text[offset..]
            .find('\n')
            .map_or(self.text.len(), |i| offset + i)
    }

    pub(crate) fn previous_boundary(&self, offset: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .rev()
            .find_map(|(i, _)| (i < offset).then_some(i))
            .unwrap_or(0)
    }

    pub(crate) fn next_boundary(&self, offset: usize) -> usize {
        self.text
            .grapheme_indices(true)
            .find_map(|(i, _)| (i > offset).then_some(i))
            .unwrap_or(self.text.len())
    }

    fn clamp_to_boundary(&self, offset: usize) -> usize {
        let offset = offset.min(self.text.len());
        if self.text.is_char_boundary(offset) {
            offset
        } else {
            self.previous_boundary(offset)
        }
    }

    // ---- UTF-16 views for the platform input handler -------------------------------------

    pub(crate) fn offset_from_utf16(&self, offset: usize) -> usize {
        let mut utf8 = 0;
        let mut utf16 = 0;
        for ch in self.text.chars() {
            if utf16 >= offset {
                break;
            }
            utf16 += ch.len_utf16();
            utf8 += ch.len_utf8();
        }
        utf8
    }

    pub(crate) fn offset_to_utf16(&self, offset: usize) -> usize {
        let mut utf16 = 0;
        let mut utf8 = 0;
        for ch in self.text.chars() {
            if utf8 >= offset {
                break;
            }
            utf8 += ch.len_utf8();
            utf16 += ch.len_utf16();
        }
        utf16
    }

    pub(crate) fn range_to_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_to_utf16(range.start)..self.offset_to_utf16(range.end)
    }

    pub(crate) fn range_from_utf16(&self, range: &Range<usize>) -> Range<usize> {
        self.offset_from_utf16(range.start)..self.offset_from_utf16(range.end)
    }

    #[cfg(test)]
    pub(crate) fn utf16_len(&self) -> usize {
        self.text.encode_utf16().count()
    }

    #[cfg(test)]
    pub(crate) fn set_selection_utf16(&mut self, range: &Range<usize>) {
        self.selection = self.range_from_utf16(range);
        self.reversed = false;
        self.goal_column = None;
    }

    /// IME commit: replace `range` (UTF-16), else the marked range, else the selection.
    pub(crate) fn replace_utf16(&mut self, range: Option<&Range<usize>>, new: &str) {
        let range = range
            .map(|r| self.range_from_utf16(r))
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection.clone());
        let new = new.replace("\r\n", "\n").replace('\r', "\n");
        self.replace(range, &new);
    }

    /// IME composition: replace as above, mark the new text, and select `selected` (UTF-16,
    /// relative to the inserted text).
    pub(crate) fn replace_and_mark_utf16(
        &mut self,
        range: Option<&Range<usize>>,
        new: &str,
        selected: Option<&Range<usize>>,
    ) {
        let range = range
            .map(|r| self.range_from_utf16(r))
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection.clone());
        self.text.replace_range(range.clone(), new);
        self.marked = (!new.is_empty()).then(|| range.start..range.start + new.len());
        let start16 = self.offset_to_utf16(range.start);
        let inserted16 = new.encode_utf16().count();
        self.selection = selected.map_or_else(
            || range.start + new.len()..range.start + new.len(),
            |s| {
                self.range_from_utf16(
                    &(start16 + s.start.min(inserted16)..start16 + s.end.min(inserted16)),
                )
            },
        );
        self.reversed = false;
        self.goal_column = None;
    }

    pub(crate) fn unmark(&mut self) {
        self.marked = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(text: &str) -> TextModel {
        let mut m = TextModel::new();
        m.set_text(text);
        m
    }

    #[test]
    fn insert_types_at_the_caret_and_over_a_selection() {
        let mut m = TextModel::new();
        m.insert("hello");
        assert_eq!((m.text(), m.caret()), ("hello", 5));
        m.move_to(0, false);
        m.insert(">> ");
        assert_eq!(m.text(), ">> hello");
        m.select_all();
        m.insert("x");
        assert_eq!((m.text(), m.caret(), m.selection()), ("x", 1, 1..1));
    }

    #[test]
    fn insert_normalises_line_endings() {
        let mut m = TextModel::new();
        m.insert("a\r\nb\rc");
        assert_eq!(m.text(), "a\nb\nc");
    }

    #[test]
    fn backspace_and_delete_remove_whole_graphemes() {
        let mut m = model("ae\u{301}🙂");
        assert!(m.backspace());
        assert_eq!(m.text(), "ae\u{301}");
        assert!(m.backspace());
        assert_eq!(m.text(), "a");
        m.move_to(0, false);
        assert!(!m.backspace(), "nothing before the start");
        assert!(m.delete());
        assert_eq!(m.text(), "");
        assert!(!m.delete());
    }

    #[test]
    fn backspace_deletes_a_selection_instead_of_a_grapheme() {
        let mut m = model("abcdef");
        m.move_to(1, false);
        m.move_to(4, true);
        assert_eq!(m.selected_text(), Some("bcd"));
        assert!(m.backspace());
        assert_eq!((m.text(), m.caret()), ("aef", 1));
    }

    #[test]
    fn left_right_collapse_selections_and_step_otherwise() {
        let mut m = model("abc");
        m.move_left(false);
        assert_eq!(m.caret(), 2);
        m.move_to(0, false);
        m.move_right(true);
        m.move_right(true);
        assert_eq!((m.selection(), m.reversed()), (0..2, false));
        m.move_right(false);
        assert_eq!(
            (m.selection(), m.caret()),
            (2..2, 2),
            "collapses to the end"
        );
        m.move_left(true);
        m.move_left(true);
        assert_eq!((m.selection(), m.reversed()), (0..2, true));
        m.move_left(false);
        assert_eq!(m.selection(), 0..0, "collapses to the start");
        m.move_left(false);
        assert_eq!(m.caret(), 0, "stops at the start");
    }

    #[test]
    fn extending_past_the_anchor_flips_the_selection() {
        let mut m = model("abcdef");
        m.move_to(3, false);
        m.move_to(5, true);
        assert_eq!((m.selection(), m.reversed()), (3..5, false));
        m.move_to(1, true);
        assert_eq!((m.selection(), m.reversed()), (1..3, true));
        assert_eq!(m.caret(), 1);
    }

    #[test]
    fn home_and_end_use_the_logical_line() {
        let mut m = model("one\ntwo\nthree");
        m.move_to(6, false);
        m.move_home(false);
        assert_eq!(m.caret(), 4);
        m.move_end(false);
        assert_eq!(m.caret(), 7);
        m.move_home(true);
        assert_eq!((m.selection(), m.reversed()), (4..7, true));
    }

    #[test]
    fn up_and_down_keep_the_column() {
        let mut m = model("abcdef\nab\nabcdef");
        m.move_to(5, false);
        m.move_down(false);
        assert_eq!(m.caret(), 9, "clamped to the short line's end");
        m.move_down(false);
        assert_eq!(m.caret(), 10 + 5, "goal column restored");
        m.move_down(false);
        assert_eq!(m.caret(), m.text().len(), "last line: down goes to the end");
        m.move_to(0, false);
        m.move_up(false);
        assert_eq!(m.caret(), 0);
        m.move_to(8, false);
        m.move_up(true);
        assert_eq!((m.selection(), m.reversed()), (1..8, true));
    }

    #[test]
    fn enter_submits_trimmed_text_and_clears() {
        let mut m = model("  fix the bug \n");
        assert_eq!(
            m.enter(false),
            EnterOutcome::Submitted("fix the bug".into())
        );
        assert!(m.is_empty());
        assert_eq!(m.caret(), 0);
    }

    #[test]
    fn enter_on_blank_text_does_nothing() {
        let mut m = model(" \n ");
        assert_eq!(m.enter(false), EnterOutcome::Ignored);
        assert_eq!(m.text(), " \n ", "blank text is left alone");
        assert!(!m.can_submit());
    }

    #[test]
    fn shift_enter_inserts_a_newline_even_when_blank() {
        let mut m = TextModel::new();
        assert_eq!(m.enter(true), EnterOutcome::Newline);
        assert_eq!(m.text(), "\n");
        m.insert("x");
        m.move_to(0, false);
        assert_eq!(m.enter(true), EnterOutcome::Newline);
        assert_eq!(m.text(), "\n\nx");
        assert_eq!(m.caret(), 1);
    }

    #[test]
    fn cut_returns_and_removes_the_selection() {
        let mut m = model("hello world");
        assert_eq!(m.cut(), None);
        m.move_to(0, false);
        m.move_to(5, true);
        assert_eq!(m.cut().as_deref(), Some("hello"));
        assert_eq!(m.text(), " world");
    }

    #[test]
    fn utf16_offsets_round_trip() {
        let m = model("e🙂漢");
        assert_eq!(m.utf16_len(), 4);
        assert_eq!(m.offset_to_utf16(5), 3);
        assert_eq!(m.offset_from_utf16(3), 5);
        assert_eq!(m.range_from_utf16(&(1..3)), 1..5);
    }

    #[test]
    fn ime_composition_marks_replaces_and_commits() {
        let mut m = model("A🙂Z");
        m.replace_and_mark_utf16(Some(&(1..3)), "é🙂", Some(&(1..3)));
        assert_eq!(m.text(), "Aé🙂Z");
        assert_eq!(m.marked(), Some(1..7));
        assert_eq!(m.selection(), 3..7);

        m.replace_and_mark_utf16(None, "漢", Some(&(1..1)));
        assert_eq!(m.text(), "A漢Z");
        assert_eq!(m.marked(), Some(1..4));
        assert_eq!(m.selection(), 4..4);

        m.replace_utf16(None, "字");
        assert_eq!((m.text(), m.marked()), ("A字Z", None));
        assert_eq!(m.caret(), 4);

        m.replace_and_mark_utf16(None, "", Some(&(0..0)));
        assert_eq!(m.marked(), None);
    }

    #[test]
    fn set_selection_utf16_clamps_to_the_text() {
        let mut m = model("e🙂漢");
        m.set_selection_utf16(&(1..3));
        assert_eq!(m.selection(), 1..5);
        m.set_selection_utf16(&(3..99));
        assert_eq!(m.selection(), 5..8);
    }
}
