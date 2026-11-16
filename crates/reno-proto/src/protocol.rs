//! The typed client protocol: LSP-style `Method`, `Notification` and `Subscription` traits and
//! the Phase 0 surface (ARCHITECTURE sections 10 and 17.5).
//!
//! This module is vocabulary only. It has no framing, dispatch or transport; JSON-RPC lives in
//! `reno-rpc` (ARCH-4). Changes within a [`PROTOCOL_VERSION`] are additive only (ARCH-13):
//! optional fields, new methods, new enum variants gated by a feature string.
//!
//! A subscription is started by calling a method named [`Subscription::NAME`] with
//! [`Subscription::Params`]. Each item then arrives in a `sub.item` frame (owned by `reno-rpc`)
//! whose payload is [`Subscription::Item`]. [`SubGap`] is the one subscription-level
//! notification that carries Reno vocabulary.

use std::path::PathBuf;

use schemars::{JsonSchema, SchemaGenerator};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::budget::{
    BudgetPauseParams, BudgetPauseResult, BudgetResumeParams, BudgetResumeResult,
    BudgetStatusParams, BudgetStatusResult,
};
use crate::defs::{
    BotDef, BotPin, DefGetParams, DefListParams, DefRecord, DefValidateParams, DefValidateResult,
    DefsSnapshot, DefsWatchParams, PolicyDef, TriggerDef,
};
use crate::exec::SessionRef;
use crate::exec::{ApprovalSubject, ExecEvent, ResolvedBy};
use crate::health::{
    ExecutorInstallParams, ExecutorInstallResult, ExecutorListResult, ExecutorRefreshParams,
    ExecutorsSnapshot, ExecutorsWatchParams,
};
use crate::ids::{ApprovalId, ExecutorId, RunId, Seq, SubscriptionId, TaskId};
use crate::routing::{
    RoutePreviewParams, RoutePreviewResult, RoutingDecision, RunRoutingParams, RunRoutingResult,
};
use crate::run::{NotResumed, RunStatus, StopDetail, StopReason};
use crate::secrets::{
    SecretDeleteParams, SecretDeleteResult, SecretListResult, SecretSetParams, SecretSetResult,
};
use crate::triggers::{
    TriggerFireParams, TriggerFireResult, TriggerListResult, TriggersSnapshot, TriggersWatchParams,
};
use crate::workspace::{
    WorkspaceCleanParams, WorkspaceCleanResult, WorkspaceGetParams, WorkspaceInfo,
    WorkspaceListParams, WorkspaceListResult,
};

/// The wire protocol version. Bumped only for a breaking change.
pub const PROTOCOL_VERSION: u32 = 1;

/// Records `approval.history` returns when `limit` is absent.
pub const APPROVAL_HISTORY_DEFAULT_LIMIT: u32 = 50;
/// The most records one `approval.history` call returns; a larger `limit` is cut to this.
pub const APPROVAL_HISTORY_MAX_LIMIT: u32 = 500;

/// Characters of a prompt a list or watch summary carries (`run.get` has the whole prompt).
pub const PROMPT_SUMMARY_CHARS: usize = 280;
/// Runs `run.list` returns when `limit` is absent.
pub const RUN_LIST_DEFAULT_LIMIT: u32 = 100;
/// The most runs one `run.list` page returns; a larger `limit` is cut to this.
pub const RUN_LIST_MAX_LIMIT: u32 = 500;

/// Feature: `run.events` honours `fromSeq`.
pub const FEATURE_RUN_EVENTS_RESUME: &str = "run.events.resume";
/// Feature: the `runs.watch` subscription exists.
pub const FEATURE_RUNS_WATCH: &str = "runs.watch";
/// Feature: the daemon sends `sub.gap` to slow event subscribers instead of buffering.
pub const FEATURE_SUB_GAP: &str = "sub.gap";
/// Feature: `approval.list`, `approval.decide` and the `approvals.watch` subscription exist.
pub const FEATURE_APPROVALS: &str = "approvals";

