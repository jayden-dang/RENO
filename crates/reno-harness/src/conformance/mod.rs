//! The conformance suite (ARCHITECTURE 5.3): one set of named checks, written once against
//! `dyn Executor`, that every executor must pass.
//!
//! The mock and the fake CLI of every driver run it in plain `cargo test` (the report table is
//! printed by `tests/conformance.rs`); the real Claude Code and Codex run it on request
//! (`tests/conformance_live.rs`, opt-in, `#[ignore]`d, because it costs money and needs
//! logins).
//!
//! # Who owns what
//!
//! The *cases* own every assertion. A [`Target`] supplies data only: the executor, a [`Profile`]
//! of facts the descriptor has no field for, and a [`Plan`] of [`Scenario`]s (a prompt plus what
//! the run must show) per case. A case that needs a capability declares it, and is **skipped**
//! (reported as skipped, never as passed) when the descriptor does not claim it.
//!
//! # ARCH-15: a claim nobody exercises is a failure
//!
//! When the descriptor claims something a case would exercise and the target has no scenario for
//! it, the case **fails** ("claims X but nothing exercises it"). The only way out is an explicit
//! [`Target::waive`] with a reason, which a live target uses for what a tiny prompt cannot
//! reliably provoke (an approval, a crash); the report shows it as `waived` and the claim as
//! unverified, never as passed. The final `claims` case checks the descriptor against the other
//! cases' outcomes.
//!
//! # What it does not prove
//!
//! `ExecSpec` has no plan-mode request, so a claimed plan mode cannot be exercised and is a
//! failure; an executor that declares none is skipped. "The wire can carry a deny message" and
//! "this executor spawns a process" are not descriptor fields, so they live in [`Profile`].

#![deny(missing_docs, clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use reno_proto::{AccessMode, ActionClass, ExecutorId, McpServerSpec, SandboxLevel, SessionRef};

use crate::exec::Executor;

mod cases;
mod drive;
mod report;

pub use cases::run_suite;
pub use report::{CaseResult, LiveMeta, Status, TargetReport, render_table};

/// The reason every denial in the suite carries.
pub const DENY_MESSAGE: &str = "conformance: denied on purpose";

/// The prompt the steer case sends mid-run.
pub const STEER_TEXT: &str = "redirect please";

/// One named check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CaseId {
    /// `probe()` and the descriptor are consistent and cost no model call.
    Probe,
    /// `Started` once, text, then exactly one terminal `Done`, last.
    Lifecycle,
    /// Tool calls carry the right `ToolCall::class`; results match calls.
    ToolClasses,
    /// Every child process is reported and gone when the run ends.
    ProcessRecords,
    /// `NeedsApproval` has a structured subject and an Allow is honoured.
    ApprovalAllow,
    /// A Deny is honoured.
    ApprovalDeny,
    /// A deny message reaches the agent where the wire can carry one.
    DenyMessage,
    /// `NeedsInput` round-trips.
    Input,
    /// A steering prompt reaches the running agent.
    Steer,
    /// Cancel ends the run `Interrupted` in bounded time and leaves no process behind.
    Cancel,
    /// A crashing harness yields `Done { Error }` with a diagnostic.
    Crash,
    /// A session can be resumed.
    Resume,
    /// Injected MCP servers reach the harness.
    Mcp,
    /// Declared access modes run; the others are refused before anything is spawned.
    AccessModes,
    /// Declared sandbox levels run; the others are refused before anything is spawned.
    SandboxLevels,
    /// Structured output is honoured when claimed, refused when not.
    OutputSchema,
    /// A claimed plan mode is exercised.
    PlanMode,
    /// A claimed isolation from the owner's personal config is applied by default and lifted by
    /// an opt-in.
    UserConfig,
    /// Frames the wire did not recognise are counted, and a clean run counts none.
    UnknownFrames,
    /// Every capability the descriptor claims was proven by a passing case (ARCH-15).
    Claims,
}

impl CaseId {
    /// Every case, in the order the suite runs them.
    pub const ALL: [CaseId; 20] = [
        Self::Probe,
        Self::Lifecycle,
        Self::ToolClasses,
        Self::ProcessRecords,
        Self::ApprovalAllow,
        Self::ApprovalDeny,
        Self::DenyMessage,
        Self::Input,
        Self::Steer,
        Self::Cancel,
        Self::Crash,
        Self::Resume,
        Self::Mcp,
        Self::AccessModes,
        Self::SandboxLevels,
        Self::OutputSchema,
        Self::PlanMode,
        Self::UserConfig,
        Self::UnknownFrames,
        Self::Claims,
    ];

