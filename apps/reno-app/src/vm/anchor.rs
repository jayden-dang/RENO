//! Where drawn markdown text came from in its source (TRANSCRIPT-2.13, 2.4 to 2.7).
//!
//! A reply is drawn from a tree of runs that have lost their markers (`**`, link targets, the
//! `$` around a formula). A selection must resolve to the *source*, so every drawn run is
//! lined up with the source text it came from, in drawing order: an [`Atom`]. Selecting maps a
//! painted position to a source byte offset, and copying maps a source range back to the text
//! the owner saw (math as its TeX in `$`, a diagram as its fenced source, an image as its alt
//! text, a file link as its path).
//!
//! Pure: no window, no layout.

use std::ops::Range;
use std::rc::Rc;

use reno_markdown::{Block, BlockTree, InlineRun};

use crate::links;
use crate::ui::markdown_view::link::file_target;
use crate::vm::selection::{self, Entry, Selection, Shown, Slot};

/// What separates an atom from the one before it when both are copied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Gap {
    /// Nothing: the same paragraph.
    Joined,
    /// A line break: code lines, list items, table rows.
    Line,
    /// A blank line: the next block.
    Blank,
    /// A tab: the next table cell.
    Tab,
}

impl Gap {
    const fn text(self) -> &'static str {
        match self {
            Self::Joined => "",
            Self::Line => "\n",
            Self::Blank => "\n\n",
            Self::Tab => "\t",
        }
    }
}

/// How an atom's drawn text relates to its source.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Shape {
    /// Drawn bytes are the source bytes.
    Exact,
    /// Drawn text with source bytes skipped or changed (a soft break, an escape): pairs of
    /// `(source offset, text offset)` at each character.
    Walk(Vec<(usize, usize)>),
    /// A reference selects as a unit, but find maps each displayed character.
    Footnote(Vec<(usize, usize)>),
    /// Not text: a formula, an image, a diagram. Selected whole.
    Whole,
}

/// One drawn run, lined up with the source it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Atom {
    /// Bytes of the row's source this run came from (empty when it could not be found).
    pub(crate) src: Range<usize>,
    /// The text as drawn (a formula's TeX, an image's alt text).
    pub(crate) text: String,
    /// What copying all of it gives.
    pub(crate) copy: String,
    /// What comes before it when copied after another atom.
    pub(crate) gap: Gap,
    shape: Shape,
    /// The source of the paragraph or heading this run is in, without the blank lines around
    /// it: where a selection that reaches the block's edge extends to, so the markers there
    /// (`**`, a closing backtick) come with it. Empty for a run in any other block.
    pub(crate) block: Option<Range<usize>>,
}

