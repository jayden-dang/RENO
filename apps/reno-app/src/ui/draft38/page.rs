//! C "One page": everything about this Mac on one scrolling setup page: Look, Agents (a live
//! table of agent, version, state and action), Project and Notifications, stacked. A sticky
//! footer reads "Start building" next to the finding sentence. Neutral colour: each state is a
//! semantic tag, and the accent is spent only on "Start building". Section heads are Geist 15
//! semibold; details are Geist Mono 12, so versions and paths line up.

use gpui::{
    AnyElement, App, Context, Entity, FontWeight, IntoElement, SharedString, div, prelude::*, px,
};

use super::flow::SetupFlow;
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, Tone};
use crate::ui::theme::Theme;
use crate::vm::first_run::AgentLine;

/// The page, for the canvas to draw in place of the screen.
pub(crate) fn page(flow: &Entity<SetupFlow>, theme: &Theme, cx: &mut App) -> AnyElement {
    let theme = *theme;
    flow.update(cx, |flow, cx| flow.page(&theme, cx))
}

/// Column widths of the agents table.
const VERSION_W: f32 = 84.0;
const STATE_W: f32 = 118.0;

impl SetupFlow {
    fn section(
        theme: &Theme,
        key: &'static str,
        title: &'static str,
        detail: String,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(SharedString::from(format!("setup-section-{key}")))
            .debug_selector(move || format!("setup-section-{key}"))
            .flex()
            .flex_col()
            .gap(px(12.0))
            .child(
                div()
                    .flex()
                    .items_baseline()
                    .gap(px(10.0))
                    .child(
                        div()
                            .text_size(px(15.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(title),
                    )
                    .child(
                        div()
                            .font_family(kit::MONO)
                            .text_size(px(12.0))
                            .text_color(theme.faint)
                            .child(detail),
                    ),
            )
    }

    fn page(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let lines = self.lines(cx);
        let finding = self.finding(cx);
        let p = crate::prefs::current(cx);
        let look = Self::section(
            theme,
            "look",
            "Look",
            format!("{:?} \u{b7} {:?} \u{b7} {:?}", p.mode, p.theme, p.accent).to_lowercase(),
        )
        .child(self.look_controls(theme, cx));
        let ready = lines.iter().filter(|l| l.state.can_take_runs()).count();
        let agents = Self::section(
            theme,
            "agents",
            "Agents",
            format!("{ready} of {} can take runs", lines.len()),
        )
        .child(self.agent_table(theme, &lines, cx));
        let project = Self::section(
            theme,
            "project",
            "Project",
            self.setup
                .project
                .clone()
                .unwrap_or_else(|| "none chosen".to_owned()),
        )
        .child(self.project_rows(theme, 34.0, true, cx));
        let notifications = Self::section(
            theme,
            "notifications",
            "Notifications",
            if p.notifications { "on" } else { "off" }.to_owned(),
        )
        .child(self.notify_row(theme, cx));
        let header = div()
            .flex()
            .items_center()
            .gap(px(12.0))
            .child(icon(Icon::Mark, 28.0, theme.text))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .text_size(px(20.0))
                            .line_height(px(26.0))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("Set up this Mac"),
                    )
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(theme.muted)
                            .child("Everything Reno needs to start, on one page. Skip any of it."),
                    ),
            );
        let footer = div()
            .id("setup-footer")
            .debug_selector(|| "setup-footer".to_owned())
            .flex_none()
            .h(px(64.0))
            .px(px(32.0))
            .flex()
            .items_center()
            .gap(px(16.0))
            .border_t_1()
            .border_color(theme.hairline)
            .bg(theme.canvas)
            .child(
                div()
                    .id("setup-finding")
                    .debug_selector(|| "setup-finding".to_owned())
                    .flex_1()
                    .min_w_0()
                    .text_size(px(12.5))
                    .line_height(px(18.0))
                    .text_color(theme.muted)
                    .child(finding),
            )
            .child(
                kit::pill_button(theme, "setup-skip", "Skip setup", Tone::Quiet)
                    .border_color(gpui::transparent_black())
                    .text_color(theme.muted)
                    .on_click(cx.listener(|this, _, _, cx| this.skip(cx))),
            )
            .child(
                kit::pill_button(theme, "setup-start", "Start building", Tone::Primary)
                    .on_click(cx.listener(|this, _, _, cx| this.finish(cx))),
            );
        div()
            .id("setup-page")
            .debug_selector(|| "setup-page".to_owned())
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .id("setup-scroll")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll)
                    .child(
                        div()
                            .w_full()
                            .max_w(px(760.0))
                            .mx_auto()
                            .px(px(32.0))
                            .pt(px(28.0))
                            .pb(px(32.0))
                            .flex()
                            .flex_col()
                            .gap(px(30.0))
                            .child(header)
                            .child(look)
                            .child(agents)
                            .child(project)
                            .child(notifications),
                    ),
            )
            .child(footer)
            .into_any_element()
    }

    /// The agents, as a live table: agent, version, state and the job's control.
    fn agent_table(
        &self,
        theme: &Theme,
        lines: &[AgentLine],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let head = |label: &'static str, width: Option<f32>| {
            div()
                .when_some(width, |d, w| d.w(px(w)).flex_none())
                .when(width.is_none(), |d| d.flex_1())
                .font_family(kit::MONO)
                .text_size(px(11.0))
                .text_color(theme.faint)
                .child(label)
        };
        let header = div()
            .h(px(28.0))
            .px(px(12.0))
            .flex()
            .items_center()
            .gap(px(12.0))
            .child(head("AGENT", None))
            .child(head("VERSION", Some(VERSION_W)))
            .child(head("STATE", Some(STATE_W)))
            .child(div().w(px(92.0)).flex_none());
        let rows: Vec<AnyElement> = lines
            .iter()
            .map(|line| {
                let tone = super::parts::state_tone(theme, &line.state);
                let id = format!("setup-agent-{}", line.id);
                let control = self.control(theme, line, false, cx);
                div()
                    .id(SharedString::from(id.clone()))
                    .debug_selector(move || id.clone())
                    .h(px(38.0))
                    .px(px(12.0))
                    .flex()
                    .items_center()
                    .gap(px(12.0))
                    .border_t_1()
                    .border_color(theme.hairline)
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .items_center()
                            .gap(px(10.0))
                            .child(kit::exec_glyph(theme, &line.id, 16.0))
                            .child(
                                div()
                                    .text_size(px(13.0))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(line.name.clone()),
                            ),
                    )
                    .child(
                        div()
                            .w(px(VERSION_W))
                            .flex_none()
                            .font_family(kit::MONO)
                            .text_size(px(12.0))
                            .text_color(theme.muted)
                            .child(
                                line.version
                                    .clone()
                                    .unwrap_or_else(|| "\u{2014}".to_owned()),
                            ),
                    )
                    .child(
                        div()
                            .w(px(STATE_W))
                            .flex_none()
                            .flex()
                            .child(kit::tag(tone, line.state.words())),
                    )
                    .child(
                        div()
                            .w(px(92.0))
                            .flex_none()
                            .flex()
                            .justify_end()
                            .children(control),
                    )
                    .into_any_element()
            })
            .collect();
        div()
            .id("setup-agents")
            .debug_selector(|| "setup-agents".to_owned())
            .flex()
            .flex_col()
            .rounded(px(10.0))
            .bg(theme.surface)
            .border_1()
            .border_color(theme.hairline)
            .overflow_hidden()
            .child(header)
            .children(rows)
            .into_any_element()
    }
}
