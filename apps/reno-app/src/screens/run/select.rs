//! Selecting text across the transcript and copying it (TRANSCRIPT-2.1, 2.2, 2.4 to 2.8, 2.10,
//! 2.13).
//!
//! The model is `vm::selection`: two source-anchored positions in reading order. This file is the
//! screen's half: it turns a pointer into a position (from the texts painted last frame, filed
//! in a registry by the renderer), keeps the drag going past the list's edge, paints what is
//! selected, and puts the text on the clipboard. Painting never changes a height, so a selection
//! never makes the list measure again.

use std::cell::Cell;
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use gpui::Action as _;
use gpui::{
    AnyElement, App, Bounds, ClipboardItem, Context, Element, ElementId,
    GlobalElementId, InspectorElementId, KeyBinding, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, SharedString, StyledText, Task, Window, actions,
    div, point, prelude::*, px,
};

use super::RunScreen;
use crate::transcript::{Key, Row};
use crate::ui::find as look;
use crate::ui::markdown_view::select::{Map, Piece, Registry, Track, flatten_layers};
use crate::ui::theme::Theme;
use crate::vm::draft33::Variant;
use crate::vm::selection::{self, Entry, Pos, Selection, Shown, Slot};

actions!(run_transcript, [CopyTranscript]);

/// The key context of the transcript's focus: ⌘C copies the selection only while it holds focus.
const CONTEXT: &str = "Transcript";

/// A drag moves the pointer this far before it is a selection and not a click.
const DRAG_SLOP: f32 = 3.0;
/// Autoscroll: how far past the list's edge the pointer is for the fastest scroll, and the most
/// the list moves in one step.
const EDGE_FULL_SPEED: f32 = 80.0;
const SCROLL_STEP_MAX: f32 = 48.0;
const SCROLL_EVERY: Duration = Duration::from_millis(16);

pub(super) fn bind_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("cmd-c", CopyTranscript, Some(CONTEXT))]);
}

#[derive(Clone, Copy)]
struct Drag {
    start: Point<Pixels>,
    last: Point<Pixels>,
    moved: bool,
}

/// The selection and what keeps it: the texts painted last frame, the drag in progress.
#[derive(Default)]
pub(super) struct Select {
    pub(super) current: Option<Selection>,
    /// The texts painted since the last draw began.
    pub(super) registry: Registry,
    /// Where each entry is in reading order.
    pub(super) order: HashMap<Key, usize>,
    drag: Option<Drag>,
    /// The list's bounds, as last painted.
    pub(super) list: Rc<Cell<Bounds<Pixels>>>,
    /// How far the list moves each step while the pointer is held past its edge (negative: up).
    scroll: f32,
    task: Option<Task<()>>,
}

/// Where a text of a row is, for painting and tracking it.
#[derive(Clone, Copy)]
pub(super) struct Spot {
    pub(super) key: Key,
    pub(super) slot: Slot,
    /// Bytes of the text's source that the text drawn starts at, and how many it has.
    pub(super) base: usize,
    pub(super) len: usize,
    /// The length of the whole source text this is part of; `None` for text with no source.
    pub(super) total: Option<usize>,
}

impl RunScreen {
    /// Works out where each entry is in reading order again; the rows changed.
    pub(super) fn reorder(&mut self) {
        self.select.order = selection::reading(&self.rows, &|_| false)
            .into_iter()
            .map(|e| (e.row.key(), e.ordinal))
            .collect();
    }

    #[cfg(test)]
    pub(crate) fn has_selection(&self) -> bool {
        self.select.current.is_some_and(|s| !s.is_empty())
    }

    /// Where the view is: the item at its top and how far into it.
    #[cfg(test)]
    pub(crate) fn scroll_offset(&self) -> (usize, Pixels) {
        let top = self.list.logical_scroll_top();
        (top.item_ix, top.offset_in_item)
    }

