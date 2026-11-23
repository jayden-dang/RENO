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

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::rc::Rc;

use gpui::{
    AnyElement, App, Bounds, CursorStyle, Div, ElementId, FontStyle, FontWeight, HighlightStyle,
    HitboxBehavior, Hsla, InteractiveText, IntoElement, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, RenderOnce, Role, ScrollHandle, SharedString,
    Stateful, StrikethroughStyle, StyledText, TextRun, Toggled, UnderlineStyle, Window, canvas,
    div, point, prelude::*, px, size,
};
use reno_client::Client;
use reno_markdown::mend::PENDING_LINK_URL;
use reno_markdown::parser::{Callout, TableAlign};
use reno_markdown::{Block, BlockTree, IncrementalParser, InlineRun};
use reno_proto::RunId;
use reno_syntax::{HighlightKind, HighlightRequest, HighlightSpan, HighlightedDocument};
use unicase::UniCase;

use self::chrome::Card;
use self::image::ImageHost;
use self::link::{Action, LinkPolicy};
use self::picture::{Overlay, Overlays, Picture};
use self::select::{Map, Registry, Seg, Track, highlight_runs};
use super::kit::MONO;
use super::theme::Theme;
use crate::transcript::Key;
use crate::vm::anchor::{Prose, footnote_marker, task_box_text};
use crate::vm::selection::Selection;

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
    anchors: RefCell<Option<Rc<Prose>>>,
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
        self.anchors.take();
        true
    }

    /// The drawn runs, code lines, formulas and diagrams of the tree, with their source.
    pub(crate) fn anchors(&self) -> Rc<Prose> {
        self.anchors
            .borrow_mut()
            .get_or_insert_with(|| {
                Rc::new(crate::vm::anchor::prose(self.parser.source(), &self.tree))
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
    /// Told when the owner zooms a diagram in, so a host that follows the end of its content can
    /// stop following.
    pub(crate) grew: Option<Grew>,
    footnotes: Option<FootnoteNavigation>,
    link_menu: Option<LinkMenu>,
}

/// What a host does when something the owner opened grows what it shows.
#[derive(Clone)]
pub(crate) struct Grew(pub(crate) Rc<dyn Fn(&mut App)>);

impl std::fmt::Debug for Grew {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Grew")
    }
}

type RevealFootnote = dyn Fn(Bounds<Pixels>, &mut App);

#[derive(Clone)]
struct FootnoteNavigation(Rc<RevealFootnote>);

impl std::fmt::Debug for FootnoteNavigation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FootnoteNavigation")
    }
}

type OpenLinkMenu = dyn Fn(Option<&str>, Point<Pixels>, &mut Window, &mut App);
type TrackLink = dyn Fn(usize, &str, Bounds<Pixels>);

#[derive(Clone)]
struct LinkMenu {
    open: Rc<OpenLinkMenu>,
    track: Rc<TrackLink>,
}

impl std::fmt::Debug for LinkMenu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LinkMenu")
    }
}

impl LinkMenu {
    fn open(
        &self,
        target: Option<&str>,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut App,
    ) {
        cx.stop_propagation();
        (self.open)(target, event.position, window, cx);
    }

    fn paint_target(&self, atom: usize, target: &str, bounds: Bounds<Pixels>, window: &Window) {
        if let Some(bounds) = super::menu::visible_target(bounds, window) {
            (self.track)(atom, target, bounds);
        }
    }
}

/// What to paint over a reply: what to look for, and which of the reply's drawn matches is the one the
/// find bar is on (counted from zero in drawing order; `None` when it is in another row).
#[derive(Debug)]
pub(crate) struct Marks {
    pub(crate) matcher: Rc<crate::vm::find::Matcher>,
    pub(crate) current: Option<usize>,
    pub(crate) current_source: Option<Range<usize>>,
    pub(crate) source_hits: Vec<(Range<usize>, bool)>,
    pub(crate) reveal: Option<u64>,
}

/// A reply drawn for the transcript's selection.
pub(crate) struct SelectHost {
    pub(crate) registry: Registry,
    pub(crate) key: Key,
    /// The reply's runs lined up with its source.
    pub(crate) anchors: Rc<Prose>,
    pub(crate) selected: Option<(Selection, usize)>,
    pub(crate) style: HighlightStyle,
}

impl SelectHost {
    fn selected_at(&self, atom: usize) -> Option<Range<usize>> {
        let (selection, ordinal) = self.selected?;
        let section = self.anchors.section_at(atom)?;
        selection.within_source(ordinal, section.slot, &section.source)
    }
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
            grew: None,
            footnotes: None,
            link_menu: None,
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
            grew: None,
            footnotes: None,
            link_menu: None,
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

    /// Says what to do when the owner zooms a diagram in.
    pub(crate) fn when_grown(mut self, grew: Option<Grew>) -> Self {
        self.grew = grew;
        self
    }

    pub(crate) fn on_footnote(
        mut self,
        reveal: impl Fn(Bounds<Pixels>, &mut App) + 'static,
    ) -> Self {
        self.footnotes = Some(FootnoteNavigation(Rc::new(reveal)));
        self
    }

