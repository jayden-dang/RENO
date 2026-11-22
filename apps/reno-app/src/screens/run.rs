//! The Run screen: one run's transcript, its details and a steer input.
//!
//! The transcript is a virtualised GPUI `list()`, bottom-aligned, that follows the tail while the
//! run streams. Its rows come from the pure `transcript::build_rows`, nested (a finished turn's
//! interim rows sit in its "Worked for" row); the list draws a flat projection of them
//! (`vm::items`): one item per top-level row and, while a fold is open, one per row inside it.
//! A store notification rebuilds the rows, compares the items by key and tells the list only what
//! changed: the items whose row grew are measured again in place, and the stretch that was
//! replaced (a finished turn folding) is replaced in one range, so a streamed delta costs one
//! item, not the list, and never moves the item under the scroll anchor. A reply's markdown is
//! parsed when its item is first drawn.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use gpui::{
    AnyElement, Context, Entity, FocusHandle, FollowMode, FontWeight, IntoElement, ListAlignment,
    ListOffset, ListState, Render, SharedString, Subscription, Task, Window, div, list, prelude::*,
    px,
};
use reno_client::visuals::Visual;
use reno_proto::{
    Approval, ApprovalDecision, ApprovalId, ExecutorId, Remember, RememberScope, RunId, RunStatus,
    RunSummary, Seq, StopReason, ThreadId,
};

use super::ScreenEvent;
use crate::panel::saved::Saved;
use crate::panel::{PanelKey, PanelState};
use crate::prefs::{Prefs, ThinkingMode};
use crate::store::{RunRouting, Store};
use crate::summary;
use crate::transcript::{self as model, Change, Facts, Key, Row, ToolEntry, TurnBuilder};
use crate::ui::composer::Composer;
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO, Tone};
use crate::ui::markdown_view::lru::Lru;
use crate::ui::markdown_view::{self, Streaming};
use crate::ui::theme::{self, Theme};
use crate::vm::changes::{self as change_facts, Loaded, Patch, PatchPane, ReviewScope};
use crate::vm::convo;
use crate::vm::executors as executor_facts;
use crate::vm::find::Opens;
use crate::vm::items::{self, Projection, Side};
use crate::vm::routing::{self, RoutingView};

mod activity;
mod changes;
mod dock;
mod files;
mod find;
mod history;
mod links;
mod output;
mod panel;
mod question;
mod select;
mod session;
mod syntax;
mod terminal;
mod transcript;

pub(crate) use dock::bind_keys;
#[cfg(test)]
pub(crate) use dock::keep_composer_under_panels;
pub(crate) use panel::PanelAction;

/// How long a burst of finished tool calls waits before the changes are read again.
const CHANGES_EVERY: Duration = Duration::from_secs(1);

/// Width of the transcript column and of the steer input.
const COLUMN: f32 = 760.0;
/// Width of the tick rail at the column's left edge.
const RAIL: f32 = 16.0;
/// Rows the list keeps measured beyond the viewport.
const OVERDRAW: f32 = 600.0;
/// How many items from the top of the view are measured again when the renderers settle.
const SETTLED_ITEMS: usize = 40;
/// Replies whose parsed tree is kept, and replies whose files were looked up, before the least
/// recently drawn go. A long thread has more replies than that; drawing one again parses it again.
const PARSED_REPLIES: usize = 512;
const PROBED_REPLIES: usize = 256;

/// What one older run of the thread contributed to the transcript, and what it was built from:
/// a finished run is not built again until its events (or its summary) change.
#[derive(Clone)]
struct Segment {
    signature: (usize, Option<RunStatus>),
    rows: Vec<Row>,
}

/// The line before the `at`th run of a thread (none before the first): which agent and model it
/// went to, and when. A run that does not say its model has the one its first event reported.
fn divider_row(
    state: &reno_client::State,
    id: &RunId,
    at: usize,
    reported_model: Option<String>,
) -> Option<Row> {
    state.run(id).filter(|_| at > 0).map(|r| Row::Divider {
        key: Key::divider(at),
        executor: r.executor.clone(),
        model: r.model.clone().or(reported_model),
        at_ms: r.created_at_ms,
    })
}

/// What comes before a run's events in the transcript: its divider, its prompt (stamped with the
/// run's start) and the routing card.
fn lead_rows(
    state: &reno_client::State,
    id: &RunId,
    segment: usize,
    reported_model: Option<String>,
    routing: Option<RoutingView>,
) -> Vec<Row> {
    // The whole prompt when the daemon sends it; otherwise (an older daemon) the title, which is
    // its first line, and which in the "bot . repo: what" shape shows its "what".
    let prompt = state.run(id).map(|r| {
        r.prompt
            .as_deref()
            .unwrap_or_else(|| summary::parse_title(&r.title).what)
    });
    let mut rows = model::build_rows(prompt, &[], &[]).rows;
    if let Some(run) = state.run(id) {
        model::stamp_prompt(&mut rows, run.created_at_ms);
    }
    let mut lead: Vec<Row> = divider_row(state, id, segment, reported_model)
        .into_iter()
        .collect();
    lead.extend(
        model::with_routing(rows, routing)
            .into_iter()
            .map(|row| row.in_segment(segment)),
    );
    lead
}

/// The thread's latest run, built as its events arrive: what `refresh` feeds, and what it checked
/// the screen's rows against last time.
struct Latest {
    builder: TurnBuilder,
    /// How many of the run's events the builder has taken, and the seq of the last.
    fed: usize,
    last_seq: Option<Seq>,
    /// The rows that come before the builder's in the transcript: the run's divider, its prompt
    /// and its routing card. They are not events.
    lead: Vec<Row>,
}

