//! `reno daemon start | stop | status | restart | logs | install | uninstall`.
//!
//! Starting, the LaunchAgent and the log files are `reno_client::launcher`, shared with the app;
//! this file is the commands' words and the waiting.
//!
//! `start` and `stop` wait for another process to come up or go away, which has no event to
//! follow, so they look again every 50 ms up to a deadline. Following a log is event-driven:
//! it wakes when the file system reports a change in the log directory.

use std::path::Path;
use std::process::{Child, ExitCode};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use notify::{RecursiveMode, Watcher};
use reno_client::launcher::logs::{recent_trouble, render_line, tail_lines_before};
use reno_client::launcher::{LABEL, Launcher, Started};
use reno_client::{Client, ClientError};
use reno_proto::DaemonStatusInfo;
use rustix::io::Errno;
use rustix::process::{Pid, test_kill_process};
use serde::Serialize;

use crate::cli::{DaemonCommand, DaemonInstall, DaemonLogs};
use crate::env::{Env, print_json, print_line};
use crate::follow::Follower;
use crate::render;

/// How long `daemon start` waits for the daemon to answer.
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// How long `daemon stop` waits for the process to be gone: the kernel's own shutdown deadline
/// is 15 seconds, so a daemon that is still working is given longer than that.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
/// How often start and stop look again. They wait on another process; there is no event to follow.
const LOOK_AGAIN: Duration = Duration::from_millis(50);
/// How many log lines an error about a failed start quotes.
const TROUBLE_LINES: usize = 8;

pub(crate) async fn run(env: &Env, command: DaemonCommand) -> anyhow::Result<ExitCode> {
    match command {
        DaemonCommand::Start => start(env).await,
        DaemonCommand::Stop => stop(env).await,
        DaemonCommand::Status => status(env).await,
        DaemonCommand::Restart => restart(env).await,
        DaemonCommand::Logs(args) => logs(env, &args).await,
        DaemonCommand::Install(args) => install(env, &args).await,
        DaemonCommand::Uninstall(args) => uninstall(env, &args),
    }
}

// ---- start -------------------------------------------------------------------------------

/// How a daemon was started, for the output.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
enum Via {
    Detached,
    Launchd,
}

/// What `start` found or did.
struct Up {
    started: bool,
    pid: u32,
    via: Option<Via>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StartedOut<'a> {
    started: bool,
    pid: u32,
    socket: &'a Path,
    #[serde(skip_serializing_if = "Option::is_none")]
    via: Option<Via>,
}

/// Makes sure a daemon is answering: it is, or it is started and waited for.
async fn ensure_up(env: &Env) -> anyhow::Result<Up> {
    if let Some(client) = env.try_connect().await? {
        return Ok(Up {
            started: false,
            pid: client.daemon().pid,
            via: None,
        });
    }
    let launcher = env.launcher()?;
    match launcher.start()? {
        Started::Spawned(mut child) => {
            let pid = wait_up(env, &launcher, Some(&mut child)).await?;
            Ok(Up {
                started: true,
                pid,
                via: Some(Via::Detached),
            })
        }
        Started::Launchd => {
            let pid = wait_up(env, &launcher, None).await?;
            Ok(Up {
                started: true,
                pid,
                via: Some(Via::Launchd),
            })
        }
    }
}

/// Waits until the daemon answers `daemon.hello`, and returns its pid. Fails when the child
/// we started exits first, or the deadline passes, with the reason from the daemon's logs.
async fn wait_up(
    env: &Env,
    launcher: &Launcher,
    mut child: Option<&mut Child>,
) -> anyhow::Result<u32> {
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match Client::connect_unix(&env.socket).await {
            Ok(client) => return Ok(client.daemon().pid),
            Err(error @ ClientError::Incompatible { .. }) => return Err(env.explain(error)),
            // Not listening yet, or still starting up.
            Err(_) => {}
        }
        if let Some(child) = child.as_deref_mut()
            && let Some(status) = child.try_wait().context("cannot watch renod")?
        {
            bail!(
                "renod exited ({status}) before it answered{}",
                why(launcher)
            );
        }
        if Instant::now() >= deadline {
            bail!(
                "renod did not answer within {} seconds{}",
                START_TIMEOUT.as_secs(),
                why(launcher)
            );
        }
        tokio::time::sleep(LOOK_AGAIN).await;
    }
}

