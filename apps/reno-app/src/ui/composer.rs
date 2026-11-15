//! The composer: a multi-line text input with class chips, a route preview and a send button.
//!
//! The text, caret, selection and IME composition live in [`TextModel`]. This file owns what
//! needs GPUI: key actions, the platform input handler (IME, `NSTextInputClient` on macOS),
//! shaping and painting wrapped lines, mouse selection, and the surrounding chrome.
//!
//! Capabilities: typing, Backspace/Delete, caret and selection by keyboard and mouse
//! (grapheme-aware), line Home/End, logical-line Up/Down, select all, cut, copy, paste, IME
//! composition, Enter to send, Shift+Enter for a newline, auto-grow up to [`MAX_ROWS`].
//! Gaps: no undo, no word-wise movement or deletion, Up/Down move by logical line not by wrapped
//! row, and no scrolling past [`MAX_ROWS`] (the caret can leave the visible rows).

use std::ops::Range;

use gpui::{
    App, AvailableSpace, Bounds, ClipboardItem, ContentMask, Context, CursorStyle, Element,
    ElementId, ElementInputHandler, Entity, EntityInputHandler, FocusHandle, Focusable, FontWeight,
    GlobalElementId, Hsla, InspectorElementId, IntoElement, KeyBinding, LayoutId, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Point, Render, SharedString,
    Style, Subscription, TextAlign, TextRun, TextStyle, UTF16Selection, UnderlineStyle, Window,
    WrappedLine, actions, div, fill, point, prelude::*, px, relative, size,
};
use reno_client::ConnectionStatus;
use reno_proto::RunId;

use super::icon::Icon;
use super::kit;
use super::text_model::{EnterOutcome, TextModel};
use super::theme::{self, Theme};
use super::toast;
use crate::route::{self, Class};
use crate::store::Store;
use crate::summary;

actions!(
    composer,
    [
        Backspace,
        Delete,
        Left,
        Right,
        Up,
        Down,
        SelectLeft,
        SelectRight,
        SelectUp,
        SelectDown,
        SelectAll,
        Home,
        End,
        SelectHome,
        SelectEnd,
        Paste,
        Copy,
        Cut,
        Submit,
        Newline,
        ShowCharacterPalette,
    ]
);

/// The key context the bindings below are scoped to.
const CONTEXT: &str = "Composer";
/// The text area is at least this many rows tall and grows to [`MAX_ROWS`].
const MIN_ROWS: usize = 2;
const MAX_ROWS: usize = 10;

pub(crate) fn bind_keys(cx: &mut App) {
    let ctx = Some(CONTEXT);
    cx.bind_keys([
        KeyBinding::new("backspace", Backspace, ctx),
        KeyBinding::new("delete", Delete, ctx),
        KeyBinding::new("left", Left, ctx),
        KeyBinding::new("right", Right, ctx),
        KeyBinding::new("up", Up, ctx),
        KeyBinding::new("down", Down, ctx),
        KeyBinding::new("shift-left", SelectLeft, ctx),
        KeyBinding::new("shift-right", SelectRight, ctx),
        KeyBinding::new("shift-up", SelectUp, ctx),
        KeyBinding::new("shift-down", SelectDown, ctx),
        KeyBinding::new("cmd-a", SelectAll, ctx),
        KeyBinding::new("home", Home, ctx),
        KeyBinding::new("end", End, ctx),
        KeyBinding::new("cmd-left", Home, ctx),
        KeyBinding::new("cmd-right", End, ctx),
        KeyBinding::new("shift-home", SelectHome, ctx),
        KeyBinding::new("shift-end", SelectEnd, ctx),
        KeyBinding::new("cmd-shift-left", SelectHome, ctx),
        KeyBinding::new("cmd-shift-right", SelectEnd, ctx),
        KeyBinding::new("cmd-v", Paste, ctx),
        KeyBinding::new("cmd-c", Copy, ctx),
        KeyBinding::new("cmd-x", Cut, ctx),
        KeyBinding::new("enter", Submit, ctx),
        KeyBinding::new("shift-enter", Newline, ctx),
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, ctx),
    ]);
}