    pub(crate) fn on_link_menu(
        mut self,
        open: impl Fn(Option<&str>, Point<Pixels>, &mut Window, &mut App) + 'static,
        track: impl Fn(usize, &str, Bounds<Pixels>) + 'static,
    ) -> Self {
        self.link_menu = Some(LinkMenu {
            open: Rc::new(open),
            track: Rc::new(track),
        });
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
    footnotes: HashMap<UniCase<&'a str>, usize>,
    footnote_bounds: Rc<RefCell<Vec<Bounds<Pixels>>>>,
    window: &'a Window,
    base: gpui::TextStyle,
    scale: f32,
    /// Whether math is typeset, and its size in logical pixels per em.
    math: Option<f32>,
    /// How many framed blocks this draw has made so far (their element ids differ by it).
    cards: std::cell::Cell<usize>,
    tables: Cell<usize>,
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
        let mut notes = Vec::new();
        collect_footnotes(self.tree.blocks.iter().map(|top| &top.block), &mut notes);
        let env = Env {
            theme: &self.theme,
            host: &self.host,
            footnotes: notes
                .iter()
                .enumerate()
                .map(|(ix, (label, _))| (UniCase::new(*label), ix))
                .collect(),
            footnote_bounds: Rc::default(),
            window,
            scale: window.scale_factor(),
            math: typeset.then(|| f32::from(size) * f32::from(percent) / 100.0),
            cards: std::cell::Cell::new(0),
            tables: Cell::new(0),
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
            .filter(|(_, top)| !is_hoisted(&top.block))
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
        let footnotes = (!notes.is_empty()).then(|| draw_footnotes(&notes, &env, cx));
        // The draw took exactly the atoms the anchors made for this tree, in the same order:
        // selection maps painted text to source through them.
        if let Some(select) = &self.host.select {
            debug_assert_eq!(
                env.atom_at.get(),
                select.anchors.atoms.len(),
                "atoms and drawing differ"
            );
        }
        div()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .children(blocks)
            .children(footnotes)
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

type Footnote<'a> = (&'a str, &'a [Block]);

fn collect_footnotes<'a>(
    blocks: impl IntoIterator<Item = &'a Block>,
    notes: &mut Vec<Footnote<'a>>,
) {
    for block in blocks {
        match block {
            Block::Footnote { label, blocks, .. } => {
                notes.push((label, blocks));
                collect_footnotes(blocks, notes);
            }
            Block::BlockQuote { children, .. } => collect_footnotes(children, notes),
            Block::List { items, .. } => collect_footnotes(items.iter().flatten(), notes),
            _ => {}
        }
    }
}

fn draw_footnotes(notes: &[Footnote<'_>], env: &Env, cx: &mut App) -> AnyElement {
    let bounds = env.footnote_bounds.clone();
    div()
        .on_children_prepainted(move |positions, _, _| *bounds.borrow_mut() = positions)
        .debug_selector(|| "md-footnotes".to_owned())
        .border_t_1()
        .border_color(env.theme.hairline)
        .pt(px(8.0))
        .flex()
        .flex_col()
        .gap(px(6.0))
        .children(
            notes
                .iter()
                .map(|(label, blocks)| {
                    div()
                        .flex()
                        .gap(px(8.0))
                        .child(
                            div()
                                .flex_none()
                                .text_color(env.theme.muted)
                                .child(footnote_marker(label)),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .flex()
                                .flex_col()
                                .gap(px(6.0))
                                .children(body_blocks(blocks, env, cx)),
                        )
                })
                .collect::<Vec<_>>(),
        )
        .into_any_element()
}

fn is_hoisted(block: &Block) -> bool {
    match block {
        Block::Footnote { .. } => true,
        Block::BlockQuote { children, .. } => only_hoisted(children),
        Block::List { items, .. } => {
            !items.is_empty() && items.iter().all(|item| only_hoisted(item))
        }
        _ => false,
    }
}

fn only_hoisted(blocks: &[Block]) -> bool {
    !blocks.is_empty() && blocks.iter().all(is_hoisted)
}

fn body_blocks(nodes: &[Block], env: &Env, cx: &mut App) -> Vec<AnyElement> {
    nodes
        .iter()
        .filter(|block| !is_hoisted(block))
        .map(|node| block(node, env, cx))
        .collect()
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
        Block::BlockQuote { kind, children } => quote(*kind, children, env, cx),
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
                    .filter(|(_, item)| !only_hoisted(item))
                    .map(|(i, item)| {
                        list_item(item, ordered_start.map(|start| start + i as u64), env, cx)
                    })
                    .collect::<Vec<_>>(),
            )
            .into_any_element(),
        Block::Table {
            header,
            rows,
            align,
        } => table(header, rows, align, env, cx),
        Block::Footnote { .. } => div().into_any_element(),
        Block::FrontMatter { lines, .. } => front_matter(lines, env),
        Block::Rule => div()
            .h(px(1.0))
            .w_full()
            .bg(theme.hairline)
            .into_any_element(),
    }
}

fn list_item(item: &[Block], number: Option<u64>, env: &Env, cx: &mut App) -> AnyElement {
    let mut body = Vec::new();
    let (marker, rest) = if let [Block::Paragraph { runs }, rest @ ..] = item
        && let [first, text @ ..] = runs.as_slice()
        && let Some(task) = &first.style.task
    {
        let marker = task_box(first, task.checked, env, cx);
        if !text.is_empty() {
            body.push(paragraph(text, env, cx));
        }
        (marker, rest)
    } else {
        let marker = number.map_or_else(|| "•".to_owned(), |n| format!("{n}."));
        (
            div()
                .text_color(env.theme.muted)
                .child(marker)
                .into_any_element(),
            item,
        )
    };
    body.extend(body_blocks(rest, env, cx));
    div()
        .flex()
        .gap(px(8.0))
        .child(div().flex_none().w(px(20.0)).child(marker))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .flex()
                .flex_col()
                .gap(px(4.0))
                .children(body),
        )
        .into_any_element()
}

fn task_box(run: &InlineRun, checked: bool, env: &Env, cx: &mut App) -> AnyElement {
    let atom = env.take_atoms(1);
    let reply = env.host.select.as_ref().map_or(0, |host| host.key.0);
    div()
        .id(SharedString::from(format!("md-task-box-{reply}-{atom}")))
        .debug_selector(|| "md-task-box".to_owned())
        .role(Role::CheckBox)
        .aria_label("Task status")
        .aria_description("Read only")
        .aria_toggled(if checked {
            Toggled::True
        } else {
            Toggled::False
        })
        .child(inline_at(std::slice::from_ref(run), atom, env, cx))
        .into_any_element()
}

