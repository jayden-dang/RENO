//! The real [`CommandRunner`](super::maintain::CommandRunner): one command line through the
//! person's login shell.
//!
//! - **The person's environment.** `<shell> -l -c <command>` with the PATH the probe uses
//!   ([`super::shell_env::login_shell_path`]), so `brew`, `npm` and `uv` resolve as they do in
//!   the person's terminal.
//! - **No way to ask a question.** Stdin is null. The command runs in a new session
//!   (`setsid`), so it has no controlling terminal for sudo to open; `SUDO_ASKPASS` points at
//!   `/usr/bin/false` and `NONINTERACTIVE=1` tells installers not to prompt. A command that
//!   still asks prints a prompt, which the policy layer ([`super::maintain::run_steps`]) turns
//!   into `NeedsPassword`.
//! - **No leftovers.** The command leads its own process group. Dropping the run kills that group
//!   unless the command already ended.

#![expect(
    unsafe_code,
    reason = "setsid before exec and kill(2) on the command's own process group"
)]

use std::ffi::OsString;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::sync::mpsc;

use super::maintain::{CommandRunner, Exit, RunnerError, clean_line};

/// How long the output still in the pipes is read after the command itself ended: a daemon it
/// left behind may hold them open for ever.
const DRAIN_GRACE: Duration = Duration::from_secs(1);

/// Runs commands through a shell.
#[derive(Debug, Clone)]
pub struct ShellRunner {
    shell: PathBuf,
    path: Option<OsString>,
}

impl ShellRunner {
    /// The person's own shell and the PATH its login reports. `None` when no shell can be found.
    pub fn login() -> Option<Self> {
        let shell = super::shell_env::login_shell()?;
        Some(Self::new(
            shell,
            super::shell_env::login_shell_path().map(OsString::from),
        ))
    }

    /// This shell, and this PATH when there is one (else the daemon's own).
    pub fn new(shell: PathBuf, path: Option<OsString>) -> Self {
        Self { shell, path }
    }

    fn command(&self, line: &str) -> Command {
        let name = self
            .shell
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let mut command = Command::new(&self.shell);
        // The shells whose `-l -c` is known; anything else is asked for a plain `-c`.
        if matches!(
            name.as_str(),
            "zsh" | "bash" | "sh" | "dash" | "ksh" | "fish"
        ) {
            command.arg("-l");
        }
        command.arg("-c").arg(line);
        if let Some(path) = &self.path {
            command.env("PATH", path);
        }
        command
            .env("SUDO_ASKPASS", "/usr/bin/false")
            .env("NONINTERACTIVE", "1")
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        // SAFETY: setsid is async-signal-safe and runs in the forked child before exec.
        unsafe {
            command.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
        command
    }
}

/// Kills the command's process group when dropped before the command ended.
struct GroupGuard {
    pid: Option<i32>,
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if let Some(pid) = self.pid {
            // SAFETY: kill(2) on a process group this runner started (its pid is the group id).
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
}

#[async_trait]
impl CommandRunner for ShellRunner {
    async fn run(&self, command: &str, out: mpsc::Sender<String>) -> Result<Exit, RunnerError> {
        let mut child = self
            .command(command)
            .spawn()
            .map_err(|e| RunnerError(format!("could not start {}: {e}", self.shell.display())))?;
        let mut guard = GroupGuard {
            pid: child.id().and_then(|id| i32::try_from(id).ok()),
        };
        let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(RunnerError("the shell has no output to read".into()));
        };
        let mut pumps = std::pin::pin!(async {
            tokio::join!(pump(stdout, out.clone()), pump(stderr, out.clone()),);
        });
        let status = tokio::select! {
            status = child.wait() => {
                // It ended: take what is left in the pipes, but not for ever.
                let _ = tokio::time::timeout(DRAIN_GRACE, &mut pumps).await;
                status
            }
            () = &mut pumps => child.wait().await,
        };
        // It ended by itself: whatever it left behind is not Reno's to kill.
        guard.pid = None;
        let status = status.map_err(|e| RunnerError(format!("could not wait for it: {e}")))?;
        Ok(match (status.code(), status.signal()) {
            (Some(code), _) => Exit::Code(code),
            (None, Some(signal)) => Exit::Signal(signal),
            (None, None) => Exit::Code(-1),
        })
    }
}

/// Reads `reader` to its end and sends each line, cleaned. A prompt that has no newline yet
/// ("Password:") is sent at once.
async fn pump(mut reader: impl AsyncRead + Unpin, out: mpsc::Sender<String>) {
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    loop {
        let read = match reader.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        pending.extend_from_slice(&chunk[..read]);
        while let Some(at) = pending.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = pending.drain(..=at).collect();
            if let Some(line) = clean_line(&String::from_utf8_lossy(&line))
                && out.send(line).await.is_err()
            {
                return;
            }
        }
        if !pending.is_empty() && (looks_like_prompt(&pending) || pending.len() > 8192) {
            let line = std::mem::take(&mut pending);
            if let Some(line) = clean_line(&String::from_utf8_lossy(&line))
                && out.send(line).await.is_err()
            {
                return;
            }
        }
    }
    if let Some(line) = clean_line(&String::from_utf8_lossy(&pending)) {
        let _ = out.send(line).await;
    }
}