/// The last log lines and where the logs are, as the tail of an error message.
fn why(launcher: &Launcher) -> String {
    let logs = launcher.logs();
    let trouble = recent_trouble(&logs, TROUBLE_LINES);
    let mut text = String::new();
    if !trouble.is_empty() {
        text.push_str("; the last lines of its log:\n");
        for line in trouble.lines() {
            text.push_str("  ");
            text.push_str(line);
            text.push('\n');
        }
        text.push_str("see ");
    } else {
        text.push_str("; see ");
    }
    text.push_str(&logs.dir.display().to_string());
    text
}

async fn start(env: &Env) -> anyhow::Result<ExitCode> {
    let up = ensure_up(env).await?;
    report_started(env, &up)
}

fn report_started(env: &Env, up: &Up) -> anyhow::Result<ExitCode> {
    if env.json {
        print_json(&StartedOut {
            started: up.started,
            pid: up.pid,
            socket: &env.socket,
            via: up.via,
        })?;
    } else if up.started {
        let how = match up.via {
            Some(Via::Launchd) => " through launchd",
            _ => "",
        };
        print_line(format_args!("started renod (pid {}){how}", up.pid))?;
    } else {
        print_line(format_args!("renod is already running (pid {})", up.pid))?;
    }
    Ok(ExitCode::SUCCESS)
}

// ---- stop and restart --------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Stopped {
    stopped: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
}

/// Asks the daemon to stop and waits until the process and its socket are gone. `None` when it
/// was not running.
async fn stop_daemon(env: &Env) -> anyhow::Result<Option<u32>> {
    let Some(client) = env.try_connect().await? else {
        return Ok(None);
    };
    let pid = client.daemon().pid;
    // The daemon answers, then closes its connections; either outcome means it heard us. What
    // counts is that the process goes away.
    match client.shutdown_daemon().await {
        Ok(()) | Err(ClientError::Disconnected) => {}
        Err(error) => return Err(env.explain(error)),
    }
    drop(client);

    let deadline = Instant::now() + STOP_TIMEOUT;
    while process_exists(pid) || env.socket.exists() {
        if Instant::now() >= deadline {
            bail!(
                "renod (pid {pid}) is still shutting down after {} seconds",
                STOP_TIMEOUT.as_secs()
            );
        }
        tokio::time::sleep(LOOK_AGAIN).await;
    }
    Ok(Some(pid))
}

