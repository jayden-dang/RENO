//! Draws a `reno-markdown` [`BlockTree`] as GPUI elements.
//!
//! [`Streaming`] is the per-message cache: an [`IncrementalParser`] that re-parses only from the
//! last stable block when text is appended, and the display tree it yields (the stable blocks are
//! shared `Arc`s, only the tail is new). [`blocks`] turns a tree into elements. Syntax
//! highlighting of code blocks (`reno-syntax`) is not wired yet; code is plain monospace.

use gpui::{
    AnyElement, FontStyle, FontWeight, IntoElement, SharedString, StrikethroughStyle, StyledText,
    TextRun, UnderlineStyle, Window, div, prelude::*, px,
};
use reno_markdown::parser::TableAlign;
use reno_markdown::{Block, BlockTree, IncrementalParser, InlineRun};

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
        Block::Paragraph { runs } => inline(runs, theme, window).into_any_element(),
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
        Block::CodeBlock { code, .. } => code_block(code.trim_end_matches('\n'), theme),
        Block::Math { tex } => code_block(tex, theme),
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

fn code_block(code: &str, theme: &Theme) -> AnyElement {
    div()
        .w_full()
        .px(px(14.0))
        .py(px(10.0))
        .rounded(px(10.0))
        .bg(theme.surface)
        .border_1()
        .border_color(theme.hairline)
        .font_family(MONO)
        .text_size(px(12.5))
        .line_height(px(19.0))
        .child(SharedString::from(code.to_owned()))
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

/// One paragraph of styled runs.
fn inline(runs: &[InlineRun], theme: &Theme, window: &Window) -> StyledText {
    let base = window.text_style();
    let mut text = String::new();
    let mut spans = Vec::with_capacity(runs.len());
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
    StyledText::new(SharedString::from(text)).with_runs(spans)
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
