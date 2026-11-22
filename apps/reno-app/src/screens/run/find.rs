//! Find in the conversation, and getting back to the live end.
//!
//! The search itself is `vm::find`, over the rows' text; this file is the screen's half: the bar
//! (its look is the draft variant's), going to a hit (opening what hides it, scrolling to it),
//! the recount that follows new events, and the "jump to latest" control that shares the
//! transcript's corners with the bar. Search opens rows into its own set, apart from the owner's,
//! so closing the bar closes again exactly what it opened.

use std::collections::HashMap;
use std::ops::Range;
use std::time::Duration;

use gpui::{
    AnyElement, Context, Entity, Focusable as _, FontWeight, ListOffset, StyledText, Subscription,
    Task, Window, div, prelude::*, px,
};

use super::RunScreen;
use crate::panel::surface::Surface;
use crate::transcript::{Key, Row};
use crate::ui::field::{Field, FieldEvent, Look};
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO, Tone};
use crate::ui::theme::Theme;
use crate::ui::{find as look, menu};
use crate::vm::draft33::Variant;
use crate::vm::find::{self, Found, Part, Source};

/// The recount follows new events and opening or closing rows at most this often.
pub(super) const RECOUNT_EVERY: Duration = Duration::from_millis(150);
/// The results list shows this many hits; the count says how many there are.
const RESULTS_SHOWN: usize = 100;

/// Where a line of an output is in its item: the step line above it and the block's padding, and
/// the height of a line, in points; and how many points of context stay above the match.
const OUTPUT_TOP: f32 = 40.0;
const OUTPUT_LINE: f32 = 18.0;
const OUTPUT_CONTEXT: f32 = 90.0;

/// What a demo hook does once the hits are in (`RENO_DRAFT33_OPEN=enter`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct Demo {
    /// Go to this hit.
    pub(crate) goto: usize,
    /// Wait for at least this many hits before going to it (the thread's runs arrive one by one).
    pub(crate) min_hits: usize,
    /// Then close search, a moment later.
    pub(crate) then_close: bool,
}

/// How long a demo waits between going to a hit and closing search.
const DEMO_CLOSE_AFTER: Duration = Duration::from_millis(1200);

#[derive(Default)]
pub(super) struct Find {
    pub(super) open: bool,
    /// The query, as the field last said it.
    pub(super) query: String,
    field: Option<(Entity<Field>, Subscription)>,
    found: Found,
    /// The hits of each row, as indexes into `found.hits`, so a row finds its own.
    by_row: HashMap<Key, Vec<usize>>,
    /// Whether the current hit was gone to since the query changed: the first Enter goes to the
    /// first hit rather than past it.
    navigated: bool,
    /// The recount waiting to run (armed by an event, not polling).
    timer: Option<Task<()>>,
    /// The calls whose output search opened, with the row each is in, to measure them again.
    opened_outputs: Vec<(String, Key)>,
    demo: Option<Demo>,
    demo_close: Option<Task<()>>,
    #[cfg(test)]
    pub(super) recounts: usize,
}

impl RunScreen {
    /// The draft variant showing; outside the draft, A's chrome.
    pub(super) fn variant(cx: &gpui::App) -> Variant {
        crate::ui::draft33::current(cx).unwrap_or(Variant::A)
    }

    pub(crate) fn find_is_open(&self) -> bool {
        self.find.open
    }

    /// "1 of 7", as the bar says it; none without a query.
    pub(crate) fn find_count_label(&self) -> Option<String> {
        if !self.find.open {
            return None;
        }
        find::count_label(
            &self.find.query,
            self.find.found.hits.len(),
            self.find.found.current,
        )
    }

    #[cfg(test)]
    pub(crate) fn find_found(&self) -> &Found {
        &self.find.found
    }

    #[cfg(test)]
    pub(crate) fn find_recounts(&self) -> usize {
        self.find.recounts
    }

