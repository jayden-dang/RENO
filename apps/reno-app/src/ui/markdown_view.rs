//! Draws a `reno-markdown` [`BlockTree`] as GPUI elements.
//!
//! [`Streaming`] is the per-message cache: an [`IncrementalParser`] that re-parses only from the
//! last stable block when text is appended, and the display tree it yields (the stable blocks are
//! shared `Arc`s, only the tail is new). [`blocks`] turns a tree into elements.
//!
//! Code blocks are highlighted with `reno-syntax` when the fence names a language it knows; the
//! result is cached per (language, code), so a repaint does not parse again. Links are clickable:
//! only `http`, `https` and `mailto` URLs open, and a link whose URL is still streaming never
//! does.

use std::cell::RefCell;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AnyElement, FontStyle, FontWeight, Hsla, InteractiveText, IntoElement, SharedString,
    StrikethroughStyle, StyledText, TextRun, UnderlineStyle, Window, div, prelude::*, px,
};
use reno_markdown::mend::PENDING_LINK_URL;
use reno_markdown::parser::TableAlign;
use reno_markdown::{Block, BlockTree, IncrementalParser, InlineRun};
use reno_syntax::{HighlightKind, HighlightRequest, HighlightSpan, HighlightedDocument};

use super::kit::MONO;
use super::theme::Theme;

/// One streaming message: its parser and the tree to draw.
#[derive(Debug, Default)]
pub(crate) struct Streaming {
    parser: IncrementalParser,
    tree: BlockTree,
}

impl Streaming {
    /// Sets the message text. An append re-parses only from the last stable block; anything else
    /// re-parses in full. Returns whether the text changed.
    pub(crate) fn set_text(&mut self, text: &str) -> bool {
        if self.parser.source() == text {
            return false;
        }
        self.parser.set_text(text);
        self.tree = self.parser.display_tree();
        true
    }

    pub(crate) fn tree(&self) -> &BlockTree {
        &self.tree
    }

    /// Bytes re-parsed by the last `set_text`: proof the work is O(tail).
    #[cfg(test)]
    pub(crate) fn last_parse_bytes(&self) -> usize {
        self.parser.last_parse_bytes()
    }
}

/// The text of a block run, for tests and accessibility.
#[cfg(test)]
pub(crate) fn plain(runs: &[InlineRun]) -> String {
    runs.iter().map(|r| r.text.as_str()).collect()
}

/// The elements of a whole tree, stacked.
pub(crate) fn blocks(tree: &BlockTree, theme: &Theme, window: &Window) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(10.0))
        .children(
            tree.blocks
                .iter()
                .map(|top| block(&top.block, theme, window)),
        )
        .into_any_element()
}

fn block(node: &Block, theme: &Theme, window: &Window) -> AnyElement {
    match node {
        Block::Paragraph { runs } => inline(runs, theme, window),
        Block::Heading { level, runs } => {
            let size = match level {
                1 => 20.0,
                2 => 17.5,
                3 => 15.5,
                _ => 14.5,
            };
            div()
                .text_size(px(size))
                .font_weight(FontWeight::SEMIBOLD)
                .pt(px(4.0))
                .child(inline(runs, theme, window))
                .into_any_element()
        }
        Block::CodeBlock { code, language } => code_block(
            code.trim_end_matches('\n'),
            language.as_deref(),
            theme,
            window,
        ),
        Block::Math { tex } => code_block(tex, None, theme, window),
        Block::BlockQuote { children } => div()
            .pl(px(12.0))
            .border_l_2()
            .border_color(theme.hairline)
            .text_color(theme.muted)
            .flex()
            .flex_col()
            .gap(px(8.0))
            .children(children.iter().map(|b| block(b, theme, window)))
            .into_any_element(),
        Block::List {
            ordered_start,
            items,
        } => div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .children(items.iter().enumerate().map(|(i, item)| {
                let marker = match ordered_start {
                    Some(start) => format!("{}.", start + i as u64),
                    None => "\u{2022}".to_owned(),
                };
                div()
                    .flex()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_none()
                            .w(px(20.0))
                            .text_color(theme.muted)
                            .child(marker),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(4.0))
                            .children(item.iter().map(|b| block(b, theme, window))),
                    )
            }))
            .into_any_element(),
        Block::Table {
            header,
            rows,
            align,
        } => table(header, rows, align, theme, window),
        Block::Rule => div()
            .h(px(1.0))
            .w_full()
            .bg(theme.hairline)
            .into_any_element(),
    }
}

