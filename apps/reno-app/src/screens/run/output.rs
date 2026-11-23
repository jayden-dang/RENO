//! What a tool printed, under its call in the transcript.
//!
//! Each group shows its calls. Output opens to a mono block with its first lines,
//! "Show N more lines" and Copy. A call that failed is drawn in the
//! danger tone; an output the daemon cut says how big the whole was.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::Arc;

use gpui::{
    AnyElement, App, ClipboardItem, Context, FontWeight, IntoElement, RenderOnce, Role,
    SharedString, Window, div, img, prelude::*, px,
};
use reno_client::{Transcript, visuals::VisualImage};
use reno_proto::{ActionClass, ExecEvent, RunId, RunStatus, RunSummary, Seq, ToolCall, ToolOutput};
use serde_json::Value;

use super::RunScreen;
use super::select::Spot;
use crate::prefs::Prefs;
use crate::transcript::{Key, Outcome, Row, ToolEntry};
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
use crate::vm::selection::{Kind, Slot, ToolInput, ToolInputPart, ToolInputs};

pub(super) type ToolKey = (RunId, u64, String);

/// The event stream can get ahead of the run summary. Keep the source ranges until the summary
/// confirms their attempt, with one range per unchanged expansion preference.
pub(super) struct ToolEvents {
    through: Option<Seq>,
    attempt: Option<u32>,
    waiting: Vec<ToolBatch>,
    closed: HashSet<ToolKey>,
}

struct ToolBatch {
    first: Seq,
    last: Seq,
    expansion: (bool, bool),
}

impl ToolEvents {
    pub(super) fn from_loaded(transcript: Option<&Transcript>) -> Self {
        let mut cursor = Self {
            through: None,
            attempt: Some(1),
            waiting: Vec::new(),
            closed: HashSet::new(),
        };
        if let Some(transcript) = transcript {
            cursor.advance(transcript);
        }
        cursor
    }

    fn advance<'a>(&mut self, transcript: &'a Transcript) -> &'a [(Seq, ExecEvent)] {
        let after = transcript
            .events
            .partition_point(|(seq, _)| Some(*seq) <= self.through);
        let new = &transcript.events[after..];
        self.through = transcript.highest;
        let boundary = new.iter().rposition(|(_, event)| {
            matches!(event, ExecEvent::Done { .. } | ExecEvent::Resumed { .. })
        });
        if let Some(at) = boundary {
            self.waiting.clear();
            self.closed.clear();
            self.attempt = match new[at].1 {
                ExecEvent::Resumed { from_attempt, .. } => Some(from_attempt.saturating_add(1)),
                _ => None,
            };
            &new[at + 1..]
        } else {
            new
        }
    }

    fn observe(&mut self, transcript: &Transcript, expansion: (bool, bool)) {
        let new = self.advance(transcript);
        if self.attempt.is_none()
            || expansion == (false, false)
            || (self.waiting.is_empty()
                && !new
                    .iter()
                    .any(|(_, event)| matches!(event, ExecEvent::Tool { .. })))
        {
            return;
        }
        let (Some(&(first, _)), Some(&(last, _))) = (new.first(), new.last()) else {
            return;
        };
        if let Some(batch) = self.waiting.last_mut()
            && batch.expansion == expansion
            && batch.last.next() == Some(first)
        {
            batch.last = last;
        } else {
            self.waiting.push(ToolBatch {
                first,
                last,
                expansion,
            });
        }
    }

    fn confirmed(&mut self, summary: Option<&RunSummary>) -> bool {
        let (Some(attempt), Some(summary)) = (self.attempt, summary) else {
            return false;
        };
        if summary.attempt > attempt
            || (summary.attempt == attempt
                && (summary.status.is_terminal()
                    || matches!(
                        summary.status,
                        RunStatus::Interrupted | RunStatus::Evaluating
                    )))
        {
            self.waiting.clear();
            self.closed.clear();
        }
        summary.attempt == attempt && summary.status == RunStatus::Running
    }
}

