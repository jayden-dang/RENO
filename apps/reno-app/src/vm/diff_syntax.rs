//! Syntax colour for the lines of a diff. A patch has only the lines around a change, so the
//! colours come from the whole file: `workspace.file_sources` gives both sides, each is
//! highlighted once, and a diff line takes the spans of the source line it numbers. A line
//! whose text is not the source's (the file moved on, or a stub) stays plain rather than wearing
//! another line's colours.

use std::ops::Range;
use std::sync::Arc;

use reno_syntax::{HighlightRequest, HighlightSpan, HighlightedDocument};

use super::changes::{DiffRow, RowKind};

/// One side of a file: its text, where each line is, and its highlight (none for a language
/// Reno has no grammar for).
#[derive(Debug)]
struct Side {
    text: Arc<str>,
    lines: Vec<Range<usize>>,
    doc: Option<HighlightedDocument>,
}

impl Side {
    fn build(path: &str, text: String) -> Self {
        let text: Arc<str> = Arc::from(text);
        let mut lines = Vec::new();
        let mut start = 0;
        for line in text.split_inclusive('\n') {
            let body = line.strip_suffix('\n').unwrap_or(line);
            let body = body.strip_suffix('\r').unwrap_or(body);
            lines.push(start..start + body.len());
            start += line.len();
        }
        let doc = reno_syntax::highlight(HighlightRequest {
            source: &text,
            path: Some(path),
            fence_tag: None,
        })
        .ok();
        Self { text, lines, doc }
    }

    /// The spans of line `number` (1-based) when it reads `text`.
    fn spans(&self, number: u32, text: &str) -> Option<&[HighlightSpan]> {
        let ix = usize::try_from(number).ok()?.checked_sub(1)?;
        let range = self.lines.get(ix)?;
        if &self.text[range.clone()] != text {
            return None;
        }
        let spans = self.doc.as_ref()?.lines.get(ix)?;
        (!spans.is_empty()).then_some(spans.as_slice())
    }
}

/// The highlighted sides of one changed file.
#[derive(Debug)]
pub(crate) struct FileSyntax {
    old: Option<Side>,
    new: Option<Side>,
}

impl FileSyntax {
    /// Highlights both sides. Costs a parse of each: run it off the UI thread.
    pub(crate) fn build(path: &str, old: Option<String>, new: Option<String>) -> Self {
        Self {
            old: old.map(|t| Side::build(path, t)),
            new: new.map(|t| Side::build(path, t)),
        }
    }

    /// The spans of a diff line, when the file has them for it. An added line is read from the
    /// new side, a removed one from the old, a context line from the new (or the old).
    pub(crate) fn spans(&self, row: &DiffRow) -> Option<&[HighlightSpan]> {
        match row.kind {
            RowKind::Add => self.new.as_ref()?.spans(row.new?, &row.text),
            RowKind::Del => self.old.as_ref()?.spans(row.old?, &row.text),
            RowKind::Context => self
                .new
                .as_ref()
                .zip(row.new)
                .and_then(|(side, n)| side.spans(n, &row.text))
                .or_else(|| {
                    self.old
                        .as_ref()
                        .zip(row.old)
                        .and_then(|(side, n)| side.spans(n, &row.text))
                }),
            RowKind::Hunk | RowKind::NoNewline | RowKind::Binary => None,
        }
    }

    /// How many of `rows` have colours.
    pub(crate) fn coloured(&self, rows: &[DiffRow]) -> usize {
        rows.iter().filter(|r| self.spans(r).is_some()).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(kind: RowKind, old: Option<u32>, new: Option<u32>, text: &str) -> DiffRow {
        DiffRow {
            kind,
            old,
            new,
            text: text.to_owned(),
        }
    }

    const OLD: &str = "fn start() {\n    run();\n}\n";
    const NEW: &str = "fn main() {\n    run();\n}\n";

    #[test]
    fn a_line_takes_the_colours_of_the_source_line_it_numbers() {
        let s = FileSyntax::build("m.rs", Some(OLD.to_owned()), Some(NEW.to_owned()));
        let added = row(RowKind::Add, None, Some(1), "fn main() {");
        let spans = s.spans(&added).expect("the keyword is coloured");
        assert!(spans.iter().any(|sp| sp.range.start == 0));
        let removed = row(RowKind::Del, Some(1), None, "fn start() {");
        assert!(
            s.spans(&removed).is_some(),
            "the old side colours a removed line"
        );
        let context = row(RowKind::Context, Some(2), Some(2), "    run();");
        assert!(s.coloured(&[added, removed, context]) >= 2);
    }

    #[test]
    fn a_line_that_is_not_the_sources_stays_plain() {
        let s = FileSyntax::build("m.rs", None, Some(NEW.to_owned()));
        // Another text at that number, a number past the end, and no number at all.
        let add = |n, t| row(RowKind::Add, None, Some(n), t);
        assert!(s.spans(&add(1, "fn other() {")).is_none());
        assert!(s.spans(&add(99, "fn main() {")).is_none());
        assert!(s.spans(&add(0, "fn main() {")).is_none());
        // An added file has no old side to colour a removed line from.
        assert!(
            s.spans(&row(RowKind::Del, Some(1), None, "fn start() {"))
                .is_none()
        );
        assert!(
            s.spans(&row(RowKind::Hunk, None, None, "@@ -1 +1 @@"))
                .is_none()
        );
    }

    #[test]
    fn a_language_without_a_grammar_is_plain_not_an_error() {
        let s = FileSyntax::build("notes.zzz", None, Some("hello\n".to_owned()));
        assert!(
            s.spans(&row(RowKind::Add, None, Some(1), "hello"))
                .is_none()
        );
    }

    #[test]
    fn line_endings_do_not_break_the_match() {
        let s = FileSyntax::build("m.rs", None, Some("fn main() {\r\n}\r\n".to_owned()));
        assert!(
            s.spans(&row(RowKind::Add, None, Some(1), "fn main() {"))
                .is_some()
        );
    }
}
