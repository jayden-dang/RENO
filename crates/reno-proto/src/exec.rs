//! The execution contract (ARCHITECTURE section 4.2): what a run is asked to do, the events it
//! yields, and what an executor declares it can do. Data only; the `Executor` trait lives in
//! `reno-harness`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::{ExecutorId, RunId};
use crate::run::{StopDetail, StopReason};

/// How much a harness may do without asking. Never run with more than requested (ARCH-15).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AccessMode {
    /// Every consequential action needs approval.
    Supervised,
    /// File edits proceed; commands and network still ask.
    AutoAcceptEdits,
    /// The harness asks nothing.
    FullAccess,
}

impl std::fmt::Display for AccessMode {
    /// The mode in words, for a person: never the variant name.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Supervised => "Supervised",
            Self::AutoAcceptEdits => "Auto-accept edits",
            Self::FullAccess => "Full access",
        })
    }
}

/// `items` in words, joined with commas; `nothing` when there are none.
pub fn list_in_words<T: std::fmt::Display>(items: &[T]) -> String {
    if items.is_empty() {
        return "nothing".to_owned();
    }
    items
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The sandbox the harness itself enforces around its tools: a ceiling below the access mode.
/// A run that asks for `ReadOnly` or `WorkspaceWrite` goes only to an executor that enforces that
/// level; `Unrestricted` asks for no ceiling beyond the access mode, so every executor can take it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxLevel {
    /// No writes.
    ReadOnly,
    /// Writes confined to the workspace.
    WorkspaceWrite,
    /// No sandbox ceiling beyond the access mode (and the run's worktree).
    Unrestricted,
}

impl std::fmt::Display for SandboxLevel {
    /// The level in words, for a person: never the variant name.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ReadOnly => "Read-only",
            Self::WorkspaceWrite => "Workspace write",
            Self::Unrestricted => "Unrestricted",
        })
    }
}

/// Requested reasoning effort, least to most. Which levels a model takes is what its harness
/// reports (`ModelInfo::efforts`); a level a model does not list is refused before the run
/// starts. Ordered, so `Low < High`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    /// Barely any reasoning (Codex `minimal`).
    Minimal,
    /// Fastest, least reasoning.
    Low,
    /// Balanced default.
    Medium,
    /// More reasoning.
    High,
    /// Extra high (Claude Code and Codex `xhigh`).
    #[serde(rename = "xhigh")]
    XHigh,
    /// The most the model offers (Claude Code `max`).
    Max,
}

impl Effort {
    /// Every level, least to most.
    pub const ALL: [Self; 6] = [
        Self::Minimal,
        Self::Low,
        Self::Medium,
        Self::High,
        Self::XHigh,
        Self::Max,
    ];

    /// The word harnesses use for it: `minimal`, `low`, `medium`, `high`, `xhigh`, `max`.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::XHigh => "xhigh",
            Self::Max => "max",
        }
    }

    /// The level a harness word names, if Reno knows it.
    pub fn from_word(word: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|e| e.as_str() == word)
    }
}

/// When a running harness can accept a steering message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Steering {
    /// Cannot be steered mid-run.
    None,
    /// Steering lands at the next turn.
    TurnBoundary,
    /// Steering lands at the next step.
    StepBoundary,
}

/// An opaque handle a harness gives for resuming a conversation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct SessionRef(pub String);

/// An MCP server injected into a harness. Reno's own server is always first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct McpServerSpec {
    /// Name the harness knows the server by.
    pub name: String,
    /// Program to launch.
    pub command: String,
    /// Arguments to the program.
    #[serde(default)]
    pub args: Vec<String>,
    /// Environment for the server process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// Everything an executor needs to start one run.