    /// The case's short name, as the report prints it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Probe => "probe",
            Self::Lifecycle => "lifecycle",
            Self::ToolClasses => "tool-classes",
            Self::ProcessRecords => "process-records",
            Self::ApprovalAllow => "approval-allow",
            Self::ApprovalDeny => "approval-deny",
            Self::DenyMessage => "deny-message",
            Self::Input => "input",
            Self::Steer => "steer",
            Self::Cancel => "cancel",
            Self::Crash => "crash",
            Self::Resume => "resume",
            Self::Mcp => "mcp",
            Self::AccessModes => "access-modes",
            Self::SandboxLevels => "sandbox-levels",
            Self::OutputSchema => "output-schema",
            Self::PlanMode => "plan-mode",
            Self::UserConfig => "user-config",
            Self::UnknownFrames => "unknown-frames",
            Self::Claims => "claims",
        }
    }

    /// What passing this case proves, in one sentence.
    pub fn proves(self) -> &'static str {
        match self {
            Self::Probe => {
                "probe() returns, without a model call, an installed, signed-in, version-consistent HealthProbe, and the descriptor is well formed"
            }
            Self::Lifecycle => {
                "the run emits Started once, then text, then exactly one Done as the last event, Completed"
            }
            Self::ToolClasses => {
                "every tool call carries the expected ToolCall.class and each ToolResult answers an earlier Tool"
            }
            Self::ProcessRecords => {
                "each child the run spawns is reported as a ProcessRecord and its process group is gone when the run ends"
            }
            Self::ApprovalAllow => {
                "NeedsApproval carries a structured subject (class, tool, command or paths) and Allow lets the action run"
            }
            Self::ApprovalDeny => {
                "Deny stops the action and the run still ends (Completed) instead of hanging"
            }
            Self::DenyMessage => "the deny message reaches the agent on wires that can carry one",
            Self::Input => {
                "NeedsInput carries structured questions and the answer reaches the agent"
            }
            Self::Steer => "a steering prompt sent mid-run reaches the agent",
            Self::Cancel => {
                "cancel ends the run Interrupted within the bound, with one Done, and no process group survives"
            }
            Self::Crash => "a harness that dies mid-run yields Done { Error } with a diagnostic",
            Self::Resume => "a run started with a session reference starts and completes",
            Self::Mcp => "the injected MCP servers reach the harness",
            Self::AccessModes => {
                "every claimed access mode runs; every unclaimed one is refused before spawning"
            }
            Self::SandboxLevels => {
                "every claimed sandbox level runs; every unclaimed one is refused before spawning"
            }
            Self::OutputSchema => {
                "an output schema is refused unless structured output is claimed and exercised"
            }
            Self::PlanMode => "a claimed plan mode is exercised (ExecSpec cannot request one yet)",
            Self::UserConfig => {
                "a claimed isolation from the owner's personal config reaches the harness by default and is lifted when the run opts in"
            }
            Self::UnknownFrames => {
                "a clean run counts no unrecognised frames and a made-up frame is counted by name"
            }
            Self::Claims => {
                "every capability the descriptor claims is backed by a passing case, not merely a declaration"
            }
        }
    }
}

/// A check on what a run did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expect {
    /// The final `Done.result` contains this text.
    ResultContains(String),
    /// Some `Text` event (or the concatenated text) contains this.
    TextContains(String),
    /// `Started.session` is exactly this.
    SessionIs(String),
    /// Some `Tool` event whose name contains `name` has this class.
    Tool {
        /// Part of the tool name the harness reports.
        name: String,
        /// The class it must carry.
        class: ActionClass,
    },
    /// Some `NeedsApproval` subject has this class.
    ApprovalClass(ActionClass),
    /// This path, relative to the run's working directory, exists afterwards.
    PathExists(String),
    /// This path, relative to the run's working directory, does not exist afterwards.
    PathAbsent(String),
    /// This file, relative to the run's working directory, exists afterwards and its text
    /// contains `text`. An absolute path is taken as it is.
    FileContains {
        /// The file.
        path: String,
        /// What it must contain.
        text: String,
    },
    /// The run asked for no approval of this class. Used where a mode must accept an action
    /// unasked (an edit under `AutoAcceptEdits`).
    NoApprovalOf(ActionClass),
}

