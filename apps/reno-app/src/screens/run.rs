//! The Run screen: one run's transcript, its details and a steer input.
//!
//! The transcript is a virtualised GPUI `list()`, bottom-aligned, that follows the tail while the
//! run streams. Its rows come from the pure `transcript::build_rows`; this file only draws them
//! and keeps a few per-row caches: the streaming markdown parse of each prose row and which tool
//! groups are open. A store notification rebuilds the rows, finds the first one that differs and
//! tells the list to re-measure from there, so a streamed delta costs one row, not the list.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use gpui::{
    AnyElement, Context, Entity, FocusHandle, FollowMode, FontWeight, IntoElement, ListAlignment,
    ListState, Render, SharedString, Subscription, Task, Window, div, list, prelude::*, px,
};
use reno_proto::{
    Approval, ApprovalDecision, ApprovalId, ExecutorId, Remember, RememberScope, RunId, RunStatus,
    RunSummary, StopReason, ThreadId,
};

use super::ScreenEvent;
use crate::panel::saved::Saved;
use crate::panel::{PanelKey, PanelState};
use crate::store::{RunRouting, Store};
use crate::summary;
use crate::transcript::{self as model, Built, Facts, Key, Resolution, Row, ToolEntry};
use crate::ui::composer::{Composer, ReasonEvent};
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO, Tone};
use crate::ui::markdown_view::{self, Streaming};
use crate::ui::theme::{self, Theme};
use crate::vm::changes::{self as change_facts, Loaded, Patch, PatchPane, ReviewScope};
use crate::vm::executors as executor_facts;
use crate::vm::routing::{self, RoutingView};
use crate::vm::rules;

mod changes;
mod dock;
mod header;
mod output;
mod panel;
mod session;
mod transcript;

pub(crate) use panel::PanelAction;

/// How long a burst of finished tool calls waits before the changes are read again.
const CHANGES_EVERY: Duration = Duration::from_secs(1);

/// Width of the transcript column and of the steer input.
const COLUMN: f32 = 760.0;
/// Rows the list keeps measured beyond the viewport.
const OVERDRAW: f32 = 600.0;

/// What one older run of the thread contributed to the transcript, and what it was built from:
/// a finished run is not built again until its events (or its summary) change.
#[derive(Clone)]
struct Segment {
    signature: (usize, Option<RunStatus>),
    rows: Vec<Row>,
}

pub(crate) struct RunScreen {
    store: Entity<Store>,
    /// The thread's latest run when the screen opened: the run the composer, the header and the
    /// Review read. The transcript shows every run of its thread up to this one.
    run: RunId,
    list: ListState,
    rows: Vec<Row>,
    /// How many leading rows are the older runs' (with their dividers), and what they were built
    /// from. While that holds, only the latest run's rows are built again.
    prefix_len: usize,
    prefix_runs: Vec<(RunId, (usize, Option<RunStatus>))>,
    /// Older runs' rows, by run.
    segments: HashMap<RunId, Segment>,
    /// What Review compares: the whole thread (default) or the latest run.
    scope: ReviewScope,
    /// The Session tab's "Close thread" is asking for a confirmation.
    close_confirm: bool,
    facts: Facts,
    /// One streaming parse per prose row, keyed by the row's first event.
    markdown: HashMap<Key, Streaming>,
    /// Tool groups and reasoning rows the user opened.
    open: HashSet<Key>,
    /// Tool calls whose output shows (in a group of more than one), and those showing all of it.
    outputs_open: HashSet<String>,
    outputs_full: HashSet<String>,
    /// The side panel's tabs, keyed by what the panel belongs to (the run, until threads land).
    panel_key: PanelKey,
    panel: PanelState,
    /// Whether the first tabs were chosen from what the run turned out to have, and whether the
    /// person has opened or closed a tab since (which stops that choice).
    panel_seeded: bool,
    panel_touched: bool,
    /// Focus for the panel, so its letters and its keys do not reach the steer input.
    panel_focus: FocusHandle,
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
    /// The reason field under each held action of this run: typing in it and pressing Deny (or
    /// Enter) denies with those words (ui-brief 2026-11-17, from direction B).
    reasons: HashMap<ApprovalId, (Entity<Composer>, Subscription)>,
    /// The held action whose Approve ▾ menu is open.
    menu_open: Option<ApprovalId>,
    _store: Subscription,
}

impl gpui::EventEmitter<ScreenEvent> for RunScreen {}

