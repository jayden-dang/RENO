//! What the run's plan, background tasks and subagents say (ROAD-35): the words and the counts,
//! from the mocked `reno_client::activity` contract. The tray above the composer and the Session
//! tab's sections draw these; neither words anything itself, so they cannot disagree.

use reno_client::activity::{
    AgentKind, AgentLane, AgentState, BackgroundTask, LineRole, PlanStatus, RunActivity, StopError,
    TaskState,
};

/// "1 step", "3 steps".
pub(crate) fn count(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// The number of the step being worked (1-based); the plan's length when all are done.
pub(crate) fn step_number(a: &RunActivity) -> usize {
    a.plan
        .iter()
        .position(|i| i.status == PlanStatus::InProgress)
        .map_or(a.plan.len(), |at| at + 1)
}

pub(crate) fn plan_done(a: &RunActivity) -> usize {
    a.plan
        .iter()
        .filter(|i| i.status == PlanStatus::Done)
        .count()
}

/// The item being worked, as the agent is doing it.
fn doing(a: &RunActivity) -> Option<&str> {
    a.plan
        .iter()
        .find(|i| i.status == PlanStatus::InProgress)
        .map(|i| i.doing.as_str())
}

/// The collapsed plan tray as (progress, current step): ("Plan · 2/5 ·", "Running the test
/// suite"). `None` with no plan.
pub(crate) fn plan_line(a: &RunActivity) -> Option<(String, String)> {
    if a.plan.is_empty() {
        return None;
    }
    Some((
        format!("Plan \u{b7} {}/{} \u{b7}", plan_done(a), a.plan.len()),
        doing(a).unwrap_or("All steps done").to_owned(),
    ))
}

pub(crate) fn running_tasks(a: &RunActivity) -> usize {
    a.tasks
        .iter()
        .filter(|t| t.state != TaskState::Stopped)
        .count()
}

/// The background chip: "2 running", "none running". `None` with no background task.
pub(crate) fn running_label(a: &RunActivity) -> Option<String> {
    if a.tasks.is_empty() {
        return None;
    }
    Some(match running_tasks(a) {
        0 => "none running".to_owned(),
        n => format!("{n} running"),
    })
}

/// The one-sentence finding: what the run is doing right now and what it left running. `None`
/// when there is nothing to follow.
pub(crate) fn finding(a: &RunActivity) -> Option<String> {
    if a.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    if let Some(doing) = doing(a) {
        parts.push(format!(
            "Step {} of {} \u{b7} {doing}",
            step_number(a),
            a.plan.len()
        ));
    } else if !a.plan.is_empty() {
        parts.push(format!("All {} steps done", a.plan.len()));
    }
    let agents: Vec<&AgentLane> = a
        .agents
        .iter()
        .filter(|l| l.state == AgentState::Running)
        .collect();
    match agents.as_slice() {
        [] => {}
        [one] => parts.push(format!("1 subagent {}", one.doing)),
        many => parts.push(format!("{} subagents running", many.len())),
    }
    let live: Vec<&BackgroundTask> = a
        .tasks
        .iter()
        .filter(|t| t.state != TaskState::Stopped)
        .collect();
    if let Some(first) = live.first() {
        let more = match live.len() {
            1 => String::new(),
            n => format!(" \u{b7} +{} more", n - 1),
        };
        let what = match first.port {
            Some(port) => format!("1 background dev server on :{port}"),
            None => "1 background task".to_owned(),
        };
        parts.push(format!("{what} for {}{more}", minutes(first.uptime_ms)));
    }
    Some(parts.join(" \u{2014} "))
}

/// "12 min", "under a minute".
pub(crate) fn minutes(ms: u64) -> String {
    match ms / 60_000 {
        0 => "under a minute".to_owned(),
        n => format!("{n} min"),
    }
}

/// How long a task has run: "12 min", and "ran 12 min" once it stopped.
pub(crate) fn uptime(task: &BackgroundTask) -> String {
    match task.state {
        TaskState::Stopped => format!("ran {}", minutes(task.uptime_ms)),
        _ => minutes(task.uptime_ms),
    }
}

/// What the Stop control of a task shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum StopControl {
    /// A "Stop" button.
    Stop,
    /// A disabled "Stopping…".
    Stopping,
    /// A "Stopped" tag.
    Stopped,
}

pub(crate) fn stop_control(task: &BackgroundTask) -> StopControl {
    match task.state {
        TaskState::Running => StopControl::Stop,
        TaskState::Stopping => StopControl::Stopping,
        TaskState::Stopped => StopControl::Stopped,
    }
}

impl StopControl {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Stop => "Stop",
            Self::Stopping => "Stopping\u{2026}",
            Self::Stopped => "Stopped",
        }
    }
}

/// The line under a task whose Stop was refused.
pub(crate) fn stop_error_text(task: &BackgroundTask, error: &StopError) -> String {
    match error {
        StopError::TaskNotRunning => {
            format!("Could not stop {}: it already ended.", task.command)
        }
        StopError::UnknownTask => {
            format!("Could not stop {}: it is no longer listed.", task.command)
        }
    }
}

/// "researcher · running · 3 steps", "Side chat · why a counter-offer? · answered".
pub(crate) fn chip_label(lane: &AgentLane) -> String {
    let state = match (lane.kind, lane.state) {
        (_, AgentState::Running) => "running",
        (AgentKind::Subagent, AgentState::Finished) => "finished",
        (AgentKind::SideChat, AgentState::Finished) => "answered",
    };
    match lane.kind {
        AgentKind::Subagent => {
            let who = lane.name.split(" \u{b7} ").next().unwrap_or(&lane.name);
            format!(
                "{who} \u{b7} {state} \u{b7} {}",
                count(steps(lane), "step", "steps")
            )
        }
        AgentKind::SideChat => format!("{} \u{b7} {state}", lane.name),
    }
}

