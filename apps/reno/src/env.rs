//! Where the daemon is, and how to reach it.

use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::anyhow;
use reno_client::{Client, ClientError};

use crate::cli::Cli;

/// The socket file's name inside a data directory.
const SOCKET_FILE: &str = "reno.sock";

/// The paths and output mode every command shares.
#[derive(Debug, Clone)]
pub(crate) struct Env {
    pub(crate) socket: PathBuf,
    /// Needed only to start a daemon.
    pub(crate) data_dir: Option<PathBuf>,
    pub(crate) json: bool,
}

impl Env {
    pub(crate) fn from_cli(cli: &Cli) -> anyhow::Result<Self> {
        let data_dir = cli.data_dir.clone().or_else(reno_client::default_data_dir);
        let socket = match (&cli.socket, &data_dir) {
            (Some(socket), _) => socket.clone(),
            (None, Some(dir)) => dir.join(SOCKET_FILE),
            (None, None) => {
                return Err(anyhow!(
                    "cannot find the daemon's socket: set --socket, --data-dir, or HOME"
                ));
            }
        };
        Ok(Self {
            socket,
            data_dir,
            json: cli.json,
        })
    }

    /// Where `renod` writes its logs.
    pub(crate) fn logs(&self) -> Option<PathBuf> {
        self.data_dir.as_ref().map(|dir| dir.join("logs"))
    }

    /// Connects to the daemon. `None` means nothing is listening.
    pub(crate) async fn try_connect(&self) -> anyhow::Result<Option<Client>> {
        match Client::connect_unix(&self.socket).await {
            Ok(client) => Ok(Some(client)),
            Err(ClientError::NotRunning(_)) => Ok(None),
            Err(error) => Err(self.explain(error)),
        }
    }

    /// Connects to the daemon, or says how to start it.
    pub(crate) async fn connect(&self) -> anyhow::Result<Client> {
        self.try_connect().await?.ok_or_else(|| self.not_running())
    }

    fn not_running(&self) -> anyhow::Error {
        anyhow!(
            "the daemon is not running (nothing is listening on {}); \
             start it with `reno daemon start`",
            self.socket.display()
        )
    }

    /// A client error as a sentence for a person.
    pub(crate) fn explain(&self, error: ClientError) -> anyhow::Error {
        match error {
            ClientError::NotRunning(_) => self.not_running(),
            ClientError::Remote(object) => anyhow!("{}", object.message),
            ClientError::Disconnected => {
                anyhow!("the daemon closed the connection (it may have stopped)")
            }
            other => anyhow!("{other}"),
        }
    }
}

/// Writes one JSON value and a newline to stdout.
pub(crate) fn print_json(value: &impl serde::Serialize) -> anyhow::Result<()> {
    let mut out = io::stdout().lock();
    serde_json::to_writer(&mut out, value)?;
    writeln!(out)?;
    Ok(())
}

/// Writes one line of text to stdout.
pub(crate) fn print_line(text: impl std::fmt::Display) -> anyhow::Result<()> {
    writeln!(io::stdout().lock(), "{text}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(cli: &[&str]) -> Env {
        use clap::Parser;
        let mut args = vec!["reno"];
        args.extend_from_slice(cli);
        args.extend(["daemon", "status"]);
        Env::from_cli(&Cli::parse_from(args)).expect("env")
    }

    #[test]
    fn a_data_dir_puts_the_socket_inside_it() {
        let env = env(&["--data-dir", "/tmp/reno-x"]);
        assert_eq!(env.socket, PathBuf::from("/tmp/reno-x/reno.sock"));
        assert_eq!(env.logs(), Some(PathBuf::from("/tmp/reno-x/logs")));
    }

    #[test]
    fn an_explicit_socket_wins() {
        let env = env(&["--data-dir", "/tmp/reno-x", "--socket", "/tmp/s.sock"]);
        assert_eq!(env.socket, PathBuf::from("/tmp/s.sock"));
    }
}