enum ToolDetail<'a> {
    Command(&'a Value),
    Patch(&'a str),
    Edit(Vec<(&'a str, &'a str)>),
}

impl<'a> ToolDetail<'a> {
    fn of(call: &'a ToolCall) -> Option<Self> {
        match call.class {
            ActionClass::Command => Some(Self::Command(&call.input)),
            ActionClass::Edit => {
                if let Some(patch) = ["patch", "diff"]
                    .iter()
                    .filter_map(|key| call.input.get(key)?.as_str())
                    .find(|patch| !patch.trim().is_empty())
                {
                    return Some(Self::Patch(patch));
                }

                let edit = |input: &'a Value| {
                    let (old, new) = ["old_string", "oldText", "old_text"]
                        .into_iter()
                        .zip(["new_string", "newText", "new_text"])
                        .find_map(|(old, new)| {
                            Some((input.get(old)?.as_str()?, input.get(new)?.as_str()?))
                        })?;
                    (old != new).then_some((old, new))
                };
                let edits: Vec<_> = std::iter::once(&call.input)
                    .chain(
                        call.input
                            .get("edits")
                            .and_then(Value::as_array)
                            .into_iter()
                            .flatten(),
                    )
                    .filter_map(edit)
                    .collect();
                (!edits.is_empty()).then_some(Self::Edit(edits))
            }
            _ => None,
        }
    }

    fn body(&self) -> Cow<'_, str> {
        match self {
            Self::Command(input) => ["command", "cmd"]
                .iter()
                .find_map(|key| input.get(key)?.as_str())
                .map_or_else(|| Cow::Owned(input.to_string()), Cow::Borrowed),
            Self::Patch(patch) => Cow::Borrowed(patch),
            Self::Edit(edits) => Cow::Owned(
                edits
                    .iter()
                    .flat_map(|(old, new)| {
                        old.lines()
                            .map(|line| format!("- {line}"))
                            .chain(new.lines().map(|line| format!("+ {line}")))
                    })
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        }
    }
}

/// Puts `text` on the clipboard and says so.
pub(super) fn copy(cx: &mut gpui::App, text: String, what: &'static str) {
    cx.write_to_clipboard(ClipboardItem::new_string(text));
    toast::push(cx, toast::Tone::Info, "Copied", what);
}

impl RunScreen {
    pub(super) fn tool_key(&self, key: Key, id: &str) -> ToolKey {
        let start = key.order().map_or(key.0, |(_, start)| start);
        (self.run_of(key), start, id.to_owned())
    }

    pub(super) fn expand_new_tools(&mut self, cx: &gpui::App) {
        let state = self.store.read(cx).state();
        let Some(transcript) = state.transcript(&self.run) else {
            return;
        };
        let previously_seen = self.tool_events.through;
        self.tool_events
            .observe(transcript, Prefs::tool_expansion(cx));
        if !self.tool_events.confirmed(state.run(&self.run)) {
            return;
        }
        let waiting = std::mem::take(&mut self.tool_events.waiting);
        let closed = std::mem::take(&mut self.tool_events.closed);
        let events = &transcript.events;
        let mut rows = self.rows[self.prefix_len..].iter().rev().peekable();
        let mut remeasure = None;
        for batch in waiting.into_iter().rev() {
            let start = events.partition_point(|(seq, _)| *seq < batch.first);
            let end = events.partition_point(|(seq, _)| *seq <= batch.last);
            let (edits, commands) = batch.expansion;
            for (seq, event) in events[start..end].iter().rev() {
                let ExecEvent::Tool { id, call } = event else {
                    continue;
                };
                let eligible = match call.class {
                    ActionClass::Command => commands,
                    ActionClass::Edit => edits && ToolDetail::of(call).is_some(),
                    _ => false,
                };
                if !eligible {
                    continue;
                }
                while rows.peek().is_some_and(|row| {
                    row.key()
                        .order()
                        .is_none_or(|(_, start)| start > seq.0.saturating_add(1))
                }) {
                    rows.next();
                }
                if let Some(Row::Tools { key, calls }) = rows.peek()
                    && calls.iter().any(|entry| &entry.id == id)
                {
                    let tool = self.tool_key(*key, id);
                    if !closed.contains(&tool) && !self.outputs_open.contains(&tool) {
                        self.outputs_open.owner_open(tool);
                        if Some(*seq) <= previously_seen
                            && remeasure != Some(*key)
                            && let Some(ix) = self.items.item_of(*key)
                        {
                            self.measure_again(ix);
                            remeasure = Some(*key);
                        }
                    }
                }
            }
        }
    }