/// Feature: `def.list`, `def.get`, `def.validate` and the `defs.watch` subscription exist, and
/// `task.run` accepts `bot`.
pub const FEATURE_DEFS: &str = "defs";

/// Feature: `trigger.list`, `trigger.fire` and the `triggers.watch` subscription exist.
pub const FEATURE_TRIGGERS: &str = "triggers";

/// Feature: `workspace.list`, `workspace.get` and `workspace.clean` exist.
pub const FEATURE_WORKSPACES: &str = "workspaces";

/// Feature: `approval.history` exists, and journals may carry `ExecEvent::ApprovalResolved`.
pub const FEATURE_APPROVAL_HISTORY: &str = "approval.history";

/// Feature: `run.steer` exists, and journals may carry `ExecEvent::UserMessage`.
pub const FEATURE_RUN_STEER: &str = "run.steer";
/// Feature: `executor.list`, `executor.refresh` and the `executors.watch` subscription exist.
pub const FEATURE_EXECUTORS: &str = "executors";
/// Feature: `executor.install` exists.
pub const FEATURE_EXECUTOR_INSTALL: &str = "executor.install";
/// Feature: `approval.decide` takes a `message` with a denial, which reaches the harness and is
/// journalled and shown in `approval.history`.
pub const FEATURE_APPROVAL_MESSAGE: &str = "approval.message";
/// Feature: `daemon.shutdown` exists.
pub const FEATURE_DAEMON_SHUTDOWN: &str = "daemon.shutdown";

/// Feature: `route.preview` and `run.routing` exist, and `task.run` answers with its routing
/// decision and accepts `inPlace`.
pub const FEATURE_ROUTING: &str = "routing";

/// Feature: `task.run` accepts `attachments`.
pub const FEATURE_ATTACHMENTS: &str = "attachments";
/// Feature: `budget.status`, `budget.pause` and `budget.resume` exist, and `task.run` may be
/// refused with the `BudgetExceeded` error code.
pub const FEATURE_BUDGET: &str = "budget";

/// Feature: `secret.list`, `secret.set` and `secret.delete` exist, and a bot's `secrets` reach
/// its runs as environment variables.
pub const FEATURE_SECRETS: &str = "secrets";
/// Feature: `run.resume` exists, journals may carry `ExecEvent::Resumed`, and a run that was
/// interrupted may be resumed by the daemon itself.
pub const FEATURE_RUN_RESUME: &str = "run.resume";
/// Feature: `ExecEvent::Done` carries a typed `reason` and `RunSummary` a typed `stop`, so a
/// client never reads the diagnostic's text to learn why a run stopped.
pub const FEATURE_STOP_DETAIL: &str = "run.stop_detail";

/// Every feature string defined at [`PROTOCOL_VERSION`].
pub const FEATURES: &[&str] = &[
    FEATURE_RUN_EVENTS_RESUME,
    FEATURE_RUNS_WATCH,
    FEATURE_SUB_GAP,
    FEATURE_APPROVALS,
    FEATURE_APPROVAL_HISTORY,
    FEATURE_RUN_STEER,
    FEATURE_DAEMON_SHUTDOWN,
    FEATURE_DEFS,
    FEATURE_TRIGGERS,
    FEATURE_WORKSPACES,
    FEATURE_EXECUTORS,
    FEATURE_EXECUTOR_INSTALL,
    FEATURE_APPROVAL_MESSAGE,
    FEATURE_ROUTING,
    FEATURE_ATTACHMENTS,
    FEATURE_BUDGET,
    crate::agent::FEATURE_AGENT,
    FEATURE_RUN_RESUME,
    FEATURE_STOP_DETAIL,
    FEATURE_SECRETS,
];

/// A request with a single reply.
pub trait Method {
    /// Dot-namespaced method name, for example `run.get`.
    const NAME: &'static str;
    /// Request params.
    type Params: Serialize + DeserializeOwned + JsonSchema;
    /// Successful reply.
    type Result: Serialize + DeserializeOwned + JsonSchema;
}