/// What the composer is for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// Home: a new task, with class chips and a route preview.
    Home,
    /// The Run screen: steers this run while it is live; a follow-up once it has finished.
    Steer(RunId),
}

pub(crate) struct Composer {
    store: Entity<Store>,
    mode: Mode,
    focus_handle: FocusHandle,
    model: TextModel,
    class: Class,
    why_open: bool,
    layout: Option<TextLayout>,
    selecting: bool,
    _store: Subscription,
}

/// The last painted text, kept for hit testing and IME geometry.
struct TextLayout {
    lines: Vec<WrappedLine>,
    starts: Vec<usize>,
    bounds: Bounds<Pixels>,
    line_height: Pixels,
    /// The text this layout was shaped from.
    text: String,
}

impl Composer {
    pub(crate) fn new(store: Entity<Store>, cx: &mut Context<Self>) -> Self {
        Self {
            _store: cx.observe(&store, |_, _, cx| cx.notify()),
            store,
            mode: Mode::Home,
            focus_handle: cx.focus_handle(),
            model: TextModel::new(),
            class: Class::default(),
            why_open: false,
            layout: None,
            selecting: false,
        }
    }

    /// A composer that steers `run`.
    pub(crate) fn steer(store: Entity<Store>, run: RunId, cx: &mut Context<Self>) -> Self {
        let mut composer = Self::new(store, cx);
        composer.mode = Mode::Steer(run);
        composer
    }

    pub(crate) fn class(&self) -> Class {
        self.class
    }

    #[cfg(test)]
    pub(crate) fn text(&self) -> &str {
        self.model.text()
    }

    /// Replaces the draft (the suggestion chips) and focuses the input.
    pub(crate) fn fill(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.model.set_text(text);
        window.focus(&self.focus_handle, cx);
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    pub(crate) fn set_class(&mut self, class: Class, cx: &mut Context<Self>) {
        if self.class != class {
            self.class = class;
            cx.notify();
        }
    }

    fn connected(&self, cx: &App) -> bool {
        self.store.read(cx).connection() == ConnectionStatus::Connected
    }

    /// Steering needs a live run; Phase 0 has no follow-up once it has finished.
    fn target_live(&self, cx: &App) -> bool {
        match &self.mode {
            Mode::Home => true,
            Mode::Steer(run) => self
                .store
                .read(cx)
                .state()
                .run(run)
                .is_some_and(|r| summary::is_steerable(r.status)),
        }
    }

    pub(crate) fn can_send(&self, cx: &App) -> bool {
        self.model.can_submit() && self.connected(cx) && self.target_live(cx)
    }

    /// Enter: send non-blank text to the daemon. A blank draft does nothing; with no daemon the
    /// draft is kept and a notice says why.
    pub(crate) fn submit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Enter belongs to the IME while it is composing.
        if self.model.marked().is_some() || !self.model.can_submit() || !self.target_live(cx) {
            return;
        }
        if !self.connected(cx) {
            toast::push(
                cx,
                toast::Tone::Error,
                "Daemon not running",
                "The task was not sent.",
            );
            return;
        }
        if let EnterOutcome::Submitted(prompt) = self.model.enter(false) {
            match &self.mode {
                Mode::Home => {
                    let executor = route::route_for(self.class).executor_id;
                    self.store
                        .update(cx, |store, cx| store.run_task(prompt, executor, cx));
                }
                Mode::Steer(run) => {
                    let run = run.clone();
                    self.store
                        .update(cx, |store, cx| store.steer(run, prompt, cx));
                }
            }
            window.notify_text_input_changed(&self.focus_handle);
            cx.notify();
        }
    }

    fn selection_changed(&mut self, window: &Window, cx: &mut Context<Self>) {
        window.notify_text_selection_changed(&self.focus_handle);
        cx.notify();
    }

    fn text_changed(&mut self, window: &Window, cx: &mut Context<Self>) {
        window.notify_text_input_changed(&self.focus_handle);
        cx.notify();
    }

    fn on_submit(&mut self, _: &Submit, window: &mut Window, cx: &mut Context<Self>) {
        self.submit(window, cx);
    }

