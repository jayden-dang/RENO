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

/// What an edit was, to decide whether it joins the undo entry before it: a run of typed
/// characters is one entry, as is a run of Backspaces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Edit {
    Typing,
    Backspace,
    Delete,
    /// An IME composition: its first mark pushed the entry, the rest join it.
    Composing,
    Other,
}

/// The state an undo returns to.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    text: String,
    selection: Range<usize>,
    reversed: bool,
}

/// Undo entries kept.
const HISTORY: usize = 200;

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
    undo: Vec<Snapshot>,
    redo: Vec<Snapshot>,
    /// The last edit, while nothing (a caret move, another kind of edit) has broken the run.
    last_edit: Option<Edit>,
}

impl TextModel {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn snapshot(&self) -> Snapshot {
        Snapshot {
            text: self.text.clone(),
            selection: self.selection.clone(),
            reversed: self.reversed,
        }
    }

    fn restore(&mut self, from: Snapshot) {
        self.text = from.text;
        self.selection = from.selection;
        self.reversed = from.reversed;
        self.marked = None;
        self.goal_column = None;
        self.last_edit = None;
    }

    /// Notes an edit about to happen: pushes an undo entry unless it continues a run.
    fn record(&mut self, kind: Edit) {
        if self.last_edit != Some(kind) || kind == Edit::Other {
            if self.undo.len() == HISTORY {
                self.undo.remove(0);
            }
            let before = self.snapshot();
            self.undo.push(before);
        }
        self.redo.clear();
        self.last_edit = Some(kind);
    }

    /// Undo. Not while the IME is composing. False when there is nothing to undo.
    pub(crate) fn undo(&mut self) -> bool {
        if self.marked.is_some() {
            return false;
        }
        let Some(previous) = self.undo.pop() else {
            return false;
        };
        let now = self.snapshot();
        self.redo.push(now);
        self.restore(previous);
        true
    }

    pub(crate) fn redo(&mut self) -> bool {
        if self.marked.is_some() {
            return false;
        }
        let Some(next) = self.redo.pop() else {
            return false;
        };
        let now = self.snapshot();
        self.undo.push(now);
        self.restore(next);
        true
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

    /// Replaces everything and puts the caret at the end. The undo history starts again: this is
    /// for a new draft (a suggestion, the draft after a send), not an edit.
    pub(crate) fn set_text(&mut self, text: &str) {
        self.text = text.to_owned();
        self.selection = self.text.len()..self.text.len();
        self.reversed = false;
        self.marked = None;
        self.goal_column = None;
        self.undo.clear();
        self.redo.clear();
        self.last_edit = None;
    }

    fn set_caret(&mut self, offset: usize) {
        self.selection = offset..offset;
        self.reversed = false;
    }

    /// Replaces the range with `new` and puts the caret after it.
    fn replace(&mut self, range: Range<usize>, new: &str, kind: Edit) {
        self.record(kind);
        self.text.replace_range(range.clone(), new);
        self.set_caret(range.start + new.len());
        self.marked = None;
        self.goal_column = None;
        // A space or newline ends the word: what is typed next is a new undo entry.
        if new.chars().any(char::is_whitespace) {
            self.last_edit = None;
        }
    }

    /// Types `new` over the marked range, else the selection. Carriage returns are normalised.
    /// One typed character joins the typing before it in one undo entry; a longer text (a
    /// paste) is its own.
    pub(crate) fn insert(&mut self, new: &str) {
        let new = new.replace("\r\n", "\n").replace('\r', "\n");
        let range = self
            .marked
            .clone()
            .unwrap_or_else(|| self.selection.clone());
        let kind = if new.chars().count() == 1 && range.is_empty() {
            Edit::Typing
        } else {
            Edit::Other
        };
        self.replace(range, &new, kind);
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
            self.replace(prev..caret, "", Edit::Backspace);
        } else {
            self.replace(self.selection.clone(), "", Edit::Other);
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
            self.replace(caret..next, "", Edit::Delete);
        } else {
            self.replace(self.selection.clone(), "", Edit::Other);
        }
        true
    }

    /// Option+Backspace: the selection, else back to the start of the word before the caret.
    pub(crate) fn delete_word_back(&mut self) -> bool {
        self.delete_before(Self::word_start_before)
    }

