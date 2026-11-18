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