    pub(super) fn tool_inputs(&self, key: Key, calls: &[ToolEntry], cx: &App) -> ToolInputs {
        let Some((segment, start)) = key.order() else {
            return Vec::new();
        };
        let run = self.run_of(key);
        let Some(transcript) = self.store.read(cx).state().transcript(&run) else {
            return Vec::new();
        };
        let next = self.items.item_of(key).and_then(|at| {
            self.items.items[at + 1..]
                .iter()
                .find_map(|item| item.key.order())
                .filter(|(next_segment, _)| *next_segment == segment)
                .map(|(_, next)| next)
        });
        let events = &transcript.events;
        let first = events.partition_point(|(seq, _)| seq.0.saturating_add(1) < start);
        let last = next.map_or(events.len(), |next| {
            events.partition_point(|(seq, _)| seq.0.saturating_add(1) < next)
        });
        let inputs: HashMap<_, _> = events
            .get(first..last)
            .unwrap_or_default()
            .iter()
            .filter_map(|(_, event)| match event {
                ExecEvent::Tool { id, call } => Some((id.as_str(), call)),
                _ => None,
            })
            .collect();
        calls
            .iter()
            .map(|entry| {
                let detail = ToolDetail::of(inputs.get(entry.id.as_str())?)?;
                let opened = self.outputs_open.contains(&self.tool_key(key, &entry.id));
                Some(ToolInput {
                    diff: !matches!(detail, ToolDetail::Command(_)),
                    body: opened.then(|| detail.body().into_owned()),
                })
            })
            .collect()
    }

