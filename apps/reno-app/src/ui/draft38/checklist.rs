//! B "Checklist": no separate screen. The new-thread state shows a "Get set up \u{b7} 2 of 4" card
//! above the composer. Each item (Appearance, Agents, First project, Notifications) expands in
//! place. The card can be put away, and it goes when everything is done. The accent is spent on
//! the progress count; done items are muted, with a check in success. Rows are Geist 13, as
//! dense as the sidebar's.

use gpui::{
    AnyElement, App, Context, Entity, FontWeight, IntoElement, Role, SharedString, div, prelude::*,
    px,
};

use super::flow::SetupFlow;
use crate::ui::icon::{Icon, icon};
use crate::ui::kit;
use crate::ui::theme::Theme;
use crate::vm::first_run::{AgentLine, AgentState, Item};

/// The card, for the new-thread state to draw above its composer.
pub(crate) fn card(flow: &Entity<SetupFlow>, theme: &Theme, cx: &mut App) -> AnyElement {
    let theme = *theme;
    flow.update(cx, |flow, cx| flow.checklist(&theme, cx))
}

impl SetupFlow {
    fn checklist(&mut self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let lines = self.lines(cx);
        let blocked = crate::vm::first_run::blocked(&lines);
        let progress = self.setup.progress(blocked);
        let all = self.setup.all_done(blocked);
        let header = div()
            .h(px(40.0))
            .px(px(14.0))
            .flex()
            .items_center()
            .gap(px(8.0))
            .child(
                div()
                    .text_size(px(13.0))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Get set up"),
            )
            .child(
                div()
                    .id("setup-progress")
                    .debug_selector(|| "setup-progress".to_owned())
                    .font_family(kit::MONO)
                    .text_size(px(12.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.accent)
                    .child(progress),
            )
            .child(div().flex_1())
            .when(all, |h| {
                h.child(
                    kit::line_button(theme, "setup-done", None, "Done", kit::Tone::Primary, true)
                        .on_click(cx.listener(|this, _, _, cx| this.finish(cx))),
                )
            })
            .child(
                kit::icon_button(theme, "setup-dismiss", Icon::PhX, 24.0)
                    .on_click(cx.listener(|this, _, _, cx| this.dismiss(cx))),
            );
        let finding = div()
            .id("setup-finding")
            .debug_selector(|| "setup-finding".to_owned())
            .px(px(14.0))
            .pb(px(8.0))
            .text_size(px(12.5))
            .line_height(px(18.0))
            .text_color(theme.muted)
            .child(self.finding(cx));
        let items = Item::ALL.map(|item| self.item(theme, item, &lines, blocked, cx));
        div()
            .id("setup-card")
            .debug_selector(|| "setup-card".to_owned())
            .w_full()
            .flex()
            .flex_col()
            .rounded(px(12.0))
            .bg(theme.surface)
            .border_1()
            .border_color(theme.hairline)
            .child(header)
            .child(finding)
            .child(div().h(px(1.0)).bg(theme.hairline))
            .children(items)
            .into_any_element()
    }

    /// One item: its row, and its body when it is open.
    fn item(
        &mut self,
        theme: &Theme,
        item: Item,
        lines: &[AgentLine],
        blocked: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let done = self.setup.is_done(item, blocked);
        let open = self.setup.open_item == Some(item);
        let key = item.title().to_lowercase().replace(' ', "-");
        let summary = match item {
            Item::Appearance => {
                if self.setup.looked {
                    let p = crate::prefs::current(cx);
                    format!("{:?} \u{b7} {:?}", p.mode, p.theme)
                } else {
                    "Pick a look".to_owned()
                }
            }
            Item::Agents => format!(
                "{} of {} ready",
                lines.iter().filter(|l| l.state.can_take_runs()).count(),
                lines.len()
            ),
            Item::Project => self
                .setup
                .project
                .clone()
                .unwrap_or_else(|| "Not chosen".to_owned()),
            Item::Notifications => {
                if self.setup.notified {
                    if crate::prefs::current(cx).notifications {
                        "On".to_owned()
                    } else {
                        "Off".to_owned()
                    }
                } else {
                    "Not answered".to_owned()
                }
            }
        };
        let hover = theme.row_selected().opacity(0.6);
        let row = div()
            .id(SharedString::from(format!("setup-item-{key}")))
            .debug_selector({
                let key = key.clone();
                move || format!("setup-item-{key}")
            })
            .h(px(34.0))
            .px(px(14.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .cursor_pointer()
            .role(Role::Button)
            .aria_label(SharedString::from(format!(
                "{}, {}",
                item.title(),
                if done { "done" } else { "to do" }
            )))
            .focusable()
            .tab_stop(true)
            .focus_visible(kit::focus_ring(theme))
            .hover(move |s| s.bg(hover))
            .on_click(cx.listener(move |this, _, _, cx| this.toggle(item, cx)))
            .child(if done {
                icon(Icon::PhCheck, 14.0, theme.success).into_any_element()
            } else {
                div()
                    .flex_none()
                    .size(px(14.0))
                    .rounded_full()
                    .border_1()
                    .border_color(theme.faint)
                    .into_any_element()
            })
            .child(
                div()
                    .flex_1()
                    .text_size(px(13.0))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(if done { theme.muted } else { theme.text })
                    .child(item.title()),
            )
            .child(
                div()
                    .text_size(px(12.5))
                    .text_color(theme.muted)
                    .child(summary),
            )
            .child(icon(
                if open { Icon::ChevUp } else { Icon::ChevDown },
                12.0,
                theme.faint,
            ));
        let body = open.then(|| {
            let content = match item {
                Item::Appearance => self.look_controls(theme, cx),
                Item::Agents => div()
                    .flex()
                    .flex_col()
                    .children(lines.iter().map(|l| self.compact_agent(theme, l, cx)))
                    .into_any_element(),
                Item::Project => self.project_rows(theme, 30.0, false, cx),
                Item::Notifications => self.notify_row(theme, cx),
            };
            div().pl(px(38.0)).pr(px(14.0)).pb(px(10.0)).child(content)
        });
        div()
            .flex()
            .flex_col()
            .child(row)
            .children(body)
            .into_any_element()
    }

    /// An agent as the checklist lists it: one 32 px line.
    fn compact_agent(&self, theme: &Theme, line: &AgentLine, cx: &mut Context<Self>) -> AnyElement {
        let tone = super::parts::state_tone(theme, &line.state);
        let id = format!("setup-agent-{}", line.id);
        let control = self.control(theme, line, false, cx);
        div()
            .id(SharedString::from(id.clone()))
            .debug_selector(move || id.clone())
            .h(px(32.0))
            .flex()
            .items_center()
            .gap(px(10.0))
            .child(kit::exec_glyph(theme, &line.id, 16.0))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_baseline()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_size(px(13.0))
                            .text_color(if line.state == AgentState::Ready {
                                theme.muted
                            } else {
                                theme.text
                            })
                            .child(line.name.clone()),
                    )
                    .children(line.version.clone().map(|v| {
                        div()
                            .font_family(kit::MONO)
                            .text_size(px(11.0))
                            .text_color(theme.faint)
                            .child(v)
                    })),
            )
            .when(!line.state.working(), |r| {
                r.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(6.0))
                        .text_size(px(12.0))
                        .text_color(tone)
                        .child(Self::state_mark(theme, &line.state))
                        .child(line.state.words()),
                )
            })
            .children(control)
            .into_any_element()
    }
}