///
/// `env` can hold a bot's secrets, so `Debug` shows its names and never its values.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecSpec {
    /// The run this execution belongs to.
    pub run_id: RunId,
    /// The task prompt.
    pub prompt: String,
    /// Working directory; always a Reno-managed worktree for write tasks.
    pub cwd: PathBuf,
    /// Model override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Standing instructions for the run, from the bot it runs as. Each driver applies them the
    /// way its harness allows: as an appended system prompt, as developer instructions, or in
    /// front of the first prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
    /// Reasoning effort override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<Effort>,
    /// Per-model option choices (option id to choice id), each one the model lists
    /// (`ModelInfo::options`). Applied when the harness starts: a changed choice is launch-time
    /// only.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub model_options: BTreeMap<String, String>,
    /// Access granted to the harness.
    pub access: AccessMode,
    /// Sandbox the harness must enforce.
    pub sandbox: SandboxLevel,
    /// Session to resume, if the executor supports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<SessionRef>,
    /// MCP servers to inject.
    #[serde(default)]
    pub mcp: Vec<McpServerSpec>,
    /// Allowlisted environment for the child process.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// JSON Schema the final result must satisfy, when the executor supports structured output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
    /// Let the harness load the owner's personal configuration (user-level settings, hooks,
    /// plugins, memory, instructions). Off by default: a run is isolated from it where the
    /// executor can enforce that (`ExecutorCaps::isolates_user_config`), and runs as it always
    /// did where it cannot. Reno's own injected MCP server and the run's policy apply either way.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub inherit_user_config: bool,
}

impl std::fmt::Debug for ExecSpec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let env: BTreeMap<&str, &str> = self
            .env
            .keys()
            .map(|key| (key.as_str(), "[redacted]"))
            .collect();
        f.debug_struct("ExecSpec")
            .field("run_id", &self.run_id)
            .field("prompt", &self.prompt)
            .field("cwd", &self.cwd)
            .field("model", &self.model)
            .field("instructions", &self.instructions)
            .field("effort", &self.effort)
            .field("model_options", &self.model_options)
            .field("access", &self.access)
            .field("sandbox", &self.sandbox)
            .field("resume", &self.resume)
            .field("mcp", &self.mcp)
            .field("env", &env)
            .field("output_schema", &self.output_schema)
            .field("inherit_user_config", &self.inherit_user_config)
            .finish()
    }
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// What a tool call returned, kept in the journal so the transcript can show it and a later
/// reader (the app, a verifier, a teammate) does not have to run the call again.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolOutput {
    /// The text, with the middle cut out when it was longer than [`Self::MAX_BYTES`]: the head and
    /// the tail are kept, joined by a line saying how much is missing. Always valid UTF-8 cut on a
    /// character boundary, with secrets redacted like the rest of the journal.
    pub text: String,
    /// The size of the whole output in bytes before it was cut.
    pub bytes: u64,
    /// Whether `text` is shorter than the whole output.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl ToolOutput {
    /// The most text a journal row keeps for one call.
    pub const MAX_BYTES: usize = 16 * 1024;
}

/// A tool invocation as far as Reno needs to know it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    /// Tool name as the harness reports it (`Bash`, `exec`, `mcp__server__tool`, …). Names
    /// differ between harnesses; use [`ToolCall::class`] to treat calls alike.
    pub name: String,
    /// The tool's input, verbatim.
    #[serde(default)]
    pub input: serde_json::Value,
    /// What kind of action this is, the same vocabulary approvals use. Every executor fills it,
    /// so views and policy never need a harness's own tool names. Absent on the wire means
    /// [`ActionClass::Other`].
    #[serde(default = "ActionClass::other")]
    pub class: ActionClass,
}

impl ToolCall {
    /// A call named `name` with `input`, classified as `class`.
    pub fn new(name: impl Into<String>, input: serde_json::Value, class: ActionClass) -> Self {
        Self {
            name: name.into(),
            input,
            class,
        }
    }
}

/// One question put to a human by a harness.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Question {
    /// Identifier the answer refers to.
    pub id: String,
    /// The question text.
    pub prompt: String,
    /// Suggested answers; empty for free text.
    #[serde(default)]
    pub options: Vec<String>,
    /// Whether more than one option may be chosen.
    #[serde(default)]
    pub multi_select: bool,
}

/// What kind of action an approval is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActionClass {
    /// Reading files.
    Read,
    /// Editing files.
    Edit,
    /// Running a command.
    Command,
    /// Network access.
    Network,
    /// An MCP tool call.
    McpTool,
    /// Anything else.
    Other,
}

impl ActionClass {
    /// [`ActionClass::Other`], for serde defaults.
    pub fn other() -> Self {
        Self::Other
    }
}

/// The exact thing a human is asked to approve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalSubject {
    /// Class of the action.
    pub action: ActionClass,
    /// Tool the action goes through.
    pub tool: String,
    /// The command line, for command actions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// Paths the action touches.
    #[serde(default)]
    pub paths: Vec<PathBuf>,
    /// The harness's own request, kept for the audit record.
    #[serde(default)]
    pub raw: serde_json::Value,
}