impl Atom {
    /// A text of its own with no markup: all of `text` is the source `src`.
    pub(crate) fn plain(src: Range<usize>, text: &str) -> Self {
        Self {
            src,
            text: text.to_owned(),
            copy: text.to_owned(),
            gap: Gap::Line,
            shape: Shape::Exact,
            block: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn is_exact(&self) -> bool {
        self.shape == Shape::Exact
    }

    /// Whether selection takes the atom as a whole.
    pub(crate) fn is_atomic(&self) -> bool {
        matches!(self.shape, Shape::Whole | Shape::Footnote(_))
    }

    /// The source byte for the drawn byte `at` of this run. On an atomic run, the nearer edge.
    pub(crate) fn src_at(&self, at: usize) -> usize {
        match &self.shape {
            Shape::Exact => (self.src.start + at).min(self.src.end),
            Shape::Walk(pairs) => pairs
                .iter()
                .find(|(_, t)| *t >= at)
                .map_or(self.src.end, |(s, _)| *s),
            Shape::Whole | Shape::Footnote(_) => {
                if at * 2 < self.text.len().max(1) {
                    self.src.start
                } else {
                    self.src.end
                }
            }
        }
    }

    pub(crate) fn search_source(&self, drawn: &Range<usize>) -> Option<Range<usize>> {
        match &self.shape {
            Shape::Footnote(offsets) if !offsets.is_empty() => {
                let source_at = |at| {
                    offsets
                        .get(offsets.partition_point(|(_, text)| *text < at))
                        .map_or(self.src.end, |(source, _)| *source)
                };
                Some(source_at(drawn.start)..source_at(drawn.end))
            }
            Shape::Footnote(_) => None,
            Shape::Whole => Some(self.src.clone()),
            _ => Some(self.src_at(drawn.start)..self.src_at(drawn.end)),
        }
    }

    /// The drawn byte for the source byte `at` (the next one drawn when `at` is in markup).
    pub(crate) fn text_at(&self, at: usize) -> usize {
        match &self.shape {
            Shape::Exact => at.saturating_sub(self.src.start).min(self.text.len()),
            Shape::Walk(pairs) => pairs
                .iter()
                .find(|(s, _)| *s >= at)
                .map_or(self.text.len(), |(_, t)| *t),
            Shape::Whole | Shape::Footnote(_) => {
                if at * 2 < self.src.start + self.src.end {
                    0
                } else {
                    self.text.len()
                }
            }
        }
    }

    /// What copying the part of this run that lies in `sel` gives, `None` when none of it does.
    fn copy_in(&self, sel: &Range<usize>) -> Option<String> {
        if self.src.is_empty() {
            return (sel.start <= self.src.start && self.src.start < sel.end)
                .then(|| self.copy.clone());
        }
        let (from, to) = (sel.start.max(self.src.start), sel.end.min(self.src.end));
        if from >= to {
            return None;
        }
        if self.is_atomic() || self.copy != self.text {
            // A formula, an image, a diagram, a link drawn with a label: all of it or none.
            return Some(self.copy.clone());
        }
        let (a, b) = (self.text_at(from), self.text_at(to));
        let part = self.text.get(a..b).unwrap_or_default();
        (!part.is_empty()).then(|| part.to_owned())
    }
}

/// The text a selection of `sel` (bytes of the source) copies: what is drawn in it, in order.
pub(crate) fn copy(atoms: &[Atom], sel: Range<usize>) -> String {
    let mut out = String::new();
    let mut any = false;
    for atom in atoms {
        let Some(part) = atom.copy_in(&sel) else {
            continue;
        };
        if any {
            out.push_str(atom.gap.text());
        }
        out.push_str(&part);
        any = true;
    }
    out
}

/// What "Copy as Markdown" puts on the clipboard: the source bytes of what `sel` covers, math
/// delimiters and markup as the owner wrote them. A text with no source (a fold head, a step
/// line) is given as shown.
pub(crate) fn source_of(sel: &Selection, entries: &[Entry<'_>], shown: &Shown<'_>) -> String {
    selection::joined(sel, entries, shown, true, |t, range| {
        if t.sourced {
            t.source.get(range).unwrap_or_default().to_owned()
        } else {
            t.copy_all()
        }
    })
}

#[derive(Clone, Debug)]
pub(crate) struct Section {
    pub(crate) slot: Slot,
    pub(crate) source: Range<usize>,
    pub(crate) atoms: Range<usize>,
}

#[derive(Debug)]
pub(crate) struct Prose {
    pub(crate) atoms: Rc<Vec<Atom>>,
    pub(crate) sections: Vec<Section>,
}

impl Prose {
    pub(crate) fn section_at(&self, atom: usize) -> Option<&Section> {
        let ix = self
            .sections
            .partition_point(|section| section.atoms.end <= atom);
        self.sections.get(ix)
    }

    pub(crate) fn slot_at(&self, atom: usize) -> Slot {
        self.section_at(atom)
            .map_or(Slot::WHOLE, |section| section.slot)
    }
}

struct Note {
    source: Range<usize>,
    children: Vec<Range<usize>>,
    atoms: Vec<Atom>,
}

/// Tracks every selectable prose span in drawing order, retaining absolute source byte ranges.
pub(crate) fn prose(source: &str, tree: &BlockTree) -> Prose {
    let mut b = Builder {
        source,
        out: Vec::new(),
        footnotes: Vec::new(),
        root_notes: Vec::new(),
        note_parent: None,
        cursor: 0,
        end: source.len(),
        lead: Gap::Joined,
        block: None,
    };
    for top in &tree.blocks {
        b.cursor = top.range.start.min(source.len());
        b.end = top.range.end.min(source.len());
        b.block = matches!(top.block, Block::Paragraph { .. } | Block::Heading { .. })
            .then(|| trimmed(source, b.cursor..b.end));
        b.block(&top.block, Gap::Blank);
    }

    let mut sections = Vec::new();
    let mut atoms = b.out;
    add_sections(
        &mut sections,
        0..source.len(),
        &b.root_notes,
        &atoms,
        0,
        Slot::prose,
    );
    for note in b.footnotes {
        let first = atoms.len();
        atoms.extend(note.atoms);
        let note_start = note.source.start;
        add_sections(
            &mut sections,
            note.source,
            &note.children,
            &atoms,
            first,
            |from| Slot::note(note_start, from),
        );
    }
    Prose {
        atoms: Rc::new(atoms),
        sections,
    }
}

#[cfg(test)]
pub(crate) fn atoms(source: &str, tree: &BlockTree) -> Vec<Atom> {
    Rc::unwrap_or_clone(prose(source, tree).atoms)
}

fn add_sections(
    sections: &mut Vec<Section>,
    scope: Range<usize>,
    hoisted: &[Range<usize>],
    atoms: &[Atom],
    mut first: usize,
    slot_of: impl Fn(usize) -> Slot,
) {
    let mut from = scope.start;
    for removed in hoisted {
        let end = from.max(removed.start);
        let after = first + atoms[first..].partition_point(|atom| atom.src.start < end);
        if first < after {
            sections.push(Section {
                slot: slot_of(from),
                source: from..end,
                atoms: first..after,
            });
        }
        from = end.max(removed.end);
        first = after;
    }
    if first < atoms.len() || hoisted.is_empty() {
        sections.push(Section {
            slot: slot_of(from),
            source: from..scope.end.max(from),
            atoms: first..atoms.len(),
        });
    }
}

struct Builder<'a> {
    source: &'a str,
    out: Vec<Atom>,
    footnotes: Vec<Note>,
    root_notes: Vec<Range<usize>>,
    note_parent: Option<usize>,
    /// Where in the source the next run is looked for.
    cursor: usize,
    /// The end of the top-level block being walked.
    end: usize,
    /// What goes before the next atom made.
    lead: Gap,
    /// The trimmed source of the top-level block being walked, when it is a paragraph or a heading.
    block: Option<Range<usize>>,
}

/// `range` of `source` without the whitespace at its ends.
fn trimmed(source: &str, range: Range<usize>) -> Range<usize> {
    let text = source.get(range.clone()).unwrap_or_default();
    let start = range.start + (text.len() - text.trim_start().len());
    start..(range.end - (text.len() - text.trim_end().len())).max(start)
}

impl Builder<'_> {
    fn push(&mut self, mut atom: Atom) {
        atom.block.clone_from(&self.block);
        atom.gap = if self.out.is_empty() {
            Gap::Joined
        } else {
            std::mem::replace(&mut self.lead, Gap::Joined)
        };
        self.lead = Gap::Joined;
        self.out.push(atom);
    }

    fn block(&mut self, block: &Block, lead: Gap) {
        self.lead = lead;
        match block {
            Block::Paragraph { runs } | Block::Heading { runs, .. } => self.runs(runs),
            Block::CodeBlock { code, language } => {
                let code = code.trim_end_matches('\n');
                if language.as_deref().map(str::trim) == Some("mermaid") {
                    let (from, to) = (self.cursor, self.end);
                    let fenced = format!("```mermaid\n{code}\n```");
                    self.push(Atom {
                        src: from..to,
                        text: code.to_owned(),
                        copy: fenced,
                        gap: Gap::Joined,
                        shape: Shape::Whole,
                        block: None,
                    });
                    self.cursor = to;
                } else {
                    for (n, line) in code.split('\n').enumerate() {
                        if n > 0 {
                            self.lead = Gap::Line;
                        }
                        let src = self.find_exact(line);
                        self.push(Atom::plain(src, line));
                    }
                }
            }
            Block::Math { tex } => {
                let src = self.find_math(tex).unwrap_or(self.cursor..self.cursor);
                self.push(Atom {
                    copy: format!("$${tex}$$"),
                    text: tex.clone(),
                    src,
                    gap: Gap::Joined,
                    shape: Shape::Whole,
                    block: None,
                });
            }
            Block::BlockQuote { children, .. } => {
                for (n, child) in children.iter().enumerate() {
                    self.block(child, if n == 0 { lead } else { Gap::Blank });
                }
            }
            Block::List { items, .. } => {
                for (n, item) in items.iter().enumerate() {
                    for (m, child) in item.iter().enumerate() {
                        let first = if n == 0 { lead } else { Gap::Line };
                        self.block(child, if m == 0 { first } else { Gap::Line });
                    }
                }
            }
            Block::Table { header, rows, .. } => {
                let all = std::iter::once(header).chain(rows);
                for (r, row) in all.enumerate() {
                    for (c, cell) in row.iter().enumerate() {
                        self.lead = match (r, c) {
                            (0, 0) => lead,
                            (_, 0) => Gap::Line,
                            _ => Gap::Tab,
                        };
                        self.runs(cell);
                    }
                }
            }
            Block::Footnote { blocks, range, .. } => self.footnote(blocks, range),
            Block::FrontMatter { lines, .. } => {
                for (ix, (text, range)) in lines.iter().enumerate() {
                    self.lead = if ix == 0 { lead } else { Gap::Line };
                    self.push(Atom::plain(range.clone(), text));
                }
            }
            Block::Rule => {}
        }
    }

    /// One atom for every run, drawn or not, so the renderer takes `runs.len()` of them.
    fn runs(&mut self, runs: &[InlineRun]) {
        for run in runs {
            let lead = self.lead;
            let atom = self.run(run);
            self.lead = lead;
            self.push(atom);
        }
    }

    fn run(&mut self, run: &InlineRun) -> Atom {
        let style = &run.style;
        if let Some(marker) = &style.task {
            let src = marker.range.clone();
            self.cursor = self.cursor.max(src.end);
            let text = task_box_text(marker.checked).to_owned();
            return Atom {
                src,
                copy: text.clone(),
                text,
                gap: Gap::Joined,
                shape: Shape::Whole,
                block: None,
            };
        }
        if let Some(label) = &style.footnote_ref {
            let src = style
                .footnote_range
                .clone()
                .unwrap_or(self.cursor..self.cursor);
            self.cursor = self.cursor.max(src.end);
            let text = footnote_marker(label);
            let offsets = self
                .source
                .get(src.clone())
                .map_or_else(Vec::new, |spelling| footnote_offsets(spelling, label, &src));
            return Atom {
                src,
                copy: text.clone(),
                text,
                gap: Gap::Joined,
                shape: Shape::Footnote(offsets),
                block: None,
            };
        }
        if let Some(image) = &style.image {
            let src = self.find_image().unwrap_or(self.cursor..self.cursor);
            return Atom {
                copy: format!("[{}]", image.alt),
                text: image.alt.clone(),
                src,
                gap: Gap::Joined,
                shape: Shape::Whole,
                block: None,
            };
        }
        if style.math {
            let src = self
                .find_math(&run.text)
                .unwrap_or(self.cursor..self.cursor);
            return Atom {
                copy: format!("${}$", run.text),
                text: run.text.clone(),
                src,
                gap: Gap::Joined,
                shape: Shape::Whole,
                block: None,
            };
        }
        let (src, shape) = self.find_text(&run.text, !style.code);
        // A link to a worktree file, drawn with a label, copies its path.
        let copy = match style.link.as_deref().map(str::trim) {
            Some(dest)
                if !links::is_web_link(dest)
                    && file_target(dest).is_some()
                    && dest != run.text.trim() =>
            {
                dest.to_owned()
            }
            _ => run.text.clone(),
        };
        Atom {
            src,
            text: run.text.clone(),
            copy,
            gap: Gap::Joined,
            shape,
            block: None,
        }
    }

    fn footnote(&mut self, blocks: &[Block], range: &Range<usize>) {
        let source = range.start.min(self.end)..range.end.min(self.end);
        let index = self.footnotes.len();
        let children = match self.note_parent {
            Some(parent) => &mut self.footnotes[parent].children,
            None => &mut self.root_notes,
        };
        children.push(source.clone());
        self.footnotes.push(Note {
            source: source.clone(),
            children: Vec::new(),
            atoms: Vec::new(),
        });

        let parent = self.note_parent.replace(index);
        let end = std::mem::replace(&mut self.end, source.end);
        self.cursor = source.start;
        if let Some(close) = self.window().find("]:") {
            self.cursor += close + 2;
        }
        let first = self.out.len();
        for child in blocks {
            self.block(child, Gap::Blank);
        }
        self.footnotes[index].atoms = self.out.split_off(first);
        if let Some(atom) = self.footnotes[index].atoms.first_mut() {
            atom.gap = Gap::Blank;
        }
        self.cursor = source.end;
        self.end = end;
        self.note_parent = parent;
    }

    fn window(&self) -> &str {
        self.source.get(self.cursor..self.end).unwrap_or_default()
    }

    /// `text` verbatim after the cursor.
    fn find_exact(&mut self, text: &str) -> Range<usize> {
        if text.is_empty() {
            return self.cursor..self.cursor;
        }
        match self.window().find(text) {
            Some(at) => {
                let from = self.cursor + at;
                self.cursor = from + text.len();
                from..self.cursor
            }
            None => self.cursor..self.cursor,
        }
    }

    /// A run's text after the cursor, allowing what the parser changes: a line break read as a
    /// space, a backslash before punctuation.
    fn find_text(&mut self, text: &str, skip_comments: bool) -> (Range<usize>, Shape) {
        if text.is_empty() {
            return (self.cursor..self.cursor, Shape::Exact);
        }
        let word_end = text.find(char::is_whitespace).unwrap_or(text.len()).max(1);
        let word = &text[..word_end.min(text.len())];
        let window = self.window();
        let skip_prefix_comments = skip_comments && !text.contains("<!--");
        let mut seen = 0;
        for (at, _) in window.match_indices(word) {
            if skip_prefix_comments {
                while seen < at {
                    let Some(open) = window[seen..at].find("<!--") else {
                        seen = at;
                        break;
                    };
                    seen += open;
                    seen += html_comment_len(&window[seen..]).unwrap_or(4);
                }
                if at < seen {
                    continue;
                }
            }
            if let Some((len, pairs)) = walk(&window[at..], text, Spelling::Prose { skip_comments })
            {
                let from = self.cursor + at;
                let pairs: Vec<_> = pairs.iter().map(|(s, t)| (from + s, *t)).collect();
                self.cursor = from + len;
                let exact = len == text.len() && self.source[from..self.cursor] == *text;
                let shape = if exact {
                    Shape::Exact
                } else {
                    Shape::Walk(pairs)
                };
                return (from..self.cursor, shape);
            }
        }
        (self.cursor..self.cursor, Shape::Whole)
    }

    /// A formula's TeX after the cursor, with the delimiters around it.
    fn find_math(&mut self, tex: &str) -> Option<Range<usize>> {
        let window = self.window();
        let mut from = 0;
        while let Some(at) = window[from..].find(tex.trim()) {
            let start = from + at;
            let end = start + tex.trim().len();
            let before = &window[..start];
            let after = &window[end..];
            let open = ["$$", "\\[", "\\(", "$"]
                .into_iter()
                .find(|d| before.trim_end().ends_with(d));
            let close = open.and_then(|o| {
                let want = match o {
                    "\\[" => "\\]",
                    "\\(" => "\\)",
                    d => d,
                };
                after.trim_start().starts_with(want).then_some(want)
            });
            if let (Some(o), Some(c)) = (open, close) {
                let lead = before.len() - before.trim_end().len();
                let trail = after.len() - after.trim_start().len();
                let a = start - lead - o.len();
                let b = end + trail + c.len();
                let (a, b) = (self.cursor + a, self.cursor + b);
                self.cursor = b;
                return Some(a..b);
            }
            from = start + 1;
            while !window.is_char_boundary(from) {
                from += 1;
            }
        }
        None
    }

    /// An image's `![alt](target)` after the cursor.
    fn find_image(&mut self) -> Option<Range<usize>> {
        let window = self.window();
        let start = window.find("![")?;
        let close = window[start..].find("](")? + start;
        let end = window[close..].find(')')? + close + 1;
        let (a, b) = (self.cursor + start, self.cursor + end);
        self.cursor = b;
        Some(a..b)
    }
}

pub(crate) fn task_box_text(checked: bool) -> &'static str {
    if checked { "☑ " } else { "☐ " }
}

