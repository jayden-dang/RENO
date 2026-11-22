//! Find in the conversation, and getting back to the live end.
//!
//! The search itself is `vm::find`, over the rows' text; this file is the screen's half: the strip
//! above the transcript, going to a hit (opening what hides it, scrolling to it), the match ticks
//! on the rail, the recount that follows new events, and the "jump to latest" control. Search opens rows into its own set, apart from the owner's,
//! so closing the bar closes again exactly what it opened.

use std::collections::HashMap;
use std::ops::Range;
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    AnyElement, Context, Entity, Focusable as _, ListOffset, Subscription, Task, Window, div,
    prelude::*, px,
};

use super::{RunScreen, Segment};
use crate::transcript::{Key, Row};
use crate::ui::field::{Field, FieldEvent, Look};
use crate::ui::find as look;
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO};
use crate::ui::markdown_view::lru::Lru;
use crate::ui::theme::Theme;
use crate::vm::find::{self, Found, Hit, Matcher, Part, Place, Query};
use reno_proto::{RunId, RunStatus};

/// The recount follows new events and opening or closing rows at most this often.
pub(super) const RECOUNT_EVERY: Duration = Duration::from_millis(150);
/// Where a line of an output is in its item: the step line above it and the block's padding, and
/// the height of a line, in points; and how many points of context stay above the match.
const OUTPUT_TOP: f32 = 40.0;
const OUTPUT_LINE: f32 = 18.0;
const OUTPUT_CONTEXT: f32 = 90.0;

/// The most older-run hit lists kept (a list is capped itself, and so is the cost of all).
const HIT_LISTS: usize = 64;

/// What an older run was built from: how many events it has, and how it stands.
type Signature = (usize, Option<RunStatus>);

/// One older run's hits for one query: the run, what it was built from, and the query.
type RunKey = (RunId, Signature, Query);

struct HitCache(Lru<RunKey, Vec<Hit>>);

impl Default for HitCache {
    fn default() -> Self {
        Self(Lru::new(HIT_LISTS, 4 * find::HIT_CAP))
    }
}

#[derive(Default)]
pub(super) struct Find {
    pub(super) open: bool,
    /// The query: the text as the field last said it, and the three options.
    pub(super) query: Query,
    pub(super) matcher: Rc<Matcher>,
    /// Why the query cannot be searched with (a pattern that does not parse), when it cannot.
    error: Option<String>,
    /// What older runs held for a query, so a recount reads only the live run's text.
    hit_cache: HitCache,
    #[cfg(test)]
    pub(super) runs_scanned: usize,
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
    #[cfg(test)]
    pub(super) recounts: usize,
}

/// The rows of each older run, with what it was built from, when `rows` hold exactly those runs'
/// rows ahead of the live run's. `None` when they do not (the rows were set by hand).
fn older_runs_in<'a>(
    rows: &'a [Row],
    prefix_len: usize,
    prefix_runs: &[(RunId, Signature)],
    segments: &HashMap<RunId, Segment>,
) -> Option<Vec<(RunId, Signature, &'a [Row])>> {
    let mut start = 0;
    let mut older = Vec::with_capacity(prefix_runs.len());
    for (id, signature) in prefix_runs {
        let segment = segments.get(id).filter(|s| &s.signature == signature)?;
        let run = rows.get(start..start + segment.rows.len())?;
        let same_end = |a: Option<&Row>, b: Option<&Row>| a.map(Row::key) == b.map(Row::key);
        if !same_end(run.first(), segment.rows.first())
            || !same_end(run.last(), segment.rows.last())
        {
            return None;
        }
        older.push((id.clone(), *signature, run));
        start += run.len();
    }
    (start == prefix_len).then_some(older)
}

impl RunScreen {
    pub(crate) fn find_is_open(&self) -> bool {
        self.find.open
    }

    /// "1 of 7", as the bar says it; none without a query.
    pub(crate) fn find_count_label(&self) -> Option<String> {
        if !self.find.open {
            return None;
        }
        if self.find.error.is_some() {
            return None;
        }
        find::count_label(
            &self.find.query.text,
            self.find.found.hits.len(),
            self.find.found.current,
        )
    }

    /// Why the query cannot be searched with, while the bar is open.
    #[cfg(test)]
    pub(crate) fn find_error(&self) -> Option<&str> {
        self.find.error.as_deref().filter(|_| self.find.open)
    }

    #[cfg(test)]
    pub(crate) fn find_query(&self) -> &Query {
        &self.find.query
    }

    #[cfg(test)]
    pub(crate) fn find_runs_scanned(&self) -> usize {
        self.find.runs_scanned
    }

