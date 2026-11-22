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
    AnyElement, App, FontStyle, FontWeight, HighlightStyle, Hsla, InteractiveText, IntoElement,
    Pixels, RenderOnce, SharedString, StrikethroughStyle, StyledText, TextRun, UnderlineStyle,
    Window, div, prelude::*, px,
};
use reno_client::Client;
use reno_markdown::mend::PENDING_LINK_URL;
use reno_markdown::parser::TableAlign;
use reno_markdown::{Block, BlockTree, IncrementalParser, InlineRun};
use reno_proto::RunId;
use reno_syntax::{HighlightKind, HighlightRequest, HighlightSpan, HighlightedDocument};

use self::chrome::Card;
use self::image::ImageHost;
use self::link::{Action, LinkPolicy};
use self::picture::{Overlay, Overlays, Picture};
use self::select::{Map, Registry, Seg, Track, highlight_runs};
use super::kit::MONO;
use super::theme::Theme;
use crate::transcript::Key;
use crate::vm::anchor::Atom;
use crate::vm::selection::Slot;

pub(crate) mod chrome;
pub(crate) mod image;
pub(crate) mod link;
pub(crate) mod lru;
pub(crate) mod math;
pub(crate) mod mermaid;
pub(crate) mod picture;
pub(crate) mod select;
pub(crate) mod visual;

pub(crate) use self::link::FileLinks;

/// One streaming message: its parser and the tree to draw.
#[derive(Debug, Default)]
pub(crate) struct Streaming {
    parser: IncrementalParser,
    tree: BlockTree,
    /// The drawn runs lined up with the source, made when selection first asks for them.
    atoms: std::cell::RefCell<Option<Rc<Vec<Atom>>>>,
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
        self.atoms.take();
        true
    }

    /// The drawn runs, code lines, formulas and diagrams of the tree, with their source.
    pub(crate) fn atoms(&self) -> Rc<Vec<Atom>> {
        self.atoms
            .borrow_mut()
            .get_or_insert_with(|| {
                Rc::new(crate::vm::anchor::atoms(self.parser.source(), &self.tree))
            })
            .clone()
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

/// What the renderer needs from whoever shows the markdown: how links behave and where a
/// worktree image is read from (TRANSCRIPT-8.2, 11.1).
#[derive(Clone, Debug)]
pub(crate) struct Host {
    pub(crate) links: LinkPolicy,
    /// `None`: images are not drawn; an image is its alt text, as the Files tab's preview always
    /// showed it.
    pub(crate) images: Option<ImageHost>,
    /// The text is still arriving: its last block may be half written, so a diagram there is
    /// shown as code until the next block starts (it would otherwise be drawn again for every
    /// chunk of its source).
    pub(crate) streaming: bool,
    /// Find hits to paint in the text (TRANSCRIPT-3.5).
    pub(crate) marks: Option<Rc<Marks>>,
    /// Where the transcript tracks painted text for selection (TRANSCRIPT-2.1).
    pub(crate) select: Option<Rc<SelectHost>>,
}

/// What to paint over a reply: the needle, and which of the reply's drawn matches is the one the
/// find bar is on (counted from zero in drawing order; `None` when it is in another row).
#[derive(Debug)]
pub(crate) struct Marks {
    pub(crate) needle: String,
    pub(crate) current: Option<usize>,
}

/// A reply drawn for the transcript's selection.
pub(crate) struct SelectHost {
    pub(crate) registry: Registry,
    pub(crate) key: Key,
    /// The reply's runs lined up with its source.
    pub(crate) atoms: Rc<Vec<Atom>>,
    /// The selected bytes of the reply's source.
    pub(crate) selected: Option<Range<usize>>,
    pub(crate) style: HighlightStyle,
}

impl std::fmt::Debug for SelectHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SelectHost")
            .field("key", &self.key)
            .field("selected", &self.selected)
            .finish_non_exhaustive()
    }
}

