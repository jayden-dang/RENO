//! The MOCKED run-activity contract (ROAD-35, ROAD-52): what a run's plan, the background tasks it
//! left running and its subagents / side chats look like to the app. The real daemon sends none of
//! this yet, so nothing but the fake daemon implements [`ActivityDesk`]; the app reads activity and
//! stops tasks only through this one seam (`Store::run_activity`, `Store::stop_task`). ROAD-52
//! replaces the trait's implementation (and, when the wire types exist, these structs) with the
//! real RPC; no app screen changes. No protocol or daemon change lives here.

use reno_proto::RunId;

/// One item of the agent's plan (its todo list).
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
    /// Stop was asked for; the task settles to `Stopped` a moment later.
    Stopping,
    Stopped,
}

/// A process the agent left running in the background.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BackgroundTask {
    /// Stable within the run.
    pub id: String,
    pub command: String,
    pub port: Option<u16>,
    /// How long it has run (for a stopped task, until it stopped).
    pub uptime_ms: u64,
    pub state: TaskState,
    /// The last lines of output, oldest first.
    pub tail: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentKind {
    Subagent,
    /// A side conversation the owner started from the run.
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
    /// A tool step; each counts as one of the agent's steps.
    Tool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentLine {
    pub role: LineRole,
    pub text: String,
}

/// A subagent or side chat, with its own short transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentLane {
    /// Stable within the run: letters and digits only (it names the side-panel tab's run id).
    pub id: String,
    pub kind: AgentKind,
    /// "researcher \u{b7} ACP v2 spec".
    pub name: String,
    /// What it is doing, as a clause after "subagent": "researching the ACP v2 spec".
    pub doing: String,
    pub state: AgentState,
    pub lines: Vec<AgentLine>,
    /// What a finished agent found, in one sentence.
    pub finding: Option<String>,
    /// Which agent text message of the lead's transcript it was spawned after (0-based).
    pub spawned_after: usize,
    /// The steps it is expected to take (a progress estimate); 0 when unknown.
    pub expected_steps: usize,
}

/// Everything the app shows about one run's activity. Empty for a run with none.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RunActivity {
    pub plan: Vec<PlanItem>,
    pub tasks: Vec<BackgroundTask>,
    pub agents: Vec<AgentLane>,
}

impl RunActivity {
    pub fn is_empty(&self) -> bool {
        self.plan.is_empty() && self.tasks.is_empty() && self.agents.is_empty()
    }
}

/// Why a stop was not taken.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StopError {
    /// The task already ended (or is already stopping).
    TaskNotRunning,
    /// The run has no such task.
    UnknownTask,
}

/// The seam: reading a run's activity and stopping one of its background tasks.
pub trait ActivityDesk: Send + Sync {
    /// The run's activity now. Empty when the run has none.
    fn activity(&self, run: &RunId) -> RunActivity;
    /// Asks the task `task` of `run` to stop. It reads `Stopping` at once and `Stopped` shortly
    /// after.
    fn stop_task(&self, run: &RunId, task: &str) -> Result<(), StopError>;
}

/// The id (suffix) of the run id a subagent's side-panel tab is keyed by.
pub fn agent_run_suffix(agent: &str) -> String {
    format!("agt{agent}")
}

/// The agent id a side-panel tab's run id names, if it names one.
pub fn agent_of_run(run: &RunId) -> Option<&str> {
    run.as_str().strip_prefix("run_")?.strip_prefix("agt")
}