    fn on_newline(&mut self, _: &Newline, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.marked().is_none() {
            self.model.enter(true);
            self.text_changed(window, cx);
        }
    }

    fn on_backspace(&mut self, _: &Backspace, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.backspace() {
            self.text_changed(window, cx);
        } else {
            window.play_system_bell();
        }
    }

    fn on_delete(&mut self, _: &Delete, window: &mut Window, cx: &mut Context<Self>) {
        if self.model.delete() {
            self.text_changed(window, cx);
        } else {
            window.play_system_bell();
        }
    }

    fn on_left(&mut self, _: &Left, window: &mut Window, cx: &mut Context<Self>) {
        self.model.move_left(false);
        self.selection_changed(window, cx);
    }

    fn on_right(&mut self, _: &Right, window: &mut Window, cx: &mut Context<Self>) {
        self.model.move_right(false);
        self.selection_changed(window, cx);
    }

    fn on_up(&mut self, _: &Up, window: &mut Window, cx: &mut Context<Self>) {
        self.model.move_up(false);
        self.selection_changed(window, cx);
    }

    fn on_down(&mut self, _: &Down, window: &mut Window, cx: &mut Context<Self>) {
        self.model.move_down(false);
        self.selection_changed(window, cx);
    }

    fn on_select_left(&mut self, _: &SelectLeft, window: &mut Window, cx: &mut Context<Self>) {
        self.model.move_left(true);
        self.selection_changed(window, cx);
    }

    fn on_select_right(&mut self, _: &SelectRight, window: &mut Window, cx: &mut Context<Self>) {
        self.model.move_right(true);
        self.selection_changed(window, cx);
    }

    fn on_select_up(&mut self, _: &SelectUp, window: &mut Window, cx: &mut Context<Self>) {
        self.model.move_up(true);
        self.selection_changed(window, cx);
    }

    fn on_select_down(&mut self, _: &SelectDown, window: &mut Window, cx: &mut Context<Self>) {
        self.model.move_down(true);
        self.selection_changed(window, cx);
    }

    fn on_select_all(&mut self, _: &SelectAll, window: &mut Window, cx: &mut Context<Self>) {
        self.model.select_all();
        self.selection_changed(window, cx);
    }

    fn on_home(&mut self, _: &Home, window: &mut Window, cx: &mut Context<Self>) {
        self.model.move_home(false);
        self.selection_changed(window, cx);
    }

    fn on_end(&mut self, _: &End, window: &mut Window, cx: &mut Context<Self>) {
        self.model.move_end(false);
        self.selection_changed(window, cx);
    }

    fn on_select_home(&mut self, _: &SelectHome, window: &mut Window, cx: &mut Context<Self>) {
        self.model.move_home(true);
        self.selection_changed(window, cx);
    }

    fn on_select_end(&mut self, _: &SelectEnd, window: &mut Window, cx: &mut Context<Self>) {
        self.model.move_end(true);
        self.selection_changed(window, cx);
    }

    fn on_paste(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.model.insert(&text);
            self.text_changed(window, cx);
        }
    }

    fn on_copy(&mut self, _: &Copy, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(selected) = self.model.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(selected.to_owned()));
        }
    }

    fn on_cut(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(selected) = self.model.cut() {
            cx.write_to_clipboard(ClipboardItem::new_string(selected));
            self.text_changed(window, cx);
        }
    }

    fn on_character_palette(
        &mut self,
        _: &ShowCharacterPalette,
        window: &mut Window,
        _: &mut Context<Self>,
    ) {
        window.show_character_palette();
    }

    fn on_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.focus_handle, cx);
        window.request_virtual_keyboard();
        self.selecting = true;
        let index = self.index_for_position(event.position);
        self.model.move_to(index, event.modifiers.shift);
        self.selection_changed(window, cx);
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.selecting {
            let index = self.index_for_position(event.position);
            self.model.move_to(index, true);
            self.selection_changed(window, cx);
        }
    }

    fn on_mouse_up(&mut self, _: &MouseUpEvent, _: &mut Window, _: &mut Context<Self>) {
        self.selecting = false;
    }

    /// The byte offset under a window position, from the last painted layout.
    fn index_for_position(&self, position: Point<Pixels>) -> usize {
        let Some(layout) = &self.layout else {
            return self.model.text().len();
        };
        layout.index_for_position(position)
    }
}

