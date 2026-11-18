//! What the run changed: a "Changes" section in the details pane, a result line under the Done
//! row, and the Changes view (the files, and the patch of the one picked).
//!
//! Everything here draws what `vm::changes` decided; the daemon's answers are in the store.

use gpui::{
    AnyElement, App, Context, FontWeight, Hsla, SharedString, Stateful, div, point, prelude::*, px,
};

use super::RunScreen;
use super::output::copy;
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO, Tone};
use crate::ui::theme::Theme;
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
/// Width of the file list in the Changes view.
const FILES_WIDTH: f32 = 330.0;
/// Height of one line of a patch.
const LINE: f32 = 18.0;

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

    pub(super) fn open_changes(&mut self, cx: &mut Context<Self>) {
        self.showing_changes = true;
        self.ensure_selection(cx);
        cx.notify();
    }

    pub(super) fn close_changes(&mut self, cx: &mut Context<Self>) {
        self.showing_changes = false;
        cx.notify();
    }

    fn select_file(&mut self, path: String, cx: &mut Context<Self>) {
        self.selected = Some(path);
        self.diff_scroll.set_offset(point(px(0.0), px(0.0)));
        let run = self.run.clone();
        if let Some(path) = self.selected.clone() {
            self.store
                .update(cx, |store, _| store.forget_failed_patch(&run, &path));
        }
        self.ensure_selection(cx);
        cx.notify();
    }

    /// Keeps a file picked while the Changes view shows (the first, until the user picks one)
    /// and asks for its patch.
    pub(super) fn ensure_selection(&mut self, cx: &mut Context<Self>) {
        if !self.showing_changes {
            return;
        }
        let Pane::Files(overview) = self.pane(cx) else {
            return;
        };
        if !self
            .selected
            .as_ref()
            .is_some_and(|p| overview.rows.iter().any(|r| &r.path == p))
        {
            self.selected = overview.rows.first().map(|r| r.path.clone());
        }
        let (Some(path), run) = (self.selected.clone(), self.run.clone()) else {
            return;
        };
        let binary = overview.rows.iter().any(|r| r.path == path && r.binary);
        if !binary {
            self.store
                .update(cx, |store, cx| store.load_patch(&run, &path, cx));
        }
        // The rows of the patch, parsed once per answer.
        let loaded = self
            .store
            .read(cx)
            .patch(&run, &path)
            .cloned()
            .unwrap_or(facts::Loaded::Loading);
        if let Some(row) = overview.rows.iter().find(|r| r.path == path) {
            let key = (path, loaded);
            if self.patch_key.as_ref() != Some(&key) {
                self.patch_pane = facts::patch_pane(row, &key.1);
                self.patch_key = Some(key);
            }
        }
    }

    // --- the details pane --------------------------------------------------------------------

    /// The "CHANGES" section of the details pane, `None` for a run without a workspace.
    pub(super) fn changes_section(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let pane = self.pane(cx);
        let faint = |text: String| {
            div()
                .text_size(px(13.5))
                .text_color(theme.muted)
                .child(SharedString::from(text))
        };
        let body: AnyElement = match &pane {
            Pane::Hidden => return None,
            Pane::Unsupported => {
                faint("This daemon is too old to show what a run changed. Update renod.".to_owned())
                    .into_any_element()
            }
            Pane::Loading => faint("Reading the changes\u{2026}".to_owned()).into_any_element(),
            Pane::Failed(why) => div()
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(faint(format!("Could not read the changes: {why}")))
                .child(
                    div().child(
                        kit::pill_button(theme, "changes-retry", "Try again", Tone::Quiet)
                            .on_click(cx.listener(|this, _, _, cx| this.reload_changes(cx))),
                    ),
                )
                .into_any_element(),
            Pane::Removed { branch } => faint(format!(
                "The workspace was removed \u{2014} nothing to show. The branch was {branch}."
            ))
            .into_any_element(),
            Pane::Empty { .. } => faint("This run made no changes.".to_owned()).into_any_element(),
            Pane::Files(o) => div()
                .id("details-changes")
                .debug_selector(|| "details-changes".to_owned())
                .flex()
                .flex_col()
                .gap(px(8.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(7.0))
                        .child(icon(Icon::Branch, 14.0, theme.muted))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .font_family(MONO)
                                .text_size(px(12.5))
                                .child(SharedString::from(o.branch.clone())),
                        ),
                )
                .child(div().text_size(px(13.5)).child(SharedString::from(format!(
                    "{} \u{b7} {}",
                    o.commits, o.stats
                ))))
                .child(
                    div().child(
                        kit::pill_button(theme, "open-changes", "View changes", Tone::Quiet)
                            .on_click(cx.listener(|this, _, _, cx| this.open_changes(cx))),
                    ),
                )
                .into_any_element(),
        };
        Some(
            div()
                .flex()
                .flex_col()
                .child(div().h(px(14.0)))
                .child(
                    div()
                        .text_size(px(12.0))
                        .text_color(theme.faint)
                        .pb(px(4.0))
                        .child("CHANGES"),
                )
                .child(
                    div()
                        .py(px(6.0))
                        .debug_selector({
                            let tag = tag(&pane);
                            move || format!("section-{tag}")
                        })
                        .child(body),
                )
                .into_any_element(),
        )
    }

    /// Reads the changes again (the "Try again" of a failed read).
    pub(super) fn reload_changes(&mut self, cx: &mut Context<Self>) {
        let run = self.run.clone();
        self.store
            .update(cx, |store, cx| store.load_changes(&run, cx));
    }

    /// The header's way into the Changes view: there when the run has changes to show or say
    /// nothing about.
    pub(super) fn changes_button(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let label = match self.pane(cx) {
            Pane::Files(o) => format!("Changes \u{b7} {}", o.rows.len()),
            Pane::Empty { .. } | Pane::Removed { .. } | Pane::Failed(_) | Pane::Loading => {
                "Changes".to_owned()
            }
            Pane::Hidden | Pane::Unsupported => return None,
        };
        let open = self.showing_changes;
        Some(
            kit::pill_button(theme, "header-changes", label, Tone::Quiet)
                .when(open, |b| b.bg(theme.raised))
                .on_click(cx.listener(move |this, _, _, cx| {
                    if this.showing_changes {
                        this.close_changes(cx);
                    } else {
                        this.open_changes(cx);
                    }
                }))
                .into_any_element(),
        )
    }

    /// "Branch reno/x has 3 commits, 7 files changed (+120 −30)", under the Done row of a
    /// finished run, opening the Changes view.
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
                .on_click(cx.listener(|this, _, _, cx| this.open_changes(cx)))
                .into_any_element(),
        )
    }

    // --- the view ----------------------------------------------------------------------------

    /// The Changes view, in place of the transcript.
    pub(super) fn changes_view(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let pane = self.pane(cx);
        let note = |text: String, tone: Hsla| {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px(px(40.0))
                .text_size(px(14.0))
                .text_color(tone)
                .child(SharedString::from(text))
                .into_any_element()
        };
        let body = match &pane {
            Pane::Hidden => note(
                "There is nothing to show for this run.".to_owned(),
                theme.muted,
            ),
            Pane::Unsupported => note(
                "This daemon is too old to show what a run changed. Update renod and look again."
                    .to_owned(),
                theme.muted,
            ),
            Pane::Loading => note("Reading the changes\u{2026}".to_owned(), theme.faint),
            Pane::Failed(why) => note(format!("Could not read the changes: {why}"), theme.danger),
            Pane::Removed { branch } => note(
                format!(
                    "The workspace was removed \u{2014} nothing to show. \
                     Whatever was kept is on the branch {branch}."
                ),
                theme.muted,
            ),
            Pane::Empty { .. } => note("This run made no changes.".to_owned(), theme.muted),
            Pane::Files(o) => self.files_and_patch(theme, o, cx),
        };
        let overview = match &pane {
            Pane::Files(o) => Some(o.clone()),
            _ => None,
        };
        div()
            .id("changes-view")
            .debug_selector(|| "changes-view".to_owned())
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .flex_col()
            .child(self.changes_bar(theme, &pane, overview.as_ref(), cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .debug_selector({
                        let tag = tag(&pane);
                        move || format!("view-{tag}")
                    })
                    .child(body),
            )
            .into_any_element()
    }

    /// Back to the transcript, the branch and what can be copied.
    fn changes_bar(
        &self,
        theme: &Theme,
        pane: &Pane,
        overview: Option<&Overview>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let branch = match pane {
            Pane::Files(o) => Some(o.branch.clone()),
            Pane::Empty { branch } | Pane::Removed { branch } => Some(branch.clone()),
            _ => None,
        };
        let path = overview.and_then(|o| o.path.clone());
        let link = |id: &'static str, label: &'static str| {
            div()
                .id(id)
                .debug_selector(move || id.to_owned())
                .flex_none()
                .text_size(px(12.5))
                .text_color(theme.muted)
                .cursor_pointer()
                .hover({
                    let text = theme.text;
                    move |s| s.text_color(text)
                })
                .child(label)
        };
        div()
            .flex_none()
            .px(px(22.0))
            .py(px(12.0))
            .flex()
            .items_center()
            .gap(px(12.0))
            .border_b_1()
            .border_color(theme.hairline)
            .child(
                kit::pill_button(theme, "changes-back", "Back to the run", Tone::Quiet)
                    .on_click(cx.listener(|this, _, _, cx| this.close_changes(cx))),
            )
            .when_some(branch, |d, branch| {
                let copied = branch.clone();
                d.child(icon(Icon::Branch, 14.0, theme.muted))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family(MONO)
                            .text_size(px(12.5))
                            .child(SharedString::from(branch)),
                    )
                    .child(
                        link("copy-branch", "Copy branch").on_click(move |_, _, cx| {
                            copy(cx, copied.clone(), "Branch name copied");
                        }),
                    )
            })
            .when_some(path, |d, path| {
                d.child(link("copy-path", "Copy path").on_click(move |_, _, cx| {
                    copy(cx, path.clone(), "Worktree path copied");
                }))
            })
            .when_some(overview, |d, o| {
                d.child(div().flex_1()).child(
                    div()
                        .flex_none()
                        .text_size(px(12.5))
                        .text_color(theme.faint)
                        .child(SharedString::from(format!(
                            "{} \u{b7} {}",
                            o.commits, o.stats
                        ))),
                )
            })
            .into_any_element()
    }

    fn files_and_patch(&self, theme: &Theme, o: &Overview, cx: &mut Context<Self>) -> AnyElement {
        let list = div()
            .id("change-files")
            .w(px(FILES_WIDTH))
            .flex_none()
            .h_full()
            .overflow_y_scroll()
            .border_r_1()
            .border_color(theme.hairline)
            .py(px(8.0))
            .children(o.rows.iter().map(|row| self.file_line(theme, row, cx)));
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .child(list)
            .child(self.patch_view(theme, o))
            .into_any_element()
    }

    fn file_line(
        &self,
        theme: &Theme,
        row: &FileRow,
        cx: &mut Context<Self>,
    ) -> Stateful<gpui::Div> {
        let selected = self.selected.as_deref() == Some(row.path.as_str());
        let path = row.path.clone();
        let selector = format!("change-file-{}", row.path);
        let stats = facts::stat_labels(row);
        let hover = theme.raised;
        div()
            .id(SharedString::from(selector.clone()))
            .debug_selector(move || selector.clone())
            .flex()
            .items_center()
            .gap(px(10.0))
            .mx(px(8.0))
            .px(px(10.0))
            .py(px(6.0))
            .rounded(px(8.0))
            .cursor_pointer()
            .when(selected, |d| d.bg(theme.raised))
            .hover(move |s| s.bg(hover))
            .on_click(cx.listener(move |this, _, _, cx| this.select_file(path.clone(), cx)))
            .child(
                div()
                    .flex_none()
                    .w(px(14.0))
                    .font_family(MONO)
                    .text_size(px(12.0))
                    .font_weight(FontWeight::BOLD)
                    .text_color(tone_color(theme, row.tone))
                    .child(row.letter.to_string()),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .child(
                        div().truncate().text_size(px(13.0)).child(
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
                    .when_some(row.from.clone(), |d, from| {
                        d.child(
                            div()
                                .truncate()
                                .text_size(px(11.5))
                                .text_color(theme.faint)
                                .child(SharedString::from(format!("from {from}"))),
                        )
                    }),
            )
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
    }

    fn patch_view(&self, theme: &Theme, o: &Overview) -> AnyElement {
        let note = |text: &str, tone: Hsla| {
            div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .px(px(40.0))
                .text_size(px(14.0))
                .text_color(tone)
                .child(SharedString::from(text.to_owned()))
                .into_any_element()
        };
        let Some(path) = self.selected.as_deref() else {
            return note("Pick a file to see what changed in it.", theme.muted);
        };
        match &self.patch_pane {
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
                let renamed = o
                    .rows
                    .iter()
                    .find(|r| r.path == path)
                    .and_then(|r| r.from.clone());
                div()
                    .id("change-patch")
                    .debug_selector(|| "change-patch".to_owned())
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex_none()
                            .px(px(18.0))
                            .py(px(9.0))
                            .border_b_1()
                            .border_color(theme.hairline)
                            .font_family(MONO)
                            .text_size(px(12.5))
                            .child(SharedString::from(match renamed {
                                Some(from) => format!("{from} \u{2192} {path}"),
                                None => path.to_owned(),
                            })),
                    )
                    .child(
                        div()
                            .id("change-diff")
                            .flex_1()
                            .min_h_0()
                            .overflow_scroll()
                            .track_scroll(&self.diff_scroll)
                            .py(px(6.0))
                            .children(
                                rows.iter()
                                    .take(MAX_LINES)
                                    .map(|row| diff_line(theme, row, digits)),
                            )
                            .when(cut, |d| {
                                d.child(
                                    div()
                                        .debug_selector(|| "patch-cut".to_owned())
                                        .px(px(18.0))
                                        .py(px(10.0))
                                        .text_size(px(12.5))
                                        .text_color(theme.faint)
                                        .child(format!(
                                            "The patch is longer than Reno shows: {shown} lines are here, the rest was cut."
                                        )),
                                )
                            }),
                    )
                    .into_any_element()
            }
        }
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