    /// Whether `key` (a fold, a reasoning row, a long message) or the output of `call` is open
    /// because search opened it.
    #[cfg(test)]
    pub(crate) fn search_holds(&self) -> bool {
        self.open.search_holds()
            || self.outputs_open.search_holds()
            || self.outputs_full.search_holds()
    }

    #[cfg(test)]
    pub(crate) fn is_open(&self, key: Key) -> bool {
        self.open.contains(&key)
    }

    #[cfg(test)]
    pub(crate) fn output_is_open(&self, call: &str) -> bool {
        self.outputs_open.contains(&call.to_owned())
    }

    // --- opening and closing -----------------------------------------------------------------

    fn find_field(&mut self, cx: &mut Context<Self>) -> Entity<Field> {
        if let Some((field, _)) = &self.find.field {
            return field.clone();
        }
        let field = cx.new(|cx| {
            Field::new(
                "find-query",
                "Find in the conversation",
                "Find",
                Look::BARE,
                cx,
            )
        });
        let sub = cx.subscribe(&field, |this, field, event: &FieldEvent, cx| match event {
            FieldEvent::Changed => {
                let text = field.read(cx).text().to_owned();
                this.find_query_changed(text, cx);
            }
            FieldEvent::Submit => this.find_step(true, cx),
        });
        self.find.field = Some((field.clone(), sub));
        field
    }