impl TextLayout {
    fn index_for_position(&self, position: Point<Pixels>) -> usize {
        let local = position - self.bounds.origin;
        if local.y < px(0.0) {
            return 0;
        }
        let mut top = px(0.0);
        for (line, start) in self.lines.iter().zip(&self.starts) {
            let height = line.size(self.line_height).height;
            if local.y < top + height {
                let hit = line
                    .closest_index_for_position(point(local.x, local.y - top), self.line_height);
                return start + hit.unwrap_or_else(|end| end);
            }
            top += height;
        }
        self.text.len()
    }

    /// Where a byte offset sits, relative to the text origin.
    fn point_for_index(&self, index: usize) -> Option<Point<Pixels>> {
        let mut top = px(0.0);
        for (line, start) in self.lines.iter().zip(&self.starts) {
            if index <= start + line.len() {
                let local = line.position_for_index(index - start, self.line_height)?;
                return Some(point(local.x, top + local.y));
            }
            top += line.size(self.line_height).height;
        }
        None
    }
}

/// Byte offset where each logical line starts, given each line's byte length. Lines are split at
/// `\n`, which the lines themselves do not include.
fn line_starts(lengths: impl IntoIterator<Item = usize>) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut next = 0;
    for len in lengths {
        starts.push(next);
        next += len + 1;
    }
    starts
}

impl EntityInputHandler for Composer {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let range = self.model.range_from_utf16(&range_utf16);
        actual_range.replace(self.model.range_to_utf16(&range));
        Some(self.model.text()[range].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: self.model.range_to_utf16(&self.model.selection()),
            reversed: self.model.reversed(),
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.model
            .marked()
            .map(|range| self.model.range_to_utf16(&range))
    }

    fn unmark_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.model.unmark();
        self.text_changed(window, cx);
    }

    fn replace_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        text: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.model.replace_utf16(range_utf16.as_ref(), text);
        self.text_changed(window, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range_utf16: Option<Range<usize>>,
        new_text: &str,
        new_selected_range_utf16: Option<Range<usize>>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.model.replace_and_mark_utf16(
            range_utf16.as_ref(),
            new_text,
            new_selected_range_utf16.as_ref(),
        );
        self.text_changed(window, cx);
    }

    fn bounds_for_range(
        &mut self,
        range_utf16: Range<usize>,
        bounds: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let layout = self.layout.as_ref()?;
        if layout.text != self.model.text() {
            return None;
        }
        let range = self.model.range_from_utf16(&range_utf16);
        let start = layout.point_for_index(range.start)?;
        let end = layout.point_for_index(range.end)?;
        Some(Bounds::from_corners(
            bounds.origin + start,
            bounds.origin + point(end.x.max(start.x), end.y + layout.line_height),
        ))
    }

    fn character_index_for_point(
        &mut self,
        position: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        let layout = self.layout.as_ref()?;
        if layout.text != self.model.text() {
            return None;
        }
        Some(
            self.model
                .offset_to_utf16(layout.index_for_position(position)),
        )
    }
}

impl Focusable for Composer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// The painted text area: shapes wrapped lines, then paints selection, text and caret and
/// installs the platform input handler.
struct TextArea {
    composer: Entity<Composer>,
    theme: Theme,
    placeholder: SharedString,
}

struct Prepaint {
    lines: Vec<WrappedLine>,
    starts: Vec<usize>,
    placeholder: Vec<WrappedLine>,
    selection: Vec<PaintQuad>,
    cursor: Option<PaintQuad>,
    line_height: Pixels,
}

