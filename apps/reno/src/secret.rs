//! `reno secret list|set|delete`.
//!
//! A value never appears on a command line: `set` reads it from a prompt that does not echo (a
//! terminal on standard input) or from standard input (`--from-stdin`, or any pipe). It goes to
//! the daemon in the one `secret.set` call and is not printed, logged or kept. Nothing here prints
//! a value back; `list` shows names and whether each is set.

use std::io::{self, BufRead, IsTerminal, Read, Write};
use std::process::ExitCode;

use anyhow::{Context, bail};
use reno_proto::{SECRET_VALUE_MAX, SecretListResult, check_secret_name};
use serde::Serialize;

use crate::cli::{SecretCommand, SecretDelete, SecretSet};
use crate::env::{Env, print_json, print_line};

pub(crate) async fn run(env: &Env, command: SecretCommand) -> anyhow::Result<ExitCode> {
    match command {
        SecretCommand::List => list(env).await,
        SecretCommand::Set(args) => set(env, args).await,
        SecretCommand::Delete(args) => delete(env, args).await,
    }
}

async fn list(env: &Env) -> anyhow::Result<ExitCode> {
    let client = env.connect().await?;
    let result = client.list_secrets().await.map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&result)?;
    } else if result.secrets.is_empty() {
        print_line("no secrets are set, and no bot asks for one")?;
    } else {
        for line in lines(&result) {
            print_line(line)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// One line per secret: its name, whether it is set, and the bots that ask for it.
fn lines(result: &SecretListResult) -> Vec<String> {
    let width = result
        .secrets
        .iter()
        .map(|s| s.name.len())
        .max()
        .unwrap_or(0);
    result
        .secrets
        .iter()
        .map(|secret| {
            let state = if secret.set { "set    " } else { "not set" };
            let used = if secret.used_by.is_empty() {
                String::new()
            } else {
                format!("  used by {}", secret.used_by.join(", "))
            };
            format!("{:width$}  {state}{used}", secret.name)
        })
        .collect()
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Stored<'a> {
    name: &'a str,
    replaced: bool,
}

async fn set(env: &Env, args: SecretSet) -> anyhow::Result<ExitCode> {
    check_secret_name(&args.name).map_err(|e| anyhow::anyhow!("{e}"))?;
    let client = env.connect().await?;
    let value = read_value(&args.name, args.from_stdin)?;
    let replaced = client
        .set_secret(&args.name, &value)
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&Stored {
            name: &args.name,
            replaced,
        })?;
    } else if replaced {
        print_line(format!("replaced {}", args.name))?;
    } else {
        print_line(format!("stored {}", args.name))?;
    }
    Ok(ExitCode::SUCCESS)
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Deleted<'a> {
    name: &'a str,
    deleted: bool,
}

async fn delete(env: &Env, args: SecretDelete) -> anyhow::Result<ExitCode> {
    check_secret_name(&args.name).map_err(|e| anyhow::anyhow!("{e}"))?;
    let client = env.connect().await?;
    let deleted = client
        .delete_secret(&args.name)
        .await
        .map_err(|e| env.explain(e))?;
    if env.json {
        print_json(&Deleted {
            name: &args.name,
            deleted,
        })?;
    } else if deleted {
        print_line(format!("deleted {}", args.name))?;
    } else {
        print_line(format!("{} was not set", args.name))?;
    }
    Ok(ExitCode::SUCCESS)
}

/// The value to store: from the no-echo prompt when standard input is a terminal and
/// `--from-stdin` was not given, else from standard input.
#[allow(clippy::print_stderr)]
fn read_value(name: &str, from_stdin: bool) -> anyhow::Result<String> {
    let stdin = io::stdin();
    if !from_stdin {
        if !stdin.is_terminal() {
            bail!("standard input is not a terminal: pipe the value in with --from-stdin");
        }
        eprint!("Value for {name} (not shown): ");
        io::stderr().flush().ok();
        let line = {
            let _echo = EchoOff::new(&stdin);
            let mut line = String::new();
            let read = stdin.lock().read_line_limited(&mut line);
            // The Enter key was not echoed either.
            eprintln!();
            read?;
            line
        };
        return finish(line);
    }
    let mut raw = String::new();
    stdin
        .lock()
        .take(SECRET_VALUE_MAX as u64 + 2)
        .read_to_string(&mut raw)
        .context("cannot read the value from standard input")?;
    finish(raw)
}