impl ApprovalSubject {
    /// A digest that identifies this subject for the repeat-denial rule: the SHA-256, as 64
    /// lowercase hex digits, of the canonical JSON of the action class, tool, command and paths.
    ///
    /// `raw` is left out on purpose. It is the harness's own request and may carry a per-request
    /// id, which would make every repeat of a denied action look new. Paths are digested as
    /// written; two spellings of one path are two subjects, which errs towards asking again.
    pub fn digest(&self) -> String {
        crate::defs::content_hash(&serde_json::json!({
            "action": self.action,
            "tool": self.tool,
            "command": self.command,
            "paths": self.paths,
        }))
    }
}

/// How an approval request ended: the executor was told yes or no.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalOutcome {
    /// The action may proceed.
    Approved,
    /// The action is refused.
    Denied,
}

/// Who or what resolved an approval request, and so why the transcript shows the outcome it
/// does.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ResolvedBy {
    /// A person decided it.
    Human,
    /// A rule decided it, with no question asked. `name` is a policy rule's `note`, or when it
    /// has none a label for a person, such as "policy rule 8 (command `git push*`)" (its number
    /// from 1, then what it matches); `default` when no rule matched and the policy's default
    /// effect applied; `standing:<rule id>` for a standing rule a person made (`rule.list`); or
    /// `builtin:reno-mcp` for a call to one of Reno's own MCP tools; or `builtin:full-access` for a
    /// request a full-access run's policy and history did not refuse (allowed by the access mode).
    Rule {
        /// The rule's name.
        name: String,
    },
    /// Nobody decided before the approval's time limit, or the run ended or the daemon
    /// restarted first. Always an outcome of `denied`. When the run's end is what closed it, the
    /// decision's `message` says so ("withdrawn: the run was cancelled"); with no message it was
    /// the time limit.
    Expired,
    /// The same action was denied earlier in this run, so it was denied again unasked.
    RepeatDenial,
}