fn run_for(style: &TextStyle, color: Hsla, len: usize) -> TextRun {
    TextRun {
        len,
        font: style.font(),
        color,
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}

/// Shapes `text` (split at newlines, wrapped at `width`), underlining the IME `marked` range.
fn shape(
    window: &Window,
    style: &TextStyle,
    text: &SharedString,
    color: Hsla,
    marked: Option<&Range<usize>>,
    width: Option<Pixels>,
) -> Vec<WrappedLine> {
    let base = run_for(style, color, text.len());
    let runs = match marked {
        Some(m) if m.end <= text.len() && m.start < m.end => vec![
            TextRun {
                len: m.start,
                ..base.clone()
            },
            TextRun {
                len: m.end - m.start,
                underline: Some(UnderlineStyle {
                    color: Some(color),
                    thickness: px(1.0),
                    wavy: false,
                }),
                ..base.clone()
            },
            TextRun {
                len: text.len() - m.end,
                ..base
            },
        ],
        _ => vec![base],
    };
    let font_size = style.font_size.to_pixels(window.rem_size());
    window
        .text_system()
        .shape_text(text.clone(), font_size, &runs, width, None)
        .map(|lines| lines.into_iter().collect())
        .unwrap_or_default()
}

fn row_count(lines: &[WrappedLine]) -> usize {
    lines
        .iter()
        .map(|l| l.wrap_boundaries().len() + 1)
        .sum::<usize>()
        .max(1)
}

/// Selection rectangles for `range`, relative to the text origin: one per visual row, and a
/// small stub where the selection takes in a newline.
fn selection_rects(
    lines: &[WrappedLine],
    starts: &[usize],
    line_height: Pixels,
    width: Pixels,
    range: &Range<usize>,
) -> Vec<Bounds<Pixels>> {
    let mut rects = Vec::new();
    let mut top = px(0.0);
    for (line, &start) in lines.iter().zip(starts) {
        let end = start + line.len();
        let height = line.size(line_height).height;
        let takes_newline = range.end > end && range.start <= end;
        if range.end > start && range.start <= end {
            let a = range.start.max(start) - start;
            let b = range.end.min(end) - start;
            if a < b || takes_newline {
                let pa = line.position_for_index(a, line_height).unwrap_or_default();
                let pb = line.position_for_index(b, line_height).unwrap_or_default();
                let row =
                    |p: Point<Pixels>| (f32::from(p.y) / f32::from(line_height)).round() as usize;
                for r in row(pa)..=row(pb) {
                    let x0 = if r == row(pa) { pa.x } else { px(0.0) };
                    let mut x1 = if r == row(pb) { pb.x } else { width };
                    if r == row(pb) && takes_newline {
                        x1 += px(6.0);
                    }
                    rects.push(Bounds::new(
                        point(x0, top + line_height * r),
                        size(x1 - x0, line_height),
                    ));
                }
            }
        }
        top += height;
    }
    rects
}

impl Element for TextArea {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let (text, marked) = {
            let composer = self.composer.read(cx);
            (
                SharedString::from(composer.model.text().to_owned()),
                composer.model.marked(),
            )
        };
        let style = window.text_style();
        let line_height = window.line_height();
        let color = self.theme.text;
        let mut layout = Style::default();
        layout.size.width = relative(1.0).into();
        let id = window.request_measured_layout(layout, move |known, available, window, _| {
            let width = known.width.or(match available.width {
                AvailableSpace::Definite(w) => Some(w),
                _ => None,
            });
            let lines = shape(window, &style, &text, color, marked.as_ref(), width);
            let rows = row_count(&lines).clamp(MIN_ROWS, MAX_ROWS);
            size(width.unwrap_or_default(), line_height * rows)
        });
        (id, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let composer = self.composer.read(cx);
        let text = SharedString::from(composer.model.text().to_owned());
        let selection = composer.model.selection();
        let caret = composer.model.caret();
        let marked = composer.model.marked();
        let focused = composer.focus_handle.is_focused(window);
        let style = window.text_style();
        let line_height = window.line_height();

        let lines = shape(
            window,
            &style,
            &text,
            self.theme.text,
            marked.as_ref(),
            Some(bounds.size.width),
        );
        let placeholder = if text.is_empty() {
            shape(
                window,
                &style,
                &self.placeholder,
                self.theme.faint,
                None,
                Some(bounds.size.width),
            )
        } else {
            Vec::new()
        };
        let starts = line_starts(lines.iter().map(WrappedLine::len));

        let accent = self.theme.accent;
        let selection_quads =
            selection_rects(&lines, &starts, line_height, bounds.size.width, &selection)
                .into_iter()
                .map(|r| {
                    fill(
                        Bounds::new(bounds.origin + r.origin, r.size),
                        accent.opacity(0.28),
                    )
                })
                .collect();

        let cursor = (focused && selection.is_empty()).then(|| {
            let at = lines
                .iter()
                .zip(&starts)
                .scan(px(0.0), |top, (line, &start)| {
                    let line_top = *top;
                    *top += line.size(line_height).height;
                    Some((line, start, line_top))
                })
                .find(|(line, start, _)| caret <= start + line.len())
                .and_then(|(line, start, top)| {
                    line.position_for_index(caret - start, line_height)
                        .map(|p| point(p.x, top + p.y))
                })
                .unwrap_or_default();
            fill(
                Bounds::new(bounds.origin + at, size(px(1.5), line_height)),
                self.theme.text,
            )
        });

        Prepaint {
            lines,
            starts,
            placeholder,
            selection: selection_quads,
            cursor,
            line_height,
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let focus_handle = self.composer.read(cx).focus_handle.clone();
        window.handle_input(
            &focus_handle,
            ElementInputHandler::new(bounds, self.composer.clone()),
            cx,
        );
        let line_height = prepaint.line_height;
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for quad in prepaint.selection.drain(..) {
                window.paint_quad(quad);
            }
            let mut top = px(0.0);
            let painted = if prepaint.lines.is_empty() || self.composer.read(cx).model.is_empty() {
                &prepaint.placeholder
            } else {
                &prepaint.lines
            };
            for line in painted {
                let _ = line.paint(
                    bounds.origin + point(px(0.0), top),
                    line_height,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                );
                top += line.size(line_height).height;
            }
            if let Some(cursor) = prepaint.cursor.take() {
                window.paint_quad(cursor);
            }
        });

        let lines = std::mem::take(&mut prepaint.lines);
        let starts = std::mem::take(&mut prepaint.starts);
        self.composer.update(cx, |composer, _| {
            composer.layout = Some(TextLayout {
                text: composer.model.text().to_owned(),
                lines,
                starts,
                bounds,
                line_height,
            });
        });
    }
}

