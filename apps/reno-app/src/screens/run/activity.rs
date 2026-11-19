//! The run's activity (ROAD-35): the plan and background tasks in trays above the composer,
//! subagent chips under the agent text where they were spawned, the same three things as
//! sections of the Session tab, and an agent's own transcript as a side-panel tab. Everything
//! reads `Store::run_activity` (the mocked contract, see `reno_client::activity`) and words
//! itself through `vm::activity`, so the tray and the Session tab always agree.

use gpui::{AnyElement, Context, FontWeight, Role, SharedString, Window, div, prelude::*, px};
use reno_client::activity::{
    AgentKind, AgentLane, AgentState, BackgroundTask, LineRole, PlanItem, PlanStatus, RunActivity,
    TaskState,
};
use reno_proto::RunId;

use super::RunScreen;
use crate::panel::surface::Surface;
use crate::transcript::Row;
use crate::ui::icon::{Icon, icon};
use crate::ui::kit::{self, MONO, Tone};
use crate::ui::theme::Theme;
use crate::vm::activity as words;

/// The run id of an agent's side-panel tab.
fn agent_run(id: &str) -> Option<RunId> {
    RunId::from_suffix(&reno_client::activity::agent_run_suffix(id)).ok()
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
            .child(kit::spinner("activity-plan-spin", 13.0, theme.accent))
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

impl RunScreen {
    fn open_agent(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
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
        let id = task.id.clone();
        let live = task.state != TaskState::Stopped;
        let uptime = words::uptime(task);
        let run = self.run.clone();
        let error = self
            .store
            .read(cx)
            .stop_error(&self.run, &task.id)
            .map(|e| words::stop_error_text(task, e));
        let stop: AnyElement = match words::stop_control(task) {
            words::StopControl::Stop => {
                let task_id = id.clone();
                kit::line_button(
                    theme,
                    format!("activity-stop-{id}"),
                    Some(Icon::Stop),
                    words::StopControl::Stop.label(),
                    Tone::Quiet,
                    true,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    let (run, task) = (run.clone(), task_id.clone());
                    // A refusal is kept by the store and said under the task.
                    this.store
                        .update(cx, |store, cx| store.stop_task(&run, &task, cx))
                        .ok();
                }))
                .into_any_element()
            }
            words::StopControl::Stopping => kit::line_button(
                theme,
                format!("activity-stopping-{id}"),
                None,
                words::StopControl::Stopping.label(),
                Tone::Quiet,
                false,
            )
            .into_any_element(),
            words::StopControl::Stopped => {
                kit::tag(theme.muted, words::StopControl::Stopped.label()).into_any_element()
            }
        };
        let shown: Vec<&String> = if lines == 0 {
            task.tail.last().into_iter().collect()
        } else {
            task.tail.iter().rev().take(lines).rev().collect()
        };
        div()
            .id(SharedString::from(format!("activity-task-{id}")))
            .debug_selector({
                let id = id.clone();
                move || format!("activity-task-{id}")
            })
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
            .children(error.map(|text| {
                div()
                    .id(SharedString::from(format!("activity-stop-error-{id}")))
                    .debug_selector({
                        let id = id.clone();
                        move || format!("activity-stop-error-{id}")
                    })
                    .ml(px(15.0))
                    .text_size(px(12.5))
                    .text_color(theme.danger)
                    .child(SharedString::from(text))
            }))
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
        let id = lane.id.clone();
        let stripe = stripe_color(theme, lane.state);
        let hover = theme.raised.opacity(0.4);
        let running = lane.state == AgentState::Running;
        let tail = lane.finding.clone().unwrap_or_else(|| {
            lane.lines
                .last()
                .map_or_else(String::new, |l| l.text.clone())
        });
        let open = id.clone();
        div()
            .id(SharedString::from(format!("activity-lane-{id}")))
            .debug_selector({
                let id = id.clone();
                move || format!("activity-lane-{id}")
            })
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
                words::state_word(lane)
            )))
            .focusable()
            .tab_stop(true)
            .focus_visible(kit::focus_ring(theme))
            .hover(move |s| s.bg(hover))
            .on_click(cx.listener(move |this, _, window, cx| {
                this.open_agent(&open, window, cx);
            }))
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
                                        .child(SharedString::from(words::count(
                                            words::steps(lane),
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
                                    .child(words::state_word(lane)),
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
                                        .w(gpui::relative(words::progress(lane)))
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

    /// The run's activity, when this connection has the contract and the run has any.
    fn activity(&self, cx: &gpui::App) -> Option<RunActivity> {
        self.store.read(cx).run_activity(&self.run)
    }

    /// The finding sentence. Only the tray says it: the Session tab does not repeat it.
    fn finding_line(
        &self,
        theme: &Theme,
        a: &RunActivity,
        place: &'static str,
    ) -> Option<AnyElement> {
        words::finding(a).map(|text| {
            div()
                .id(SharedString::from(format!("activity-finding-{place}")))
                .debug_selector(move || format!("activity-finding-{place}"))
                .w_full()
                .text_size(px(12.5))
                .line_height(px(18.5))
                .text_color(theme.muted)
                .child(SharedString::from(text))
                .into_any_element()
        })
    }

    /// `RENO_ACTIVITY_OPEN=plan,tasks,stop:dev`: opens the trays and presses Stop (screenshots).
    pub(crate) fn open_activity_demo(&mut self, steps: &str, cx: &mut Context<Self>) {
        for step in steps.split(',') {
            match step {
                "plan" => self.activity_plan_open = true,
                "tasks" => self.activity_tasks_open = true,
                other => {
                    if let Some(task) = other.strip_prefix("stop:") {
                        let run = self.run.clone();
                        self.store
                            .update(cx, |store, cx| store.stop_task(&run, task, cx))
                            .ok();
                    }
                }
            }
        }
        cx.notify();
    }

    // --- the trays above the composer -----------------------------------------------------------

    /// The plan tray and the background chip (and its tray), over the composer.
    pub(super) fn activity_trays(
        &mut self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let Some(a) = self.activity(cx) else {
            return Vec::new();
        };
        let (plan_open, tasks_open) = (self.activity_plan_open, self.activity_tasks_open);
        let mut out = Vec::new();
        if let Some((progress, current)) = words::plan_line(&a) {
            let header = div()
                .id("activity-plan-tray")
                .debug_selector(|| "activity-plan-tray".to_owned())
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
                .on_click(cx.listener(|this, _, _, cx| {
                    this.activity_plan_open = !this.activity_plan_open;
                    cx.notify();
                }))
                .child(icon(Icon::List, 14.0, theme.muted))
                .child(
                    div()
                        .id("activity-plan-line")
                        .debug_selector(|| "activity-plan-line".to_owned())
                        .flex_1()
                        .min_w_0()
                        .flex()
                        .gap(px(6.0))
                        .whitespace_nowrap()
                        .text_size(px(13.0))
                        .child(SharedString::from(progress))
                        .child(
                            div()
                                .min_w_0()
                                .truncate()
                                .text_color(theme.accent)
                                .child(SharedString::from(current)),
                        ),
                )
                .child(icon(
                    if plan_open {
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
                        self.finding_line(theme, &a, "tray")
                            .map(|f| div().pb(px(6.0)).child(f)),
                    )
                    .when(plan_open, |d| {
                        d.child(
                            div()
                                .id("activity-plan-list")
                                .debug_selector(|| "activity-plan-list".to_owned())
                                .child(div().h(px(1.0)).bg(theme.hairline).mb(px(4.0)))
                                .children(
                                    a.plan.iter().map(|i| plan_row(theme, i, 13.0, 4.0, false)),
                                ),
                        )
                    })
                    .into_any_element(),
            );
        }
        if let Some(label) = words::running_label(&a) {
            let dot = if words::running_tasks(&a) > 0 {
                theme.accent
            } else {
                theme.faint
            };
            let chip = kit::chip(theme, "")
                .id("activity-bg-chip")
                .debug_selector(|| "activity-bg-chip".to_owned())
                .cursor_pointer()
                .role(Role::Button)
                .aria_label("Background tasks")
                .focusable()
                .tab_stop(true)
                .focus_visible(kit::focus_ring(theme))
                .when(tasks_open, |c| c.border_color(theme.accent.opacity(0.6)))
                .on_click(cx.listener(|this, _, _, cx| {
                    this.activity_tasks_open = !this.activity_tasks_open;
                    cx.notify();
                }))
                .child(kit::dot(dot, 7.0))
                .child(SharedString::from(label))
                .child(icon(
                    if tasks_open {
                        Icon::ChevUp
                    } else {
                        Icon::ChevDown
                    },
                    13.0,
                    theme.muted,
                ));
            if tasks_open {
                // "Output ›": the Session tab has the full output; the composer keeps the keyboard.
                let links: Vec<AnyElement> = a
                    .tasks
                    .iter()
                    .map(|t| {
                        let id = t.id.clone();
                        div()
                            .id(SharedString::from(format!("activity-open-output-{id}")))
                            .debug_selector(move || format!("activity-open-output-{id}"))
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
                            // A press on the link must not move the keyboard out of the composer.
                            .on_mouse_down(gpui::MouseButton::Left, |_, window, _| {
                                window.prevent_default();
                            })
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_surface(Surface::Session, window, cx);
                            }))
                            .child("Output \u{203a}")
                            .into_any_element()
                    })
                    .collect();
                let rows: Vec<AnyElement> = a
                    .tasks
                    .iter()
                    .zip(links)
                    .enumerate()
                    .map(|(n, (t, link))| {
                        div()
                            .when(n > 0, |d| d.border_t_1().border_color(theme.hairline))
                            .child(self.task_row(theme, t, 0, 12.5, Some(link), cx))
                            .into_any_element()
                    })
                    .collect();
                out.push(
                    div()
                        .id("activity-bg-tray")
                        .debug_selector(|| "activity-bg-tray".to_owned())
                        .flex()
                        .flex_col()
                        .px(px(14.0))
                        .py(px(4.0))
                        .rounded(px(12.0))
                        .bg(theme.surface)
                        .border_1()
                        .border_color(theme.hairline)
                        .children(rows)
                        .into_any_element(),
                );
            }
            out.push(div().flex().justify_end().child(chip).into_any_element());
        }
        out
    }

    /// Chips under the agent text rows where subagents (and the side chat) were spawned.
    pub(super) fn activity_chips(
        &self,
        theme: &Theme,
        ix: usize,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let a = self.activity(cx)?;
        let prose_at = self.rows[..=ix.min(self.rows.len().saturating_sub(1))]
            .iter()
            .filter(|r| matches!(r, Row::Prose { .. }))
            .count()
            .checked_sub(1)
            .filter(|_| matches!(self.rows.get(ix), Some(Row::Prose { .. })))?;
        let chips: Vec<AnyElement> = a
            .agents
            .iter()
            .filter(|l| l.spawned_after == prose_at)
            .map(|lane| {
                let id = lane.id.clone();
                let open = id.clone();
                let label = words::chip_label(lane);
                div()
                    .id(SharedString::from(format!("activity-chip-{id}")))
                    .debug_selector(move || format!("activity-chip-{id}"))
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
                    .aria_label(SharedString::from(label.clone()))
                    .focusable()
                    .tab_stop(true)
                    .focus_visible(kit::focus_ring(theme))
                    .hover(|s| s.border_color(theme.accent.opacity(0.5)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_agent(&open, window, cx);
                    }))
                    .child(div().text_color(theme.muted).child("\u{21b3}"))
                    .child(kit::dot(stripe_color(theme, lane.state), 7.0))
                    .child(SharedString::from(label))
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

    // --- the Session tab's sections -------------------------------------------------------------

    /// Plan, Running and Agents, above the run's metadata. `None` for a run with no activity.
    pub(super) fn activity_sections(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let a = self.activity(cx)?;
        let shown = words::session_sections(&a);
        let heading = |text: &str, count: String| {
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
                .child(
                    div()
                        .font_family(MONO)
                        .font_weight(FontWeight::NORMAL)
                        .child(SharedString::from(count)),
                )
        };
        let mut body = div()
            .id("activity-sections")
            .debug_selector(|| "activity-sections".to_owned())
            .flex()
            .flex_col()
            .pb(px(6.0))
            .border_b_1()
            .border_color(theme.hairline);
        if shown.plan {
            body = body
                .child(heading(
                    "Plan",
                    format!("{}/{}", words::plan_done(&a), a.plan.len()),
                ))
                .child(
                    div()
                        .id("activity-session-plan")
                        .debug_selector(|| "activity-session-plan".to_owned())
                        .children(a.plan.iter().map(|i| plan_row(theme, i, 12.5, 6.0, true))),
                );
        }
        if shown.running {
            body = body.child(heading("Running", words::running_tasks(&a).to_string()));
            let mut rows = div()
                .id("activity-session-running")
                .debug_selector(|| "activity-session-running".to_owned());
            for (n, t) in a.tasks.iter().enumerate() {
                let id = t.id.clone();
                let full = self.activity_output.as_deref() == Some(t.id.as_str());
                let toggle = kit::line_button(
                    theme,
                    format!("activity-output-{id}"),
                    Some(Icon::Terminal),
                    if full { "Hide output" } else { "Output" },
                    Tone::Quiet,
                    true,
                )
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.activity_output = if this.activity_output.as_deref() == Some(id.as_str()) {
                        None
                    } else {
                        Some(id.clone())
                    };
                    cx.notify();
                }));
                rows = rows.child(
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
            body = body.child(rows);
        }
        if shown.agents {
            let lanes: Vec<AnyElement> = a
                .agents
                .iter()
                .map(|l| self.agent_lane(theme, l, 12.5, false, cx))
                .collect();
            body = body
                .child(heading("Agents", a.agents.len().to_string()))
                .child(
                    div()
                        .id("activity-session-agents")
                        .debug_selector(|| "activity-session-agents".to_owned())
                        .flex()
                        .flex_col()
                        .gap(px(6.0))
                        .pb(px(6.0))
                        .children(lanes),
                );
        }
        Some(body.into_any_element())
    }

    // --- the subagent / side-chat tab ---------------------------------------------------------

    /// The body of an agent's tab: its short transcript, read-only. `None` when the run has no such
    /// agent (the tab then shows its placeholder).
    pub(super) fn activity_agent_body(
        &self,
        theme: &Theme,
        run: &RunId,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let id = reno_client::activity::agent_of_run(run)?;
        let lane = self.activity(cx)?.agents.into_iter().find(|l| l.id == id)?;
        let running = lane.state == AgentState::Running;
        let mut col = div()
            .id("activity-agent-body")
            .debug_selector(|| "activity-agent-body".to_owned())
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
                        words::state_word(&lane),
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
