//! `renod`: the Reno daemon. Arguments, logging, the runtime and signals; everything else is in
//! the library.

use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::{Context, bail};
use clap::Parser;
use renod::defs::{self, DEFS_DIR_ENV};
use renod::paths::{self, DATA_DIR_ENV, SOCKET_ENV, ensure_private_dir};
use renod::{Daemon, DaemonConfig, ExecutorSource, Paths, logging};
use tokio::signal::unix::{SignalKind, signal};

/// The allocator is chosen by measurement (ARCHITECTURE 17.8): the `mimalloc` feature selects v2.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// The worker threads of the runtime (ARCHITECTURE 17.2).
const WORKER_THREADS: usize = 2;
/// The blocking pool: store opens and journal replay, nothing else.
const BLOCKING_THREADS: usize = 8;

/// The Reno daemon.
#[derive(Debug, Parser)]
#[command(name = "renod", version, about)]
struct Args {
    /// Where the daemon keeps its files [default: the platform's data directory for Reno].
    #[arg(long, env = DATA_DIR_ENV, value_name = "DIR")]
    data_dir: Option<PathBuf>,
    /// The socket to serve on [default: reno.sock in the data directory].
    #[arg(long, env = SOCKET_ENV, value_name = "PATH")]
    socket: Option<PathBuf>,
    /// Where the bots, triggers and policy are kept, as TOML files in a git repository
    /// [default: ~/.reno]. An example bot and the default policy are written there on first start,
    /// when it holds no definitions yet.
    #[arg(long, env = DEFS_DIR_ENV, value_name = "DIR")]
    defs_dir: Option<PathBuf>,
    /// Leave the starting terminal's session, as `reno daemon start` does.
    #[arg(long, hide = true)]
    detach: bool,
    /// Make the mock executor wait to be steered or cancelled, for tests and demos.
    #[arg(long, env = "RENO_MOCK_HOLD", hide = true, value_parser = clap::builder::BoolishValueParser::new())]
    mock_hold: bool,
    /// Register only the mock executor: look for no harness CLI, for tests.
    #[arg(long, env = "RENO_NO_HARNESSES", hide = true, value_parser = clap::builder::BoolishValueParser::new())]
    no_harnesses: bool,
    /// How many runs may be live at once [default: the kernel's own], for benchmarks.
    #[arg(long, env = "RENO_MAX_CONCURRENT", hide = true, value_name = "N")]
    max_concurrent: Option<usize>,
}

#[allow(clippy::print_stderr)]
fn main() -> ExitCode {
    match run(Args::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // The logs may not be open yet, and a terminal user wants to see this.
            eprintln!("renod: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> anyhow::Result<()> {
    let data_dir = match args.data_dir {
        Some(dir) => dir,
        None => paths::default_data_dir()
            .context("cannot find a data directory: set --data-dir, or HOME")?,
    };
    let defs_dir = match args.defs_dir {
        Some(dir) => dir,
        None => defs::default_defs_dir()
            .context("cannot find the definitions directory: set --defs-dir, or HOME")?,
    };
    let paths = Paths::new(data_dir, args.socket).with_defs_dir(defs_dir);

    if args.detach {
        // A fresh session, so the terminal's hangup never reaches the daemon. It fails only when
        // this process already leads a group, in which case it is already detached enough.
        let _ = rustix::process::setsid();
    }

    ensure_private_dir(&paths.data_dir)
        .with_context(|| format!("cannot prepare {}", paths.data_dir.display()))?;
    let logs = paths.logs();
    ensure_private_dir(&logs).with_context(|| format!("cannot prepare {}", logs.display()))?;
    let _logs = logging::init(&logs)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKER_THREADS)
        .max_blocking_threads(BLOCKING_THREADS)
        .thread_name("renod")
        .enable_all()
        .build()
        .context("cannot start the async runtime")?;
    let outcome = runtime.block_on(serve(
        paths,
        args.mock_hold,
        !args.no_harnesses,
        args.max_concurrent,
    ));
    if let Err(error) = &outcome {
        tracing::error!(%error, "renod failed");
    }
    outcome
}

async fn serve(
    paths: Paths,
    mock_hold: bool,
    harnesses: bool,
    max_concurrent: Option<usize>,
) -> anyhow::Result<()> {
    let mut config = DaemonConfig::new(
        paths,
        ExecutorSource::Discover {
            hold_mock: mock_hold,
            harnesses,
        },
    );
    if let Some(limit) = max_concurrent {
        config.kernel.max_concurrent = limit;
    }
    let daemon = Daemon::start(config).await?;

    let mut terminate = signal(SignalKind::terminate()).context("cannot watch SIGTERM")?;
    let mut interrupt = signal(SignalKind::interrupt()).context("cannot watch SIGINT")?;
    tokio::select! {
        _ = terminate.recv() => tracing::info!("SIGTERM"),
        _ = interrupt.recv() => tracing::info!("SIGINT"),
        () = daemon.shutdown_requested().cancelled() => {}
    }
    daemon.request_shutdown();

    let stopped = daemon.wait().await?;
    if stopped.kernel_aborted || stopped.server_aborted {
        bail!("shut down past a deadline: {stopped:?}");
    }
    Ok(())
}
