//! What the run changed: the Review tab of the side panel (the finding line, the branch bar and
//! one collapsible section per changed file) and the result line under the Done row.
//!
//! Everything here draws what `vm::changes` decided; the daemon's answers are in the store.

use gpui::{
    AnyElement, App, Context, FontWeight, Hsla, ListAlignment, ListState, SharedString, Stateful,
    div, list, prelude::*, px,
};

use super::RunScreen;
use super::output::copy;
use crate::panel::surface::Surface;
use crate::panel::view::{Prefs, finding_text};
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO, Tone};
use crate::ui::theme::Theme;
use crate::vm::changes::{
    self as facts, DiffRow, FileRow, KindTone, Overview, Pane, PatchPane, RowKind,
};
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
    list: ListState,
    files: Vec<FileRow>,
    rows: Vec<ReviewRow>,
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
    fn sync(&mut self, files: Vec<FileRow>, rows: Vec<ReviewRow>) {
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
        }
        self.rows = rows;
    }

    /// The file whose bar the top of the list is under, when that bar has scrolled out of view.
    fn sticky(&self) -> Option<&FileRow> {
        let top = self.list.logical_scroll_top().item_ix;
        match self.rows.get(top)? {
            ReviewRow::Header { .. } => None,
            row => self.files.get(row.file()),
        }
    }
}

