//! What the run changed: the Review tab of the side panel (the finding line, the branch bar and
//! one collapsible section per changed file) and the result line under the Done row.
//!
//! Everything here draws what `vm::changes` decided; the daemon's answers are in the store.

use std::collections::{HashMap, HashSet};

use gpui::{
    AnyElement, App, Context, FontWeight, Hsla, ListAlignment, ListState, SharedString, Stateful,
    StyledText, Window, div, list, prelude::*, px,
};
use reno_syntax::HighlightSpan;

use super::RunScreen;
use super::output::copy;
use crate::panel::surface::Surface;
use crate::panel::view::Prefs;
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO, Tone};
use crate::ui::markdown_view;
use crate::ui::theme::Theme;
use crate::vm::changes::{
    self as facts, DiffRow, FileRow, KindTone, Loaded, Overview, Pane, PatchPane, ReviewScope,
    RowKind,
};
use crate::vm::diff_syntax::FileSyntax;
use crate::vm::review::{Note, ReviewRow, review_rows};

/// A name for each state of the pane, the selector tests find it by.
fn tag(pane: &Pane) -> &'static str {
    match pane {
        Pane::Hidden => "changes-hidden",
        Pane::Unsupported => "changes-unsupported",
        Pane::Loading => "changes-loading",
        Pane::Failed(_) => "changes-failed",
        Pane::Removed { .. } => "changes-removed",
        Pane::Empty { .. } => "changes-empty",
        Pane::Files(_) => "changes-files",
    }
}

/// Rows the list keeps measured beyond the viewport.
const OVERDRAW: f32 = 400.0;
/// Height of one line of a patch.
const LINE: f32 = 18.0;
/// Above this many files, Review opens with all but the first few folded.
const OPEN_ALL_UP_TO: usize = 8;
const OPEN_FIRST: usize = 3;

/// A virtualised list of Review rows with the files they index. Review has one; each File tab has
/// one for its single file.
pub(super) struct Flat {
    pub(super) list: ListState,
    pub(super) files: Vec<FileRow>,
    pub(super) rows: Vec<ReviewRow>,
}

impl Flat {
    pub(super) fn new() -> Self {
        Self {
            list: ListState::new(0, ListAlignment::Top, px(OVERDRAW)),
            files: Vec::new(),
            rows: Vec::new(),
        }
    }

    /// Takes the new rows and tells the list from which one on they differ, so scrolling and the
    /// rows measured above that point stay. Rows are indices: what they draw is read live.
    pub(super) fn sync(&mut self, files: Vec<FileRow>, rows: Vec<ReviewRow>) {
        self.files = files;
        if self.rows == rows {
            return;
        }
        if self.rows.is_empty() {
            self.list.reset_with_uniform_height(rows.len(), px(LINE));
        } else {
            let same = self
                .rows
                .iter()
                .zip(&rows)
                .take_while(|(a, b)| a == b)
                .count();
            self.list
                .splice(same..self.rows.len(), rows.len().saturating_sub(same));
            // New rows have no height yet; a line is the best guess, so scrolling and the
            // scrollbar are not thrown off by rows that were never measured.
            self.list.clone().with_uniform_item_height(px(LINE));
        }
        self.rows = rows;
    }

    /// The file whose bar the top of the list is under, when that bar has scrolled out of view.
    pub(super) fn sticky(&self) -> Option<&FileRow> {
        let top = self.list.logical_scroll_top().item_ix;
        match self.rows.get(top)? {
            ReviewRow::Header { .. } => None,
            row => self.files.get(row.file()),
        }
    }
}

/// Which list a row belongs to.
#[derive(Clone)]
pub(super) enum Scope {
    Review,
    File(String),
    /// A Commit tab's list, by commit.
    Commit(String),
}