impl Host {
    /// The Files tab's preview: the links it always had, images as text (8.5).
    pub(crate) fn preview() -> Self {
        Self {
            links: LinkPolicy::Preview,
            images: None,
            streaming: false,
            marks: None,
            select: None,
        }
    }

    /// The transcript: web links through the link opener, worktree files through `files`,
    /// worktree images read for `run` through `client`. With no run or client a worktree image
    /// shows "Image not loaded".
    pub(crate) fn transcript(
        run: Option<RunId>,
        client: Option<Client>,
        files: Option<Rc<dyn FileLinks>>,
    ) -> Self {
        Self {
            links: LinkPolicy::Transcript { files },
            images: Some(ImageHost { run, client }),
            streaming: false,
            marks: None,
            select: None,
        }
    }

    /// Says whether the text is still arriving (see [`Host::streaming`]).
    pub(crate) fn while_streaming(mut self, streaming: bool) -> Self {
        self.streaming = streaming;
        self
    }

    /// Paints the find hits `marks` names.
    pub(crate) fn marked(mut self, marks: Option<Rc<Marks>>) -> Self {
        self.marks = marks;
        self
    }

    /// Tracks the painted text for selection and paints what is selected.
    pub(crate) fn selectable(mut self, select: Option<Rc<SelectHost>>) -> Self {
        self.select = select;
        self
    }
}

/// Bumped each time something the renderers load in the background lands (an image, a diagram)
/// or a block changes height (wrap): a list that caches row heights compares it between frames
/// and measures the rows again when it moved.
#[derive(Default)]
struct Settled(u64);

impl gpui::Global for Settled {}

/// Records that a renderer's output changed size or arrived, and redraws.
pub(crate) fn settled(cx: &mut App) {
    cx.default_global::<Settled>().0 += 1;
    cx.refresh_windows();
}

/// See [`Settled`].
pub(crate) fn settled_generation(cx: &App) -> u64 {
    cx.try_global::<Settled>().map_or(0, |s| s.0)
}

/// Everything one draw of a tree reads, taken once at its start.
struct Env<'a> {
    theme: &'a Theme,
    host: &'a Host,
    window: &'a Window,
    base: gpui::TextStyle,
    scale: f32,
    /// Whether math is typeset, and its size in logical pixels per em.
    math: Option<f32>,
    /// How many framed blocks this draw has made so far (their element ids differ by it).
    cards: std::cell::Cell<usize>,
    /// How many runs and lines this draw has taken from the reply's atoms, and how many find
    /// hits it has numbered: both in drawing order.
    atom_at: std::cell::Cell<usize>,
    hit_at: std::cell::Cell<usize>,
}

impl Env<'_> {
    /// The place of the next `n` atoms in the reply's list.
    fn take_atoms(&self, n: usize) -> usize {
        self.atom_at.replace(self.atom_at.get() + n)
    }

    fn next_card(&self) -> usize {
        self.cards.replace(self.cards.get() + 1)
    }
}

/// A tree drawn for a host. A component, so that it reads the app (settings, the draft switch,
/// the renderers' caches) when it is drawn and the caller needs no `App` to make one.
#[derive(IntoElement)]
struct Markdown {
    tree: BlockTree,
    theme: Theme,
    host: Host,
}

impl RenderOnce for Markdown {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let base = window.text_style();
        let (typeset, percent) = crate::prefs::math(cx);
        let size = base.font_size.to_pixels(window.rem_size());
        let env = Env {
            theme: &self.theme,
            host: &self.host,
            window,
            scale: window.scale_factor(),
            math: typeset.then(|| f32::from(size) * f32::from(percent) / 100.0),
            cards: std::cell::Cell::new(0),
            atom_at: std::cell::Cell::new(0),
            hit_at: std::cell::Cell::new(0),
            base,
        };
        let last = self.tree.blocks.len().saturating_sub(1);
        let blocks = self
            .tree
            .blocks
            .iter()
            .enumerate()
            .map(|(at, top)| match &top.block {
                Block::CodeBlock { code, language }
                    if self.host.streaming && at == last && is_mermaid(language) =>
                {
                    env.take_atoms(1);
                    code_block(
                        code.trim_end_matches('\n'),
                        language.as_deref(),
                        None,
                        &env,
                        cx,
                    )
                }
                other => block(other, &env, cx),
            })
            .collect::<Vec<_>>();
        // The draw took exactly the atoms the anchors made for this tree, in the same order:
        // selection maps painted text to source through them.
        if let Some(select) = &self.host.select {
            debug_assert_eq!(
                env.atom_at.get(),
                select.atoms.len(),
                "atoms and drawing differ"
            );
        }
        div().flex().flex_col().gap(px(10.0)).children(blocks)
    }
}