    #[cfg(test)]
    pub(crate) fn find_hit_lists(&self) -> usize {
        self.find.hit_cache.0.len()
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
        self.find.found = Found::default();
        self.find.by_row.clear();
        crate::ui::find::set_open(cx, false);
        let rows = self.open.end_search();
        self.outputs_full.end_search();
        self.outputs_open.end_search();
        let outputs = std::mem::take(&mut self.find.opened_outputs);
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
        self.find.query.text = text;
        self.find_asked_again(cx);
    }

    /// The query changed: the first hit is current again, and nothing is gone to.
    fn find_asked_again(&mut self, cx: &mut Context<Self>) {
        self.find_compile();
        self.find.navigated = false;
        self.find.found = Found::new(self.find_hits());
        self.find_reindex();
        cx.notify();
    }

    /// ⌥⌘C, or the button: match case on or off.
    pub(crate) fn toggle_match_case(&mut self, cx: &mut Context<Self>) {
        if self.find.open {
            self.find.query.case ^= true;
            self.find_asked_again(cx);
        }
    }

    /// ⌥⌘W, or the button: whole word on or off.
    pub(crate) fn toggle_whole_word(&mut self, cx: &mut Context<Self>) {
        if self.find.open {
            self.find.query.word ^= true;
            self.find_asked_again(cx);
        }
    }

    /// ⌥⌘X, or the button: the text is a pattern, or not.
    pub(crate) fn toggle_regex(&mut self, cx: &mut Context<Self>) {
        if self.find.open {
            self.find.query.regex ^= true;
            self.find_asked_again(cx);
        }
    }

    fn find_compile(&mut self) {
        match self.find.query.compile() {
            Ok(matcher) => {
                self.find.matcher = Rc::new(matcher);
                self.find.error = None;
            }
            Err(why) => {
                self.find.matcher = Rc::default();
                self.find.error = Some(why);
            }
        }
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
        if text != self.find.query.text {
            self.find.query.text = text;
            self.find_compile();
        }
        let hits = self.find_hits();
        // Until a hit was gone to, the current match is the first in reading order, also when
        // earlier runs arrive after the query was typed; after that it stays put.
        self.find.found = if self.find.navigated {
            self.find.found.recounted(hits)
        } else {
            Found::new(hits)
        };
        self.find_reindex();
        #[cfg(test)]
        {
            self.find.recounts += 1;
        }
        cx.notify();
    }