impl Render for Composer {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let route = route::route_for(self.class);
        let can_send = self.can_send(cx);
        let focused = self.focus_handle.is_focused(window);
        let entity = cx.entity();

        let placeholder: SharedString = match &self.mode {
            Mode::Home => "What should Reno orchestrate?".into(),
            Mode::Steer(run) => {
                let state = self.store.read(cx);
                let summary = state.state().run(run);
                let bot = summary
                    .and_then(|r| summary::parse_title(&r.title).bot.map(str::to_owned))
                    .unwrap_or_else(|| "the run".to_owned());
                if self.target_live(cx) {
                    format!("Steer {bot}\u{2026} (delivered at the next step)").into()
                } else {
                    format!("Follow up with {bot}\u{2026} (not available yet)").into()
                }
            }
        };

        let text_area = div()
            .w_full()
            .px(px(22.0))
            .pt(px(18.0))
            .cursor(CursorStyle::IBeam)
            .font_weight(FontWeight::MEDIUM)
            .text_size(px(15.0))
            .line_height(px(23.0))
            .text_color(theme.text)
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(TextArea {
                composer: entity,
                theme,
                placeholder,
            });

        let class_chips = Class::ALL.map(|class| {
            let selected = class == self.class;
            div()
                .id(SharedString::from(format!("class-{}", class.label())))
                .debug_selector(move || format!("class-{}", class.label()))
                .h(px(26.0))
                .px(px(12.0))
                .flex()
                .items_center()
                .rounded(px(999.0))
                .text_size(px(13.0))
                .cursor_pointer()
                .when(selected, |c| {
                    c.bg(theme.accent.opacity(0.18)).text_color(theme.accent)
                })
                .when(!selected, |c| {
                    c.bg(theme.raised.opacity(0.5))
                        .text_color(theme.muted)
                        .hover(|s| s.text_color(theme.text))
                })
                .on_click(cx.listener(move |this, _, _, cx| this.set_class(class, cx)))
                .child(class.label())
        });

