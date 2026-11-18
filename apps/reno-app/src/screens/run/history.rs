//! The History tab of the side panel (the commit graph) and the Commit tab it opens (one commit's
//! diff, drawn by the same virtualised rows as Review).
//!
//! `vm::git_graph` decides the lanes, the badges, paging, search and which commits can open a
//! diff; this file sends what it asks the daemon for, applies the answers (each under the ticket
//! it was asked with) and draws the rows with `list()` and a `canvas` per graph cell.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::time::Duration;

use gpui::{
    AnyElement, Bounds, Context, Corners, Entity, FontWeight, Hsla, ListAlignment, ListState,
    PathBuilder, SharedString, Subscription, Task, Window, canvas, div, fill, hsla, list, point,
    prelude::*, px, size,
};
use reno_client::{Client, ClientError};
use reno_proto::{
    DiffScope, GitCommit, GitLogParams, GitLogRev, GitLogSearchParams, GitRefKind, RunId,
    WorkspaceDiffResult,
};

use super::RunScreen;
use super::changes::{Flat, Scope};
use super::output::copy;
use crate::panel::surface::Surface;
use crate::panel::view::Prefs;
use crate::summary;
use crate::ui::composer::{Composer, ReasonEvent};
use crate::ui::icon::Icon;
use crate::ui::kit::{self, MONO, Tone};
use crate::ui::theme::Theme;
use crate::vm::changes::{self as facts, Loaded, Patch, PatchPane};
use crate::vm::git_graph::{
    self as graph, Badge, GraphRow, HistoryModel, Openable, Request, SearchState, SegmentShape,
    Status,
};
use crate::vm::review::review_rows;

/// How long typing waits before the search is sent.
const SEARCH_DEBOUNCE: Duration = Duration::from_millis(180);
/// A row of the log.
const ROW: f32 = 36.0;
/// Rows the list keeps measured beyond the viewport.
const OVERDRAW: f32 = 360.0;
/// Below this panel width only the graph and the subject show.
pub(super) const NARROW_BELOW: f32 = 520.0;
/// The node's diameter, the lane spacing and the padding around the lanes.
const NODE: f32 = 7.0;
const LANE: f32 = 12.0;
const PAD: f32 = 10.0;
/// The graph never takes more than this share of the panel.
const GRAPH_SHARE: f32 = 0.34;
/// Rows from the end at which the next page is asked for.
const NEAR_END: usize = 12;
/// Above this many files a commit opens with all but the first few folded.
const OPEN_ALL_UP_TO: usize = 8;
const OPEN_FIRST: usize = 3;

/// One run's History tab, kept while the run screen is open (the tab shows and hides).
pub(super) struct HistoryTab {
    pub(super) model: HistoryModel,
    search: Entity<Composer>,
    _search_events: [Subscription; 2],
    search_timer: Option<Task<()>>,
    list: ListState,
    /// How many rows the list was told about.
    counted: usize,
    /// The tab was hidden since it last read: the next show reads the log again.
    stale: bool,
}

/// One Commit tab: the commit's files and their patches, read when it shows.
pub(super) struct CommitTab {
    diff: Loaded<WorkspaceDiffResult>,
    patches: HashMap<String, Loaded<Patch>>,
    pub(super) panes: HashMap<String, (Loaded<Patch>, PatchPane)>,
    pub(super) folded: HashSet<String>,
    default_applied: bool,
    pub(super) flat: Flat,
}

impl CommitTab {
    fn new() -> Self {
        Self {
            diff: Loaded::Loading,
            patches: HashMap::new(),
            panes: HashMap::new(),
            folded: HashSet::new(),
            default_applied: false,
            flat: Flat::new(),
        }
    }
}

fn say(error: &ClientError) -> String {
    match error {
        ClientError::Remote(object) => object.message.clone(),
        other => other.to_string(),
    }
}

/// The colour of a lane.
fn lane_color(theme: &Theme, color_id: usize) -> Hsla {
    let hue = ((color_id as f32) * 0.137 + 0.58).fract();
    if theme.appearance.is_dark() {
        hsla(hue, 0.62, 0.64, 1.0)
    } else {
        hsla(hue, 0.60, 0.42, 1.0)
    }
}

fn badge_color(theme: &Theme, kind: GitRefKind) -> Hsla {
    match kind {
        GitRefKind::Head => theme.accent,
        GitRefKind::Branch => theme.success,
        GitRefKind::Remote => theme.muted,
        GitRefKind::Tag => theme.warning,
    }
}

fn author_color(theme: &Theme, key: &str) -> Hsla {
    let hue = graph::author_hue(key);
    if theme.appearance.is_dark() {
        hsla(hue, 0.55, 0.62, 1.0)
    } else {
        hsla(hue, 0.55, 0.44, 1.0)
    }
}

