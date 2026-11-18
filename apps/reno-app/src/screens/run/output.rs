//! What a tool printed, under its call in the transcript.
//!
//! Collapsed by default: a group opens to its calls, and a call with output opens to a mono
//! block with its first lines, "Show N more lines" and Copy. A call that failed is drawn in the
//! danger tone; an output the daemon cut says how big the whole was.

use gpui::{AnyElement, ClipboardItem, Context, FontWeight, SharedString, div, prelude::*, px};
use reno_proto::ToolOutput;

use super::RunScreen;
use crate::transcript::{Key, Outcome, ToolEntry};
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::MONO;
use crate::ui::theme::Theme;
use crate::ui::toast;
use crate::vm::changes as facts;

/// Puts `text` on the clipboard and says so.
pub(super) fn copy(cx: &mut gpui::App, text: String, what: &'static str) {
    cx.write_to_clipboard(ClipboardItem::new_string(text));
    toast::push(cx, toast::Tone::Info, "Copied", what);
}

impl RunScreen {
    /// Opens or closes the output of one call, or shows all of it, and measures the row again.
    fn toggle_output(&mut self, key: Key, id: &str, full: bool, cx: &mut Context<Self>) {
        let set = if full {
            &mut self.outputs_full
        } else {
            &mut self.outputs_open
        };
        if !set.remove(id) {
            set.insert(id.to_owned());
        }
        if let Some(ix) = self.rows.iter().position(|r| r.key() == key) {
            self.list.splice(ix..ix + 1, 1);
        }
        cx.notify();
    }

    /// Whether the output of `call` shows: a group of one shows it as soon as it opens, a longer
    /// group lets each call open its own.
    fn shows_output(&self, call: &ToolEntry, group: usize) -> bool {
        call.output.is_some() && (group == 1 || self.outputs_open.contains(&call.id))
    }

    /// One call of an open group: its line and, when it shows, its output.
    pub(super) fn tool_entry(
        &self,
        theme: &Theme,
        key: Key,
        call: &ToolEntry,
        group: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let shown = self.shows_output(call, group);
        let (summary_text, color) = match &call.outcome {
            Outcome::Pending => ("running".to_owned(), theme.faint),
            Outcome::Ok(s) => (s.clone().unwrap_or_else(|| "ok".to_owned()), theme.muted),
            Outcome::Failed(s) => (
                s.clone().unwrap_or_else(|| "failed".to_owned()),
                theme.danger,
            ),
        };
        // A longer group's calls open one by one; a group of one needs no chevron.
        let expandable = call.output.is_some() && group > 1;
        let id = call.id.clone();
        let line = div()
            .id(SharedString::from(format!("tool-line-{id}")))
            .debug_selector({
                let id = id.clone();
                move || format!("tool-line-{id}")
            })
            .flex()
            .items_start()
            .justify_between()
            .gap(px(16.0))
            .px(px(14.0))
            .py(px(8.0))
            .when(expandable, |d| {
                let hover = theme.raised;
                let id = id.clone();
                d.cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_output(key, &id, false, cx);
                    }))
            })
            .child(
                div()
                    .min_w_0()
                    .flex()
                    .items_start()
                    .gap(px(8.0))
                    .when(expandable, |d| {
                        d.child(div().pt(px(2.0)).child(icon(
                            if shown { Icon::ChevDown } else { Icon::Fwd },
                            12.0,
                            theme.faint,
                        )))
                    })
                    .child(
                        div()
                            .min_w_0()
                            .font_family(MONO)
                            .text_size(px(12.5))
                            .child(SharedString::from(call.label.clone())),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .max_w(px(280.0))
                    .truncate()
                    .text_size(px(12.5))
                    .text_color(color)
                    .child(summary_text),
            );
        let block = call
            .output
            .as_ref()
            .filter(|_| shown)
            .map(|out| self.output_block(theme, key, call, out, cx));
        div()
            .flex()
            .flex_col()
            .child(line)
            .children(block)
            .into_any_element()
    }

    fn output_block(
        &self,
        theme: &Theme,
        key: Key,
        call: &ToolEntry,
        out: &ToolOutput,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = call.id.clone();
        let failed = matches!(call.outcome, Outcome::Failed(_));
        let full = self.outputs_full.contains(&id);
        let folded = facts::fold(&out.text, facts::OUTPUT_LINES);
        let body = if full {
            out.text.strip_suffix('\n').unwrap_or(&out.text)
        } else {
            folded.shown
        };
        let (fill, hover_fill, ink) = if failed {
            (
                theme.danger.opacity(0.07),
                theme.danger.opacity(0.11),
                theme.danger,
            )
        } else {
            (theme.shell, theme.raised, theme.text)
        };
        let copy_text = out.text.clone();
        let more = (!full && folded.hidden > 0).then(|| {
            let id = id.clone();
            div()
                .id(SharedString::from(format!("output-more-{id}")))
                .debug_selector({
                    let id = id.clone();
                    move || format!("output-more-{id}")
                })
                .text_size(px(12.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.accent)
                .cursor_pointer()
                .child(facts::more_label(folded.hidden))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_output(key, &id, true, cx);
                }))
        });
        let less = (full && folded.hidden > 0).then(|| {
            let id = id.clone();
            div()
                .id(SharedString::from(format!("output-less-{id}")))
                .text_size(px(12.5))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.muted)
                .cursor_pointer()
                .child("Show less")
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.toggle_output(key, &id, true, cx);
                }))
        });
        let cut = facts::cut_note(out);
        div()
            .mx(px(12.0))
            .mb(px(10.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                div()
                    .id(SharedString::from(format!("tool-output-{id}")))
                    .debug_selector({
                        let id = id.clone();
                        move || format!("tool-output-{id}")
                    })
                    .w_full()
                    .px(px(14.0))
                    .py(px(9.0))
                    .rounded(px(10.0))
                    .bg(fill)
                    .hover(move |s| s.bg(hover_fill))
                    .font_family(MONO)
                    .text_size(px(12.0))
                    .line_height(px(18.0))
                    .text_color(ink)
                    .children(body.split('\n').map(|line| {
                        div()
                            .min_h(px(18.0))
                            .child(SharedString::from(line.to_owned()))
                    })),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .px(px(2.0))
                    .children(more)
                    .children(less)
                    .when_some(cut, |d, note| {
                        d.child(
                            div()
                                .debug_selector({
                                    let id = id.clone();
                                    move || format!("output-cut-{id}")
                                })
                                .text_size(px(12.5))
                                .text_color(theme.faint)
                                .child(note),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        div()
                            .id(SharedString::from(format!("output-copy-{id}")))
                            .debug_selector({
                                let id = id.clone();
                                move || format!("output-copy-{id}")
                            })
                            .text_size(px(12.5))
                            .text_color(theme.muted)
                            .cursor_pointer()
                            .hover({
                                let text = theme.text;
                                move |s| s.text_color(text)
                            })
                            .child("Copy")
                            .on_click(move |_, _, cx| {
                                copy(cx, copy_text.clone(), "Output copied");
                            }),
                    ),
            )
            .into_any_element()
    }
}