    /// The texts painted last frame: which entry and text, and where.
    #[cfg(test)]
    pub(crate) fn painted_pieces(&self) -> Vec<(Key, Slot, Bounds<Pixels>)> {
        self.select
            .registry
            .borrow()
            .iter()
            .map(|p| (p.key, p.slot, p.bounds))
            .collect()
    }

    /// The selection, if any, as the row `key` is drawn: its entry's place in reading order.
    fn selection_for(&self, key: Key) -> Option<(Selection, usize)> {
        let sel = self.select.current.filter(|s| !s.is_empty())?;
        Some((sel, *self.select.order.get(&key)?))
    }

    /// The bytes of the reply's source that are selected.
    pub(super) fn selected_in_prose(&self, key: Key, len: usize) -> Option<Range<usize>> {
        let (sel, ordinal) = self.selection_for(key)?;
        sel.within(ordinal, Slot::WHOLE, Some(len))
    }

    /// One text of a row, painted with the selection and the find hits over it, and tracked so
    /// that a pointer on it is a position in the selection.
    pub(super) fn text_piece(
        &self,
        theme: &Theme,
        variant: Variant,
        spot: Spot,
        text: &str,
        finds: &[(Range<usize>, bool)],
    ) -> AnyElement {
        let mut layers = Vec::new();
        if let Some((sel, ordinal)) = self.selection_for(spot.key)
            && let Some(source) = sel.within(ordinal, spot.slot, spot.total)
        {
            let map = piece_map(spot);
            if let Some(range) = map.layout_range(&source, text.len(), spot.total.is_none()) {
                layers.push((
                    range,
                    crate::ui::markdown_view::select::selection_style(theme, variant),
                ));
            }
        }
        let len = text.len();
        layers.extend(
            finds
                .iter()
                .filter(|(r, _)| r.end <= len && r.start < r.end)
                .map(|(r, current)| (r.clone(), look::hit_style(theme, variant, *current))),
        );
        let styled = StyledText::new(SharedString::from(text.to_owned()))
            .with_highlights(flatten_layers(&layers));
        let layout = styled.layout().clone();
        Track::new(
            styled.into_any_element(),
            layout,
            (spot.key, spot.slot, piece_map(spot)),
            self.select.registry.clone(),
        )
        .into_any_element()
    }

    // --- the pointer ---------------------------------------------------------------------------

    /// The position under `point`, from the texts painted last frame: the text it is on or, past
    /// the text, the nearest one.
    fn position_at(&self, point: Point<Pixels>) -> Option<Pos> {
        let pieces = self.select.registry.borrow();
        let area = self.select.list.get();
        let gap = |p: &Piece| {
            let b = &p.bounds;
            let dy = if point.y < b.top() {
                b.top() - point.y
            } else if point.y > b.bottom() {
                point.y - b.bottom()
            } else {
                px(0.0)
            };
            let dx = if point.x < b.left() {
                b.left() - point.x
            } else if point.x > b.right() {
                point.x - b.right()
            } else {
                px(0.0)
            };
            // Rows are what the pointer is between: a line above or below beats one beside.
            (f32::from(dy), f32::from(dx))
        };
        let piece = pieces
            .iter()
            .filter(|p| p.bounds.intersects(&area) || area.size.width == px(0.0))
            .filter(|p| self.select.order.contains_key(&p.key))
            .min_by(|a, b| gap(a).partial_cmp(&gap(b)).unwrap_or(std::cmp::Ordering::Equal))?;
        let off = if piece.map.is_sourceless() {
            0
        } else {
            piece.source_at(point)
        };
        Some(Pos {
            ordinal: *self.select.order.get(&piece.key)?,
            slot: piece.slot,
            off,
        })
    }

    /// The left button went down in the transcript: a drag may start.
    pub(super) fn select_down(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        if event.button != MouseButton::Left || event.modifiers.platform || event.modifiers.shift {
            return;
        }
        self.select.drag = Some(Drag {
            start: event.position,
            last: event.position,
            moved: false,
        });
        let _ = cx;
    }

