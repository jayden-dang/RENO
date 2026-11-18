//! `reno term open|list|attach|close`: a run's terminals (RP2).
//!
//! Each is a few client calls and some printing (ARCH-2): the same methods the app's Terminal
//! tab uses. `attach` is for trying a terminal by hand: it passes your keyboard to the shell and
//! the shell's output to your screen, bytes as they are. The terminal outlives it: detach with
//! Ctrl-] and the shell keeps running.

use std::io::{self, Read, Write};
use std::process::ExitCode;

use clap::{Args, Subcommand};
use futures::StreamExt;
use reno_proto::{RunId, TermEvent, TermId, TermInfo, TermOpenParams};

use crate::env::{Env, print_json, print_line, print_note};

/// The key that detaches: Ctrl-], as in `telnet`.
const DETACH: u8 = 0x1d;

#[derive(Debug, Subcommand)]
pub(crate) enum TermCommand {
    /// Start a shell in a run's worktree and print its id. On a run that is live the shell is
    /// sandboxed: it cannot write the paths the protected-path guard watches.
    Open(TermOpen),
    /// List the terminals in a run's worktree.
    List(TermList),
    /// Attach to a terminal: its output on your screen and your keyboard to its shell, as they
    /// are. Ctrl-] detaches and leaves the shell running; the exit code of the shell is this
    /// command's when the shell ends.
    Attach(TermAttach),
    /// End a terminal's shell and forget it.
    Close(TermClose),
}

#[derive(Debug, Args)]
pub(crate) struct TermOpen {
    /// The run.
    pub(crate) run: RunId,
    /// Run this command in the shell once it starts.
    #[arg(long, value_name = "COMMAND")]
    pub(crate) command: Option<String>,
}

#[derive(Debug, Args)]
pub(crate) struct TermList {
    /// The run.
    pub(crate) run: RunId,
}

#[derive(Debug, Args)]
pub(crate) struct TermAttach {
    /// The terminal.
    pub(crate) id: TermId,
    /// Replay output after this sequence number [default: everything the daemon still holds].
    #[arg(long, value_name = "SEQ")]
    pub(crate) after_seq: Option<u64>,
}

#[derive(Debug, Args)]
pub(crate) struct TermClose {
    /// The terminal.
    pub(crate) id: TermId,
}

pub(crate) async fn run(env: &Env, command: TermCommand) -> anyhow::Result<ExitCode> {
    match command {
        TermCommand::Open(args) => open(env, args).await,
        TermCommand::List(args) => list(env, args).await,
        TermCommand::Attach(args) => attach(env, args).await,
        TermCommand::Close(args) => close(env, args).await,
    }
}

async fn open(env: &Env, args: TermOpen) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let (cols, rows) = size().unwrap_or((80, 24));
    let opened = client
        .term_open(TermOpenParams {
            run_id: args.run,
            cols,
            rows,
            command: args.command,
        })
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&opened)?;
        return Ok(ExitCode::SUCCESS);
    }
    if opened.sandboxed {
        print_note(
            "(the run is live: this shell cannot write protected paths; it keeps that limit)",
        )?;
    }
    if opened.in_place {
        print_note("(the run has no worktree: this shell is in its own folder)")?;
    }
    print_line(&opened.id)?;
    Ok(ExitCode::SUCCESS)
}

async fn list(env: &Env, args: TermList) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let terminals = client
        .term_list(args.run)
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&terminals)?;
        return Ok(ExitCode::SUCCESS);
    }
    if terminals.is_empty() {
        print_note("no terminals")?;
    }
    for terminal in &terminals {
        print_line(row(terminal))?;
    }
    Ok(ExitCode::SUCCESS)
}

/// `term_01.. zsh sandboxed exited /path`.
fn row(terminal: &TermInfo) -> String {
    let mut flags = Vec::new();
    if terminal.sandboxed {
        flags.push("sandboxed");
    }
    if terminal.in_place {
        flags.push("in-place");
    }
    if terminal.exited {
        flags.push("exited");
    }
    format!(
        "{}  {}  {}  {}",
        terminal.id,
        terminal.shell,
        if flags.is_empty() {
            "-".to_owned()
        } else {
            flags.join(",")
        },
        terminal.cwd
    )
}

async fn close(env: &Env, args: TermClose) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    client
        .term_close(args.id)
        .await
        .map_err(|e| env.explain(e))?;
    Ok(ExitCode::SUCCESS)
}

/// The terminal's size, when stdout is one.
fn size() -> Option<(u16, u16)> {
    let winsize = rustix::termios::tcgetwinsize(io::stdout()).ok()?;
    (winsize.ws_col > 0 && winsize.ws_row > 0).then_some((winsize.ws_col, winsize.ws_row))
}

/// The local terminal in raw mode while it lives, restored on drop, so every key goes to the
/// remote shell and nothing is echoed or cooked here. Does nothing when stdin is not a terminal.
struct Raw {
    saved: Option<rustix::termios::Termios>,
}

