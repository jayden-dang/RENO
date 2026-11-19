//! ROAD-35 draft (throwaway): the three variants' views for following a run. Everything reads the
//! mocked `DraftActivity` through `crate::draft35::Draft35`; nothing here touches the store's
//! data or the protocol.
//!
//! A Trays: a plan tray and a background tray over the composer, subagent chips in the transcript.
//! B Session cockpit: the Session tab's Plan / Running / Agents sections and a bar count chip.
//! D Trays + cockpit: A and B together (the default).

use gpui::{AnyElement, Context, FontWeight, Role, SharedString, Window, div, prelude::*, px};
use reno_client::fake::draft35::{
    AgentKind, AgentLane, AgentState, BackgroundTask, DraftActivity, LineRole, PlanItem,
    PlanStatus, TaskState,
};
use reno_proto::RunId;

use super::RunScreen;
use crate::draft35::{Draft35, Variant};
use crate::panel::surface::Surface;
use crate::transcript::Row;
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO, Tone};
use crate::ui::theme::Theme;

/// What one frame of the draft needs, copied out of the entity.
struct Snap {
    variant: Variant,
    a: DraftActivity,
    plan_open: bool,
    tasks_open: bool,
    output_of: Option<&'static str>,
}

fn snap(cx: &gpui::App) -> Option<Snap> {
    let draft = Draft35::get(cx)?;
    let d = draft.read(cx);
    (!d.activity.is_empty()).then(|| Snap {
        variant: d.variant,
        a: d.activity.clone(),
        plan_open: d.plan_open,
        tasks_open: d.tasks_open,
        output_of: d.output_of,
    })
}

fn edit(cx: &mut gpui::App, f: impl FnOnce(&mut Draft35)) {
    if let Some(draft) = Draft35::get(cx) {
        draft.update(cx, |d, cx| {
            f(d);
            cx.notify();
        });
    }
}

/// The run id behind a draft agent's side-panel tab.
fn agent_run(id: &str) -> Option<RunId> {
    RunId::from_suffix(&format!("d35{id}")).ok()
}

fn count_label(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

// --- shared pieces ----------------------------------------------------------------------------

fn plan_marker(theme: &Theme, status: PlanStatus) -> AnyElement {
    let slot = div()
        .flex_none()
        .size(px(16.0))
        .flex()
        .items_center()
        .justify_center();
    match status {
        PlanStatus::Done => slot
            .child(icon(Icon::Check, 13.0, theme.faint))
            .into_any_element(),
        PlanStatus::InProgress => slot
            .child(kit::spinner("draft35-plan-spin", 13.0, theme.accent))
            .into_any_element(),
        PlanStatus::Pending => slot
            .child(
                div()
                    .size(px(10.0))
                    .rounded(px(5.0))
                    .border_1()
                    .border_color(theme.faint),
            )
            .into_any_element(),
    }
}

fn plan_row(theme: &Theme, item: &PlanItem, size: f32, pad: f32, band: bool) -> AnyElement {
    let (ink, weight) = match item.status {
        PlanStatus::Done => (theme.faint, FontWeight::NORMAL),
        PlanStatus::InProgress => (theme.accent, FontWeight::MEDIUM),
        PlanStatus::Pending => (theme.muted, FontWeight::NORMAL),
    };
    div()
        .flex()
        .items_start()
        .gap(px(10.0))
        .px(px(pad))
        .py(px(pad / 2.0 + 2.0))
        .rounded(px(7.0))
        .when(band && item.status == PlanStatus::InProgress, |d| {
            d.bg(theme.accent.opacity(0.10))
        })
        .text_size(px(size))
        .line_height(px(size + 6.0))
        .font_weight(weight)
        .text_color(ink)
        .child(div().pt(px(2.0)).child(plan_marker(theme, item.status)))
        .child(
            div()
                .flex_1()
                .min_w_0()
                .child(SharedString::from(item.text.clone())),
        )
        .into_any_element()
}

fn stripe_color(theme: &Theme, state: AgentState) -> gpui::Hsla {
    match state {
        AgentState::Running => theme.accent,
        AgentState::Finished => theme.faint.opacity(0.7),
    }
}

fn state_word(lane: &AgentLane) -> &'static str {
    match (lane.kind, lane.state) {
        (_, AgentState::Running) => "Running",
        (AgentKind::Subagent, AgentState::Finished) => "Finished",
        (AgentKind::SideChat, AgentState::Finished) => "Answered",
    }
}