    /// Cmd+Backspace: the selection, else back to the start of the line.
    pub(crate) fn delete_line_back(&mut self) -> bool {
        self.delete_before(|model, caret| {
            let start = model.line_start(caret);
            // At the very start of a line, take the newline before it.
            if start == caret {
                model.previous_boundary(caret)
            } else {
                start
            }
        })
    }

    fn delete_before(&mut self, target: impl Fn(&Self, usize) -> usize) -> bool {
        if !self.selection.is_empty() {
            self.replace(self.selection.clone(), "", Edit::Other);
            return true;
        }
        let caret = self.caret();
        let start = target(self, caret);
        if start == caret {
            return false;
        }
        self.replace(start..caret, "", Edit::Other);
        true
    }

    /// Option+Delete: the selection, else forward to the end of the word after the caret.
    pub(crate) fn delete_word_forward(&mut self) -> bool {
        if !self.selection.is_empty() {
            self.replace(self.selection.clone(), "", Edit::Other);
            return true;
        }
        let caret = self.caret();
        let end = self.word_end_after(caret);
        if end == caret {
            return false;
        }
        self.replace(caret..end, "", Edit::Other);
        true
    }

    /// Removes the selection and returns it, for cut.
    pub(crate) fn cut(&mut self) -> Option<String> {
        let selected = self.selected_text()?.to_owned();
        self.replace(self.selection.clone(), "", Edit::Other);
        Some(selected)
    }

    /// Moves the caret to `offset`, or extends the selection to it.
    pub(crate) fn move_to(&mut self, offset: usize, extend: bool) {
        let offset = self.clamp_to_boundary(offset);
        self.goal_column = None;
        self.last_edit = None;
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
        self.last_edit = None;
    }

    /// Option+Left: to the start of the word before the caret.
    pub(crate) fn move_word_left(&mut self, extend: bool) {
        let target = self.word_start_before(self.caret());
        self.move_to(target, extend);
    }

    /// Option+Right: to the end of the word after the caret.
    pub(crate) fn move_word_right(&mut self, extend: bool) {
        let target = self.word_end_after(self.caret());
        self.move_to(target, extend);
    }

    /// Cmd+Up: the start of the text.
    pub(crate) fn move_doc_start(&mut self, extend: bool) {
        self.move_to(0, extend);
    }

    /// Cmd+Down: the end of the text.
    pub(crate) fn move_doc_end(&mut self, extend: bool) {
        self.move_to(self.text.len(), extend);
    }

    /// Double-click: selects the word (or the run of spaces) at `offset`.
    pub(crate) fn select_word_at(&mut self, offset: usize) {
        let offset = self.clamp_to_boundary(offset);
        let found = self
            .text
            .split_word_bound_indices()
            .find(|(start, word)| offset < start + word.len())
            .map(|(start, word)| start..start + word.len());
        let range = found.unwrap_or(self.text.len()..self.text.len());
        self.select_range(range);
    }

    /// Triple-click: selects the logical line at `offset`, without its newline.
    pub(crate) fn select_line_at(&mut self, offset: usize) {
        let offset = self.clamp_to_boundary(offset);
        self.select_range(self.line_start(offset)..self.line_end(offset));
    }

    fn select_range(&mut self, range: Range<usize>) {
        self.selection = range;
        self.reversed = false;
        self.goal_column = None;
        self.last_edit = None;
    }

    /// Whether a word-boundary segment is a word (has a letter or digit) and not spaces or
    /// punctuation, which Option+arrow steps over.
    fn is_word(segment: &str) -> bool {
        segment.chars().any(char::is_alphanumeric)
    }

    /// The start of the nearest word that starts before `offset`.
    fn word_start_before(&self, offset: usize) -> usize {
        self.text
            .split_word_bound_indices()
            .filter(|(start, word)| *start < offset && Self::is_word(word))
            .map(|(start, _)| start)
            .next_back()
            .unwrap_or(0)
    }

    /// The end of the nearest word that ends after `offset`.
    fn word_end_after(&self, offset: usize) -> usize {
        self.text
            .split_word_bound_indices()
            .find(|(start, word)| start + word.len() > offset && Self::is_word(word))
            .map_or(self.text.len(), |(start, word)| start + word.len())
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
        self.last_edit = None;
    }

