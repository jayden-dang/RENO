//! `reno daemon start | stop | status`.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use reno_client::ClientError;
use reno_proto::DaemonStatusInfo;
use rustix::io::Errno;
use rustix::process::{Pid, test_kill_process};
use serde::Serialize;

use crate::cli::DaemonCommand;
use crate::env::{Env, print_json, print_line};
use crate::render;

/// How long `daemon start` waits for the daemon to answer.
const START_TIMEOUT: Duration = Duration::from_secs(15);
/// How long `daemon stop` waits for the process to be gone: the kernel's own shutdown deadline
/// is 15 seconds, so a daemon that is still working is given longer than that.
const STOP_TIMEOUT: Duration = Duration::from_secs(30);
/// How often start and stop look again. They wait on another process; there is no event to follow.
const LOOK_AGAIN: Duration = Duration::from_millis(50);

pub(crate) async fn run(env: &Env, command: DaemonCommand) -> anyhow::Result<ExitCode> {
    match command {
        DaemonCommand::Start => start(env).await,
        DaemonCommand::Stop => stop(env).await,
        DaemonCommand::Status => status(env).await,
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Started<'a> {
    started: bool,
    pid: u32,
    socket: &'a Path,
}

async fn start(env: &Env) -> anyhow::Result<ExitCode> {
    if let Some(client) = env.try_connect().await? {
        return report_started(env, false, client.daemon().pid);
    }

    let data_dir = env
        .data_dir
        .as_deref()
        .context("cannot tell where to keep the daemon's files: set --data-dir, or HOME")?;
    let renod = find_renod()?;
    let logs = env.logs().unwrap_or_else(|| data_dir.join("logs"));
    // Its own session, no inherited stdio: it must outlive this command and this terminal. Its
    // output goes to its own logs.
    let mut child = Command::new(&renod)
        .arg("--data-dir")
        .arg(data_dir)
        .arg("--socket")
        .arg(&env.socket)
        .arg("--detach")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("cannot start {}", renod.display()))?;

    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        match reno_client::Client::connect_unix(&env.socket).await {
            Ok(client) => return report_started(env, true, client.daemon().pid),
            Err(error @ ClientError::Incompatible { .. }) => return Err(env.explain(error)),
            // Not listening yet, or still starting up.
            Err(_) => {}
        }
        if let Some(status) = child.try_wait().context("cannot watch renod")? {
            bail!(
                "renod exited ({status}) before it answered; see the logs in {}",
                logs.display()
            );
        }
        if Instant::now() >= deadline {
            bail!(
                "renod did not answer within {} seconds; see the logs in {}",
                START_TIMEOUT.as_secs(),
                logs.display()
            );
        }
        tokio::time::sleep(LOOK_AGAIN).await;
    }
}

fn report_started(env: &Env, started: bool, pid: u32) -> anyhow::Result<ExitCode> {
    if env.json {
        print_json(&Started {
            started,
            pid,
            socket: &env.socket,
        })?;
    } else if started {
        print_line(format_args!("started renod (pid {pid})"))?;
    } else {
        print_line(format_args!("renod is already running (pid {pid})"))?;
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Stopped {
    stopped: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pid: Option<u32>,
}

async fn stop(env: &Env) -> anyhow::Result<ExitCode> {
    let Some(client) = env.try_connect().await? else {
        if env.json {
            print_json(&Stopped {
                stopped: false,
                pid: None,
            })?;
        } else {
            print_line("renod is not running")?;
        }
        return Ok(ExitCode::SUCCESS);
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
    if env.json {
        print_json(&Stopped {
            stopped: true,
            pid: Some(pid),
        })?;
    } else {
        print_line(format_args!("stopped renod (pid {pid})"))?;
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct StatusOut<'a> {
    running: bool,
    socket: &'a Path,
    #[serde(flatten)]
    info: Option<&'a DaemonStatusInfo>,
}

async fn status(env: &Env) -> anyhow::Result<ExitCode> {
    let Some(client) = env.try_connect().await? else {
        if env.json {
            print_json(&StatusOut {
                running: false,
                socket: &env.socket,
                info: None,
            })?;
        } else {
            print_line(format_args!(
                "not running (nothing is listening on {})",
                env.socket.display()
            ))?;
        }
        return Ok(ExitCode::FAILURE);
    };
    let info = client.status().await.map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&StatusOut {
            running: true,
            socket: &env.socket,
            info: Some(&info),
        })?;
    } else {
        print_line(render::status_text(&info, &env.socket))?;
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

/// `renod` next to this program, else on `PATH`.
fn find_renod() -> anyhow::Result<PathBuf> {
    let beside = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf));
    let path = std::env::var_os("PATH").unwrap_or_default();
    let dirs: Vec<PathBuf> = beside
        .into_iter()
        .chain(std::env::split_paths(&path))
        .collect();
    find_in(&dirs).context("cannot find `renod`: looked next to this program and on PATH")
}

fn find_in(dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter()
        .map(|dir| dir.join("renod"))
        .find(|candidate| is_executable_file(candidate))
}

fn is_executable_file(path: &Path) -> bool {
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renod_is_looked_for_in_order_and_must_be_executable() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let dirs = vec![first.path().to_path_buf(), second.path().to_path_buf()];
        assert_eq!(find_in(&dirs), None);

        // A file that is not executable does not count.
        std::fs::write(first.path().join("renod"), b"#!/bin/sh\n").unwrap();
        assert_eq!(find_in(&dirs), None);

        let later = second.path().join("renod");
        std::fs::write(&later, b"#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&later, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(find_in(&dirs), Some(later));

        let earlier = first.path().join("renod");
        std::fs::set_permissions(&earlier, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(find_in(&dirs), Some(earlier));
    }

    #[test]
    fn a_process_that_is_running_exists_and_an_impossible_pid_does_not() {
        assert!(process_exists(std::process::id()));
        assert!(!process_exists(u32::MAX));
    }
}