fn front_matter(lines: &[(String, Range<usize>)], env: &Env) -> AnyElement {
    let cells = lines.iter().flat_map(|(line, source)| {
        let value = line
            .split_once(':')
            .map_or("", |(_, value)| value.trim_start());
        let split = line.len() - value.len();
        let atom = env.take_atoms(1);
        let seg = Seg {
            layout: 0..line.len(),
            atom,
        };
        let finds = find_hits(env, line, 0, Some(atom));
        let styles = layers(env, &[seg], line.len(), &finds);
        [0..split, split..line.len()]
            .into_iter()
            .enumerate()
            .map(move |(column, cell)| {
                let text = &line[cell.clone()];
                let mut run = env.base.to_run(text.len());
                if column == 0 {
                    run.font.weight = FontWeight::SEMIBOLD;
                    run.color = env.theme.muted;
                }
                let marks = styles
                    .iter()
                    .filter_map(|(range, style)| {
                        let start = range.start.max(cell.start);
                        let end = range.end.min(cell.end);
                        (start < end).then(|| (start - cell.start..end - cell.start, *style))
                    })
                    .collect::<Vec<_>>();
                let styled = StyledText::new(SharedString::from(text.to_owned()))
                    .with_runs(highlight_runs(vec![run], &marks));
                let layout = styled.layout().clone();
                let element = styled.into_any_element();
                let element = match &env.host.select {
                    Some(select) if !text.is_empty() => Track::new(
                        element,
                        layout,
                        (
                            select.key,
                            select.anchors.slot_at(atom),
                            Map::Linear {
                                base: source.start + cell.start,
                                len: text.len(),
                            },
                        ),
                        select.registry.clone(),
                    )
                    .into_any_element(),
                    _ => element,
                };
                div().min_w_0().px(px(10.0)).py(px(2.0)).child(element)
            })
    });
    div()
        .debug_selector(|| "md-front-matter".to_owned())
        .grid()
        .grid_cols_max_content(2)
        .self_start()
        .max_w_full()
        .py(px(3.0))
        .rounded(px(6.0))
        .border_1()
        .border_color(env.theme.hairline)
        .bg(env.theme.surface)
        .children(cells)
        .into_any_element()
}

