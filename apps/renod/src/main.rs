//! `renod`: the Reno daemon. Arguments, logging, the runtime and signals; everything else is in
//! the library.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use anyhow::{Context, bail};
use clap::Parser;
use reno_harness::ClaudeExecutor;
use reno_harness::exec::Executor;
use reno_kernel::ExecutorRegistry;
use renod::defs::{self, DEFS_DIR_ENV};
use renod::paths::{self, DATA_DIR_ENV, SOCKET_ENV, ensure_private_dir};
use renod::{
    Daemon, DaemonConfig, ExecutorSet, ExecutorSource, Paths, executors, logging, secrets,
};
use tokio::signal::unix::{SignalKind, signal};

/// The allocator is chosen by measurement (ARCHITECTURE 17.8): the `mimalloc` feature selects v2.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// The worker threads of the runtime (ARCHITECTURE 17.2).
const WORKER_THREADS: usize = 2;
/// The blocking pool: store opens and journal replay, nothing else.
const BLOCKING_THREADS: usize = 8;

fn parse_stream_mock(value: &str) -> Result<executors::StreamMock, String> {
    match value {
        "steady" => Ok(executors::StreamMock::Steady),
        "burst" => Ok(executors::StreamMock::Burst),
        "longreply" => Ok(executors::StreamMock::LongReply),
        "findburst" => Ok(executors::StreamMock::FindBurst),
        other => Err(format!(
            "want steady, burst, longreply or findburst, got {other}"
        )),
    }
}

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
    /// Bench only: make the mock stream text at a steady rate (`steady`, the default: about 20
    /// events a second for 60 s) or play 2,000 messages at once (`burst`). Used by the app
    /// benchmark; it wins over `--mock-hold`. Other bench scripts are `longreply` and `findburst`.
    #[arg(
        long,
        env = "RENO_STREAM_MOCK",
        hide = true,
        value_name = "KIND",
        num_args = 0..=1,
        default_missing_value = "steady",
        require_equals = true,
        value_parser = parse_stream_mock
    )]
    stream_mock: Option<executors::StreamMock>,
    /// Register only the mock executor: look for no harness CLI, for tests.
    #[arg(long, env = "RENO_NO_HARNESSES", hide = true, value_parser = clap::builder::BoolishValueParser::new())]
    no_harnesses: bool,
    /// Register only the mock and Claude Code, the latter driven through this program: for tests
    /// that need a run with a real child process, and for pinning a binary. Looks for no other CLI.
    #[arg(long, env = "RENO_CLAUDE_EXECUTABLE", hide = true, value_name = "PATH")]
    claude_executable: Option<PathBuf>,
    /// Keep secrets in memory instead of the Keychain, for tests: they vanish when the daemon
    /// stops, and no real Keychain is touched.
    #[arg(long, env = "RENO_MEMORY_SECRETS", hide = true, value_parser = clap::builder::BoolishValueParser::new())]
    memory_secrets: bool,
    /// How many runs may be live at once [default: the kernel's own], for benchmarks.
    #[arg(long, env = "RENO_MAX_CONCURRENT", hide = true, value_name = "N")]
    max_concurrent: Option<usize>,
    /// The `reno` binary Reno's own MCP server runs [default: `reno` next to this program].
    #[arg(long, env = "RENO_BIN", hide = true, value_name = "PATH")]
    reno_bin: Option<PathBuf>,
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
    // A launcher that points our stderr at a file only ever appends to it.
    logging::bound_stderr_file(std::io::stderr(), logging::STDERR_LIMIT);
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
        (args.mock_hold, args.stream_mock),
        !args.no_harnesses,
        args.claude_executable,
        args.max_concurrent,
        args.memory_secrets,
        args.reno_bin,
    ));
    if let Err(error) = &outcome {
        tracing::error!(%error, "renod failed");
    }
    outcome
}

async fn serve(
    paths: Paths,
    (mock_hold, stream_mock): (bool, Option<executors::StreamMock>),
    harnesses: bool,
    claude_executable: Option<PathBuf>,
    max_concurrent: Option<usize>,
    memory_secrets: bool,
    reno_bin: Option<PathBuf>,
) -> anyhow::Result<()> {
    let executors = match claude_executable {
        Some(program) => ExecutorSource::Given(pinned_claude(mock_hold, program)?),
        None => ExecutorSource::Discover {
            hold_mock: mock_hold,
            stream_mock,
            harnesses,
        },
    };
    let mut config = DaemonConfig::new(paths, executors);
    config.system_wake = true;
    // The one place the real runner and the real settings files are wired. Tests never get them.
    if harnesses {
        config.maintenance = renod::real_maintenance();
        config.pty = Some(std::sync::Arc::new(renod::terminals::RealPty::new()));
        config.terminal_env = std::env::vars_os().collect();
    }
    let keychain: Arc<dyn secrets::Keychain> = if memory_secrets {
        Arc::new(secrets::MemoryKeychain::new())
    } else {
        secrets::login_keychain()
    };
    config.secrets = Some(Arc::new(secrets::KeychainSecrets::new(keychain)));
    config.reno_bin = reno_bin;
    if harnesses {
        // Each engine keeps the agents added from the registry in its own data directory, and
        // asks the registry's own address (a staging daemon may point `RENO_ACP_REGISTRY_INDEX`
        // elsewhere; it must still be https).
        reno_harness::acp_registry::set_agents_root(config.paths.data_dir.join("agents"));
        config.registry_index_url = Some(
            std::env::var("RENO_ACP_REGISTRY_INDEX")
                .ok()
                .filter(|url| !url.is_empty())
                .unwrap_or_else(|| reno_harness::acp_registry::INDEX_URL.to_owned()),
        );
    }
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

/// The mock, and Claude Code through `program`.
fn pinned_claude(mock_hold: bool, program: PathBuf) -> anyhow::Result<ExecutorSet> {
    let claude: Arc<dyn Executor> = Arc::new(ClaudeExecutor::new().with_executable(program));
    let registry = ExecutorRegistry::new()
        .with(Arc::new(executors::mock_executor(mock_hold)?))
        .with(claude.clone());
    Ok(ExecutorSet {
        registry,
        harnesses: vec![claude],
    })
}
