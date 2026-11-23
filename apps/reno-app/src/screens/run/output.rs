//! What a tool printed, under its call in the transcript.
//!
//! Collapsed by default: a group opens to its calls, and a call with output opens to a mono
//! block with its first lines, "Show N more lines" and Copy. A call that failed is drawn in the
//! danger tone; an output the daemon cut says how big the whole was.

use std::sync::Arc;

use gpui::{
    AnyElement, App, ClipboardItem, Context, FontWeight, IntoElement, RenderOnce, SharedString,
    Window, div, img, prelude::*, px,
};
use reno_client::visuals::VisualImage;
use reno_proto::{ExecEvent, RunId, ToolOutput};

use super::RunScreen;
use super::select::Spot;
use crate::transcript::{Key, Outcome, ToolEntry};
use crate::ui::find as find_look;
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO};
use crate::ui::markdown_view::image::{self, ImageHost, ImageSource, Shown};
use crate::ui::markdown_view::link::clean_path;
use crate::ui::theme::Theme;
use crate::ui::toast;
use crate::vm::changes as facts;
use crate::vm::convo;
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
            self.keep_in_view(key, cx);
        }
    }

    /// One step of a tool group: a muted line with its icon, and under it, when the call printed
    /// something and the line is open, that output.
    pub(super) fn step_row(
        &self,
        theme: &Theme,
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
        let edit_calls = (line.icon == convo::StepIcon::Edit)
            .then(|| convo::line_calls(calls).get(ln).cloned())
            .flatten();
        let edits = edit_calls.clone().and_then(|run| {
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
                let tint = find_look::hit_style(theme, false)
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
            c.output
                .as_ref()
                .map(|out| self.output_block(theme, (key, ln), (at.unwrap_or(0), c), out, cx))
        });
        let images = if self.store.read(cx).capabilities().tool_images {
            let run = self.run_of(key);
            at.map(|n| n..n + 1)
                .or(edit_calls)
                .and_then(|range| calls.get(range))
                .into_iter()
                .flatten()
                .filter_map(|call| self.tool_result_images(theme, &run, call, cx))
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        div()
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(row)
            .children(images)
            .children(block)
            .into_any_element()
    }

    fn tool_result_images(
        &self,
        theme: &Theme,
        run: &RunId,
        call: &ToolEntry,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !call.finished() {
            return None;
        }
        let store = self.store.read(cx);
        let images = store.tool_images(run, &call.id);
        if images.is_empty() {
            return None;
        }
        let host = ImageHost {
            run: Some(run.clone()),
            client: store.client(),
        };
        let file = store.state().transcript(run).and_then(|transcript| {
            let input = transcript
                .events
                .iter()
                .rev()
                .find_map(|(_, event)| match event {
                    ExecEvent::Tool { id, call: request } if id == &call.id => Some(&request.input),
                    _ => None,
                })?;
            ["file_path", "path"]
                .into_iter()
                .find_map(|key| input.get(key).and_then(serde_json::Value::as_str))
                .and_then(clean_path)
        });
        Some(
            div()
                .ml(px(22.0))
                .flex()
                .flex_col()
                .items_start()
                .gap(px(6.0))
                .children(images.into_iter().map(|image| ToolImage {
                    call: call.id.clone(),
                    image,
                    host: host.clone(),
                    theme: *theme,
                }))
                .children(file.map(|path| {
                    kit::line_button(
                        theme,
                        format!("tool-image-file-{}", call.id),
                        None,
                        "Go to file",
                        kit::Tone::Quiet,
                        true,
                    )
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_file_at(&path, None, window, cx);
                    }))
                }))
                .into_any_element(),
        )
    }

    fn output_block(
        &self,
        theme: &Theme,
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

#[derive(IntoElement)]
struct ToolImage {
    call: String,
    image: VisualImage,
    host: ImageHost,
    theme: Theme,
}

impl RenderOnce for ToolImage {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let source = match self.image {
            VisualImage::Worktree(path) => clean_path(&path).map(ImageSource::Worktree),
            VisualImage::Data { mime, bytes } => image::raster_format(&mime)
                .map(|format| ImageSource::Data(Arc::new(gpui::Image::from_bytes(format, bytes)))),
        };
        let frame = div().debug_selector({
            let selector = format!("tool-image-{}", self.call);
            move || selector.clone()
        });
        match source.map_or(Shown::Failed, |source| {
            image::resolve(&source, &self.host, cx)
        }) {
            Shown::Ready(image) => {
                let Some(picture) = image.use_render_image(window, cx) else {
                    return frame;
                };
                let size = picture.size(0).map(|dimension| dimension.0 as f32);
                if size.width <= 0.0 || size.height <= 0.0 {
                    return frame;
                }
                let scale = (384.0 / size.width.max(size.height)).min(1.0);
                frame
                    .relative()
                    .w(px(size.width * scale))
                    .max_w_full()
                    .aspect_ratio(size.width / size.height)
                    .child(
                        img(picture)
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full()
                            .rounded(px(6.0)),
                    )
            }
            state => frame.child(
                image::note(
                    &self.theme,
                    if matches!(state, Shown::Loading) {
                        "Loading image\u{2026}"
                    } else {
                        "Image not loaded"
                    },
                )
                .debug_selector(move || format!("tool-image-status-{}", self.call)),
            ),
        }
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
