//! The command line, as `clap` sees it.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use reno_proto::defs::DefKind;
use reno_proto::{ApprovalId, ExecutorId, RunId, Seq, WorkspaceStatus};

/// Talk to the Reno daemon.
#[derive(Debug, Parser)]
#[command(name = "reno", version, about, arg_required_else_help = true)]
pub(crate) struct Cli {
    /// Where the daemon keeps its files [default: the platform's data directory for Reno].
    #[arg(long, global = true, env = "RENO_DATA_DIR", value_name = "DIR")]
    pub(crate) data_dir: Option<PathBuf>,
    /// The daemon's socket [default: reno.sock in the data directory].
    #[arg(long, global = true, env = "RENO_SOCKET", value_name = "PATH")]
    pub(crate) socket: Option<PathBuf>,
    /// Print machine-readable JSON instead of text. Streams print one JSON object per line.
    #[arg(long, global = true)]
    pub(crate) json: bool,
    #[command(subcommand)]
    pub(crate) command: Command,
}

#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Start, stop and inspect the daemon.
    #[command(subcommand)]
    Daemon(DaemonCommand),
    /// Run a task.
    #[command(subcommand)]
    Task(TaskCommand),
    /// List, follow and cancel runs.
    #[command(subcommand)]
    Run(RunCommand),
    /// List pending approvals and the history of decided ones.
    #[command(subcommand)]
    Approval(ApprovalCommand),
    /// Check what the policy would do with an action.
    #[command(subcommand)]
    Policy(PolicyCommand),
    /// List, show and check bots, triggers and the policy.
    #[command(subcommand)]
    Defs(DefsCommand),
    /// List triggers and their state, and fire one now.
    #[command(subcommand)]
    Trigger(TriggerCommand),
    /// List the git worktrees write runs work in, and clean them up.
    #[command(subcommand)]
    Workspace(WorkspaceCommand),
    /// List the executors (the agent CLIs Reno drives) with what the daemon found out about
    /// each, and ask it to look again.
    #[command(subcommand)]
    Executor(ExecutorCommand),
    /// List, set and delete the secrets bots may ask for. Values live in the macOS Keychain,
    /// never in a definition file, and are never shown.
    #[command(subcommand)]
    Secret(SecretCommand),
    /// The kill switch: stop new runs from starting, for everything or one bot. Triggers are
    /// held and catch up on `reno resume`. Persists across daemon restarts.
    Pause(Pause),
    /// Lift a pause started with `reno pause`.
    Resume(Resume),
    /// Show each bot's limits and what it has used today, and what is paused.
    Budget,
    /// Reno's own MCP server on stdio, for one run. Reno starts it when it launches a run; it is
    /// not for people.
    Mcp(crate::mcp::McpArgs),
    /// List and revoke the standing rules: answers you kept (`reno approve --remember bot`) so
    /// the same action is not asked again.
    #[command(subcommand)]
    Rule(RuleCommand),
    /// Approve one pending action.
    Approve(Decide),
    /// Deny one pending action.
    Deny(Decide),
}

#[derive(Debug, Subcommand)]
pub(crate) enum DaemonCommand {
    /// Start `renod` in the background and wait until it answers. Does nothing when it is
    /// already running. With a LaunchAgent installed, asks launchd to start it instead.
    Start,
    /// Ask the daemon to stop and wait until it has.
    Stop,
    /// Show whether the daemon is running. Exits 1 when it is not.
    Status,
    /// Stop the daemon if it is running, then start it.
    Restart,
    /// Show the daemon's log: the last lines, and with --follow the lines as they are written.
    Logs(DaemonLogs),
    /// Install a LaunchAgent that starts the daemon at login and restarts it after a crash.
    ///
    /// Writes ~/Library/LaunchAgents/dev.reno.renod.plist, stops a daemon started by hand so
    /// that launchd owns it, and loads the agent with `launchctl bootstrap gui/<uid>`.
    Install(DaemonInstall),
    /// Unload the LaunchAgent and delete its plist. This stops the daemon.
    Uninstall(DaemonInstall),
}

#[derive(Debug, Args)]
pub(crate) struct Pause {
    /// Pause only this bot [default: everything].
    #[arg(long, value_name = "NAME")]
    pub(crate) bot: Option<String>,
    /// Also stop the runs that are live now: each ends `interrupted`. Without it they finish.
    #[arg(long)]
    pub(crate) stop_live: bool,
}

