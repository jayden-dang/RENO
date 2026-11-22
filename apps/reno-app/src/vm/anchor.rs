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

use reno_markdown::{Block, BlockTree, InlineRun};

use crate::links;
use crate::ui::markdown_view::link::file_target;

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
        }
    }

    #[cfg(test)]
    pub(crate) fn is_exact(&self) -> bool {
        self.shape == Shape::Exact
    }

    /// A formula, an image or a diagram: selected whole.
    pub(crate) fn is_atomic(&self) -> bool {
        self.shape == Shape::Whole
    }

    /// The source byte for the drawn byte `at` of this run. On an atomic run, the nearer edge.
    pub(crate) fn src_at(&self, at: usize) -> usize {
        match &self.shape {
            Shape::Exact => (self.src.start + at).min(self.src.end),
            Shape::Walk(pairs) => pairs
                .iter()
                .find(|(_, t)| *t >= at)
                .map_or(self.src.end, |(s, _)| *s),
            Shape::Whole => {
                if at * 2 < self.text.len().max(1) {
                    self.src.start
                } else {
                    self.src.end
                }
            }
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
            Shape::Whole => {
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
        if self.shape == Shape::Whole || self.copy != self.text {
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

/// The runs, code lines, formulas and diagrams of `tree` (parsed from `source`), in drawing
/// order, each with the source bytes it came from.
pub(crate) fn atoms(source: &str, tree: &BlockTree) -> Vec<Atom> {
    let mut b = Builder {
        source,
        out: Vec::new(),
        cursor: 0,
        end: source.len(),
        lead: Gap::Joined,
    };
    for top in &tree.blocks {
        b.cursor = top.range.start.min(source.len());
        b.end = top.range.end.min(source.len());
        b.block(&top.block, Gap::Blank);
    }
    b.out
}

struct Builder<'a> {
    source: &'a str,
    out: Vec<Atom>,
    /// Where in the source the next run is looked for.
    cursor: usize,
    /// The end of the top-level block being walked.
    end: usize,
    /// What goes before the next atom made.
    lead: Gap,
}

impl Builder<'_> {
    fn push(&mut self, mut atom: Atom) {
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
                });
            }
            Block::BlockQuote { children } => {
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
        if let Some(image) = &style.image {
            let src = self.find_image().unwrap_or(self.cursor..self.cursor);
            return Atom {
                copy: format!("[{}]", image.alt),
                text: image.alt.clone(),
                src,
                gap: Gap::Joined,
                shape: Shape::Whole,
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
            };
        }
        let (src, shape) = self.find_text(&run.text);
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
        }
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
    fn find_text(&mut self, text: &str) -> (Range<usize>, Shape) {
        if text.is_empty() {
            return (self.cursor..self.cursor, Shape::Exact);
        }
        let word_end = text.find(char::is_whitespace).unwrap_or(text.len()).max(1);
        let word = &text[..word_end.min(text.len())];
        let window = self.window();
        for (at, _) in window.match_indices(word) {
            if let Some((len, pairs)) = walk(&window[at..], text) {
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

/// Reads `text` off the front of `source`: a run of whitespace in the text matches a run of
/// whitespace in the source (a line break, with its quote or list margin), and a backslash
/// before punctuation is skipped. The source bytes used, with `(source, text)` offsets at each
/// character; `None` when the text is not there.
fn walk(source: &str, text: &str) -> Option<(usize, Vec<(usize, usize)>)> {
    let mut pairs = Vec::new();
    let mut at = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((t, c)) = chars.next() {
        pairs.push((at, t));
        if c.is_whitespace() {
            let run = source[at..]
                .find(|s: char| !s.is_whitespace() && s != '>')
                .unwrap_or(source.len() - at);
            if run == 0 {
                return None;
            }
            at += run;
            while chars.peek().is_some_and(|(_, n)| n.is_whitespace()) {
                chars.next();
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

#[cfg(test)]
mod tests {
    use super::*;
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
}