/// Whether text with no newline yet is a prompt for an answer.
fn looks_like_prompt(pending: &[u8]) -> bool {
    let text = String::from_utf8_lossy(pending);
    let text = text.trim_end();
    text.ends_with(':') || text.ends_with('?')
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// A plain `sh`: the tests run only commands built into it, never a real tool.
    fn runner() -> ShellRunner {
        ShellRunner::new(PathBuf::from("/bin/sh"), None)
    }

    async fn collect(command: &str) -> (Result<Exit, RunnerError>, Vec<String>) {
        let (tx, mut rx) = mpsc::channel(64);
        let exit = runner().run(command, tx).await;
        let mut lines = Vec::new();
        while let Ok(line) = rx.try_recv() {
            lines.push(line);
        }
        (exit, lines)
    }

    #[tokio::test]
    async fn output_of_both_streams_arrives_as_lines_with_the_exit_status() {
        let (exit, mut lines) = collect("echo out; echo err 1>&2; exit 3").await;
        assert_eq!(exit.unwrap(), Exit::Code(3));
        lines.sort();
        assert_eq!(lines, ["err", "out"]);
    }

    #[tokio::test]
    async fn stdin_is_null_and_sudo_cannot_ask() {
        // `read` gets end-of-file at once, and the askpass is /usr/bin/false.
        let (exit, lines) = collect("read x && echo got; echo \"askpass=$SUDO_ASKPASS\"").await;
        assert_eq!(exit.unwrap(), Exit::Code(0));
        assert!(
            lines.contains(&"askpass=/usr/bin/false".to_owned()),
            "{lines:?}"
        );
        assert!(!lines.iter().any(|l| l == "got"));
    }

    #[tokio::test]
    async fn a_prompt_without_a_newline_is_sent_at_once() {
        let (tx, mut rx) = mpsc::channel(8);
        let shell = runner();
        let run = shell.run("printf 'Password:'; sleep 30", tx);
        let mut run = std::pin::pin!(run);
        let first = tokio::select! {
            line = rx.recv() => line,
            _ = &mut run => None,
        };
        assert_eq!(first.as_deref(), Some("Password:"));
    }

    #[tokio::test]
    async fn dropping_the_run_kills_the_whole_process_group() {
        let (tx, mut rx) = mpsc::channel(8);
        // A child of the shell sleeps and the shell prints its pid: it is the group's, not the shell's.
        let shell = runner();
        let mut run = Box::pin(shell.run("sleep 60 & echo $!; wait", tx));
        let pid: i32 = tokio::select! {
            line = rx.recv() => line.unwrap().parse().unwrap(),
            _ = &mut run => panic!("it ended"),
        };
        drop(run);
        let mut gone = false;
        for _ in 0..100 {
            // SAFETY: signal 0 only checks that the pid exists.
            if unsafe { libc::kill(pid, 0) } != 0 {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(gone, "the shell's child survived the drop");
    }
}
