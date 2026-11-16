//! `reno`: the command line for the Reno daemon.
//!
//! It knows the daemon only through `reno-client` (ARCH-2): every command is a few client calls
//! and some printing. With `--json` every command prints machine-readable output; streams print
//! one JSON object per line.
//!
//! Exit codes: 0 on success (for `task run` and `run tail`, only when the run completed), 1 when
//! the command failed or the run did not complete, 2 for a command line that does not parse.

mod cli;
mod commands;
mod daemon;
mod defs;
mod env;
mod follow;
mod policy;
mod render;

use std::io;
use std::process::ExitCode;

use clap::Parser;
use reno_proto::ApprovalDecision;

use crate::cli::{Cli, Command};
use crate::env::Env;

#[allow(clippy::print_stderr)]
fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => code,
        Err(error) if is_broken_pipe(&error) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("reno: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    let env = Env::from_cli(&cli)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(dispatch(&env, cli.command))
}

async fn dispatch(env: &Env, command: Command) -> anyhow::Result<ExitCode> {
    match command {
        Command::Daemon(command) => daemon::run(env, command).await,
        Command::Task(command) => commands::task(env, command).await,
        Command::Run(command) => commands::run(env, command).await,
        Command::Approval(command) => commands::approval(env, command).await,
        Command::Trigger(command) => commands::trigger(env, command).await,
        Command::Workspace(command) => commands::workspace(env, command).await,
        Command::Defs(command) => defs::run(env, command).await,
        Command::Policy(command) => policy::run(env, command),
        Command::Approve(args) => commands::decide(env, args, ApprovalDecision::ApproveOnce).await,
        Command::Deny(args) => commands::decide(env, args, ApprovalDecision::Deny).await,
    }
}

/// Output piped into `head` and the like: the reader leaving is not a failure.
fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<io::Error>())
        .any(|e| e.kind() == io::ErrorKind::BrokenPipe)
}