/// How wide the graph is for `lanes` lanes in a panel of `panel` px, and the spacing between
/// lanes (squeezed when the lanes would take more than their share).
fn geometry(lanes: usize, panel: f32) -> (f32, f32) {
    let lanes = lanes.max(1);
    let natural = PAD * 2.0 + NODE + (lanes - 1) as f32 * LANE;
    let cap = (panel * GRAPH_SHARE).max(48.0);
    if natural <= cap || lanes == 1 {
        return (natural.min(cap.max(natural)), LANE);
    }
    let spacing = ((cap - PAD * 2.0 - NODE) / (lanes - 1) as f32).clamp(3.0, LANE);
    (PAD * 2.0 + NODE + (lanes - 1) as f32 * spacing, spacing)
}

/// Paints the lines and the node of one row of the graph into `bounds`.
fn paint_graph(
    bounds: Bounds<gpui::Pixels>,
    row: &GraphRow,
    spacing: f32,
    theme: &Theme,
    window: &mut Window,
) {
    let x = |lane: usize| bounds.origin.x + px(PAD + NODE / 2.0 + lane as f32 * spacing);
    // A hair past the row's edges, so adjacent rows meet without a seam.
    let top = bounds.origin.y - px(0.5);
    let mid = bounds.origin.y + bounds.size.height / 2.0;
    let bottom = bounds.origin.y + bounds.size.height + px(0.5);
    for segment in &row.segments {
        let (from, to) = match segment.shape {
            SegmentShape::Through => (
                point(x(segment.from_lane), top),
                point(x(segment.to_lane), bottom),
            ),
            SegmentShape::Incoming => (
                point(x(segment.from_lane), top),
                point(x(row.node_lane), mid),
            ),
            SegmentShape::Outgoing => (
                point(x(row.node_lane), mid),
                point(x(segment.to_lane), bottom),
            ),
        };
        let mut path = PathBuilder::stroke(px(1.5));
        path.move_to(from);
        if from.x == to.x {
            path.line_to(to);
        } else {
            let half = (from.y + to.y) / 2.0;
            path.cubic_bezier_to(to, point(from.x, half), point(to.x, half));
        }
        if let Ok(path) = path.build() {
            window.paint_path(path, lane_color(theme, segment.color_id));
        }
    }
    let center = point(x(row.node_lane), mid);
    let color = lane_color(theme, row.node_color_id);
    if row.is_head {
        let ring = NODE + 5.0;
        window.paint_quad(
            gpui::outline(
                Bounds::centered_at(center, size(px(ring), px(ring))),
                color,
                gpui::BorderStyle::Solid,
            )
            .corner_radii(Corners::all(px(ring / 2.0))),
        );
    }
    window.paint_quad(
        fill(Bounds::centered_at(center, size(px(NODE), px(NODE))), color)
            .corner_radii(Corners::all(px(NODE / 2.0))),
    );
}

impl RunScreen {
    // --- when it runs -------------------------------------------------------------------------

    /// Whether the panel shows `surface` in front now.
    fn shows(&self, surface: &Surface, cx: &gpui::App) -> bool {
        self.panel.active() == surface && self.panel_shown && Prefs::get(cx).open != Some(false)
    }

    /// Starts or refreshes what the History and Commit tabs read, once they show. Called wherever
    /// the panel's tabs change and when the store does.
    pub(super) fn sync_history(&mut self, cx: &mut Context<Self>) {
        let supported = self.store.read(cx).supports_history()
            && self.store.read(cx).history_client().is_some();
        if !supported {
            return;
        }
        if self.shows(&Surface::History, cx) {
            if self.history.is_none() {
                let tab = self.new_history_tab(cx);
                self.history = Some(tab);
                self.history_reload(cx);
            } else if let Some(tab) = self.history.as_mut()
                && std::mem::take(&mut tab.stale)
            {
                self.history_reload(cx);
            }
        } else if let Some(tab) = self.history.as_mut() {
            tab.stale = true;
        }
        if let Surface::Commit(sha) = self.panel.active().clone()
            && self.shows(&Surface::Commit(sha.clone()), cx)
        {
            self.sync_commit(&sha, cx);
        }
    }

    fn new_history_tab(&mut self, cx: &mut Context<Self>) -> HistoryTab {
        let store = self.store.clone();
        let search = cx.new(|cx| {
            Composer::field(
                store,
                "Search commits",
                "message, author or sha \u{b7} Esc clears",
                cx,
            )
        });
        let observed = cx.observe(&search, |this, field, cx| {
            let text = field.read(cx).typed_reason().unwrap_or_default();
            this.history_search_text(&text, cx);
        });
        let events = cx.subscribe(&search, |this, _, event: &ReasonEvent, cx| {
            if let ReasonEvent::Submitted(_) = event {
                // Enter does not wait out the debounce.
                this.commit_history_search(cx);
            }
        });
        let list = ListState::new(0, ListAlignment::Top, px(OVERDRAW));
        let this = cx.weak_entity();
        list.set_scroll_handler(move |event, _, cx| {
            let end = event.visible_range.end;
            this.update(cx, |screen, cx| screen.history_scrolled(end, cx))
                .ok();
        });
        HistoryTab {
            model: HistoryModel::with_page_size(self.history_page),
            search,
            _search_events: [observed, events],
            search_timer: None,
            list,
            counted: 0,
            stale: false,
        }
    }

