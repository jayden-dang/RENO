//! The command line, as `clap` sees it.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};
use reno_proto::{ApprovalId, RunId, Seq};

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
    /// List pending approvals.
    #[command(subcommand)]
    Approval(ApprovalCommand),
    /// Approve one pending action.
    Approve(Decide),
    /// Deny one pending action.
    Deny(Decide),
}

#[derive(Debug, Subcommand)]
pub(crate) enum DaemonCommand {
    /// Start `renod` in the background and wait until it answers.
    Start,
    /// Ask the daemon to stop and wait until it has.
    Stop,
    /// Show whether the daemon is running. Exits 1 when it is not.
    Status,
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
    /// The executor to run on.
    #[arg(long, default_value = "mock")]
    pub(crate) executor: String,
    /// The working directory [default: the current directory].
    #[arg(long, value_name = "DIR")]
    pub(crate) cwd: Option<PathBuf>,
    /// The model to ask for.
    #[arg(long)]
    pub(crate) model: Option<String>,
    /// What to do.
    pub(crate) prompt: String,
}

#[derive(Debug, Subcommand)]
pub(crate) enum RunCommand {
    /// List runs, newest first.
    List(RunList),
    /// Follow a run's transcript from a sequence number (the start by default), until it ends.
    Tail(RunTail),
    /// Stop a run.
    Cancel(RunCancel),
}

#[derive(Debug, Args)]
pub(crate) struct RunList {
    /// Show at most this many runs.
    #[arg(long)]
    pub(crate) limit: Option<u32>,
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
pub(crate) struct RunCancel {
    /// The run.
    pub(crate) run: RunId,
}

#[derive(Debug, Subcommand)]
pub(crate) enum ApprovalCommand {
    /// List the actions waiting for a decision.
    List,
}

#[derive(Debug, Args)]
pub(crate) struct Decide {
    /// The approval, as `reno approval list` shows it.
    pub(crate) approval: ApprovalId,
}
