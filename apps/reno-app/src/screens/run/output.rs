//! What a tool printed, under its call in the transcript.
//!
//! Collapsed by default: a group opens to its calls, and a call with output opens to a mono
//! block with its first lines, "Show N more lines" and Copy. A call that failed is drawn in the
//! danger tone; an output the daemon cut says how big the whole was.

use gpui::{AnyElement, ClipboardItem, Context, FontWeight, SharedString, div, prelude::*, px};
use reno_proto::ToolOutput;

use super::RunScreen;
use super::select::Spot;
use crate::transcript::{Key, Outcome, ToolEntry};
use crate::ui::find as find_look;
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::MONO;
use crate::ui::theme::Theme;
use crate::ui::toast;
use crate::vm::changes as facts;
use crate::vm::convo;
use crate::vm::draft33::Variant;
use crate::vm::find;
use crate::vm::selection::{Kind, Slot};

/// Puts `text` on the clipboard and says so.
pub(super) fn copy(cx: &mut gpui::App, text: String, what: &'static str) {
    cx.write_to_clipboard(ClipboardItem::new_string(text));
    toast::push(cx, toast::Tone::Info, "Copied", what);
}

impl RunScreen {
    /// Opens or closes the output of one call, or shows all of it, and measures the row again.
    /// Opening one while the view follows the tail stops following, so what grew stays in view.
    fn toggle_output(&mut self, key: Key, id: &str, full: bool, cx: &mut Context<Self>) {
        let set = if full {
            &mut self.outputs_full
        } else {
            &mut self.outputs_open
        };
        let opening = !set.contains(&id.to_owned());
        let following = self.list.is_following_tail();
        set.toggle(id.to_owned());
        self.reflow(key, cx);
        if opening && following {
            self.keep_in_view(key);
        }
    }