    // --- talking to the daemon ----------------------------------------------------------------

    fn history_call<T, Fut>(
        &mut self,
        cx: &mut Context<Self>,
        call: impl FnOnce(Client, RunId) -> Fut,
        done: impl FnOnce(&mut Self, Result<T, String>, &mut Context<Self>) + 'static,
    ) where
        T: Send + 'static,
        Fut: Future<Output = Result<T, ClientError>> + Send + 'static,
    {
        let Some(client) = self.store.read(cx).history_client() else {
            done(self, Err("the daemon is not connected".to_owned()), cx);
            return;
        };
        let task = gpui_tokio::Tokio::spawn(cx, call(client, self.run.clone()));
        cx.spawn(async move |this, cx| {
            let result = match task.await {
                Ok(Ok(value)) => Ok(value),
                Ok(Err(error)) => Err(say(&error)),
                Err(error) => Err(error.to_string()),
            };
            this.update(cx, |screen, cx| done(screen, result, cx)).ok();
        })
        .detach();
    }

    /// Reads the log again (the first read, "Try again", a tab shown after being hidden).
    pub(crate) fn history_reload(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.history.as_mut() else {
            return;
        };
        let reqs = tab.model.start();
        self.run_history(reqs, cx);
        cx.notify();
    }

    fn run_history(&mut self, requests: Vec<Request>, cx: &mut Context<Self>) {
        for request in requests {
            match request {
                Request::Page {
                    cursor,
                    limit,
                    ticket,
                } => {
                    let first = cursor.is_none();
                    self.history_call(
                        cx,
                        move |client, run| async move {
                            client
                                .git_log(GitLogParams {
                                    run_id: run,
                                    cursor,
                                    limit: Some(limit),
                                    rev: Some(GitLogRev::Branch),
                                })
                                .await
                        },
                        move |this, result, cx| {
                            this.with_history(cx, |m| m.apply_page(ticket, first, result));
                        },
                    );
                }
                Request::Thread { ticket } => self.history_call(
                    cx,
                    move |client, run| async move {
                        client
                            .git_log(GitLogParams {
                                run_id: run,
                                cursor: None,
                                limit: Some(reno_proto::GIT_LOG_MAX_LIMIT),
                                rev: Some(GitLogRev::Thread),
                            })
                            .await
                    },
                    move |this, result, cx| {
                        this.with_history(cx, |m| m.apply_thread(ticket, result));
                    },
                ),
                Request::Search { query, ticket } => self.history_call(
                    cx,
                    move |client, run| async move {
                        client
                            .git_log_search(GitLogSearchParams {
                                run_id: run,
                                query,
                                limit: Some(graph::SEARCH_LIMIT),
                                rev: Some(GitLogRev::Branch),
                            })
                            .await
                    },
                    move |this, result, cx| {
                        this.with_history(cx, |m| m.apply_search(ticket, result));
                    },
                ),
            }
        }
    }

    /// Runs `apply` on the model, then tells the list what rows it has now.
    fn with_history(&mut self, cx: &mut Context<Self>, apply: impl FnOnce(&mut HistoryModel)) {
        let Some(tab) = self.history.as_mut() else {
            return;
        };
        let before = tab.model.rows().first().map(|c| c.sha.clone());
        let was = tab.counted;
        apply(&mut tab.model);
        // One more row than there are commits: the footer that says what follows the log.
        let now = tab.model.rows().len() + 1;
        let same_head = tab.model.rows().first().map(|c| c.sha.clone()) == before;
        if same_head && now >= was && was > 0 {
            // Pages appended below: scrolling and measured rows stay.
            tab.list.splice(was - 1..was, now - was + 1);
        } else {
            tab.list.reset_with_uniform_height(now, px(ROW));
        }
        tab.counted = now;
        cx.notify();
    }

    // --- commands -----------------------------------------------------------------------------