    /// IME commit: replace `range` (UTF-16), else the marked range, else the selection.
    pub(crate) fn replace_utf16(&mut self, range: Option<&Range<usize>>, new: &str) {
        let range = range
            .map(|r| self.range_from_utf16(r))
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection.clone());
        let new = new.replace("\r\n", "\n").replace('\r', "\n");
        // A commit ends the composition its first mark began: the same undo entry.
        let kind = if self.marked.is_some() {
            Edit::Composing
        } else {
            Edit::Other
        };
        self.replace(range, &new, kind);
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
        // The first mark opens an undo entry; the updates and the commit after it join it.
        if self.marked.is_none() {
            self.record(Edit::Other);
        }
        self.redo.clear();
        self.last_edit = Some(Edit::Composing);
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

    // ---- undo and redo ------------------------------------------------------------------

    fn typed(m: &mut TextModel, text: &str) {
        for ch in text.chars() {
            m.insert(&ch.to_string());
        }
    }

    #[test]
    fn typing_is_undone_a_word_at_a_time_and_redone() {
        let mut m = TextModel::new();
        typed(&mut m, "hello world");
        assert_eq!(m.text(), "hello world");
        assert!(m.undo());
        assert_eq!(m.text(), "hello ", "the last word goes first");
        assert!(m.undo());
        assert_eq!(m.text(), "");
        assert!(!m.undo(), "nothing left");
        assert!(m.redo());
        assert_eq!(m.text(), "hello ");
        assert!(m.redo());
        assert_eq!((m.text(), m.caret()), ("hello world", 11));
        assert!(!m.redo());
    }

    #[test]
    fn an_edit_after_an_undo_drops_the_redo_entries() {
        let mut m = model("abc");
        m.insert("d");
        assert!(m.undo());
        m.insert("x");
        assert!(!m.redo());
        assert_eq!(m.text(), "abcx");
    }

    #[test]
    fn moving_the_caret_starts_a_new_undo_entry() {
        let mut m = TextModel::new();
        typed(&mut m, "ab");
        m.move_to(0, false);
        typed(&mut m, "cd");
        assert_eq!(m.text(), "cdab");
        assert!(m.undo());
        assert_eq!((m.text(), m.caret()), ("ab", 0), "the caret comes back too");
        assert!(m.undo());
        assert_eq!(m.text(), "");
    }

    #[test]
    fn a_paste_a_cut_and_a_run_of_backspaces_are_each_one_entry() {
        let mut m = TextModel::new();
        m.insert("pasted text");
        typed(&mut m, "!");
        assert!(m.undo());
        assert_eq!(m.text(), "pasted text", "the paste is its own entry");
        m.move_to(0, false);
        m.move_to(6, true);
        assert_eq!(m.cut().as_deref(), Some("pasted"));
        assert!(m.undo());
        assert_eq!((m.text(), m.selection()), ("pasted text", 0..6));
        m.move_to(11, false);
        for _ in 0..4 {
            assert!(m.backspace());
        }
        assert_eq!(m.text(), "pasted ");
        assert!(m.undo());
        assert_eq!(m.text(), "pasted text");
    }

    #[test]
    fn a_whole_ime_composition_is_one_undo_entry() {
        let mut m = TextModel::new();
        typed(&mut m, "a ");
        // Marks and a commit, as the platform sends them.
        m.replace_and_mark_utf16(None, "v", Some(&(1..1)));
        m.replace_and_mark_utf16(None, "vi", Some(&(2..2)));
        m.replace_and_mark_utf16(None, "việ", Some(&(3..3)));
        assert!(!m.undo(), "not while composing");
        m.replace_utf16(None, "việt");
        assert_eq!(m.text(), "a việt");
        assert!(m.undo());
        assert_eq!(m.text(), "a ", "the whole word, not one letter");
        assert!(m.redo());
        assert_eq!(m.text(), "a việt");
    }

    #[test]
    fn set_text_forgets_the_history() {
        let mut m = TextModel::new();
        typed(&mut m, "draft");
        m.set_text("another");
        assert!(!m.undo());
        assert_eq!(m.text(), "another");
    }

