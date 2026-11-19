//! ROAD-35 draft only. The MOCKED activity contract: what a run's plan, its background tasks and
//! its subagents / side chats look like to the app. The real daemon sends none of this today;
//! ROAD-52 (or a backend that reports it) replaces [`DraftActivity`] with the real thing, and
//! the three draft variants read nothing else. No protocol or daemon change lives here.
//!
//! Time is whatever the caller feeds [`DraftActivity::tick`]; nothing here reads a clock.

/// The run ids of the two draft runs the fake daemon seeds (see `FakeConfig::draft35_runs`).
pub const BUSY_RUN: &str = "plan1";
pub const EMPTY_RUN: &str = "plan0";

/// A plan item advances every this many milliseconds on the fake.
const PLAN_STEP_MS: u64 = 20_000;
/// Stop marks a task stopped after this long.
const STOP_MS: u64 = 250;
/// A background task prints a line this often.
const TAIL_EVERY_MS: u64 = 2_000;
/// Lines of output a task keeps.
const TAIL_KEEP: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlanStatus {
    Done,
    InProgress,
    Pending,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanItem {
    /// As a to-do: "Run the test suite".
    pub text: String,
    /// As the agent is doing it: "Running the test suite".
    pub doing: String,
    pub status: PlanStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TaskState {
    Running,
    /// Stop was pressed; it settles to `Stopped` after 250 ms.
    Stopping,
    Stopped,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackgroundTask {
    pub id: &'static str,
    pub command: String,
    pub port: Option<u16>,
    pub uptime_ms: u64,
    pub state: TaskState,
    /// The last lines of output, oldest first.
    pub tail: Vec<String>,
    stopping_ms: u64,
    since_line_ms: u64,
    lines_printed: usize,
    script: &'static [&'static str],
}

impl BackgroundTask {
    pub fn is_running(&self) -> bool {
        self.state != TaskState::Stopped
    }

    pub fn last_line(&self) -> &str {
        self.tail.last().map_or("", String::as_str)
    }

    /// "12 min", "4 min", "under a minute".
    pub fn uptime_label(&self) -> String {
        match self.uptime_ms / 60_000 {
            0 => "under a minute".to_owned(),
            n => format!("{n} min"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentKind {
    Subagent,
    SideChat,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentState {
    Running,
    Finished,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineRole {
    /// What the agent was asked.
    Prompt,
    /// What the agent said.
    Agent,
    /// A tool step: one counts toward the agent's steps.
    Tool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentLine {
    pub role: LineRole,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentLane {
    /// Stable id; the app makes a `RunId` of it for the side-panel tab (`d35<id>`).
    pub id: &'static str,
    pub kind: AgentKind,
    /// "researcher \u{b7} ACP v2 spec".
    pub name: String,
    /// What it is doing, as a clause after "subagent": "researching the ACP v2 spec".
    pub doing: String,
    pub state: AgentState,
    pub lines: Vec<AgentLine>,
    /// What a finished agent found, in one sentence.
    pub finding: Option<String>,
    /// Which agent text row of the lead's transcript it was spawned after (0-based).
    pub after_prose: usize,
    /// The steps it is expected to take, for a progress bar.
    pub expected_steps: usize,
    /// Lines that arrive later, one per `PLAN_STEP_MS`.
    later: Vec<AgentLine>,
}

impl AgentLane {
    pub fn steps(&self) -> usize {
        self.lines
            .iter()
            .filter(|l| l.role == LineRole::Tool)
            .count()
    }

    pub fn progress(&self) -> f32 {
        match self.state {
            AgentState::Finished => 1.0,
            AgentState::Running => {
                (self.steps() as f32 / self.expected_steps.max(1) as f32).min(0.95)
            }
        }
    }

    pub fn last_line(&self) -> &str {
        self.lines.last().map_or("", |l| l.text.as_str())
    }

    /// "researcher \u{b7} running \u{b7} 3 steps".
    pub fn chip_label(&self) -> String {
        let state = match (self.kind, self.state) {
            (_, AgentState::Running) => "running",
            (AgentKind::Subagent, AgentState::Finished) => "finished",
            (AgentKind::SideChat, AgentState::Finished) => "answered",
        };
        match self.kind {
            AgentKind::Subagent => {
                let who = self.name.split(" \u{b7} ").next().unwrap_or(&self.name);
                let n = self.steps();
                format!("{who} \u{b7} {state} \u{b7} {n} step{}", plural(n))
            }
            AgentKind::SideChat => format!("{} \u{b7} {state}", self.name),
        }
    }
}

fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Everything the variants show about one run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DraftActivity {
    pub plan: Vec<PlanItem>,
    pub tasks: Vec<BackgroundTask>,
    pub agents: Vec<AgentLane>,
    since_step_ms: u64,
}

const DEV_LINES: &[&str] = &[
    "[vite] hmr update /src/App.tsx",
    "GET /api/runs 200 14ms",
    "[vite] hmr update /src/panel/Plan.tsx",
    "GET /api/runs/plan1/events 200 9ms",
    "GET /api/health 200 2ms",
    "[vite] page reload src/main.tsx",
];

const WATCH_LINES: &[&str] = &[
    "[Running 'cargo test']",
    "test acp::handshake::negotiates_lower_version ... ok",
    "test acp::handshake::rejects_disjoint_versions ... ok",
    "test result: ok. 41 passed; 0 failed",
    "[Finished running. Exit status: 0]",
    "[Waiting for changes...]",
];

fn plan_item(text: &str, doing: &str, status: PlanStatus) -> PlanItem {
    PlanItem {
        text: text.to_owned(),
        doing: doing.to_owned(),
        status,
    }
}

fn line(role: LineRole, text: &str) -> AgentLine {
    AgentLine {
        role,
        text: text.to_owned(),
    }
}

impl DraftActivity {
    /// A run with nothing to follow: no plan, no tasks, no agents.
    pub fn empty() -> Self {
        Self {
            plan: Vec::new(),
            tasks: Vec::new(),
            agents: Vec::new(),
            since_step_ms: 0,
        }
    }

    /// The busy Claude Code run: 2 of 5 plan items done and one in progress, two background
    /// tasks, two subagents and one side chat.
    pub fn busy() -> Self {
        use PlanStatus::{Done, InProgress, Pending};
        Self {
            plan: vec![
                plan_item(
                    "Read the ACP handshake code and its failing test",
                    "Reading the handshake code",
                    Done,
                ),
                plan_item(
                    "Fix the version negotiation",
                    "Fixing the version negotiation",
                    Done,
                ),
                plan_item("Run the test suite", "Running the test suite", InProgress),
                plan_item(
                    "Update the conformance fixtures",
                    "Updating the conformance fixtures",
                    Pending,
                ),
                plan_item("Open the pull request", "Opening the pull request", Pending),
            ],
            tasks: vec![
                task("dev", "npm run dev", Some(5173), 12, DEV_LINES, &[
                    "VITE v5.4.2  ready in 412 ms",
                    "Local:   http://localhost:5173/",
                    "GET /api/health 200 3ms",
                ]),
                task("watch", "cargo watch -x test", None, 4, WATCH_LINES, &[
                    "[Running 'cargo test']",
                    "test acp::handshake::negotiates_lower_version ... ok",
                ]),
            ],
            agents: vec![
                AgentLane {
                    id: "researcher",
                    kind: AgentKind::Subagent,
                    name: "researcher \u{b7} ACP v2 spec".to_owned(),
                    doing: "researching the ACP v2 spec".to_owned(),
                    state: AgentState::Running,
                    lines: vec![
                        line(
                            LineRole::Prompt,
                            "Find what ACP v2 changes about the initialize handshake. Report the exact version-negotiation rule.",
                        ),
                        line(LineRole::Tool, "Fetched agentclientprotocol.com/spec/v2/initialize"),
                        line(LineRole::Tool, "Read schema/initialize.json"),
                        line(
                            LineRole::Agent,
                            "v2 adds a counter-offer: the agent answers with the version it picks.",
                        ),
                        line(LineRole::Tool, "Searched the changelog for \"protocolVersion\""),
                    ],
                    finding: None,
                    after_prose: 0,
                    expected_steps: 6,
                    later: vec![
                        line(LineRole::Tool, "Read schema/errors.json"),
                        line(
                            LineRole::Agent,
                            "The client must send its highest version and accept any lower one the agent returns.",
                        ),
                        line(LineRole::Tool, "Searched examples/ for a v1 fallback"),
                    ],
                },
                AgentLane {
                    id: "reviewer",
                    kind: AgentKind::Subagent,
                    name: "reviewer \u{b7} diff".to_owned(),
                    doing: "reviewing the diff".to_owned(),
                    state: AgentState::Finished,
                    lines: vec![
                        line(
                            LineRole::Prompt,
                            "Review the working-tree diff of handshake.rs. Report only real problems.",
                        ),
                        line(LineRole::Tool, "Read the diff of handshake.rs (+38 -9)"),
                        line(
                            LineRole::Agent,
                            "One problem: when the agent returns a version the client never offered, the new code treats it as a match.",
                        ),
                    ],
                    finding: Some(
                        "A version the client never offered is accepted as a match (handshake.rs:88)."
                            .to_owned(),
                    ),
                    after_prose: 1,
                    expected_steps: 1,
                    later: Vec::new(),
                },
                AgentLane {
                    id: "sidechat",
                    kind: AgentKind::SideChat,
                    name: "Side chat \u{b7} why a counter-offer?".to_owned(),
                    doing: "answering a side question".to_owned(),
                    state: AgentState::Finished,
                    lines: vec![
                        line(
                            LineRole::Prompt,
                            "Quick question while you work: why does v2 need a counter-offer at all?",
                        ),
                        line(
                            LineRole::Agent,
                            "In v1 the agent either accepted the client's version or closed the connection. The counter-offer lets an older agent say which version it can speak, so the client can fall back instead of failing.",
                        ),
                    ],
                    finding: None,
                    after_prose: 2,
                    expected_steps: 0,
                    later: Vec::new(),
                },
            ],
            since_step_ms: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.plan.is_empty() && self.tasks.is_empty() && self.agents.is_empty()
    }

    pub fn plan_done(&self) -> usize {
        self.plan
            .iter()
            .filter(|i| i.status == PlanStatus::Done)
            .count()
    }

    /// The item being worked, or the next pending one.
    pub fn current(&self) -> Option<&PlanItem> {
        self.plan
            .iter()
            .find(|i| i.status == PlanStatus::InProgress)
    }

    /// 1-based number of the current step; the plan's length when all are done.
    pub fn step_number(&self) -> usize {
        match self
            .plan
            .iter()
            .position(|i| i.status == PlanStatus::InProgress)
        {
            Some(at) => at + 1,
            None => self.plan.len(),
        }
    }

    pub fn running_tasks(&self) -> usize {
        self.tasks.iter().filter(|t| t.is_running()).count()
    }

    pub fn running_agents(&self) -> usize {
        self.agents
            .iter()
            .filter(|a| a.state == AgentState::Running)
            .count()
    }

    pub fn task(&self, id: &str) -> Option<&BackgroundTask> {
        self.tasks.iter().find(|t| t.id == id)
    }

    pub fn agent(&self, id: &str) -> Option<&AgentLane> {
        self.agents.iter().find(|a| a.id == id)
    }

    /// The worked-in finding, one sentence; `None` when there is nothing to follow.
    pub fn finding(&self) -> Option<String> {
        if self.is_empty() {
            return None;
        }
        let mut parts = Vec::new();
        if let Some(item) = self.current() {
            parts.push(format!(
                "Step {} of {} \u{b7} {}",
                self.step_number(),
                self.plan.len(),
                item.doing
            ));
        } else if !self.plan.is_empty() {
            parts.push(format!("All {} steps done", self.plan.len()));
        }
        let running: Vec<&AgentLane> = self
            .agents
            .iter()
            .filter(|a| a.state == AgentState::Running)
            .collect();
        match running.as_slice() {
            [] => {}
            [one] => parts.push(format!("1 subagent {}", one.doing)),
            many => parts.push(format!("{} subagents running", many.len())),
        }
        let live: Vec<&BackgroundTask> = self.tasks.iter().filter(|t| t.is_running()).collect();
        if let Some(first) = live.first() {
            let more = if live.len() > 1 {
                format!(" \u{b7} +{} more", live.len() - 1)
            } else {
                String::new()
            };
            let what = match first.port {
                Some(port) => format!("1 background dev server on :{port}"),
                None => "1 background task".to_owned(),
            };
            parts.push(format!("{what} for {}{more}", first.uptime_label()));
        }
        Some(parts.join(" \u{2014} "))
    }

    /// Stop a background task: it reads "stopping" now and "stopped" 250 ms later.
    pub fn stop_task(&mut self, id: &str) {
        if let Some(task) = self.tasks.iter_mut().find(|t| t.id == id)
            && task.state == TaskState::Running
        {
            task.state = TaskState::Stopping;
            task.stopping_ms = 0;
        }
    }

    /// Moves the fake forward: uptime and output stream, a plan item completes every 20 s, a
    /// running subagent takes its next step as the plan advances.
    pub fn tick(&mut self, elapsed_ms: u64) {
        for task in &mut self.tasks {
            match task.state {
                TaskState::Running => {
                    task.uptime_ms += elapsed_ms;
                    task.since_line_ms += elapsed_ms;
                    while task.since_line_ms >= TAIL_EVERY_MS {
                        task.since_line_ms -= TAIL_EVERY_MS;
                        let at = task.lines_printed % task.script.len();
                        task.lines_printed += 1;
                        task.tail.push(task.script[at].to_owned());
                        if task.tail.len() > TAIL_KEEP {
                            task.tail.remove(0);
                        }
                    }
                }
                TaskState::Stopping => {
                    task.stopping_ms += elapsed_ms;
                    if task.stopping_ms >= STOP_MS {
                        task.state = TaskState::Stopped;
                        task.tail.push("^C".to_owned());
                        task.tail.push("process stopped by you".to_owned());
                    }
                }
                TaskState::Stopped => {}
            }
        }
        if self.plan.is_empty() {
            return;
        }
        self.since_step_ms += elapsed_ms;
        while self.since_step_ms >= PLAN_STEP_MS {
            self.since_step_ms -= PLAN_STEP_MS;
            self.advance_plan();
        }
    }

    fn advance_plan(&mut self) {
        if let Some(at) = self
            .plan
            .iter()
            .position(|i| i.status == PlanStatus::InProgress)
        {
            self.plan[at].status = PlanStatus::Done;
            if let Some(next) = self.plan.get_mut(at + 1) {
                next.status = PlanStatus::InProgress;
            }
        }
        for agent in &mut self.agents {
            if agent.state == AgentState::Running && !agent.later.is_empty() {
                let next = agent.later.remove(0);
                agent.lines.push(next);
                if agent.later.is_empty() {
                    agent.state = AgentState::Finished;
                    agent.finding = Some(
                        "v2: the agent counter-offers a version; the client accepts any version at or below its own."
                            .to_owned(),
                    );
                }
            }
        }
    }
}

fn task(
    id: &'static str,
    command: &str,
    port: Option<u16>,
    minutes: u64,
    script: &'static [&'static str],
    tail: &[&str],
) -> BackgroundTask {
    BackgroundTask {
        id,
        command: command.to_owned(),
        port,
        uptime_ms: minutes * 60_000,
        state: TaskState::Running,
        tail: tail.iter().map(|s| (*s).to_owned()).collect(),
        stopping_ms: 0,
        since_line_ms: 0,
        lines_printed: 0,
        script,
    }
}

/// The tab label of a draft agent's side-panel tab, by its run id (`d35<id>`).
pub fn agent_tab_title(run_id: &str) -> Option<&'static str> {
    match run_id.strip_prefix("run_")?.strip_prefix("d35")? {
        "researcher" => Some("researcher"),
        "reviewer" => Some("reviewer"),
        "sidechat" => Some("Side chat"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_busy_run_says_the_worked_in_line() {
        assert_eq!(
            DraftActivity::busy().finding().as_deref(),
            Some(
                "Step 3 of 5 \u{b7} Running the test suite \u{2014} 1 subagent researching the ACP v2 spec \u{2014} 1 background dev server on :5173 for 12 min \u{b7} +1 more"
            )
        );
        assert_eq!(DraftActivity::empty().finding(), None);
    }

    #[test]
    fn the_plan_advances_and_stop_settles() {
        let mut a = DraftActivity::busy();
        a.tick(20_000);
        assert_eq!(a.plan_done(), 3);
        a.stop_task("dev");
        a.tick(250);
        assert_eq!(a.task("dev").map(|t| t.state), Some(TaskState::Stopped));
    }
}
