//! Reno's MCP servers for one Pi run, and the other files a run's flags name.
//!
//! Pi 1.5.0 has MCP support of its own, read from `~/.pi/agent/mcp.json` and a trusted
//! project's `.pi/mcp.json`; adding a server there (`pi mcp add`) writes the owner's Pi
//! configuration, which Reno never does. Instead a small extension, `mcp.mjs`, is written to the
//! run's scratch directory and loaded with `--extension`. It reads the servers from a JSON file
//! in the same directory (named by [`CONFIG_ENV`], never inlined into the environment, which
//! Pi's tool subprocesses inherit), connects them over stdio when the session starts, and
//! registers their tools as `mcp__<server>__<tool>`: the names Pi's own MCP client would give
//! them, so Reno classifies a call the same way. The scratch directory goes with the run.

use std::io::Write;
use std::path::{Path, PathBuf};

use reno_proto::McpServerSpec;
use serde_json::json;

/// The variable that names the file holding the servers, for the bridge to read.
pub(super) const CONFIG_ENV: &str = "RENO_PI_MCP";

/// The bridge extension's source: Reno's, not Pi's, loaded from the run's scratch directory.
const BRIDGE: &str = include_str!("mcp.mjs");

/// The files that load Reno's servers into one Pi.
#[derive(Debug)]
pub(super) struct Bridge {
    /// The extension to load (`--extension`).
    pub(super) extension: PathBuf,
    /// The servers it connects (`RENO_PI_MCP`).
    pub(super) config: PathBuf,
}

/// Write `bytes` to `dir/name`, readable by the owner only.
pub(super) fn write_private(dir: &Path, name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
    let path = dir.join(name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    file.write_all(bytes)?;
    Ok(path)
}

/// The bridge and its config for `servers`, in `dir`; `None` when the run injects no server.
pub(super) fn install(dir: &Path, servers: &[McpServerSpec]) -> std::io::Result<Option<Bridge>> {
    if servers.is_empty() {
        return Ok(None);
    }
    let config: Vec<_> = servers
        .iter()
        .map(|server| {
            json!({
                "name": server.name,
                "command": server.command,
                "args": server.args,
                "env": server.env,
            })
        })
        .collect();
    let config = write_private(
        dir,
        "mcp-servers.json",
        &serde_json::to_vec(&config).map_err(std::io::Error::other)?,
    )?;
    let extension = write_private(dir, "reno-mcp.mjs", BRIDGE.as_bytes())?;
    Ok(Some(Bridge { extension, config }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn server(name: &str) -> McpServerSpec {
        McpServerSpec {
            name: name.into(),
            command: format!("/opt/{name}/server"),
            args: vec!["mcp".into()],
            env: [("RENO_RUN".to_owned(), "r1".to_owned())].into(),
        }
    }

    #[test]
    fn no_server_means_no_bridge() {
        let dir = tempfile::tempdir().unwrap();
        assert!(install(dir.path(), &[]).unwrap().is_none());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[test]
    fn the_servers_go_to_a_private_file_beside_the_bridge_and_nowhere_else() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let bridge = install(dir.path(), &[server("reno"), server("plugin-focus")])
            .unwrap()
            .unwrap();
        for file in [&bridge.extension, &bridge.config] {
            assert!(file.starts_with(dir.path()));
            let mode = std::fs::metadata(file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{}", file.display());
        }
        let written: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&bridge.config).unwrap()).unwrap();
        assert_eq!(written[0]["name"], "reno");
        assert_eq!(written[1]["name"], "plugin-focus");
        assert_eq!(written[0]["env"]["RENO_RUN"], "r1");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
        // The bridge names tools like Pi's own MCP client does and reads its config from the file.
        let source = std::fs::read_to_string(&bridge.extension).unwrap();
        assert!(source.contains("mcp__"));
        assert!(source.contains("process.env.RENO_PI_MCP"));
    }

    #[test]
    fn a_file_is_never_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        write_private(dir.path(), "a", b"1").unwrap();
        assert!(write_private(dir.path(), "a", b"2").is_err());
    }
}