        let why = div()
            .id("why")
            .text_size(px(13.0))
            .text_color(theme.faint)
            .cursor_pointer()
            .border_b_1()
            .border_color(theme.faint.opacity(0.6))
            .on_click(cx.listener(|this, _, _, cx| {
                this.why_open = !this.why_open;
                cx.notify();
            }))
            .child("why?");

        let send = kit::send_button(&theme, can_send)
            .debug_selector(|| "send".to_owned())
            .when(can_send, |s| {
                s.on_click(cx.listener(|this, _, window, cx| this.submit(window, cx)))
            });

        let foot = match &self.mode {
            Mode::Home => div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .px(px(18.0))
                .pb(px(14.0))
                .child(
                    kit::icon_button(&theme, "attach", Icon::Clip, 28.0)
                        .on_click(|_, _, cx| toast::not_yet(cx, "Attachments")),
                )
                .children(class_chips)
                .child(div().flex_1())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .text_size(px(13.0))
                        .child(kit::exec_glyph(&theme, route.executor_id, 14.0))
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.text)
                                .child(route.executor),
                        )
                        .child(
                            div()
                                .text_color(theme.muted)
                                .child(format!("{} \u{b7} {}", route.model, route.bot)),
                        )
                        .child(why),
                )
                .child(send),
            Mode::Steer(run) => {
                let store = self.store.read(cx);
                let summary = store.state().run(run);
                let executor = summary.map_or("", |r| r.executor.as_str());
                let model = summary
                    .and_then(|r| r.model.clone())
                    .unwrap_or_else(|| "model".to_owned());
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .px(px(18.0))
                    .pb(px(14.0))
                    .child(kit::icon_chip(&theme, Icon::Cpu, model).when(
                        !executor.is_empty(),
                        |c| c.child(kit::exec_glyph(&theme, executor, 13.0)),
                    ))
                    .child(div().flex_1())
                    .child(send)
            }
        };

        div()
            .id("composer")
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_submit))
            .on_action(cx.listener(Self::on_newline))
            .on_action(cx.listener(Self::on_backspace))
            .on_action(cx.listener(Self::on_delete))
            .on_action(cx.listener(Self::on_left))
            .on_action(cx.listener(Self::on_right))
            .on_action(cx.listener(Self::on_up))
            .on_action(cx.listener(Self::on_down))
            .on_action(cx.listener(Self::on_select_left))
            .on_action(cx.listener(Self::on_select_right))
            .on_action(cx.listener(Self::on_select_up))
            .on_action(cx.listener(Self::on_select_down))
            .on_action(cx.listener(Self::on_select_all))
            .on_action(cx.listener(Self::on_home))
            .on_action(cx.listener(Self::on_end))
            .on_action(cx.listener(Self::on_select_home))
            .on_action(cx.listener(Self::on_select_end))
            .on_action(cx.listener(Self::on_paste))
            .on_action(cx.listener(Self::on_copy))
            .on_action(cx.listener(Self::on_cut))
            .on_action(cx.listener(Self::on_character_palette))
            .w_full()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .rounded(px(26.0))
            .bg(theme.surface)
            .border_1()
            .border_color(if focused {
                theme.text.opacity(0.2)
            } else {
                theme.hairline
            })
            .child(text_area)
            .child(foot)
            .when(self.why_open, |c| {
                c.child(
                    div()
                        .px(px(22.0))
                        .pb(px(14.0))
                        .text_size(px(12.5))
                        .text_color(theme.muted)
                        .child(format!(
                            "Routing preview (placeholder until the daemon exposes routing): {}",
                            route.why.join(" · ")
                        )),
                )
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_starts_skip_the_newline_between_lines() {
        assert_eq!(line_starts([3, 0, 5]), [0, 4, 5]);
        assert_eq!(line_starts([]), Vec::<usize>::new());
        assert_eq!(line_starts([0]), [0]);
    }
}