/// One event from a running executor. A failing harness yields `Done { stop: Error }`; the
/// stream itself never errors mid-run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum ExecEvent {
    /// The harness session began.
    Started {
        /// Handle for resuming this session.
        session: SessionRef,
        /// Model actually in use.
        model: String,
    },
    /// Assistant text.
    Text {
        /// The text chunk.
        text: String,
    },
    /// Model reasoning text.
    Reasoning {
        /// The reasoning chunk.
        text: String,
    },
    /// A tool call began.
    Tool {
        /// Call id, matched by `ToolResult`.
        id: String,
        /// The call.
        call: ToolCall,
    },
    /// A tool call finished.
    ToolResult {
        /// Id of the call.
        id: String,
        /// Whether it succeeded.
        ok: bool,
        /// Short human-readable outcome.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        summary: Option<String>,
        /// What the tool printed or returned, cut to [`ToolOutput::MAX_BYTES`]. The journal is the
        /// only record of it: an event written without it cannot be given one later.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        output: Option<ToolOutput>,
    },
    /// Tokens used since the previous `Usage` of the run (executors report per turn, or once at
    /// the end). The daemon adds them up: a bot's daily token budget is the sum of its runs'.
    ///
    /// The four counts are disjoint: `input_tokens` is the input that was neither read from nor
    /// written to the prompt cache, so the whole input of a call is
    /// `input_tokens + cache_read_tokens + cache_write_tokens`.
    Usage {
        /// Input tokens that were not served from or written to the prompt cache.
        input_tokens: u64,
        /// Output tokens.
        output_tokens: u64,
        /// Input tokens served from the prompt cache (about a tenth of the price of input).
        /// Absent means 0, as does an executor that does not report it.
        #[serde(default, skip_serializing_if = "is_zero")]
        cache_read_tokens: u64,
        /// Input tokens written to the prompt cache. Absent means 0.
        #[serde(default, skip_serializing_if = "is_zero")]
        cache_write_tokens: u64,
    },
    /// The harness asks a human questions.
    NeedsInput {
        /// Id the answer is sent against.
        request_id: String,
        /// The questions.
        questions: Vec<Question>,
    },
    /// The harness asks a human to approve an action.
    NeedsApproval {
        /// Id the decision is sent against.
        request_id: String,
        /// What is being approved.
        subject: ApprovalSubject,
    },
    /// An approval request was resolved, by a person, a policy rule, a time limit or the
    /// repeat-denial rule. Follows the `NeedsApproval` it answers, so a transcript shows why the
    /// harness was told yes or no. Only daemons that negotiate the `approval.history` feature
    /// emit it.
    ApprovalResolved {
        /// The `request_id` of the `NeedsApproval` this answers.
        request_id: String,
        /// What the harness was told.
        decision: ApprovalOutcome,
        /// Who or what decided.
        by: ResolvedBy,
        /// For a denial, the reason: a rule's message, or the one a human gave with
        /// `approval.decide`. It also goes to the harness where the harness can take text; the
        /// harness's own decision type carries none, so this is where the reason is kept.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// A question (`NeedsInput`) went unanswered for the policy's approval time limit and was
    /// declined: the harness was told there is no answer, and the run goes on. Follows the
    /// `NeedsInput` it answers. Older clients that do not know it skip it.
    InputExpired {
        /// The `request_id` of the `NeedsInput` this answers.
        request_id: String,
    },
    /// The run's model was changed while it ran (`run.set_model`): `model` is the one the
    /// harness now reports in use, which it applies from its next model call. Only daemons that
    /// negotiate the `run.model` feature emit it; older clients skip it.
    ModelChanged {
        /// The model now in use, as the harness resolved it (`claude-sonnet-5-5` for `sonnet`).
        model: String,
    },
    /// The latest occupancy of the model's context window: how many tokens the next model call
    /// starts from, and how many the window holds. Not billing (that is `Usage`, which adds up;
    /// this replaces itself): never derive one from the other. A field the harness did not say
    /// is absent and means "unchanged", see [`ContextUsage::apply`]. Only daemons that negotiate
    /// the `run.context` feature emit it; older clients skip it.
    ContextUsage {
        /// Tokens now in the context, when the harness says.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tokens: Option<u64>,
        /// Size of the context window, when the harness says.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        window: Option<u64>,
    },
    /// The run's effort or model options were changed while it ran (`run.configure`): the
    /// configuration in force after the change, whether or not the harness applies it yet (see
    /// `RunConfigureResult` for when). Only daemons that negotiate the `run.configure` feature
    /// emit it; older clients skip it.
    ConfigChanged {
        /// The effort now asked for; absent: the model's own default.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effort: Option<Effort>,
        /// Every per-model option choice now asked for (option id to choice id).
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        model_options: BTreeMap<String, String>,
    },
    /// A message from the human, added by `run.steer`. Only daemons that negotiate the
    /// `run.steer` feature emit it.
    UserMessage {
        /// The message text.
        text: String,
    },
    /// The run called Reno's `report` tool: a structured status, shown in the transcript. Only
    /// daemons that negotiate the `agent` feature emit it.
    AgentReport {
        /// How the work stands.
        status: crate::agent::ReportStatus,
        /// A short summary.
        summary: String,
        /// Anything longer.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        details: Option<String>,
    },
    /// The run ended; always the last event.
    Done {
        /// Why it stopped.
        stop: StopReason,
        /// Final result text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        /// Failure detail, when `stop` is `error`. For people: read `reason` to decide.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diagnostic: Option<String>,
        /// The typed reason, when the daemon knows one (a budget, a refused start, a cancel, a
        /// shutdown). Absent from an executor's own `Done` and from daemons that predate it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<StopDetail>,
    },
    /// The run was resumed: a new attempt continues the harness session. Follows the
    /// `Done` of the attempt that ended, and precedes the new attempt's `Started`. Only daemons
    /// that negotiate the `run.resume` feature emit it.
    Resumed {
        /// The attempt that ended and is continued.
        from_attempt: u32,
        /// The session the new attempt resumes.
        session: SessionRef,
    },
}

/// A model an executor offers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    /// Model id as passed to the harness.
    pub id: String,
    /// Display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// What the harness says it is for ("Most efficient for simpler tasks"), when it says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The effort levels the harness says this model takes, least to most. Empty: the harness
    /// did not say, and no effort may be asked of it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub efforts: Vec<Effort>,
    /// The level the model uses when none is asked for, when the harness says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_effort: Option<Effort>,
    /// Per-model options a run may choose at launch (the context window of a Claude model).
    /// Empty: none. Needs feature `run.context`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<ModelOption>,
}

/// When a changed configuration of a running run (`run.configure`) takes hold.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum ConfigTiming {
    /// On the live process, from its next model call, inside the turn that runs.
    Now,
    /// From the next turn of the run (a steering message or a new prompt starts one).
    NextTurn,
    /// Not on the live process: stored on the run, and used the next time it is launched (a
    /// resume).
    AfterRestart,
}