pub(crate) fn footnote_marker(label: &str) -> String {
    format!("[{label}]")
}

fn footnote_offsets(spelling: &str, label: &str, src: &Range<usize>) -> Vec<(usize, usize)> {
    let Some(body) = spelling
        .strip_prefix("[^")
        .and_then(|text| text.strip_suffix(']'))
    else {
        return Vec::new();
    };
    let inside = body.trim_start_matches(|c: char| c.is_ascii_whitespace());
    let Some((_, label_offsets)) = walk(inside, label, Spelling::FootnoteLabel) else {
        return Vec::new();
    };
    let label_start = src.start + 2 + body.len() - inside.len();
    std::iter::once((src.start, 0))
        .chain(
            label_offsets
                .into_iter()
                .map(|(source, text)| (label_start + source, 1 + text)),
        )
        .chain(std::iter::once((
            src.end.saturating_sub(1),
            1 + label.len(),
        )))
        .collect()
}

#[derive(Clone, Copy)]
enum Spelling {
    Prose { skip_comments: bool },
    FootnoteLabel,
}

/// Reads `text` off the front of `source`: a run of whitespace in the text matches a run of
/// whitespace in the source (a line break, with its quote or list margin), and a backslash
/// before punctuation is skipped. The source bytes used, with `(source, text)` offsets at each
/// character; `None` when the text is not there.
fn walk(source: &str, text: &str, spelling: Spelling) -> Option<(usize, Vec<(usize, usize)>)> {
    let prose = matches!(spelling, Spelling::Prose { .. });
    let skip_comments = matches!(
        spelling,
        Spelling::Prose {
            skip_comments: true
        }
    );
    let space = |c: char| {
        if prose {
            c.is_whitespace()
        } else {
            c.is_ascii_whitespace()
        }
    };
    let mut pairs = Vec::new();
    let mut at = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((t, c)) = chars.next() {
        if skip_comments && !text[t..].starts_with("<!--") {
            while let Some(len) = html_comment_len(&source[at..]) {
                at += len;
            }
        }
        pairs.push((at, t));
        if space(c) {
            let start = at;
            while chars.peek().is_some_and(|(_, n)| space(*n)) {
                chars.next();
            }
            let next_is_literal_comment = chars
                .peek()
                .is_some_and(|(next, _)| text[*next..].starts_with("<!--"));
            loop {
                at += source[at..]
                    .find(|s: char| !(space(s) || prose && s == '>'))
                    .unwrap_or(source.len() - at);
                let Some(len) = (skip_comments && !next_is_literal_comment)
                    .then(|| html_comment_len(&source[at..]))
                    .flatten()
                else {
                    break;
                };
                at += len;
            }
            if at == start {
                return None;
            }
            continue;
        }
        let rest = &source[at..];
        if rest.starts_with(c) {
            at += c.len_utf8();
        } else if rest.starts_with('\\') && rest[1..].starts_with(c) {
            at += 1 + c.len_utf8();
        } else {
            return None;
        }
    }
    Some((at, pairs))
}

