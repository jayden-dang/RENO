//! What the run changed: the Review tab of the side panel (the finding line, the branch bar and
//! one collapsible section per changed file) and the result line under the Done row.
//!
//! Everything here draws what `vm::changes` decided; the daemon's answers are in the store.

use gpui::{
    AnyElement, App, Context, FontWeight, Hsla, SharedString, Stateful, div, prelude::*, px,
};

use super::RunScreen;
use super::output::copy;
use crate::panel::surface::Surface;
use crate::panel::view::{Prefs, finding_text};
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO, Tone};
use crate::ui::theme::Theme;
use crate::ui::toast;
use crate::vm::changes::{
    self as facts, DiffRow, FileRow, KindTone, Overview, Pane, PatchPane, RowKind,
};

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

/// The most patch lines drawn at once; a patch is cut by the daemon at 256 KiB.
const MAX_LINES: usize = 4_000;
/// Height of one line of a patch.
const LINE: f32 = 18.0;
/// Above this many files, Review opens with all but the first few folded.
const OPEN_ALL_UP_TO: usize = 8;
const OPEN_FIRST: usize = 3;

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

    /// Asks for the patch of every file Review shows open, and parses each answer once. Called
    /// when the store changes and when the panel or a file section does.
    pub(super) fn ensure_patches(&mut self, cx: &mut Context<Self>) {
        if !self.review_is_showing(cx) {
            return;
        }
        let Pane::Files(overview) = self.pane(cx) else {
            return;
        };
        if !self.review_default_applied {
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
        for row in overview
            .rows
            .iter()
            .filter(|r| !self.review_folded.contains(&r.path))
        {
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
        let note = |text: String, tone: Hsla| {
            div()
                .flex_1()
                .px(px(28.0))
                .pt(px(36.0))
                .text_size(px(13.5))
                .text_color(tone)
                .child(SharedString::from(text))
        };
        let body: AnyElement = match &pane {
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
            Pane::Files(o) => self.files_view(theme, o, cx),
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
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .child(bar)
            .child(
                div()
                    .id("review-files")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.review_scroll)
                    .pb(px(16.0))
                    .children(
                        o.rows
                            .iter()
                            .enumerate()
                            .map(|(ix, row)| self.file_section(theme, ix, row, cx)),
                    ),
            )
            .into_any_element()
    }

    fn file_section(
        &self,
        theme: &Theme,
        ix: usize,
        row: &FileRow,
        cx: &mut Context<Self>,
    ) -> Stateful<gpui::Div> {
        let open = !self.review_folded.contains(&row.path);
        let path = row.path.clone();
        let selector = format!("change-file-{}", row.path);
        let stats = facts::stat_labels(row);
        let header = div()
            .id(SharedString::from(selector.clone()))
            .debug_selector(move || selector.clone())
            .h(px(36.0))
            .pl(px(10.0))
            .pr(px(6.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .bg(theme.text.opacity(0.05))
            .cursor_pointer()
            .on_click({
                let path = path.clone();
                cx.listener(move |this, _, _, cx| this.toggle_file(path.clone(), cx))
            })
            .child(icon(
                if open { Icon::ChevDown } else { Icon::Fwd },
                13.0,
                theme.muted,
            ))
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
                let selector = format!("change-open-{}", row.path);
                let hover = theme.text.opacity(0.06);
                div()
                    .id(SharedString::from(selector.clone()))
                    .debug_selector(move || selector.clone())
                    .flex_none()
                    .size(px(24.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.0))
                    .hover(move |s| s.bg(hover))
                    .on_click(|_, _, cx| {
                        cx.stop_propagation();
                        toast::not_yet(cx, "A file in its own tab");
                    })
                    .child(icon(Icon::Link, 13.0, theme.muted))
            });
        div()
            .id(("review-file", ix))
            .mx(px(12.0))
            .mt(px(12.0))
            .border_1()
            .border_color(theme.hairline)
            .rounded(px(10.0))
            .overflow_hidden()
            .child(header)
            .when(open, |d| d.child(self.patch_view(theme, &row.path)))
    }

    fn patch_view(&self, theme: &Theme, path: &str) -> AnyElement {
        let note = |text: &str, tone: Hsla| {
            div()
                .px(px(16.0))
                .py(px(12.0))
                .text_size(px(13.0))
                .text_color(tone)
                .child(SharedString::from(text.to_owned()))
                .into_any_element()
        };
        let selector = format!("change-patch-{path}");
        let pane = self
            .patch_panes
            .get(path)
            .map_or(&PatchPane::Loading, |(_, pane)| pane);
        let body = match pane {
            PatchPane::Loading => note("Reading the patch\u{2026}", theme.faint),
            PatchPane::Failed(why) => {
                note(&format!("Could not read the patch: {why}"), theme.danger)
            }
            PatchPane::Binary => note(
                "This is a binary file: git has no lines to show for it.",
                theme.muted,
            ),
            PatchPane::Nothing => note("This file has no text changes.", theme.muted),
            PatchPane::Rows { rows, truncated } => {
                let digits = facts::gutter_digits(rows);
                let shown = rows.len().min(MAX_LINES);
                let cut = *truncated || rows.len() > MAX_LINES;
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .id(SharedString::from(format!("diff-{path}")))
                            .overflow_x_scroll()
                            .py(px(4.0))
                            .children(
                                rows.iter()
                                    .take(MAX_LINES)
                                    .map(|row| diff_line(theme, row, digits)),
                            ),
                    )
                    .when(cut, |d| {
                        d.child(
                            div()
                                .debug_selector(|| "patch-cut".to_owned())
                                .px(px(16.0))
                                .py(px(10.0))
                                .text_size(px(12.5))
                                .text_color(theme.faint)
                                .child(format!(
                                    "The patch is longer than Reno shows: {shown} lines are here, the rest was cut."
                                )),
                        )
                    })
                    .into_any_element()
            }
        };
        div()
            .id(SharedString::from(selector.clone()))
            .debug_selector(move || selector.clone())
            .child(body)
            .into_any_element()
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
        .min_w_full()
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
