//! Selecting text across the transcript and copying it (TRANSCRIPT-2.1, 2.2, 2.4 to 2.8, 2.10,
//! 2.13).
//!
//! The model is `vm::selection`: two source-anchored positions in reading order. This file is the
//! screen's half: it turns a pointer into a position (from the texts painted last frame, filed
//! in a registry by the renderer), keeps the drag going past the list's edge, paints what is
//! selected, and puts the text on the clipboard. Painting never changes a height, so a selection
//! never makes the list measure again.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyElement, App, Bounds, ClipboardItem, Context, Element, ElementId, GlobalElementId,
    HighlightStyle, InspectorElementId, KeyBinding, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, Pixels, Point, SharedString, StyledText, Task, Window, actions,
    div, prelude::*, px,
};

use super::RunScreen;
use crate::actions::{
    End, FocusRunInput, Home, LineDown, LineUp, NextMenuTarget, NextPrompt, OpenMenu, PageDown,
    PageUp, PrevPrompt, PreviousMenuTarget,
};
use crate::transcript::Key;
use crate::ui::find as look;
use crate::ui::markdown_view::select::{Map, Piece, Registry, Track, flatten_layers};
use crate::ui::theme::Theme;
use crate::vm::anchor;
use crate::vm::selection::{self, Entry, Pos, SelectUnit, Selection, Shown, Slot};

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
    cx.bind_keys([
        KeyBinding::new("cmd-c", CopyTranscript, Some(CONTEXT)),
        KeyBinding::new("pageup", PageUp, Some(CONTEXT)),
        KeyBinding::new("pagedown", PageDown, Some(CONTEXT)),
        KeyBinding::new("up", LineUp, Some(CONTEXT)),
        KeyBinding::new("down", LineDown, Some(CONTEXT)),
        KeyBinding::new("home", Home, Some(CONTEXT)),
        KeyBinding::new("end", End, Some(CONTEXT)),
        KeyBinding::new("shift-pageup", PrevPrompt, Some(CONTEXT)),
        KeyBinding::new("shift-pagedown", NextPrompt, Some(CONTEXT)),
        KeyBinding::new("shift-f10", OpenMenu, Some(CONTEXT)),
        KeyBinding::new("alt-up", PreviousMenuTarget, Some(CONTEXT)),
        KeyBinding::new("alt-down", NextMenuTarget, Some(CONTEXT)),
        KeyBinding::new("tab", FocusRunInput, Some(CONTEXT)),
    ]);
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
    pub(super) menu_targets: Rc<RefCell<Vec<MenuTarget>>>,
    pub(super) menu_target: Option<MenuTargetId>,
    /// Where each entry is in reading order.
    pub(super) order: HashMap<Key, usize>,
    drag: Option<Drag>,
    /// The list's bounds, as last painted.
    pub(super) list: Rc<Cell<Bounds<Pixels>>>,
    /// How far the list moves each step while the pointer is held past its edge (negative: up).
    scroll: f32,
    task: Option<Task<()>>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) struct MenuTargetId {
    pub(super) reply: Key,
    pub(super) link: Option<usize>,
}

#[derive(Clone)]
pub(super) struct MenuTarget {
    pub(super) id: MenuTargetId,
    pub(super) destination: Option<String>,
    pub(super) bounds: Bounds<Pixels>,
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