impl RunScreen {
    fn open_agent(&mut self, id: &'static str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(run) = agent_run(id) {
            self.open_surface(Surface::SideChat(run), window, cx);
        }
    }

    /// One background task. `lines`: how many output lines show under it (0: just the last).
    fn task_row(
        &self,
        theme: &Theme,
        task: &BackgroundTask,
        lines: usize,
        size: f32,
        extra: Option<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = task.id;
        let live = task.state != TaskState::Stopped;
        let uptime = if live {
            task.uptime_label()
        } else {
            format!("ran {}", task.uptime_label())
        };
        let stop: AnyElement = match task.state {
            TaskState::Running => kit::line_button(
                theme,
                format!("draft35-stop-{id}"),
                Some(Icon::Stop),
                "Stop",
                Tone::Quiet,
                true,
            )
            .on_click(cx.listener(move |_, _, _, cx| {
                edit(cx, |d| d.activity.stop_task(id));
            }))
            .into_any_element(),
            TaskState::Stopping => kit::line_button(
                theme,
                format!("draft35-stopping-{id}"),
                None,
                "Stopping\u{2026}",
                Tone::Quiet,
                false,
            )
            .into_any_element(),
            TaskState::Stopped => kit::tag(theme.muted, "Stopped").into_any_element(),
        };
        let shown: Vec<&String> = if lines == 0 {
            task.tail.last().into_iter().collect()
        } else {
            task.tail.iter().rev().take(lines).rev().collect()
        };
        div()
            .id(SharedString::from(format!("draft35-task-{id}")))
            .debug_selector(move || format!("draft35-task-{id}"))
            .flex()
            .flex_col()
            .gap(px(5.0))
            .py(px(7.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(kit::dot(if live { theme.accent } else { theme.faint }, 7.0))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .font_family(MONO)
                            .text_size(px(size))
                            .font_weight(FontWeight::MEDIUM)
                            .child(SharedString::from(task.command.clone())),
                    )
                    .children(
                        task.port
                            .map(|p| kit::tag(theme.muted, format!(":{p}")).whitespace_nowrap()),
                    )
                    .child(div().flex_1())
                    .child(
                        div()
                            .flex_none()
                            .whitespace_nowrap()
                            .font_family(MONO)
                            .text_size(px(12.0))
                            .text_color(theme.muted)
                            .child(SharedString::from(uptime)),
                    )
                    .children(extra)
                    .child(stop),
            )
            .child(
                div()
                    .ml(px(15.0))
                    .flex()
                    .flex_col()
                    .when(lines > 0, |d| {
                        d.p(px(8.0))
                            .rounded(px(7.0))
                            .bg(theme.surface)
                            .border_1()
                            .border_color(theme.hairline)
                    })
                    .font_family(MONO)
                    .text_size(px(12.0))
                    .text_color(theme.muted)
                    .children(shown.into_iter().map(|l| {
                        div()
                            .truncate()
                            .whitespace_nowrap()
                            .child(SharedString::from(l.clone()))
                    })),
            )
            .into_any_element()
    }

    /// One subagent or side-chat lane (the All runs lane look): a status stripe, the name, the
    /// state and a one-line tail; `bar` adds a progress bar. Click opens its transcript tab.
    fn agent_lane(
        &self,
        theme: &Theme,
        lane: &AgentLane,
        size: f32,
        bar: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = lane.id;
        let stripe = stripe_color(theme, lane.state);
        let hover = theme.raised.opacity(0.4);
        let running = lane.state == AgentState::Running;
        let tail = lane
            .finding
            .clone()
            .unwrap_or_else(|| lane.last_line().to_owned());
        div()
            .id(SharedString::from(format!("draft35-lane-{id}")))
            .debug_selector(move || format!("draft35-lane-{id}"))
            .flex()
            .items_stretch()
            .rounded(px(7.0))
            .bg(theme.surface)
            .border_1()
            .border_color(theme.hairline)
            .overflow_hidden()
            .cursor_pointer()
            .role(Role::Button)
            .aria_label(SharedString::from(format!(
                "{}, {}",
                lane.name,
                state_word(lane)
            )))
            .focusable()
            .tab_stop(true)
            .focus_visible(kit::focus_ring(theme))
            .hover(move |s| s.bg(hover))
            .on_click(cx.listener(move |this, _, window, cx| this.open_agent(id, window, cx)))
            .child(div().w(px(3.0)).flex_none().bg(stripe))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .flex_col()
                    .gap(px(4.0))
                    .px(px(10.0))
                    .py(px(7.0))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.0))
                            .child(icon(
                                if lane.kind == AgentKind::SideChat {
                                    Icon::Msg
                                } else {
                                    Icon::Bot
                                },
                                14.0,
                                theme.muted,
                            ))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(px(size))
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(SharedString::from(lane.name.clone())),
                            )
                            .when(lane.kind == AgentKind::Subagent, |d| {
                                d.child(
                                    div()
                                        .flex_none()
                                        .whitespace_nowrap()
                                        .font_family(MONO)
                                        .text_size(px(12.0))
                                        .text_color(theme.muted)
                                        .child(SharedString::from(count_label(
                                            lane.steps(),
                                            "step",
                                            "steps",
                                        ))),
                                )
                            })
                            .child(
                                div()
                                    .flex_none()
                                    .w(px(62.0))
                                    .whitespace_nowrap()
                                    .text_size(px(12.5))
                                    .text_color(if running { theme.accent } else { theme.muted })
                                    .child(state_word(lane)),
                            ),
                    )
                    .when(bar && lane.kind == AgentKind::Subagent, |d| {
                        d.child(
                            div()
                                .h(px(4.0))
                                .w_full()
                                .rounded(px(2.0))
                                .bg(theme.raised)
                                .child(
                                    div()
                                        .h_full()
                                        .w(gpui::relative(lane.progress()))
                                        .rounded(px(2.0))
                                        .bg(stripe),
                                ),
                        )
                    })
                    .child(
                        div()
                            .truncate()
                            .whitespace_nowrap()
                            .font_family(MONO)
                            .text_size(px(12.0))
                            .text_color(theme.muted)
                            .child(SharedString::from(tail)),
                    ),
            )
            .into_any_element()
    }

    fn finding_line(&self, theme: &Theme, a: &DraftActivity, size: f32) -> Option<AnyElement> {
        a.finding().map(|text| {
            div()
                .id("draft35-finding")
                .debug_selector(|| "draft35-finding".to_owned())
                .w_full()
                .text_size(px(size))
                .line_height(px(size + 6.0))
                .text_color(theme.muted)
                .child(SharedString::from(text))
                .into_any_element()
        })
    }

    // --- A: trays -------------------------------------------------------------------------

    /// The plan tray and the background chip (and its tray), over the composer.
    pub(super) fn draft_trays(&mut self, theme: &Theme, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(s) = snap(cx).filter(|s| matches!(s.variant, Variant::A | Variant::D)) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if !s.a.plan.is_empty() {
            let current = s.a.current().map_or("All steps done", |i| i.doing.as_str());
            let header = div()
                .id("draft35-plan-tray")
                .debug_selector(|| "draft35-plan-tray".to_owned())
                .flex()
                .items_center()
                .gap(px(8.0))
                .h(px(30.0))
                .cursor_pointer()
                .role(Role::Button)
                .aria_label("Plan tray")
                .focusable()
                .tab_stop(true)
                .focus_visible(kit::focus_ring(theme))
                .on_click(cx.listener(|_, _, _, cx| edit(cx, |d| d.plan_open = !d.plan_open)))
                .child(icon(Icon::List, 14.0, theme.muted))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .gap(px(6.0))
                        .whitespace_nowrap()
                        .text_size(px(13.0))
                        .child(SharedString::from(format!(
                            "Plan \u{b7} {}/{} \u{b7}",
                            s.a.plan_done(),
                            s.a.plan.len()
                        )))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(theme.accent)
                                .child(SharedString::from(current.to_owned())),
                        ),
                )
                .child(icon(
                    if s.plan_open {
                        Icon::ChevUp
                    } else {
                        Icon::ChevDown
                    },
                    14.0,
                    theme.muted,
                ));
            out.push(
                div()
                    .flex()
                    .flex_col()
                    .px(px(14.0))
                    .py(px(4.0))
                    .rounded(px(12.0))
                    .bg(theme.surface)
                    .border_1()
                    .border_color(theme.hairline)
                    .child(header)
                    .children(
                        self.finding_line(theme, &s.a, 12.5)
                            .map(|f| div().pb(px(6.0)).child(f)),
                    )
                    .when(s.plan_open, |d| {
                        d.child(div().h(px(1.0)).bg(theme.hairline).mb(px(4.0)))
                            .children(
                                s.a.plan
                                    .iter()
                                    .map(|i| plan_row(theme, i, 13.0, 4.0, false)),
                            )
                    })
                    .into_any_element(),
            );
        }
        if !s.a.tasks.is_empty() {
            let running = s.a.running_tasks();
            let (dot, label) = if running > 0 {
                (theme.accent, format!("{running} running"))
            } else {
                (theme.faint, "none running".to_owned())
            };
            let chip = kit::chip(theme, "")
                .id("draft35-bg-chip")
                .debug_selector(|| "draft35-bg-chip".to_owned())
                .cursor_pointer()
                .role(Role::Button)
                .aria_label("Background tasks")
                .focusable()
                .tab_stop(true)
                .focus_visible(kit::focus_ring(theme))
                .when(s.tasks_open, |c| c.border_color(theme.accent.opacity(0.6)))
                .on_click(cx.listener(|_, _, _, cx| edit(cx, |d| d.tasks_open = !d.tasks_open)))
                .child(kit::dot(dot, 7.0))
                .child(SharedString::from(label))
                .child(icon(
                    if s.tasks_open {
                        Icon::ChevUp
                    } else {
                        Icon::ChevDown
                    },
                    13.0,
                    theme.muted,
                ));
            // "Output ›": the Session tab has the full output; the composer keeps the keyboard.
            let links: Vec<AnyElement> =
                s.a.tasks
                    .iter()
                    .map(|t| {
                        let id = t.id;
                        div()
                            .id(SharedString::from(format!("draft35-open-output-{id}")))
                            .debug_selector(move || format!("draft35-open-output-{id}"))
                            .flex_none()
                            .whitespace_nowrap()
                            .text_size(px(12.5))
                            .text_color(theme.muted)
                            .cursor_pointer()
                            .role(Role::Button)
                            .aria_label("Open output in Session")
                            .focusable()
                            .tab_stop(true)
                            .focus_visible(kit::focus_ring(theme))
                            .hover(|s| s.text_color(theme.text))
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_surface(Surface::Session, window, cx);
                            }))
                            .child("Output \u{203a}")
                            .into_any_element()
                    })
                    .collect();
            if s.tasks_open {
                out.push(
                    div()
                        .id("draft35-bg-tray")
                        .debug_selector(|| "draft35-bg-tray".to_owned())
                        .flex()
                        .flex_col()
                        .px(px(14.0))
                        .py(px(4.0))
                        .rounded(px(12.0))
                        .bg(theme.surface)
                        .border_1()
                        .border_color(theme.hairline)
                        .children(
                            s.a.tasks
                                .iter()
                                .zip(links)
                                .enumerate()
                                .map(|(n, (t, link))| {
                                    div()
                                        .when(n > 0, |d| {
                                            d.border_t_1().border_color(theme.hairline)
                                        })
                                        .child(self.task_row(theme, t, 0, 12.5, Some(link), cx))
                                }),
                        )
                        .into_any_element(),
                );
            }
            out.push(div().flex().justify_end().child(chip).into_any_element());
        }
        out
    }

    /// Chips under the agent text rows where subagents (and the side chat) were spawned.
    pub(super) fn draft_chips(
        &self,
        theme: &Theme,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let s = snap(cx).filter(|s| matches!(s.variant, Variant::A | Variant::D))?;
        let prose_at = self.rows[..=ix.min(self.rows.len().saturating_sub(1))]
            .iter()
            .filter(|r| matches!(r, Row::Prose { .. }))
            .count()
            .checked_sub(1)
            .filter(|_| matches!(self.rows.get(ix), Some(Row::Prose { .. })))?;
        let chips: Vec<AnyElement> =
            s.a.agents
                .iter()
                .filter(|a| a.after_prose == prose_at)
                .map(|lane| {
                    let id = lane.id;
                    let running = lane.state == AgentState::Running;
                    div()
                        .id(SharedString::from(format!("draft35-chip-{id}")))
                        .debug_selector(move || format!("draft35-chip-{id}"))
                        .flex()
                        .items_center()
                        .gap(px(7.0))
                        .h(px(28.0))
                        .px(px(11.0))
                        .rounded(px(999.0))
                        .bg(theme.surface)
                        .border_1()
                        .border_color(theme.hairline)
                        .text_size(px(13.0))
                        .text_color(theme.text)
                        .cursor_pointer()
                        .role(Role::Button)
                        .aria_label(SharedString::from(lane.chip_label()))
                        .focusable()
                        .tab_stop(true)
                        .focus_visible(kit::focus_ring(theme))
                        .hover(|s| s.border_color(theme.accent.opacity(0.5)))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.open_agent(id, window, cx);
                        }))
                        .child(div().text_color(theme.muted).child("\u{21b3}"))
                        .child(kit::dot(stripe_color(theme, lane.state), 7.0))
                        .child(SharedString::from(lane.chip_label()))
                        .when(running, |d| d)
                        .into_any_element()
                })
                .collect();
        (!chips.is_empty()).then(|| {
            div()
                .pt(px(10.0))
                .flex()
                .flex_wrap()
                .gap(px(8.0))
                .children(chips)
                .into_any_element()
        })
    }

    // --- B: the Session tab's cockpit -------------------------------------------------------

    pub(super) fn draft_cockpit(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let s = snap(cx).filter(|s| matches!(s.variant, Variant::B | Variant::D))?;
        let heading = |text: &str, count: Option<String>| {
            div()
                .pt(px(14.0))
                .pb(px(4.0))
                .flex()
                .items_baseline()
                .gap(px(8.0))
                .text_size(px(12.0))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.faint)
                .child(SharedString::from(text.to_owned()))
                .children(count.map(|c| {
                    div()
                        .font_family(MONO)
                        .font_weight(FontWeight::NORMAL)
                        .child(SharedString::from(c))
                }))
        };
        let mut body = div()
            .id("draft35-cockpit")
            .debug_selector(|| "draft35-cockpit".to_owned())
            .flex()
            .flex_col()
            .pb(px(6.0))
            .border_b_1()
            .border_color(theme.hairline)
            .children(
                self.finding_line(theme, &s.a, 12.5)
                    .filter(|_| s.variant == Variant::B),
            );
        if !s.a.plan.is_empty() {
            body = body
                .child(heading(
                    "Plan",
                    Some(format!("{}/{}", s.a.plan_done(), s.a.plan.len())),
                ))
                .children(s.a.plan.iter().map(|i| plan_row(theme, i, 12.5, 6.0, true)));
        }
        if !s.a.tasks.is_empty() {
            body = body.child(heading("Running", Some(format!("{}", s.a.running_tasks()))));
            for (n, t) in s.a.tasks.iter().enumerate() {
                let id = t.id;
                let full = s.output_of == Some(id);
                let toggle = kit::line_button(
                    theme,
                    format!("draft35-output-{id}"),
                    Some(Icon::Terminal),
                    if full { "Hide output" } else { "Output" },
                    Tone::Quiet,
                    true,
                )
                .on_click(cx.listener(move |_, _, _, cx| {
                    edit(cx, |d| {
                        d.output_of = if d.output_of == Some(id) {
                            None
                        } else {
                            Some(id)
                        };
                    });
                }));
                body = body.child(
                    div()
                        .when(n > 0, |d| d.border_t_1().border_color(theme.hairline))
                        .child(self.task_row(
                            theme,
                            t,
                            if full { 12 } else { 2 },
                            12.0,
                            Some(toggle.into_any_element()),
                            cx,
                        )),
                );
            }
        }
        if !s.a.agents.is_empty() {
            body = body
                .child(heading("Agents", Some(format!("{}", s.a.agents.len()))))
                .child(
                    div().flex().flex_col().gap(px(6.0)).pb(px(6.0)).children(
                        s.a.agents
                            .iter()
                            .map(|l| self.agent_lane(theme, l, 12.5, false, cx)),
                    ),
                );
        }
        Some(body.into_any_element())
    }

    // --- the subagent / side-chat tab ---------------------------------------------------------

    /// The body of a draft agent's tab: its short transcript, read-only.
    pub(super) fn draft_agent_body(
        &self,
        theme: &Theme,
        run: &RunId,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let id = run.as_str().strip_prefix("run_")?.strip_prefix("d35")?;
        let draft = Draft35::get(cx)?;
        let lane = draft.read(cx).activity.agent(id)?.clone();
        let running = lane.state == AgentState::Running;
        let mut col = div()
            .id("draft35-agent-body")
            .debug_selector(|| "draft35-agent-body".to_owned())
            .size_full()
            .overflow_y_scroll()
            .px(px(20.0))
            .py(px(16.0))
            .flex()
            .flex_col()
            .gap(px(14.0))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(px(14.0))
                            .font_weight(FontWeight::MEDIUM)
                            .child(SharedString::from(lane.name.clone())),
                    )
                    .child(kit::tag(
                        if running { theme.accent } else { theme.muted },
                        state_word(&lane),
                    )),
            );
        if let Some(finding) = &lane.finding {
            col = col.child(
                div()
                    .p(px(10.0))
                    .rounded(px(8.0))
                    .bg(theme.surface)
                    .border_1()
                    .border_color(theme.hairline)
                    .text_size(px(13.0))
                    .line_height(px(19.0))
                    .child(
                        div()
                            .text_size(px(12.0))
                            .text_color(theme.faint)
                            .child("Finding"),
                    )
                    .child(SharedString::from(finding.clone())),
            );
        }
        for l in &lane.lines {
            let text = SharedString::from(l.text.clone());
            col = col.child(match l.role {
                LineRole::Prompt => div()
                    .flex()
                    .justify_end()
                    .child(
                        div()
                            .max_w(px(320.0))
                            .px(px(12.0))
                            .py(px(8.0))
                            .rounded(px(12.0))
                            .bg(theme.raised)
                            .text_size(px(13.0))
                            .line_height(px(19.0))
                            .child(text),
                    )
                    .into_any_element(),
                LineRole::Agent => div()
                    .text_size(px(13.5))
                    .line_height(px(20.0))
                    .child(text)
                    .into_any_element(),
                LineRole::Tool => div()
                    .flex()
                    .items_center()
                    .gap(px(8.0))
                    .text_size(px(13.0))
                    .text_color(theme.muted)
                    .child(icon(Icon::Search, 13.0, theme.muted))
                    .child(div().min_w_0().truncate().child(text))
                    .into_any_element(),
            });
        }
        if running {
            col = col.child(
                div()
                    .text_size(px(13.0))
                    .text_color(theme.muted)
                    .opacity(Self::shimmer(cx))
                    .child("Working"),
            );
        }
        Some(col.into_any_element())
    }
}
