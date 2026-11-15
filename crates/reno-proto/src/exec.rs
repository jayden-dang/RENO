//! The execution contract (ARCHITECTURE section 4.2): what a run is asked to do, the events it
//! yields, and what an executor declares it can do. Data only; the `Executor` trait lives in
//! `reno-harness`.

use std::collections::BTreeMap;
use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ids::{ExecutorId, RunId};
use crate::run::StopReason;

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

/// The sandbox the harness itself enforces around its tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SandboxLevel {
    /// No writes.
    ReadOnly,
    /// Writes confined to the workspace.
    WorkspaceWrite,
    /// No sandbox.
    Unrestricted,
}

/// Requested reasoning effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    /// Fastest, least reasoning.
    Low,
    /// Balanced default.
    Medium,
    /// Most reasoning.
    High,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
}

/// A tool invocation as far as Reno needs to know it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    /// Tool name as the harness reports it.
    pub name: String,
    /// The tool's input, verbatim.
    #[serde(default)]
    pub input: serde_json::Value,
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
    },
    /// Token usage so far.
    Usage {
        /// Input tokens.
        input_tokens: u64,
        /// Output tokens.
        output_tokens: u64,
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
    /// The run ended; always the last event.
    Done {
        /// Why it stopped.
        stop: StopReason,
        /// Final result text.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        /// Failure detail, when `stop` is `error`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diagnostic: Option<String>,
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
    /// Sandbox levels the executor actually enforces.
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