    /// The pointer moved with the button down.
    pub(super) fn select_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(mut drag) = self.select.drag else {
            return;
        };
        if event.pressed_button != Some(MouseButton::Left) {
            self.select_end(cx);
            return;
        }
        drag.last = event.position;
        if !drag.moved {
            let (dx, dy) = (
                f32::from(event.position.x - drag.start.x),
                f32::from(event.position.y - drag.start.y),
            );
            if dx.hypot(dy) < DRAG_SLOP {
                self.select.drag = Some(drag);
                return;
            }
            drag.moved = true;
            self.select.drag = Some(drag);
            // The selection owns the keyboard from here, so that ⌘C copies it.
            window.focus(&self.transcript_focus, cx);
            if let Some(from) = self.position_at(drag.start) {
                self.select.current = Some(Selection {
                    anchor: from,
                    head: from,
                });
            }
        }
        self.select.drag = Some(drag);
        self.select_head(event.position, cx);
        self.edge_scroll(event.position, window, cx);
    }

    /// The selection's moving end is at `point`.
    pub(super) fn select_head(&mut self, point: Point<Pixels>, cx: &mut Context<Self>) {
        let at = self.position_at(point);
        if let (Some(sel), Some(at)) = (self.select.current.as_mut(), at) {
            sel.head = at;
            cx.notify();
        }
    }

    /// The button came up: a drag ends; a click that did not drag clears the selection unless it
    /// is on the selected text (TRANSCRIPT-2.10).
    pub(super) fn select_up(&mut self, event: &MouseUpEvent, cx: &mut Context<Self>) {
        let Some(drag) = self.select.drag else {
            return;
        };
        let was_drag = drag.moved;
        self.select_end(cx);
        if was_drag || event.button != MouseButton::Left {
            return;
        }
        let inside = self.select.current.zip(self.position_at(event.position)).is_some_and(
            |(sel, at)| {
                let (a, b) = sel.ordered();
                a <= at && at <= b
            },
        );
        if !inside && self.select.current.take().is_some() {
            cx.notify();
        }
    }

    fn select_end(&mut self, _cx: &mut Context<Self>) {
        self.select.drag = None;
        self.select.scroll = 0.0;
        self.select.task = None;
    }

    /// While the pointer is held past the top or bottom of the list, the list scrolls that way,
    /// faster the further past it is (TRANSCRIPT-2.2).
    fn edge_scroll(&mut self, at: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        let area = self.select.list.get();
        let past = if at.y < area.top() {
            -f32::from(area.top() - at.y)
        } else if at.y > area.bottom() {
            f32::from(at.y - area.bottom())
        } else {
            0.0
        };
        let step = past.signum() * (past.abs() / EDGE_FULL_SPEED).min(1.0) * SCROLL_STEP_MAX;
        self.select.scroll = if past == 0.0 { 0.0 } else { step.max(2.0 * past.signum()) };
        if self.select.scroll == 0.0 {
            self.select.task = None;
            return;
        }
        if self.select.task.is_some() {
            return;
        }
        self.select.task = Some(cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(SCROLL_EVERY).await;
                let go = this
                    .update_in(cx, |this, window, cx| this.scroll_tick(window, cx))
                    .unwrap_or(false);
                if !go {
                    break;
                }
            }
        }));
    }

    /// One step of the autoscroll; whether to go on.
    pub(crate) fn scroll_tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let (Some(drag), step) = (self.select.drag, self.select.scroll) else {
            self.select.task = None;
            return false;
        };
        if step == 0.0 {
            self.select.task = None;
            return false;
        }
        self.list.scroll_by(px(step));
        cx.notify();
        // The texts move with the list: the end follows the pointer once they are painted again.
        let this = cx.entity();
        window.on_next_frame(move |_, cx| {
            this.update(cx, |this, cx| this.select_head(drag.last, cx));
        });
        true
    }

    // --- copying -------------------------------------------------------------------------------

    /// What ⌘C copies: the selected text in order, with a blank line between entries.
    pub(crate) fn selected_text(&self, cx: &App) -> Option<String> {
        let sel = self.select.current.filter(|s| !s.is_empty())?;
        let _ = cx;
        let open = |k: Key| self.open.contains(&k);
        let out_open = |id: &str| self.outputs_open.contains(&id.to_owned());
        let out_full = |id: &str| self.outputs_full.contains(&id.to_owned());
        let tree = |key: Key, text: &str| {
            self.markdown
                .get(&key)
                .map_or_else(|| reno_markdown::parse_full(text), |s| s.tree().clone())
        };
        let shown = Shown {
            open: &open,
            output_open: &out_open,
            output_full: &out_full,
            tree: &tree,
        };
        let entries = selection::reading(&self.rows, &open);
        let text = selection::copy_text(&sel, &entries, &shown);
        (!text.is_empty()).then_some(text)
    }

    /// The entries and source ranges the selection covers (TRANSCRIPT-2.13): what a later
    /// comments item anchors to.
    #[allow(dead_code)]
    pub(crate) fn selection_anchors(&self) -> Vec<selection::Anchor> {
        let Some(sel) = self.select.current.filter(|s| !s.is_empty()) else {
            return Vec::new();
        };
        let open = |k: Key| self.open.contains(&k);
        let out_open = |id: &str| self.outputs_open.contains(&id.to_owned());
        let out_full = |id: &str| self.outputs_full.contains(&id.to_owned());
        let tree = |key: Key, text: &str| {
            self.markdown
                .get(&key)
                .map_or_else(|| reno_markdown::parse_full(text), |s| s.tree().clone())
        };
        let shown = Shown {
            open: &open,
            output_open: &out_open,
            output_full: &out_full,
            tree: &tree,
        };
        let entries: Vec<Entry<'_>> = selection::reading(&self.rows, &open);
        selection::anchors(&sel, &entries, &shown)
    }

    /// ⌘C in the transcript.
    pub(super) fn copy_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text(cx) {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// Clears the selection.
    pub(crate) fn clear_selection(&mut self, cx: &mut Context<Self>) {
        if self.select.current.take().is_some() {
            cx.notify();
        }
    }

    /// The transcript's own focus, a node of no size that carries ⌘C; and the layer that
    /// follows the pointer through a drag.
    pub(super) fn select_nodes(&self, cx: &mut Context<Self>) -> [AnyElement; 2] {
        let focus = div()
            .id("transcript-focus")
            .absolute()
            .size_0()
            .key_context(CONTEXT)
            .track_focus(&self.transcript_focus)
            .on_action(cx.listener(|this, _: &CopyTranscript, _, cx| this.copy_selection(cx)))
            .into_any_element();
        let tracker = DragTracker {
            screen: cx.entity(),
            list: self.select.list.clone(),
        }
        .into_any_element();
        [focus, tracker]
    }

    /// The registry is emptied before each draw of the list: texts file themselves as they paint.
    pub(super) fn begin_paint(&self) {
        self.select.registry.borrow_mut().clear();
    }

    /// `RENO_DRAFT33_OPEN=select|copycode`: the walk's step 8 or 9, then the clipboard's text on
    /// stdout as `clipboard: ...` with line breaks written `\n`.
    #[allow(clippy::print_stdout)]
    pub(crate) fn demo_copy(&mut self, select: bool, window: &mut Window, cx: &mut Context<Self>) {
        if select {
            let first_text = |at: usize, text: &str| {
                let mut at = at.min(text.len());
                while !text.is_char_boundary(at) {
                    at -= 1;
                }
                at
            };
            let entries = selection::reading(&self.rows, &|_| false);
            let user = entries.iter().find(|e| matches!(e.row, Row::User { .. }));
            let prose = entries.iter().find(|e| matches!(e.row, Row::Prose { .. }));
            let (Some(user), Some(prose)) = (user, prose) else {
                println!("clipboard: (no prompt and reply to select)");
                return;
            };
            let (Row::User { text: asked, .. }, Row::Prose { text: reply, .. }) =
                (user.row, prose.row)
            else {
                return;
            };
            // The middle of the first paragraph of the reply.
            let tree = reno_markdown::parse_full(reply);
            let atoms = crate::vm::anchor::atoms(reply, &tree);
            let middle = atoms.first().map_or(reply.len() / 2, |a| {
                first_text((a.src.start + a.src.end) / 2, reply)
            });
            self.select.current = Some(Selection {
                anchor: Pos {
                    ordinal: user.ordinal,
                    slot: Slot::WHOLE,
                    off: first_text(asked.len() / 2, asked),
                },
                head: Pos {
                    ordinal: prose.ordinal,
                    slot: Slot::WHOLE,
                    off: middle,
                },
            });
            // The same path as the key: the transcript holds focus and ⌘C is dispatched to it.
            window.focus(&self.transcript_focus, cx);
            cx.notify();
            window.on_next_frame(|window, cx| {
                window.dispatch_action(CopyTranscript.boxed_clone(), cx);
            });
        } else {
            let code = self.rows.iter().find_map(|row| match row {
                Row::Prose { text, .. } => reno_markdown::parse_full(text)
                    .blocks
                    .iter()
                    .find_map(|top| match &top.block {
                        reno_markdown::Block::CodeBlock { code, language }
                            if language.as_deref() == Some("rust") =>
                        {
                            Some(code.trim_end_matches('\n').to_owned())
                        }
                        _ => None,
                    }),
                _ => None,
            });
            match code {
                Some(code) => crate::ui::markdown_view::chrome::copy(&code, "code", cx),
                None => println!("clipboard: (no Rust code block)"),
            }
        }
        cx.spawn_in(window, async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(500))
                .await;
            let text = cx
                .update(|_, cx| cx.read_from_clipboard().and_then(|c| c.text()))
                .ok()
                .flatten()
                .unwrap_or_default();
            println!("clipboard: {}", text.replace('\n', "\\n"));
        })
        .detach();
    }

    /// Sets the selection, as a drag would have.
    #[cfg(test)]
    pub(crate) fn set_selection(&mut self, sel: Option<Selection>, cx: &mut Context<Self>) {
        self.select.current = sel;
        cx.notify();
    }
}