#[cfg(test)]
thread_local! {
    /// Rows the lists built (drew) so far, for the test that a big patch is not all drawn.
    pub(super) static ROWS_BUILT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn tone_color(theme: &Theme, tone: KindTone) -> Hsla {
    match tone {
        KindTone::Success => theme.success,
        KindTone::Warning => theme.warning,
        KindTone::Danger => theme.danger,
        KindTone::Accent => theme.accent,
    }
}

impl RunScreen {
    /// What the daemon has said about the run's changes, in the words of the screen.
    pub(super) fn pane(&self, cx: &App) -> Pane {
        self.pane_in(ReviewScope::Thread, cx)
    }

    /// The scope Review compares: the thread's when the daemon cannot answer the chosen one.
    pub(super) fn effective_scope(&self, cx: &App) -> ReviewScope {
        if self.store.read(cx).supports_scope(self.scope) {
            self.scope
        } else {
            ReviewScope::Thread
        }
    }

    /// The syntax cache's name for what Review compares now.
    pub(super) fn review_origin(&self, cx: &App) -> String {
        format!("review:{:?}", self.effective_scope(cx))
    }

    /// The pane for what Review compares now.
    pub(super) fn review_pane(&self, cx: &App) -> Pane {
        self.pane_in(self.effective_scope(cx), cx)
    }

    fn pane_in(&self, scope: ReviewScope, cx: &App) -> Pane {
        let store = self.store.read(cx);
        let connected = store.connection() == reno_client::ConnectionStatus::Connected;
        match store.changes_in(&self.run, scope) {
            Some(read) => facts::pane(store.supports_changes(), &read.workspace, &read.diff),
            // An older daemon says so; before it is connected, or asked, nothing is said.
            None if connected && !store.supports_changes() => Pane::Unsupported,
            None => Pane::Hidden,
        }
    }

    /// Whether Review is what the open panel shows now, so its patches are wanted.
    fn review_is_showing(&self, cx: &App) -> bool {
        *self.panel.active() == Surface::Review && Prefs::get(cx).open != Some(false)
    }

    pub(super) fn toggle_file(&mut self, path: String, cx: &mut Context<Self>) {
        if self.review_folded.remove(&path) {
            // Opened again: a patch that failed to read is asked for once more.
            let run = self.run.clone();
            let scope = self.effective_scope(cx);
            self.store
                .update(cx, |store, _| store.forget_failed_patch(&run, scope, &path));
        } else {
            self.review_folded.insert(path);
        }
        self.ensure_patches(cx);
        cx.notify();
    }

    /// The path of the File tab in front, while the panel shows.
    fn file_tab_showing(&self, cx: &App) -> Option<String> {
        match self.panel.active() {
            Surface::File(path) if Prefs::get(cx).open != Some(false) => Some(path.clone()),
            _ => None,
        }
    }

    /// Asks for the patch of every file Review shows open, and of the File tab in front, and
    /// parses each answer once. Then it tells the lists their new rows. Called when the store
    /// changes and when the panel or a file section does.
    pub(super) fn ensure_patches(&mut self, cx: &mut Context<Self>) {
        // The Files tab starts or stops its watch with the same events.
        self.sync_files(cx);
        self.sync_terminals(cx);
        // The History and Commit tabs read what they show, once they show.
        self.sync_history(cx);
        let review = self.review_is_showing(cx);
        let file_tab = self.file_tab_showing(cx);
        if !review && file_tab.is_none() {
            return;
        }
        let Pane::Files(overview) = self.review_pane(cx) else {
            return;
        };
        let scope = self.effective_scope(cx);
        let origin = self.review_origin(cx);
        let split = Prefs::get(cx).split;
        if review && !self.review_default_applied {
            self.review_default_applied = true;
            if overview.rows.len() > OPEN_ALL_UP_TO {
                self.review_folded.extend(
                    overview
                        .rows
                        .iter()
                        .skip(OPEN_FIRST)
                        .map(|r| r.path.clone()),
                );
            }
        }
        self.patch_panes
            .retain(|path, _| overview.rows.iter().any(|r| &r.path == path));
        let run = self.run.clone();
        let wanted: Vec<&FileRow> = overview
            .rows
            .iter()
            .filter(|r| {
                (review && !self.review_folded.contains(&r.path))
                    || file_tab.as_deref() == Some(r.path.as_str())
            })
            .collect();
        let mut coloured = Vec::new();
        for row in wanted {
            if !row.binary {
                self.store.update(cx, |store, cx| {
                    store.load_patch_in(&run, scope, &row.path, cx)
                });
            }
            // The rows of the patch, parsed once per answer.
            let loaded = self
                .store
                .read(cx)
                .patch_in(&run, scope, &row.path)
                .cloned()
                .unwrap_or(facts::Loaded::Loading);
            if self.patch_panes.get(&row.path).map(|(l, _)| l) != Some(&loaded) {
                let pane = facts::patch_pane(row, &loaded);
                self.patch_panes
                    .insert(row.path.clone(), (loaded.clone(), pane));
            }
            // The colours of the file, once its patch is in.
            if let facts::Loaded::Ready(patch) = &loaded
                && !row.binary
            {
                coloured.push((row.path.clone(), patch.checksum.clone()));
            }
        }
        for (path, checksum) in coloured {
            self.ensure_syntax(&origin, scope.diff_scope(), &path, &checksum, cx);
        }
        let pane_of = |path: &str| self.patch_panes.get(path).map(|(_, pane)| pane);
        let review_flat = review.then(|| {
            review_rows(
                &overview.rows,
                |p| self.review_folded.contains(p),
                pane_of,
                split,
            )
        });
        let file_flat = file_tab.as_ref().and_then(|path| {
            let row = overview.rows.iter().find(|r| &r.path == path)?;
            let files = vec![row.clone()];
            let rows = review_rows(&files, |_| false, pane_of, split);
            Some((path.clone(), files, rows))
        });
        if let Some(rows) = review_flat {
            self.review.sync(overview.rows.clone(), rows);
        }
        if let Some((path, files, rows)) = file_flat {
            self.file_views
                .entry(path)
                .or_insert_with(Flat::new)
                .sync(files, rows);
        }
    }

    /// Puts `rows` in as the parsed patch of `path` and rebuilds the lists, as if the daemon had
    /// answered with them.
    #[cfg(test)]
    pub(crate) fn inject_patch(&mut self, path: &str, rows: Vec<DiffRow>, cx: &mut Context<Self>) {
        let loaded = self
            .store
            .read(cx)
            .patch_in(&self.run, self.effective_scope(cx), path)
            .cloned()
            .unwrap_or(facts::Loaded::Loading);
        let pane = PatchPane::Rows {
            rows,
            truncated: false,
        };
        self.patch_panes.insert(path.to_owned(), (loaded, pane));
        self.ensure_patches(cx);
    }

    /// How many rows Review's list has.
    #[cfg(test)]
    pub(crate) fn review_row_count(&self) -> usize {
        self.review.rows.len()
    }

    /// How many of Review's rows are side-by-side pairs.
    #[cfg(test)]
    pub(crate) fn review_pair_rows(&self) -> usize {
        self.review
            .rows
            .iter()
            .filter(|r| matches!(r, ReviewRow::Pair { .. }))
            .count()
    }

    #[cfg(test)]
    pub(crate) fn review_scroll_to_end(&self) {
        self.review.list.scroll_to_end();
    }

    #[cfg(test)]
    pub(crate) fn review_top(&self) -> usize {
        self.review.list.logical_scroll_top().item_ix
    }

    #[cfg(test)]
    pub(crate) fn review_scroll_by(&self, distance: gpui::Pixels) {
        self.review.list.scroll_by(distance);
    }

    /// Folds every file but `keep` (a person clicking their bars).
    #[cfg(test)]
    pub(crate) fn fold_all_but(&mut self, keep: &str, cx: &mut Context<Self>) {
        let paths: Vec<String> = self.review.files.iter().map(|f| f.path.clone()).collect();
        for path in paths.into_iter().filter(|p| p != keep) {
            if !self.review_folded.contains(&path) {
                self.toggle_file(path, cx);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn patch_pane_of(&self, path: &str) -> Option<&PatchPane> {
        self.patch_panes.get(path).map(|(_, pane)| pane)
    }

    /// Reads the changes again (the "Try again" of a failed read).
    pub(super) fn reload_changes(&mut self, cx: &mut Context<Self>) {
        let run = self.run.clone();
        let scope = self.effective_scope(cx);
        self.store.update(cx, |store, cx| {
            // The thread's changes are what the Session tab, the tab counts and the result line
            // say; Review's own scope is read as well while it is the run's.
            store.load_changes(&run, cx);
            if scope != ReviewScope::Thread {
                store.load_changes_in(&run, scope, cx);
            }
        });
    }

    /// Review compares `scope` from now on (a person's choice in the sticky bar).
    pub(crate) fn set_scope(&mut self, scope: ReviewScope, cx: &mut Context<Self>) {
        if self.scope == scope || !self.store.read(cx).supports_scope(scope) {
            return;
        }
        self.scope = scope;
        // The files and their patches are another list now.
        self.patch_panes.clear();
        self.review_folded.clear();
        self.review_default_applied = false;
        self.review.sync(Vec::new(), Vec::new());
        self.reload_changes(cx);
        self.ensure_patches(cx);
        cx.notify();
    }

    #[cfg(test)]
    pub(crate) fn scope(&self) -> ReviewScope {
        self.scope
    }

    /// "This run | This thread | Uncommitted": the segmented control of Review's bar. A scope
    /// the daemon cannot answer stays, dimmed, with its reason as the tooltip.
    fn scope_switch(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let current = self.effective_scope(cx);
        let one = |scope: ReviewScope, id: &'static str| {
            let on = scope == current;
            let usable = self.store.read(cx).supports_scope(scope);
            div()
                .id(id)
                .debug_selector(move || id.to_owned())
                .h(px(24.0))
                .px(px(9.0))
                .flex()
                .items_center()
                .rounded(px(6.0))
                .text_size(px(12.0))
                .when(usable, |d| d.cursor_pointer())
                .when(!usable, |d| d.cursor_default().opacity(0.45))
                .when(on, |d| {
                    d.bg(theme.raised)
                        .text_color(theme.text)
                        .font_weight(FontWeight::MEDIUM)
                })
                .when(!on, |d| d.text_color(theme.muted))
                .when(!usable, |d| d.tooltip(kit::tip(scope.needs_words())))
                .child(scope.label())
                .on_click(cx.listener(move |this, _, _, cx| this.set_scope(scope, cx)))
        };
        div()
            .id("review-scope")
            .debug_selector(|| "review-scope".to_owned())
            .flex_none()
            .p(px(2.0))
            .flex()
            .items_center()
            .gap(px(2.0))
            .rounded(px(8.0))
            .bg(theme.text.opacity(0.06))
            .child(one(ReviewScope::Run, "scope-run"))
            .child(one(ReviewScope::Thread, "scope-thread"))
            .child(one(ReviewScope::WorkingTree, "scope-working-tree"))
            .into_any_element()
    }

    /// "Unified | Split": how the diffs draw. Kept with the panel's layout.
    pub(super) fn mode_switch(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let split = Prefs::get(cx).split;
        let one = |on: bool, label: &'static str, id: &'static str, to: bool| {
            div()
                .id(id)
                .debug_selector(move || id.to_owned())
                .h(px(24.0))
                .px(px(9.0))
                .flex()
                .items_center()
                .rounded(px(6.0))
                .text_size(px(12.0))
                .cursor_pointer()
                .when(on, |d| {
                    d.bg(theme.raised)
                        .text_color(theme.text)
                        .font_weight(FontWeight::MEDIUM)
                })
                .when(!on, |d| d.text_color(theme.muted))
                .child(label)
                .on_click(cx.listener(move |this, _, _, cx| this.set_split(to, cx)))
        };
        div()
            .id("diff-mode")
            .debug_selector(|| "diff-mode".to_owned())
            .flex_none()
            .p(px(2.0))
            .flex()
            .items_center()
            .gap(px(2.0))
            .rounded(px(8.0))
            .bg(theme.text.opacity(0.06))
            .child(one(!split, "Unified", "mode-unified", false))
            .child(one(split, "Split", "mode-split", true))
            .into_any_element()
    }

    /// Draws diffs side by side (or not) from now on, and keeps the choice with the layout.
    pub(crate) fn set_split(&mut self, split: bool, cx: &mut Context<Self>) {
        let prefs = Prefs::get(cx);
        if prefs.split == split {
            return;
        }
        Prefs { split, ..prefs }.set(cx);
        self.resync_commits(cx);
        self.ensure_patches(cx);
        cx.notify();
    }

    /// The slim bar under the branch bar: the scope switch when Review offers it, and the
    /// unified / split choice.
    pub(super) fn controls_bar(
        &self,
        theme: &Theme,
        with_scope: bool,
        with_mode: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .id("review-controls")
            .debug_selector(|| "review-controls".to_owned())
            .flex_none()
            .h(px(36.0))
            .px(px(16.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .border_b_1()
            .border_color(theme.hairline)
            .when(with_scope, |d| d.child(self.scope_switch(theme, cx)))
            .child(div().flex_1())
            .when(with_mode, |d| d.child(self.mode_switch(theme, cx)))
            .into_any_element()
    }

    /// The bar that holds only the switch, for a scope that shows no files.
    fn scope_bar(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        self.controls_bar(theme, true, false, cx)
    }

    /// Whether Review offers the scope switch: the daemon has threads and a workspace to compare.
    fn offers_scope(&self, cx: &App) -> bool {
        let store = self.store.read(cx);
        store.supports_threads()
            && matches!(
                store.changes(&self.run).map(|c| &c.workspace),
                Some(Loaded::Ready(Some(_)))
            )
    }

    /// "Branch reno/x has 3 commits, 7 files changed (+120 −30)", under the Done row of a
    /// finished run, opening Review.
    pub(super) fn result_line(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        let line = self.result.clone()?;
        Some(
            div()
                .id("changes-result")
                .debug_selector(|| "changes-result".to_owned())
                .flex()
                .items_center()
                .gap(px(9.0))
                .pt(px(8.0))
                .text_size(px(13.5))
                .text_color(theme.muted)
                .cursor_pointer()
                .hover({
                    let text = theme.text;
                    move |s| s.text_color(text)
                })
                .child(icon(Icon::Branch, 15.0, theme.muted))
                .child(SharedString::from(line))
                .child(icon(Icon::Fwd, 13.0, theme.faint))
                .on_click(cx.listener(|this, _, window, cx| {
                    this.open_surface(Surface::Review, window, cx);
                }))
                .into_any_element(),
        )
    }

    // --- the Review tab ----------------------------------------------------------------------

    /// The Review tab: the finding line, then the states of the pane, or the branch bar and the
    /// files. The transcript beside it stays.
    pub(super) fn review_body(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let pane = self.review_pane(cx);
        let body: AnyElement = match &pane {
            Pane::Files(o) => self.files_view(theme, o, cx),
            other => {
                // Without files there is no branch bar: the switch keeps its own, so a scope
                // that shows nothing can be left.
                let note = self.pane_note(theme, other, cx);
                if self.offers_scope(cx) {
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .child(self.scope_bar(theme, cx))
                        .child(note)
                        .into_any_element()
                } else {
                    note
                }
            }
        };
        div()
            .id("review")
            .debug_selector(|| "review".to_owned())
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .debug_selector({
                        let tag = tag(&pane);
                        move || format!("view-{tag}")
                    })
                    .child(body),
            )
            .into_any_element()
    }

    /// What a pane without files says, in a column of its own.
    fn pane_note(&self, theme: &Theme, pane: &Pane, cx: &mut Context<Self>) -> AnyElement {
        let note = |text: String, tone: Hsla| {
            div()
                .flex_1()
                .px(px(28.0))
                .pt(px(36.0))
                .text_size(px(13.5))
                .text_color(tone)
                .child(SharedString::from(text))
        };
        match pane {
            // Not asked yet, which a restored tab can be: not the same as "nothing".
            Pane::Hidden
                if self
                    .store
                    .read(cx)
                    .changes_in(&self.run, self.effective_scope(cx))
                    .is_none() =>
            {
                note("Reading the changes\u{2026}".to_owned(), theme.faint).into_any_element()
            }
            Pane::Hidden => note(
                "There is nothing to show for this run.".to_owned(),
                theme.muted,
            )
            .into_any_element(),
            Pane::Unsupported => note(
                "This daemon is too old to show what a run changed. Update renod and look again."
                    .to_owned(),
                theme.muted,
            )
            .into_any_element(),
            Pane::Loading => {
                note("Reading the changes\u{2026}".to_owned(), theme.faint).into_any_element()
            }
            Pane::Failed(why) => note(format!("Could not read the changes: {why}"), theme.danger)
                .child(
                    div().pt(px(12.0)).child(
                        kit::pill_button(theme, "changes-retry", "Try again", Tone::Quiet)
                            .on_click(cx.listener(|this, _, _, cx| this.reload_changes(cx))),
                    ),
                )
                .into_any_element(),
            Pane::Removed { branch } => note(
                format!(
                    "The workspace was removed \u{2014} nothing to show. \
                     Whatever was kept is on the branch {branch}."
                ),
                theme.muted,
            )
            .into_any_element(),
            Pane::Empty { .. } => note(
                match self.effective_scope(cx) {
                    ReviewScope::WorkingTree => {
                        "Nothing is uncommitted: the worktree matches its latest commit."
                    }
                    _ => "This run made no changes.",
                }
                .to_owned(),
                theme.muted,
            )
            .into_any_element(),
            Pane::Files(_) => div().into_any_element(),
        }
    }

    /// A File tab: one changed file's bar and its diff, from the same rows Review draws.
    pub(super) fn file_body(
        &self,
        theme: &Theme,
        path: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pane = self.pane(cx);
        let inner: AnyElement = match &pane {
            Pane::Files(o) if o.rows.iter().any(|r| r.path == path) => {
                match self.file_views.get(path) {
                    Some(flat) => list(flat.list.clone(), {
                        let path = path.to_owned();
                        cx.processor(move |this, ix, window, cx| {
                            this.flat_row(&Scope::File(path.clone()), ix, window, cx)
                        })
                    })
                    .size_full()
                    .into_any_element(),
                    // Between the changes arriving and the next frame.
                    None => div().into_any_element(),
                }
            }
            Pane::Files(_) => div()
                .px(px(28.0))
                .pt(px(36.0))
                .text_size(px(13.5))
                .text_color(theme.muted)
                .child("This file is not among the run\u{2019}s changes.")
                .into_any_element(),
            other => self.pane_note(theme, other, cx),
        };
        let selector = format!("file-tab-{path}");
        div()
            .id(SharedString::from(selector.clone()))
            .debug_selector(move || selector.clone())
            .size_full()
            .flex()
            .flex_col()
            .child(inner)
            .into_any_element()
    }

    /// The bar that stays above the files, and the files.
    fn files_view(&self, theme: &Theme, o: &Overview, cx: &mut Context<Self>) -> AnyElement {
        let branch = o.branch.clone();
        let bar = div()
            .flex_none()
            .h(px(40.0))
            .pl(px(16.0))
            .pr(px(12.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .border_b_1()
            .border_color(theme.hairline)
            .text_size(px(12.5))
            .child(icon(Icon::Branch, 14.0, theme.muted))
            .child(
                div()
                    .id("review-branch")
                    .debug_selector(|| "review-branch".to_owned())
                    .min_w_0()
                    .truncate()
                    .font_family(MONO)
                    .text_size(px(12.0))
                    .child(SharedString::from(branch.clone())),
            )
            .child(
                div()
                    .flex_none()
                    .whitespace_nowrap()
                    .text_color(theme.muted)
                    .child(SharedString::from(format!(
                        "\u{b7} {} \u{b7} {}",
                        o.commits, o.stats
                    ))),
            )
            .child(div().flex_1())
            .child(
                kit::line_button(theme, "copy-branch", None, "Copy branch", Tone::Quiet, true)
                    .on_click(move |_, _, cx| {
                        copy(cx, branch.clone(), "Branch name copied");
                    }),
            );
        let sticky = self.review.sticky().cloned();
        let sticky = sticky.map(|file| {
            div()
                .absolute()
                .top_0()
                .left_0()
                .right_0()
                .px(px(12.0))
                .bg(theme.canvas)
                .child(self.file_header(
                    theme,
                    &file,
                    Header::Fold {
                        scope: Scope::Review,
                        open: true,
                        tabs: true,
                    },
                    "sticky-",
                    cx,
                ))
        });
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(bar)
            .child(self.controls_bar(theme, self.offers_scope(cx), true, cx))
            .child(
                div()
                    .id("review-files")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        list(
                            self.review.list.clone(),
                            cx.processor(|this, ix, window, cx| {
                                this.flat_row(&Scope::Review, ix, window, cx)
                            }),
                        )
                        .size_full(),
                    )
                    .children(sticky),
            )
            .into_any_element()
    }

    /// What the list of `scope` draws from.
    fn source(&self, scope: &Scope, cx: &App) -> Option<Source<'_>> {
        Some(match scope {
            Scope::Review => Source {
                flat: &self.review,
                panes: &self.patch_panes,
                folded: Some(&self.review_folded),
                origin: self.review_origin(cx),
                header: HeaderKind::Fold { tabs: true },
            },
            Scope::File(path) => Source {
                flat: self.file_views.get(path)?,
                panes: &self.patch_panes,
                folded: None,
                origin: self.review_origin(cx),
                header: HeaderKind::Fixed,
            },
            Scope::Commit(sha) => {
                let tab = self.commits.get(sha)?;
                Source {
                    flat: &tab.flat,
                    panes: &tab.panes,
                    folded: Some(&tab.folded),
                    origin: format!("commit:{sha}"),
                    header: HeaderKind::Fold { tabs: false },
                }
            }
        })
    }

    /// The colours of a file's lines, once they have arrived for the patch shown.
    pub(super) fn file_syntax(
        &self,
        origin: &str,
        path: &str,
        loaded: &facts::Loaded<facts::Patch>,
    ) -> Option<std::sync::Arc<FileSyntax>> {
        match loaded {
            facts::Loaded::Ready(patch) => {
                self.syntax.ready(origin, path, &patch.checksum).cloned()
            }
            _ => None,
        }
    }

    /// Row `ix` of a list, drawn from the rows and the parsed patches as they are now.
    pub(super) fn flat_row(
        &self,
        scope: &Scope,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        #[cfg(test)]
        ROWS_BUILT.with(|n| n.set(n.get() + 1));
        let theme = crate::ui::theme::current(cx);
        let Some(src) = self.source(scope, cx) else {
            return div().into_any_element();
        };
        let flat = src.flat;
        let Some(row) = flat.rows.get(ix).copied() else {
            return div().into_any_element();
        };
        let Some(file) = flat.files.get(row.file()).cloned() else {
            return div().into_any_element();
        };
        // The first row under a bar carries the selector the tests find a file's patch by.
        let first_body = ix > 0 && matches!(flat.rows[ix - 1], ReviewRow::Header { .. });
        let tag = move |d: gpui::Div, path: &str| -> AnyElement {
            if first_body {
                let selector = format!("change-patch-{path}");
                d.id(SharedString::from(selector.clone()))
                    .debug_selector(move || selector.clone())
                    .into_any_element()
            } else {
                d.into_any_element()
            }
        };
        let note = |text: &str, tone: Hsla| {
            div()
                .px(px(28.0))
                .py(px(10.0))
                .text_size(px(13.0))
                .text_color(tone)
                .child(SharedString::from(text.to_owned()))
        };
        match row {
            ReviewRow::Header { .. } => {
                let kind = match (&src.header, src.folded) {
                    (HeaderKind::Fold { tabs }, Some(folded)) => Header::Fold {
                        scope: scope.clone(),
                        open: !folded.contains(&file.path),
                        tabs: *tabs,
                    },
                    _ => Header::Fixed,
                };
                let top = if matches!(kind, Header::Fixed) {
                    0.0
                } else {
                    12.0
                };
                div()
                    .px(px(12.0))
                    .pt(px(top))
                    .child(self.file_header(&theme, &file, kind, "", cx))
                    .into_any_element()
            }
            ReviewRow::Note { note: kind, .. } => {
                let (text, tone) = match kind {
                    Note::Loading => ("Reading the patch\u{2026}".to_owned(), theme.faint),
                    Note::Failed => {
                        let why = match src.panes.get(&file.path) {
                            Some((_, PatchPane::Failed(why))) => why.clone(),
                            _ => String::new(),
                        };
                        (format!("Could not read the patch: {why}"), theme.danger)
                    }
                    Note::Binary => (
                        "This is a binary file: git has no lines to show for it.".to_owned(),
                        theme.muted,
                    ),
                    Note::Nothing => ("This file has no text changes.".to_owned(), theme.muted),
                };
                tag(note(&text, tone), &file.path)
            }
            ReviewRow::Line { line, digits, .. } => {
                let Some((loaded, PatchPane::Rows { rows, .. })) = src.panes.get(&file.path) else {
                    return div().into_any_element();
                };
                let Some(row) = rows.get(line) else {
                    return div().into_any_element();
                };
                let syntax = self.file_syntax(&src.origin, &file.path, loaded);
                let spans = syntax.as_ref().and_then(|s| s.spans(row));
                tag(
                    div().px(px(12.0)).child(diff_line(
                        &theme,
                        row,
                        digits,
                        spans,
                        &window.text_style(),
                    )),
                    &file.path,
                )
            }
            ReviewRow::Pair {
                left,
                right,
                digits,
                ..
            } => {
                let Some((loaded, PatchPane::Rows { rows, .. })) = src.panes.get(&file.path) else {
                    return div().into_any_element();
                };
                let syntax = self.file_syntax(&src.origin, &file.path, loaded);
                let side = |ix: Option<usize>| {
                    ix.and_then(|i| rows.get(i))
                        .map(|row| (row, syntax.as_ref().and_then(|s| s.spans(row))))
                };
                tag(
                    div().px(px(12.0)).child(diff_pair(
                        &theme,
                        side(left),
                        side(right),
                        digits,
                        &window.text_style(),
                    )),
                    &file.path,
                )
            }
            ReviewRow::Cut { shown, .. } => div()
                .debug_selector(|| "patch-cut".to_owned())
                .px(px(28.0))
                .py(px(10.0))
                .text_size(px(12.5))
                .text_color(theme.faint)
                .child(format!(
                    "The patch is longer than Reno shows: {shown} lines are here, the rest was cut."
                ))
                .into_any_element(),
        }
    }

    /// Folds or opens a file of a list, whichever tab it is in.
    fn toggle_in(&mut self, scope: &Scope, path: String, cx: &mut Context<Self>) {
        match scope {
            Scope::Review => self.toggle_file(path, cx),
            Scope::Commit(sha) => self.toggle_commit_file(&sha.clone(), path, cx),
            Scope::File(_) => {}
        }
    }

    /// A file's bar: status letter, path, counts, copy path and, in Review, the buttons that open
    /// the file in its tabs. Review's and a Commit's bars fold; the File tab's own bar does not.
    /// `prefix` tells the sticky copy's selectors from the real bar's.
    pub(super) fn file_header(
        &self,
        theme: &Theme,
        row: &FileRow,
        kind: Header,
        prefix: &str,
        cx: &mut Context<Self>,
    ) -> Stateful<gpui::Div> {
        let path = row.path.clone();
        let selector = format!("{prefix}change-file-{}", row.path);
        let stats = facts::stat_labels(row);
        let fold = match &kind {
            Header::Fold { scope, open, tabs } => Some((scope.clone(), *open, *tabs)),
            Header::Fixed => None,
        };
        let tabs = fold.as_ref().is_some_and(|(_, _, tabs)| *tabs);
        let icon_button = |id: String, glyph: Icon| {
            let hover = theme.text.opacity(0.06);
            div()
                .id(SharedString::from(id.clone()))
                .debug_selector(move || id.clone())
                .flex_none()
                .size(px(24.0))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.0))
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .child(icon(glyph, 13.0, theme.muted))
        };
        div()
            .id(SharedString::from(selector.clone()))
            .debug_selector(move || selector.clone())
            .h(px(36.0))
            .pl(px(10.0))
            .pr(px(6.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded_t(px(8.0))
            .bg(theme.text.opacity(0.05))
            .when_some(fold, |d, (scope, open, _)| {
                let path = path.clone();
                d.cursor_pointer()
                    .on_click(
                        cx.listener(move |this, _, _, cx| this.toggle_in(&scope, path.clone(), cx)),
                    )
                    .child(icon(
                        if open { Icon::ChevDown } else { Icon::Fwd },
                        13.0,
                        theme.muted,
                    ))
            })
            .child(
                div()
                    .flex_none()
                    .w(px(12.0))
                    .font_family(MONO)
                    .text_size(px(12.0))
                    .font_weight(FontWeight::BOLD)
                    .text_color(tone_color(theme, row.tone))
                    .child(row.letter.to_string()),
            )
            .child(
                div().min_w_0().truncate().text_size(px(13.0)).child(
                    div()
                        .flex()
                        .child(
                            div()
                                .text_color(theme.faint)
                                .child(SharedString::from(row.dir.clone())),
                        )
                        .child(
                            div()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(SharedString::from(row.name.clone())),
                        ),
                ),
            )
            .child(div().flex_1())
            .child(match stats {
                Some((added, removed)) => div()
                    .flex_none()
                    .flex()
                    .gap(px(6.0))
                    .font_family(MONO)
                    .text_size(px(11.5))
                    .child(div().text_color(theme.success).child(added))
                    .child(div().text_color(theme.danger).child(removed)),
                None => div()
                    .flex_none()
                    .text_size(px(11.5))
                    .text_color(theme.faint)
                    .child("binary"),
            })
            .child({
                // The path, to paste. The bar's own click would fold the file.
                let path = path.clone();
                icon_button(format!("{prefix}change-copy-{}", row.path), Icon::Clip)
                    .tooltip(kit::tip("Copy path"))
                    .on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        copy(cx, path.clone(), "Path copied");
                    })
            })
            .when(tabs, |d| {
                // The file itself, in the Files tab, beside the diff's own tab.
                let path = path.clone();
                d.child(
                    icon_button(format!("{prefix}change-files-{}", row.path), Icon::Folder)
                        .tooltip(kit::tip("Show in Files"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.open_file_at(&path, None, window, cx);
                        })),
                )
            })
            .when(tabs, |d| {
                let path = path.clone();
                d.child(
                    icon_button(format!("{prefix}change-open-{}", row.path), Icon::Link)
                        .tooltip(kit::tip("Open in its own tab"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            // The bar's own click would fold the file.
                            cx.stop_propagation();
                            this.open_surface(Surface::File(path.clone()), window, cx);
                        })),
                )
            })
    }
}