/// A one-way message with no reply.
pub trait Notification {
    /// Dot-namespaced notification name.
    const NAME: &'static str;
    /// Notification params.
    type Params: Serialize + DeserializeOwned + JsonSchema;
}

/// A stream of items started by a request.
pub trait Subscription {
    /// Dot-namespaced name of the method that starts the subscription.
    const NAME: &'static str;
    /// Params of the starting request.
    type Params: Serialize + DeserializeOwned + JsonSchema;
    /// One streamed item.
    type Item: Serialize + DeserializeOwned + JsonSchema;
}

/// Empty params: serialises as `{}`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct NoParams {}

/// One run, as shown in lists and watches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunSummary {
    /// The run.
    pub run_id: RunId,
    /// The task it attempts.
    pub task_id: TaskId,
    /// The executor running it.
    pub executor: ExecutorId,
    /// Current lifecycle state.
    pub status: RunStatus,
    /// Attempt number, from 1.
    pub attempt: u32,
    /// First line of the prompt, for display.
    pub title: String,
    /// The prompt the task was submitted with. Absent from daemons that predate it. In
    /// `run.list` and `runs.watch` it is cut to [`PROMPT_SUMMARY_CHARS`] characters, ending in
    /// `…` when cut; `run.get` returns it whole.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// The model the run was asked for, or the one the executor reported once it started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Why it stopped, once it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<StopReason>,
    /// The typed reason behind the run's last stop, when the daemon knows one: a budget, a
    /// refused start, a cancel, a shutdown, a crash. Needs feature `run.stop_detail`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop: Option<StopDetail>,
    /// Why an `interrupted` run was not resumed, once the daemon has looked. Absent while the
    /// run may still be resumed, and for any run that is not interrupted. Needs feature
    /// `run.resume`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_resumed: Option<NotResumed>,
    /// The harness session of the run, once its executor has reported one: what a resume
    /// continues. Needs feature `run.resume`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<SessionRef>,
    /// The bot the run was submitted for, and the version of its definition the run is pinned
    /// to. Absent for a run submitted without a bot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot: Option<BotPin>,
    /// Creation time, in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// Last state change, in milliseconds since the Unix epoch.
    pub updated_at_ms: u64,
}

/// Params of `daemon.hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HelloParams {
    /// Client name, for logs.
    pub client_name: String,
    /// Highest protocol version the client speaks.
    pub protocol_version: u32,
    /// Features the client understands.
    pub features: Vec<String>,
}

/// Result of `daemon.hello`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HelloResult {
    /// Daemon build version.
    pub server_version: String,
    /// Protocol version the connection will use.
    pub protocol_version: u32,
    /// Features both sides support.
    pub features: Vec<String>,
    /// Daemon process id.
    pub pid: u32,
}

/// Result of `daemon.status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct DaemonStatusInfo {
    /// Daemon build version.
    pub version: String,
    /// Daemon process id.
    pub pid: u32,
    /// Seconds since the daemon started.
    pub uptime_secs: u64,
    /// Runs in a non-terminal state other than `queued`.
    pub active_runs: u32,
    /// Runs waiting to be claimed.
    pub queued_runs: u32,
    /// Executors the daemon found installed.
    pub executors: Vec<ExecutorId>,
}

/// Result of `daemon.shutdown`: empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DaemonShutdownResult {}

/// Params of `task.run`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TaskRunParams {
    /// The task prompt.
    pub prompt: String,
    /// Executor to run it on. May be left out when `bot` is given: the daemon then takes the
    /// first executor in the bot's list that it has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor: Option<ExecutorId>,
    /// The bot to run it as. The run is pinned to the version of the bot's definition that is
    /// current when the task is submitted. Needs feature `defs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot: Option<String>,
    /// Working directory; the daemon's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<PathBuf>,
    /// Model override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Let a write run work directly in `cwd` when it is not a git repository, as a bot's
    /// `inPlaceWrites` does. Without it such a run is refused, because there is no worktree to
    /// isolate it in. A git repository is never written in place, whatever this says. Needs
    /// feature `routing`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub in_place: bool,
    /// Files the task refers to. The daemon checks that each is a readable file, within
    /// [`ATTACHMENTS_MAX_COUNT`] and [`ATTACHMENT_MAX_BYTES`], and appends an "Attached files:"
    /// list of their paths to the prompt; the executor reads them from disk. Needs feature
    /// `attachments`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<Attachment>,
}