    /// One step of a tool group: a muted line with its icon, and under it, when the call printed
    /// something and the line is open, that output.
    pub(super) fn step_row(
        &self,
        theme: &Theme,
        variant: Variant,
        (key, ln): (Key, usize),
        line: &convo::StepLine,
        calls: &[ToolEntry],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (at, call) = line
            .call
            .as_ref()
            .and_then(|id| calls.iter().enumerate().find(|(_, c)| &c.id == id))
            .map_or((None, None), |(n, c)| (Some(n), Some(c)));
        // The matches in the command or path the line shows. A match in a part of the label the
        // line does not show (a run of edits says only "Edited 2 files") tints the whole line.
        let hits = at.map_or_else(Vec::new, |n| {
            self.find_highlights(key, find::Part::Label(n))
        });
        let (painted, unpainted) = match call.and_then(|c| convo::label_in_line(c, &line.text)) {
            Some((label_at, line_at)) => {
                let label_len = call.map_or(0, |c| c.label.trim_end().len());
                let (inside, outside): (Vec<_>, Vec<_>) = hits
                    .into_iter()
                    .partition(|(r, _)| r.start >= label_at && r.end <= label_len);
                let moved = inside
                    .into_iter()
                    .map(|(r, cur)| {
                        (
                            r.start - label_at + line_at..r.end - label_at + line_at,
                            cur,
                        )
                    })
                    .collect::<Vec<_>>();
                (moved, outside)
            }
            None => (Vec::new(), hits),
        };
        // A run of edits says only "Edited 2 files": where a match is in a file's path, the line
        // names that file, with the match painted.
        let edits = (line.icon == convo::StepIcon::Edit)
            .then(|| convo::line_calls(calls).get(ln).cloned())
            .flatten()
            .and_then(|run| {
                let verb = line.text.split(' ').next().unwrap_or("Edited");
                convo::edit_line_with_hits(calls, run, verb, &|n| {
                    self.find_highlights(key, find::Part::Label(n))
                })
            });
        let (line_text, painted, unpainted) = match edits {
            Some((text, shown)) => (text, shown, Vec::new()),
            None => (line.text.clone(), painted, unpainted),
        };
        let output = call.filter(|c| c.output.is_some());
        let shown = output.is_some_and(|c| self.outputs_open.contains(&c.id));
        let glyph = match line.icon {
            convo::StepIcon::Terminal => Icon::Terminal,
            convo::StepIcon::Read => Icon::Search,
            convo::StepIcon::Edit => Icon::Pencil,
            convo::StepIcon::Fetch => Icon::Globe,
            convo::StepIcon::Tool => Icon::Spark,
        };
        let opacity = if line.live { Self::shimmer(cx) } else { 1.0 };
        let id = line.call.clone().unwrap_or_else(|| "edits".to_owned());
        let selector = format!("step-{id}");
        let row = div()
            .id(SharedString::from(selector.clone()))
            .debug_selector(move || selector.clone())
            .flex()
            .items_center()
            .gap(px(8.0))
            .text_size(px(13.0))
            .text_color(theme.muted)
            .when_some(output.map(|c| c.id.clone()), |d, call_id| {
                let text = theme.text;
                d.cursor_pointer()
                    .hover(move |s| s.text_color(text))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_output(key, &call_id, false, cx);
                    }))
            })
            .child(icon(
                glyph,
                14.0,
                if line.failed {
                    theme.danger
                } else {
                    theme.faint
                },
            ))
            .when(!unpainted.is_empty(), |d| {
                let current = unpainted.iter().any(|(_, cur)| *cur);
                let tint = find_look::hit_style(theme, variant, false)
                    .background_color
                    .unwrap_or(theme.raised);
                d.bg(tint)
                    .rounded(px(5.0))
                    .px(px(6.0))
                    .mx(px(-6.0))
                    .border_l_2()
                    .border_color(if current {
                        theme.accent
                    } else {
                        gpui::transparent_black()
                    })
            })
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .opacity(opacity)
                    .when(line.live, |d| d.debug_selector(|| "step-live".to_owned()))
                    .child(self.text_piece(
                        theme,
                        variant,
                        Spot {
                            key,
                            slot: Slot::new(ln, Kind::Step),
                            base: 0,
                            len: 0,
                            total: None,
                        },
                        &line_text,
                        &painted,
                    )),
            )
            .when(output.is_some(), |d| {
                d.child(icon(
                    if shown { Icon::ChevDown } else { Icon::Fwd },
                    12.0,
                    theme.faint,
                ))
            });
        let block = output.filter(|_| shown).and_then(|c| {
            c.output.as_ref().map(|out| {
                self.output_block(theme, variant, (key, ln), (at.unwrap_or(0), c), out, cx)
            })
        });
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(row)
            .children(block)
            .into_any_element()
    }

    fn output_block(
        &self,
        theme: &Theme,
        variant: Variant,
        (key, ln): (Key, usize),
        (at, call): (usize, &ToolEntry),
        out: &ToolOutput,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = call.id.clone();
        let hits = self.find_highlights(key, find::Part::Output(at));
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
                    .children({
                        let mut start = 0;
                        lines_with(body, &hits)
                            .into_iter()
                            .map(|(line, here)| {
                                let base = start;
                                start += line.len() + 1;
                                div().min_h(px(18.0)).child(self.text_piece(
                                    theme,
                                    variant,
                                    Spot {
                                        key,
                                        slot: Slot::new(ln, Kind::Output),
                                        base,
                                        len: line.len(),
                                        total: Some(body.len()),
                                    },
                                    line,
                                    &here,
                                ))
                            })
                            .collect::<Vec<_>>()
                    }),
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
                                .child(self.text_piece(
                                    theme,
                                    variant,
                                    Spot {
                                        key,
                                        slot: Slot::new(ln, Kind::Cut),
                                        base: 0,
                                        len: 0,
                                        total: None,
                                    },
                                    &note,
                                    &[],
                                )),
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

/// Matches in a text, as byte ranges with whether each is the current one.
type Marks = Vec<(std::ops::Range<usize>, bool)>;

/// The lines of `body`, each with the matches that fall in it, as byte ranges of the line.
fn lines_with<'a>(body: &'a str, hits: &[(std::ops::Range<usize>, bool)]) -> Vec<(&'a str, Marks)> {
    let mut start = 0;
    body.split('\n')
        .map(|line| {
            let end = start + line.len();
            let here = hits
                .iter()
                .filter(|(r, _)| r.start >= start && r.end <= end)
                .map(|(r, cur)| (r.start - start..r.end - start, *cur))
                .collect();
            start = end + 1;
            (line, here)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::lines_with;

    #[test]
    fn matches_are_put_on_the_line_they_are_in_with_line_relative_offsets() {
        let lines = lines_with("ab\ncd needle\nef", &[(6..12, true)]);
        assert_eq!(lines.len(), 3);
        assert!(lines[0].1.is_empty() && lines[2].1.is_empty());
        assert_eq!(lines[1].0, "cd needle");
        assert_eq!(lines[1].1, vec![(3..9, true)]);
    }
}