/// What a list's file bars do.
enum HeaderKind {
    /// Fold on a click; `tabs` adds the buttons that open the file in its own tabs.
    Fold { tabs: bool },
    /// The File tab's one bar.
    Fixed,
}

/// One file bar, as drawn.
pub(super) enum Header {
    Fold {
        scope: Scope,
        open: bool,
        tabs: bool,
    },
    Fixed,
}

/// What a list draws its rows from.
struct Source<'a> {
    flat: &'a Flat,
    panes: &'a HashMap<String, (facts::Loaded<facts::Patch>, PatchPane)>,
    folded: Option<&'a HashSet<String>>,
    /// The syntax cache's name for these files.
    origin: String,
    header: HeaderKind,
}

/// A diff line's wash, its text ink, its sign and the sign's ink.
fn line_look(theme: &Theme, kind: RowKind) -> (Option<Hsla>, Hsla, &'static str, Hsla) {
    match kind {
        RowKind::Add => (
            Some(theme.success.opacity(0.12)),
            theme.text,
            "+",
            theme.success,
        ),
        RowKind::Del => (
            Some(theme.danger.opacity(0.12)),
            theme.text,
            "\u{2212}",
            theme.danger,
        ),
        RowKind::Context => (None, theme.text, " ", theme.faint),
        RowKind::Hunk => (None, theme.muted, "", theme.faint),
        RowKind::NoNewline | RowKind::Binary => (None, theme.faint, "", theme.faint),
    }
}