impl RunScreen {
    pub(crate) fn new(store: Entity<Store>, run: RunId, cx: &mut Context<Self>) -> Self {
        store.update(cx, |store, cx| store.open_run(run.clone(), cx));
        let steer = cx.new(|cx| Composer::steer(store.clone(), run.clone(), cx));
        let key = panel_key_for(store.read(cx), &run);
        let saved = Saved::get(&key, cx).or_else(|| migrated(store.read(cx), &key, cx));
        let (panel, panel_seeded) = match saved {
            // What the person left: the first-tabs choice must not undo it.
            Some(saved) => (PanelState::restore(&saved), true),
            None => (PanelState::default(), false),
        };
        let list = ListState::new(0, ListAlignment::Bottom, px(OVERDRAW));
        list.set_follow_mode(FollowMode::Tail);
        let mut screen = Self {
            _store: cx.observe(&store, |this, _, cx| {
                this.refresh(cx);
                cx.notify();
            }),
            store,
            run: run.clone(),
            list,
            rows: Vec::new(),
            prefix_len: 0,
            prefix_runs: Vec::new(),
            segments: HashMap::new(),
            scope: ReviewScope::default(),
            close_confirm: false,
            facts: Facts::default(),
            markdown: HashMap::new(),
            open: HashSet::new(),
            outputs_open: HashSet::new(),
            outputs_full: HashSet::new(),
            panel_key: key,
            panel,
            panel_seeded,
            panel_touched: false,
            panel_focus: cx.focus_handle(),
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
            reasons: HashMap::new(),
            menu_open: None,
        };
        screen.refresh(cx);
        screen
    }

    pub(crate) fn run_id(&self) -> &RunId {
        &self.run
    }

    pub(crate) fn steer_input(&self) -> &Entity<Composer> {
        &self.steer
    }

    pub(crate) fn focus_handle(&self, cx: &gpui::App) -> FocusHandle {
        use gpui::Focusable as _;
        self.steer.read(cx).focus_handle(cx)
    }

    /// Whether the card offers a reason field: the daemon takes one with a denial.
    pub(crate) fn shows_reason(&self, approval: &ApprovalId, cx: &gpui::App) -> bool {
        self.reasons.contains_key(approval)
            && self
                .store
                .read(cx)
                .state()
                .supports(reno_proto::FEATURE_APPROVAL_MESSAGE)
    }

    #[cfg(test)]
    pub(crate) fn reason_input(&self, approval: &ApprovalId) -> Option<&Entity<Composer>> {
        self.reasons.get(approval).map(|(input, _)| input)
    }

    #[cfg(test)]
    pub(crate) fn rows(&self) -> &[Row] {
        &self.rows
    }

    #[cfg(test)]
    pub(crate) fn facts(&self) -> &Facts {
        &self.facts
    }

    #[cfg(test)]
    pub(crate) fn is_following_tail(&self) -> bool {
        self.list.is_following_tail()
    }

    fn toggle(&mut self, key: Key, cx: &mut Context<Self>) {
        if !self.open.remove(&key) {
            self.open.insert(key);
        }
        // The row's height changed: measure it again.
        if let Some(ix) = self.rows.iter().position(|r| r.key() == key) {
            self.list.splice(ix..ix + 1, 1);
        }
        cx.notify();
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
        self.rebuild_rows(routing, &pending, cx);
        self.keep_reasons(&pending, cx);
        self.resolve_panel_key(cx);
        self.follow_changes(cx);
        self.follow_thread(cx);
    }

    /// The runs the transcript shows, oldest first, ending with this screen's run.
    fn thread_runs(&self, cx: &gpui::App) -> Vec<RunId> {
        let all = self.store.read(cx).thread_run_ids(&self.run);
        match all.iter().position(|r| r == &self.run) {
            Some(at) => all[..=at].to_vec(),
            None => vec![self.run.clone()],
        }
    }

    /// The thread this screen shows, once the daemon has named it.
    pub(crate) fn thread_id(&self, cx: &gpui::App) -> Option<ThreadId> {
        self.store.read(cx).thread_of(&self.run)
    }

    /// Rebuilds what changed: the latest run's rows each time, an older run's only when its
    /// events or summary moved. Rows of older runs sit before it, each run after the first
    /// behind a divider. The list is told only what differs.
    fn rebuild_rows(
        &mut self,
        routing: Option<RoutingView>,
        pending: &[Approval],
        cx: &mut Context<Self>,
    ) {
        let ids = self.thread_runs(cx);
        let (older, anchor) = ids.split_at(ids.len() - 1);
        let segment_at = ids.len() - 1;
        let state = self.store.read(cx).state();
        let signature = |id: &RunId| {
            (
                state.transcript(id).map_or(0, |t| t.events.len()),
                state.run(id).map(|r| r.status),
            )
        };
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
            model::build_rows(prompt, events, with)
        };
        let divider = |id: &RunId, at: usize, model: Option<String>| {
            state.run(id).filter(|_| at > 0).map(|r| Row::Divider {
                key: Key::divider(at),
                executor: r.executor.clone(),
                model: r.model.clone().or(model),
                at_ms: r.created_at_ms,
            })
        };
        let prefix_runs: Vec<_> = older.iter().map(|id| (id.clone(), signature(id))).collect();
        let prefix_same = prefix_runs == self.prefix_runs;
        let mut built = build(&anchor[0], pending);
        built.rows = model::with_routing(built.rows, routing);
        let mut tail: Vec<Row> = divider(&anchor[0], segment_at, built.facts.model.clone())
            .into_iter()
            .collect();
        tail.extend(built.rows.into_iter().map(|row| row.in_segment(segment_at)));
        let facts = built.facts;