async fn stop(env: &Env) -> anyhow::Result<ExitCode> {
    let pid = stop_daemon(env).await?;
    if env.json {
        print_json(&Stopped {
            stopped: pid.is_some(),
            pid,
        })?;
    } else if let Some(pid) = pid {
        print_line(format_args!("stopped renod (pid {pid})"))?;
    } else {
        print_line("renod is not running")?;
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Restarted {
    #[serde(skip_serializing_if = "Option::is_none")]
    stopped_pid: Option<u32>,
    pid: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    via: Option<Via>,
}

async fn restart(env: &Env) -> anyhow::Result<ExitCode> {
    let stopped_pid = stop_daemon(env).await?;
    let up = ensure_up(env).await?;
    if env.json {
        print_json(&Restarted {
            stopped_pid,
            pid: up.pid,
            via: up.via,
        })?;
    } else {
        match stopped_pid {
            Some(old) => print_line(format_args!("restarted renod (pid {old} -> {})", up.pid))?,
            None => print_line(format_args!(
                "renod was not running; started it (pid {})",
                up.pid
            ))?,
        }
    }
    Ok(ExitCode::SUCCESS)
}

// ---- status ------------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusOut<'a> {
    running: bool,
    socket: &'a Path,
    /// The LaunchAgent's plist, when one for this data directory is installed.
    #[serde(skip_serializing_if = "Option::is_none")]
    launch_agent: Option<&'a Path>,
    /// Whether everything is paused (`reno pause`); absent when the daemon does not say.
    #[serde(skip_serializing_if = "Option::is_none")]
    paused: Option<bool>,
    #[serde(flatten)]
    info: Option<&'a DaemonStatusInfo>,
}

async fn status(env: &Env) -> anyhow::Result<ExitCode> {
    let launcher = env.launcher().ok();
    let agent = launcher
        .as_ref()
        .filter(|launcher| launcher.is_managed())
        .map(Launcher::agent_path);
    let Some(client) = env.try_connect().await? else {
        if env.json {
            print_json(&StatusOut {
                running: false,
                socket: &env.socket,
                launch_agent: agent.as_deref(),
                paused: None,
                info: None,
            })?;
        } else {
            print_line(format_args!(
                "not running (nothing is listening on {})",
                env.socket.display()
            ))?;
            if let Some(agent) = &agent {
                print_line(format_args!(
                    "launch agent: {} (`reno daemon start` asks launchd to start it)",
                    agent.display()
                ))?;
            }
        }
        return Ok(ExitCode::FAILURE);
    };
    let info = client.status().await.map_err(|e| env.explain(e))?;
    // Best effort: a daemon from before budgets does not answer.
    let paused = client
        .budget_status()
        .await
        .ok()
        .map(|budget| budget.paused);
    if env.json {
        print_json(&StatusOut {
            running: true,
            socket: &env.socket,
            launch_agent: agent.as_deref(),
            paused,
            info: Some(&info),
        })?;
    } else {
        print_line(render::status_text(&info, &env.socket))?;
        if paused == Some(true) {
            print_line("paused: nothing new starts (`reno resume` lifts it)")?;
        }
        if let Some(agent) = &agent {
            print_line(format_args!("launch agent: {}", agent.display()))?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

// ---- logs --------------------------------------------------------------------------------

async fn logs(env: &Env, args: &DaemonLogs) -> anyhow::Result<ExitCode> {
    let launcher = env.launcher()?;
    let paths = launcher.logs();
    // The follower takes its place first; the lines shown end exactly where it starts, so a
    // line written meanwhile is shown once, by the follower, and never lost.
    let follower = if args.follow {
        Some(Follower::from_end(&paths.log)?)
    } else {
        None
    };
    let end = follower.as_ref().map_or(u64::MAX, Follower::offset);
    let lines = tail_lines_before(&paths.log, args.lines, end)
        .with_context(|| format!("cannot read {}", paths.log.display()))?;
    if lines.is_empty() && !args.follow && !env.json {
        print_line(format_args!("no log lines yet in {}", paths.log.display()))?;
    }
    for line in &lines {
        print_log_line(env, line)?;
    }
    let Some(mut follower) = follower else {
        return Ok(ExitCode::SUCCESS);
    };
    std::fs::create_dir_all(&paths.dir)
        .with_context(|| format!("cannot create {}", paths.dir.display()))?;
    let (wake, mut changed) = tokio::sync::mpsc::channel::<()>(1);
    // The machine's pool of file-watch streams is shared by every process and can be empty for a
    // while. Following is worth waiting for: try again after a growing delay (a retry on error,
    // not a poll), and say so once. Lines written meanwhile are read as soon as the watch runs.
    let mut failures = 0u32;
    let _watcher = loop {
        let wake = wake.clone();
        let started = notify::recommended_watcher(move |_event| {
            // One pending wake-up is enough: the follower reads everything that is there.
            let _ = wake.try_send(());
        })
        .and_then(|mut watcher| {
            watcher.watch(&paths.dir, RecursiveMode::NonRecursive)?;
            Ok(watcher)
        });
        match started {
            Ok(watcher) => break watcher,
            Err(error) => {
                if failures == 0 {
                    crate::env::print_note(format_args!(
                        "reno: cannot watch {} yet ({error}); trying again",
                        paths.dir.display()
                    ))?;
                }
                failures = failures.saturating_add(1);
                let wait = Duration::from_millis(250)
                    .saturating_mul(2u32.saturating_pow(failures.saturating_sub(1)))
                    .min(Duration::from_secs(5));
                tokio::time::sleep(wait).await;
            }
        }
    };
    loop {
        for line in follower.read_new()? {
            print_log_line(env, &line)?;
        }
        if changed.recv().await.is_none() {
            bail!("the file watcher stopped");
        }
    }
}

fn print_log_line(env: &Env, line: &str) -> anyhow::Result<()> {
    if env.json {
        print_line(line)
    } else {
        print_line(render_line(line))
    }
}

// ---- install and uninstall ---------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Installed<'a> {
    dry_run: bool,
    label: &'a str,
    path: &'a Path,
    plist: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
}

async fn install(env: &Env, args: &DaemonInstall) -> anyhow::Result<ExitCode> {
    let launcher = env.launcher()?;
    let agent = launcher.agent()?;
    let plist = agent.plist();
    let path = launcher.agent_path();
    let domain = launcher.domain();

    if args.dry_run {
        if env.json {
            print_json(&Installed {
                dry_run: true,
                label: LABEL,
                path: &path,
                plist: &plist,
                pid: None,
            })?;
        } else {
            print_line(format_args!("would write {}:\n", path.display()))?;
            print_line(plist.trim_end())?;
            print_line(format_args!(
                "\nthen run: launchctl bootstrap {domain} {}",
                path.display()
            ))?;
        }
        return Ok(ExitCode::SUCCESS);
    }

    // A daemon started by hand holds the data-directory lock: the agent's would fail on it, and
    // launchd would keep restarting that failure. So launchd must find the field empty.
    let stopped = stop_daemon(env).await?;
    launcher.write_agent(&agent)?;
    // A reinstall replaces a loaded job: unload (which stops it and waits), then load.
    let replaced = launcher.unload()?;
    launcher.load()?;
    let pid = wait_up(env, &launcher, None).await?;

    if env.json {
        print_json(&Installed {
            dry_run: false,
            label: LABEL,
            path: &path,
            plist: &plist,
            pid: Some(pid),
        })?;
    } else {
        if let Some(old) = stopped {
            print_line(format_args!(
                "stopped renod (pid {old}) so that launchd can own it"
            ))?;
        }
        print_line(format_args!("wrote {}:\n", path.display()))?;
        print_line(plist.trim_end())?;
        if replaced {
            print_line(format_args!(
                "\nran: launchctl bootout {domain}/{LABEL}  (the old job)"
            ))?;
        }
        print_line(format_args!(
            "{}ran: launchctl bootstrap {domain} {}",
            if replaced { "" } else { "\n" },
            path.display()
        ))?;
        print_line(format_args!(
            "renod is running (pid {pid}) and starts at login; `reno daemon uninstall` removes it"
        ))?;
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Uninstalled<'a> {
    dry_run: bool,
    unloaded: bool,
    removed: bool,
    path: &'a Path,
}

fn uninstall(env: &Env, args: &DaemonInstall) -> anyhow::Result<ExitCode> {
    let launcher = env.launcher()?;
    let path = launcher.agent_path();
    let domain = launcher.domain();
    let (unloaded, removed) = if args.dry_run {
        (launcher.is_loaded()?, path.exists())
    } else {
        (launcher.unload()?, launcher.remove_agent()?)
    };
    if env.json {
        print_json(&Uninstalled {
            dry_run: args.dry_run,
            unloaded,
            removed,
            path: &path,
        })?;
        return Ok(ExitCode::SUCCESS);
    }
    let (would, did) = if args.dry_run {
        ("would run", "would delete")
    } else {
        ("ran", "deleted")
    };
    if unloaded {
        print_line(format_args!(
            "{would}: launchctl bootout {domain}/{LABEL}  (stops the daemon)"
        ))?;
    }
    if removed {
        print_line(format_args!("{did} {}", path.display()))?;
    }
    if !unloaded && !removed {
        print_line(format_args!(
            "no LaunchAgent is installed ({} does not exist and the job is not loaded)",
            path.display()
        ))?;
    }
    Ok(ExitCode::SUCCESS)
}

/// Whether a process with this pid exists. A process of another user still counts.
fn process_exists(pid: u32) -> bool {
    let Some(pid) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
        return false;
    };
    !matches!(test_kill_process(pid), Err(Errno::SRCH))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_that_is_running_exists_and_an_impossible_pid_does_not() {
        assert!(process_exists(std::process::id()));
        assert!(!process_exists(u32::MAX));
    }
}