fn piece_map(spot: Spot) -> Map {
    match spot.total {
        Some(_) => Map::Linear {
            base: spot.base,
            len: spot.len,
        },
        None => Map::Sourceless,
    }
}

/// Follows the pointer for a drag that began in the transcript, wherever it goes: the list's
/// own mouse events stop at its edge.
struct DragTracker {
    screen: gpui::Entity<RunScreen>,
    list: Rc<Cell<Bounds<Pixels>>>,
}

impl Element for DragTracker {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = gpui::Style::default();
        style.position = gpui::Position::Absolute;
        style.inset.top = px(0.0).into();
        style.inset.left = px(0.0).into();
        style.size = gpui::Size {
            width: gpui::relative(1.0).into(),
            height: gpui::relative(1.0).into(),
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _: &mut (),
        _window: &mut Window,
        _cx: &mut App,
    ) {
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        _cx: &mut App,
    ) {
        self.list.set(bounds);
        let screen = self.screen.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if phase.bubble() {
                screen.update(cx, |this, cx| this.select_move(event, window, cx));
            }
        });
        let screen = self.screen.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, _window, cx| {
            if phase.bubble() {
                screen.update(cx, |this, cx| this.select_up(event, cx));
            }
        });
        let _ = point(px(0.0), px(0.0));
    }
}