    /// The hits of the thread. The older runs' come from the cache when the run and the query are
    /// as they were; only the runs that are not (the live one, always) are read through.
    pub(super) fn find_hits(&mut self) -> Vec<Hit> {
        if self.find.matcher.is_empty() {
            return Vec::new();
        }
        let Some(older) = older_runs_in(
            &self.rows,
            self.prefix_len,
            &self.prefix_runs,
            &self.segments,
        ) else {
            #[cfg(test)]
            {
                self.find.runs_scanned += 1;
            }
            return find::find_all(&self.rows, &self.find.matcher);
        };
        let mut hits: Vec<Hit> = Vec::new();
        let mut at = Place::default();
        for (id, signature, rows) in older {
            if hits.len() >= find::HIT_CAP {
                break;
            }
            let key = (id, signature, self.find.query.clone());
            let local = match self.find.hit_cache.0.get(&key) {
                Some(local) => local.clone(),
                None => {
                    #[cfg(test)]
                    {
                        self.find.runs_scanned += 1;
                    }
                    let local = find::find_in_run(rows, &self.find.matcher);
                    self.find
                        .hit_cache
                        .0
                        .insert(key, local.clone(), local.len() + 1);
                    local
                }
            };
            hits.extend(find::placed(&local, at));
            let taken = Place::taken_by(rows);
            at = Place {
                rows: at.rows + taken.rows,
                turns: at.turns + taken.turns,
            };
        }
        if hits.len() < find::HIT_CAP {
            #[cfg(test)]
            {
                self.find.runs_scanned += 1;
            }
            let live = find::find_in_run(&self.rows[self.prefix_len..], &self.find.matcher);
            hits.extend(find::placed(&live, at));
        }
        hits.truncate(find::HIT_CAP);
        hits
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
        if !self.find.open || self.find.query.text.is_empty() || self.find.timer.is_some() {
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

    /// What the markdown view paints over a reply (TRANSCRIPT-3.5): the needle, and which of the
    /// reply's matches is the one the bar is on. `None` unless the bar is open and the reply
    /// holds a match.
    pub(super) fn find_marks(&self, key: Key) -> Option<Rc<crate::ui::markdown_view::Marks>> {
        if !self.find.open || self.find.query.text.is_empty() {
            return None;
        }
        let hits = self.find.by_row.get(&key)?;
        Some(Rc::new(crate::ui::markdown_view::Marks {
            matcher: self.find.matcher.clone(),
            current: hits
                .iter()
                .position(|&at| self.find.found.current == Some(at)),
        }))
    }

    // --- jump to latest ------------------------------------------------------------------------

    /// The control that takes the view back to the end and follows the tail again.
    pub(crate) fn jump_to_latest(&mut self, cx: &mut Context<Self>) {
        self.list.set_follow_mode(gpui::FollowMode::Tail);
        self.note_place(cx);
        cx.notify();
    }

    /// "Jump to latest": a round button at the bottom right, while the view is not at the tail.
    pub(super) fn jump_control(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.list.is_following_tail() || self.rows.is_empty() {
            return None;
        }
        Some(
            div()
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
                        .on_click(cx.listener(|this, _, _, cx| this.jump_to_latest(cx))),
                )
                .into_any_element(),
        )
    }

    // --- the bar -------------------------------------------------------------------------------

    /// The query field in a rounded box that shows when it has the keyboard, with the three
    /// options at its right edge. The box turns `danger` while the query cannot be searched.
    fn find_field_box(
        &mut self,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let field = self.find_field(cx);
        let focused = field.read(cx).focus_handle(cx).is_focused(window);
        let [case, word, regex] = self.find_toggles(theme, cx);
        div()
            .flex_1()
            .min_w_0()
            .h(px(28.0))
            .pl(px(10.0))
            .pr(px(4.0))
            .flex()
            .items_center()
            .gap(px(4.0))
            .rounded(px(999.0))
            .bg(theme.raised)
            .border_1()
            .border_color(if self.find.error.is_some() {
                theme.danger
            } else if focused {
                theme.accent
            } else {
                theme.hairline
            })
            .child(div().flex_1().min_w_0().child(field))
            .child(case)
            .child(word)
            .child(regex)
            .into_any_element()
    }

    /// Match case, whole word and regex, each pressed while it is on.
    fn find_toggles(&self, theme: &Theme, cx: &mut Context<Self>) -> [AnyElement; 3] {
        let query = &self.find.query;
        let toggle = |id: &'static str,
                      label: &'static str,
                      spoken: &'static str,
                      tip: &'static str,
                      on: bool| {
            // The field is `raised` already, which a pressed segment would be too: an accent tint
            // says it is on.
            kit::segment(theme, id, label, on)
                .when(on, |d| {
                    d.bg(theme.accent.opacity(0.2)).text_color(theme.accent)
                })
                .h(px(20.0))
                .px(px(7.0))
                .text_size(px(11.5))
                .aria_label(spoken)
                .tooltip(kit::tip(tip))
        };
        [
            toggle(
                "find-case",
                "Aa",
                "Match case",
                "Match case (\u{2325}\u{2318}C)",
                query.case,
            )
            .on_click(cx.listener(|this, _, _, cx| this.toggle_match_case(cx)))
            .into_any_element(),
            toggle(
                "find-word",
                "ab",
                "Whole word",
                "Whole word (\u{2325}\u{2318}W)",
                query.word,
            )
            .on_click(cx.listener(|this, _, _, cx| this.toggle_whole_word(cx)))
            .into_any_element(),
            toggle(
                "find-regex",
                ".*",
                "Regular expression",
                "Regular expression (\u{2325}\u{2318}X)",
                query.regex,
            )
            .font_family(MONO)
            .on_click(cx.listener(|this, _, _, cx| this.toggle_regex(cx)))
            .into_any_element(),
        ]
    }

    /// "1 of 7", or "No results".
    fn find_count_text(&self, theme: &Theme, size: f32, mono: bool) -> AnyElement {
        if let Some(why) = &self.find.error {
            return div()
                .id("find-error")
                .debug_selector(|| "find-error".to_owned())
                .flex_none()
                .max_w(px(220.0))
                .truncate()
                .text_size(px(size))
                .text_color(theme.danger)
                .tooltip(kit::tip(why.clone()))
                .child(gpui::SharedString::from(why.clone()))
                .into_any_element();
        }
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

    /// The bar above the transcript: a full-width strip under the status bar.
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

    /// Match ticks on the prompt rail (while the strip holds a query): one for each row that holds a match, the
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
                        .debug_selector(move || {
                            if lit {
                                "find-tick-current"
                            } else {
                                "find-tick"
                            }
                            .to_owned()
                        })
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

    /// The strip above the transcript; `None` when find is closed.
    pub(super) fn find_bar_above(
        &mut self,
        theme: &Theme,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        self.find.open.then(|| self.find_strip(theme, window, cx))
    }
}