#[derive(Debug, Args)]
pub(crate) struct Resume {
    /// Resume only this bot's own pause [default: the global pause].
    #[arg(long, value_name = "NAME")]
    pub(crate) bot: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct DaemonLogs {
    /// Keep printing new lines as they are written, until interrupted.
    #[arg(short, long)]
    pub(crate) follow: bool,
    /// How many of the last lines to show first.
    #[arg(short = 'n', long, default_value_t = 50, value_name = "N")]
    pub(crate) lines: usize,
}

#[derive(Debug, Args)]
pub(crate) struct DaemonInstall {
    /// Print what would be written and run, and change nothing.
    #[arg(long)]
    pub(crate) dry_run: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum TaskCommand {
    /// Run a prompt and stream its transcript until it ends. Exits 0 only if the run completed.
    ///
    /// With --json the first line is {"runId": ...}, then one {"seq", "event"} object per line.
    Run(TaskRun),
}

#[derive(Debug, Args)]
pub(crate) struct TaskRun {
    /// The executor to run on. Without it, and without --bot, the daemon tries its default
    /// executors in order (Claude Code, then Codex; only the mock when no real harness is
    /// installed) and falls back to the next when one is signed out or missing; other executors
    /// (ACP agents, the mock beside real harnesses) are reached only by naming them or through a
    /// bot. With --bot, the bot's executors in order. Naming one means no fallback to another.
    #[arg(long)]
    pub(crate) executor: Option<String>,
    /// Run as this bot: the run is pinned to the version of the bot's definition in force now.
    #[arg(long)]
    pub(crate) bot: Option<String>,
    /// The working directory [default: the current directory].
    #[arg(long, value_name = "DIR")]
    pub(crate) cwd: Option<PathBuf>,
    /// The model to ask for.
    #[arg(long)]
    pub(crate) model: Option<String>,
    /// Allow a write run to work directly in a directory that is not a git repository (as a
    /// bot's `in_place_writes` does). A git repository is never written in place.
    #[arg(long)]
    pub(crate) in_place: bool,
    /// What to do.
    pub(crate) prompt: String,
}

#[derive(Debug, Subcommand)]
pub(crate) enum RunCommand {
    /// List runs, newest first.
    List(RunList),
    /// Follow a run's transcript from a sequence number (the start by default), until it ends.
    Tail(RunTail),
    /// Show one run, and why the router chose its executor: the candidates it kept and dropped,
    /// the rule that matched and any fallback steps.
    Show(RunShow),
    /// Stop a run.
    Cancel(RunCancel),
    /// Continue an interrupted run (or a failed one that has a session) in its harness session.
    /// The daemon already does this on its own after a restart when the run's executor and bot
    /// allow it; this is for the ones it left alone.
    Resume(RunResume),
}

#[derive(Debug, Args)]
pub(crate) struct RunList {
    /// Show at most this many runs, newest first.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..))]
    pub(crate) limit: u32,
    /// Show every run, fetched a page at a time. Overrides `--limit`.
    #[arg(long)]
    pub(crate) all: bool,
}

#[derive(Debug, Args)]
pub(crate) struct RunShow {
    /// The run.
    pub(crate) run: RunId,
    /// Name every executor routing passed over, not just the ones with a reason about the
    /// executor itself. `--json` always has the whole decision.
    #[arg(long)]
    pub(crate) verbose: bool,
}

#[derive(Debug, Args)]
pub(crate) struct RunTail {
    /// The run.
    pub(crate) run: RunId,
    /// First event to show, counted from 1. Inclusive.
    #[arg(long, value_name = "SEQ")]
    pub(crate) from: Option<Seq>,
}

#[derive(Debug, Args)]
pub(crate) struct RunResume {
    /// The run.
    pub(crate) run: RunId,
    /// What to tell the harness as the session continues; the daemon's standard "continue"
    /// prompt by default.
    #[arg(long)]
    pub(crate) prompt: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct RunCancel {
    /// The run.
    pub(crate) run: RunId,
}

#[derive(Debug, Subcommand)]
pub(crate) enum TriggerCommand {
    /// List every trigger: when it fires next, what it last did and whether it is backed off.
    List,
    /// Fire a trigger now: a manual trigger, or any enabled one.
    Fire(TriggerFire),
}

#[derive(Debug, Args)]
pub(crate) struct TriggerFire {
    /// The trigger, as `reno trigger list` shows it.
    pub(crate) name: String,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ExecutorCommand {
    /// List every executor: whether it is installed and at which version, whether that version
    /// is one the adapter was verified on, whether it is signed in, and which models it offers.
    List,
    /// Probe one executor (or all) again, wait for the answer and show it. Nothing probes on a
    /// timer: run this after installing or signing in.
    Refresh(ExecutorRefresh),
    /// Fetch and verify the managed adapter of an executor that `reno executor list` says needs
    /// one, wait for the probe that follows and show the result. The only step that installs:
    /// runs never do. It can take a while, and needs the network.
    Install(ExecutorInstall),
    /// Show the frames each executor's wire layer did not recognise since the daemon started,
    /// by kind, and when each kind was first seen. A rising number is the early sign that a
    /// harness CLI moved: see docs/KEEPING-UP.md.
    Frames(ExecutorFrames),
}

#[derive(Debug, Args)]
pub(crate) struct ExecutorFrames {
    /// The executor, as `reno executor list` shows it [default: all of them].
    pub(crate) id: Option<ExecutorId>,
}

#[derive(Debug, Args)]
pub(crate) struct ExecutorInstall {
    /// The executor, as `reno executor list` shows it.
    pub(crate) id: ExecutorId,
}

#[derive(Debug, Args)]
pub(crate) struct ExecutorRefresh {
    /// The executor, as `reno executor list` shows it [default: all of them].
    pub(crate) id: Option<ExecutorId>,
}

#[derive(Debug, Subcommand)]
pub(crate) enum SecretCommand {
    /// List the secrets that are set and those a bot asks for, and which bots ask. Never prints
    /// a value.
    List,
    /// Store a secret in the Keychain. The value is read from a prompt that does not echo, or
    /// from standard input with --from-stdin; it is never an argument, so it stays out of the
    /// shell history and the process list.
    Set(SecretSet),
    /// Remove a secret from the Keychain.
    Delete(SecretDelete),
}

#[derive(Debug, Args)]
pub(crate) struct SecretSet {
    /// The secret's name: upper-case letters, digits and `_`, starting with a letter, such as
    /// GITHUB_TOKEN. A bot lists the names it wants in `secrets`.
    pub(crate) name: String,
    /// Read the value from standard input (all of it, less one final newline) instead of
    /// prompting.
    #[arg(long)]
    pub(crate) from_stdin: bool,
}

#[derive(Debug, Args)]
pub(crate) struct SecretDelete {
    /// The secret's name.
    pub(crate) name: String,
}

#[derive(Debug, Subcommand)]
pub(crate) enum WorkspaceCommand {
    /// List workspaces, newest first: a write run works in its own git worktree on the branch
    /// `reno/<run>`, never in your checkout.
    List(WorkspaceList),
    /// Show one run's workspace: where it is, which branch, what it was cut from, what it holds.
    Show(WorkspaceShow),
    /// Remove workspaces that hold nothing to lose. A workspace with unmerged commits or
    /// uncommitted changes is kept unless --force.
    ///
    /// Give one run, or --merged (workspaces whose branch is merged into their repository's
    /// current HEAD), or --all-clean (workspaces with no commits and no uncommitted changes).
    Clean(WorkspaceClean),
}

#[derive(Debug, Args)]
pub(crate) struct WorkspaceList {
    /// Only workspaces in this status: preparing, active, kept, removed or lost. Repeatable.
    #[arg(long, value_name = "STATUS", value_parser = parse_workspace_status)]
    pub(crate) status: Vec<WorkspaceStatus>,
}

fn parse_workspace_status(text: &str) -> Result<WorkspaceStatus, String> {
    WorkspaceStatus::ALL
        .into_iter()
        .find(|status| crate::render::wire_name(status) == text)
        .ok_or_else(|| "expected one of preparing, active, kept, removed, lost".to_owned())
}

#[derive(Debug, Args)]
pub(crate) struct WorkspaceShow {
    /// The run.
    pub(crate) run: RunId,
}

#[derive(Debug, Args)]
#[command(group(clap::ArgGroup::new("target").required(true).args(["run", "merged", "all_clean"])))]
pub(crate) struct WorkspaceClean {
    /// Clean this run's workspace.
    pub(crate) run: Option<RunId>,
    /// Clean every kept workspace whose branch is merged into its repository's HEAD.
    #[arg(long)]
    pub(crate) merged: bool,
    /// Clean every workspace that has no commits and no uncommitted changes.
    #[arg(long)]
    pub(crate) all_clean: bool,
    /// Remove even a workspace with unmerged commits or uncommitted changes, deleting its
    /// branch. This is the only way to lose work.
    #[arg(long)]
    pub(crate) force: bool,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ApprovalCommand {
    /// List the actions waiting for a decision.
    List,
    /// Show decided approvals, newest first: who decided each, and by which rule.
    History(ApprovalHistory),
}

#[derive(Debug, Args)]
pub(crate) struct ApprovalHistory {
    /// Only this run's approvals.
    #[arg(long)]
    pub(crate) run: Option<RunId>,
    /// Show at most this many.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..))]
    pub(crate) limit: u32,
}

#[derive(Debug, Subcommand)]
pub(crate) enum PolicyCommand {
    /// Evaluate an action against `policy.toml` and print the decision and the rule. Needs no
    /// daemon.
    ///
    /// The action is a JSON approval subject, for example
    /// '{"action":"command","tool":"bash","command":"cargo test"}'. A subject's paths are judged
    /// against --cwd, and a rule's scope against --bot. Exits 0 whatever the decision; it fails
    /// only when it cannot read the policy or the subject.
    Check(PolicyCheck),
}

#[derive(Debug, Args)]
pub(crate) struct PolicyCheck {
    /// The approval subject, as JSON: `action` (read, edit, command, network, mcp_tool or other),
    /// `tool`, and optionally `command` and `paths`.
    pub(crate) subject: String,
    /// Evaluate as a run of this bot (for rules with a `scope`).
    #[arg(long)]
    pub(crate) bot: Option<String>,
    /// The run's worktree [default: the current directory].
    #[arg(long, value_name = "DIR")]
    pub(crate) cwd: Option<PathBuf>,
    /// The definitions directory [default: $RENO_DEFS_DIR, or ~/.reno].
    #[arg(long, env = "RENO_DEFS_DIR", value_name = "DIR")]
    pub(crate) dir: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub(crate) struct Decide {
    /// The approval, as `reno approval list` shows it.
    pub(crate) approval: ApprovalId,
    /// Keep this answer as a standing rule for this run, this run's bot, or everything. The rule
    /// holds exactly this action (this tool, this command line, these paths), nothing broader.
    /// Without it the answer is for this one request.
    #[arg(long, value_enum, value_name = "SCOPE")]
    pub(crate) remember: Option<RememberArg>,
    /// With --remember: the rule stops applying after this many seconds [default: never].
    #[arg(long, value_name = "SECONDS", requires = "remember")]
    pub(crate) expires_in: Option<u64>,
}

/// How far a remembered answer reaches.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub(crate) enum RememberArg {
    /// This run.
    Run,
    /// Every run of this run's bot.
    Bot,
    /// Every run.
    Always,
}

#[derive(Debug, Subcommand)]
pub(crate) enum RuleCommand {
    /// List the standing rules in force, newest first.
    List(RuleList),
    /// Stop a standing rule applying, for good.
    Revoke(RuleRevoke),
}

#[derive(Debug, Args)]
pub(crate) struct RuleList {
    /// Also list rules that were revoked or have expired.
    #[arg(long)]
    pub(crate) all: bool,
}

#[derive(Debug, Args)]
pub(crate) struct RuleRevoke {
    /// The rule, as `reno rule list` shows it.
    pub(crate) id: reno_proto::RuleId,
}

#[derive(Debug, Subcommand)]
pub(crate) enum DefsCommand {
    /// List the definitions in force, with their versions, and any errors in the files.
    List(DefsList),
    /// Show one version of a definition.
    Show(DefsShow),
    /// Validate the definition files and print every error. Needs no daemon.
    ///
    /// Exits 0 when the files are valid, 1 when they are not.
    Check(DefsCheck),
}

#[derive(Debug, Args)]
pub(crate) struct DefsList {
    /// Only this kind: bot, trigger or policy.
    #[arg(long)]
    pub(crate) kind: Option<DefKind>,
}

#[derive(Debug, Args)]
pub(crate) struct DefsShow {
    /// The kind: bot, trigger or policy.
    pub(crate) kind: DefKind,
    /// The name; `policy` for the policy.
    pub(crate) name: String,
    /// The version [default: the newest].
    #[arg(long, value_name = "N")]
    pub(crate) version: Option<u32>,
}

#[derive(Debug, Args)]
pub(crate) struct DefsCheck {
    /// The definitions directory [default: $RENO_DEFS_DIR, or ~/.reno].
    #[arg(long, env = "RENO_DEFS_DIR", value_name = "DIR")]
    pub(crate) dir: Option<PathBuf>,
    /// An executor id to accept besides the ones Reno builds (mock, claude-code, codex).
    #[arg(long = "executor", value_name = "ID")]
    pub(crate) executors: Vec<ExecutorId>,
}