/// What a code token looks like. Accent is not spent here (UI brief); tokens differ by weight and
/// by the semantic colours.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Token {
    Plain,
    Comment,
    Str,
    Number,
    Keyword,
    Type,
    Function,
}

pub(crate) fn token_of(kind: HighlightKind) -> Token {
    use HighlightKind as K;
    match kind {
        K::Comment => Token::Comment,
        K::String | K::StringSpecial | K::Escape | K::MarkupRaw => Token::Str,
        K::Number | K::Boolean | K::Constant => Token::Number,
        K::Keyword | K::Tag | K::MarkupHeading | K::MarkupStrong => Token::Keyword,
        K::Type | K::TypeBuiltin | K::Constructor => Token::Type,
        K::Function | K::FunctionBuiltin | K::Macro => Token::Function,
        _ => Token::Plain,
    }
}

fn token_look(theme: &Theme, token: Token) -> (Hsla, FontWeight) {
    match token {
        Token::Plain | Token::Function => (theme.text, FontWeight::NORMAL),
        Token::Comment => (theme.faint, FontWeight::NORMAL),
        Token::Str => (theme.success, FontWeight::NORMAL),
        Token::Number => (theme.warning, FontWeight::NORMAL),
        Token::Keyword => (theme.text, FontWeight::SEMIBOLD),
        Token::Type => (theme.muted, FontWeight::MEDIUM),
    }
}

/// Splits a line into consecutive `(byte range, token)` pieces that cover all of it. Overlapping
/// or out-of-range spans are skipped, so the pieces always tile the line.
pub(crate) fn line_pieces(line: &str, spans: &[HighlightSpan]) -> Vec<(Range<usize>, Token)> {
    let mut sorted: Vec<&HighlightSpan> = spans.iter().collect();
    sorted.sort_by_key(|span| (span.range.start, span.range.end));
    let mut pieces = Vec::new();
    let mut at = 0;
    for span in sorted {
        let Range { start, end } = span.range;
        if start < at
            || end > line.len()
            || start >= end
            || !line.is_char_boundary(start)
            || !line.is_char_boundary(end)
        {
            continue;
        }
        if start > at {
            pieces.push((at..start, Token::Plain));
        }
        pieces.push((start..end, token_of(span.kind)));
        at = end;
    }
    if at < line.len() {
        pieces.push((at..line.len(), Token::Plain));
    }
    pieces
}

/// Most distinct code blocks kept highlighted; the cache is emptied when it grows past this.
const HIGHLIGHT_CACHE: usize = 128;
/// Code longer than this is shown plain: highlighting runs on every change of a streaming block.
const HIGHLIGHT_MAX_BYTES: usize = 64 * 1024;

type Cache = HashMap<(String, String), Option<Rc<HighlightedDocument>>>;

thread_local! {
    static HIGHLIGHTS: RefCell<Cache> = RefCell::new(HashMap::new());
}

/// The highlighted lines of `code`, or `None` when the fence names no known language.
pub(crate) fn highlighted(code: &str, language: Option<&str>) -> Option<Rc<HighlightedDocument>> {
    let tag = language?.trim();
    if tag.is_empty() || code.len() > HIGHLIGHT_MAX_BYTES {
        return None;
    }
    let key = (tag.to_owned(), code.to_owned());
    HIGHLIGHTS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(hit) = cache.get(&key) {
            return hit.clone();
        }
        if cache.len() >= HIGHLIGHT_CACHE {
            cache.clear();
        }
        let doc = reno_syntax::highlight(HighlightRequest {
            source: code,
            path: None,
            fence_tag: Some(tag),
        })
        .ok()
        .map(Rc::new);
        cache.insert(key, doc.clone());
        doc
    })
}