/// A request to change a run's effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "type",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum EffortChange {
    /// Ask for `effort`, one the run's model lists.
    Set {
        /// The level.
        effort: Effort,
    },
    /// Go back to the model's own default.
    Clear,
}

/// The id of the context-window option of a model ([`ModelOption::id`]), whose choices are
/// [`CONTEXT_WINDOW_200K`] and [`CONTEXT_WINDOW_1M`].
pub const MODEL_OPTION_CONTEXT_WINDOW: &str = "contextWindow";
/// The `200k` choice of [`MODEL_OPTION_CONTEXT_WINDOW`].
pub const CONTEXT_WINDOW_200K: &str = "200k";
/// The `1m` choice of [`MODEL_OPTION_CONTEXT_WINDOW`].
pub const CONTEXT_WINDOW_1M: &str = "1m";

/// A launch-time setting a model offers besides effort, as a closed set of choices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelOption {
    /// What `task.run`'s `modelOptions` names it by (`contextWindow`).
    pub id: String,
    /// Display name.
    pub label: String,
    /// The values it takes.
    pub choices: Vec<ModelOptionChoice>,
    /// The choice id used when none is asked for. For a read-only option, the choice in force.
    pub default_choice: String,
    /// Whether `task.run` and `run.configure` may set it. False: the harness reports the value
    /// (Grok's context window) but offers no way to change it, so `choices` only say what
    /// sizes exist and a request for it is refused. Absent means true.
    #[serde(default = "settable_default", skip_serializing_if = "is_settable")]
    pub settable: bool,
}

fn settable_default() -> bool {
    true
}

fn is_settable(settable: &bool) -> bool {
    *settable
}

/// One value of a [`ModelOption`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ModelOptionChoice {
    /// What `modelOptions` names it by (`1m`).
    pub id: String,
    /// Display name.
    pub label: String,
}

/// How full the model's context is: the latest of what `ExecEvent::ContextUsage` reported.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContextUsage {
    /// Tokens in the context, when known.
    pub tokens: Option<u64>,
    /// Size of the window, when known.
    pub window: Option<u64>,
}

impl ContextUsage {
    /// Merge a report: a field it does not carry keeps its previous value, a window of 0 is
    /// ignored (a harness that does not know says 0), and 0 tokens is valid (a compaction
    /// emptied the context).
    pub fn apply(&mut self, tokens: Option<u64>, window: Option<u64>) {
        self.tokens = tokens.or(self.tokens);
        self.window = window.filter(|n| *n > 0).or(self.window);
    }

    /// Tokens over window, when both are known.
    #[allow(clippy::cast_precision_loss)]
    pub fn fraction(self) -> Option<f64> {
        Some(self.tokens? as f64 / self.window.filter(|n| *n > 0)? as f64)
    }

    /// Fold a run's journal: the usage after its last `ContextUsage` event. Never reads
    /// `Usage`, which is billing.
    pub fn latest<'a>(events: impl IntoIterator<Item = &'a ExecEvent>) -> Self {
        let mut usage = Self::default();
        for event in events {
            if let ExecEvent::ContextUsage { tokens, window } = event {
                usage.apply(*tokens, *window);
            }
        }
        usage
    }
}

#[cfg(test)]
mod context_usage_tests {
    use super::*;

    #[test]
    fn a_missing_field_keeps_its_previous_value() {
        let mut usage = ContextUsage::default();
        usage.apply(Some(150_000), Some(200_000));
        usage.apply(None, Some(1_000_000));
        assert_eq!(
            (usage.tokens, usage.window),
            (Some(150_000), Some(1_000_000))
        );
        usage.apply(Some(90_000), None);
        assert_eq!(
            (usage.tokens, usage.window),
            (Some(90_000), Some(1_000_000))
        );
    }

    #[test]
    fn a_zero_window_is_ignored_and_zero_tokens_are_valid() {
        let mut usage = ContextUsage::default();
        usage.apply(Some(150_000), Some(200_000));
        usage.apply(Some(0), Some(0));
        assert_eq!((usage.tokens, usage.window), (Some(0), Some(200_000)));
        let mut empty = ContextUsage::default();
        empty.apply(None, Some(0));
        assert_eq!(empty, ContextUsage::default());
    }