fn html_comment_len(source: &str) -> Option<usize> {
    let body = source.strip_prefix("<!--")?;
    body.find("-->").map(|start| start + 7)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_visible_reference_uses_its_own_source_bytes_after_a_hidden_duplicate() {
        for source in [
            "Before <!-- [^n] -->[^n].\n\n[^n]: Note.",
            "Before<!-- [^n] -->[^n].\n\n[^n]: Note.",
        ] {
            let anchored = atoms(source, &reno_markdown::parse_full(source));
            let footnote = anchored.iter().find(|atom| atom.text == "[n]").unwrap();
            let visible = source.find("-->").unwrap() + 3;
            assert_eq!(footnote.src, visible..visible + "[^n]".len());
            assert!(footnote.is_atomic());
        }
    }

    #[test]
    fn raw_html_keeps_source_anchors_with_both_a_literal_and_a_hidden_comment() {
        let source = "<div title=\"<!--\">Visible<!-- drop --> text</div>";
        let anchored = atoms_of(source);
        let raw = &anchored[0];
        assert_eq!(raw.text, "<div title=\"<!--\">Visible text</div>");
        assert_eq!(raw.src, 0..source.len());
        let visible = source.find("text").unwrap();
        let drawn = raw.text.find("text").unwrap();
        assert_eq!(
            raw.search_source(&(drawn..drawn + 4)),
            Some(visible..visible + 4)
        );
        assert_eq!(copy(&anchored, 0..source.len()), raw.text);
    }

    #[test]
    fn a_literal_greater_than_stays_searchable_inside_a_footnote_label() {
        for raw in ["a > b", "a  >   b"] {
            let source = format!("Use [^{raw}].\n\n[^a > b]: Note.");
            let anchored = atoms_of(&source);
            let note = anchored.iter().find(|atom| atom.text == "[a > b]").unwrap();
            let source_mark = source.find('>').unwrap();
            let drawn_mark = note.text.find('>').unwrap();
            assert_eq!(
                note.search_source(&(drawn_mark..drawn_mark + 1)),
                Some(source_mark..source_mark + 1),
                "{source:?}"
            );
            assert!(note.is_atomic());
        }
    }

    #[test]
    fn metadata_pairs_with_extra_spacing_stay_selectable_from_source() {
        let source = "---\r\n# key: hidden\r\nkey :  ready\r\n---\r\n\r\nTail.";
        let anchored = atoms(source, &reno_markdown::parse_full(source));
        let row = &anchored[0];
        let expected = source.find("key :  ready").unwrap();
        assert_eq!(row.src, expected..expected + "key :  ready".len());
        assert_eq!(row.text, "key :  ready");
        assert_eq!(copy(&anchored, row.src.clone()), "key :  ready");
    }
    use crate::transcript::{Key, Row};
    use crate::vm::selection::{self, Pos, Selection, Shown, Slot};
    use reno_markdown::parse_full;

    fn atoms_of(source: &str) -> Vec<Atom> {
        atoms(source, &parse_full(source))
    }

    fn copied(source: &str, from: usize, to: usize) -> String {
        copy(&atoms_of(source), from..to)
    }

    fn all(source: &str) -> String {
        copied(source, 0, source.len())
    }

    #[test]
    fn plain_text_is_anchored_byte_for_byte() {
        let a = atoms_of("hello world");
        assert_eq!(a.len(), 1);
        assert_eq!(a[0].src, 0..11);
        assert!(a[0].is_exact());
        assert_eq!(copy(&a, 6..11), "world");
    }

    #[test]
    fn emphasis_markers_are_not_in_the_text_but_the_runs_keep_their_source() {
        let src = "a **bold** and *it*";
        let a = atoms_of(src);
        let texts: Vec<&str> = a.iter().map(|x| x.text.as_str()).collect();
        assert_eq!(texts, ["a ", "bold", " and ", "it"]);
        assert_eq!(&src[a[1].src.clone()], "bold");
        assert_eq!(all(src), "a bold and it");
        // A selection that starts inside the markers copies only what is drawn.
        assert_eq!(copied(src, 2, 15), "bold and ");
    }

    #[test]
    fn multibyte_text_maps_on_character_boundaries() {
        let src = "héllo wörld";
        assert_eq!(copied(src, 0, "héllo".len()), "héllo");
        assert_eq!(copied(src, "héllo ".len(), src.len()), "wörld");
    }

    #[test]
    fn inline_math_is_one_atom_over_its_dollars_and_copies_as_tex_in_dollars() {
        let src = "area $x^2$ here";
        let a = atoms_of(src);
        let m = a.iter().find(|x| x.is_atomic()).expect("a formula atom");
        assert_eq!(&src[m.src.clone()], "$x^2$");
        assert_eq!(all(src), "area $x^2$ here");
        // Half a formula copies all of it.
        assert_eq!(copied(src, 5, 8), "$x^2$");
    }

    #[test]
    fn math_copies_with_dollars_whatever_delimiter_the_source_used() {
        assert_eq!(all(r"see \(a+b\) ok"), "see $a+b$ ok");
        assert_eq!(
            all("x\n\n$$\\int_0^1 x\\,dx$$\n\ny"),
            "x\n\n$$\\int_0^1 x\\,dx$$\n\ny"
        );
        assert_eq!(all("\\[a=b\\]"), "$$a=b$$");
    }

    #[test]
    fn a_diagram_copies_as_its_fenced_source() {
        let src = "```mermaid\nflowchart LR\n  A --> B\n```\n";
        assert_eq!(all(src), "```mermaid\nflowchart LR\n  A --> B\n```");
    }

    #[test]
    fn an_image_copies_as_its_alt_text_in_brackets() {
        assert_eq!(
            all("see ![build graph](docs/graph.png) now"),
            "see [build graph] now"
        );
    }

    #[test]
    fn a_file_link_drawn_with_a_label_copies_its_path() {
        let src = "wait in [supervisor](src/supervisor.rs:42), ok";
        assert_eq!(all(src), "wait in src/supervisor.rs:42, ok");
        // A web link copies its label, as drawn.
        assert_eq!(all("[docs](https://example.org/a) here"), "docs here");
        // An inline-code path is its own text.
        assert_eq!(all("at `src/a.rs:3` now"), "at src/a.rs:3 now");
    }

    #[test]
    fn code_lines_are_atoms_and_copy_one_per_line() {
        let src = "```rust\nfn a() {\n    1\n}\n```\n";
        let a = atoms_of(src);
        assert_eq!(a.len(), 3);
        assert_eq!(&src[a[1].src.clone()], "    1");
        assert_eq!(all(src), "fn a() {\n    1\n}");
        assert_eq!(copy(&a, a[1].src.start + 4..a[2].src.end), "1\n}");
    }

    #[test]
    fn blocks_are_separated_by_a_blank_line_and_list_items_by_a_line() {
        assert_eq!(all("one\n\ntwo"), "one\n\ntwo");
        assert_eq!(all("- a\n- b\n- c"), "a\nb\nc");
        assert_eq!(all("# Title\n\nbody"), "Title\n\nbody");
        assert_eq!(all("| a | b |\n|---|---|\n| 1 | 2 |"), "a\tb\n1\t2");
    }

    #[test]
    fn a_soft_line_break_in_the_source_is_one_run_and_a_partial_copy_follows_it() {
        let src = "first line\nsecond line";
        let a = atoms_of(src);
        assert_eq!(a.len(), 1);
        assert_eq!(&src[a[0].src.clone()], src);
        assert_eq!(copy(&a, 11..src.len()), "second line");
    }

    #[test]
    fn a_run_that_cannot_be_found_in_the_source_still_copies_whole() {
        let a = atoms_of("a &amp; b");
        assert_eq!(copy(&a, 0..9), "a & b");
    }

    #[test]
    fn positions_map_between_drawn_text_and_source() {
        let src = "a **bold** b";
        let a = atoms_of(src);
        let bold = &a[1];
        assert_eq!(bold.src_at(2), 4 + 2);
        assert_eq!(bold.text_at(bold.src.start + 1), 1);
        // On an atomic run the nearer edge wins.
        let m = atoms_of("p $x$ q")
            .into_iter()
            .find(Atom::is_atomic)
            .unwrap();
        assert_eq!(m.src_at(0), m.src.start);
        assert_eq!(m.src_at(m.text.len()), m.src.end);
    }

    fn source_between(rows: &[Row], open: &[Key], from: Pos, to: Pos) -> String {
        let is_open = |k: Key| open.contains(&k);
        let entries = selection::reading(rows, &is_open);
        let never = |_: Key, _: &str| false;
        let tree = |_: Key, text: &str| parse_full(text);
        let shown = Shown {
            open: &is_open,
            output_open: &never,
            output_full: &never,
            tool_inputs: &|_, _| Vec::new(),
            tree: &tree,
        };
        let sel = Selection {
            anchor: from,
            head: to,
        };
        source_of(&sel, &entries, &shown)
    }

    fn whole(ordinal: usize, off: usize) -> Pos {
        Pos {
            ordinal,
            slot: Slot::WHOLE,
            off,
        }
    }

    fn reply(n: u64, text: &str) -> Row {
        Row::Prose {
            key: Key(n),
            text: text.to_owned(),
        }
    }

    #[test]
    fn the_source_keeps_the_markup_and_the_delimiters_the_owner_wrote() {
        let src = "see **bold** and \\(a+b\\) and [sup](src/s.rs:4)\n\n\\[x=y\\]";
        let rows = [reply(0, src)];
        let got = source_between(&rows, &[], whole(0, 0), whole(0, src.len()));
        assert_eq!(got, src);
        assert_ne!(all(src), src, "what is seen is not the source");
    }

    #[test]
    fn the_source_of_a_range_is_those_bytes_and_entries_are_a_blank_line_apart() {
        let rows = [
            Row::User {
                key: Key(0),
                text: "héllo **wörld**".to_owned(),
                steer: false,
                at_ms: None,
            },
            reply(2, "tail with `code` and more"),
        ];
        let got = source_between(&rows, &[], whole(0, "héllo ".len()), whole(1, 16));
        assert_eq!(got, "**wörld**\n\ntail with `code`");
    }

    #[test]
    fn a_closed_fold_gives_its_head_line_as_shown() {
        let rows = [
            reply(0, "before"),
            Row::Worked {
                key: Key::fold(Key(2)),
                secs: Some(192),
                rows: vec![reply(2, "hidden **inside**")],
            },
            reply(8, "after"),
        ];
        let range = (whole(0, 0), whole(3, 5));
        assert_eq!(
            source_between(&rows, &[], range.0, range.1),
            "before\n\nWorked for 3m 12s\n\nafter"
        );
        assert_eq!(
            source_between(&rows, &[Key::fold(Key(2))], range.0, range.1),
            "before\n\nWorked for 3m 12s\n\nhidden **inside**\n\nafter"
        );
    }
}
