//! ROAD-31 draft, completions: how the list the box opens after `@`, `/` or `$` is drawn. A is a
//! popover rising from the box with the groups Files, Claude Code, Reno and Skills; B a sheet
//! docked to the box with the tabs Files, Commands and Skills and a preview; C a short list
//! inside the box under the caret line (the top match is also painted as ghost text after the
//! caret by `decor`). None of them is focusable: the box keeps the keyboard, and a click on a
//! row inserts without moving it.

use gpui::{
    Anchor as Corner, AnyElement, Context, FontWeight, Hsla, MouseButton, SharedString, anchored,
    deferred, div, point, prelude::*, px,
};

use super::{Completion, Composer, SHEET_ROW, SHEET_TOP, rows_cap, sheet_rows};
use crate::ui::composer::menu::menu;
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO};
use crate::ui::theme::Theme;
use crate::vm::draft31::{Entry, EntryKind, Trigger, Variant, trigger_fits};

fn glyph(kind: EntryKind) -> Icon {
    match kind {
        EntryKind::File => Icon::File,
        EntryKind::App => Icon::Mark,
        EntryKind::Harness => Icon::Bolt,
        EntryKind::Skill => Icon::Wand,
    }
}

/// C tints by kind: files in the accent, commands in the warning, skills in the success tone.
fn tint(theme: &Theme, kind: EntryKind) -> Hsla {
    match kind {
        EntryKind::File => theme.accent,
        EntryKind::App | EntryKind::Harness => theme.warning,
        EntryKind::Skill => theme.success,
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
    /// Whether a list is open and this variant draws it as an overlay (A, B).
    pub(in crate::ui::composer) fn road31_list<'a>(
        &'a self,
        cx: &gpui::App,
    ) -> Option<(&'a Completion, Variant)> {
        Some((self.road31.complete.as_ref()?, self.road31(cx)?))
    }

    /// A row. `size` is the label's point size; `compact` is C's tighter row.
    pub(in crate::ui::composer) fn road31_entry_row(
        &self,
        theme: &Theme,
        at: usize,
        entry: &Entry,
        selected: bool,
        variant: Variant,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let (ink, wash) = if variant == Variant::C {
            let ink = tint(theme, entry.kind);
            (ink, ink.opacity(0.14))
        } else {
            (theme.accent, theme.accent.opacity(0.14))
        };
        let (size, height) = match variant {
            Variant::A => (12.5, 28.0),
            Variant::B => (12.0, 26.0),
            Variant::C => (12.5, 26.0),
        };
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
                if selected || variant == Variant::C {
                    ink
                } else {
                    theme.muted
                },
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
                if let Some(c) = this.road31.complete.as_mut()
                    && c.selected != at
                {
                    c.selected = at;
                    cx.notify();
                }
            }))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.road31_accept(Some(at), window, cx);
            }))
    }

    /// The failure of a source, with the way to ask again.
    pub(in crate::ui::composer) fn road31_failure(
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
                    .on_click(cx.listener(|this, _, _, cx| this.road31_retry(cx))),
            )
    }

    /// The rows in view, with a header where the group changes (A, B).
    pub(in crate::ui::composer) fn road31_grouped_rows(
        &self,
        theme: &Theme,
        c: &Completion,
        variant: Variant,
        headers: bool,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let cap = rows_cap(variant, self.road31.sheet_cap.get());
        let mut list = div().flex().flex_col();
        if let Some(why) = &c.listing.failure {
            if headers && c.active.trigger == Trigger::Slash {
                list = list.child(Self::road31_header(theme, "Claude Code"));
            }
            list = list.child(self.road31_failure(theme, why, cx));
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
        let end = (c.top + cap).min(c.listing.entries.len());
        for at in c.top..end {
            let entry = &c.listing.entries[at];
            if headers && entry.group != group {
                group = &entry.group;
                list = list.child(Self::road31_header(theme, group));
            }
            list =
                list.child(self.road31_entry_row(theme, at, entry, c.selected == at, variant, cx));
        }
        list
    }

    pub(in crate::ui::composer) fn road31_header(theme: &Theme, text: &str) -> gpui::Div {
        div()
            .px(px(10.0))
            .pt(px(6.0))
            .pb(px(2.0))
            .text_size(px(11.5))
            .text_color(theme.faint)
            .child(SharedString::from(text.to_uppercase()))
    }

    /// The keys, as a small foot.
    pub(in crate::ui::composer) fn road31_hints(theme: &Theme, tab_switches: bool) -> gpui::Div {
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
            .child(hint(
                if tab_switches {
                    "\u{23ce}"
                } else {
                    "\u{23ce} Tab"
                },
                "insert",
            ))
            .children(tab_switches.then(|| hint("Tab", "switch")))
            .child(hint("Esc", "close"))
    }

    /// A and B: the list docked to the box. `None` for C (its list is in the box), or when
    /// nothing is open.
    pub(in crate::ui::composer) fn road31_completion_overlay(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<gpui::Deferred> {
        let (c, variant) = self.road31_list(cx)?;
        let bounds = self.road31.box_bounds.get()?;
        let panel = match variant {
            Variant::A => {
                let rows = self.road31_grouped_rows(theme, c, variant, true, cx);
                menu(theme, "complete-popover", 460.0)
                    .child(rows)
                    .child(Self::road31_hints(theme, false))
            }
            Variant::B => self.road31_sheet_panel(theme, c, f32::from(bounds.size.width), cx),
            Variant::C => return None,
        };
        let dx = if variant == Variant::A { 14.0 } else { 0.0 };
        Some(
            deferred(
                anchored()
                    .attach(bounds, Corner::TopLeft)
                    .anchor(Corner::BottomLeft)
                    .offset(point(px(dx), px(-8.0)))
                    .snap_to_window_with_margin(px(8.0))
                    .child(panel.occlude()),
            )
            .with_priority(1),
        )
    }

    /// B: tabs, results on the left, a preview on the right.
    pub(in crate::ui::composer) fn road31_sheet_panel(
        &self,
        theme: &Theme,
        c: &Completion,
        width: f32,
        cx: &mut Context<Self>,
    ) -> gpui::Stateful<gpui::Div> {
        let text = self.model.text().to_owned();
        let active = c.active.clone();
        let tabs = Trigger::ALL
            .into_iter()
            .map(|t| {
                let on = t == active.trigger;
                let fits = trigger_fits(&text, active.start, t);
                let a = active.clone();
                div()
                    .id(SharedString::from(format!("sheet-tab-{}", t.tab())))
                    .debug_selector(move || format!("sheet-tab-{}", t.tab()))
                    .h(px(26.0))
                    .px(px(10.0))
                    .flex()
                    .items_center()
                    .gap(px(5.0))
                    .rounded(px(8.0))
                    .text_size(px(12.5))
                    .when(on, |d| d.bg(theme.raised).text_color(theme.text))
                    .when(!on, |d| {
                        d.text_color(if fits { theme.muted } else { theme.faint })
                    })
                    .when(fits && !on, |d| {
                        d.cursor_pointer().hover(|s| s.bg(theme.row_selected()))
                    })
                    .child(
                        div()
                            .font_family(MONO)
                            .text_color(theme.accent)
                            .child(SharedString::from(t.glyph().to_string())),
                    )
                    .child(t.tab())
                    .on_mouse_down(MouseButton::Left, |_, window, cx| {
                        window.prevent_default();
                        cx.stop_propagation();
                    })
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if fits {
                            this.road31_set_trigger(&a, t, window, cx);
                        }
                    }))
            })
            .collect::<Vec<_>>();
        // The sheet stays under the status bar: show the whole rows that fit above the box.
        let room = self
            .road31
            .box_bounds
            .get()
            .map_or(400.0, |b| f32::from(b.origin.y) - SHEET_TOP - 8.0);
        let cap = sheet_rows(room);
        self.road31.sheet_cap.set(cap);
        let body_h = cap as f32 * SHEET_ROW + 2.0 * 24.0 + 12.0;
        let rows = self.road31_grouped_rows(theme, c, Variant::B, true, cx);
        let preview = self.road31_preview(theme, c);
        menu(theme, "complete-sheet", width)
            .p(px(0.0))
            .rounded(px(18.0))
            .overflow_hidden()
            .child(
                div()
                    .px(px(10.0))
                    .py(px(8.0))
                    .flex()
                    .items_center()
                    .gap(px(2.0))
                    .border_b_1()
                    .border_color(theme.hairline)
                    .children(tabs),
            )
            .child(
                div()
                    .flex()
                    .h(px(body_h))
                    .child(div().w(px(width * 0.52)).flex_none().p(px(6.0)).child(rows))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .border_l_1()
                            .border_color(theme.hairline)
                            .p(px(14.0))
                            .child(preview),
                    ),
            )
            .child(
                div()
                    .border_t_1()
                    .border_color(theme.hairline)
                    .pb(px(6.0))
                    .child(Self::road31_hints(theme, true)),
            )
    }

    /// The sheet's right side: a file's first lines, or a command's help and where it is from.
    pub(in crate::ui::composer) fn road31_preview(
        &self,
        theme: &Theme,
        c: &Completion,
    ) -> AnyElement {
        let Some(entry) = c.listing.entries.get(c.selected) else {
            return div()
                .text_size(px(12.0))
                .text_color(theme.faint)
                .child("Nothing to preview")
                .into_any_element();
        };
        let title = div()
            .font_family(MONO)
            .text_size(px(12.5))
            .font_weight(FontWeight::MEDIUM)
            .child(SharedString::from(entry.label.clone()));
        let source = div()
            .pt(px(10.0))
            .text_size(px(11.5))
            .text_color(theme.faint)
            .child(SharedString::from(entry.source.clone()));
        let body = if entry.kind == EntryKind::File {
            if entry.head.is_empty() {
                div()
                    .pt(px(8.0))
                    .text_size(px(12.0))
                    .text_color(theme.muted)
                    .child("Added to the message as a file the agent can read.")
                    .into_any_element()
            } else {
                div()
                    .id("complete-preview-lines")
                    .debug_selector(|| "complete-preview-lines".to_owned())
                    .pt(px(8.0))
                    .font_family(MONO)
                    .text_size(px(12.0))
                    .text_color(theme.muted)
                    .children(entry.head.iter().enumerate().map(|(n, line)| {
                        div()
                            .flex()
                            .gap(px(10.0))
                            .child(
                                div()
                                    .w(px(16.0))
                                    .text_color(theme.faint)
                                    .child(SharedString::from((n + 1).to_string())),
                            )
                            .child(SharedString::from(if line.is_empty() {
                                " ".to_owned()
                            } else {
                                line.clone()
                            }))
                    }))
                    .into_any_element()
            }
        } else {
            div()
                .pt(px(8.0))
                .flex()
                .flex_col()
                .gap(px(8.0))
                .text_size(px(12.5))
                .text_color(theme.muted)
                .child(SharedString::from(entry.detail.clone()))
                .children(entry.usage.clone().map(|u| {
                    div()
                        .font_family(MONO)
                        .text_size(px(12.0))
                        .text_color(theme.text)
                        .child(SharedString::from(u))
                }))
                .into_any_element()
        };
        div()
            .child(title)
            .child(body)
            .child(source)
            .into_any_element()
    }

    /// C: the short list inside the box, under the caret line.
    pub(in crate::ui::composer) fn road31_inline_list(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (c, variant) = self.road31_list(cx)?;
        if variant != Variant::C {
            return None;
        }
        let rows = self.road31_grouped_rows(theme, c, variant, false, cx);
        Some(
            div()
                .id("complete-inline")
                .debug_selector(|| "complete-inline".to_owned())
                .mx(px(12.0))
                .pt(px(4.0))
                .pb(px(2.0))
                .border_t_1()
                .border_color(theme.hairline)
                .child(rows)
                .child(Self::road31_hints(theme, false))
                .into_any_element(),
        )
    }
}