pub(crate) struct RunScreen {
    store: Entity<Store>,
    /// The thread's latest run when the screen opened: the run the composer, the header and the
    /// Review read. The transcript shows every run of its thread up to this one.
    run: RunId,
    list: ListState,
    /// The nested rows: what the transcript says, and what its tests read.
    rows: Vec<Row>,
    /// The flat items the list draws, over `rows` and the open folds.
    items: Projection,
    /// The prompt tick rail, and where each top-level row starts (0.0 to 1.0): made when the rows
    /// change, not on every frame.
    ticks: Vec<convo::TickMark>,
    row_starts: Vec<f32>,
    /// How many leading rows are the older runs' (with their dividers), and what they were built
    /// from. While that holds, only the latest run's rows are built again.
    prefix_len: usize,
    prefix_runs: Vec<(RunId, (usize, Option<RunStatus>))>,
    /// Older runs' rows, by run.
    segments: HashMap<RunId, Segment>,
    /// How many times an older run's rows were built (tests assert it stays put).
    #[cfg(test)]
    older_builds: usize,
    /// The latest run's rows as its events arrive; none while an approval waits on this run (its
    /// card is drawn from the state, not from the events), until it is built again.
    latest: Option<Latest>,
    /// How many times the rows were built whole (tests assert a streamed delta costs none), and
    /// how many list items were told to measure again.
    #[cfg(test)]
    rebuilds: usize,
    #[cfg(test)]
    remeasured: std::cell::Cell<usize>,
    /// What Review compares: the whole thread (default) or the latest run.
    scope: ReviewScope,
    /// The Session tab's "Close thread" is asking for a confirmation.
    close_confirm: bool,
    facts: Facts,
    /// One streaming parse per prose row, keyed by the row's first event, made when the row is
    /// first drawn. The oldest go when the thread is long; the reply still arriving never does.
    markdown: Lru<Key, Streaming>,
    live_reply: Option<Key>,
    /// Folds, tool groups, reasoning rows and long messages that are open: the owner's, and those
    /// find opened (which it closes again).
    open: Opens<Key>,
    /// The reasoning row streaming now (the last row of a live run): a third open source, held
    /// in neither set of `open`, that ends with the stream.
    streaming_open: Option<Key>,
    /// Reasoning rows the owner closed that the stream or the Thinking setting would show open.
    reasoning_shut: HashSet<Key>,
    /// The Thinking setting the item heights were last made for.
    thinking_seen: ThinkingMode,
    /// Where a Preview window is scrolled, and the row and length it was last taken to the end
    /// for (so the owner can scroll up while no new text arrives).
    reasoning_scroll: gpui::ScrollHandle,
    reasoning_scrolled: std::cell::Cell<Option<(Key, usize)>>,
    /// Tool calls whose output shows, and those showing all of it.
    outputs_open: Opens<String>,
    outputs_full: Opens<String>,
    /// Find in the conversation.
    find: find::Find,
    /// The side panel's tabs, keyed by what the panel belongs to (the run, until threads land).
    panel_key: PanelKey,
    panel: PanelState,
    /// Whether the first tabs were chosen from what the run turned out to have, and whether the
    /// person has opened or closed a tab since (which stops that choice).
    panel_seeded: bool,
    panel_touched: bool,
    /// Focus for the panel, so its letters and its keys do not reach the steer input.
    panel_focus: FocusHandle,
    /// The panel was drawn on the last frame (it is not hidden or closed).
    panel_shown: bool,
    /// The Files tab's tree, viewer and watch, made when it first shows.
    files: Option<files::FilesTab>,
    /// The Terminal tabs: a slot each, with its view made when it first shows.
    terms: terminal::Terms,
    /// The History tab's log, search and selection, made when it first shows.
    history: Option<history::HistoryTab>,
    /// Commits per page of the log (a test makes it small to see paging).
    history_page: u32,
    /// The Commit tabs, by commit, each made when it first shows.
    commits: HashMap<String, history::CommitTab>,
    /// Syntax colour of the files in diffs, by file and patch.
    syntax: syntax::Syntax,
    /// Review's files the person folded, and whether the many-files default was applied.
    review_folded: HashSet<String>,
    review_default_applied: bool,
    review: changes::Flat,
    /// One list per File tab, made when the tab first shows.
    file_views: HashMap<String, changes::Flat>,
    /// The patch rows of each file Review shows, with the answer they were made from.
    patch_panes: HashMap<String, (Loaded<Patch>, PatchPane)>,
    /// The line under the Done row naming the branch and what it holds.
    result: Option<String>,
    /// Whether the changes were asked for, the run's status and tool calls when they were last
    /// looked at, and the wait before reading them again.
    asked_changes: bool,
    last_status: Option<reno_proto::RunStatus>,
    last_writes: usize,
    changes_timer: Option<Task<()>>,
    steer: Entity<Composer>,
    /// The approval panel's focus: it never takes the keyboard on its own; a click on it or ⌘J
    /// gives it the keys (the mis-press guard).
    approval_focus: FocusHandle,
    /// The approval the panel was last drawn for, and whether the steer box stays on screen
    /// under it (the owner was typing when it opened).
    panel_for: Option<ApprovalId>,
    keep_composer: bool,
    /// The dock was drawn at least once: a panel already waiting when the screen opened is not
    /// one the owner was interrupted by.
    dock_drawn: bool,
    /// The open question's panel state, and whether a focus on the panel is owed (Esc in its
    /// line hands the keyboard back to the panel).
    question: Option<question::QuestionState>,
    question_focus: FocusHandle,
    pending_focus_panel: bool,
    /// The held action whose ▾ menu is open.
    menu_open: Option<ApprovalId>,
    /// "Deny with a note…" was chosen: the approval, its one-line field and the field's events.
    note: Option<(ApprovalId, Entity<Composer>, Subscription)>,
    /// The plan tray and the background tray over the composer are open, and the background task
    /// whose full output the Session tab shows.
    activity_plan_open: bool,
    activity_tasks_open: bool,
    activity_output: Option<String>,
    /// How many agents the transcript's chips were measured for (a chip changes a row's height).
    activity_chips: usize,
    /// Selecting text across the transcript, and the node that holds the keyboard for ⌘C.
    select: select::Select,
    transcript_focus: FocusHandle,
    /// The runs of the thread shown, oldest first; a row's run is in the high bits of its key.
    runs: Vec<RunId>,
    /// The agent-posted visuals of those runs (none with the real daemon).
    visuals: Vec<Visual>,
    /// The worktree's files a reply names, and the replies already looked through.
    file_index: links::Index,
    probed: Lru<Key, usize>,
    /// The last renderer-settled generation seen: when it moves, drawn rows are measured again.
    settled_seen: u64,
    _store: Subscription,
}

impl gpui::EventEmitter<ScreenEvent> for RunScreen {}