/// Strips one final line ending and checks what is left: not empty, not too large.
fn finish(mut raw: String) -> anyhow::Result<String> {
    if raw.ends_with('\n') {
        raw.pop();
        if raw.ends_with('\r') {
            raw.pop();
        }
    }
    if raw.is_empty() {
        bail!("the value is empty: nothing was stored");
    }
    if raw.len() > SECRET_VALUE_MAX {
        bail!("the value is longer than {SECRET_VALUE_MAX} bytes: nothing was stored");
    }
    Ok(raw)
}

/// `read_line` that stops at the size limit instead of growing without bound.
trait ReadLineLimited {
    fn read_line_limited(&mut self, into: &mut String) -> io::Result<usize>;
}

impl<T: io::BufRead> ReadLineLimited for T {
    fn read_line_limited(&mut self, into: &mut String) -> io::Result<usize> {
        self.by_ref()
            .take(SECRET_VALUE_MAX as u64 + 2)
            .read_line(into)
    }
}

/// Terminal echo off while it lives, restored on drop. Does nothing when the terminal's
/// attributes cannot be read or set.
struct EchoOff {
    saved: Option<rustix::termios::Termios>,
}

impl EchoOff {
    fn new(stdin: &io::Stdin) -> Self {
        use rustix::termios::{LocalModes, OptionalActions, tcgetattr, tcsetattr};
        let Ok(saved) = tcgetattr(stdin) else {
            return Self { saved: None };
        };
        let mut quiet = saved.clone();
        quiet.local_modes.remove(LocalModes::ECHO);
        // Keep the newline echo off too: the caller prints its own.
        quiet.local_modes.remove(LocalModes::ECHONL);
        match tcsetattr(stdin, OptionalActions::Flush, &quiet) {
            Ok(()) => Self { saved: Some(saved) },
            Err(_) => Self { saved: None },
        }
    }
}

impl Drop for EchoOff {
    fn drop(&mut self) {
        use rustix::termios::{OptionalActions, tcsetattr};
        if let Some(saved) = self.saved.take() {
            let _ = tcsetattr(io::stdin(), OptionalActions::Flush, &saved);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use clap::Parser;
    use reno_proto::SecretInfo;

    use super::*;
    use crate::cli::Cli;

    #[test]
    fn one_final_line_ending_is_dropped_and_the_rest_kept() {
        assert_eq!(finish("abc\n".into()).unwrap(), "abc");
        assert_eq!(finish("abc\r\n".into()).unwrap(), "abc");
        assert_eq!(finish("abc".into()).unwrap(), "abc");
        assert_eq!(
            finish("-----BEGIN\nline\n-----END\n".into()).unwrap(),
            "-----BEGIN\nline\n-----END"
        );
        assert_eq!(finish("  spaced  \n".into()).unwrap(), "  spaced  ");
    }

    #[test]
    fn an_empty_or_oversized_value_is_refused_without_echoing_it() {
        assert!(finish(String::new()).is_err());
        assert!(finish("\n".into()).is_err());
        let error = finish("x".repeat(SECRET_VALUE_MAX + 1)).unwrap_err();
        assert!(!error.to_string().contains("xxxx"));
    }

    #[test]
    fn the_value_cannot_be_given_as_an_argument() {
        for args in [
            vec!["reno", "secret", "set", "GITHUB_TOKEN", "ghp_value"],
            vec![
                "reno",
                "secret",
                "set",
                "GITHUB_TOKEN",
                "--value",
                "ghp_value",
            ],
            vec![
                "reno",
                "secret",
                "set",
                "GITHUB_TOKEN",
                "--from-stdin",
                "ghp_value",
            ],
        ] {
            assert!(Cli::try_parse_from(&args).is_err(), "{args:?}");
        }
        assert!(Cli::try_parse_from(["reno", "secret", "set", "GITHUB_TOKEN"]).is_ok());
        assert!(
            Cli::try_parse_from(["reno", "secret", "set", "GITHUB_TOKEN", "--from-stdin"]).is_ok()
        );
    }

    #[test]
    fn the_list_shows_names_state_and_users_aligned() {
        let result = SecretListResult {
            secrets: vec![
                SecretInfo {
                    name: "GITHUB_TOKEN".into(),
                    set: true,
                    used_by: vec!["builder".into(), "keeper".into()],
                },
                SecretInfo {
                    name: "NPM".into(),
                    set: false,
                    used_by: Vec::new(),
                },
            ],
        };
        assert_eq!(
            lines(&result),
            [
                "GITHUB_TOKEN  set      used by builder, keeper",
                "NPM           not set",
            ]
        );
    }
}