    fn history_search_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(tab) = self.history.as_mut() else {
            return;
        };
        if !tab.model.set_query(text) {
            return;
        }
        tab.search_timer = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(SEARCH_DEBOUNCE).await;
            this.update(cx, |screen, cx| screen.commit_history_search(cx))
                .ok();
        }));
        cx.notify();
    }

    fn commit_history_search(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.history.as_mut() else {
            return;
        };
        tab.search_timer = None;
        let request = tab.model.commit_search();
        // The list shows other rows now (matches, or the log again).
        self.with_history(cx, |_| {});
        if let Some(request) = request {
            self.run_history(vec![request], cx);
        }
        cx.notify();
    }

    /// The list scrolled to rows up to `end`: near its end, the next page is asked for.
    fn history_scrolled(&mut self, end: usize, cx: &mut Context<Self>) {
        let Some(tab) = self.history.as_ref() else {
            return;
        };
        if end + NEAR_END >= tab.model.rows().len() {
            self.history_more(cx);
        }
    }

    /// Asks for the next page of the log.
    pub(crate) fn history_more(&mut self, cx: &mut Context<Self>) {
        let Some(tab) = self.history.as_mut() else {
            return;
        };
        let request = tab.model.load_more();
        self.with_history(cx, |_| {});
        if let Some(request) = request {
            self.run_history(vec![request], cx);
        }
    }

    /// A click on a row: selects it and opens its diff, unless the commit is older than the
    /// thread's base (the daemon refuses those, so no diff is offered).
    pub(crate) fn history_click(&mut self, sha: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(tab) = self.history.as_mut() else {
            return;
        };
        tab.model.select(Some(sha));
        let openable = tab.model.openable(sha);
        cx.notify();
        if openable != Openable::BeforeBase {
            self.open_surface(Surface::Commit(sha.to_owned()), window, cx);
        }
    }

    /// Up and down move the selection; Enter opens it.
    pub(super) fn history_key(
        &mut self,
        key: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(tab) = self.history.as_mut() else {
            return false;
        };
        match key {
            "down" | "up" => {
                let by = if key == "down" { 1 } else { -1 };
                if let Some(ix) = tab.model.step(by) {
                    tab.list.scroll_to_reveal_item(ix);
                }
                cx.notify();
                true
            }
            "enter" => {
                if let Some(sha) = tab.model.selected().map(str::to_owned) {
                    self.history_click(&sha, window, cx);
                }
                true
            }
            _ => false,
        }
    }

    fn toggle_badge(&mut self, key: &str, cx: &mut Context<Self>) {
        self.with_history(cx, |m| m.toggle_branch(key));
    }

    // --- the Commit tab -----------------------------------------------------------------------

    /// Makes the Commit tab of `sha` when it first shows, and reads what it lacks: the files of
    /// the commit, then the patch of each file shown open.
    fn sync_commit(&mut self, sha: &str, cx: &mut Context<Self>) {
        if !self.commits.contains_key(sha) {
            self.commits.insert(sha.to_owned(), CommitTab::new());
            self.ask_commit_files(sha, cx);
        }
        let split = Prefs::get(cx).split;
        let Some(tab) = self.commits.get_mut(sha) else {
            return;
        };
        let Loaded::Ready(diff) = &tab.diff else {
            return;
        };
        let rows = facts::file_rows(&diff.files);
        if !tab.default_applied {
            tab.default_applied = true;
            if rows.len() > OPEN_ALL_UP_TO {
                tab.folded
                    .extend(rows.iter().skip(OPEN_FIRST).map(|r| r.path.clone()));
            }
        }
        let wanted: Vec<String> = rows
            .iter()
            .filter(|r| !r.binary && !tab.folded.contains(&r.path))
            .filter(|r| !tab.patches.contains_key(&r.path))
            .map(|r| r.path.clone())
            .collect();
        for path in &wanted {
            tab.patches.insert(path.clone(), Loaded::Loading);
        }
        for row in &rows {
            if let Some(loaded) = tab.patches.get(&row.path)
                && tab.panes.get(&row.path).map(|(l, _)| l) != Some(loaded)
            {
                let pane = facts::patch_pane(row, loaded);
                tab.panes.insert(row.path.clone(), (loaded.clone(), pane));
            }
        }
        let flat_rows = review_rows(
            &rows,
            |p| tab.folded.contains(p),
            |p| tab.panes.get(p).map(|(_, pane)| pane),
            split,
        );
        tab.flat.sync(rows, flat_rows);
        // The colours, for the patches that are in.
        let ready: Vec<(String, String)> = tab
            .patches
            .iter()
            .filter_map(|(path, loaded)| match loaded {
                Loaded::Ready(p) => Some((path.clone(), p.checksum.clone())),
                _ => None,
            })
            .collect();
        let origin = format!("commit:{sha}");
        for (path, checksum) in ready {
            self.ensure_syntax(
                &origin,
                Some(DiffScope::Commit {
                    sha: sha.to_owned(),
                }),
                &path,
                &checksum,
                cx,
            );
        }
        for path in wanted {
            self.ask_commit_patch(sha, &path, cx);
        }
    }

    fn ask_commit_files(&mut self, sha: &str, cx: &mut Context<Self>) {
        let at = sha.to_owned();
        let named = sha.to_owned();
        self.history_call(
            cx,
            move |client, run| async move {
                client
                    .workspace_diff_scoped(run, None, DiffScope::Commit { sha: at })
                    .await
            },
            move |this, result, cx| {
                if let Some(tab) = this.commits.get_mut(&named) {
                    tab.diff = match result {
                        Ok(diff) => Loaded::Ready(diff),
                        Err(why) => Loaded::Failed(why),
                    };
                }
                this.sync_history(cx);
                cx.notify();
            },
        );
    }

    fn ask_commit_patch(&mut self, sha: &str, path: &str, cx: &mut Context<Self>) {
        let (at, file) = (sha.to_owned(), path.to_owned());
        let (named, key) = (sha.to_owned(), path.to_owned());
        self.history_call(
            cx,
            move |client, run| async move {
                client
                    .workspace_diff_scoped(run, Some(file), DiffScope::Commit { sha: at })
                    .await
            },
            move |this, result, cx| {
                if let Some(tab) = this.commits.get_mut(&named) {
                    tab.patches.insert(
                        key,
                        match result {
                            Ok(d) => Loaded::Ready(Patch {
                                text: d.patch,
                                truncated: d.patch_truncated,
                                checksum: d.checksum,
                            }),
                            Err(why) => Loaded::Failed(why),
                        },
                    );
                }
                this.sync_history(cx);
                cx.notify();
            },
        );
    }

    /// Folds or opens a file of a Commit tab.
    pub(super) fn toggle_commit_file(&mut self, sha: &str, path: String, cx: &mut Context<Self>) {
        if let Some(tab) = self.commits.get_mut(sha)
            && !tab.folded.remove(&path)
        {
            tab.folded.insert(path);
        }
        self.sync_history(cx);
        cx.notify();
    }

    /// Reads the commit again (the "Try again" of a refusal or a failed read).
    fn commit_retry(&mut self, sha: &str, cx: &mut Context<Self>) {
        self.commits.remove(sha);
        self.sync_history(cx);
        cx.notify();
    }

    /// Draws the unified or split rows again after the choice changed.
    pub(super) fn resync_commits(&mut self, cx: &mut Context<Self>) {
        let shas: Vec<String> = self.commits.keys().cloned().collect();
        for sha in shas {
            self.sync_commit(&sha, cx);
        }
    }

    // --- test seams ---------------------------------------------------------------------------

    #[cfg(test)]
    pub(crate) fn history_model(&self) -> Option<&HistoryModel> {
        self.history.as_ref().map(|t| &t.model)
    }

    #[cfg(test)]
    pub(crate) fn history_search_field(&self) -> Option<&Entity<Composer>> {
        self.history.as_ref().map(|t| &t.search)
    }

    #[cfg(test)]
    pub(crate) fn commit_diff(&self, sha: &str) -> Option<&Loaded<WorkspaceDiffResult>> {
        self.commits.get(sha).map(|t| &t.diff)
    }

    #[cfg(test)]
    pub(crate) fn commit_pane(&self, sha: &str, path: &str) -> Option<&PatchPane> {
        self.commits
            .get(sha)
            .and_then(|t| t.panes.get(path))
            .map(|(_, p)| p)
    }

    // --- drawing the History tab ---------------------------------------------------------------

    /// The page of the History tab.
    pub(super) fn history_body(
        &mut self,
        theme: &Theme,
        width: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (supported, connected) = {
            let store = self.store.read(cx);
            (store.supports_history(), store.history_client().is_some())
        };
        let page = |tag: &'static str, inner: AnyElement| -> AnyElement {
            div()
                .id("history-tab")
                .debug_selector(|| "history-tab".to_owned())
                .size_full()
                .flex()
                .flex_col()
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .debug_selector(move || format!("view-history-{tag}"))
                        .child(inner),
                )
                .into_any_element()
        };
        let note = |text: String, tone: Hsla| -> Div {
            div()
                .px(px(28.0))
                .pt(px(36.0))
                .text_size(px(13.5))
                .text_color(tone)
                .child(SharedString::from(text))
        };
        if !supported {
            return page(
                "unsupported",
                note(
                    "This daemon is too old to show the commit history. Update renod and look again."
                        .to_owned(),
                    theme.muted,
                )
                .into_any_element(),
            );
        }
        if !connected {
            return page(
                "offline",
                note("The daemon is not connected.".to_owned(), theme.faint).into_any_element(),
            );
        }
        let Some(tab) = self.history.as_ref() else {
            return page(
                "loading",
                note("Reading the history\u{2026}".to_owned(), theme.faint).into_any_element(),
            );
        };
        let model = &tab.model;
        let search_bar = div()
            .id("history-search")
            .debug_selector(|| "history-search".to_owned())
            .flex_none()
            .px(px(12.0))
            .py(px(8.0))
            .border_b_1()
            .border_color(theme.hairline)
            .child(tab.search.clone());
        let (tag, body): (&'static str, AnyElement) = match &model.status {
            Status::Idle | Status::Loading => (
                "loading",
                note("Reading the history\u{2026}".to_owned(), theme.faint).into_any_element(),
            ),
            Status::Failed(why) => (
                "failed",
                note(format!("Could not read the history: {why}"), theme.danger)
                    .child(
                        div().pt(px(12.0)).child(
                            kit::pill_button(theme, "history-retry", "Try again", Tone::Quiet)
                                .on_click(cx.listener(|this, _, _, cx| this.history_reload(cx))),
                        ),
                    )
                    .into_any_element(),
            ),
            Status::Ready if !model.repo => (
                "norepo",
                note(
                    "This folder is not a git repository, so there is no history to show."
                        .to_owned(),
                    theme.muted,
                )
                .into_any_element(),
            ),
            Status::Ready => match model.search_state() {
                Some(SearchState::Failed(why)) => (
                    "search-failed",
                    note(format!("The search failed: {why}"), theme.danger).into_any_element(),
                ),
                Some(SearchState::Ready { .. }) if model.rows().is_empty() => (
                    "nomatch",
                    note(
                        format!(
                            "No commit matches \u{201c}{}\u{201d}.",
                            model.query().trim()
                        ),
                        theme.muted,
                    )
                    .into_any_element(),
                ),
                _ if model.rows().is_empty() => (
                    "empty",
                    note("This branch has no commits yet.".to_owned(), theme.muted)
                        .into_any_element(),
                ),
                _ => ("log", self.history_list(theme, width, cx)),
            },
        };
        div()
            .id("history-tab")
            .debug_selector(|| "history-tab".to_owned())
            .size_full()
            .flex()
            .flex_col()
            .child(search_bar)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .debug_selector(move || format!("view-history-{tag}"))
                    .child(body),
            )
            .into_any_element()
    }

    fn history_list(&self, theme: &Theme, width: f32, cx: &mut Context<Self>) -> AnyElement {
        let Some(tab) = self.history.as_ref() else {
            return div().into_any_element();
        };
        let _ = theme;
        let state = tab.list.clone();
        list(
            state,
            cx.processor(move |this, ix, window, cx| this.history_row(ix, width, window, cx)),
        )
        .size_full()
        .into_any_element()
    }

    /// Row `ix` of the log, or the footer after the last one.
    fn history_row(
        &self,
        ix: usize,
        width: f32,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = crate::ui::theme::current(cx);
        let Some(tab) = self.history.as_ref() else {
            return div().into_any_element();
        };
        let model = &tab.model;
        let Some(commit) = model.rows().get(ix) else {
            return self.history_footer(&theme, cx);
        };
        let Some(graph_row) = model.graph().rows.get(ix).cloned() else {
            return div().into_any_element();
        };
        let narrow = width < NARROW_BELOW;
        let (graph_w, spacing) = geometry(model.graph().max_lane_count, width);
        let short = graph::short(&commit.sha).to_owned();
        let selected = model.selected() == Some(commit.sha.as_str());
        let openable = model.openable(&commit.sha);
        let hover = theme.text.opacity(0.05);
        let sha = commit.sha.clone();
        let badges = graph::badges(commit, model.hidden());
        let now_ms = self.store.read(cx).now_ms();
        let theme_for_graph = theme;
        let graph_cell = div()
            .flex_none()
            .w(px(graph_w))
            .h(px(ROW))
            .debug_selector({
                let id = format!("history-graph-{short}");
                move || id.clone()
            })
            .child(
                canvas(
                    |_, _, _| (),
                    move |bounds, (), window, _| {
                        paint_graph(bounds, &graph_row, spacing, &theme_for_graph, window);
                    },
                )
                .size_full(),
            );
        let selector = format!("history-row-{short}");
        div()
            .id(SharedString::from(selector.clone()))
            .debug_selector(move || selector.clone())
            .h(px(ROW))
            .w_full()
            .flex()
            .items_center()
            .gap(px(8.0))
            .pr(px(12.0))
            .when(selected, |d| d.bg(theme.text.opacity(0.07)))
            .when(openable == Openable::BeforeBase, |d| {
                d.opacity(0.6).tooltip(kit::tip(
                    "Before this thread's base: the daemon shows no diff for it.",
                ))
            })
            .when(openable != Openable::BeforeBase, |d| {
                d.cursor_pointer().hover(move |s| s.bg(hover))
            })
            .on_click(cx.listener(move |this, _, window, cx| {
                this.history_click(&sha, window, cx);
            }))
            .child(graph_cell)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(13.0))
                    .child(SharedString::from(commit.subject.clone())),
            )
            .when(!narrow, |d| {
                d.children(self.badges_view(&theme, &badges, cx))
                    .child(author_cell(&theme, commit))
                    .child(
                        div()
                            .flex_none()
                            .w(px(34.0))
                            .text_right()
                            .text_size(px(12.0))
                            .text_color(theme.faint)
                            .child(summary::age_label(now_ms.saturating_sub(commit.time_ms))),
                    )
                    .child(
                        div()
                            .flex_none()
                            .w(px(54.0))
                            .font_family(MONO)
                            .text_size(px(11.5))
                            .text_color(theme.muted)
                            .child(SharedString::from(short.clone())),
                    )
            })
            .into_any_element()
    }

    /// The ref badges of a row: up to three, then "+n".
    fn badges_view(
        &self,
        theme: &Theme,
        badges: &[Badge],
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        const SHOWN: usize = 3;
        let mut out: Vec<AnyElement> = badges
            .iter()
            .take(SHOWN)
            .map(|badge| {
                let color = badge_color(theme, badge.kind);
                let label = if badge.hidden > 0 {
                    format!("{} +{}", badge.label, badge.hidden)
                } else {
                    badge.label.clone()
                };
                let key = badge.key.clone();
                div()
                    .id(SharedString::from(format!("history-badge-{}", badge.label)))
                    .debug_selector({
                        let id = format!("history-badge-{}", badge.label);
                        move || id.clone()
                    })
                    .flex_none()
                    .max_w(px(112.0))
                    .truncate()
                    .h(px(18.0))
                    .px(px(6.0))
                    .flex()
                    .items_center()
                    .rounded(px(5.0))
                    .bg(color.opacity(0.14))
                    .text_color(color)
                    .text_size(px(11.0))
                    .font_family(MONO)
                    .when_some(key, |d, key| {
                        d.cursor_pointer()
                            .tooltip(kit::tip("Collapse or expand this branch's run of commits"))
                            .on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.toggle_badge(&key, cx);
                            }))
                    })
                    .child(SharedString::from(label))
                    .into_any_element()
            })
            .collect();
        if badges.len() > SHOWN {
            out.push(
                div()
                    .flex_none()
                    .text_size(px(11.0))
                    .text_color(theme.faint)
                    .child(format!("+{}", badges.len() - SHOWN))
                    .into_any_element(),
            );
        }
        out
    }

    /// What follows the last row: more to load, loading, the end, or why it stopped.
    fn history_footer(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let Some(tab) = self.history.as_ref() else {
            return div().into_any_element();
        };
        let model = &tab.model;
        let line = |text: String, tone: Hsla| {
            div()
                .h(px(ROW))
                .px(px(14.0))
                .flex()
                .items_center()
                .text_size(px(12.0))
                .text_color(tone)
                .child(SharedString::from(text))
        };
        if let Some(why) = &model.more_error {
            return line(format!("Could not read more: {why}"), theme.danger)
                .child(
                    kit::line_button(theme, "history-more", None, "Try again", Tone::Quiet, true)
                        .on_click(cx.listener(|this, _, _, cx| this.history_more(cx))),
                )
                .gap(px(10.0))
                .into_any_element();
        }
        if model.loading_more {
            return line("Reading more\u{2026}".to_owned(), theme.faint).into_any_element();
        }
        if model.has_more() {
            return div()
                .h(px(ROW))
                .px(px(14.0))
                .flex()
                .items_center()
                .child(
                    kit::line_button(
                        theme,
                        "history-more",
                        None,
                        format!("Load {} more", graph::PAGE),
                        Tone::Quiet,
                        true,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.history_more(cx))),
                )
                .into_any_element();
        }
        let text = match model.search_state() {
            Some(SearchState::Ready { truncated: true }) => {
                "More commits match: refine the search."
            }
            Some(_) => "",
            None if model.shallow => "Shallow clone: the history stops short of its first commit.",
            None => "That is every commit.",
        };
        line(text.to_owned(), theme.faint)
            .debug_selector(|| "history-end".to_owned())
            .into_any_element()
    }

    // --- drawing the Commit tab ----------------------------------------------------------------

    /// The page of a Commit tab: the commit's line, then its files, from the same rows Review
    /// draws.
    pub(super) fn commit_body(
        &mut self,
        theme: &Theme,
        sha: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let supported = self.store.read(cx).supports_history();
        let commit: Option<GitCommit> = self
            .history
            .as_ref()
            .and_then(|t| t.model.commit(sha).cloned());
        let now_ms = self.store.read(cx).now_ms();
        let note = |text: String, tone: Hsla| {
            div()
                .px(px(28.0))
                .pt(px(30.0))
                .text_size(px(13.5))
                .text_color(tone)
                .child(SharedString::from(text))
        };
        let (tag, body): (&'static str, AnyElement) = if !supported {
            (
                "unsupported",
                note(
                    "This daemon is too old to show a commit's diff. Update renod and look again."
                        .to_owned(),
                    theme.muted,
                )
                .into_any_element(),
            )
        } else {
            match self.commits.get(sha).map(|t| &t.diff) {
                None | Some(Loaded::Loading) => (
                    "loading",
                    note("Reading the commit\u{2026}".to_owned(), theme.faint).into_any_element(),
                ),
                Some(Loaded::Failed(why)) => {
                    let at = sha.to_owned();
                    (
                        "failed",
                        note(format!("Could not show this commit: {why}"), theme.danger)
                            .child(
                                div().pt(px(12.0)).child(
                                    kit::pill_button(
                                        theme,
                                        "commit-retry",
                                        "Try again",
                                        Tone::Quiet,
                                    )
                                    .on_click(cx.listener(
                                        move |this, _, _, cx| {
                                            this.commit_retry(&at, cx);
                                        },
                                    )),
                                ),
                            )
                            .into_any_element(),
                    )
                }
                Some(Loaded::Ready(diff)) if diff.files.is_empty() => (
                    "empty",
                    note("This commit changes no files.".to_owned(), theme.muted)
                        .into_any_element(),
                ),
                Some(Loaded::Ready(_)) => {
                    let state = self.commits.get(sha).map(|t| t.flat.list.clone());
                    let at = sha.to_owned();
                    (
                        "files",
                        match state {
                            Some(state) => div()
                                .size_full()
                                .flex()
                                .flex_col()
                                .child(self.controls_bar(theme, false, true, cx))
                                .child(
                                    div().flex_1().min_h_0().child(
                                        list(
                                            state,
                                            cx.processor(move |this, ix, window, cx| {
                                                this.flat_row(
                                                    &Scope::Commit(at.clone()),
                                                    ix,
                                                    window,
                                                    cx,
                                                )
                                            }),
                                        )
                                        .size_full(),
                                    ),
                                )
                                .into_any_element(),
                            None => div().into_any_element(),
                        },
                    )
                }
            }
        };
        let copy_sha = sha.to_owned();
        let head = div()
            .id("commit-head")
            .debug_selector(|| "commit-head".to_owned())
            .flex_none()
            .px(px(16.0))
            .py(px(10.0))
            .flex()
            .flex_col()
            .gap(px(4.0))
            .border_b_1()
            .border_color(theme.hairline)
            .child(
                div()
                    .text_size(px(14.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(SharedString::from(commit.as_ref().map_or_else(
                        || format!("Commit {}", graph::short(sha)),
                        |c| c.subject.clone(),
                    ))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(px(12.0))
                    .text_color(theme.muted)
                    .child(
                        div()
                            .font_family(MONO)
                            .child(SharedString::from(graph::short(sha).to_owned())),
                    )
                    .children(commit.as_ref().map(|c| {
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(author_cell(theme, c))
                            .child(SharedString::from(summary::ago_label(
                                now_ms.saturating_sub(c.time_ms),
                            )))
                    }))
                    .child(div().flex_1())
                    .child(
                        kit::line_button(
                            theme,
                            "commit-copy-sha",
                            Some(Icon::Clip),
                            "Copy sha",
                            Tone::Quiet,
                            true,
                        )
                        .on_click(move |_, _, cx| copy(cx, copy_sha.clone(), "Commit name copied")),
                    ),
            );
        let selector = format!("commit-tab-{sha}");
        div()
            .id(SharedString::from(selector.clone()))
            .debug_selector(move || selector.clone())
            .size_full()
            .flex()
            .flex_col()
            .child(head)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .debug_selector(move || format!("view-commit-{tag}"))
                    .child(body),
            )
            .into_any_element()
    }
}

use gpui::Div;

/// A colour dot for the author, then the name. No network avatars.
fn author_cell(theme: &Theme, commit: &GitCommit) -> Div {
    div()
        .flex_none()
        .max_w(px(120.0))
        .flex()
        .items_center()
        .gap(px(5.0))
        .text_size(px(12.0))
        .text_color(theme.muted)
        .child(
            div()
                .flex_none()
                .size(px(14.0))
                .rounded(px(7.0))
                .flex()
                .items_center()
                .justify_center()
                .bg(author_color(theme, &commit.author_key))
                .text_size(px(8.5))
                .font_weight(FontWeight::BOLD)
                .text_color(theme.canvas)
                .child(SharedString::from(graph::initial(&commit.author_name))),
        )
        .child(
            div()
                .min_w_0()
                .truncate()
                .child(SharedString::from(commit.author_name.clone())),
        )
}
