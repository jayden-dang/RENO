//! The fake daemon's side of the mocked run-activity contract (see [`crate::activity`]): a busy
//! Claude Code run whose plan advances, two background tasks that stream output and can be
//! stopped, two subagents and a side chat. Time is whatever [`FakeDaemon::advance_activity`] is
//! given; nothing here reads a clock.

use std::collections::HashMap;
use std::sync::Mutex;

use reno_proto::RunId;

use super::FakeDaemon;
use crate::activity::{
    ActivityDesk, AgentKind, AgentLane, AgentLine, AgentState, BackgroundTask, LineRole, PlanItem,
    PlanStatus, RunActivity, StopError, TaskState,
};

/// The two runs the demo data is seeded for (`FakeConfig::activity_runs`).
pub(super) const BUSY_RUN: &str = "plan1";

/// A plan item completes this often.
const PLAN_STEP_MS: u64 = 20_000;
/// Stop settles after this long.
const STOP_MS: u64 = 250;
/// A task prints a line this often.
const TAIL_EVERY_MS: u64 = 2_000;
/// Lines of output a task keeps.
const TAIL_KEEP: usize = 40;

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

/// One run's simulated activity: the contract's data plus what drives it.
#[derive(Clone, Debug)]
pub(super) struct Sim {
    activity: RunActivity,
    since_step_ms: u64,
    /// Per task, by index: (ms since Stop was asked, ms since the last line, lines printed, script).
    drive: Vec<TaskDrive>,
    /// Per agent, by index: the lines that arrive later, one per plan step.
    later: Vec<Vec<AgentLine>>,
}

#[derive(Clone, Debug)]
struct TaskDrive {
    stopping_ms: u64,
    since_line_ms: u64,
    printed: usize,
    script: &'static [&'static str],
}