    pub(super) fn has_selection(&self) -> bool {
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

    /// The place of the character `within` bytes into the first painted text that holds
    /// `needle`, as last painted.
    #[cfg(test)]
    pub(crate) fn painted_point(&self, needle: &str, within: usize) -> Option<Point<Pixels>> {
        self.select.registry.borrow().iter().find_map(|p| {
            let at = p.layout.text().find(needle)? + within;
            let origin = p.layout.position_for_index(at)?;
            Some(origin + gpui::point(px(1.0), p.layout.line_height() / 2.0))
        })
    }

    /// Selects bytes `range` of the source of the row `key`.
    #[cfg(test)]
    pub(crate) fn select_source(&mut self, key: Key, range: Range<usize>) {
        let ordinal = self.select.order[&key];
        let anchors = self.markdown.peek(&key).map(|stream| stream.anchors());
        let at = |off, end| {
            let section = anchors.as_ref().and_then(|prose| {
                prose.sections.iter().position(|section| {
                    section.source.contains(&off) || (end && section.source.end == off)
                })
            });
            Pos {
                ordinal,
                slot: Slot::prose(section.unwrap_or(0)),
                off,
            }
        };
        self.select.current = Some(Selection {
            anchor: at(range.start, false),
            head: at(range.end, true),
        });
    }

    /// The words and place of the head line of the row `key`, as last painted.
    #[cfg(test)]
    pub(crate) fn painted_head(&self, key: Key) -> Option<(String, Bounds<Pixels>)> {
        self.select
            .registry
            .borrow()
            .iter()
            .find(|p| p.key == key && p.slot == Slot::HEAD)
            .map(|p| (p.layout.text(), p.bounds))
    }

    /// The selection, if any, as the row `key` is drawn: its entry's place in reading order.
    pub(super) fn selection_for(&self, key: Key) -> Option<(Selection, usize)> {
        let sel = self.select.current.filter(|s| !s.is_empty())?;
        Some((sel, *self.select.order.get(&key)?))
    }

    /// The highlights over one text: the selection under the find hits, which win where they meet.
    pub(super) fn piece_layers(
        &self,
        theme: &Theme,
        spot: Spot,
        text: &str,
        finds: &[(Range<usize>, bool)],
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        let mut layers = Vec::new();
        if let Some((sel, ordinal)) = self.selection_for(spot.key)
            && let Some(source) = sel.within(ordinal, spot.slot, spot.total)
        {
            let map = piece_map(spot);
            if let Some(range) = map.layout_range(&source, text.len(), spot.total.is_none()) {
                layers.push((
                    range,
                    crate::ui::markdown_view::select::selection_style(theme),
                ));
            }
        }
        let len = text.len();
        layers.extend(
            finds
                .iter()
                .filter(|(r, _)| r.end <= len && r.start < r.end)
                .map(|(r, current)| (r.clone(), look::hit_style(theme, *current))),
        );
        layers
    }

    /// One text of a row, painted with the selection and the find hits over it, and tracked so
    /// that a pointer on it is a position in the selection.
    pub(super) fn text_piece(
        &self,
        theme: &Theme,
        spot: Spot,
        text: &str,
        finds: &[(Range<usize>, bool)],
    ) -> AnyElement {
        let layers = self.piece_layers(theme, spot, text, finds);
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

    /// The text of `pieces` (painted last frame) that is nearest `point`: the one it is on or,
    /// past the texts, the nearest.
    fn nearest_piece<'a>(&self, pieces: &'a [Piece], point: Point<Pixels>) -> Option<&'a Piece> {
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
        pieces
            .iter()
            .filter(|p| p.bounds.intersects(&area) || area.size.width == px(0.0))
            .filter(|p| self.select.order.contains_key(&p.key))
            .min_by(|a, b| {
                gap(a)
                    .partial_cmp(&gap(b))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
    }

    /// The position under `point`, from the texts painted last frame.
    fn position_at(&self, point: Point<Pixels>) -> Option<Pos> {
        let pieces = self.select.registry.borrow();
        let piece = self.nearest_piece(&pieces, point)?;
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

    /// What `unit` selects at `point`: the word or drawn line under it, or the block around it.
    fn unit_selection(&self, point: Point<Pixels>, unit: SelectUnit) -> Option<Selection> {
        let at = self.position_at(point)?;
        let whole = unit == SelectUnit::Block || {
            let pieces = self.select.registry.borrow();
            self.nearest_piece(&pieces, point)?.map.is_sourceless()
        };
        if whole {
            return self.view(|entries, shown| selection::block_at(at, entries, shown));
        }
        let range = {
            let pieces = self.select.registry.borrow();
            let piece = self.nearest_piece(&pieces, point)?;
            piece.unit_at(point, unit).filter(|r| r.start < r.end)?
        };
        Some(Selection {
            anchor: Pos {
                off: range.start,
                ..at
            },
            head: Pos {
                off: range.end,
                ..at
            },
        })
    }

    /// The left button went down in the transcript: a drag may start, or a word, a line or a
    /// block is selected by the second, third and fourth press, or a shift-press extends the
    /// selection to the pointer.
    pub(super) fn select_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button != MouseButton::Left || event.modifiers.platform {
            return;
        }
        if event.modifiers.shift {
            let extended = self
                .select
                .current
                .filter(|s| !s.is_empty())
                .zip(self.position_at(event.position))
                .map(|(sel, at)| sel.extended_to(at));
            if extended.is_some() {
                self.select.current = extended;
                self.focus_transcript(window, cx);
                cx.notify();
            }
            return;
        }
        match SelectUnit::of(event.click_count) {
            SelectUnit::Char => {
                self.select.drag = Some(Drag {
                    start: event.position,
                    last: event.position,
                    moved: false,
                });
            }
            unit => {
                self.select.drag = None;
                if let Some(sel) = self.unit_selection(event.position, unit) {
                    self.select.current = Some(sel);
                    self.focus_transcript(window, cx);
                    cx.notify();
                }
            }
        }
    }

    /// The selection owns the keyboard, so that ⌘C copies it. Deferred: the press is still
    /// bubbling and its other handlers would take the focus back.
    fn focus_transcript(&self, window: &mut Window, cx: &mut Context<Self>) {
        cx.defer_in(window, |this, window, cx| {
            window.focus(&this.transcript_focus, cx);
        });
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
        let inside = self
            .select
            .current
            .zip(self.position_at(event.position))
            .is_some_and(|(sel, at)| {
                let (a, b) = sel.ordered();
                a <= at && at <= b
            });
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
        self.select.scroll = if past == 0.0 {
            0.0
        } else {
            step.max(2.0 * past.signum())
        };
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

    /// Runs `f` over the entries in reading order and what each shows.
    fn view<T>(&self, f: impl FnOnce(&[Entry<'_>], &Shown<'_>) -> T) -> T {
        let open = |k: Key| self.open.contains(&k);
        let out_open = |id: &str| self.outputs_open.contains(&id.to_owned());
        let out_full = |id: &str| self.outputs_full.contains(&id.to_owned());
        let tree = |key: Key, text: &str| {
            self.markdown
                .peek(&key)
                .map_or_else(|| reno_markdown::parse_full(text), |s| s.tree().clone())
        };
        let shown = Shown {
            open: &open,
            output_open: &out_open,
            output_full: &out_full,
            tree: &tree,
        };
        f(&selection::reading(&self.rows, &open), &shown)
    }

    /// What ⌘C copies: the selected text in order, with a blank line between entries.
    pub(crate) fn selected_text(&self) -> Option<String> {
        let sel = self.select.current.filter(|s| !s.is_empty())?;
        let text = self.view(|entries, shown| selection::copy_text(&sel, entries, shown));
        (!text.is_empty()).then_some(text)
    }

    /// The markdown source of the selection, as the owner wrote it.
    fn selected_source(&self) -> Option<String> {
        let sel = self.select.current.filter(|s| !s.is_empty())?;
        let text = self.view(|entries, shown| anchor::source_of(&sel, entries, shown));
        (!text.is_empty()).then_some(text)
    }

    /// The entries and source ranges the selection covers (TRANSCRIPT-2.13): what a later
    /// comments item anchors to.
    #[allow(dead_code)]
    pub(crate) fn selection_anchors(&self) -> Vec<selection::Anchor> {
        let Some(sel) = self.select.current.filter(|s| !s.is_empty()) else {
            return Vec::new();
        };
        self.view(|entries, shown| selection::anchors(&sel, entries, shown))
    }

    /// ⌘C in the transcript.
    pub(super) fn copy_selection(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_text() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    /// "Copy as Markdown": the selection's source bytes on the clipboard.
    pub(super) fn copy_markdown(&mut self, cx: &mut Context<Self>) {
        if let Some(text) = self.selected_source() {
            cx.write_to_clipboard(ClipboardItem::new_string(text));
        }
    }

    fn step_menu_target(&mut self, forward: bool, cx: &mut Context<Self>) {
        let policy = self.transcript_link_policy(cx);
        let targets = self.select.menu_targets.borrow();
        let mut visible: Vec<_> = targets
            .iter()
            .filter(|target| {
                target
                    .destination
                    .as_deref()
                    .is_none_or(|url| super::menu_link_allowed(&policy, url, cx))
            })
            .map(|target| target.id)
            .collect();
        visible.sort_unstable_by_key(|id| (self.items.item_of(id.reply), id.link));
        visible.dedup();
        let current = visible
            .iter()
            .position(|&id| Some(id) == self.select.menu_target);
        let next = match (current, forward) {
            (Some(at), true) => visible.get(at + 1).or_else(|| visible.first()),
            (Some(at), false) => at
                .checked_sub(1)
                .and_then(|at| visible.get(at))
                .or_else(|| visible.last()),
            (None, true) => visible.first(),
            (None, false) => visible.last(),
        };
        self.select.menu_target = next.copied();
        cx.notify();
    }

    pub(super) fn menu_target_outline(&self, theme: &Theme) -> Option<AnyElement> {
        let selected = self.select.menu_target?;
        let targets = self.select.menu_targets.clone();
        let accent = theme.accent;
        Some(
            div()
                .absolute()
                .inset_0()
                .debug_selector(move || {
                    if selected.link.is_some() {
                        "transcript-context-link"
                    } else {
                        "transcript-context-reply"
                    }
                    .to_owned()
                })
                .child(
                    gpui::canvas(
                        |_, _, _| (),
                        move |area, (), window, _| {
                            if let Some(target) =
                                targets.borrow().iter().find(|target| target.id == selected)
                            {
                                let outline = target.bounds.dilate(px(1.0)).intersect(&area);
                                window.paint_quad(gpui::outline(
                                    outline,
                                    accent,
                                    gpui::BorderStyle::Solid,
                                ));
                            }
                        },
                    )
                    .size_full(),
                )
                .into_any_element(),
        )
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
            .on_action(cx.listener(|this, _: &FocusRunInput, w, cx| this.focus_run_input(w, cx)))
            .on_action(
                cx.listener(|this, _: &OpenMenu, w, cx| this.open_transcript_menu_by_key(w, cx)),
            )
            .on_action(cx.listener(|this, _: &PreviousMenuTarget, _, cx| {
                this.step_menu_target(false, cx);
            }))
            .on_action(cx.listener(|this, _: &NextMenuTarget, _, cx| {
                this.step_menu_target(true, cx);
            }))
            .on_action(cx.listener(|this, _: &CopyTranscript, _, cx| this.copy_selection(cx)))
            .on_action(cx.listener(|this, _: &PageUp, _, cx| this.key_page(-1, cx)))
            .on_action(cx.listener(|this, _: &PageDown, _, cx| this.key_page(1, cx)))
            .on_action(cx.listener(|this, _: &LineUp, _, cx| this.key_lines(-1, cx)))
            .on_action(cx.listener(|this, _: &LineDown, _, cx| this.key_lines(1, cx)))
            .on_action(cx.listener(|this, _: &Home, _, cx| this.key_home(cx)))
            .on_action(cx.listener(|this, _: &End, _, cx| this.jump_to_latest(cx)))
            .on_action(cx.listener(|this, _: &PrevPrompt, w, cx| this.step_prompt(-1, w, cx)))
            .on_action(cx.listener(|this, _: &NextPrompt, w, cx| this.step_prompt(1, w, cx)))
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
        self.select.menu_targets.borrow_mut().clear();
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
        let style = gpui::Style {
            position: gpui::Position::Absolute,
            inset: gpui::Edges {
                top: px(0.0).into(),
                left: px(0.0).into(),
                ..gpui::Edges::default()
            },
            size: gpui::Size {
                width: gpui::relative(1.0).into(),
                height: gpui::relative(1.0).into(),
            },
            ..gpui::Style::default()
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
    }
}