fn is_mermaid(language: &Option<String>) -> bool {
    language.as_deref().map(str::trim) == Some("mermaid")
}

/// The elements of a whole tree, stacked, for `host`.
pub(crate) fn blocks_with(tree: &BlockTree, theme: &Theme, host: Host) -> AnyElement {
    Markdown {
        tree: tree.clone(),
        theme: *theme,
        host,
    }
    .into_any_element()
}

fn block(node: &Block, env: &Env, cx: &mut App) -> AnyElement {
    let theme = env.theme;
    match node {
        Block::Paragraph { runs } => paragraph(runs, env, cx),
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
                .child(inline(runs, env, cx))
                .into_any_element()
        }
        Block::CodeBlock { code, language } => {
            let code = code.trim_end_matches('\n');
            if is_mermaid(language) {
                let at = env.take_atoms(1);
                diagram(code, at, env, cx)
            } else {
                let at = env.take_atoms(code.split('\n').count());
                code_block(code, language.as_deref(), Some(at), env, cx)
            }
        }
        Block::Math { tex } => {
            let at = env.take_atoms(1);
            display_math(tex, at, env, cx)
        }
        Block::BlockQuote { children } => div()
            .pl(px(12.0))
            .border_l_2()
            .border_color(theme.hairline)
            .text_color(theme.muted)
            .flex()
            .flex_col()
            .gap(px(8.0))
            .children(
                children
                    .iter()
                    .map(|b| block(b, env, cx))
                    .collect::<Vec<_>>(),
            )
            .into_any_element(),
        Block::List {
            ordered_start,
            items,
        } => div()
            .flex()
            .flex_col()
            .gap(px(4.0))
            .children(
                items
                    .iter()
                    .enumerate()
                    .map(|(i, item)| {
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
                                    .children(
                                        item.iter().map(|b| block(b, env, cx)).collect::<Vec<_>>(),
                                    ),
                            )
                    })
                    .collect::<Vec<_>>(),
            )
            .into_any_element(),
        Block::Table {
            header,
            rows,
            align,
        } => table(header, rows, align, env, cx),
        Block::Rule => div()
            .h(px(1.0))
            .w_full()
            .bg(theme.hairline)
            .into_any_element(),
    }
}