/// A file attached to a task: a path on the daemon's machine, with the name to show for it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Attachment {
    /// Absolute path of a readable regular file.
    pub path: PathBuf,
    /// The name to show: usually the file name.
    pub name: String,
    /// The media type, when known (`image/png`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
}

/// The most files one task may attach.
pub const ATTACHMENTS_MAX_COUNT: usize = 10;
/// The largest file one task may attach, in bytes.
pub const ATTACHMENT_MAX_BYTES: u64 = 25 * 1024 * 1024;

impl Attachment {
    /// The prompt text the daemon appends for `attachments`: a blank line, "Attached files:",
    /// and one `- path (name, mime)` line each. Empty when there are none.
    pub fn prompt_suffix(attachments: &[Self]) -> String {
        if attachments.is_empty() {
            return String::new();
        }
        let mut text = String::from("\n\nAttached files:\n");
        for a in attachments {
            text.push_str(&format!("- {}", a.path.display()));
            match &a.mime {
                Some(mime) => text.push_str(&format!(" ({}, {mime})\n", a.name)),
                None => text.push_str(&format!(" ({})\n", a.name)),
            }
        }
        text
    }
}

/// Result of `task.run`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TaskRunResult {
    /// The new run.
    pub run_id: RunId,
    /// Where the router sent it and why. Absent from daemons that predate routing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<RoutingDecision>,
}

/// Params of `run.get`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunGetParams {
    /// The run to fetch.
    pub run_id: RunId,
}

/// Params of `run.list`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunListParams {
    /// Only runs in one of these states; all runs when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Vec<RunStatus>>,
    /// Maximum number of runs, newest first. Defaults to [`RUN_LIST_DEFAULT_LIMIT`] and is cut
    /// to [`RUN_LIST_MAX_LIMIT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Cursor: only runs older than this one. Pass the previous page's `next`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<RunId>,
}

/// Result of `run.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunListResult {
    /// Matching runs, newest first.
    pub runs: Vec<RunSummary>,
    /// Pass as `before` to get the next page; absent on the last page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next: Option<RunId>,
}

/// Params of `run.cancel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunCancelParams {
    /// The run to cancel.
    pub run_id: RunId,
    /// Why, for the audit trail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Result of `run.cancel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunCancelResult {
    /// The run's status once the cancel was accepted.
    pub status: RunStatus,
}

/// Params of `run.resume`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunResumeParams {
    /// The run to resume: `interrupted`, or `failed` with a session.
    pub run_id: RunId,
    /// What to tell the harness when the session continues; the daemon's standard "continue"
    /// prompt when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
}

/// Result of `run.resume`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunResumeResult {
    /// The run's status once it was queued again (`queued`).
    pub status: RunStatus,
    /// The attempt it will be, from 2.
    pub attempt: u32,
}

/// The `data` of the `Conflict` error `run.resume` answers with when the run cannot be resumed.
/// Read it with `serde_json::from_value`; the message is for people.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct NotResumable {
    /// The run.
    pub run_id: RunId,
    /// Why it cannot be resumed.
    pub why: NotResumed,
}

/// Params of `run.steer`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunSteerParams {
    /// The live run to steer.
    pub run_id: RunId,
    /// The steering message, delivered at the executor's next step or turn boundary.
    pub prompt: String,
}

/// Result of `run.steer`: empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RunSteerResult {}

/// Params of the `run.events` subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunEventsParams {
    /// The run to follow.
    pub run_id: RunId,
    /// First sequence number to deliver, **inclusive** — the value a `sub.gap` names as
    /// `resumeFrom`. A run's journal numbers its events from 1, so `0` and an absent value both
    /// start from the beginning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_seq: Option<Seq>,
}