fn code_block(code: &str, language: Option<&str>, theme: &Theme, window: &Window) -> AnyElement {
    let frame = div()
        .w_full()
        .px(px(14.0))
        .py(px(10.0))
        .rounded(px(10.0))
        .bg(theme.surface)
        .border_1()
        .border_color(theme.hairline)
        .font_family(MONO)
        .text_size(px(12.5))
        .line_height(px(19.0));
    let doc = highlighted(code, language);
    let lines: Vec<&str> = code.split('\n').collect();
    let Some(doc) = doc.filter(|d| d.lines.len() == lines.len()) else {
        return frame
            .child(SharedString::from(code.to_owned()))
            .into_any_element();
    };
    let base = window.text_style();
    frame
        .flex()
        .flex_col()
        .children(lines.iter().zip(doc.lines.iter()).map(|(line, spans)| {
            if line.is_empty() {
                // An empty line still takes its height.
                return div().h(px(19.0)).into_any_element();
            }
            let runs: Vec<TextRun> = line_pieces(line, spans)
                .into_iter()
                .map(|(range, token)| {
                    let (color, weight) = token_look(theme, token);
                    let mut font = base.font();
                    font.family = MONO.into();
                    font.weight = weight;
                    TextRun {
                        len: range.len(),
                        font,
                        color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }
                })
                .collect();
            StyledText::new(SharedString::from((*line).to_owned()))
                .with_runs(runs)
                .into_any_element()
        }))
        .into_any_element()
}

fn table(
    header: &[Vec<InlineRun>],
    rows: &[Vec<Vec<InlineRun>>],
    align: &[TableAlign],
    theme: &Theme,
    window: &Window,
) -> AnyElement {
    let row = |cells: &[Vec<InlineRun>], head: bool| {
        div()
            .flex()
            .border_b_1()
            .border_color(theme.hairline)
            .when(head, |r| r.font_weight(FontWeight::MEDIUM))
            .children(cells.iter().enumerate().map(|(i, cell)| {
                let cell_box = div().flex_1().min_w_0().px(px(10.0)).py(px(6.0));
                let cell_box = match align.get(i) {
                    Some(TableAlign::Center) => cell_box.text_center(),
                    Some(TableAlign::Right) => cell_box.text_right(),
                    _ => cell_box,
                };
                cell_box.child(inline(cell, theme, window))
            }))
    };
    div()
        .w_full()
        .rounded(px(10.0))
        .border_1()
        .border_color(theme.hairline)
        .bg(theme.surface)
        .overflow_hidden()
        .child(row(header, true))
        .children(rows.iter().map(|r| row(r, false)))
        .into_any_element()
}

/// Whether a link may be opened from the transcript: web and mail links only. Anything else
/// (`file:`, custom schemes) could launch another program, and a link still streaming has no
/// destination yet.
pub(crate) fn openable(url: &str) -> bool {
    if url == PENDING_LINK_URL {
        return false;
    }
    let lower = url.trim().to_ascii_lowercase();
    ["http://", "https://", "mailto:"]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
}