    #[test]
    fn the_fraction_needs_both_and_a_window() {
        let usage = ContextUsage {
            tokens: Some(50),
            window: Some(200),
        };
        assert_eq!(usage.fraction(), Some(0.25));
        assert_eq!(
            ContextUsage {
                tokens: Some(1),
                window: None
            }
            .fraction(),
            None
        );
        assert_eq!(
            ContextUsage {
                tokens: None,
                window: Some(1)
            }
            .fraction(),
            None
        );
        assert_eq!(
            ContextUsage {
                tokens: Some(1),
                window: Some(0)
            }
            .fraction(),
            None
        );
    }

    #[test]
    fn the_journal_folds_to_the_last_report_and_ignores_billing() {
        let events = [
            ExecEvent::ContextUsage {
                tokens: Some(10),
                window: Some(200_000),
            },
            ExecEvent::Usage {
                input_tokens: 999_999,
                output_tokens: 5,
                cache_read_tokens: 0,
                cache_write_tokens: 0,
            },
            ExecEvent::ContextUsage {
                tokens: Some(30),
                window: None,
            },
        ];
        let usage = ContextUsage::latest(&events);
        assert_eq!((usage.tokens, usage.window), (Some(30), Some(200_000)));
        assert_eq!(ContextUsage::latest(&events[1..2]), ContextUsage::default());
    }
}

/// Harness CLI versions an adapter supports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct VersionRange {
    /// Oldest supported version.
    pub min: String,
    /// Newest version the conformance suite passed against.
    pub verified: String,
}

/// What an executor declares it can do. Nothing is trusted beyond this (ARCH-15).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[allow(clippy::struct_excessive_bools)]
pub struct ExecutorCaps {
    /// When steering messages can land.
    pub steering: Steering,
    /// Whether sessions can be resumed.
    pub resume: bool,
    /// Access modes the executor can honour.
    pub access_modes: Vec<AccessMode>,
    /// Sandbox levels the executor can honour: a run is only started at a level listed here. An
    /// executor with no sandbox of its own honours only `Unrestricted`.
    pub sandbox_levels: Vec<SandboxLevel>,
    /// False means it cannot join a team or use boards.
    pub mcp_injection: bool,
    /// Whether `output_schema` is honoured.
    pub structured_output: bool,
    /// Whether the end of a turn is reported deterministically.
    pub deterministic_turn_end: bool,
    /// Whether a plan mode exists.
    pub plan_mode: bool,
    /// Models on offer.
    pub models: Vec<ModelInfo>,
    /// Whether a running run's model can be changed (`run.set_model`). The harness applies it
    /// from its next model call, inside the same turn. Only claimed where a live run proved it
    /// (Claude Code's `set_model` control request, checked against 2.1.326); Codex fixes the
    /// model per turn and an ACP agent's `session/set_model` is unproven mid-prompt.
    #[serde(default)]
    pub model_switch: bool,
    /// When a change of a running run's effort (`run.configure`) takes hold on this executor,
    /// as a person would see it: `now` (inside the turn), `next_turn`. Absent: it never does
    /// live; a changed effort is stored and used the next time the run is launched
    /// (`after_restart`). Claude Code's `apply_flag_settings` is upstream-verified on 2.1.286
    /// only: no run of this build has been checked against a live CLI (ARCH-15).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_effort: Option<ConfigTiming>,
    /// Likewise for the per-model options (`ModelInfo::options`). Claude Code's `[1m]` window
    /// switches with its model (`now`); a changed 200K limit is a launch environment variable
    /// and is always `after_restart`, whatever this says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub live_model_options: Option<ConfigTiming>,
    /// Names the CLI accepts for a model besides the ids in `models`: short aliases that
    /// resolve on the CLI's side (`sonnet`, `opus`), so they are never reported as unknown.
    /// Empty when the harness has none.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub model_aliases: Vec<String>,
    /// Whether a run is kept from loading the owner's personal configuration for that harness
    /// (its user-level settings, hooks, plugins, memory and instructions) unless the bot opts in
    /// (`inherit_user_config`). Only claimed where the driver enforces it; false means a run
    /// loads whatever the owner's own CLI would (ARCH-15).
    #[serde(default)]
    pub isolates_user_config: bool,
}

/// The suffix that asks Claude Code for the 1M-token context variant of a model or alias.
const LONG_CONTEXT_SUFFIX: &str = "[1m]";