/// One item of the `run.events` subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunEventItem {
    /// Position in the run's journal, counted from 1.
    pub seq: Seq,
    /// The event.
    pub event: ExecEvent,
}

/// Params of the `runs.watch` subscription.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunsWatchParams {
    /// Only runs in one of these states; all runs when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Vec<RunStatus>>,
}

/// One item of the `runs.watch` subscription: the whole current list (latest value wins). The
/// first item is always the current list, sent immediately on subscribe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RunsSnapshot {
    /// Current runs, newest first.
    pub runs: Vec<RunSummary>,
}

/// An action a harness is held on, waiting for a human decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Approval {
    /// The approval, as `approval.decide` refers to it.
    pub id: ApprovalId,
    /// The run that is held.
    pub run_id: RunId,
    /// Exactly what is being approved.
    pub subject: ApprovalSubject,
    /// When the action was held, in milliseconds since the Unix epoch.
    pub requested_at_ms: u64,
}

/// A human's answer to an [`Approval`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    /// Allow this one action; the next similar action asks again.
    ApproveOnce,
    /// Refuse the action.
    Deny,
}

/// Result of `approval.list`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalListResult {
    /// Pending approvals, oldest first.
    pub approvals: Vec<Approval>,
}

/// Params of `approval.decide`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalDecideParams {
    /// The approval to answer.
    pub approval_id: ApprovalId,
    /// The answer.
    pub decision: ApprovalDecision,
    /// Why, for a denial: it reaches the harness (as a rule's message does), is journalled in
    /// `ApprovalResolved.message` and shows in `approval.history`. Refused with an approval.
    /// At most [`APPROVAL_MESSAGE_MAX_CHARS`] characters; blank counts as absent. Needs feature
    /// `approval.message`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The longest `message` of `approval.decide`, in characters.
pub const APPROVAL_MESSAGE_MAX_CHARS: usize = 1000;

/// Result of `approval.decide`: empty.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalDecideResult {}

/// Where an approval record stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    /// Waiting for a human.
    Pending,
    /// The harness was told yes.
    Approved,
    /// The harness was told no.
    Denied,
    /// Nobody decided in time, or the run ended or the daemon restarted first. The harness, if
    /// it was still there, was told no.
    Expired,
}

/// The stored record of one approval request: what was asked, and how it was resolved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRecord {
    /// The approval's id.
    pub id: ApprovalId,
    /// The run that raised it.
    pub run_id: RunId,
    /// The harness's id for the request (the `requestId` of the `NeedsApproval` event).
    pub request_id: String,
    /// Exactly what was asked.
    pub subject: ApprovalSubject,
    /// Where it stands.
    pub status: ApprovalStatus,
    /// Who or what decided. Absent while the approval is pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_by: Option<ResolvedBy>,
    /// When it was raised, in milliseconds since the Unix epoch.
    pub created_at_ms: u64,
    /// When it was decided. Absent while pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decided_at_ms: Option<u64>,
    /// The deadline after which an undecided approval is denied as expired.
    pub expires_at_ms: u64,
    /// The reason a human gave with a denial. Only daemons that negotiate `approval.message`
    /// fill it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Params of `approval.history`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalHistoryParams {
    /// Only this run's approvals; every run's when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<RunId>,
    /// Most records, newest decision first. Defaults to [`APPROVAL_HISTORY_DEFAULT_LIMIT`] and
    /// is cut to [`APPROVAL_HISTORY_MAX_LIMIT`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

/// Result of `approval.history`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalHistoryResult {
    /// Decided approvals (never pending ones), newest decision first.
    pub approvals: Vec<ApprovalRecord>,
}

/// Params of the `approvals.watch` subscription.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ApprovalsWatchParams {}

/// One item of the `approvals.watch` subscription: the whole pending list (latest value wins).
/// The first item is the list as it stands when the subscription starts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalsSnapshot {
    /// Pending approvals, oldest first.
    pub approvals: Vec<Approval>,
}