/// One run to make: a prompt, how to start it, and what it must show.
#[derive(Debug, Clone)]
pub struct Scenario {
    /// The prompt. Against a fake it names a scripted scenario; against a real CLI it is a tiny
    /// real prompt.
    pub prompt: String,
    /// The access mode to run in.
    pub access: AccessMode,
    /// The sandbox level to run at.
    pub sandbox: SandboxLevel,
    /// Environment for the child.
    pub env: BTreeMap<String, String>,
    /// MCP servers to inject.
    pub mcp: Vec<McpServerSpec>,
    /// A session to resume.
    pub resume: Option<SessionRef>,
    /// Resume the session the lifecycle run started (a real CLI's session ids cannot be known
    /// ahead). Used when `resume` is `None`.
    pub resume_lifecycle_session: bool,
    /// A model override (live targets pass the cheapest).
    pub model: Option<String>,
    /// Paths, relative to the working directory, removed before the run, so an earlier run's
    /// leftovers cannot satisfy an [`Expect::PathExists`].
    pub fresh: Vec<String>,
    /// What a steer case sends mid-run, instead of [`STEER_TEXT`].
    pub steer_text: Option<String>,
    /// What the run must show.
    pub expect: Vec<Expect>,
}

impl Scenario {
    /// A full-access, unrestricted run of `prompt` with nothing else asked.
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            access: AccessMode::FullAccess,
            sandbox: SandboxLevel::Unrestricted,
            env: BTreeMap::new(),
            mcp: Vec::new(),
            resume: None,
            resume_lifecycle_session: false,
            model: None,
            fresh: Vec::new(),
            steer_text: None,
            expect: Vec::new(),
        }
    }

    /// Run in `access`.
    #[must_use]
    pub fn access(mut self, access: AccessMode) -> Self {
        self.access = access;
        self
    }

    /// Run at `sandbox`.
    #[must_use]
    pub fn sandbox(mut self, sandbox: SandboxLevel) -> Self {
        self.sandbox = sandbox;
        self
    }

    /// Set one environment variable of the child.
    #[must_use]
    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Inject these MCP servers.
    #[must_use]
    pub fn mcp(mut self, servers: Vec<McpServerSpec>) -> Self {
        self.mcp = servers;
        self
    }

    /// Resume this session.
    #[must_use]
    pub fn resume(mut self, session: impl Into<String>) -> Self {
        self.resume = Some(SessionRef(session.into()));
        self
    }

    /// Resume the session the lifecycle run started.
    #[must_use]
    pub fn resume_lifecycle_session(mut self) -> Self {
        self.resume_lifecycle_session = true;
        self
    }

    /// Remove this path (relative to the working directory) before the run.
    #[must_use]
    pub fn fresh(mut self, path: impl Into<String>) -> Self {
        self.fresh.push(path.into());
        self
    }

    /// Send this instead of [`STEER_TEXT`] when steering.
    #[must_use]
    pub fn steer_text(mut self, text: impl Into<String>) -> Self {
        self.steer_text = Some(text.into());
        self
    }

    /// Require this of the run.
    #[must_use]
    pub fn expect(mut self, expect: Expect) -> Self {
        self.expect.push(expect);
        self
    }
}

/// What an approval run must show on each answer.
#[derive(Debug, Clone)]
pub struct ApprovalPlan {
    /// The run that asks for approval. It runs in a mode that asks.
    pub scenario: Scenario,
    /// What an Allow must produce.
    pub on_allow: Vec<Expect>,
    /// What a Deny must produce.
    pub on_deny: Vec<Expect>,
    /// What a Deny carrying [`DENY_MESSAGE`] must produce, when the wire carries the message.
    pub on_deny_message: Vec<Expect>,
}

/// What a [`ModeRun`] shows about one access mode or sandbox level.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proof {
    /// The run exercises this access mode (its scenario runs in it).
    Access(AccessMode),
    /// The run exercises this sandbox level (its scenario runs at it).
    Sandbox(SandboxLevel),
}

/// How a [`ModeRun`] answers the approvals it is asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalAnswer {
    /// Allow every approval.
    Allow,
    /// Deny every approval: what a sandbox run uses, so that a request to leave the sandbox is
    /// never granted by the test itself.
    Deny,
}