    /// Opens or closes the output of one call, or shows all of it, and measures the row again.
    /// Opening one while the view follows the tail stops following, so what grew stays in view.
    fn toggle_output(&mut self, key: Key, id: &str, full: bool, cx: &mut Context<Self>) {
        let tool = self.tool_key(key, id);
        let set = if full {
            &mut self.outputs_full
        } else {
            &mut self.outputs_open
        };
        let opening = !set.contains(&tool);
        if !full && !opening && tool.0 == self.run && !self.tool_events.waiting.is_empty() {
            self.tool_events.closed.insert(tool.clone());
        }
        let following = self.list.is_following_tail();
        set.toggle(tool);
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
        (line, range): (&convo::StepLine, Range<usize>),
        (calls, inputs): (&[ToolEntry], &[Option<ToolInput>]),
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let at = line.call.as_ref().map(|_| range.start);
        let call = at.and_then(|n| calls.get(n));
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
            .then(|| {
                let verb = line.text.split(' ').next().unwrap_or("Edited");
                convo::edit_line_with_hits(calls, range.clone(), verb, &|n| {
                    self.find_highlights(key, find::Part::Label(n))
                })
            })
            .flatten();
        let (line_text, painted, unpainted) = match edits {
            Some((text, shown)) => (text, shown, Vec::new()),
            None => (line.text.clone(), painted, unpainted),
        };
        let output = call.filter(|c| c.output.is_some());
        let detail = at.and_then(|n| inputs.get(n)?.as_ref());
        let toggle = call.filter(|_| output.is_some() || detail.is_some());
        let shown = toggle.is_some_and(|c| self.outputs_open.contains(&self.tool_key(key, &c.id)));
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
            .when_some(toggle.map(|c| c.id.clone()), |d, call_id| {
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
            .when(toggle.is_some(), |d| {
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
        let edits = (line.icon == convo::StepIcon::Edit && call.is_none())
            .then(|| range.clone())
            .into_iter()
            .flatten()
            .filter_map(|n| Some((n, calls.get(n)?, inputs.get(n)?.as_ref()?)))
            .map(|(n, call, input)| self.edit_disclosure(theme, (key, ln, n), call, input, cx))
            .collect::<Vec<_>>();
        let images = if self.store.read(cx).capabilities().tool_images {
            let run = self.run_of(key);
            calls
                .get(range)
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
            .children(edits)
            .children(images)
            .children(block)
            .children(detail.zip(call.zip(at)).and_then(|(input, (call, n))| {
                self.tool_detail_block(
                    theme,
                    key,
                    Slot::tool_input(ln, n, ToolInputPart::Body),
                    &call.id,
                    input,
                )
            }))
            .into_any_element()
    }

    fn edit_disclosure(
        &self,
        theme: &Theme,
        (key, ln, at): (Key, usize, usize),
        call: &ToolEntry,
        input: &ToolInput,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = call.id.clone();
        let label = ToolInput::file_label(call);
        let body = self.tool_detail_block(
            theme,
            key,
            Slot::tool_input(ln, at, ToolInputPart::Body),
            &id,
            input,
        );
        let label_piece = self.text_piece(
            theme,
            Spot {
                key,
                slot: Slot::tool_input(ln, at, ToolInputPart::File),
                base: 0,
                len: label.len(),
                total: Some(label.len()),
            },
            label,
            &[],
        );
        let focus = theme.accent;
        div()
            .ml(px(22.0))
            .flex()
            .flex_col()
            .gap(px(6.0))
            .child(
                super::transcript::disclosure(
                    theme,
                    SharedString::from(format!("step-{id}")),
                    body.is_some(),
                    label_piece,
                )
                .h(px(26.0))
                .px(px(10.0))
                .rounded(px(999.0))
                .border_1()
                .border_color(theme.hairline)
                .text_size(px(12.5))
                .font_weight(FontWeight::MEDIUM)
                .role(Role::Button)
                .aria_label(label.to_owned())
                .focusable()
                .tab_stop(true)
                .focus_visible(move |s| s.border_color(focus))
                .on_click(
                    cx.listener(move |this, _, _, cx| this.toggle_output(key, &id, false, cx)),
                ),
            )
            .children(body)
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
        let full = self.outputs_full.contains(&self.tool_key(key, &id));
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
    fn tool_detail_block(
        &self,
        theme: &Theme,
        key: Key,
        slot: Slot,
        id: &str,
        input: &ToolInput,
    ) -> Option<AnyElement> {
        let text = input.body.as_deref()?;
        let selector = format!("tool-detail-{id}");
        let mut base = 0;
        Some(
            div()
                .id(SharedString::from(selector.clone()))
                .debug_selector(move || selector.clone())
                .mx(px(12.0))
                .mb(px(10.0))
                .px(px(14.0))
                .py(px(9.0))
                .rounded(px(10.0))
                .bg(theme.shell)
                .font_family(MONO)
                .text_size(px(12.0))
                .line_height(px(18.0))
                .children(text.split('\n').map(|line| {
                    let tone = match (input.diff, line.as_bytes().first()) {
                        (true, Some(b'+')) => theme.success,
                        (true, Some(b'-')) => theme.danger,
                        _ => theme.text,
                    };
                    let spot = Spot {
                        key,
                        slot,
                        base,
                        len: line.len(),
                        total: Some(text.len()),
                    };
                    base += line.len() + 1;
                    div()
                        .min_h(px(18.0))
                        .text_color(tone)
                        .child(self.text_piece(theme, spot, line, &[]))
                }))
                .into_any_element(),
        )
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
