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
mod files;
mod follow;
mod history;
mod mcp;
mod policy;
mod registry;
mod render;
mod rule;
mod secret;
mod term;

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
    // `reno mcp` is started by Reno with its own environment: it needs neither a data directory
    // nor a home, and must not start a daemon.
    if let Command::Mcp(args) = cli.command {
        return mcp::run(args, cli.socket);
    }
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
        Command::Pause(args) => commands::pause(env, args).await,
        Command::Resume(args) => commands::resume(env, args).await,
        Command::Budget => commands::budget(env).await,
        Command::Workspace(command) => commands::workspace(env, command).await,
        Command::Thread(command) => commands::thread(env, command).await,
        Command::Fs(command) => files::fs(env, command).await,
        Command::Git(command) => files::git(env, command).await,
        Command::Term(command) => term::run(env, command).await,
        Command::Executor(command) => commands::executor(env, command).await,
        Command::Defs(command) => defs::run(env, command).await,
        Command::Secret(command) => secret::run(env, command).await,
        Command::Rule(command) => rule::run(env, command).await,
        Command::Policy(command) => policy::run(env, command),
        // Served before `Env` exists, in `run`.
        Command::Mcp(_) => Err(anyhow::anyhow!(
            "reno mcp is served before commands are dispatched"
        )),
        Command::Approve(args) => {
            commands::decide(env, args, None, ApprovalDecision::ApproveOnce).await
        }
        Command::Deny(args) => {
            commands::decide(env, args.decide, args.message, ApprovalDecision::Deny).await
        }
    }
}

/// Output piped into `head` and the like: the reader leaving is not a failure.
fn is_broken_pipe(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<io::Error>())
        .any(|e| e.kind() == io::ErrorKind::BrokenPipe)
}