/// The text of a line: coloured by its syntax spans when it has them, plain otherwise. A line
/// with tabs is drawn plain with the tabs spread, since the spans count bytes of the raw text.
fn line_text(
    theme: &Theme,
    row: &DiffRow,
    ink: Hsla,
    spans: Option<&[HighlightSpan]>,
    base: &gpui::TextStyle,
) -> AnyElement {
    match spans {
        Some(spans) if !row.text.is_empty() && !row.text.contains('\t') => {
            StyledText::new(SharedString::from(row.text.clone()))
                .with_runs(markdown_view::line_runs(&row.text, spans, theme, base))
                .into_any_element()
        }
        _ => div()
            .text_color(ink)
            .when(row.kind == RowKind::NoNewline, |d| d.italic())
            .child(SharedString::from(row.text.replace('\t', "    ")))
            .into_any_element(),
    }
}

fn gutter_of(digits: usize) -> gpui::Pixels {
    px(digits as f32 * 7.4 + 14.0)
}

/// One line of a patch: the two line numbers, the sign and the text, tinted for what it is.
fn diff_line(
    theme: &Theme,
    row: &DiffRow,
    digits: usize,
    spans: Option<&[HighlightSpan]>,
    base: &gpui::TextStyle,
) -> AnyElement {
    let gutter = gutter_of(digits);
    let number = |n: Option<u32>| {
        div()
            .flex_none()
            .w(gutter)
            .pr(px(8.0))
            .flex()
            .justify_end()
            .text_color(theme.faint)
            .child(n.map(|n| n.to_string()).unwrap_or_default())
    };
    let (wash, ink, sign, sign_ink) = line_look(theme, row.kind);
    div()
        .flex()
        .flex_none()
        .w_full()
        .overflow_hidden()
        .h(px(LINE))
        .items_center()
        .font_family(MONO)
        .text_size(px(12.0))
        .when_some(wash, |d, wash| d.bg(wash))
        .child(number(row.old))
        .child(number(row.new))
        .child(
            div()
                .flex_none()
                .w(px(14.0))
                .text_color(sign_ink)
                .child(sign),
        )
        .child(
            div()
                .flex_none()
                .whitespace_nowrap()
                .pr(px(18.0))
                .child(line_text(theme, row, ink, spans, base)),
        )
        .into_any_element()
}