    #[test]
    fn the_history_is_bounded() {
        let mut m = TextModel::new();
        for _ in 0..HISTORY + 50 {
            m.insert("word ");
            m.move_to(0, false);
            m.move_to(m.text().len(), false);
        }
        let mut undone = 0;
        while m.undo() {
            undone += 1;
        }
        assert_eq!(undone, HISTORY);
    }

    // ---- words, lines, double and triple click ----------------------------------------------

    #[test]
    fn option_arrows_step_over_words_and_punctuation() {
        let mut m = model("foo, bar-baz  qux");
        m.move_to(0, false);
        m.move_word_right(false);
        assert_eq!(m.caret(), 3, "end of foo");
        m.move_word_right(false);
        assert_eq!(m.caret(), 8, "end of bar");
        m.move_word_right(false);
        assert_eq!(m.caret(), 12, "end of baz");
        m.move_word_right(false);
        m.move_word_right(false);
        assert_eq!(m.caret(), 17, "stops at the end");
        m.move_word_left(false);
        assert_eq!(m.caret(), 14, "start of qux");
        m.move_word_left(false);
        assert_eq!(m.caret(), 9, "start of baz");
        m.move_word_left(true);
        assert_eq!((m.selection(), m.reversed()), (5..9, true));
        m.move_to(1, false);
        m.move_word_left(false);
        assert_eq!(m.caret(), 0);
    }

    #[test]
    fn word_movement_keeps_vietnamese_words_whole() {
        let mut m = model("xin chào việt nam");
        m.move_to(0, false);
        m.move_word_right(false);
        m.move_word_right(false);
        assert_eq!(&m.text()[..m.caret()], "xin chào");
        m.move_word_right(false);
        assert_eq!(&m.text()[..m.caret()], "xin chào việt");
        m.move_word_left(false);
        assert_eq!(&m.text()[m.caret()..], "việt nam");

        // Decomposed accents are part of their letter.
        let mut d = model("vie\u{323}\u{302}t nam");
        d.move_to(0, false);
        d.move_word_right(false);
        assert_eq!(&d.text()[..d.caret()], "vie\u{323}\u{302}t");
    }

    #[test]
    fn option_backspace_and_delete_take_a_word() {
        let mut m = model("one two three");
        assert!(m.delete_word_back());
        assert_eq!(m.text(), "one two ");
        assert!(m.undo());
        assert_eq!(m.text(), "one two three");
        m.move_to(4, false);
        assert!(m.delete_word_forward());
        assert_eq!(m.text(), "one  three");
        m.move_to(0, false);
        assert!(!m.delete_word_back());
        m.select_all();
        assert!(m.delete_word_back(), "a selection is deleted whole");
        assert_eq!(m.text(), "");
    }

    #[test]
    fn cmd_backspace_deletes_to_the_start_of_the_line() {
        let mut m = model("first\nsecond line");
        m.move_to(9, false);
        assert!(m.delete_line_back());
        assert_eq!(m.text(), "first\nond line");
        m.move_to(6, false);
        assert!(m.delete_line_back(), "at a line start it takes the newline");
        assert_eq!(m.text(), "firstond line");
    }

    #[test]
    fn double_click_selects_a_word_and_triple_click_a_line() {
        let mut m = model("alpha beta\ngamma");
        m.select_word_at(7);
        assert_eq!(m.selected_text(), Some("beta"));
        m.select_word_at(5);
        assert_eq!(m.selected_text(), Some(" "));
        m.select_word_at(m.text().len());
        assert_eq!(m.selection(), 16..16, "past the end selects nothing");
        m.select_line_at(3);
        assert_eq!(m.selected_text(), Some("alpha beta"));
        m.select_line_at(12);
        assert_eq!(m.selected_text(), Some("gamma"));
        m.move_doc_start(true);
        assert_eq!((m.selection(), m.reversed()), (0..11, true));
        m.move_doc_end(false);
        assert_eq!(m.caret(), 16);
    }

    // ---- graphemes: Vietnamese, emoji ---------------------------------------------------