/// One run that shows what an access mode or a sandbox level really does: an edit accepted
/// unasked, a command that asks, a write that does not land. A live target gives one per mode it
/// can drive cheaply; without it the mode is not run on that target and is reported `waived`.
#[derive(Debug, Clone)]
pub struct ModeRun {
    /// The mode or level it proves. The scenario must run in it.
    pub proves: Proof,
    /// The run.
    pub scenario: Scenario,
    /// How approvals are answered.
    pub answer: ApprovalAnswer,
    /// What must hold afterwards, besides the scenario's own expectations.
    pub expect: Vec<Expect>,
}

/// A run that shows whether the owner's personal config reaches the harness: the same scenario
/// is run isolated (the default) and opted in, and each must show its own expectations.
#[derive(Debug, Clone)]
pub struct UserConfigPlan {
    /// The run. Against a fake it reports what the harness was given.
    pub scenario: Scenario,
    /// What the isolated run (the default) must show.
    pub isolated: Vec<Expect>,
    /// What the run of a bot that inherits the owner's config must show.
    pub inherited: Vec<Expect>,
}

/// A run that makes the wire layer see one frame it does not know.
#[derive(Debug, Clone)]
pub struct UnknownFramePlan {
    /// The run.
    pub scenario: Scenario,
    /// Counter keys that must each rise by at least one (substring match).
    pub keys: Vec<String>,
}

/// The scenarios a target offers, one per behaviour. `None` means the target cannot exercise it.
#[derive(Debug, Clone, Default)]
pub struct Plan {
    /// The plain run: text, tool calls, a result. Also runs under every access mode and sandbox
    /// level, and is the clean-path run of the unknown-frame counters.
    pub lifecycle: Option<Scenario>,
    /// A run that asks for an approval.
    pub approval: Option<ApprovalPlan>,
    /// A run that asks a question.
    pub input: Option<Scenario>,
    /// A run that waits for a steering prompt after its first text.
    pub steer: Option<Scenario>,
    /// A run that never ends by itself.
    pub cancel: Option<Scenario>,
    /// A run in which the harness dies.
    pub crash: Option<Scenario>,
    /// A resumed run.
    pub resume: Option<Scenario>,
    /// A run with injected MCP servers.
    pub mcp: Option<Scenario>,
    /// A run with an output schema (only for an executor that claims structured output).
    pub output_schema: Option<Scenario>,
    /// A run that makes the wire see an unknown frame.
    pub unknown_frames: Option<UnknownFramePlan>,
    /// A run that shows the owner's personal config is kept out (only for an executor that
    /// claims `isolates_user_config`).
    pub user_config: Option<UserConfigPlan>,
    /// Runs that show what an access mode or sandbox level does. They stand in for the plain run
    /// of the lifecycle scenario under that mode when the target is not exhaustive.
    pub mode_runs: Vec<ModeRun>,
}

impl Plan {
    /// About how many model runs the whole suite makes with this plan: one per scenario, two for
    /// the approval (allow and deny), one per mode run, and the plain runs of the access-mode and
    /// sandbox cases that every target makes. An estimate for the line a live run prints first;
    /// the cases that reuse an earlier run are not counted twice.
    pub fn estimated_runs(&self) -> usize {
        let scenarios = [
            &self.lifecycle,
            &self.input,
            &self.steer,
            &self.cancel,
            &self.crash,
            &self.resume,
            &self.mcp,
            &self.output_schema,
        ]
        .into_iter()
        .filter(|s| s.is_some())
        .count();
        let approvals = if self.approval.is_some() { 2 } else { 0 };
        let unknown = usize::from(self.unknown_frames.is_some());
        // The user-config case runs its scenario twice.
        let unknown = unknown + 2 * usize::from(self.user_config.is_some());
        // The access-mode and sandbox cases each run the lifecycle scenario once at its own
        // mode, when there is a lifecycle scenario.
        let baselines = if self.lifecycle.is_some() { 2 } else { 0 };
        scenarios + approvals + unknown + baselines + self.mode_runs.len()
    }
}