fn item(text: &str, doing: &str, status: PlanStatus) -> PlanItem {
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

fn task(id: &str, command: &str, port: Option<u16>, minutes: u64, tail: &[&str]) -> BackgroundTask {
    BackgroundTask {
        id: id.to_owned(),
        command: command.to_owned(),
        port,
        uptime_ms: minutes * 60_000,
        state: TaskState::Running,
        tail: tail.iter().map(|s| (*s).to_owned()).collect(),
    }
}

impl Sim {
    pub(super) fn busy() -> Self {
        use PlanStatus::{Done, InProgress, Pending};
        let activity = RunActivity {
            plan: vec![
                item(
                    "Read the ACP handshake code and its failing test",
                    "Reading the handshake code",
                    Done,
                ),
                item(
                    "Fix the version negotiation",
                    "Fixing the version negotiation",
                    Done,
                ),
                item("Run the test suite", "Running the test suite", InProgress),
                item(
                    "Update the conformance fixtures",
                    "Updating the conformance fixtures",
                    Pending,
                ),
                item("Open the pull request", "Opening the pull request", Pending),
            ],
            tasks: vec![
                task(
                    "dev",
                    "npm run dev",
                    Some(5173),
                    12,
                    &[
                        "VITE v5.4.2  ready in 412 ms",
                        "Local:   http://localhost:5173/",
                        "GET /api/health 200 3ms",
                    ],
                ),
                task(
                    "watch",
                    "cargo watch -x test",
                    None,
                    4,
                    &["[Finished running. Exit status: 0]", "[Waiting for changes...]"],
                ),
            ],
            agents: vec![
                AgentLane {
                    id: "researcher".to_owned(),
                    kind: AgentKind::Subagent,
                    name: "researcher \u{b7} ACP v2 spec".to_owned(),
                    doing: "researching the ACP v2 spec".to_owned(),
                    state: AgentState::Running,
                    lines: vec![
                        line(
                            LineRole::Prompt,
                            "Find what ACP v2 changes about the initialize handshake. Report the exact version-negotiation rule.",
                        ),
                        line(
                            LineRole::Tool,
                            "Fetched agentclientprotocol.com/spec/v2/initialize",
                        ),
                        line(LineRole::Tool, "Read schema/initialize.json"),
                        line(
                            LineRole::Agent,
                            "v2 adds a counter-offer: the agent answers with the version it picks.",
                        ),
                        line(
                            LineRole::Tool,
                            "Searched the changelog for \"protocolVersion\"",
                        ),
                    ],
                    finding: None,
                    spawned_after: 0,
                    expected_steps: 6,
                },
                AgentLane {
                    id: "reviewer".to_owned(),
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
                    spawned_after: 1,
                    expected_steps: 1,
                },
                AgentLane {
                    id: "chat".to_owned(),
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
                    spawned_after: 2,
                    expected_steps: 0,
                },
            ],
        };
        let later = vec![
            vec![
                line(LineRole::Tool, "Read schema/errors.json"),
                line(
                    LineRole::Agent,
                    "The client must send its highest version and accept any lower one the agent returns.",
                ),
                line(LineRole::Tool, "Searched examples/ for a v1 fallback"),
            ],
            Vec::new(),
            Vec::new(),
        ];
        let drive = |script| TaskDrive {
            stopping_ms: 0,
            since_line_ms: 0,
            printed: 0,
            script,
        };
        Self {
            activity,
            since_step_ms: 0,
            drive: vec![drive(DEV_LINES), drive(WATCH_LINES)],
            later,
        }
    }

    fn tick(&mut self, elapsed_ms: u64) {
        for (task, drive) in self.activity.tasks.iter_mut().zip(&mut self.drive) {
            match task.state {
                TaskState::Running => {
                    task.uptime_ms += elapsed_ms;
                    drive.since_line_ms += elapsed_ms;
                    while drive.since_line_ms >= TAIL_EVERY_MS {
                        drive.since_line_ms -= TAIL_EVERY_MS;
                        let at = drive.printed % drive.script.len();
                        drive.printed += 1;
                        task.tail.push(drive.script[at].to_owned());
                        if task.tail.len() > TAIL_KEEP {
                            task.tail.remove(0);
                        }
                    }
                }
                TaskState::Stopping => {
                    drive.stopping_ms += elapsed_ms;
                    if drive.stopping_ms >= STOP_MS {
                        task.state = TaskState::Stopped;
                        task.tail.push("^C".to_owned());
                        task.tail.push("process stopped by you".to_owned());
                    }
                }
                TaskState::Stopped => {}
            }
        }
        if self.activity.plan.is_empty() {
            return;
        }
        self.since_step_ms += elapsed_ms;
        while self.since_step_ms >= PLAN_STEP_MS {
            self.since_step_ms -= PLAN_STEP_MS;
            self.advance_plan();
        }
    }

    fn advance_plan(&mut self) {
        let plan = &mut self.activity.plan;
        if let Some(at) = plan.iter().position(|i| i.status == PlanStatus::InProgress) {
            plan[at].status = PlanStatus::Done;
            if let Some(next) = plan.get_mut(at + 1) {
                next.status = PlanStatus::InProgress;
            }
        }
        for (agent, later) in self.activity.agents.iter_mut().zip(&mut self.later) {
            if agent.state == AgentState::Running && !later.is_empty() {
                agent.lines.push(later.remove(0));
                if later.is_empty() {
                    agent.state = AgentState::Finished;
                    agent.finding = Some(
                        "v2: the agent counter-offers a version; the client accepts any version at or below its own."
                            .to_owned(),
                    );
                }
            }
        }
    }

    fn stop(&mut self, id: &str) -> Result<(), StopError> {
        let task = self
            .activity
            .tasks
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or(StopError::UnknownTask)?;
        if task.state != TaskState::Running {
            return Err(StopError::TaskNotRunning);
        }
        task.state = TaskState::Stopping;
        Ok(())
    }
}

/// The simulations by run, held by the fake daemon.
pub(super) type Sims = Mutex<HashMap<RunId, Sim>>;

pub(super) fn seed(on: bool) -> Sims {
    let mut sims = HashMap::new();
    if on && let Ok(run) = RunId::from_suffix(BUSY_RUN) {
        sims.insert(run, Sim::busy());
    }
    Mutex::new(sims)
}

impl FakeDaemon {
    /// Moves the simulated activity forward by `elapsed_ms`: uptimes and output, plan items,
    /// subagent steps, a stop settling. The app calls this on a timer; tests call it directly.
    pub fn advance_activity(&self, elapsed_ms: u64) {
        if let Ok(mut sims) = self.activity.lock() {
            for sim in sims.values_mut() {
                sim.tick(elapsed_ms);
            }
        }
    }

    /// A task exits on its own (for tests: a Stop press that arrives too late).
    pub fn end_task(&self, run: &RunId, task: &str) {
        if let Ok(mut sims) = self.activity.lock()
            && let Some(t) = sims
                .get_mut(run)
                .and_then(|s| s.activity.tasks.iter_mut().find(|t| t.id == task))
        {
            t.state = TaskState::Stopped;
        }
    }
}

impl ActivityDesk for FakeDaemon {
    fn activity(&self, run: &RunId) -> RunActivity {
        self.activity
            .lock()
            .ok()
            .and_then(|sims| sims.get(run).map(|s| s.activity.clone()))
            .unwrap_or_default()
    }

    fn stop_task(&self, run: &RunId, task: &str) -> Result<(), StopError> {
        let mut sims = self.activity.lock().map_err(|_| StopError::UnknownTask)?;
        sims.get_mut(run).ok_or(StopError::UnknownTask)?.stop(task)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn busy() -> (FakeDaemon, RunId) {
        let daemon = FakeDaemon::start(super::super::FakeConfig {
            activity_runs: true,
            ..super::super::FakeConfig::default()
        })
        .unwrap();
        (daemon, RunId::from_suffix(BUSY_RUN).unwrap())
    }

    #[test]
    fn a_run_with_no_activity_reads_empty() {
        let (daemon, _) = busy();
        let other = RunId::from_suffix("plan0").unwrap();
        assert!(daemon.activity(&other).is_empty());
    }

    #[test]
    fn stop_reads_stopping_settles_after_250_ms_and_is_refused_twice() {
        let (daemon, run) = busy();
        daemon.stop_task(&run, "dev").unwrap();
        let state = |d: &FakeDaemon| d.activity(&run).tasks[0].state;
        assert_eq!(state(&daemon), TaskState::Stopping);
        assert_eq!(
            daemon.stop_task(&run, "dev"),
            Err(StopError::TaskNotRunning)
        );
        daemon.advance_activity(250);
        assert_eq!(state(&daemon), TaskState::Stopped);
        assert_eq!(daemon.stop_task(&run, "nope"), Err(StopError::UnknownTask));
    }

    #[test]
    fn the_plan_advances_every_20_seconds() {
        let (daemon, run) = busy();
        daemon.advance_activity(20_000);
        let plan = daemon.activity(&run).plan;
        assert_eq!(plan[2].status, PlanStatus::Done);
        assert_eq!(plan[3].status, PlanStatus::InProgress);
    }
}