impl RunScreen {
    pub(crate) fn new(store: Entity<Store>, run: RunId, cx: &mut Context<Self>) -> Self {
        store.update(cx, |store, cx| store.open_run(run.clone(), cx));
        let steer = cx.new(|cx| Composer::steer(store.clone(), run.clone(), cx));
        let key = panel_key_for(store.read(cx), &run);
        let saved = Saved::get(&key, cx).or_else(|| migrated(store.read(cx), &key, cx));
        // What was saved names terminals by id; the panel names them by slot.
        let (terms, saved) = match saved {
            Some(saved) => {
                let (terms, saved) = terminal::Terms::restore(saved);
                (terms, Some(saved))
            }
            None => (terminal::Terms::default(), None),
        };
        let (panel, panel_seeded) = match saved {
            // What the person left: the first-tabs choice must not undo it.
            Some(saved) => (PanelState::restore(&saved), true),
            None => (PanelState::default(), false),
        };
        let list = ListState::new(0, ListAlignment::Bottom, px(OVERDRAW));
        list.set_follow_mode(FollowMode::Tail);
        // A scroll may have brought the view back to the tail (or taken it off): the "jump to
        // latest" control reads that on the next frame, once the list has settled.
        let this = cx.entity_id();
        list.set_scroll_handler(move |_, window, _| {
            window.on_next_frame(move |_, cx| cx.notify(this));
        });
        let mut screen = Self {
            _store: cx.observe(&store, |this, _, cx| {
                this.refresh(cx);
                cx.notify();
            }),
            store,
            run: run.clone(),
            list,
            rows: Vec::new(),
            items: Projection::default(),
            ticks: Vec::new(),
            row_starts: Vec::new(),
            prefix_len: 0,
            prefix_runs: Vec::new(),
            segments: HashMap::new(),
            #[cfg(test)]
            older_builds: 0,
            latest: None,
            #[cfg(test)]
            rebuilds: 0,
            #[cfg(test)]
            remeasured: std::cell::Cell::new(0),
            scope: ReviewScope::default(),
            close_confirm: false,
            facts: Facts::default(),
            markdown: Lru::new(PARSED_REPLIES, usize::MAX),
            live_reply: None,
            open: Opens::default(),
            streaming_open: None,
            reasoning_shut: HashSet::new(),
            thinking_seen: Prefs::thinking(cx),
            reasoning_scroll: gpui::ScrollHandle::new(),
            reasoning_scrolled: std::cell::Cell::new(None),
            outputs_open: Opens::default(),
            outputs_full: Opens::default(),
            find: find::Find::default(),
            panel_key: key,
            panel,
            panel_seeded,
            panel_touched: false,
            panel_focus: cx.focus_handle(),
            panel_shown: false,
            files: None,
            terms,
            history: None,
            history_page: crate::vm::git_graph::PAGE,
            commits: HashMap::new(),
            syntax: syntax::Syntax::default(),
            review_folded: HashSet::new(),
            review_default_applied: false,
            review: changes::Flat::new(),
            file_views: HashMap::new(),
            patch_panes: HashMap::new(),
            result: None,
            asked_changes: false,
            last_status: None,
            last_writes: 0,
            changes_timer: None,
            steer,
            approval_focus: cx.focus_handle(),
            panel_for: None,
            keep_composer: false,
            dock_drawn: false,
            question: None,
            question_focus: cx.focus_handle(),
            pending_focus_panel: false,
            menu_open: None,
            note: None,
            activity_plan_open: false,
            activity_tasks_open: false,
            activity_output: None,
            activity_chips: 0,
            select: select::Select::default(),
            transcript_focus: cx.focus_handle(),
            runs: Vec::new(),
            visuals: Vec::new(),
            file_index: links::Index::default(),
            probed: Lru::new(PROBED_REPLIES, usize::MAX),
            settled_seen: markdown_view::settled_generation(cx),
        };
        screen.refresh(cx);
        screen
    }

    pub(crate) fn run_id(&self) -> &RunId {
        &self.run
    }

    /// The "write your own" line of the question panel, while it shows.
    #[cfg(test)]
    pub(crate) fn question_input(&self) -> Option<&Entity<Composer>> {
        self.question.as_ref().map(|q| &q.input)
    }

    /// The inline refusal under the question panel.
    #[cfg(test)]
    pub(crate) fn question_refusal(&self, cx: &gpui::App) -> Option<String> {
        let q = self.question.as_ref()?;
        let agent = self
            .store
            .read(cx)
            .state()
            .run(&self.run)
            .map(|r| summary::executor_name(r.executor.as_str()))?;
        q.refusal.as_ref().map(|r| r.message(&agent))
    }

    /// The question panel's page.
    #[cfg(test)]
    pub(crate) fn question_page(&self) -> Option<usize> {
        self.question.as_ref().map(|q| q.draft.page)
    }

    pub(crate) fn steer_input(&self) -> &Entity<Composer> {
        &self.steer
    }

