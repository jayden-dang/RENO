//! The completions list: a popover rising from the box, grouped (Files, Claude Code, Reno,
//! Skills), at most eight rows, with the keys under it. It is not focusable: the box keeps the
//! keyboard, and a click on a row inserts without moving it.

use gpui::{
    Anchor as Corner, Context, MouseButton, SharedString, anchored, deferred, div, point,
    prelude::*, px,
};

use super::{Completion, Composer, ROWS};
use crate::ui::composer::menu::menu;
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO};
use crate::ui::theme::Theme;
use crate::vm::completions::{Entry, EntryKind, Trigger};

fn glyph(kind: EntryKind) -> Icon {
    match kind {
        EntryKind::File => Icon::File,
        EntryKind::App => Icon::Mark,
        EntryKind::Harness => Icon::Bolt,
        EntryKind::Skill => Icon::Wand,
    }
}

/// What a row calls itself: a file by its name (the folder is the muted line), a command whole.
fn row_name(entry: &Entry) -> &str {
    match entry.kind {
        EntryKind::File => entry.label.rsplit('/').next().unwrap_or(&entry.label),
        _ => &entry.label,
    }
}

impl Composer {
    /// One row.
    pub(in crate::ui::composer) fn completion_entry_row(
        &self,
        theme: &Theme,
        at: usize,
        entry: &Entry,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let (ink, wash) = (theme.accent, theme.accent.opacity(0.14));
        let (size, height) = (12.5, 28.0);
        div()
            .id(SharedString::from(format!("complete-row-{at}")))
            .debug_selector(move || format!("complete-row-{at}"))
            .w_full()
            .h(px(height))
            .px(px(10.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .rounded(px(7.0))
            .cursor_pointer()
            .text_size(px(size))
            .when(selected, |r| r.bg(wash))
            .when(!selected, |r| r.hover(|s| s.bg(theme.row_selected())))
            .child(icon(
                glyph(entry.kind),
                14.0,
                if selected { ink } else { theme.muted },
            ))
            .child(
                div()
                    .flex_none()
                    .max_w(px(220.0))
                    .truncate()
                    .font_family(MONO)
                    .text_size(px(size - 0.5))
                    .text_color(theme.text)
                    .child(SharedString::from(row_name(entry).to_owned())),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(px(size - 0.5))
                    .text_color(theme.faint)
                    .child(SharedString::from(entry.detail.clone())),
            )
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                // The box keeps the keyboard.
                window.prevent_default();
                cx.stop_propagation();
            })
            .on_mouse_move(cx.listener(move |this, _, _, cx| {
                if let Some(c) = this.completions.complete.as_mut()
                    && c.selected != at
                {
                    c.selected = at;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.completion_accept(Some(at), window, cx);
            }))
    }

    /// The failure of a source, with the way to ask again.
    pub(in crate::ui::composer) fn completion_failure(
        &self,
        theme: &Theme,
        why: &str,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id("complete-failure")
            .debug_selector(|| "complete-failure".to_owned())
            .px(px(10.0))
            .py(px(6.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .text_size(px(12.5))
            .text_color(theme.warning)
            .child(icon(Icon::PhWarning, 14.0, theme.warning))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .child(SharedString::from(why.to_owned())),
            )
            .child(
                div()
                    .id("complete-retry")
                    .debug_selector(|| "complete-retry".to_owned())
                    .flex_none()
                    .px(px(8.0))
                    .py(px(2.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .text_color(theme.muted)
                    .hover(|s| s.bg(theme.row_selected()))
                    .child("Try again")
                    .on_mouse_down(MouseButton::Left, |_, window, cx| {
                        window.prevent_default();
                        cx.stop_propagation();
                    })
                    .on_click(cx.listener(|this, _, _, cx| this.completion_retry(cx))),
            )
    }

    /// The rows in view, with a header where the group changes
    pub(in crate::ui::composer) fn completion_grouped_rows(
        &self,
        theme: &Theme,
        c: &Completion,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let mut list = div().flex().flex_col();
        if let Some(why) = &c.listing.failure {
            if c.active.trigger == Trigger::Slash {
                list = list.child(Self::completion_header(theme, "Claude Code"));
            }
            list = list.child(self.completion_failure(theme, why, cx));
        }
        if c.listing.entries.is_empty() && c.listing.failure.is_none() {
            let words = if c.listing.pending {
                "Searching\u{2026}".to_owned()
            } else {
                match c.active.trigger {
                    Trigger::At => "No file matches".to_owned(),
                    Trigger::Slash => "No command matches".to_owned(),
                    Trigger::Dollar => "No skill matches".to_owned(),
                }
            };
            return list.child(
                div()
                    .id("complete-empty")
                    .debug_selector(|| "complete-empty".to_owned())
                    .px(px(10.0))
                    .py(px(10.0))
                    .text_size(px(12.5))
                    .text_color(theme.faint)
                    .child(SharedString::from(words)),
            );
        }
        let mut group = "";
        let end = (c.top + ROWS).min(c.listing.entries.len());
        for at in c.top..end {
            let entry = &c.listing.entries[at];
            if entry.group != group {
                group = &entry.group;
                list = list.child(Self::completion_header(theme, group));
            }
            list = list.child(self.completion_entry_row(theme, at, entry, c.selected == at, cx));
        }
        list
    }

    pub(in crate::ui::composer) fn completion_header(theme: &Theme, text: &str) -> gpui::Div {
        div()
            .px(px(10.0))
            .pt(px(6.0))
            .pb(px(2.0))
            .text_size(px(11.5))
            .text_color(theme.faint)
            .child(SharedString::from(text.to_uppercase()))
    }

    /// The keys, as a small foot.
    pub(in crate::ui::composer) fn completion_hints(theme: &Theme) -> gpui::Div {
        let hint = |key: &'static str, what: &'static str| {
            div()
                .flex()
                .items_center()
                .gap(px(4.0))
                .child(kit::keycap(theme, key, None))
                .child(what)
        };
        div()
            .flex_none()
            .px(px(10.0))
            .pt(px(6.0))
            .pb(px(2.0))
            .flex()
            .items_center()
            .gap(px(12.0))
            .text_size(px(11.5))
            .text_color(theme.faint)
            .child(hint("\u{2191}\u{2193}", "move"))
            .child(hint("\u{23ce} Tab", "insert"))
            .child(hint("Esc", "close"))
    }

    /// The list docked to the box, rising from its left edge. `None` when nothing is open.
    pub(in crate::ui::composer) fn completion_overlay(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Deferred> {
        let c = self.completions.complete.as_ref()?;
        let bounds = self.completions.box_bounds.get()?;
        let rows = self.completion_grouped_rows(theme, c, cx);
        let panel = menu(theme, "complete-popover", 460.0)
            .child(rows)
            .child(Self::completion_hints(theme));
        Some(
            deferred(
                anchored()
                    .attach(bounds, Corner::TopLeft)
                    .anchor(Corner::BottomLeft)
                    .offset(point(px(14.0), px(-8.0)))
                    .snap_to_window_with_margin(px(8.0))
                    .child(panel.occlude()),
            )
            .with_priority(1),
        )
    }
}