        if prefix_same {
            self.apply_tail(facts, tail);
            return;
        }
        // Some older run moved: build it again, the others come from the cache.
        let mut cache = std::mem::take(&mut self.segments);
        let mut rows: Vec<Row> = Vec::new();
        for (at, (id, sig)) in prefix_runs.iter().enumerate() {
            let segment = match cache.remove(id).filter(|c| &c.signature == sig) {
                Some(segment) => segment,
                None => {
                    let built = build(id, &[]);
                    let mut seg_rows: Vec<Row> = divider(id, at, built.facts.model.clone())
                        .into_iter()
                        .collect();
                    seg_rows.extend(built.rows.into_iter().map(|row| row.in_segment(at)));
                    Segment {
                        signature: *sig,
                        rows: seg_rows,
                    }
                }
            };
            rows.extend(segment.rows.iter().cloned());
            self.segments.insert(id.clone(), segment);
        }
        self.prefix_len = rows.len();
        self.prefix_runs = prefix_runs;
        for row in &rows {
            if let Row::Prose { key, text } = row {
                self.markdown.entry(*key).or_default().set_text(text);
            }
        }
        rows.extend(tail);
        self.apply(Built { rows, facts });
    }

    /// The latest run's rows replace what followed the older runs'.
    fn apply_tail(&mut self, facts: Facts, tail: Vec<Row>) {
        self.facts = facts;
        for row in &tail {
            if let Row::Prose { key, text } = row {
                self.markdown.entry(*key).or_default().set_text(text);
            }
        }
        let at = self.prefix_len.min(self.rows.len());
        let same = self.rows[at..]
            .iter()
            .zip(&tail)
            .take_while(|(old, new)| old == new)
            .count();
        let old_len = self.rows.len() - at;
        if same < old_len || same < tail.len() {
            self.list
                .splice(at + same..self.rows.len(), tail.len() - same);
        }
        self.rows.truncate(at);
        self.rows.extend(tail);
    }

    /// Follows the thread: the panel key once the daemon names the thread, and a newer run of
    /// the thread (started elsewhere) opening in this screen's place.
    fn follow_thread(&mut self, cx: &mut Context<Self>) {
        let store = self.store.read(cx);
        let newer = store.thread_run_ids(&self.run).last().cloned().filter(|latest| {
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
            let writes = self
                .rows
                .iter()
                .filter_map(|row| match row {
                    Row::Tools { calls, .. } => Some(calls),
                    _ => None,
                })
                .flatten()
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
            if let Some(ix) = self.rows.iter().rposition(|r| matches!(r, Row::Done { .. })) {
                self.list.splice(ix..ix + 1, 1);
            }
        }
    }

    /// One reason field per held action of this run; a field goes when its action is answered.
    fn keep_reasons(&mut self, pending: &[Approval], cx: &mut Context<Self>) {
        self.reasons
            .retain(|id, _| pending.iter().any(|approval| &approval.id == id));
        if self
            .menu_open
            .as_ref()
            .is_some_and(|id| !pending.iter().any(|approval| &approval.id == id))
        {
            self.menu_open = None;
        }
        for approval in pending {
            if self.reasons.contains_key(&approval.id) {
                continue;
            }
            let store = self.store.clone();
            let input = cx.new(|cx| Composer::reason(store, cx));
            let id = approval.id.clone();
            let events = cx.subscribe(&input, move |this, _, event: &ReasonEvent, cx| {
                if let ReasonEvent::Submitted(message) = event {
                    this.deny(id.clone(), message.clone(), cx);
                }
            });
            self.reasons.insert(approval.id.clone(), (input, events));
        }
    }

    /// Deny `approval`, with the reason the agent is told.
    fn deny(&mut self, approval: ApprovalId, message: Option<String>, cx: &mut Context<Self>) {
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

    fn apply(&mut self, built: Built) {
        let Built { rows, facts } = built;
        self.facts = facts;
        for row in &rows {
            if let Row::Prose { key, text } = row {
                self.markdown.entry(*key).or_default().set_text(text);
            }
        }
        let same = self
            .rows
            .iter()
            .zip(&rows)
            .take_while(|(old, new)| old == new)
            .count();
        if same < self.rows.len() || same < rows.len() {
            self.list.splice(same..self.rows.len(), rows.len() - same);
        }
        self.rows = rows;
    }

    pub(super) fn summary<'a>(&self, cx: &'a gpui::App) -> Option<&'a RunSummary> {
        self.store.read(cx).state().run(&self.run)
    }
}

impl Render for RunScreen {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = theme::current(cx);
        let transcript = div()
            .id("transcript")
            .flex_1()
            .min_h_0()
            .w_full()
            .max_w(px(COLUMN))
            .child(
                list(
                    self.list.clone(),
                    cx.processor(|this, ix, window, cx| this.render_row(ix, window, cx)),
                )
                .size_full(),
            );
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
            .child(self.header(&theme, cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .items_center()
                    .px(px(24.0))
                    .child(transcript)
                    .child(self.dock()),
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
use transcript::{question_card, question_icon_tone};

#[cfg(test)]
mod question_card_tests;

#[cfg(test)]
mod changes_tests;

#[cfg(test)]
mod panel_tests;
#[cfg(test)]
mod persist_tests;