    pub(crate) fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        use gpui::Focusable as _;
        self.steer.read(cx).focus_handle(cx)
    }

    #[cfg(test)]
    pub(crate) fn note_input(&self) -> Option<&Entity<Composer>> {
        self.note.as_ref().map(|(_, input, _)| input)
    }

    #[cfg(test)]
    pub(crate) fn older_builds(&self) -> usize {
        self.older_builds
    }

    /// How many times the rows were built whole.
    #[cfg(test)]
    pub(crate) fn rebuilds(&self) -> usize {
        self.rebuilds
    }

    /// How many list items were told to measure again.
    #[cfg(test)]
    pub(crate) fn remeasured(&self) -> usize {
        self.remeasured.get()
    }

    #[cfg(test)]
    pub(crate) fn panel_key(&self) -> &PanelKey {
        &self.panel_key
    }

    #[cfg(test)]
    pub(crate) fn panel_state(&self) -> &PanelState {
        &self.panel
    }

    #[cfg(test)]
    pub(crate) fn rows(&self) -> &[Row] {
        &self.rows
    }

    #[cfg(test)]
    pub(crate) fn facts(&self) -> &Facts {
        &self.facts
    }

    /// The first top-level row the list shows (an item inside a fold is its fold's row).
    #[cfg(test)]
    pub(crate) fn top_row(&self) -> usize {
        self.items
            .top_of(self.list.logical_scroll_top().item_ix, self.rows.len())
    }

    /// The flat items the list draws, for tests.
    #[cfg(test)]
    pub(crate) fn items(&self) -> &Projection {
        &self.items
    }

    #[cfg(test)]
    pub(crate) fn item_count(&self) -> usize {
        self.list.item_count()
    }

    #[cfg(test)]
    pub(crate) fn is_following_tail(&self) -> bool {
        self.list.is_following_tail()
    }

    /// The owner opens or closes a fold, a reasoning row, a tool group or a long message.
    /// Opening one while the view follows the tail stops following and puts what opened at the
    /// top of the view, so what grew stays in view.
    fn toggle(&mut self, key: Key, cx: &mut Context<Self>) {
        let opening = !self.open.contains(&key);
        let following = self.list.is_following_tail();
        self.open.toggle(key);
        self.reflow(key, cx);
        if opening && following {
            self.keep_in_view(key);
        }
    }

    /// The owner opens or closes a reasoning row. What the stream or the Thinking setting shows
    /// open is closed for good by a click, and what the owner opens stays open once the stream
    /// ends.
    pub(super) fn toggle_reasoning(&mut self, key: Key, cx: &mut Context<Self>) {
        if self.reasoning_open(key, cx) {
            if self.open.contains(&key) {
                self.open.toggle(key);
            }
            self.reasoning_shut.insert(key);
            self.reflow(key, cx);
        } else {
            self.reasoning_shut.remove(&key);
            self.toggle(key, cx);
        }
    }

    /// Whether the reasoning row `key` shows its text.
    pub(super) fn reasoning_open(&self, key: Key, cx: &gpui::App) -> bool {
        let by_setting = match Prefs::thinking(cx) {
            ThinkingMode::Open => true,
            ThinkingMode::Closed => false,
            ThinkingMode::Auto | ThinkingMode::Preview => self.streaming_open == Some(key),
        };
        self.open.contains(&key) || (by_setting && !self.reasoning_shut.contains(&key))
    }

    /// The reasoning row that is the end of a live run is the one streaming; when that changes
    /// the old and the new row are measured again.
    fn track_streaming_reasoning(&mut self, cx: &Context<Self>) {
        let streaming = match self.rows.last() {
            Some(Row::Reasoning { key, .. }) if self.is_live(cx) => Some(*key),
            _ => None,
        };
        if streaming == self.streaming_open {
            return;
        }
        let before = std::mem::replace(&mut self.streaming_open, streaming);
        for key in before.into_iter().chain(streaming) {
            self.remeasure(key);
        }
    }

    /// The Thinking setting changed: the reasoning rows are measured again.
    fn follow_thinking_setting(&mut self, cx: &gpui::App) {
        let now = Prefs::thinking(cx);
        if now == self.thinking_seen {
            return;
        }
        self.thinking_seen = now;
        for ix in 0..self.items.len() {
            if matches!(self.items.row(&self.rows, ix), Some(Row::Reasoning { .. })) {
                self.measure_again(ix);
            }
        }
    }

    /// Stops following the tail, with the item of `key` at the top of the view.
    pub(super) fn keep_in_view(&mut self, key: Key) {
        if let Some(ix) = self.items.item_of(key) {
            self.list.scroll_to(ListOffset {
                item_ix: ix,
                offset_in_item: px(0.0),
            });
        }
    }

    /// What `key` shows changed: a fold's rows come into the list or leave it; anything else is
    /// one item that is measured again.
    pub(super) fn reflow(&mut self, key: Key, cx: &mut Context<Self>) {
        let is_fold = self
            .items
            .item_of(key)
            .and_then(|ix| self.items.row(&self.rows, ix))
            .is_some_and(|row| matches!(row, Row::Worked { .. }));
        if is_fold {
            self.reproject(cx);
        } else {
            self.remeasure(key);
        }
        self.find_note_change(cx);
        cx.notify();
    }

    /// The row holding `key` changed height: measure its item again.
    pub(super) fn remeasure(&mut self, key: Key) {
        if let Some(ix) = self.items.item_of(key) {
            self.measure_again(ix);
        }
    }

    /// Item `ix` is measured again the next time it is drawn; its old height stands until then,
    /// and the view's place in it does not move.
    fn measure_again(&self, ix: usize) {
        #[cfg(test)]
        self.remeasured.set(self.remeasured.get() + 1);
        self.list.remeasure_items(ix..ix + 1);
    }

    /// The open folds changed: the items are made again over the same rows, and the list is told
    /// which came or went. The fold's head keeps its place.
    fn reproject(&mut self, cx: &mut Context<Self>) {
        self.replace_rows(0, None, cx);
    }

    /// Makes the items of the rows from top-level row `from` on (the rows before it did not
    /// change) and tells the list what differs. `rows` are the new rows from `from`: `None`
    /// when the rows stay and only the open folds changed.
    fn replace_rows(&mut self, from: usize, rows: Option<Vec<Row>>, cx: &mut Context<Self>) {
        let from = from.min(self.rows.len());
        let rows_replaced = rows.is_some();
        let old_rows = match rows {
            Some(new) => {
                let old = self.rows.split_off(from);
                self.rows.extend(new);
                old
            }
            // The rows stay where they are: compare them with themselves.
            None => Vec::new(),
        };
        let same_rows = old_rows.is_empty() && from == 0;
        let was_last_done = self
            .items
            .last_done_item()
            .map(|ix| self.items.items[ix].key);
        let before = self.items.len();
        let open = &self.open;
        let old_items = self
            .items
            .reproject(&self.rows, from, |k| open.contains(&k));
        let at = before - old_items.len();
        let replaced = rows_replaced;
        let plan = items::diff(
            Side {
                rows: if same_rows { &self.rows } else { &old_rows },
                base: if same_rows { 0 } else { from },
                items: &old_items,
            },
            Side {
                rows: &self.rows,
                base: 0,
                items: &self.items.items[at..],
            },
            at,
        );
        self.apply_plan(&plan);
        // The Done that says what the branch holds moved to another row: both are measured again.
        let now_last_done = self
            .items
            .last_done_item()
            .map(|ix| self.items.items[ix].key);
        if was_last_done != now_last_done {
            for key in was_last_done.into_iter().chain(now_last_done) {
                self.remeasure(key);
            }
        }
        if replaced && !plan.is_empty() {
            self.rail_changed();
            // New events: the find count follows, soon, from a timer this arms.
            self.find_note_change(cx);
        }
    }

    /// Tells the list what [`items::diff`] found.
    fn apply_plan(&self, plan: &items::Plan) {
        if let Some((range, count)) = &plan.splice {
            self.list.splice(range.clone(), *count);
        }
        for &ix in &plan.remeasure {
            self.measure_again(ix);
        }
    }

    /// The rows changed: the tick rail is worked out again, once, not on every frame.
    fn rail_changed(&mut self) {
        self.reorder();
        self.ticks = convo::tick_marks(&self.rows);
        self.row_starts = convo::row_starts(&self.rows);
    }

    /// Rebuilds the rows from the store and tells the list what changed.
    fn refresh(&mut self, cx: &mut Context<Self>) {
        // The routing decision changes while a run starts (a fallback): read it again when the
        // run's executor or status does. Asking is a no-op while the answer held is current.
        let run = self.run.clone();
        self.store
            .update(cx, |store, cx| store.ensure_routing(&run, cx));
        let routing = match self.store.read(cx).routing(&self.run) {
            Some(RunRouting::Ready(Some(decision))) => Some(routing::view(decision)),
            _ => None,
        };
        let pending: Vec<Approval> = self
            .store
            .read(cx)
            .state()
            .approvals_for(&self.run)
            .cloned()
            .collect();
        if !self.advance_latest(routing.clone(), &pending, cx) {
            self.rebuild_rows(routing, &pending, cx);
        }
        self.track_streaming_reasoning(cx);
        self.sync_visuals(cx);
        self.measure_activity_chips(cx);
        self.keep_menu(&pending);
        self.resolve_panel_key(cx);
        self.follow_changes(cx);
        self.follow_thread(cx);
    }

    /// Chips under the transcript's agent text change those rows' height: measure the prose rows
    /// they hang under again when the run's agents appear or go.
    fn measure_activity_chips(&mut self, cx: &gpui::App) {
        let spawned: Vec<usize> = self
            .store
            .read(cx)
            .run_activity(&self.run)
            .map(|a| a.agents.iter().map(|l| l.spawned_after).collect())
            .unwrap_or_default();
        if spawned.len() == self.activity_chips {
            return;
        }
        self.activity_chips = spawned.len();
        for n in spawned {
            if let Some(ix) = self.items.prose_item(n) {
                self.measure_again(ix);
            }
        }
    }

    /// Puts the agent-posted visuals after the entries they were posted with, once each: none with
    /// a connection that offers none (TRANSCRIPT-12.1, 12.3).
    fn sync_visuals(&mut self, cx: &mut Context<Self>) {
        let mut visuals = Vec::new();
        let mut posted: HashMap<Key, Vec<usize>> = HashMap::new();
        let store = self.store.read(cx);
        for (segment, run) in self.runs.iter().enumerate() {
            for visual in store.run_visuals(run) {
                if let Some(host) = items::host_of(&self.rows, segment, visual.after.0) {
                    posted.entry(host).or_default().push(visuals.len());
                    visuals.push(visual);
                }
            }
        }
        let moved = self.items.set_posted(posted);
        if moved || visuals != self.visuals {
            self.visuals = visuals;
            self.reproject(cx);
        }
    }

    /// The renderers settled something (a diagram, an image, a wrap): a row they drew may be taller
    /// or shorter now, so the rows in view that hold their output are measured again and none is
    /// drawn over the one below.
    fn remeasure_settled(&mut self, cx: &gpui::App) {
        let seen = markdown_view::settled_generation(cx);
        if seen == self.settled_seen {
            return;
        }
        self.settled_seen = seen;
        let top = self.list.logical_scroll_top().item_ix;
        let end = (top + SETTLED_ITEMS).min(self.items.len());
        for ix in top..end {
            let holds_output = self.items.items[ix].visual.is_some()
                || matches!(self.items.row(&self.rows, ix), Some(Row::Prose { .. }));
            if holds_output {
                self.measure_again(ix);
            }
        }
    }

    /// The runs the transcript shows, oldest first, ending with this screen's run.
    fn thread_runs(&self, cx: &gpui::App) -> Vec<RunId> {
        let all = self.store.read(cx).thread_run_ids(&self.run);
        match all.iter().position(|r| r == &self.run) {
            Some(at) => all[..=at].to_vec(),
            None => vec![self.run.clone()],
        }
    }

    /// The latest run's rows from the events that arrived since the last time, when that is all
    /// there is to do: the older runs are as they were, nothing waits on an approval, the events
    /// are the ones the builder has taken with more after them, and the rows are the ones it made.
    /// Then only the first row that differs, the one before it and the rows after them are made
    /// again as items. Whether it did; if not, the rows are built whole.
    fn advance_latest(
        &mut self,
        routing: Option<RoutingView>,
        pending: &[Approval],
        cx: &mut Context<Self>,
    ) -> bool {
        if !pending.is_empty() || self.latest.is_none() {
            return false;
        }
        let ids = self.thread_runs(cx);
        let segment = ids.len() - 1;
        let state = self.store.read(cx).state();
        let Some(run) = state.run(&ids[segment]) else {
            return false;
        };
        let events = state
            .transcript(&ids[segment])
            .map_or(&[][..], |t| t.events.as_slice());
        let older = ids[..segment]
            .iter()
            .map(|id| (id.clone(), Self::signature(state, id)))
            .collect::<Vec<_>>();
        let Some(latest) = self.latest.as_mut() else {
            return false;
        };
        let continues = latest.fed <= events.len()
            && latest
                .fed
                .checked_sub(1)
                .is_none_or(|last| Some(events[last].0) == latest.last_seq);
        let mirrored =
            self.rows.len() == self.prefix_len + latest.lead.len() + latest.builder.rows().len();
        if ids != self.runs || older != self.prefix_runs || !continues || !mirrored {
            return false;
        }

        let mut first: Option<usize> = None;
        let mut note = |at: Option<usize>| {
            if let Some(at) = at {
                first = Some(first.map_or(at, |was| was.min(at)));
            }
        };
        for (seq, event) in &events[latest.fed..] {
            let change = latest.builder.push(*seq, event);
            note(Self::first_row(latest.builder.rows(), &change));
        }
        latest.fed = events.len();
        latest.last_seq = events.last().map(|(seq, _)| *seq);
        let timed = latest.builder.set_run_time(
            run.status.is_terminal(),
            run.created_at_ms,
            run.updated_at_ms,
        );
        note(Self::first_row(latest.builder.rows(), &timed));
        self.facts = latest.builder.facts().clone();

        let lead = lead_rows(
            state,
            &ids[segment],
            segment,
            latest.builder.facts().model.clone(),
            routing,
        );
        let lead_len = latest.lead.len();
        let (from, tail) = if lead != latest.lead {
            latest.lead = lead;
            let mut tail = latest.lead.clone();
            tail.extend_from_slice(latest.builder.rows());
            (0, tail)
        } else if let Some(at) = first {
            // The row before the first that differs goes with it: it was the last one, or
            // the prose before a `Done`, and is not what it was.
            let start = (lead_len + at).saturating_sub(1);
            let mut tail = Vec::new();
            if start < lead_len {
                tail.extend_from_slice(&latest.lead[start..]);
                tail.extend_from_slice(latest.builder.rows());
            } else {
                tail.extend_from_slice(&latest.builder.rows()[start - lead_len..]);
            }
            (start, tail)
        } else {
            return true;
        };
        self.replace_rows(self.prefix_len + from, Some(tail), cx);
        true
    }

    /// The first top-level row a change says is not what it was.
    fn first_row(rows: &[Row], change: &Change) -> Option<usize> {
        match change {
            Change::Unchanged => None,
            Change::Folded { turn, .. } => Some(turn.start),
            Change::Grew(key) | Change::Added(key) => {
                rows.iter().rposition(|row| row.key() == *key)
            }
        }
    }

    /// What an older run's rows were built from: how many events it has, and how it stands.
    fn signature(state: &reno_client::State, id: &RunId) -> (usize, Option<RunStatus>) {
        (
            state.transcript(id).map_or(0, |t| t.events.len()),
            state.run(id).map(|r| r.status),
        )
    }

    /// Builds the rows whole: the latest run's, and an older run's when its events or summary
    /// moved. Rows of older runs sit before it, each run after the first behind a divider. The
    /// list is told only what differs.
    fn rebuild_rows(
        &mut self,
        routing: Option<RoutingView>,
        pending: &[Approval],
        cx: &mut Context<Self>,
    ) {
        #[cfg(test)]
        {
            self.rebuilds += 1;
        }
        let ids = self.thread_runs(cx);
        self.runs.clone_from(&ids);
        let (older, anchor) = ids.split_at(ids.len() - 1);
        let segment_at = ids.len() - 1;
        let state = self.store.read(cx).state();
        let build = |id: &RunId, with: &[Approval]| {
            let events = state
                .transcript(id)
                .map_or(&[][..], |t| t.events.as_slice());
            // The whole prompt when the daemon sends it; otherwise (an older daemon) the title,
            // which is its first line, and which in the "bot . repo: what" shape shows its "what".
            let prompt = state.run(id).map(|r| {
                r.prompt
                    .as_deref()
                    .unwrap_or_else(|| summary::parse_title(&r.title).what)
            });
            let mut built = model::build_rows(prompt, events, with);
            if let Some(run) = state.run(id) {
                model::stamp_prompt(&mut built.rows, run.created_at_ms);
                let elapsed = convo::run_elapsed_ms(
                    run.status.is_terminal(),
                    run.created_at_ms,
                    run.updated_at_ms,
                    events,
                );
                built.rows = model::fold_turns(built.rows, elapsed);
            }
            built
        };
        let prefix_runs: Vec<_> = older
            .iter()
            .map(|id| (id.clone(), Self::signature(state, id)))
            .collect();
        let prefix_same = prefix_runs == self.prefix_runs;
        let (tail, latest) = match (pending.is_empty(), state.run(&anchor[0])) {
            (true, Some(run)) => {
                let events = state
                    .transcript(&anchor[0])
                    .map_or(&[][..], |t| t.events.as_slice());
                let mut builder = TurnBuilder::new(segment_at);
                for (seq, event) in events {
                    builder.push(*seq, event);
                }
                builder.set_run_time(
                    run.status.is_terminal(),
                    run.created_at_ms,
                    run.updated_at_ms,
                );
                let lead = lead_rows(
                    state,
                    &anchor[0],
                    segment_at,
                    builder.facts().model.clone(),
                    routing,
                );
                let mut tail = lead.clone();
                tail.extend_from_slice(builder.rows());
                let latest = Latest {
                    fed: events.len(),
                    last_seq: events.last().map(|(seq, _)| *seq),
                    builder,
                    lead,
                };
                self.facts = latest.builder.facts().clone();
                (tail, Some(latest))
            }
            _ => {
                let mut built = build(&anchor[0], pending);
                built.rows = model::with_routing(built.rows, routing);
                let mut tail: Vec<Row> =
                    divider_row(state, &anchor[0], segment_at, built.facts.model.clone())
                        .into_iter()
                        .collect();
                tail.extend(built.rows.into_iter().map(|row| row.in_segment(segment_at)));
                self.facts = built.facts;
                (tail, None)
            }
        };
        self.latest = latest;

        if prefix_same {
            // The older runs' rows and items stay: only the latest run's are made again.
            let from = self.prefix_len.min(self.rows.len());
            self.replace_rows(from, Some(tail), cx);
            return;
        }
        // Some older run moved: build it again, the others come from the cache. The runs before
        // the first that moved, and their rows and items, stay as they are.
        let same_upto = prefix_runs
            .iter()
            .zip(&self.prefix_runs)
            .take_while(|(now, was)| now == was)
            .count();
        let mut cache = std::mem::take(&mut self.segments);
        let mut from = 0;
        let mut rows: Vec<Row> = Vec::new();
        for (at, (id, sig)) in prefix_runs.iter().enumerate() {
            let segment = match cache.remove(id).filter(|c| &c.signature == sig) {
                Some(segment) => segment,
                None => {
                    #[cfg(test)]
                    {
                        self.older_builds += 1;
                    }
                    let events = state
                        .transcript(id)
                        .map_or(&[][..], |t| t.events.as_slice());
                    let mut built = model::build_rows(None, events, &[]);
                    if let Some(run) = state.run(id) {
                        let elapsed = convo::run_elapsed_ms(
                            run.status.is_terminal(),
                            run.created_at_ms,
                            run.updated_at_ms,
                            events,
                        );
                        built.rows = model::fold_turns(built.rows, elapsed);
                    }
                    let mut seg_rows = lead_rows(state, id, at, built.facts.model.clone(), None);
                    seg_rows.extend(built.rows.into_iter().map(|row| row.in_segment(at)));
                    Segment {
                        signature: *sig,
                        rows: seg_rows,
                    }
                }
            };
            if at < same_upto {
                from += segment.rows.len();
            } else {
                rows.extend(segment.rows.iter().cloned());
            }
            self.segments.insert(id.clone(), segment);
        }
        self.prefix_len = from + rows.len();
        self.prefix_runs = prefix_runs;
        rows.extend(tail);
        self.replace_rows(from, Some(rows), cx);
    }

    /// Opens every fold of a finished turn (a screenshot hook: `RENO_OPEN_FOLDS=1`).
    pub(crate) fn open_all_folds(&mut self, cx: &mut Context<Self>) {
        let folds: Vec<Key> = self
            .rows
            .iter()
            .filter(|row| matches!(row, Row::Worked { .. }))
            .map(Row::key)
            .collect();
        if folds.is_empty() {
            return;
        }
        for key in folds {
            self.open.owner_open(key);
        }
        self.list.pause_following_tail();
        self.reproject(cx);
        self.find_note_change(cx);
        cx.notify();
    }

    /// `RENO_BENCH=1`: once the transcript has stopped growing, scroll it from its end to its
    /// start and back, a fixed step a frame, and print the time between frames (`bench.rs`).
    pub(crate) fn bench_scroll(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use crate::bench::{self, Pass, ScrollPlan};
        use std::cell::RefCell;
        use std::rc::Rc;
        use std::time::Instant;

        struct Script {
            list: ListState,
            plan: ScrollPlan,
            last_frame: Option<Instant>,
            intervals: Vec<Duration>,
        }
        fn step(script: Rc<RefCell<Script>>, window: &mut Window, _cx: &mut gpui::App) {
            {
                let mut s = script.borrow_mut();
                let now = Instant::now();
                if let Some(last) = s.last_frame.replace(now) {
                    s.intervals.push(now - last);
                }
                let before = s.list.logical_scroll_top();
                match s.plan.pass() {
                    Pass::Up => s.list.scroll_by(px(-bench::SCROLL_STEP_PX)),
                    Pass::Down => s.list.scroll_by(px(bench::SCROLL_STEP_PX)),
                    Pass::Done => {}
                }
                let moved = s.list.logical_scroll_top() != before;
                s.plan.after_step(moved);
                if s.plan.pass() == Pass::Done {
                    bench::report_scroll(s.list.item_count(), &s.intervals);
                    return;
                }
            }
            window.refresh();
            window.on_next_frame(move |window, cx| step(script, window, cx));
        }

        let list = self.list.clone();
        cx.spawn_in(window, async move |_, cx| {
            // The rows are in when the count has held for a second and a half.
            let mut held = 0;
            let mut count = 0;
            while held < 6 {
                cx.background_executor()
                    .timer(Duration::from_millis(250))
                    .await;
                let now = list.item_count();
                held = if now == count && now > 0 { held + 1 } else { 0 };
                count = now;
            }
            cx.update(|window, cx| {
                let script = Rc::new(RefCell::new(Script {
                    list,
                    plan: ScrollPlan::new(),
                    last_frame: None,
                    intervals: Vec::new(),
                }));
                step(script, window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Brings the row at `ix` to the top of the list (a tick on the rail was clicked).
    fn scroll_to_row(&mut self, ix: usize, cx: &mut Context<Self>) {
        self.list.scroll_to(ListOffset {
            item_ix: self.items.item_of_top(ix),
            offset_in_item: px(0.0),
        });
        cx.notify();
    }

    /// The thin rail at the column's left edge: one tick for each user message; a click scrolls
    /// to it.
    fn tick_rail(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let marks = self.ticks.as_slice();
        let scrolls = f32::from(self.list.max_offset_for_scrollbar().y);
        if marks.is_empty() || !convo::rail_visible(scrolls) {
            return None;
        }
        let top = self
            .items
            .top_of(self.list.logical_scroll_top().item_ix, self.rows.len());
        let current = convo::current_tick(marks, top);
        let (faint, text) = (theme.faint.opacity(0.6), theme.text);
        Some(
            div()
                .id("tick-rail")
                .debug_selector(|| "tick-rail".to_owned())
                .absolute()
                .left_0()
                .top_0()
                .bottom_0()
                .w(px(RAIL))
                .children(marks.iter().enumerate().map(|(n, mark)| {
                    let ix = mark.row;
                    let lit = current == Some(n);
                    let selector = format!("tick-{ix}");
                    div()
                        .id(SharedString::from(selector.clone()))
                        .debug_selector(move || selector.clone())
                        .absolute()
                        .top(gpui::relative(mark.at))
                        .left_0()
                        .w(px(RAIL))
                        .h(px(12.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .cursor_pointer()
                        .child(
                            div()
                                .w(px(if lit { 10.0 } else { 7.0 }))
                                .h(px(2.0))
                                .rounded(px(1.0))
                                .bg(if lit { text } else { faint })
                                .when(lit, |d| d.debug_selector(|| "tick-current".to_owned()))
                                .hover(move |s| s.bg(text)),
                        )
                        .on_click(cx.listener(move |this, _, _, cx| this.scroll_to_row(ix, cx)))
                }))
                // Where the matches are, beside the prompt ticks.
                .children(self.find_rail_ticks(theme))
                .into_any_element(),
        )
    }

    /// Follows the thread: the panel key once the daemon names the thread, and a newer run of
    /// the thread (started elsewhere) opening in this screen's place.
    fn follow_thread(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let newer = store
            .thread_run_ids(&self.run)
            .last()
            .cloned()
            .filter(|latest| {
                let state = store.state();
                latest != &self.run
                    && state
                        .run(latest)
                        .zip(state.run(&self.run))
                        .is_some_and(|(l, mine)| l.created_at_ms > mine.created_at_ms)
            });
        if let Some(latest) = newer {
            cx.emit(ScreenEvent::OpenRun(latest));
        }
    }

    /// The panel's key is the thread once the daemon says which it is. Until then (a run just
    /// started) it is the run; the tabs the person has carry over.
    fn resolve_panel_key(&mut self, cx: &mut Context<Self>) {
        let want = panel_key_for(self.store.read(cx), &self.run);
        if want == self.panel_key {
            return;
        }
        self.panel_key = want;
        match Saved::get(&self.panel_key, cx) {
            // The thread's tabs are already known (an earlier run of it): they win, unless the
            // person has changed this panel since it opened.
            Some(saved) if !self.panel_touched => {
                self.panel = PanelState::restore(&saved);
                self.panel_seeded = true;
            }
            _ => self.persist_panel(cx),
        }
    }

    /// Keeps what the run changed current: read when the run opens and when it ends, and about
    /// once a second while it works (a tool call that can write finished, or its status moved).
    fn follow_changes(&mut self, cx: &mut Context<Self>) {
        if self.store.read(cx).supports_changes() {
            let status = self.summary(cx).map(|r| r.status);
            let ended = status.is_some_and(RunStatus::is_terminal);
            let writes = tool_calls(&self.rows)
                .filter(|c| c.kind != model::ToolKind::Other && c.finished())
                .count();
            let moved = self.last_status != status || writes > self.last_writes;
            if !self.asked_changes || (ended && self.last_status != status) {
                self.asked_changes = true;
                self.reload_changes(cx);
            } else if moved {
                self.schedule_changes(cx);
            }
            self.last_status = status;
            self.last_writes = writes;
        }
        self.update_result(cx);
        self.seed_panel(cx);
        self.ensure_patches(cx);
    }

    /// Reads the changes in a second, once however many calls finish meanwhile.
    fn schedule_changes(&mut self, cx: &mut Context<Self>) {
        if self.changes_timer.is_some() {
            return;
        }
        self.changes_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CHANGES_EVERY).await;
            this.update(cx, |this, cx| {
                this.changes_timer = None;
                this.reload_changes(cx);
            })
            .ok();
        }));
    }

    /// Sets the result line under the Done row and measures that row again when it changes.
    fn update_result(&mut self, cx: &mut Context<Self>) {
        let ended = self.summary(cx).is_some_and(|r| r.status.is_terminal());
        let line = match (ended, self.pane(cx)) {
            (true, change_facts::Pane::Files(_)) => self
                .store
                .read(cx)
                .changes(&self.run)
                .and_then(|c| match &c.diff {
                    Loaded::Ready(d) => change_facts::result_line(d),
                    _ => None,
                }),
            _ => None,
        };
        if line != self.result {
            self.result = line;
            if let Some(ix) = self.items.last_done_item() {
                self.measure_again(ix);
            }
        }
    }

    /// The ▾ menu closes with its approval.
    fn keep_menu(&mut self, pending: &[Approval]) {
        if self
            .menu_open
            .as_ref()
            .is_some_and(|id| !pending.iter().any(|approval| &approval.id == id))
        {
            self.menu_open = None;
        }
        if self
            .note
            .as_ref()
            .is_some_and(|(id, ..)| !pending.iter().any(|approval| &approval.id == id))
        {
            self.note = None;
        }
    }

    /// Deny `approval`, with the reason the agent is told.
    fn deny(&mut self, approval: ApprovalId, message: Option<String>, cx: &mut Context<Self>) {
        self.note = None;
        let reasons = self
            .store
            .read(cx)
            .state()
            .supports(reno_proto::FEATURE_APPROVAL_MESSAGE);
        let message = message.filter(|_| reasons);
        self.store.update(cx, |store, cx| {
            store.decide_with_message(approval, ApprovalDecision::Deny, message, cx);
        });
    }

    /// Approve `approval`, keeping the answer as far as `scope` reaches (`None`: once).
    fn approve(
        &mut self,
        approval: ApprovalId,
        scope: Option<RememberScope>,
        cx: &mut Context<Self>,
    ) {
        self.menu_open = None;
        let remember = scope.map(|scope| Remember {
            scope,
            expires_in_secs: None,
        });
        self.store.update(cx, |store, cx| {
            store.decide_remembering(approval, ApprovalDecision::ApproveOnce, None, remember, cx);
        });
    }

    pub(super) fn summary<'a>(&self, cx: &'a gpui::App) -> Option<&'a RunSummary> {
        self.store.read(cx).state().run(&self.run)
    }
}

impl Render for RunScreen {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        // Settings › General › Show hidden files.
        let show_hidden = crate::prefs::current(cx).hidden_files;
        if let Some(files) = self.files.as_mut() {
            files.model.set_show_hidden(show_hidden);
        }
        self.remeasure_settled(cx);
        self.follow_thinking_setting(cx);
        // The texts file themselves again as the list paints them.
        self.begin_paint();
        let rail = self.tick_rail(&theme, cx);
        let transcript = div()
            .id("transcript")
            .debug_selector(|| "transcript".to_owned())
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .max_w(px(COLUMN))
            .flex()
            .flex_col()
            .child(
                div()
                    .w_full()
                    .flex_1()
                    .min_h_0()
                    .pl(px(RAIL))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(|this, event, _, _| this.select_down(event)),
                    )
                    .child(
                        list(
                            self.list.clone(),
                            cx.processor(|this, ix, _, cx| this.render_row(ix, cx)),
                        )
                        .size_full(),
                    ),
            )
            // After the list, so it is on top of it and takes the clicks.
            .children(self.select_nodes(cx))
            .children(rail)
            .children(self.jump_control(&theme, cx));
        let chat = div()
            .id("chat-canvas")
            .debug_selector(|| "chat-canvas".to_owned())
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .rounded(px(14.0))
            .bg(theme.canvas)
            .border_1()
            .border_color(theme.hairline)
            .overflow_hidden()
            .children(self.find_bar_above(&theme, window, cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .px(px(24.0))
                    .child(transcript)
                    .child(self.dock(window, cx)),
            );

        let stage = div()
            .id("run")
            .key_context("Run")
            .size_full()
            .relative()
            .flex()
            .child(chat);
        self.with_panel(stage, &theme, window, cx)
    }
}