impl Raw {
    fn enter() -> Self {
        use rustix::termios::{OptionalActions, tcgetattr, tcsetattr};
        let stdin = io::stdin();
        let Ok(saved) = tcgetattr(&stdin) else {
            return Self { saved: None };
        };
        let mut raw = saved.clone();
        raw.make_raw();
        match tcsetattr(&stdin, OptionalActions::Flush, &raw) {
            Ok(()) => Self { saved: Some(saved) },
            Err(_) => Self { saved: None },
        }
    }
}

impl Drop for Raw {
    fn drop(&mut self) {
        use rustix::termios::{OptionalActions, tcsetattr};
        if let Some(saved) = self.saved.take() {
            let _ = tcsetattr(io::stdin(), OptionalActions::Flush, &saved);
        }
    }
}

/// What stdin delivered: bytes, up to a detach key.
enum Typed {
    Bytes(Vec<u8>),
    Detach,
}

/// Splits a read at the detach key: what came before it, and whether it was there.
fn split_detach(chunk: &[u8]) -> (&[u8], bool) {
    match chunk.iter().position(|b| *b == DETACH) {
        Some(at) => (&chunk[..at], true),
        None => (chunk, false),
    }
}

async fn attach(env: &Env, args: TermAttach) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let id = args.id;
    let mut output = client
        .term_attach(id.clone(), args.after_seq)
        .await
        .map_err(|e| env.explain(e))?;

    let raw = Raw::enter();
    let interactive = raw.saved.is_some();
    if let Some((cols, rows)) = size().filter(|_| interactive) {
        let _ = client.term_resize(id.clone(), cols, rows).await;
    }
    if interactive {
        print_note("(attached; Ctrl-] detaches and leaves the shell running)\r")?;
    }

    // stdin is read on a thread of its own: a read cannot be cancelled, and the process ends
    // when this function does. One write is in flight at a time, in the order typed.
    let (typed_tx, mut typed) = tokio::sync::mpsc::channel::<Typed>(8);
    std::thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0_u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let (before, detach) = split_detach(&buf[..n]);
                    if !before.is_empty()
                        && typed_tx
                            .blocking_send(Typed::Bytes(before.to_vec()))
                            .is_err()
                    {
                        break;
                    }
                    if detach {
                        let _ = typed_tx.blocking_send(Typed::Detach);
                        break;
                    }
                }
            }
        }
    });

    let mut stdout = io::stdout();
    let mut code = ExitCode::SUCCESS;
    let mut stdin_open = true;
    loop {
        tokio::select! {
            event = output.next() => match event {
                None => break,
                Some(Err(error)) => {
                    drop(raw);
                    return Err(env.explain(error));
                }
                Some(Ok(TermEvent::Data { data, .. })) => {
                    if let Some(bytes) = reno_proto::decode_bytes(&data) {
                        stdout.write_all(&bytes)?;
                        stdout.flush()?;
                    }
                }
                Some(Ok(TermEvent::Gap { lost_from, seq })) => {
                    print_note(format!(
                        "\r\n[output {lost_from}..{} was dropped: the daemon holds only the last 1 MiB]\r",
                        seq - 1
                    ))?;
                }
                Some(Ok(TermEvent::Exit { code: exit, signal, .. })) => {
                    let how = match (exit, signal) {
                        (Some(exit), _) => format!("exit code {exit}"),
                        (None, Some(signal)) => format!("signal {signal}"),
                        (None, None) => "no exit status".to_owned(),
                    };
                    print_note(format!("\r\n[the shell ended: {how}]\r"))?;
                    code = ExitCode::from(exit.map_or(1, |c| u8::try_from(c).unwrap_or(1)));
                    break;
                }
            },
            typed = typed.recv(), if stdin_open => match typed {
                Some(Typed::Bytes(bytes)) => {
                    if let Err(error) = client.term_write(id.clone(), &bytes).await {
                        drop(raw);
                        return Err(env.explain(error));
                    }
                }
                Some(Typed::Detach) => {
                    print_note("\r\n[detached; the shell is still running]\r")?;
                    break;
                }
                // stdin ended (a pipe): keep showing output until the shell ends.
                None => stdin_open = false,
            },
        }
    }
    drop(raw);
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_detach_key_cuts_a_read_where_it_stands() {
        assert_eq!(split_detach(b"ls\x1dgone"), (&b"ls"[..], true));
        assert_eq!(split_detach(b"plain"), (&b"plain"[..], false));
        assert_eq!(split_detach(b"\x1d"), (&b""[..], true));
    }

    #[test]
    fn a_row_says_what_the_terminal_is() {
        let id = TermId::from_suffix("a1").unwrap_or_else(|_| unreachable!());
        let terminal = TermInfo {
            id,
            cwd: "/w".into(),
            shell: "zsh".into(),
            sandboxed: true,
            in_place: false,
            exited: true,
            opened_at_ms: 1,
        };
        assert_eq!(row(&terminal), "term_a1  zsh  sandboxed,exited  /w");
    }
}