    /// ⌘F: opens the bar with its query field focused, or focuses the field when it is open.
    pub(crate) fn open_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let field = self.find_field(cx);
        if !self.find.open {
            self.find.open = true;
            crate::ui::find::set_open(cx, true);
            if Self::variant(cx) == Variant::C {
                self.open_surface(Surface::Find, window, cx);
            }
            self.find_recount(cx);
        }
        window.focus(&field.read(cx).focus_handle(cx), cx);
        cx.notify();
    }

    /// Esc or the close button: the bar goes, the highlights go, and what search opened closes
    /// again. What the owner opened stays.
    pub(crate) fn close_find(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.find.open {
            return;
        }
        self.end_find(cx);
        // The keyboard goes back to the box. While an approval holds its place the box is not on
        // screen, and the keyboard must not land on the approval (a second Escape would deny it):
        // it goes to the side panel, which keeps the screen's keys working.
        if self.panel_for.is_some() && !self.keep_composer {
            window.focus(&self.panel_focus, cx);
        } else {
            window.focus(&self.focus_handle(cx), cx);
        }
    }

    fn end_find(&mut self, cx: &mut Context<Self>) {
        self.find.open = false;
        self.find.timer = None;
        self.find.demo = None;
        self.find.found = Found::default();
        self.find.by_row.clear();
        crate::ui::find::set_open(cx, false);
        let rows = self.open.end_search();
        self.outputs_full.end_search();
        self.outputs_open.end_search();
        let outputs = std::mem::take(&mut self.find.opened_outputs);
        if self.panel.tabs().contains(&Surface::Find) {
            self.panel.close(&Surface::Find);
            self.persist_panel(cx);
        }
        self.reopen_folds_closed(&rows, &outputs, cx);
        cx.notify();
    }

    /// Search closed: the folds it opened leave the list (the view lands on the head of the fold
    /// it was in), and the rows it opened are measured again.
    fn reopen_folds_closed(
        &mut self,
        rows: &[Key],
        outputs: &[(String, Key)],
        cx: &mut Context<Self>,
    ) {
        self.reproject(cx);
        for key in rows.iter().chain(outputs.iter().map(|(_, row)| row)) {
            self.remeasure(*key);
        }
    }

    // --- the query and the count ---------------------------------------------------------------

    fn find_query_changed(&mut self, text: String, cx: &mut Context<Self>) {
        self.find.query = text;
        self.find.navigated = false;
        self.find.found = Found::new(find::find_all(&self.rows, &self.find.query));
        self.find_reindex();
        self.find_demo_ready(cx);
        cx.notify();
    }

    /// Counts again over the rows now, keeping the current match where it is.
    pub(super) fn find_recount(&mut self, cx: &mut Context<Self>) {
        if !self.find.open {
            return;
        }
        let text = self
            .find
            .field
            .as_ref()
            .map_or_else(String::new, |(f, _)| f.read(cx).text().to_owned());
        self.find.query = text;
        let hits = find::find_all(&self.rows, &self.find.query);
        self.find.found = self.find.found.recounted(hits);
        self.find_reindex();
        #[cfg(test)]
        {
            self.find.recounts += 1;
        }
        self.find_demo_ready(cx);
        cx.notify();
    }

    fn find_reindex(&mut self) {
        self.find.by_row.clear();
        for (at, hit) in self.find.found.hits.iter().enumerate() {
            self.find.by_row.entry(hit.key).or_default().push(at);
        }
    }

    /// Events arrived, or a row opened or closed: the count follows, at most once per
    /// [`RECOUNT_EVERY`], from a timer this arms (nothing polls). The current match stays put.
    pub(super) fn find_note_change(&mut self, cx: &mut Context<Self>) {
        if !self.find.open || self.find.query.is_empty() || self.find.timer.is_some() {
            return;
        }
        self.find.timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(RECOUNT_EVERY).await;
            this.update(cx, |this, cx| {
                this.find.timer = None;
                this.find_recount(cx);
            })
            .ok();
        }));
    }

    // --- going to a hit ----------------------------------------------------------------------

    /// Enter or ⌘G (next), ⇧Enter or ⇧⌘G (previous): the next hit, wrapping, opened and scrolled
    /// to. The first Enter after typing goes to the first hit rather than past it.
    pub(crate) fn find_step(&mut self, forward: bool, cx: &mut Context<Self>) {
        if !self.find.open || self.find.found.hits.is_empty() {
            return;
        }
        if self.find.navigated || !forward {
            self.find.found.step(forward);
        }
        self.find.navigated = true;
        self.find_reveal(cx);
    }

    /// A result of the Find tab was chosen.
    pub(super) fn find_go(&mut self, at: usize, cx: &mut Context<Self>) {
        if self.find.found.go_to(at).is_some() {
            self.find.navigated = true;
            self.find_reveal(cx);
        }
    }

    /// The row a hit is in.
    pub(super) fn hit_row(&self, hit: &find::Hit) -> Option<&Row> {
        match hit.fold {
            Some(fold) => {
                let top = self.items.items.get(self.items.item_of(fold)?)?.top;
                match self.rows.get(top)? {
                    Row::Worked { rows, .. } => rows.iter().find(|r| r.key() == hit.key),
                    _ => None,
                }
            }
            None => self.items.row(&self.rows, self.items.item_of(hit.key)?),
        }
    }

    /// Opens what hides the current hit (its fold, its reasoning, the output and the cut part of
    /// it) into search's own sets, and scrolls to it.
    fn find_reveal(&mut self, cx: &mut Context<Self>) {
        let Some(hit) = self.find.found.current_hit().cloned() else {
            return;
        };
        let Some((need, into)) = self.hit_row(&hit).map(|row| {
            let output_can_open = match (row, hit.part) {
                (Row::Tools { calls, .. }, Part::Output(n)) => calls
                    .get(n)
                    .is_some_and(|c| find::output_can_open(calls, &c.id)),
                _ => true,
            };
            let mut need = find::reveal(&hit, row, hit.fold);
            if !output_can_open {
                need.output = None;
            }
            // A match deep in a tall output: the view rests a few lines above the match, not at
            // the top of the item.
            let into = match (hit.part, find::text_of(row, hit.part)) {
                (Part::Output(_), Some(text)) => {
                    let line = text[..hit.range.start].matches('\n').count();
                    (OUTPUT_TOP + OUTPUT_LINE * line as f32 - OUTPUT_CONTEXT).max(0.0)
                }
                _ => 0.0,
            };
            (need, into)
        }) else {
            return;
        };
        let mut opened_any = false;
        let mut fold_opened = false;
        if let Some(fold) = need.fold {
            fold_opened = self.open.search_open(fold);
            opened_any |= fold_opened;
        }
        let mut measure = Vec::new();
        if need.row && self.open.search_open(hit.key) {
            opened_any = true;
            measure.push(hit.key);
        }
        if let Some((call, cut)) = need.output {
            if self.outputs_open.search_open(call.clone()) {
                opened_any = true;
                self.find.opened_outputs.push((call.clone(), hit.key));
            }
            if cut && self.outputs_full.search_open(call.clone()) {
                opened_any = true;
                self.find.opened_outputs.push((call, hit.key));
            }
            measure.push(hit.key);
        }
        if opened_any {
            self.list.pause_following_tail();
        }
        if fold_opened {
            self.reproject(cx);
        }
        for key in measure {
            self.remeasure(key);
        }
        if let Some(ix) = self.items.item_of(hit.key) {
            // The item before it too, so the hit has a line of context above it, unless the view
            // rests inside the item (a match far down an output).
            let (item_ix, offset_in_item) = if into > 0.0 {
                (ix, px(into))
            } else {
                (ix.saturating_sub(1), px(0.0))
            };
            self.list.scroll_to(ListOffset {
                item_ix,
                offset_in_item,
            });
        }
        cx.notify();
    }

    // --- what the draw reads -----------------------------------------------------------------

    /// The matches in one text of one row, as byte ranges with whether each is the current one.
    pub(super) fn find_highlights(&self, key: Key, part: Part) -> Vec<(Range<usize>, bool)> {
        let Some(hits) = self.find.by_row.get(&key) else {
            return Vec::new();
        };
        hits.iter()
            .filter_map(|&at| {
                let hit = self.find.found.hits.get(at)?;
                (hit.part == part).then(|| (hit.range.clone(), self.find.found.current == Some(at)))
            })
            .collect()
    }

    /// Whether a row holds a match, and whether the current one is in it: for rows whose text
    /// cannot be painted over (rendered prose).
    pub(super) fn find_row_mark(&self, key: Key) -> Option<bool> {
        let hits = self.find.by_row.get(&key)?;
        Some(hits.iter().any(|&at| self.find.found.current == Some(at)))
    }

    /// `text` as one styled run with its matches painted. `ranges` are bytes of `text`; those
    /// beyond it (the cut part of a message) are left out.
    pub(super) fn find_paint(
        theme: &Theme,
        variant: Variant,
        text: impl Into<gpui::SharedString>,
        ranges: &[(Range<usize>, bool)],
    ) -> StyledText {
        let text: gpui::SharedString = text.into();
        let len = text.len();
        let highlights = ranges
            .iter()
            .filter(|(r, _)| r.end <= len && r.start < r.end)
            .map(|(r, current)| (r.clone(), look::hit_style(theme, variant, *current)))
            .collect::<Vec<_>>();
        StyledText::new(text).with_highlights(highlights)
    }

    // --- the demo hooks ------------------------------------------------------------------------

    /// `RENO_DRAFT33_OPEN=find|enter|enter3|escape`: opens find with `query` and, once the hits
    /// are in, goes to one and perhaps closes search.
    pub(crate) fn demo_find(
        &mut self,
        query: &str,
        demo: Option<Demo>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_find(window, cx);
        if let Some((field, _)) = &self.find.field {
            field.update(cx, |f, cx| f.set_text(query, cx));
        }
        self.find.demo = demo;
        self.find_query_changed(query.to_owned(), cx);
    }

    fn find_demo_ready(&mut self, cx: &mut Context<Self>) {
        let Some(demo) = self.find.demo else {
            return;
        };
        if self.find.found.hits.len() < demo.min_hits.max(demo.goto + 1) {
            return;
        }
        self.find.demo = None;
        self.find.found.go_to(demo.goto);
        self.find.navigated = true;
        self.find_reveal(cx);
        if demo.then_close {
            self.find.demo_close = Some(cx.spawn(async move |this, cx| {
                cx.background_executor().timer(DEMO_CLOSE_AFTER).await;
                this.update(cx, |this, cx| this.demo_close_find(cx)).ok();
            }));
        }
    }

    fn demo_close_find(&mut self, cx: &mut Context<Self>) {
        self.find.demo_close = None;
        if self.find.open {
            self.end_find(cx);
        }
    }

    /// `RENO_DRAFT33_OPEN=latest`: once the reply is long enough to scroll, the view is taken off
    /// its tail, as a scroll up would. Whether it was.
    pub(crate) fn demo_scroll_up(&mut self, cx: &mut Context<Self>) -> bool {
        if self.list.max_offset_for_scrollbar().y < px(1200.0) {
            return false;
        }
        self.list.scroll_by(px(-700.0));
        cx.notify();
        true
    }

    // --- jump to latest ------------------------------------------------------------------------

    /// The control that takes the view back to the end and follows the tail again.
    pub(super) fn jump_to_latest(&mut self, cx: &mut Context<Self>) {
        self.list.set_follow_mode(gpui::FollowMode::Tail);
        cx.notify();
    }

    /// "Jump to latest", while the view is not at the tail. Its place and look are the variant's.
    pub(super) fn jump_control(
        &self,
        theme: &Theme,
        variant: Variant,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if self.list.is_following_tail() || self.rows.is_empty() {
            return None;
        }
        let live = self
            .summary(cx)
            .is_some_and(|r| r.status == reno_proto::RunStatus::Running);
        let go = cx.listener(|this, _, _, cx| this.jump_to_latest(cx));
        Some(match variant {
            Variant::A => div()
                .absolute()
                .bottom(px(14.0))
                .right(px(14.0))
                .child(
                    kit::icon_button(theme, "jump-latest", Icon::ChevDown, 32.0)
                        .rounded(px(16.0))
                        .bg(theme.surface)
                        .border_color(theme.hairline)
                        .shadow_md()
                        .tooltip(kit::tip("Jump to latest"))
                        .on_click(go),
                )
                .into_any_element(),
            Variant::B => div()
                .id("jump-latest")
                .debug_selector(|| "jump-latest".to_owned())
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .h(px(28.0))
                .flex()
                .items_center()
                .justify_center()
                .gap(px(6.0))
                .bg(theme.accent.opacity(0.16))
                .border_t_1()
                .border_color(theme.accent.opacity(0.35))
                .text_size(px(12.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.accent)
                .cursor_pointer()
                .child(icon(Icon::ChevDown, 13.0, theme.accent))
                .child(if live {
                    "Latest \u{b7} streaming"
                } else {
                    "Latest"
                })
                .on_click(go)
                .into_any_element(),
            Variant::C => div()
                .absolute()
                .left_0()
                .right_0()
                .bottom(px(12.0))
                .flex()
                .justify_center()
                .child(
                    kit::pill_button(theme, "jump-latest", "Latest \u{2193}", Tone::Quiet)
                        .bg(theme.surface)
                        .border_1()
                        .border_color(theme.hairline)
                        .shadow_md()
                        .on_click(go),
                )
                .into_any_element(),
        })
    }

    // --- the bar -------------------------------------------------------------------------------

    /// The query field in a rounded box that shows when it has the keyboard.
    fn find_field_box(
        &mut self,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let field = self.find_field(cx);
        let focused = field.read(cx).focus_handle(cx).is_focused(window);
        div()
            .flex_1()
            .min_w_0()
            .h(px(28.0))
            .px(px(10.0))
            .flex()
            .items_center()
            .rounded(px(999.0))
            .bg(theme.raised)
            .border_1()
            .border_color(if focused {
                theme.accent
            } else {
                theme.hairline
            })
            .child(field)
            .into_any_element()
    }

    /// "1 of 7", or "No results".
    fn find_count_text(&self, theme: &Theme, size: f32, mono: bool) -> AnyElement {
        let label = self.find_count_label().unwrap_or_default();
        div()
            .id("find-count")
            .debug_selector(|| "find-count".to_owned())
            .flex_none()
            .min_w(px(44.0))
            .text_size(px(size))
            .text_color(theme.muted)
            .when(mono, |d| d.font_family(MONO))
            .child(gpui::SharedString::from(label))
            .into_any_element()
    }

    /// The previous, next and close buttons.
    fn find_buttons(&self, theme: &Theme, cx: &mut Context<Self>) -> [AnyElement; 3] {
        [
            kit::icon_button(theme, "find-prev", Icon::ChevUp, 26.0)
                .tooltip(kit::tip("Previous match"))
                .on_click(cx.listener(|this, _, _, cx| this.find_step(false, cx)))
                .into_any_element(),
            kit::icon_button(theme, "find-next", Icon::ChevDown, 26.0)
                .tooltip(kit::tip("Next match"))
                .on_click(cx.listener(|this, _, _, cx| this.find_step(true, cx)))
                .into_any_element(),
            kit::icon_button(theme, "find-close", Icon::X, 26.0)
                .tooltip(kit::tip("Close"))
                .on_click(cx.listener(|this, _, window, cx| this.close_find(window, cx)))
                .into_any_element(),
        ]
    }

    /// The bar above the transcript (variant A): a full-width strip under the status bar.
    pub(super) fn find_strip(
        &mut self,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let field = self.find_field_box(theme, window, cx);
        let count = self.find_count_text(theme, 13.0, false);
        let [prev, next, close] = self.find_buttons(theme, cx);
        div()
            .id("find-bar")
            .debug_selector(|| "find-bar".to_owned())
            .key_context(look::BAR_CONTEXT)
            .flex_none()
            .w_full()
            .h(px(36.0))
            .px(px(14.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .bg(theme.surface)
            .border_b_1()
            .border_color(theme.hairline)
            .text_size(px(13.0))
            .child(icon(Icon::Search, 14.0, theme.faint))
            .child(field)
            .child(count)
            .child(prev)
            .child(next)
            .child(close)
            .into_any_element()
    }

    /// The bar over the transcript (variant B): a floating 320 pt panel at the top right, with
    /// the count inside it.
    pub(super) fn find_panel(
        &mut self,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let field = self.find_field_box(theme, window, cx);
        let count = self.find_count_text(theme, 11.5, true);
        let [prev, next, close] = self.find_buttons(theme, cx);
        div()
            .absolute()
            .top(px(12.0))
            .right(px(18.0))
            .child(
                menu::panel(theme, "find-bar", 320.0)
                    .key_context(look::BAR_CONTEXT)
                    .p(px(8.0))
                    .gap(px(6.0))
                    .text_size(px(12.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.0))
                            .child(field)
                            .child(close),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(4.0))
                            .px(px(4.0))
                            .child(count)
                            .child(div().flex_1())
                            .child(prev)
                            .child(next),
                    ),
            )
            .into_any_element()
    }

    /// The Find tab of the side panel (variant C): the query field, the count and one row for
    /// each hit, with the turn it is in and the line around it.
    pub(super) fn find_results_body(
        &mut self,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let field = self.find_field_box(theme, window, cx);
        let count = self.find_count_text(theme, 12.0, false);
        let [prev, next, close] = self.find_buttons(theme, cx);
        let total = self.find.found.hits.len();
        let hits: Vec<(usize, find::Hit)> = self
            .find
            .found
            .hits
            .iter()
            .cloned()
            .enumerate()
            .take(RESULTS_SHOWN)
            .collect();
        let rows: Vec<AnyElement> = hits
            .iter()
            .map(|(at, hit)| self.find_result_row(theme, *at, hit, cx))
            .collect();
        let summary = if self.find.query.is_empty() {
            "Type to search the whole conversation".to_owned()
        } else if total == 0 {
            "No results".to_owned()
        } else {
            find::results_label(total)
        };
        div()
            .id("find-bar")
            .debug_selector(|| "find-bar".to_owned())
            .key_context(look::BAR_CONTEXT)
            .size_full()
            .flex()
            .flex_col()
            .gap(px(10.0))
            .p(px(14.0))
            .text_size(px(13.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .child(icon(Icon::Search, 14.0, theme.faint))
                    .child(field)
                    .child(close),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .px(px(2.0))
                    .child(
                        div()
                            .id("find-results-count")
                            .debug_selector(|| "find-results-count".to_owned())
                            .text_color(theme.muted)
                            .child(gpui::SharedString::from(summary)),
                    )
                    .child(div().flex_1())
                    .child(count)
                    .child(prev)
                    .child(next),
            )
            .child(
                div()
                    .id("find-results")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap(px(2.0))
                    .overflow_y_scroll()
                    .children(rows)
                    .when(total > RESULTS_SHOWN, |d| {
                        d.child(
                            div()
                                .px(px(10.0))
                                .py(px(6.0))
                                .text_size(px(12.0))
                                .text_color(theme.faint)
                                .child(gpui::SharedString::from(format!(
                                    "{} more: narrow the search",
                                    total - RESULTS_SHOWN
                                ))),
                        )
                    }),
            )
            .into_any_element()
    }

    /// One result: "Turn 3 · cargo test", and the line around the match, the match in semibold.
    fn find_result_row(
        &self,
        theme: &Theme,
        at: usize,
        hit: &find::Hit,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (place, text) = match self.hit_row(hit) {
            Some(row) => (
                place_of(hit, row),
                find::text_of(row, hit.part).unwrap_or_default().to_owned(),
            ),
            None => (hit.source.word().to_owned(), String::new()),
        };
        let snippet = find::snippet(&text, &hit.range, 36);
        let mono = matches!(hit.source, Source::Command | Source::Output);
        let current = self.find.found.current == Some(at);
        let selector = format!("find-result-{at}");
        div()
            .id(gpui::SharedString::from(selector.clone()))
            .debug_selector(move || selector.clone())
            .w_full()
            .px(px(10.0))
            .py(px(7.0))
            .flex()
            .flex_col()
            .gap(px(3.0))
            .rounded(px(8.0))
            .cursor_pointer()
            .when(current, |d| {
                d.bg(theme.row_selected())
                    .border_l_2()
                    .border_color(theme.accent)
            })
            .hover({
                let fill = theme.row_selected();
                move |s| s.bg(fill)
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.0))
                    .text_size(px(12.0))
                    .text_color(theme.faint)
                    .child(gpui::SharedString::from(format!("Turn {}", hit.turn)))
                    .child("\u{b7}")
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .child(gpui::SharedString::from(place)),
                    ),
            )
            .child(
                // One line: the words before the match, the match in semibold, the words after.
                div()
                    .w_full()
                    .flex()
                    .items_baseline()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_size(px(13.0))
                    .text_color(theme.muted)
                    .when(mono, |d| d.font_family(MONO).text_size(px(12.0)))
                    .child(
                        div()
                            .flex_none()
                            .child(gpui::SharedString::from(snippet.before)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child(gpui::SharedString::from(snippet.matched)),
                    )
                    .child(
                        div()
                            .flex_none()
                            .child(gpui::SharedString::from(snippet.after)),
                    ),
            )
            .on_click(cx.listener(move |this, _, _, cx| this.find_go(at, cx)))
            .into_any_element()
    }

    /// Match ticks on the prompt rail (variant B): one for each row that holds a match, the
    /// current one longer. Each is a place on the same scale as the prompt ticks.
    pub(super) fn find_rail_ticks(&self, theme: &Theme) -> Vec<AnyElement> {
        if !self.find.open {
            return Vec::new();
        }
        let current = self
            .find
            .found
            .current_hit()
            .map(|h| h.fold.unwrap_or(h.key));
        let mut at: Vec<(usize, bool)> = Vec::new();
        for hit in &self.find.found.hits {
            let holder = hit.fold.unwrap_or(hit.key);
            let Some(item) = self.items.item_of(holder) else {
                continue;
            };
            let top = self.items.items[item].top;
            let lit = current == Some(holder);
            match at.last_mut() {
                Some(last) if last.0 == top => last.1 |= lit,
                _ => at.push((top, lit)),
            }
        }
        at.into_iter()
            .filter_map(|(top, lit)| {
                let start = *self.row_starts.get(top)?;
                Some(
                    div()
                        .absolute()
                        .top(gpui::relative(start))
                        .right(px(1.0))
                        .w(px(if lit { 6.0 } else { 4.0 }))
                        .h(px(if lit { 3.0 } else { 2.0 }))
                        .rounded(px(1.0))
                        .bg(theme.accent)
                        .into_any_element(),
                )
            })
            .take(400)
            .collect()
    }

    /// The bar of the variant showing, over or above the transcript; `None` when closed or when
    /// the variant keeps it in the side panel.
    pub(super) fn find_bar_above(
        &mut self,
        theme: &Theme,
        variant: Variant,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        (self.find.open && variant == Variant::A).then(|| self.find_strip(theme, window, cx))
    }

    pub(super) fn find_bar_over(
        &mut self,
        theme: &Theme,
        variant: Variant,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        (self.find.open && variant == Variant::B).then(|| self.find_panel(theme, window, cx))
    }

    /// Keeps the Find tab to variant C: it shows while the bar is open, and goes when the draft
    /// moves to another variant.
    pub(super) fn sync_find_tab(&mut self, variant: Variant) {
        let has = self.panel.tabs().contains(&Surface::Find);
        if self.find.open && variant == Variant::C && !has {
            self.panel.open(Surface::Find);
        } else if has && (!self.find.open || variant != Variant::C) {
            self.panel.close(&Surface::Find);
        }
    }
}