impl ExecutorCaps {
    /// Whether `model` is one of the executor's aliases, with or without the `[1m]` suffix.
    pub fn is_model_alias(&self, model: &str) -> bool {
        let base = model.strip_suffix(LONG_CONTEXT_SUFFIX).unwrap_or(model);
        self.model_aliases
            .iter()
            .any(|alias| alias == base || alias == model)
    }

    /// The id to look `model` up by in a model list: without its `[1m]` suffix when the
    /// executor has aliases (an executor with none does not know the suffix).
    pub fn base_model<'a>(&self, model: &'a str) -> &'a str {
        if self.model_aliases.is_empty() {
            model
        } else {
            model.strip_suffix(LONG_CONTEXT_SUFFIX).unwrap_or(model)
        }
    }
}

/// An installed harness and what it can do.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ExecutorDescriptor {
    /// Executor id, for example `claude-code`.
    pub id: ExecutorId,
    /// Version of the Reno executor contract the adapter implements.
    pub contract_version: u32,
    /// Driver version plus the detected harness CLI version.
    pub adapter_version: String,
    /// Supported harness CLI versions.
    pub cli_verified: VersionRange,
    /// Declared capabilities.
    pub caps: ExecutorCaps,
}

impl ToolOutput {
    /// The most text a driver hands the supervisor for one call: a generous cut, so the
    /// supervisor redacts secret values from the text before the journal's cut
    /// ([`Self::recut`]) drops the middle. The cut of the raw form falls far from what the
    /// journal keeps, so a value cut in half by it leaves nothing in the journal.
    pub const RAW_MAX_BYTES: usize = 1024 * 1024;

    /// Room above [`Self::MAX_BYTES`] for the `… N bytes cut …` line.
    const MARKER_SLACK: usize = 64;

    /// For the executors' normalizers: the text cut to [`Self::RAW_MAX_BYTES`] by the same head
    /// and tail rule. It is not fit for the journal: the supervisor redacts it, then
    /// [`recut`](Self::recut)s it.
    pub fn from_raw(text: &str) -> Self {
        Self::cut(text, text.len() as u64, Self::RAW_MAX_BYTES)
    }

    /// [`Self::from_raw`], or none for an empty text (nothing to show).
    pub fn from_raw_opt(text: &str) -> Option<Self> {
        (!text.is_empty()).then(|| Self::from_raw(text))
    }

    /// Keeps `text` whole when it fits in [`Self::MAX_BYTES`]; otherwise keeps the first and the
    /// last half of the budget (cut on character boundaries) joined by a `… N bytes cut …` line.
    /// `bytes` is always the original size.
    pub fn from_text(text: &str) -> Self {
        Self::cut(text, text.len() as u64, Self::MAX_BYTES)
    }

    /// [`Self::from_text`], or none for an empty text (nothing to show).
    pub fn from_text_opt(text: &str) -> Option<Self> {
        (!text.is_empty()).then(|| Self::from_text(text))
    }

    /// The journal form of a raw output: cut to [`Self::MAX_BYTES`] by the same head and tail
    /// rule, `bytes` kept as the real size and the marker counting what is missing from the whole
    /// output. Idempotent: what already fits, or was cut to fit, comes back as it is.
    pub fn recut(self) -> Self {
        let fits = self.text.len() <= Self::MAX_BYTES
            || (self.truncated && self.text.len() <= Self::MAX_BYTES + Self::MARKER_SLACK);
        if fits {
            return self;
        }
        Self::cut(&self.text, self.bytes, Self::MAX_BYTES)
    }

    /// `text` (which stands for a whole output of `bytes`) cut to `limit`.
    fn cut(text: &str, bytes: u64, limit: usize) -> Self {
        if text.len() <= limit {
            return Self {
                text: text.to_owned(),
                bytes,
                truncated: bytes > text.len() as u64,
            };
        }
        let half = limit / 2;
        let mut head_end = half;
        while !text.is_char_boundary(head_end) {
            head_end -= 1;
        }
        let mut tail_start = text.len() - half;
        while !text.is_char_boundary(tail_start) {
            tail_start += 1;
        }
        let kept = (head_end + text.len() - tail_start) as u64;
        let cut = bytes.saturating_sub(kept);
        let mut out = String::with_capacity(limit + 40);
        out.push_str(&text[..head_end]);
        if !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&format!("… {cut} bytes cut …\n"));
        out.push_str(&text[tail_start..]);
        Self {
            text: out,
            bytes,
            truncated: true,
        }
    }
}