/// Which list a row belongs to.
#[derive(Clone)]
enum Scope {
    Review,
    File(String),
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
        let store = self.store.read(cx);
        let connected = store.connection() == reno_client::ConnectionStatus::Connected;
        match store.changes(&self.run) {
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
            self.store
                .update(cx, |store, _| store.forget_failed_patch(&run, &path));
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
        let review = self.review_is_showing(cx);
        let file_tab = self.file_tab_showing(cx);
        if !review && file_tab.is_none() {
            return;
        }
        let Pane::Files(overview) = self.pane(cx) else {
            return;
        };
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
        for row in overview.rows.iter().filter(|r| {
            (review && !self.review_folded.contains(&r.path))
                || file_tab.as_deref() == Some(r.path.as_str())
        }) {
            if !row.binary {
                self.store
                    .update(cx, |store, cx| store.load_patch(&run, &row.path, cx));
            }
            // The rows of the patch, parsed once per answer.
            let loaded = self
                .store
                .read(cx)
                .patch(&run, &row.path)
                .cloned()
                .unwrap_or(facts::Loaded::Loading);
            if self.patch_panes.get(&row.path).map(|(l, _)| l) != Some(&loaded) {
                let pane = facts::patch_pane(row, &loaded);
                self.patch_panes.insert(row.path.clone(), (loaded, pane));
            }
        }
        let pane_of = |path: &str| self.patch_panes.get(path).map(|(_, pane)| pane);
        let review_flat = review
            .then(|| review_rows(&overview.rows, |p| self.review_folded.contains(p), pane_of));
        let file_flat = file_tab.as_ref().and_then(|path| {
            let row = overview.rows.iter().find(|r| &r.path == path)?;
            let files = vec![row.clone()];
            let rows = review_rows(&files, |_| false, pane_of);
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
            .patch(&self.run, path)
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

    #[cfg(test)]
    pub(crate) fn review_scroll_to_end(&self) {
        self.review.list.scroll_to_end();
    }

    #[cfg(test)]
    pub(crate) fn patch_pane_of(&self, path: &str) -> Option<&PatchPane> {
        self.patch_panes.get(path).map(|(_, pane)| pane)
    }

    /// Reads the changes again (the "Try again" of a failed read).
    pub(super) fn reload_changes(&mut self, cx: &mut Context<Self>) {
        let run = self.run.clone();
        self.store
            .update(cx, |store, cx| store.load_changes(&run, cx));
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
        let pane = self.pane(cx);
        let finding = self.home(cx).map(|home| {
            div()
                .id("review-finding")
                .debug_selector(|| "review-finding".to_owned())
                .flex_none()
                .px(px(16.0))
                .py(px(12.0))
                .border_b_1()
                .border_color(theme.hairline)
                .text_size(px(13.0))
                .line_height(px(19.5))
                .text_color(theme.muted)
                .child(finding_text(theme, &home.now, theme.muted))
        });
        let body: AnyElement = match &pane {
            Pane::Files(o) => self.files_view(theme, o, cx),
            other => self.pane_note(theme, other, cx),
        };
        div()
            .id("review")
            .debug_selector(|| "review".to_owned())
            .size_full()
            .flex()
            .flex_col()
            .children(finding)
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
            Pane::Hidden if self.store.read(cx).changes(&self.run).is_none() => {
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
            Pane::Empty { .. } => {
                note("This run made no changes.".to_owned(), theme.muted).into_any_element()
            }
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
                        cx.processor(move |this, ix, _, cx| {
                            this.flat_row(&Scope::File(path.clone()), ix, cx)
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
                .child(self.file_header(theme, &file, true, true, "sticky-", cx))
        });
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(bar)
            .child(
                div()
                    .id("review-files")
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        list(
                            self.review.list.clone(),
                            cx.processor(|this, ix, _, cx| this.flat_row(&Scope::Review, ix, cx)),
                        )
                        .size_full(),
                    )
                    .children(sticky),
            )
            .into_any_element()
    }

    /// Row `ix` of a list, drawn from the rows and the parsed patches as they are now.
    fn flat_row(&self, scope: &Scope, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        #[cfg(test)]
        ROWS_BUILT.with(|n| n.set(n.get() + 1));
        let theme = crate::ui::theme::current(cx);
        let flat = match scope {
            Scope::Review => &self.review,
            Scope::File(path) => match self.file_views.get(path) {
                Some(flat) => flat,
                None => return div().into_any_element(),
            },
        };
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
                let (foldable, open) = match scope {
                    Scope::Review => (true, !self.review_folded.contains(&file.path)),
                    Scope::File(_) => (false, true),
                };
                let top = if matches!(scope, Scope::Review) {
                    12.0
                } else {
                    0.0
                };
                div()
                    .px(px(12.0))
                    .pt(px(top))
                    .child(self.file_header(&theme, &file, foldable, open, "", cx))
                    .into_any_element()
            }
            ReviewRow::Note { note: kind, .. } => {
                let (text, tone) = match kind {
                    Note::Loading => ("Reading the patch\u{2026}".to_owned(), theme.faint),
                    Note::Failed => {
                        let why = match self.patch_panes.get(&file.path) {
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
                let Some((_, PatchPane::Rows { rows, .. })) = self.patch_panes.get(&file.path)
                else {
                    return div().into_any_element();
                };
                let Some(row) = rows.get(line) else {
                    return div().into_any_element();
                };
                tag(
                    div().px(px(12.0)).child(diff_line(&theme, row, digits)),
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

    /// A file's bar: status letter, path, counts and ↗, which opens the file in its own tab.
    /// Review's bars fold; the File tab's own bar does not. `prefix` tells the sticky copy's
    /// selectors from the real bar's.
    fn file_header(
        &self,
        theme: &Theme,
        row: &FileRow,
        foldable: bool,
        open: bool,
        prefix: &str,
        cx: &mut Context<Self>,
    ) -> Stateful<gpui::Div> {
        let path = row.path.clone();
        let selector = format!("{prefix}change-file-{}", row.path);
        let stats = facts::stat_labels(row);
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
            .when(foldable, |d| {
                let path = path.clone();
                d.cursor_pointer()
                    .on_click(cx.listener(move |this, _, _, cx| this.toggle_file(path.clone(), cx)))
            })
            .when(foldable, |d| {
                d.child(icon(
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
            .when(foldable, |d| {
                let selector = format!("{prefix}change-open-{}", row.path);
                let hover = theme.text.opacity(0.06);
                let path = path.clone();
                d.child(
                    div()
                        .id(SharedString::from(selector.clone()))
                        .debug_selector(move || selector.clone())
                        .flex_none()
                        .size(px(24.0))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(6.0))
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            // The bar's own click would fold the file.
                            cx.stop_propagation();
                            this.open_surface(Surface::File(path.clone()), window, cx);
                        }))
                        .child(icon(Icon::Link, 13.0, theme.muted)),
                )
            })
    }
}

/// One line of a patch: the two line numbers, the sign and the text, tinted for what it is.
fn diff_line(theme: &Theme, row: &DiffRow, digits: usize) -> AnyElement {
    let gutter = px(digits as f32 * 7.4 + 14.0);
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
    let (wash, ink, sign) = match row.kind {
        RowKind::Add => (Some(theme.success.opacity(0.12)), theme.text, "+"),
        RowKind::Del => (Some(theme.danger.opacity(0.12)), theme.text, "\u{2212}"),
        RowKind::Context => (None, theme.text, " "),
        RowKind::Hunk => (None, theme.muted, ""),
        RowKind::NoNewline | RowKind::Binary => (None, theme.faint, ""),
    };
    let text = row.text.replace('\t', "    ");
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
                .text_color(match row.kind {
                    RowKind::Add => theme.success,
                    RowKind::Del => theme.danger,
                    _ => theme.faint,
                })
                .child(sign),
        )
        .child(
            div()
                .flex_none()
                .whitespace_nowrap()
                .pr(px(18.0))
                .text_color(ink)
                .when(row.kind == RowKind::NoNewline, |d| d.italic())
                .child(SharedString::from(text)),
        )
        .into_any_element()
}