type Side<'a> = Option<(&'a DiffRow, Option<&'a [HighlightSpan]>)>;

/// One row of the split view: the old line on the left, the new on the right. A side with no
/// line is a quiet blank.
fn diff_pair(
    theme: &Theme,
    left: Side<'_>,
    right: Side<'_>,
    digits: usize,
    base: &gpui::TextStyle,
) -> AnyElement {
    let gutter = gutter_of(digits);
    let half = |side: Side<'_>, old: bool| -> AnyElement {
        let Some((row, spans)) = side else {
            return div()
                .flex_1()
                .min_w_0()
                .h_full()
                .bg(theme.text.opacity(0.03))
                .into_any_element();
        };
        let (wash, ink, _, _) = line_look(theme, row.kind);
        let number = if old { row.old } else { row.new };
        div()
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .items_center()
            .overflow_hidden()
            .when_some(wash, |d, wash| d.bg(wash))
            .child(
                div()
                    .flex_none()
                    .w(gutter)
                    .pr(px(8.0))
                    .flex()
                    .justify_end()
                    .text_color(theme.faint)
                    .child(number.map(|n| n.to_string()).unwrap_or_default()),
            )
            .child(
                div()
                    .min_w_0()
                    .whitespace_nowrap()
                    .pr(px(8.0))
                    .child(line_text(theme, row, ink, spans, base)),
            )
            .into_any_element()
    };
    div()
        .flex()
        .flex_none()
        .w_full()
        .overflow_hidden()
        .h(px(LINE))
        .items_center()
        .font_family(MONO)
        .text_size(px(12.0))
        .child(half(left, true))
        .child(div().flex_none().w(px(1.0)).h_full().bg(theme.hairline))
        .child(half(right, false))
        .into_any_element()
}