/// One paragraph of styled runs; links that may be opened are clickable.
fn inline(runs: &[InlineRun], theme: &Theme, window: &Window) -> AnyElement {
    let base = window.text_style();
    let mut text = String::new();
    let mut spans = Vec::with_capacity(runs.len());
    let mut links: Vec<(Range<usize>, String)> = Vec::new();
    for run in runs {
        if run.text.is_empty() {
            continue;
        }
        let style = &run.style;
        let mut font = base.font();
        if style.bold {
            font.weight = FontWeight::SEMIBOLD;
        }
        if style.italic {
            font.style = FontStyle::Italic;
        }
        if style.code || style.math {
            font.family = MONO.into();
        }
        let link = style.link.is_some();
        if let Some(url) = style.link.as_deref().filter(|u| openable(u)) {
            links.push((text.len()..text.len() + run.text.len(), url.to_owned()));
        }
        spans.push(TextRun {
            len: run.text.len(),
            font,
            color: if link { theme.accent } else { base.color },
            background_color: (style.code || style.math).then(|| theme.raised.opacity(0.55)),
            underline: link.then(|| UnderlineStyle {
                thickness: px(1.0),
                color: Some(theme.accent),
                wavy: false,
            }),
            strikethrough: style.strikethrough.then(|| StrikethroughStyle {
                thickness: px(1.0),
                color: Some(base.color),
            }),
        });
        text.push_str(&run.text);
    }
    let styled = StyledText::new(SharedString::from(text.clone())).with_runs(spans);
    if links.is_empty() {
        return styled.into_any_element();
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    let (ranges, urls): (Vec<_>, Vec<_>) = links.into_iter().unzip();
    InteractiveText::new(("md-text", hasher.finish() as usize), styled)
        .on_click(ranges, move |ix, _, cx| {
            if let Some(url) = urls.get(ix) {
                cx.open_url(url);
            }
        })
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn first_paragraph(stream: &Streaming) -> String {
        match &stream.tree().blocks[0].block {
            Block::Paragraph { runs } => plain(runs),
            other => panic!("expected a paragraph, got {other:?}"),
        }
    }

    #[test]
    fn only_web_and_mail_links_open() {
        assert!(openable("https://example.org/a"));
        assert!(openable("HTTP://example.org"));
        assert!(openable("mailto:a@example.org"));
        assert!(!openable("file:///etc/passwd"));
        assert!(!openable("javascript:alert(1)"));
        assert!(!openable("zed://x"));
        assert!(
            !openable(PENDING_LINK_URL),
            "a link still streaming is not clickable"
        );
        assert!(!openable(""));
    }

    #[test]
    fn rust_code_is_highlighted_and_cached() {
        let code = "fn main() {\n    let n = 1; // one\n}";
        let doc = highlighted(code, Some("rust")).expect("rust is bundled");
        assert_eq!(doc.lines.len(), 3);
        let kinds: Vec<Token> = line_pieces("fn main() {", &doc.lines[0])
            .into_iter()
            .map(|(_, t)| t)
            .collect();
        assert!(kinds.contains(&Token::Keyword), "{kinds:?}");
        let second = line_pieces("    let n = 1; // one", &doc.lines[1]);
        assert!(second.iter().any(|(_, t)| *t == Token::Comment));
        assert!(second.iter().any(|(_, t)| *t == Token::Number));
        // The same code again is the same allocation.
        let again = highlighted(code, Some("rust")).unwrap();
        assert!(Rc::ptr_eq(&doc, &again));
    }

    #[test]
    fn unknown_or_missing_languages_stay_plain() {
        assert!(highlighted("x", None).is_none());
        assert!(highlighted("x", Some("klingon")).is_none());
        assert!(highlighted("x", Some("  ")).is_none());
    }

    #[test]
    fn pieces_always_tile_the_line() {
        let spans = vec![
            HighlightSpan {
                range: 2..4,
                kind: HighlightKind::Keyword,
            },
            // Overlaps the previous one: skipped.
            HighlightSpan {
                range: 3..6,
                kind: HighlightKind::String,
            },
            // Out of range: skipped.
            HighlightSpan {
                range: 8..99,
                kind: HighlightKind::Number,
            },
        ];
        let pieces = line_pieces("abcdefgh", &spans);
        let total: usize = pieces.iter().map(|(r, _)| r.len()).sum();
        assert_eq!(total, 8);
        assert_eq!(pieces[0], (0..2, Token::Plain));
        assert_eq!(pieces[1], (2..4, Token::Keyword));
        assert_eq!(pieces[2], (4..8, Token::Plain));
        // Multi-byte text is split only on character boundaries.
        let pieces = line_pieces(
            "é1",
            &[HighlightSpan {
                range: 1..2,
                kind: HighlightKind::Number,
            }],
        );
        assert_eq!(pieces, vec![(0..3, Token::Plain)]);
    }

    #[test]
    fn appends_reparse_only_the_tail() {
        let mut stream = Streaming::default();
        let mut text = String::new();
        // Many paragraphs, then keep appending to the last one.
        for i in 0..200 {
            text.push_str(&format!("Paragraph {i} with some words in it.\n\n"));
        }
        assert!(stream.set_text(&text));
        let total = text.len();
        text.push_str("tail");
        assert!(stream.set_text(&text));
        assert!(
            stream.last_parse_bytes() < total / 10,
            "re-parsed {} of {} bytes",
            stream.last_parse_bytes(),
            total
        );
        text.push_str(" more");
        assert!(stream.set_text(&text));
        assert!(stream.last_parse_bytes() < total / 10);
        assert!(!stream.set_text(&text), "the same text is a no-op");
    }

    #[test]
    fn hanging_markers_are_mended_while_streaming() {
        let mut stream = Streaming::default();
        stream.set_text("A **bold");
        assert_eq!(first_paragraph(&stream), "A bold");
        stream.set_text("A **bold** move");
        assert_eq!(first_paragraph(&stream), "A bold move");
    }

    #[test]
    fn a_rewrite_resets_the_parse() {
        let mut stream = Streaming::default();
        stream.set_text("one");
        stream.set_text("two");
        assert_eq!(first_paragraph(&stream), "two");
    }
}