fn quote(kind: Option<Callout>, children: &[Block], env: &Env, cx: &mut App) -> AnyElement {
    let theme = env.theme;
    let quote = div()
        .pl(px(12.0))
        .border_l_2()
        .flex()
        .flex_col()
        .gap(px(8.0));
    let quote = if let Some(kind) = kind {
        let (selector, label, color) = match kind {
            Callout::Note => ("md-callout-note", "Note", theme.accent),
            Callout::Tip => ("md-callout-tip", "Tip", theme.success),
            Callout::Warning => ("md-callout-warning", "Warning", theme.warning),
            Callout::Caution => ("md-callout-caution", "Caution", theme.danger),
            Callout::Important => ("md-callout-important", "Important", theme.accent),
        };
        quote
            .debug_selector(|| selector.to_owned())
            .border_color(color)
            .bg(color.opacity(0.07))
            .rounded(px(6.0))
            .py(px(8.0))
            .pr(px(12.0))
            .child(
                div()
                    .debug_selector(|| label.to_owned())
                    .text_color(color)
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(label),
            )
    } else {
        quote.border_color(theme.hairline).text_color(theme.muted)
    };
    quote
        .children(body_blocks(children, env, cx))
        .into_any_element()
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
            shown,
            first + n,
            images,
            env.host,
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
        && let Some(seg) = segs.first()
        && let Some(selected) = select.selected_at(seg.atom)
    {
        let map = Map::Atoms {
            atoms: select.anchors.atoms.clone(),
            segs: segs.to_vec(),
        };
        if let Some(range) = map.layout_range(&selected, len, false) {
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
fn find_hits(env: &Env, text: &str, base: usize, atom: Option<usize>) -> Vec<(Range<usize>, bool)> {
    let Some(marks) = &env.host.marks else {
        return Vec::new();
    };
    let source = atom
        .and_then(|ix| env.host.select.as_ref()?.anchors.atoms.get(ix))
        .filter(|atom| !atom.src.is_empty());
    marks
        .matcher
        .matches(text)
        .into_iter()
        .map(|range| {
            let n = env.hit_at.replace(env.hit_at.get() + 1);
            let current = match source {
                Some(atom) => marks.current_source.as_ref().is_some_and(|current| {
                    atom.search_source(&range)
                        .is_some_and(|src| src.start < current.end && current.start < src.end)
                }),
                None => marks.current == Some(n),
            };
            (base + range.start..base + range.end, current)
        })
        .collect()
}

fn task_find_hit(
    env: &Env,
    source: &Range<usize>,
    glyph: Range<usize>,
) -> Option<(Range<usize>, bool)> {
    let matches = &env.host.marks.as_ref()?.source_hits;
    let first = matches.partition_point(|(hit, _)| hit.end <= source.start);
    let mut overlaps = matches[first..]
        .iter()
        .take_while(|(hit, _)| hit.start < source.end);
    let (_, current) = overlaps.next()?;
    Some((glyph, *current || overlaps.any(|(_, current)| *current)))
}

/// Files a painted text with the transcript's selection, when the host tracks one.
fn tracked(env: &Env, element: AnyElement, layout: gpui::TextLayout, segs: Vec<Seg>) -> AnyElement {
    match &env.host.select {
        Some(select) if !segs.is_empty() => Track::new(
            element,
            layout,
            (
                select.key,
                select.anchors.slot_at(segs[0].atom),
                Map::Atoms {
                    atoms: select.anchors.atoms.clone(),
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
    let (Some(selected), Some(atom)) = (select.selected_at(at), select.anchors.atoms.get(at))
    else {
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
            let finds = find_hits(env, line, 0, atoms.map(|base| base + ix));
            let runs = highlight_runs(runs, &layers(env, &segs, line.len(), &finds));
            let styled = StyledText::new(SharedString::from((*line).to_owned())).with_runs(runs);
            let layout = styled.layout().clone();
            tracked(env, styled.into_any_element(), layout, segs)
        })
        .collect()
}

/// The code area of a frame: mono, with its lines wrapped or scrolling sideways.
fn keyboard_horizontal(
    viewport: Stateful<Div>,
    scroll: ScrollHandle,
    label: &'static str,
    theme: &Theme,
) -> Stateful<Div> {
    let accent = theme.accent;
    viewport
        .track_scroll(&scroll)
        .role(Role::ScrollView)
        .aria_label(label)
        .focusable()
        .tab_stop(true)
        .border_1()
        .border_color(gpui::transparent_black())
        .focus_visible(move |style| style.border_color(accent))
        .on_key_down(move |event, window, cx| {
            let key = &event.keystroke;
            if key.modifiers.platform || key.modifiers.control || key.modifiers.alt {
                return;
            }
            let offset = scroll.offset();
            let reach = scroll.max_offset().x;
            if reach <= px(0.0) {
                return;
            }
            let step = px(80.0);
            let x = match key.key.as_str() {
                "left" => offset.x + step,
                "right" => offset.x - step,
                "home" => px(0.0),
                "end" => -reach,
                _ => return,
            };
            scroll.set_offset(point(x.clamp(-reach, px(0.0)), offset.y));
            window.refresh();
            cx.stop_propagation();
        })
}

#[derive(IntoElement)]
struct HorizontalCode {
    key: usize,
    content: AnyElement,
    theme: Theme,
}

impl RenderOnce for HorizontalCode {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        let id: ElementId = ("md-code-scroll", self.key).into();
        let scroll = horizontal_state(id.clone(), window);
        keyboard_horizontal(
            div()
                .id(id)
                .w_full()
                .flex()
                .overflow_x_scroll()
                .restrict_scroll_to_axis(),
            scroll,
            "Scroll code horizontally",
            &self.theme,
        )
        .child(self.content)
    }
}

fn horizontal_state(id: ElementId, window: &mut Window) -> ScrollHandle {
    window.with_global_id(id, |id, window| {
        window.with_element_state(id, |state: Option<ScrollHandle>, _| {
            let state = state.unwrap_or_default();
            (state.clone(), state)
        })
    })
}

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
        HorizontalCode {
            key: key as usize,
            content: area
                .flex_none()
                .min_w_full()
                .whitespace_nowrap()
                .into_any_element(),
            theme: *env.theme,
        }
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

/// The padding around a drawn diagram, on each side.
const DIAGRAM_PAD: f32 = 12.0;

/// A drawn diagram: fit to the column, but never smaller than three quarters of its size, so its
/// labels stay legible; a wider diagram scrolls sideways.
pub(crate) fn diagram_picture(diagram: &mermaid::Diagram, key: u64) -> AnyElement {
    diagram_in_scroller(diagram, key, None)
}

/// Where the zoom of a diagram is kept: its key, and how far it is zoomed now.
#[derive(Clone)]
struct Zooming {
    key: u64,
    now: Option<chrome::Zoom>,
    grew: Option<Grew>,
}

#[derive(IntoElement)]
struct DiagramViewport {
    scroll_key: usize,
    content: AnyElement,
    zoom: Option<Zooming>,
    natural: f32,
    column: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl RenderOnce for DiagramViewport {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let id: ElementId = ("md-diagram-scroll", self.scroll_key).into();
        let scroll = horizontal_state(id.clone(), window);
        let theme = super::theme::current(cx);
        let label = if self.zoom.is_some() {
            "Diagram. Arrow keys scroll. Plus and minus zoom. Zero resets zoom."
        } else {
            "Scroll diagram horizontally"
        };
        keyboard_horizontal(
            div()
                .id(id)
                .debug_selector(|| "md-diagram".to_owned())
                .relative()
                .w_full()
                .overflow_x_scroll()
                .restrict_scroll_to_axis(),
            scroll,
            label,
            &theme,
        )
        .when_some(self.zoom, |d, zoom| {
            d.on_key_down(move |event, window, cx| {
                if event.keystroke.modifiers.platform
                    || event.keystroke.modifiers.alt
                    || event.keystroke.modifiers.control
                {
                    return;
                }
                match event.keystroke.key.as_str() {
                    "0" => chrome::reset_zoom(zoom.key, cx),
                    "+" | "=" => zoom_drawing(&zoom, self.column.get(), self.natural, 120.0, cx),
                    "-" => zoom_drawing(&zoom, self.column.get(), self.natural, -120.0, cx),
                    _ => return,
                }
                window.refresh();
                cx.stop_propagation();
            })
        })
        .child(self.content)
    }
}

fn zoom_drawing(
    zoom: &Zooming,
    bounds: Option<Bounds<Pixels>>,
    natural: f32,
    delta: f32,
    cx: &mut App,
) {
    let Some(bounds) = bounds else { return };
    let inner = f32::from(bounds.size.width) - 2.0 * DIAGRAM_PAD;
    let fit = picture::shown_width(inner, natural, DIAGRAM_MIN_SCALE) / natural.max(1.0);
    let grew = chrome::zoom_by(zoom.key, delta, fit, cx);
    if let Some(Grew(grew)) = zoom.grew.as_ref().filter(|_| grew) {
        grew(cx);
    }
}

/// The picture inside a box that scrolls sideways. With a `zooming`, holding the command key while
/// scrolling over it zooms it between fit-to-width and [`chrome::MAX_ZOOM`], and what that
/// leaves outside the column is reached by scrolling sideways.
fn diagram_in_scroller(
    diagram: &mermaid::Diagram,
    scroll_key: u64,
    zooming: Option<Zooming>,
) -> AnyElement {
    let zoom = zooming.as_ref().and_then(|z| z.now).map(|z| z.factor);
    // Zoomed in, the picture is its zoomed size and no less. Zoomed out of its natural size (a
    // column narrower than that), it is that share of its size and no less than the fit.
    let (scale, min_scale) = match zoom {
        Some(factor) if factor >= 1.0 => (factor, 1.0),
        Some(factor) => (1.0, factor.max(DIAGRAM_MIN_SCALE)),
        None => (1.0, DIAGRAM_MIN_SCALE),
    };
    let (width, height) = (diagram.width * scale, diagram.height * scale);
    let column: Rc<std::cell::Cell<Option<gpui::Bounds<Pixels>>>> = Rc::default();
    let natural = diagram.width;
    let keyboard_zoom = zooming.clone();
    let content = div()
        .w_full()
        .min_w_0()
        .child({
            let seen = column.clone();
            gpui::canvas(move |bounds, _, _| seen.set(Some(bounds)), |_, _, _, _| {})
                .absolute()
                .top_0()
                .left_0()
                .w_full()
                .h(px(0.0))
        })
        .child(
            div()
                .debug_selector(|| "md-diagram-zoom".to_owned())
                .w_full()
                .min_w(px(width * min_scale + 2.0 * DIAGRAM_PAD))
                .flex()
                .p(px(DIAGRAM_PAD))
                .child(Picture {
                    image: diagram.image.clone(),
                    width,
                    height,
                    min_scale,
                })
                .when_some(zooming, |d, zooming| {
                    let column = column.clone();
                    d.on_scroll(move |event, window, cx| {
                        if !event.modifiers.platform {
                            return;
                        }
                        cx.stop_propagation();
                        let delta = event.delta.pixel_delta(window.line_height());
                        zoom_drawing(&zooming, column.get(), natural, f32::from(delta.y), cx);
                    })
                }),
        )
        .into_any_element();
    DiagramViewport {
        scroll_key: scroll_key as usize,
        content,
        zoom: keyboard_zoom,
        natural,
        column,
    }
    .into_any_element()
}

/// The place of the diagram `ordinal` of the reply being drawn, if the reply has a row of its own:
/// it outlives a change of the diagram's source, so the drawing it had is kept while the new one
/// draws.
fn diagram_place(env: &Env, ordinal: usize) -> Option<u64> {
    let select = env.host.select.as_ref()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    (select.key.0, ordinal).hash(&mut hasher);
    Some(hasher.finish())
}

/// A Mermaid block in the diagram frame, with Preview and Code tabs: the drawing (the previous
/// one while a changed source draws, with "Rendering\u{2026}"), or the source with the note when
/// there is none; a plain code block when it cannot be drawn (TRANSCRIPT-10.1, 10.2, 10.3).
fn diagram(source: &str, atom: usize, env: &Env, cx: &mut App) -> AnyElement {
    let key = chrome::key_of(Some("mermaid"), source);
    let ordinal = env.next_card();
    let place = diagram_place(env, ordinal);
    let zoom = chrome::zoom(key, cx);
    let raster = chrome::raster_factor(key, cx);
    let drawing = mermaid::drawing(source, env.theme, env.scale * raster, place, cx);
    if matches!(drawing, mermaid::Drawing::Failed) {
        return code_block(source, Some("mermaid"), None, env, cx);
    }
    let scroll_key = key ^ ordinal as u64;
    let note = || {
        div()
            .px(px(14.0))
            .pb(px(8.0))
            .text_size(px(12.0))
            .text_color(env.theme.faint)
            .debug_selector(|| "md-diagram-rendering".to_owned())
            .child("Rendering\u{2026}")
    };
    let source_view = || {
        div()
            .debug_selector(|| "md-diagram-source".to_owned())
            .child(code_body(source, (None, None), false, scroll_key, env))
    };
    let zooming = || {
        Some(Zooming {
            key,
            now: zoom,
            grew: env.host.grew.clone(),
        })
    };
    let tab = chrome::tab(key, cx);
    let face = match (tab, drawing) {
        (chrome::DiagramTab::Code, _) => source_view().into_any_element(),
        (chrome::DiagramTab::Preview, mermaid::Drawing::Ready(diagram)) => {
            diagram_in_scroller(&diagram, scroll_key, zooming())
        }
        (
            chrome::DiagramTab::Preview,
            mermaid::Drawing::Rendering {
                previous: Some(old),
            },
        ) => div()
            .child(diagram_in_scroller(&old, scroll_key, zooming()))
            .when(!old.is_look_of(source, env.theme), |d| d.child(note()))
            .into_any_element(),
        (chrome::DiagramTab::Preview, _) => {
            div().child(source_view()).child(note()).into_any_element()
        }
    };
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
        div()
            .child(chrome::tab_bar(key, tab, zoom, env.theme))
            .child(face)
            .into_any_element(),
    );
    // A selected diagram is lifted whole: it has no text to paint over.
    match (&env.host.select, atom_selected(env, atom)) {
        (Some(select), true) => div()
            .rounded(px(10.0))
            .bg(select.style.background_color.unwrap_or(env.theme.raised))
            .p(px(3.0))
            .child(card)
            .into_any_element(),
        _ => card,
    }
}

fn table(
    header: &[Vec<InlineRun>],
    rows: &[Vec<Vec<InlineRun>>],
    align: &[TableAlign],
    env: &Env,
    cx: &mut App,
) -> AnyElement {
    let theme = *env.theme;
    let reveal = env
        .host
        .marks
        .as_ref()
        .filter(|marks| marks.current.is_some())
        .and_then(|marks| marks.reveal)
        .map(|request| TableReveal {
            request,
            edges: Rc::default(),
        });
    let cells = std::iter::once(header)
        .chain(rows.iter().map(Vec::as_slice))
        .enumerate()
        .flat_map(|(row, cells)| {
            cells
                .iter()
                .enumerate()
                .map(move |(column, cell)| (row, column, cell))
        })
        .map(|(row, column, cell)| {
            let cell_box = div()
                .border_b_1()
                .border_color(theme.hairline)
                .px(px(10.0))
                .py(px(6.0))
                .when(row == 0, |d| d.font_weight(FontWeight::MEDIUM));
            let align = align.get(column).copied().unwrap_or_default();
            let cell_box = match align {
                TableAlign::Center => cell_box.text_center(),
                TableAlign::Right => cell_box.text_right(),
                TableAlign::Left => cell_box.text_left(),
            };
            let first = env.take_atoms(cell.len());
            let reveal = reveal.as_ref().map(|reveal| (reveal, align));
            cell_box.child(inline_with_reveal(cell, first, env, cx, reveal))
        })
        .collect::<Vec<_>>();
    let grid = div()
        .grid()
        .grid_cols_max_content(u16::try_from(header.len()).unwrap_or(u16::MAX))
        .flex_none()
        .min_w_full()
        .whitespace_nowrap()
        .children(cells);
    let ordinal = env.tables.replace(env.tables.get() + 1);
    let reply = env.host.select.as_ref().map_or(0, |host| host.key.0);
    TableViewport {
        id: format!("md-table-{reply}-{ordinal}").into(),
        content: grid.into_any_element(),
        theme,
        reveal,
    }
    .into_any_element()
}

#[derive(IntoElement)]
struct TableViewport {
    id: SharedString,
    content: AnyElement,
    theme: Theme,
    reveal: Option<TableReveal>,
}

struct TableReveal {
    request: u64,
    edges: Rc<Cell<Option<(Pixels, Pixels)>>>,
}

#[derive(Default)]
struct TableScroll {
    handle: ScrollHandle,
    grab: Cell<Option<Pixels>>,
    revealed: Cell<Option<u64>>,
}

impl TableScroll {
    fn reveal_match(&self, reveal: &TableReveal, window: &Window, cx: &mut App) {
        let Some((left, right)) = reveal.edges.take() else {
            return;
        };
        if self.revealed.replace(Some(reveal.request)) == Some(reveal.request) {
            return;
        }
        let viewport = self.handle.bounds();
        let margin = px(6.0).min(viewport.size.width / 4.0);
        let start = viewport.left() + margin;
        let end = viewport.right() - margin;
        let shift = if left < start || right - left > end - start {
            start - left
        } else {
            (end - right).min(px(0.0))
        };
        let current = self.handle.offset().x;
        let next = (current + shift).clamp(-self.handle.max_offset().x, px(0.0));
        if next != current {
            let handle = self.handle.clone();
            // The current frame already placed the table at the previous offset.
            window.defer(cx, move |window, _| {
                handle.set_offset(point(next, px(0.0)));
                window.refresh();
            });
        }
    }
}

impl RenderOnce for TableViewport {
    fn render(self, window: &mut Window, _: &mut App) -> impl IntoElement {
        let scroll = window.with_global_id(self.id.clone().into(), |id, window| {
            window.with_element_state(id, |state: Option<Rc<TableScroll>>, _| {
                let state = state.unwrap_or_default();
                (state.clone(), state)
            })
        });
        div()
            .w_full()
            .min_w_0()
            .rounded(px(10.0))
            .border_1()
            .border_color(self.theme.hairline)
            .bg(self.theme.surface)
            .overflow_hidden()
            .child(
                keyboard_horizontal(
                    div()
                        .when_some(self.reveal, |viewport, reveal| {
                            let scroll = scroll.clone();
                            viewport.on_children_prepainted(move |_, window, cx| {
                                scroll.reveal_match(&reveal, window, cx);
                            })
                        })
                        .id(self.id)
                        .debug_selector(|| "md-table-scroll".to_owned())
                        .w_full()
                        .flex()
                        .overflow_x_scroll()
                        .restrict_scroll_to_axis(),
                    scroll.handle.clone(),
                    "Scroll table horizontally",
                    &self.theme,
                )
                .child(self.content),
            )
            .child(table_scrollbar(scroll, self.theme))
    }
}

fn table_scrollbar(scroll: Rc<TableScroll>, theme: Theme) -> impl IntoElement {
    let handle = scroll.handle.clone();
    div()
        .h(px(10.0))
        .debug_selector(|| "md-table-scrollbar".to_owned())
        .child(
            canvas(
                move |bounds, window, _| {
                    let reach = handle.max_offset().x;
                    let viewport = f32::from(bounds.size.width);
                    (reach > px(0.0) && viewport > 0.0).then(|| {
                        let content = viewport + f32::from(reach);
                        let (left, width) = super::scrollbar::thumb_geometry(
                            viewport,
                            content,
                            -f32::from(handle.offset().x),
                        );
                        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
                        (hitbox, left, width, viewport, content)
                    })
                },
                move |bounds, geometry, window, _| {
                    let Some((hitbox, left, width, viewport, content)) = geometry else {
                        return;
                    };
                    let height = px(3.0);
                    let track = Bounds {
                        origin: point(bounds.left(), bounds.center().y - height / 2.0),
                        size: size(bounds.size.width, height),
                    };
                    let thumb = Bounds {
                        origin: point(track.left() + px(left), track.top()),
                        size: size(px(width), height),
                    };
                    window.paint_quad(gpui::fill(track, theme.hairline));
                    window.paint_quad(gpui::fill(thumb, theme.faint));
                    window.set_cursor_style(CursorStyle::ResizeLeftRight, &hitbox);

                    let down = scroll.clone();
                    window.on_mouse_event(move |event: &MouseDownEvent, phase, window, cx| {
                        if phase.capture()
                            && event.button == MouseButton::Left
                            && hitbox.is_hovered(window)
                        {
                            let at = event.position.x - bounds.left();
                            let grab = if (px(left)..=px(left + width)).contains(&at) {
                                at - px(left)
                            } else {
                                px(width / 2.0)
                            };
                            down.grab.set(Some(grab));
                            scroll_table_to_thumb(&down.handle, viewport, content, at - grab);
                            window.refresh();
                            cx.stop_propagation();
                        }
                    });
                    let moved = scroll.clone();
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
                        if phase.capture()
                            && event.pressed_button == Some(MouseButton::Left)
                            && let Some(grab) = moved.grab.get()
                        {
                            let at = event.position.x - bounds.left() - grab;
                            scroll_table_to_thumb(&moved.handle, viewport, content, at);
                            window.refresh();
                            cx.stop_propagation();
                        }
                    });
                    window.on_mouse_event(move |event: &MouseUpEvent, phase, _, cx| {
                        if phase.capture()
                            && event.button == MouseButton::Left
                            && scroll.grab.take().is_some()
                        {
                            cx.stop_propagation();
                        }
                    });
                },
            )
            .size_full(),
        )
}

fn scroll_table_to_thumb(handle: &ScrollHandle, viewport: f32, content: f32, left: Pixels) {
    let offset = super::scrollbar::offset_for_thumb_top(viewport, content, f32::from(left));
    handle.set_offset(point(px(-offset), px(0.0)));
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

enum InlineAction {
    Link {
        action: Action,
        target: String,
        atom: usize,
    },
    Footnote {
        target: usize,
        atom: usize,
        label: String,
    },
}

struct LinkSpan {
    atom: usize,
    range: Range<usize>,
    target: String,
}

/// [`inline`] for runs that are atoms `first`, `first + 1`, ... of the reply.
fn inline_at(runs: &[InlineRun], first: usize, env: &Env, cx: &mut App) -> AnyElement {
    inline_with_reveal(runs, first, env, cx, None)
}

fn inline_with_reveal(
    runs: &[InlineRun],
    first: usize,
    env: &Env,
    cx: &mut App,
    reveal: Option<(&TableReveal, TableAlign)>,
) -> AnyElement {
    let theme = env.theme;
    let base = &env.base;
    let policy = &env.host.links;
    let font_size = base.font_size.to_pixels(env.window.rem_size());
    let mut text = String::new();
    let mut spans = Vec::with_capacity(runs.len());
    let mut links: Vec<(Range<usize>, InlineAction)> = Vec::new();
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
        let marker = style.footnote_ref.as_deref().map(footnote_marker);
        let task = style.task.as_ref().map(|task| task_box_text(task.checked));
        let shown = marker.as_deref().or(task).unwrap_or(&run.text);
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
        let footnote = style
            .footnote_ref
            .as_ref()
            .filter(|_| env.host.footnotes.is_some())
            .and_then(|label| env.footnotes.get(&UniCase::new(label.as_str())))
            .copied();
        let link = footnote.is_some()
            || action.is_some()
            || (style.link.is_some()
                && (matches!(policy, LinkPolicy::Preview)
                    || style.link.as_deref() == Some(PENDING_LINK_URL)));
        if let Some(action) = footnote
            .map(|target| InlineAction::Footnote {
                target,
                atom: first + n,
                label: style.footnote_ref.clone().unwrap_or_default(),
            })
            .or_else(|| {
                action.map(|action| {
                    let target = style.link.as_deref().unwrap_or(&run.text).to_owned();
                    InlineAction::Link {
                        action,
                        target,
                        atom: first + n,
                    }
                })
            })
        {
            links.push((from..from + shown.len(), action));
        }
        spans.push(TextRun {
            len: shown.len(),
            font,
            color: match &style.task {
                Some(task) if task.checked => theme.accent,
                Some(_) => theme.faint,
                None if link => theme.accent,
                None => base.color,
            },
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
        text.push_str(shown);
        segs.push(Seg {
            layout: from..text.len(),
            atom: first + n,
        });
        if let Some(task) = &style.task {
            finds.extend(task_find_hit(
                env,
                &task.range,
                from..from + shown.trim_end().len(),
            ));
        } else {
            finds.extend(find_hits(env, shown, from, Some(first + n)));
        }
    }
    let spans = highlight_runs(spans, &layers(env, &segs, text.len(), &finds));
    let styled = StyledText::new(SharedString::from(text.clone())).with_runs(spans);
    let layout = styled.layout().clone();
    let element =
        if links.is_empty() {
            styled.into_any_element()
        } else {
            let targets = env.host.link_menu.clone().map(|menu| {
                let targets = links
                    .iter()
                    .filter_map(|(range, action)| match action {
                        InlineAction::Link { target, atom, .. } => Some(LinkSpan {
                            atom: *atom,
                            range: range.clone(),
                            target: target.clone(),
                        }),
                        InlineAction::Footnote { .. } => None,
                    })
                    .collect::<Vec<_>>();
                (menu, targets)
            });
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut hasher);
            let footnotes: Vec<_> = env
                .host
                .select
                .as_ref()
                .into_iter()
                .flat_map(|select| {
                    links.iter().filter_map(|(range, action)| {
                        let InlineAction::Footnote {
                            target,
                            atom,
                            label,
                        } = action
                        else {
                            return None;
                        };
                        Some(FootnoteControl {
                            id: ElementId::Name(
                                format!("md-footnote-control-{}-{atom}", select.key.0).into(),
                            ),
                            label: format!("Go to footnote {label}"),
                            target: *target,
                            range: range.clone(),
                            layout: layout.clone(),
                            navigation: env.host.footnotes.clone()?,
                            definitions: env.footnote_bounds.clone(),
                            ink: theme.accent,
                        })
                    })
                })
                .collect();
            let (ranges, actions): (Vec<_>, Vec<_>) = links.into_iter().unzip();
            let policy = policy.clone();
            let navigation = env.host.footnotes.clone();
            let bounds = env.footnote_bounds.clone();
            let text = InteractiveText::new(("md-text", hasher.finish() as usize), styled)
                .on_click(ranges, move |ix, window, cx| match actions.get(ix) {
                    Some(InlineAction::Link { action, .. }) => policy.run(action, window, cx),
                    Some(InlineAction::Footnote { target, .. }) => {
                        let destination = bounds.borrow().get(*target).copied();
                        if let (Some(navigation), Some(destination)) = (&navigation, destination) {
                            (navigation.0)(destination, cx);
                        }
                    }
                    None => {}
                });
            let text = if let Some((menu, targets)) = targets {
                inline_link_menu(text, layout.clone(), menu, targets)
            } else {
                text.into_any_element()
            };
            if footnotes.is_empty() {
                text
            } else {
                div()
                    .relative()
                    .child(text)
                    .children(footnotes)
                    .into_any_element()
            }
        };
    let element = tracked(env, element, layout.clone(), segs);
    let element = if overlays.is_empty() {
        element
    } else {
        Overlays::new(element, layout.clone(), overlays).into_any_element()
    };
    if let Some((reveal, align)) = reveal
        && let Some((range, _)) = finds.into_iter().find(|(_, current)| *current)
    {
        let edges = reveal.edges.clone();
        div()
            .on_children_prepainted(move |_, _, _| {
                edges.set(table_match_edges(&layout, &range, align));
            })
            .child(element)
            .into_any_element()
    } else {
        element
    }
}

fn table_match_edges(
    layout: &gpui::TextLayout,
    range: &Range<usize>,
    align: TableAlign,
) -> Option<(Pixels, Pixels)> {
    let start = layout.position_for_index(range.start)?.x;
    let end = layout.position_for_index(range.end)?.x;
    let align_offset = match align {
        TableAlign::Left => px(0.0),
        TableAlign::Center | TableAlign::Right => {
            let line = layout.line_layout_for_index(range.start)?;
            let extra = layout.bounds().size.width - line.width();
            if align == TableAlign::Center {
                extra / 2.0
            } else {
                extra
            }
        }
    };
    Some((start.min(end) + align_offset, start.max(end) + align_offset))
}

#[derive(IntoElement)]
struct FootnoteControl {
    id: ElementId,
    label: String,
    target: usize,
    range: Range<usize>,
    layout: gpui::TextLayout,
    navigation: FootnoteNavigation,
    definitions: Rc<RefCell<Vec<Bounds<Pixels>>>>,
    ink: Hsla,
}

impl RenderOnce for FootnoteControl {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let focus = window.with_global_id(self.id.clone(), |id, window| {
            window.with_element_state(id, |handle: Option<gpui::FocusHandle>, _| {
                let handle = handle.unwrap_or_else(|| cx.focus_handle().tab_stop(true));
                (handle.clone(), handle)
            })
        });
        let focused = focus.clone();
        div()
            .absolute()
            .size_0()
            .id(self.id)
            .track_focus(&focus)
            .tab_stop(true)
            .role(Role::Button)
            .aria_label(self.label)
            .on_key_down(move |event, _, cx| {
                if matches!(event.keystroke.key.as_str(), "enter" | "space")
                    && let Some(bounds) = self.definitions.borrow().get(self.target).copied()
                {
                    (self.navigation.0)(bounds, cx);
                    cx.stop_propagation();
                }
            })
            .child(
                canvas(
                    |_, _, _| (),
                    move |_, (), window, _| {
                        if focused.is_focused(window)
                            && let Some(bounds) = link_bounds(&self.layout, &self.range)
                        {
                            window.paint_quad(gpui::outline(
                                bounds,
                                self.ink,
                                gpui::BorderStyle::Solid,
                            ));
                        }
                    },
                )
                .absolute()
                .size_0(),
            )
    }
}

fn inline_link_menu(
    text: InteractiveText,
    layout: gpui::TextLayout,
    menu: LinkMenu,
    targets: Vec<LinkSpan>,
) -> AnyElement {
    let targets = Rc::new(targets);
    let painted = {
        let (menu, layout, targets) = (menu.clone(), layout.clone(), targets.clone());
        gpui::canvas(
            |_, _, _| (),
            move |_, (), window, _| {
                for target in targets.iter() {
                    if let Some(bounds) = link_bounds(&layout, &target.range) {
                        menu.paint_target(target.atom, &target.target, bounds, window);
                    }
                }
            },
        )
        .absolute()
        .inset_0()
    };
    div()
        .relative()
        .child(text)
        .child(painted)
        // Bubble from the top of the text after the popups' outside-press handlers run.
        .child(div().absolute().inset_0().on_mouse_down(
            MouseButton::Right,
            move |event, window, cx| {
                let target = layout
                    .index_for_position(event.position)
                    .ok()
                    .and_then(|ix| {
                        targets.iter().find_map(|target| {
                            target.range.contains(&ix).then_some(target.target.as_str())
                        })
                    });
                menu.open(target, event, window, cx);
            },
        ))
        .into_any_element()
}

fn link_bounds(layout: &gpui::TextLayout, range: &Range<usize>) -> Option<Bounds<Pixels>> {
    let start = layout.position_for_index(range.start)?;
    let end = layout.position_for_index(range.end)?;
    let (left, right) = if start.y == end.y {
        (start.x.min(end.x), start.x.max(end.x))
    } else {
        (layout.bounds().left(), layout.bounds().right())
    };
    Some(Bounds::from_corners(
        gpui::point(left, start.y),
        gpui::point(right, end.y + layout.line_height()),
    ))
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
        let matcher = crate::vm::find::Query {
            text: "draw".to_owned(),
            ..Default::default()
        }
        .compile()
        .map(Rc::new)
        .expect("a plain query compiles");
        let host = Host::preview().marked(Some(Rc::new(Marks {
            reveal: None,
            source_hits: Vec::new(),
            matcher,
            current: Some(1),
            current_source: None,
        })));
        vcx.update(|window, _| {
            let env = Env {
                theme: &theme,
                host: &host,
                footnotes: HashMap::new(),
                footnote_bounds: Rc::default(),
                window,
                base: window.text_style(),
                scale: 1.0,
                math: None,
                cards: std::cell::Cell::new(0),
                tables: Cell::new(0),
                atom_at: std::cell::Cell::new(0),
                hit_at: std::cell::Cell::new(0),
            };
            // Two runs, as `inline` goes through them: one match in the first, two in the second.
            let first = find_hits(&env, "draws it", 0, None);
            let second = find_hits(&env, "a Draw and draw", 8, None);
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

    #[gpui::test]
    fn a_current_find_match_follows_its_source_when_a_note_moves_after_the_tail(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_view, vcx) = cx.add_window_view(|_, _| gpui::Empty);
        let theme = Theme::for_appearance(crate::ui::theme::Appearance::Dark);
        let source = "Intro[^a].\n\n[^a]: Search in the note.\n\nSearch in the tail.";
        let start = source.find("Search").unwrap();
        let mut stream = Streaming::default();
        stream.set_text(source);
        let host = Host::transcript(None, None, None)
            .marked(Some(Rc::new(Marks {
                reveal: None,
                source_hits: Vec::new(),
                matcher: Rc::new(
                    crate::vm::find::Query {
                        text: "Search".into(),
                        ..Default::default()
                    }
                    .compile()
                    .unwrap(),
                ),
                current: Some(0),
                current_source: Some(start..start + "Search".len()),
            })))
            .selectable(Some(Rc::new(SelectHost {
                registry: Registry::default(),
                key: Key(0),
                anchors: stream.anchors(),
                selected: None,
                style: HighlightStyle::default(),
            })));
        vcx.update(|window, _| {
            let env = Env {
                theme: &theme,
                host: &host,
                footnotes: HashMap::new(),
                footnote_bounds: Rc::default(),
                window,
                base: window.text_style(),
                scale: 1.0,
                math: None,
                cards: Default::default(),
                tables: Default::default(),
                atom_at: Default::default(),
                hit_at: Default::default(),
            };
            let atoms = &host.select.as_ref().unwrap().anchors.atoms;
            let match_in = |text| {
                let ix = atoms.iter().position(|atom| atom.text == text).unwrap();
                find_hits(&env, text, 0, Some(ix))
            };
            assert_eq!(match_in("Search in the tail."), [(0..6, false)]);
            assert_eq!(match_in("Search in the note."), [(0..6, true)]);
        });
    }

    #[gpui::test]
    fn only_the_current_occurrence_in_a_footnote_marker_is_highlighted_as_current(
        cx: &mut gpui::TestAppContext,
    ) {
        let (_view, vcx) = cx.add_window_view(|_, _| gpui::Empty);
        let theme = Theme::for_appearance(crate::ui::theme::Appearance::Dark);
        let source = "Read [^banana].\n\n[^banana]: Note.";
        let start = source.find("banana").unwrap() + 3;
        let mut stream = Streaming::default();
        stream.set_text(source);
        let anchors = stream.anchors();
        let at = anchors
            .atoms
            .iter()
            .position(|atom| atom.text == "[banana]")
            .unwrap();
        let host = Host::transcript(None, None, None)
            .marked(Some(Rc::new(Marks {
                reveal: None,
                source_hits: Vec::new(),
                matcher: Rc::new(
                    crate::vm::find::Query {
                        text: "a".into(),
                        ..Default::default()
                    }
                    .compile()
                    .unwrap(),
                ),
                current: Some(1),
                current_source: Some(start..start + 1),
            })))
            .selectable(Some(Rc::new(SelectHost {
                registry: Registry::default(),
                key: Key(0),
                anchors,
                selected: None,
                style: HighlightStyle::default(),
            })));
        vcx.update(|window, _| {
            let env = Env {
                theme: &theme,
                host: &host,
                footnotes: HashMap::new(),
                footnote_bounds: Rc::default(),
                window,
                base: window.text_style(),
                scale: 1.0,
                math: None,
                cards: Default::default(),
                tables: Default::default(),
                atom_at: Default::default(),
                hit_at: Default::default(),
            };
            assert_eq!(
                find_hits(&env, "[banana]", 0, Some(at)),
                [(2..3, false), (4..5, true), (6..7, false)]
            );
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