    #[test]
    fn backspace_removes_a_whole_decomposed_vietnamese_letter() {
        // "việt" with every accent as a combining mark: v i e + dot below + circumflex, t.
        let mut m = model("vie\u{323}\u{302}t");
        assert!(m.backspace());
        assert_eq!(m.text(), "vie\u{323}\u{302}");
        assert!(m.backspace());
        assert_eq!(m.text(), "vi", "base letter and both marks go together");
        m.move_to(0, false);
        m.move_right(false);
        m.move_right(false);
        assert_eq!(m.caret(), 2);
    }

    #[test]
    fn emoji_sequences_move_and_delete_as_one_character() {
        // A family (ZWJ sequence), a flag (two regional indicators) and a skin-toned thumb.
        let text = "a👨‍👩‍👧🇻🇳👍🏽b";
        let mut m = model(text);
        m.move_left(false);
        m.move_left(false);
        assert_eq!(&m.text()[m.caret()..], "👍🏽b");
        m.move_left(false);
        assert_eq!(&m.text()[m.caret()..], "🇻🇳👍🏽b");
        m.move_left(false);
        assert_eq!(&m.text()[m.caret()..], "👨‍👩‍👧🇻🇳👍🏽b");
        assert!(m.delete());
        assert_eq!(m.text(), "a🇻🇳👍🏽b");
        m.select_all();
        m.move_right(false);
        m.move_left(true);
        assert_eq!(m.selected_text(), Some("b"));
    }

    #[test]
    fn utf16_ranges_of_astral_characters_round_trip() {
        let m = model("a👍🏽b");
        // 👍🏽 is two code points, four UTF-16 units.
        assert_eq!(m.utf16_len(), 6);
        assert_eq!(m.range_to_utf16(&(1..9)), 1..5);
        assert_eq!(m.range_from_utf16(&(1..5)), 1..9);
    }

    // ---- Telex-shaped composition ----------------------------------------------------------

    #[test]
    fn telex_rewrites_committed_text_in_place() {
        // Typing "a" then "a" in Telex turns the committed "a" into "â": the platform replaces
        // the UTF-16 range of the earlier letter with no marked text.
        let mut m = TextModel::new();
        m.replace_utf16(None, "a");
        m.replace_utf16(Some(&(0..1)), "\u{e2}");
        assert_eq!((m.text(), m.marked()), ("\u{e2}", None));
        m.replace_utf16(None, "n");
        assert_eq!(m.text(), "\u{e2}n");
        // A tone mark afterwards rewrites the whole syllable.
        m.replace_utf16(Some(&(0..2)), "\u{1ea5}n");
        assert_eq!(m.text(), "\u{1ea5}n");
        assert_eq!(m.caret(), 4);
        assert!(m.undo());
        assert_eq!(m.text(), "\u{e2}n");
    }

    #[test]
    fn a_japanese_conversion_replaces_the_marked_kana() {
        let mut m = model("今日は");
        m.replace_and_mark_utf16(None, "き", Some(&(1..1)));
        m.replace_and_mark_utf16(None, "きょ", Some(&(2..2)));
        assert_eq!(m.text(), "今日はきょ");
        assert_eq!(m.marked(), Some(9..15));
        // The candidate list converts the marked range.
        m.replace_and_mark_utf16(None, "京", Some(&(1..1)));
        assert_eq!(m.text(), "今日は京");
        m.replace_utf16(None, "京");
        assert_eq!((m.text(), m.marked()), ("今日は京", None));
        assert_eq!(m.caret(), m.text().len());
    }

    #[test]
    fn marking_over_a_selection_replaces_it() {
        let mut m = model("hello world");
        m.move_to(0, false);
        m.move_to(5, true);
        m.replace_and_mark_utf16(None, "x", Some(&(1..1)));
        assert_eq!(m.text(), "x world");
        assert_eq!(m.marked(), Some(0..1));
    }

    #[test]
    fn selecting_with_the_keyboard_and_typing_over_it() {
        let mut m = model("one two");
        m.move_to(0, false);
        m.move_word_right(true);
        m.move_right(true);
        assert_eq!(m.selected_text(), Some("one "));
        m.move_word_left(true);
        assert_eq!(m.selected_text(), None, "back to the anchor");
        m.move_word_right(true);
        m.insert("1");
        assert_eq!(m.text(), "1 two");
    }
}
