//! A "Wizard": first run as a full-window, centred flow of five steps, one thing per screen,
//! with progress dots and Back / Continue, and "Skip setup" at the bottom left. The accent is
//! spent on the step's primary button and the progress dots. Step titles are Geist 24 semibold,
//! body Geist 14.
//!
//! It covers everything under the status bar, so the bar, and the switch's label in it, stay.

use gpui::{
    AnyElement, App, Context, Entity, FontWeight, IntoElement, SharedString, div, prelude::*, px,
};

use super::flow::SetupFlow;
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, Tone};
use crate::ui::theme::Theme;
use crate::vm::first_run::{AgentLine, AgentState, Step};
use crate::vm::shell::BAR_HEIGHT;

/// The flow's column: wide enough for an agent's line, narrow enough to read.
const COLUMN: f32 = 560.0;
/// The column's height, so the footer does not move between steps.
const HEIGHT: f32 = 500.0;

/// The wizard, laid over the window under the status bar.
pub(crate) fn overlay(flow: &Entity<SetupFlow>, theme: &Theme, cx: &mut App) -> AnyElement {
    let theme = *theme;
    flow.update(cx, |flow, cx| flow.wizard(&theme, cx))
}

impl SetupFlow {
    fn wizard(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let step = self.setup.step;
        let lines = self.lines(cx);
        let body = match step {
            Step::Welcome => self.welcome(theme, &lines, cx),
            Step::Look => self.look_step(theme, cx),
            Step::Agents => self.agents_step(theme, &lines, cx),
            Step::Project => self.project_step(theme, cx),
            Step::Done => self.done_step(theme, &lines, cx),
        };
        let dots = div()
            .flex()
            .items_center()
            .gap(px(6.0))
            .children(Step::ALL.iter().map(|s| {
                let on = *s == step;
                let reached = s.index() <= step.index();
                div()
                    .h(px(6.0))
                    .w(px(if on { 20.0 } else { 6.0 }))
                    .rounded(px(3.0))
                    .bg(if reached {
                        theme.accent
                    } else {
                        theme.text.opacity(0.18)
                    })
            }));
        let first = step == Step::Welcome;
        let last = step == Step::Done;
        let skip = (!last).then(|| {
            kit::pill_button(theme, "setup-skip", "Skip setup", Tone::Quiet)
                .border_color(gpui::transparent_black())
                .text_color(theme.muted)
                // Flush with the column's edge, not the pill's padding.
                .ml(px(-14.0))
                .on_click(cx.listener(|this, _, _, cx| this.skip(cx)))
        });
        let back = (!first).then(|| {
            kit::pill_button(theme, "setup-back", "Back", Tone::Quiet)
                .on_click(cx.listener(|this, _, _, cx| this.back(cx)))
        });
        let go = if last {
            kit::pill_button(theme, "setup-start", "Start building", Tone::Primary)
                .on_click(cx.listener(|this, _, _, cx| this.finish(cx)))
        } else {
            let label = if first { "Get started" } else { "Continue" };
            kit::pill_button(theme, "setup-continue", label, Tone::Primary)
                .on_click(cx.listener(|this, _, _, cx| this.next(cx)))
        };
        let footer = div()
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(div().flex_1().children(skip))
            .children(back)
            .child(go);
        div()
            .id("setup-wizard")
            .debug_selector(|| "setup-wizard".to_owned())
            .absolute()
            .top(px(BAR_HEIGHT))
            .left_0()
            .right_0()
            .bottom_0()
            .occlude()
            .bg(theme.shell)
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(COLUMN))
                    .max_w_full()
                    .h(px(HEIGHT))
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap(px(24.0))
                    .child(dots)
                    .child(div().flex_1().min_h_0().w_full().child(body))
                    .child(footer),
            )
            .into_any_element()
    }

    fn heading(theme: &Theme, title: &'static str, lead: impl Into<SharedString>) -> gpui::Div {
        div()
            .flex()
            .flex_col()
            .gap(px(8.0))
            .child(
                div()
                    .text_size(px(24.0))
                    .line_height(px(30.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(title),
            )
            .child(
                div()
                    .text_size(px(14.0))
                    .line_height(px(21.0))
                    .text_color(theme.muted)
                    .child(lead.into()),
            )
    }

    /// One agent, roomy: its mark, name and version, its state in words, and the job's control.
    fn agent_card(&self, theme: &Theme, line: &AgentLine, cx: &mut Context<Self>) -> AnyElement {
        let tone = super::parts::state_tone(theme, &line.state);
        let id = format!("setup-agent-{}", line.id);
        let control = self.control(theme, line, false, cx);
        let ready = line.state == AgentState::Ready;
        div()
            .id(SharedString::from(id.clone()))
            .debug_selector(move || id.clone())
            .h(px(46.0))
            .w_full()
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(12.0))
            .rounded(px(10.0))
            .bg(theme.surface)
            .border_1()
            .border_color(theme.hairline)
            .child(kit::exec_glyph(theme, &line.id, 20.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_baseline()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_size(px(14.0))
                            .font_weight(FontWeight::MEDIUM)
                            .child(line.name.clone()),
                    )
                    .children(line.version.clone().map(|v| {
                        div()
                            .font_family(kit::MONO)
                            .text_size(px(11.5))
                            .text_color(theme.faint)
                            .child(v)
                    })),
            )
            .when(control.is_none() || ready, |r| {
                r.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .text_size(px(12.5))
                        .text_color(tone)
                        .child(Self::state_mark(theme, &line.state))
                        .child(line.state.words()),
                )
            })
            .when(!ready, |r| {
                r.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(10.0))
                        .when(control.is_some() && !line.state.working(), |d| {
                            d.child(
                                div()
                                    .text_size(px(12.5))
                                    .text_color(tone)
                                    .child(line.state.words()),
                            )
                        })
                        .children(control),
                )
            })
            .into_any_element()
    }

    /// Step 1: the finding, and the agents that block it with their controls.
    fn welcome(
        &mut self,
        theme: &Theme,
        lines: &[AgentLine],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let blocked: Vec<&AgentLine> = lines.iter().filter(|l| !l.state.can_take_runs()).collect();
        div()
            .flex()
            .flex_col()
            .gap(px(20.0))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(12.0))
                    .child(icon(Icon::Mark, 40.0, theme.text))
                    .child(Self::heading(
                        theme,
                        "Welcome to Reno",
                        "Reno runs coding agents on this Mac and keeps their work in threads you can read.",
                    )),
            )
            .child(
                div()
                    .id("setup-finding")
                    .debug_selector(|| "setup-finding".to_owned())
                    .text_size(px(15.0))
                    .line_height(px(22.0))
                    .font_weight(FontWeight::MEDIUM)
                    .child(self.finding(cx)),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.0))
                    .children(blocked.into_iter().map(|l| self.agent_card(theme, l, cx))),
            )
            .into_any_element()
    }

    /// Step 2: the look, kept as it is chosen.
    fn look_step(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(px(24.0))
            .child(Self::heading(
                theme,
                "How should Reno look?",
                "It changes as you choose. You can change it later in Settings \u{203a} Appearance.",
            ))
            .child(self.look_controls(theme, cx))
            .into_any_element()
    }

    /// Step 3: every agent, with its state and the job's control.
    fn agents_step(
        &mut self,
        theme: &Theme,
        lines: &[AgentLine],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(Self::heading(theme, "Agents on this Mac", self.finding(cx)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(6.0))
                    .children(lines.iter().map(|l| self.agent_card(theme, l, cx))),
            )
            .into_any_element()
    }

    /// Step 4: the first project, brought in from this Mac.
    fn project_step(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        use super::flow::CloneUi;
        let can_import = self.can_import(cx);
        let known = !self.projects(cx).is_empty();
        let buttons = can_import.then(|| {
            div()
                .flex()
                .items_center()
                .gap(px(8.0))
                .child(
                    kit::pill_button(
                        theme,
                        "setup-import",
                        "Import from this Mac\u{2026}",
                        Tone::Primary,
                    )
                    .on_click(cx.listener(|this, _, _, cx| this.import_from_mac(cx))),
                )
                .child(
                    kit::pill_button(theme, "setup-clone", "Clone from URL\u{2026}", Tone::Quiet)
                        .on_click(cx.listener(|this, _, window, cx| this.open_clone(window, cx))),
                )
        });
        let clone = self.clone.clone().map(|state| {
            let note: Option<(SharedString, gpui::Hsla)> = match &state {
                CloneUi::Typing => None,
                CloneUi::Cloning => Some(("Cloning\u{2026}".into(), theme.muted)),
                CloneUi::Failed(why) => Some((why.clone().into(), theme.danger)),
            };
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(8.0))
                        .child(div().w(px(340.0)).child(self.url.clone()))
                        .child(
                            kit::pill_button_enabled(
                                theme,
                                "setup-clone-go",
                                "Clone",
                                Tone::Quiet,
                                state != CloneUi::Cloning,
                            )
                            .on_click(cx.listener(|this, _, _, cx| this.start_clone(cx))),
                        ),
                )
                .children(note.map(|(text, tone)| {
                    div()
                        .id("setup-clone-note")
                        .debug_selector(|| "setup-clone-note".to_owned())
                        .text_size(px(12.5))
                        .text_color(tone)
                        .child(text)
                }))
        });
        let error = self.import_error.clone().map(|why| {
            div()
                .id("setup-import-error")
                .debug_selector(|| "setup-import-error".to_owned())
                .text_size(px(13.0))
                .text_color(theme.danger)
                .child(why)
        });
        let found = self.found(cx).map(|words| {
            div()
                .id("setup-found")
                .debug_selector(|| "setup-found".to_owned())
                .px(px(12.0))
                .py(px(9.0))
                .rounded(px(9.0))
                .bg(theme.surface)
                .border_1()
                .border_color(theme.hairline)
                .text_size(px(13.0))
                .child(words)
        });
        let list = known.then(|| {
            div()
                .flex()
                .flex_col()
                .gap(px(6.0))
                .child(
                    div()
                        .text_size(px(12.5))
                        .text_color(theme.muted)
                        .child("Or pick one Reno already knows"),
                )
                .child(self.project_rows(theme, 36.0, false, cx))
        });
        div()
            .flex()
            .flex_col()
            .gap(px(16.0))
            .child(Self::heading(
                theme,
                "Bring in your first project",
                "Pick a folder on this Mac. Your first thread starts there.",
            ))
            .children(buttons)
            .children(clone)
            .children(error)
            .children(found)
            .children(list)
            .into_any_element()
    }

    /// Step 5: what was set, and the way out.
    fn done_step(
        &mut self,
        theme: &Theme,
        lines: &[AgentLine],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ready = lines.iter().filter(|l| l.state.can_take_runs()).count();
        let p = crate::prefs::current(cx);
        let row = |label: &'static str, value: String| {
            div()
                .h(px(40.0))
                .px(px(14.0))
                .flex()
                .items_center()
                .gap(px(12.0))
                .child(
                    div()
                        .w(px(110.0))
                        .text_size(px(13.0))
                        .text_color(theme.muted)
                        .child(label),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(px(13.5))
                        .child(value),
                )
        };
        let mode = format!("{:?} \u{b7} {:?} \u{b7} {:?}", p.mode, p.theme, p.accent);
        let project = self
            .setup
            .project
            .clone()
            .unwrap_or_else(|| "none yet; pick one from the sidebar".to_owned());
        div()
            .flex()
            .flex_col()
            .gap(px(20.0))
            .child(Self::heading(
                theme,
                "You're all set",
                "Setup is done and will not show again. Everything here is in Settings.",
            ))
            .child(
                div()
                    .id("setup-summary")
                    .debug_selector(|| "setup-summary".to_owned())
                    .flex()
                    .flex_col()
                    .rounded(px(10.0))
                    .bg(theme.surface)
                    .border_1()
                    .border_color(theme.hairline)
                    .child(row("Look", mode))
                    .child(row(
                        "Agents",
                        format!("{ready} of {} can take runs", lines.len()),
                    ))
                    .child(row("First project", project)),
            )
            .into_any_element()
    }
}