/// Params of the `sub.gap` notification (ARCH-12).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SubGapParams {
    /// The subscription that fell behind and was cut off.
    pub subscription: SubscriptionId,
    /// Re-subscribe with `fromSeq` set to this to continue without loss.
    pub resume_from: Seq,
}

macro_rules! method {
    ($(#[$meta:meta])* $ty:ident, $name:literal, $params:ty => $result:ty) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy)]
        pub struct $ty;

        impl Method for $ty {
            const NAME: &'static str = $name;
            type Params = $params;
            type Result = $result;
        }
    };
}

macro_rules! subscription {
    ($(#[$meta:meta])* $ty:ident, $name:literal, $params:ty => $item:ty) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy)]
        pub struct $ty;

        impl Subscription for $ty {
            const NAME: &'static str = $name;
            type Params = $params;
            type Item = $item;
        }
    };
}

method!(
    /// `daemon.hello`: negotiates protocol version and features. Must be the first request.
    DaemonHello, "daemon.hello", HelloParams => HelloResult
);
method!(
    /// `daemon.status`: daemon health and load.
    DaemonStatus, "daemon.status", NoParams => DaemonStatusInfo
);
method!(
    /// `daemon.shutdown`: asks the daemon to stop. It answers once shutdown has begun, and the
    /// connection may close right after. Needs feature `daemon.shutdown`.
    DaemonShutdown, "daemon.shutdown", NoParams => DaemonShutdownResult
);
method!(
    /// `task.run`: creates a task and its first run.
    TaskRun, "task.run", TaskRunParams => TaskRunResult
);
method!(
    /// `run.get`: one run's summary.
    RunGet, "run.get", RunGetParams => RunSummary
);
method!(
    /// `run.routing`: the routing decision stored for a run, with its fallback steps. Needs
    /// feature `routing`.
    RunRouting, "run.routing", RunRoutingParams => RunRoutingResult
);
method!(
    /// `route.preview`: the decision a task would get now, without creating anything. Needs
    /// feature `routing`.
    RoutePreview, "route.preview", RoutePreviewParams => RoutePreviewResult
);
method!(
    /// `budget.status`: every bot's limits and what it has used today, and whether anything is
    /// paused. Needs feature `budget`.
    BudgetStatus, "budget.status", BudgetStatusParams => BudgetStatusResult
);
method!(
    /// `budget.pause`: the kill switch. Stops new runs from being claimed (everything, or one
    /// bot), holds that bot's triggers, and with `stopLive` stops the live runs. The pause
    /// survives a restart. Needs feature `budget`.
    BudgetPause, "budget.pause", BudgetPauseParams => BudgetPauseResult
);
method!(
    /// `budget.resume`: lifts a pause. Needs feature `budget`.
    BudgetResume, "budget.resume", BudgetResumeParams => BudgetResumeResult
);
method!(
    /// `run.list`: runs, optionally filtered by status.
    RunList, "run.list", RunListParams => RunListResult
);
method!(
    /// `run.cancel`: asks a run to stop.
    RunCancel, "run.cancel", RunCancelParams => RunCancelResult
);
method!(
    /// `run.resume`: queues an `interrupted` run, or a `failed` one that has a session, to continue
    /// its harness session. Refused with `Conflict` and a [`NotResumed`] in `data` when it cannot
    /// be. Needs feature `run.resume`.
    RunResume, "run.resume", RunResumeParams => RunResumeResult
);
method!(
    /// `run.steer`: sends a steering message to a live run. Needs feature `run.steer`.
    RunSteer, "run.steer", RunSteerParams => RunSteerResult
);
method!(
    /// `def.list`: the current definitions and any load errors. Needs feature `defs`.
    DefList, "def.list", DefListParams => DefsSnapshot
);
method!(
    /// `def.get`: one version of one definition, with its body. Needs feature `defs`.
    DefGet, "def.get", DefGetParams => DefRecord
);
method!(
    /// `def.validate`: checks a body against every rule without saving it. Needs feature
    /// `defs`.
    DefValidate, "def.validate", DefValidateParams => DefValidateResult
);
method!(
    /// `trigger.list`: every trigger and its state. Needs feature `triggers`.
    TriggerList, "trigger.list", NoParams => TriggerListResult
);
method!(
    /// `trigger.fire`: fires a manual trigger, or any enabled one, now. Needs feature
    /// `triggers`.
    TriggerFire, "trigger.fire", TriggerFireParams => TriggerFireResult
);
method!(
    /// `workspace.list`: the worktrees write runs work in. Needs feature `workspaces`.
    WorkspaceList, "workspace.list", WorkspaceListParams => WorkspaceListResult
);
method!(
    /// `workspace.get`: one run's workspace. Needs feature `workspaces`.
    WorkspaceGet, "workspace.get", WorkspaceGetParams => WorkspaceInfo
);
method!(
    /// `workspace.clean`: removes workspaces that hold nothing to lose (or, with `force`, any).
    /// Needs feature `workspaces`.
    WorkspaceClean, "workspace.clean", WorkspaceCleanParams => WorkspaceCleanResult
);
method!(
    /// `secret.list`: the names of the secrets in the Keychain and of those bots ask for, and
    /// whether each is set. Never a value. Needs feature `secrets`.
    SecretList, "secret.list", NoParams => SecretListResult
);
method!(
    /// `secret.set`: stores a value under a name, straight into the Keychain. The one place a
    /// value crosses the wire, and only toward the daemon. Needs feature `secrets`.
    SecretSet, "secret.set", SecretSetParams => SecretSetResult
);
method!(
    /// `secret.delete`: removes a secret from the Keychain. Needs feature `secrets`.
    SecretDelete, "secret.delete", SecretDeleteParams => SecretDeleteResult
);
method!(
    /// `executor.list`: every executor with its descriptor and last health. Needs feature
    /// `executors`.
    ExecutorList, "executor.list", NoParams => ExecutorListResult
);
method!(
    /// `executor.refresh`: probes one executor (or all) again and answers with the result. The
    /// only calls that probe are this, the daemon's start, and a run that failed to start
    /// because its harness was not installed or not signed in. Needs feature `executors`.
    ExecutorRefresh, "executor.refresh", ExecutorRefreshParams => ExecutorListResult
);
method!(
    /// `executor.install`: fetches and verifies the managed adapter (or pinned release archive)
    /// of an executor whose last probe said it needs one, probes it again and answers with the
    /// result. The one place a harness is installed: a run never installs. Needs feature
    /// `executor.install`.
    ExecutorInstall, "executor.install", ExecutorInstallParams => ExecutorInstallResult
);
method!(
    /// `approval.list`: the pending approvals. Needs feature `approvals`.
    ApprovalList, "approval.list", NoParams => ApprovalListResult
);
method!(
    /// `approval.decide`: answers one pending approval. Needs feature `approvals`.
    ApprovalDecide, "approval.decide", ApprovalDecideParams => ApprovalDecideResult
);
method!(
    /// `approval.history`: decided approvals, who decided each and by which rule. Needs feature
    /// `approval.history`.
    ApprovalHistory, "approval.history", ApprovalHistoryParams => ApprovalHistoryResult
);

subscription!(
    /// `run.events`: a run's journal from a sequence number, then live.
    RunEvents, "run.events", RunEventsParams => RunEventItem
);
subscription!(
    /// `runs.watch`: the current run list, replaced on every change.
    RunsWatch, "runs.watch", RunsWatchParams => RunsSnapshot
);
subscription!(
    /// `defs.watch`: the current definitions and load errors, replaced on every change. Needs
    /// feature `defs`.
    DefsWatch, "defs.watch", DefsWatchParams => DefsSnapshot
);
subscription!(
    /// `triggers.watch`: every trigger and its state, replaced on every change. Needs feature
    /// `triggers`.
    TriggersWatch, "triggers.watch", TriggersWatchParams => TriggersSnapshot
);
subscription!(
    /// `executors.watch`: every executor and its last health, replaced on every change. Needs
    /// feature `executors`.
    ExecutorsWatch, "executors.watch", ExecutorsWatchParams => ExecutorsSnapshot
);
subscription!(
    /// `approvals.watch`: the pending approvals, replaced on every change. Needs feature
    /// `approvals`.
    ApprovalsWatch, "approvals.watch", ApprovalsWatchParams => ApprovalsSnapshot
);

/// `sub.gap`: a slow subscriber was cut off; resume from `resumeFrom`.
#[derive(Debug, Clone, Copy)]
pub struct SubGap;

impl Notification for SubGap {
    const NAME: &'static str = "sub.gap";
    type Params = SubGapParams;
}

fn schema_of<T: JsonSchema>() -> Value {
    SchemaGenerator::default()
        .into_root_schema_for::<T>()
        .to_value()
}

fn method_entry<M: Method>() -> Value {
    json!({
        "name": M::NAME,
        "params": schema_of::<M::Params>(),
        "result": schema_of::<M::Result>(),
    })
}

fn notification_entry<N: Notification>() -> Value {
    json!({ "name": N::NAME, "params": schema_of::<N::Params>() })
}

fn subscription_entry<S: Subscription>() -> Value {
    json!({
        "name": S::NAME,
        "params": schema_of::<S::Params>(),
        "item": schema_of::<S::Item>(),
    })
}

/// The whole wire contract as JSON: protocol version, features, and for every method,
/// notification and subscription its name and the JSON Schemas of its params and result or item,
/// and the schemas of the definition files (the `body` of `def.get` and `def.validate`).
/// CI snapshots this value (ARCH-13). Register every new protocol type here.
pub fn protocol_schema() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "features": FEATURES,
        "methods": [
            method_entry::<DaemonHello>(),
            method_entry::<DaemonStatus>(),
            method_entry::<DaemonShutdown>(),
            method_entry::<TaskRun>(),
            method_entry::<RunGet>(),
            method_entry::<RunRouting>(),
            method_entry::<RoutePreview>(),
            method_entry::<BudgetStatus>(),
            method_entry::<BudgetPause>(),
            method_entry::<BudgetResume>(),
            method_entry::<RunList>(),
            method_entry::<RunCancel>(),
            method_entry::<RunResume>(),
            method_entry::<RunSteer>(),
            method_entry::<ApprovalList>(),
            method_entry::<ApprovalDecide>(),
            method_entry::<ApprovalHistory>(),
            method_entry::<DefList>(),
            method_entry::<DefGet>(),
            method_entry::<DefValidate>(),
            method_entry::<TriggerList>(),
            method_entry::<TriggerFire>(),
            method_entry::<WorkspaceList>(),
            method_entry::<WorkspaceGet>(),
            method_entry::<WorkspaceClean>(),
            method_entry::<SecretList>(),
            method_entry::<SecretSet>(),
            method_entry::<SecretDelete>(),
            method_entry::<ExecutorList>(),
            method_entry::<ExecutorRefresh>(),
            method_entry::<ExecutorInstall>(),
            method_entry::<crate::agent::AgentAuth>(),
            method_entry::<crate::agent::AgentReport>(),
            method_entry::<crate::agent::AgentRequestApproval>(),
            method_entry::<crate::agent::AgentBoardRead>(),
            method_entry::<crate::agent::AgentBoardWrite>(),
        ],
        "notifications": [
            notification_entry::<SubGap>(),
        ],
        "definitions": {
            "bot": schema_of::<BotDef>(),
            "trigger": schema_of::<TriggerDef>(),
            "policy": schema_of::<PolicyDef>(),
        },
        "subscriptions": [
            subscription_entry::<RunEvents>(),
            subscription_entry::<RunsWatch>(),
            subscription_entry::<ApprovalsWatch>(),
            subscription_entry::<DefsWatch>(),
            subscription_entry::<TriggersWatch>(),
            subscription_entry::<ExecutorsWatch>(),
        ],
    })
}