#[cfg(test)]
mod tool_output_tests {
    use super::ToolOutput;

    #[test]
    fn empty_is_kept_whole() {
        let o = ToolOutput::from_text("");
        assert_eq!((o.text.as_str(), o.bytes, o.truncated), ("", 0, false));
        assert!(ToolOutput::from_text_opt("").is_none());
    }

    #[test]
    fn exact_limit_is_not_cut() {
        let s = "a".repeat(ToolOutput::MAX_BYTES);
        let o = ToolOutput::from_text(&s);
        assert!(!o.truncated);
        assert_eq!(o.text, s);
        let o = ToolOutput::from_text(&format!("{s}b"));
        assert!(o.truncated);
        assert_eq!(o.bytes, ToolOutput::MAX_BYTES as u64 + 1);
    }

    #[test]
    fn multibyte_at_the_cut_stays_valid() {
        for pad in 0..4 {
            let s = format!("{}{}", "x".repeat(pad), "é€😀".repeat(8000));
            let o = ToolOutput::from_text(&s);
            assert!(o.truncated);
            assert_eq!(o.bytes, s.len() as u64);
            assert!(o.text.contains("bytes cut"));
            assert!(o.text.len() <= ToolOutput::MAX_BYTES + 40);
        }
    }

    #[test]
    fn huge_keeps_head_and_tail() {
        let s = format!("HEAD{}TAIL", "z".repeat(10_000_000));
        let o = ToolOutput::from_text(&s);
        assert!(o.truncated);
        assert_eq!(o.bytes, s.len() as u64);
        assert!(o.text.starts_with("HEAD"));
        assert!(o.text.ends_with("TAIL"));
        assert!(o.text.len() <= ToolOutput::MAX_BYTES + 40);
        let cut: usize = o
            .text
            .lines()
            .find_map(|l| l.strip_prefix("… ")?.strip_suffix(" bytes cut …"))
            .unwrap()
            .parse()
            .unwrap();
        assert!(cut > 9_900_000);
    }

    fn marker(text: &str) -> usize {
        text.lines()
            .find_map(|l| l.strip_prefix("… ")?.strip_suffix(" bytes cut …"))
            .unwrap()
            .parse()
            .unwrap()
    }

    #[test]
    fn a_raw_output_keeps_up_to_a_mebibyte_and_recuts_to_the_journal_limit() {
        let small = "a".repeat(ToolOutput::RAW_MAX_BYTES);
        let o = ToolOutput::from_raw(&small);
        assert!(!o.truncated);
        assert_eq!(o.text.len(), ToolOutput::RAW_MAX_BYTES);

        let s = format!("HEAD{}TAIL", "z".repeat(5_000_000));
        let raw = ToolOutput::from_raw(&s);
        assert!(raw.truncated && raw.text.len() <= ToolOutput::RAW_MAX_BYTES + 40);
        assert_eq!(raw.bytes, s.len() as u64);

        let journal = raw.recut();
        assert!(journal.truncated);
        assert_eq!(journal.bytes, s.len() as u64);
        assert!(journal.text.starts_with("HEAD") && journal.text.ends_with("TAIL"));
        assert!(journal.text.len() <= ToolOutput::MAX_BYTES + 40);
        // The marker counts what is missing from the whole output, not from the raw cut.
        assert!(marker(&journal.text) > 4_980_000);
        assert_eq!(journal, ToolOutput::from_text(&s));
    }

    #[test]
    fn recut_is_idempotent_and_leaves_a_small_output_alone() {
        let small = ToolOutput::from_raw("hello");
        assert_eq!(small.clone().recut(), small);
        let s = "é€😀".repeat(100_000);
        let once = ToolOutput::from_raw(&s).recut();
        assert!(once.text.len() <= ToolOutput::MAX_BYTES + 40);
        assert_eq!(once.clone().recut(), once);
        assert_eq!(ToolOutput::from_text(&s).recut(), ToolOutput::from_text(&s));
    }

    #[test]
    fn a_redacted_whole_output_between_the_limits_is_cut_down() {
        let o = ToolOutput {
            text: "b".repeat(100_000),
            bytes: 100_000,
            truncated: false,
        };
        let r = o.recut();
        assert!(r.truncated && r.text.len() <= ToolOutput::MAX_BYTES + 40);
        assert_eq!(r.bytes, 100_000);
    }
}