/// "Reply", "Prompt", or the command a tool hit is in ("cargo test -p reno-harness").
fn place_of(hit: &find::Hit, row: &Row) -> String {
    match (hit.source, row, hit.part) {
        (
            Source::Command | Source::Output,
            Row::Tools { calls, .. },
            Part::Label(n) | Part::Output(n),
        ) => calls.get(n).map_or_else(
            || hit.source.word().to_owned(),
            |c| {
                c.label
                    .split_whitespace()
                    .take(4)
                    .collect::<Vec<_>>()
                    .join(" ")
            },
        ),
        (source, ..) => {
            let word = source.word();
            let mut chars = word.chars();
            chars
                .next()
                .map_or_else(String::new, |c| c.to_uppercase().chain(chars).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::{Outcome, ToolEntry, ToolKind};

    #[test]
    fn a_tool_hit_names_its_command_and_the_others_their_kind() {
        let call = ToolEntry {
            id: "c".to_owned(),
            kind: ToolKind::Command,
            label: "cargo test -p reno-harness acp::shutdown".to_owned(),
            outcome: Outcome::Ok(None),
            output: None,
        };
        let row = Row::Tools {
            key: Key(2),
            calls: vec![call],
        };
        let hits = find::find_all(std::slice::from_ref(&row), "harness");
        assert_eq!(place_of(&hits[0], &row), "cargo test -p reno-harness");
        let reply = Row::Prose {
            key: Key(4),
            text: "x".to_owned(),
        };
        let hits = find::find_all(std::slice::from_ref(&reply), "x");
        assert_eq!(place_of(&hits[0], &reply), "Reply");
    }
}