pub(crate) fn state_word(lane: &AgentLane) -> &'static str {
    match (lane.kind, lane.state) {
        (_, AgentState::Running) => "Running",
        (AgentKind::Subagent, AgentState::Finished) => "Finished",
        (AgentKind::SideChat, AgentState::Finished) => "Answered",
    }
}

/// Tool steps an agent has taken.
pub(crate) fn steps(lane: &AgentLane) -> usize {
    lane.lines
        .iter()
        .filter(|l| l.role == LineRole::Tool)
        .count()
}

/// How far a subagent is, 0..1, for its lane's bar.
pub(crate) fn progress(lane: &AgentLane) -> f32 {
    match lane.state {
        AgentState::Finished => 1.0,
        AgentState::Running => (steps(lane) as f32 / lane.expected_steps.max(1) as f32).min(0.95),
    }
}

/// The tab label of an agent's side-panel tab.
pub(crate) fn tab_title(agent: &str) -> String {
    if agent == "chat" {
        "Side chat".to_owned()
    } else {
        agent.to_owned()
    }
}

/// Which sections the Session tab shows: only what the run has. The finding is not one of them:
/// the tray above the composer already states it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionSections {
    pub(crate) plan: bool,
    pub(crate) running: bool,
    pub(crate) agents: bool,
}

pub(crate) fn session_sections(a: &RunActivity) -> SessionSections {
    SessionSections {
        plan: !a.plan.is_empty(),
        running: !a.tasks.is_empty(),
        agents: !a.agents.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use reno_client::activity::{AgentLine, PlanItem};

    use super::*;

    fn busy() -> RunActivity {
        use PlanStatus::{Done, InProgress, Pending};
        let item = |text: &str, doing: &str, status| PlanItem {
            text: text.to_owned(),
            doing: doing.to_owned(),
            status,
        };
        let task = |id: &str, command: &str, port, minutes: u64| BackgroundTask {
            id: id.to_owned(),
            command: command.to_owned(),
            port,
            uptime_ms: minutes * 60_000,
            state: TaskState::Running,
            tail: Vec::new(),
        };
        RunActivity {
            plan: vec![
                item("Read", "Reading", Done),
                item("Fix", "Fixing", Done),
                item("Run the test suite", "Running the test suite", InProgress),
                item("Update", "Updating", Pending),
                item("Open", "Opening", Pending),
            ],
            tasks: vec![
                task("dev", "npm run dev", Some(5173), 12),
                task("watch", "cargo watch -x test", None, 4),
            ],
            agents: vec![AgentLane {
                id: "researcher".to_owned(),
                kind: AgentKind::Subagent,
                name: "researcher \u{b7} ACP v2 spec".to_owned(),
                doing: "researching the ACP v2 spec".to_owned(),
                state: AgentState::Running,
                lines: vec![
                    AgentLine {
                        role: LineRole::Tool,
                        text: "a".to_owned(),
                    };
                    3
                ],
                finding: None,
                spawned_after: 0,
                expected_steps: 6,
            }],
        }
    }

    #[test]
    fn the_finding_states_the_step_the_agent_and_the_server() {
        assert_eq!(
            finding(&busy()).as_deref(),
            Some(
                "Step 3 of 5 \u{b7} Running the test suite \u{2014} 1 subagent researching the ACP v2 spec \u{2014} 1 background dev server on :5173 for 12 min \u{b7} +1 more"
            )
        );
    }

    #[test]
    fn an_empty_run_has_no_words() {
        let none = RunActivity::default();
        assert_eq!(finding(&none), None);
        assert_eq!(plan_line(&none), None);
        assert_eq!(running_label(&none), None);
        assert_eq!(session_sections(&none), SessionSections::default());
    }

    #[test]
    fn the_collapsed_plan_line_names_the_progress_and_the_current_step() {
        assert_eq!(
            plan_line(&busy()),
            Some((
                "Plan \u{b7} 2/5 \u{b7}".to_owned(),
                "Running the test suite".to_owned()
            ))
        );
    }

    #[test]
    fn the_chip_counts_the_tasks_still_running() {
        let mut a = busy();
        assert_eq!(running_label(&a).as_deref(), Some("2 running"));
        a.tasks[0].state = TaskState::Stopping;
        assert_eq!(running_label(&a).as_deref(), Some("2 running"));
        a.tasks[0].state = TaskState::Stopped;
        assert_eq!(running_label(&a).as_deref(), Some("1 running"));
        a.tasks[1].state = TaskState::Stopped;
        assert_eq!(running_label(&a).as_deref(), Some("none running"));
    }

    #[test]
    fn stop_reads_stopping_then_stopped_and_a_refusal_has_words() {
        let mut a = busy();
        assert_eq!(stop_control(&a.tasks[0]).label(), "Stop");
        a.tasks[0].state = TaskState::Stopping;
        assert_eq!(stop_control(&a.tasks[0]).label(), "Stopping\u{2026}");
        a.tasks[0].state = TaskState::Stopped;
        assert_eq!(stop_control(&a.tasks[0]).label(), "Stopped");
        assert_eq!(
            stop_error_text(&a.tasks[0], &StopError::TaskNotRunning),
            "Could not stop npm run dev: it already ended."
        );
    }

    #[test]
    fn a_subagent_chip_says_who_how_it_is_and_how_far() {
        assert_eq!(
            chip_label(&busy().agents[0]),
            "researcher \u{b7} running \u{b7} 3 steps"
        );
    }
}