/// Facts about an executor that the descriptor has no field for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Profile {
    /// It spawns child processes it reports as `ProcessRecord`s (false for the mock).
    pub spawns_process: bool,
    /// Its wire carries a denial's reason to the agent (Claude's permission response, ACP's
    /// `_meta`; not Codex's approval reply).
    pub deny_message_on_wire: bool,
    /// Its wire layer counts the frames it does not recognise
    /// ([`Executor::unknown_frames`]).
    pub counts_unknown_frames: bool,
    /// Its wire can put a question to a person (`NeedsInput`). False for an executor that
    /// has no way to take the answer back (Cursor's SDK has no answer channel for
    /// `askQuestion`, so the tool is not offered).
    pub asks_questions: bool,
    /// It refuses a run that asks for the owner's personal config rather than running it, so the
    /// run that shows the config reaching the harness does not exist. The refusal is covered by
    /// the executor's own tests; the user-config case is skipped.
    pub refuses_inherited_config: bool,
}

impl Profile {
    /// A driver that spawns a child, counts unknown frames and cannot carry a deny message.
    pub fn driver() -> Self {
        Self {
            spawns_process: true,
            deny_message_on_wire: false,
            counts_unknown_frames: true,
            asks_questions: true,
            refuses_inherited_config: false,
        }
    }

    /// An executor that spawns nothing and counts nothing, like the mock.
    pub fn in_process() -> Self {
        Self {
            spawns_process: false,
            deny_message_on_wire: false,
            counts_unknown_frames: false,
            asks_questions: true,
            refuses_inherited_config: false,
        }
    }
}

/// Time bounds of one suite run.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// The hard ceiling of one case. A case that outlasts it fails; it never hangs the suite.
    pub case: Duration,
    /// How long a cancelled run may take to end, and its processes to go.
    pub cancel: Duration,
    /// The ceiling of the whole target. Cases left when it is spent fail unrun.
    pub total: Duration,
}

impl Limits {
    /// Bounds for a fake CLI: seconds per case.
    pub fn fake() -> Self {
        Self {
            case: Duration::from_secs(30),
            cancel: Duration::from_secs(10),
            total: Duration::from_secs(300),
        }
    }

    /// Bounds for a real CLI and a real model: generous, but a budget.
    pub fn live() -> Self {
        Self {
            case: Duration::from_secs(120),
            cancel: Duration::from_secs(20),
            total: Duration::from_secs(900),
        }
    }
}

/// One executor under test, and the data its cases run on.
pub struct Target {
    /// The name the report prints.
    pub name: String,
    /// The executor.
    pub executor: Arc<dyn Executor>,
    /// Facts the descriptor lacks.
    pub profile: Profile,
    /// The scenarios.
    pub plan: Plan,
    /// The working directory of every run.
    pub cwd: PathBuf,
    /// The time bounds.
    pub limits: Limits,
    /// Run every claimed access mode and sandbox level (a run each). A live target turns this off
    /// to spare the model calls: the modes it does not run are reported `waived`, unverified.
    pub exhaustive: bool,
    waived: BTreeMap<CaseId, String>,
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Target")
            .field("name", &self.name)
            .field("profile", &self.profile)
            .field("waived", &self.waived)
            .finish_non_exhaustive()
    }
}

impl Target {
    /// A target with an empty plan.
    pub fn new(
        name: impl Into<String>,
        executor: Arc<dyn Executor>,
        profile: Profile,
        cwd: PathBuf,
        limits: Limits,
    ) -> Self {
        Self {
            name: name.into(),
            executor,
            profile,
            plan: Plan::default(),
            cwd,
            limits,
            exhaustive: true,
            waived: BTreeMap::new(),
        }
    }

    /// Set the plan.
    #[must_use]
    pub fn with_plan(mut self, plan: Plan) -> Self {
        self.plan = plan;
        self
    }

    /// Declare that `case` cannot be exercised on this target, and why. If the descriptor claims
    /// what the case would prove, the report says `waived` and the claim is unverified.
    #[must_use]
    pub fn waive(mut self, case: CaseId, why: impl Into<String>) -> Self {
        self.waived.insert(case, why.into());
        self
    }

    /// The cases waived on this target, with why.
    pub fn waived_cases(&self) -> Vec<(CaseId, &str)> {
        self.waived.iter().map(|(c, w)| (*c, w.as_str())).collect()
    }

    pub(crate) fn waiver(&self, case: CaseId) -> Option<&str> {
        self.waived.get(&case).map(String::as_str)
    }

    /// The id the executor declares.
    pub fn id(&self) -> ExecutorId {
        self.executor.descriptor().id
    }
}