/// A paragraph. Where the host draws images, an image splits it: the text before, the picture,
/// the text after.
fn paragraph(runs: &[InlineRun], env: &Env, cx: &mut App) -> AnyElement {
    let first = env.take_atoms(runs.len());
    let Some(images) = &env.host.images else {
        return inline_at(runs, first, env, cx);
    };
    if !runs.iter().any(|r| r.style.image.is_some()) {
        return inline_at(runs, first, env, cx);
    }
    let mut parts: Vec<AnyElement> = Vec::new();
    let mut text: Vec<InlineRun> = Vec::new();
    // Where in the reply's atoms the text collected so far starts.
    let mut from = first;
    for (n, run) in runs.iter().enumerate() {
        let Some(shown) = &run.style.image else {
            text.push(run.clone());
            continue;
        };
        if !text.is_empty() {
            let part = std::mem::take(&mut text);
            parts.push(inline_at(&part, from, env, cx));
        }
        from = first + n + 1;
        parts.push(image::view(
            &shown.alt,
            &shown.source,
            shown.link.as_deref(),
            images,
            &env.host.links,
            env.theme,
            cx,
        ));
    }
    if !text.is_empty() {
        parts.push(inline_at(&text, from, env, cx));
    }
    div()
        .flex()
        .flex_col()
        .items_start()
        .gap(px(6.0))
        .children(parts)
        .into_any_element()
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

/// The text runs of one highlighted code line, in the mono face; they cover all of `line`.
pub(crate) fn line_runs(
    line: &str,
    spans: &[HighlightSpan],
    theme: &Theme,
    base: &gpui::TextStyle,
) -> Vec<TextRun> {
    line_pieces(line, spans)
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
        .collect()
}

/// The highlights to put over one laid-out text: the selection first, then the find hits (which
/// win where they meet). `segs` says which atoms the text is, `len` its length.
fn layers(
    env: &Env,
    segs: &[Seg],
    len: usize,
    finds: &[(Range<usize>, bool)],
) -> Vec<(Range<usize>, HighlightStyle)> {
    let mut out = Vec::new();
    if let Some(select) = &env.host.select
        && let Some(selected) = &select.selected
    {
        let map = Map::Atoms {
            atoms: select.atoms.clone(),
            segs: segs.to_vec(),
        };
        if let Some(range) = map.layout_range(selected, len, false) {
            out.push((range, select.style));
        }
    }
    if env.host.marks.is_some() {
        out.extend(
            finds.iter().map(|(range, current)| {
                (range.clone(), super::find::hit_style(env.theme, *current))
            }),
        );
    }
    out
}

/// The find hits in `text`, which starts at byte `base` of its laid-out text, numbered in drawing
/// order across the reply, with whether each is the one the bar is on.
fn find_hits(env: &Env, text: &str, base: usize) -> Vec<(Range<usize>, bool)> {
    let Some(marks) = &env.host.marks else {
        return Vec::new();
    };
    crate::vm::find::matches(text, &marks.needle)
        .into_iter()
        .map(|range| {
            let n = env.hit_at.replace(env.hit_at.get() + 1);
            (
                base + range.start..base + range.end,
                marks.current == Some(n),
            )
        })
        .collect()
}

/// Files a painted text with the transcript's selection, when the host tracks one.
fn tracked(env: &Env, element: AnyElement, layout: gpui::TextLayout, segs: Vec<Seg>) -> AnyElement {
    match &env.host.select {
        Some(select) if !segs.is_empty() => Track::new(
            element,
            layout,
            (
                select.key,
                Slot::WHOLE,
                Map::Atoms {
                    atoms: select.atoms.clone(),
                    segs,
                },
            ),
            select.registry.clone(),
        )
        .into_any_element(),
        _ => element,
    }
}

/// Whether the selection touches atom `at` (a formula or diagram, which is selected whole).
fn atom_selected(env: &Env, at: usize) -> bool {
    let Some(select) = &env.host.select else {
        return false;
    };
    let (Some(selected), Some(atom)) = (&select.selected, select.atoms.get(at)) else {
        return false;
    };
    if atom.src.is_empty() {
        selected.start <= atom.src.start && atom.src.start < selected.end
    } else {
        selected.start < atom.src.end && atom.src.start < selected.end
    }
}

/// The lines of `code`, highlighted when the fence names a language this build knows. `atoms` is
/// where the lines are in the reply's atoms (`None` for source shown in place of a diagram).
fn code_lines(
    code: &str,
    language: Option<&str>,
    atoms: Option<usize>,
    env: &Env,
) -> Vec<AnyElement> {
    let theme = env.theme;
    let lines: Vec<&str> = code.split('\n').collect();
    let doc = highlighted(code, language).filter(|d| d.lines.len() == lines.len());
    lines
        .iter()
        .enumerate()
        .map(|(ix, line)| {
            if line.is_empty() {
                // An empty line still takes its height.
                return div().h(px(19.0)).into_any_element();
            }
            let runs = match &doc {
                Some(doc) => line_runs(line, &doc.lines[ix], theme, &env.base),
                None => {
                    let mut font = env.base.font();
                    font.family = MONO.into();
                    vec![TextRun {
                        len: line.len(),
                        font,
                        color: env.base.color,
                        background_color: None,
                        underline: None,
                        strikethrough: None,
                    }]
                }
            };
            let segs: Vec<Seg> = atoms
                .map(|base| Seg {
                    layout: 0..line.len(),
                    atom: base + ix,
                })
                .into_iter()
                .collect();
            let finds = find_hits(env, line, 0);
            let runs = highlight_runs(runs, &layers(env, &segs, line.len(), &finds));
            let styled = StyledText::new(SharedString::from((*line).to_owned())).with_runs(runs);
            let layout = styled.layout().clone();
            tracked(env, styled.into_any_element(), layout, segs)
        })
        .collect()
}

/// The code area of a frame: mono, with its lines wrapped or scrolling sideways.
fn code_body(
    code: &str,
    (language, atoms): (Option<&str>, Option<usize>),
    wrapped: bool,
    key: u64,
    env: &Env,
) -> AnyElement {
    let area = div()
        .px(px(14.0))
        .py(px(10.0))
        .font_family(MONO)
        .text_size(px(12.5))
        .line_height(px(19.0))
        .flex()
        .flex_col()
        .children(code_lines(code, language, atoms, env));
    if wrapped {
        area.w_full().min_w_0().into_any_element()
    } else {
        div()
            .id(("md-code-scroll", key as usize))
            .w_full()
            .overflow_x_scroll()
            .child(area.flex_none().min_w_full().whitespace_nowrap())
            .into_any_element()
    }
}

/// A code block in its frame, with Copy and wrap (TRANSCRIPT-7.1, 7.2).
fn code_block(
    code: &str,
    language: Option<&str>,
    atoms: Option<usize>,
    env: &Env,
    cx: &mut App,
) -> AnyElement {
    let key = chrome::key_of(language, code);
    let ordinal = env.next_card();
    let wrapped = chrome::wraps(key, cx);
    chrome::card(
        &Card {
            kind: chrome::Kind::Code,
            copy: code,
            key,
            wrappable: true,
            wrapped,
            ordinal,
        },
        env.theme,
        code_body(code, (language, atoms), wrapped, key ^ ordinal as u64, env),
    )
}

/// TeX source in code style, with no controls: what math is when it is not typeset.
fn source_block(tex: &str, env: &Env) -> AnyElement {
    div()
        .w_full()
        .px(px(14.0))
        .py(px(10.0))
        .rounded(px(10.0))
        .bg(env.theme.surface)
        .border_1()
        .border_color(env.theme.hairline)
        .font_family(MONO)
        .text_size(px(12.5))
        .line_height(px(19.0))
        .child(SharedString::from(tex.to_owned()))
        .into_any_element()
}

/// Display math is drawn this much larger than the text around it (then by the owner's math size),
/// so its limits and fractions stay easy to read; inline math is the text's own size.
const DISPLAY_MATH_EM: f32 = 1.2;

/// Display math on its own line, centred; its source when typesetting is off or fails
/// (TRANSCRIPT-9.1, 9.3, 9.5).
fn display_math(tex: &str, atom: usize, env: &Env, cx: &mut App) -> AnyElement {
    let Some(size) = env.math else {
        return source_block(tex, env);
    };
    match math::typeset(
        tex,
        true,
        size * DISPLAY_MATH_EM,
        env.theme.text,
        env.scale,
        cx,
    ) {
        Some(set) => div()
            .debug_selector(|| "md-math-display".to_owned())
            .w_full()
            .flex()
            .py(px(4.0))
            .when(atom_selected(env, atom), |d| {
                d.rounded(px(6.0)).bg(env
                    .host
                    .select
                    .as_ref()
                    .and_then(|s| s.style.background_color)
                    .unwrap_or(env.theme.raised))
            })
            .child(Picture {
                image: set.image.clone(),
                width: set.width,
                height: set.height(),
                min_scale: 0.0,
            })
            .into_any_element(),
        None => source_block(tex, env),
    }
}

/// The least share of its size a diagram is drawn at (TRANSCRIPT-10.4): it is fitted to the
/// column whole, and only one that would need its labels smaller than this to fit scrolls.
const DIAGRAM_MIN_SCALE: f32 = 0.75;

/// A drawn diagram: fit to the column, but never smaller than three quarters of its size, so its
/// labels stay legible; a wider diagram scrolls sideways.
pub(crate) fn diagram_picture(diagram: &mermaid::Diagram, key: u64) -> AnyElement {
    div()
        .debug_selector(|| "md-diagram".to_owned())
        .id(("md-diagram-scroll", key as usize))
        .w_full()
        .overflow_x_scroll()
        .child(div().w_full().flex().p(px(12.0)).child(Picture {
            image: diagram.image.clone(),
            width: diagram.width,
            height: diagram.height,
            min_scale: DIAGRAM_MIN_SCALE,
        }))
        .into_any_element()
}

/// A Mermaid block: the drawing in the diagram frame, its source with "Rendering\u{2026}" while it is
/// drawn, a plain code block when it cannot be (TRANSCRIPT-10.1, 10.3).
fn diagram(source: &str, atom: usize, env: &Env, cx: &mut App) -> AnyElement {
    let key = chrome::key_of(Some("mermaid"), source);
    let ordinal = env.next_card();
    let selected = atom_selected(env, atom);
    let frame = |body: AnyElement| {
        let card = chrome::card(
            &Card {
                kind: chrome::Kind::Diagram,
                copy: source,
                key,
                wrappable: false,
                wrapped: false,
                ordinal,
            },
            env.theme,
            body,
        );
        // A selected diagram is lifted whole: it has no text to paint over.
        match (&env.host.select, selected) {
            (Some(select), true) => div()
                .rounded(px(10.0))
                .bg(select.style.background_color.unwrap_or(env.theme.raised))
                .p(px(3.0))
                .child(card)
                .into_any_element(),
            _ => card,
        }
    };
    match mermaid::drawing(source, env.theme, env.scale, cx) {
        mermaid::Drawing::Ready(diagram) => frame(diagram_picture(&diagram, key ^ ordinal as u64)),
        mermaid::Drawing::Rendering => frame(
            div()
                .child(code_body(
                    source,
                    (None, None),
                    false,
                    key ^ ordinal as u64,
                    env,
                ))
                .child(
                    div()
                        .px(px(14.0))
                        .pb(px(8.0))
                        .text_size(px(12.0))
                        .text_color(env.theme.faint)
                        .debug_selector(|| "md-diagram-rendering".to_owned())
                        .child("Rendering\u{2026}"),
                )
                .into_any_element(),
        ),
        mermaid::Drawing::Failed => code_block(source, Some("mermaid"), None, env, cx),
    }
}

fn table(
    header: &[Vec<InlineRun>],
    rows: &[Vec<Vec<InlineRun>>],
    align: &[TableAlign],
    env: &Env,
    cx: &mut App,
) -> AnyElement {
    let theme = env.theme;
    let mut row = |cells: &[Vec<InlineRun>], head: bool| {
        div()
            .flex()
            .border_b_1()
            .border_color(theme.hairline)
            .when(head, |r| r.font_weight(FontWeight::MEDIUM))
            .children(
                cells
                    .iter()
                    .enumerate()
                    .map(|(i, cell)| {
                        let cell_box = div().flex_1().min_w_0().px(px(10.0)).py(px(6.0));
                        let cell_box = match align.get(i) {
                            Some(TableAlign::Center) => cell_box.text_center(),
                            Some(TableAlign::Right) => cell_box.text_right(),
                            _ => cell_box,
                        };
                        cell_box.child(inline(cell, env, cx))
                    })
                    .collect::<Vec<_>>(),
            )
    };
    let head = row(header, true);
    let body: Vec<_> = rows.iter().map(|r| row(r, false)).collect();
    div()
        .w_full()
        .rounded(px(10.0))
        .border_1()
        .border_color(theme.hairline)
        .bg(theme.surface)
        .overflow_hidden()
        .child(head)
        .children(body)
        .into_any_element()
}

/// The advance of a no-break space in `font`: the unit inline math reserves its width in.
fn gap_unit(font: &gpui::Font, size: Pixels, window: &Window) -> f32 {
    let system = window.text_system();
    let id = system.resolve_font(font);
    system
        .advance(id, size, '\u{a0}')
        .map(|advance| f32::from(advance.width))
        .ok()
        .filter(|w| *w > 0.5)
        .unwrap_or_else(|| f32::from(size) * 0.25)
}

/// One paragraph of styled runs. A link that the host's policy opens is clickable; one it does
/// not is plain text. Inline math reserves a gap of no-break spaces in the text and is painted
/// over it, its baseline on the text's.
fn inline(runs: &[InlineRun], env: &Env, cx: &mut App) -> AnyElement {
    let first = env.take_atoms(runs.len());
    inline_at(runs, first, env, cx)
}

/// [`inline`] for runs that are atoms `first`, `first + 1`, ... of the reply.
fn inline_at(runs: &[InlineRun], first: usize, env: &Env, cx: &mut App) -> AnyElement {
    let theme = env.theme;
    let base = &env.base;
    let policy = &env.host.links;
    let font_size = base.font_size.to_pixels(env.window.rem_size());
    let mut text = String::new();
    let mut spans = Vec::with_capacity(runs.len());
    let mut links: Vec<(Range<usize>, Action)> = Vec::new();
    let mut overlays: Vec<Overlay> = Vec::new();
    let mut segs: Vec<Seg> = Vec::new();
    let mut finds: Vec<(Range<usize>, bool)> = Vec::new();
    for (n, run) in runs.iter().enumerate() {
        if run.text.is_empty() {
            continue;
        }
        let from = text.len();
        let style = &run.style;
        let mut font = base.font();
        if style.bold {
            font.weight = FontWeight::SEMIBOLD;
        }
        if style.italic {
            font.style = FontStyle::Italic;
        }
        if style.math
            && let Some(size) = env.math
            && let Some(set) = math::typeset(&run.text, false, size, theme.text, env.scale, cx)
        {
            let unit = gap_unit(&font, font_size, env.window);
            let count = ((set.width / unit).ceil() as usize).max(1);
            let gap = "\u{a0}".repeat(count);
            overlays.push(Overlay {
                range: text.len()..text.len() + gap.len(),
                math: set,
            });
            spans.push(TextRun {
                len: gap.len(),
                font,
                color: base.color,
                background_color: None,
                underline: None,
                strikethrough: None,
            });
            text.push_str(&gap);
            segs.push(Seg {
                layout: from..text.len(),
                atom: first + n,
            });
            continue;
        }
        let mut action = style
            .link
            .as_deref()
            .map(|url| policy.action(url))
            .filter(|a| *a != Action::Plain);
        // An inline-code path that names a file the run has is a link to it (8.1).
        if action.is_none()
            && style.code
            && style.link.is_none()
            && link::looks_like_path(&run.text)
            && let Some(files) = policy.files()
            && let Some(target) = link::file_target(&run.text)
            && files.resolves(&target.path, cx)
        {
            action = Some(Action::File(target));
        }
        if style.code || style.math {
            font.family = MONO.into();
        }
        // A streaming link and every link in the preview look like links; in the transcript one
        // the policy will not open is plain text.
        let link = action.is_some()
            || (style.link.is_some()
                && (matches!(policy, LinkPolicy::Preview)
                    || style.link.as_deref() == Some(PENDING_LINK_URL)));
        if let Some(action) = action {
            links.push((text.len()..text.len() + run.text.len(), action));
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
        segs.push(Seg {
            layout: from..text.len(),
            atom: first + n,
        });
        finds.extend(find_hits(env, &run.text, from));
    }
    let spans = highlight_runs(spans, &layers(env, &segs, text.len(), &finds));
    let styled = StyledText::new(SharedString::from(text.clone())).with_runs(spans);
    let layout = styled.layout().clone();
    let element = if links.is_empty() {
        styled.into_any_element()
    } else {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        let (ranges, actions): (Vec<_>, Vec<_>) = links.into_iter().unzip();
        let policy = policy.clone();
        InteractiveText::new(("md-text", hasher.finish() as usize), styled)
            .on_click(ranges, move |ix, window, cx| {
                if let Some(action) = actions.get(ix) {
                    policy.run(action, window, cx);
                }
            })
            .into_any_element()
    };
    let element = tracked(env, element, layout.clone(), segs);
    if overlays.is_empty() {
        element
    } else {
        Overlays::new(element, layout, overlays).into_any_element()
    }
}

/// Whether a link may be opened from the Files tab's preview: web and mail links only. Anything
/// else (`file:`, custom schemes) could launch another program, and a link still streaming has no
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

    #[gpui::test]
    fn every_hit_in_the_drawn_text_is_numbered_in_drawing_order_and_the_current_one_apart(
        cx: &mut gpui::TestAppContext,
    ) {
        use crate::ui::theme::{Appearance, Theme};
        let (_view, vcx) = cx.add_window_view(|_, _| gpui::Empty);
        let theme = Theme::for_appearance(Appearance::Dark);
        let host = Host::preview().marked(Some(Rc::new(Marks {
            needle: "draw".to_owned(),
            current: Some(1),
        })));
        vcx.update(|window, _| {
            let env = Env {
                theme: &theme,
                host: &host,
                window,
                base: window.text_style(),
                scale: 1.0,
                math: None,
                cards: std::cell::Cell::new(0),
                atom_at: std::cell::Cell::new(0),
                hit_at: std::cell::Cell::new(0),
            };
            // Two runs, as `inline` goes through them: one match in the first, two in the second.
            let first = find_hits(&env, "draws it", 0);
            let second = find_hits(&env, "a Draw and draw", 8);
            assert_eq!(first, vec![(0..4, false)]);
            assert_eq!(
                second,
                vec![(10..14, true), (19..23, false)],
                "the second hit is the current one"
            );
            // What is painted over the text: the hits, the current one with its own style.
            let hits: Vec<_> = first.into_iter().chain(second).collect();
            let layers = layers(&env, &[], 23, &hits);
            assert_eq!(layers.len(), 3);
            let hit = |cur| crate::ui::find::hit_style(&theme, cur);
            assert_eq!(layers[0].1, hit(false));
            assert_eq!(layers[1].1, hit(true));
            assert_ne!(layers[0].1, layers[1].1);
        });
    }

    #[test]
    fn the_render_checks_flowchart_is_drawn_whole_and_legible_in_the_column() {
        use crate::ui::theme::{Appearance, Theme};
        let source = reno_client::fake::transcript::RENDER_REPLY
            .split("```mermaid\n")
            .nth(1)
            .and_then(|rest| rest.split("```").next())
            .expect("the reply holds a flowchart");
        let theme = Theme::for_appearance(Appearance::Dark);
        let svg = mermaid::render_svg(source, &theme).expect("it draws");
        let drawn = mermaid::rasterize(&svg, 1.0).expect("it rasterises");
        // The transcript's column inside a diagram's frame and padding.
        let column = 560.0;
        // Fit to width, never below the floor that keeps labels legible: the seeded diagram is
        // drawn whole, with no sideways scroll, at no less than three quarters of its size.
        assert_eq!(DIAGRAM_MIN_SCALE, 0.75, "three quarters of its size");
        let shown = picture::shown_width(column, drawn.width, DIAGRAM_MIN_SCALE);
        assert!(
            shown >= drawn.width * DIAGRAM_MIN_SCALE && shown <= column,
            "natural width {}, shown {shown}",
            drawn.width
        );
        assert!(
            drawn.width * DIAGRAM_MIN_SCALE <= column,
            "the whole diagram fits: natural width {}",
            drawn.width
        );
    }

    #[test]
    fn a_rewrite_resets_the_parse() {
        let mut stream = Streaming::default();
        stream.set_text("one");
        stream.set_text("two");
        assert_eq!(first_paragraph(&stream), "two");
    }
}