/// Every tool call of `rows`, those inside a fold too.
fn tool_calls(rows: &[Row]) -> Box<dyn Iterator<Item = &ToolEntry> + '_> {
    Box::new(rows.iter().flat_map(|row| match row {
        Row::Tools { calls, .. } => Box::new(calls.iter()) as Box<dyn Iterator<Item = &ToolEntry>>,
        Row::Worked { rows, .. } => tool_calls(rows),
        _ => Box::new(std::iter::empty()),
    }))
}

/// The panel key of `run`: its thread when the daemon has threads and has named it, else the run.
fn panel_key_for(store: &Store, run: &RunId) -> PanelKey {
    match store.thread_of(run) {
        Some(thread) if store.supports_threads() => PanelKey(thread.to_string()),
        _ => PanelKey(run.to_string()),
    }
}

/// What an earlier version saved for the thread's first run under its run id: a thread of one run
/// takes it over, so tabs survive the move to thread keys. A key already saved wins (the caller
/// asks only when there is none).
fn migrated(store: &Store, key: &PanelKey, cx: &gpui::App) -> Option<crate::panel::Descriptors> {
    let thread = ThreadId::from_suffix(key.0.strip_prefix("thr_")?).ok()?;
    let info = store.state().thread(&thread)?;
    let [only] = info.runs.as_slice() else {
        return None;
    };
    Saved::get(&PanelKey(only.run_id.to_string()), cx)
}

#[cfg(test)]
mod changes_tests;

#[cfg(test)]
mod convo_tests;

#[cfg(test)]
mod files_tests;
#[cfg(test)]
mod find_tests;
#[cfg(test)]
mod history_tests;
#[cfg(test)]
mod panel_tests;
#[cfg(test)]
mod persist_tests;
#[cfg(test)]
mod select_tests;
#[cfg(test)]
mod terminal_tests;
