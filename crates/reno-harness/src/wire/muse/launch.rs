//! How one `muse serve` host is launched and what a run asks of it, as data.
//!
//! Everything a run asks of Muse is a flag, an environment variable or a field of a session
//! command, never a write to Muse's configuration (the owner's rule that Reno does not touch a
//! harness's settings, credentials or Keychain entries). Nothing needs a file: the instructions
//! ride the prompt and the MCP servers ride the `session/start` config, which Muse keeps for the
//! session only.
//!
//! | What | How |
//! |---|---|
//! | `muse serve --provider meta` | the MSP host on stdio; the provider is pinned so the owner's settings cannot redirect a run |
//! | `--disable-shell --disable-write --no-session-log` | the read-only host of a probe: no shell, no workspace writes, memory-only sessions |
//! | `MUSE_NO_AUTO_UPDATE=1` | the launcher script (the installed `muse` is one) checks for a new release on every start and updates itself in the background; with this it does neither |
//!
//! Not set: `--trust-workspace`. A workspace's own skills and rules load only when the owner's
//! trust store (`~/.config/muse/trust.json`) trusts it, which is the owner's decision, as for
//! Pi; headless `serve` cannot ask. `--disable-sandbox`: the host's own sandbox stays on in
//! every mode, so a run never has less than the sandbox the owner's Muse gives it.
//! `--approval-mode` does not exist for `serve`: the approval mode is chosen on the wire.

use std::collections::BTreeMap;

use reno_proto::McpServerSpec;
use serde_json::{Value, json};

/// The environment the launcher script needs to leave the installation alone. A run, a probe and
/// every other process Reno starts for Muse sets it, a version check included: the launcher
/// updates on any start, `--version` among them.
pub(crate) const NO_AUTO_UPDATE: (&str, &str) = ("MUSE_NO_AUTO_UPDATE", "1");

/// The provider every run pins.
pub(crate) const PROVIDER: &str = "meta";

/// How a host is started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// A coding session.
    Run,
    /// Discovery (sign-in, models): no shell, no writes, no session log.
    Probe,
}

/// The arguments after the program for `mode`, serving `provider`.
pub(crate) fn serve_args(mode: Mode, provider: &str) -> Vec<String> {
    let mut args: Vec<String> = ["serve", "--provider", provider].map(String::from).into();
    if mode == Mode::Probe {
        args.extend(["--disable-shell", "--disable-write", "--no-session-log"].map(String::from));
    }
    args
}

/// The approval mode and permission profile a run asks of Muse, and what must come back.
///
/// Every approval Muse raises goes to Reno, whatever the access mode: `promptUnmatched` with the
/// `:ask-me` profile asks for what the profile does not already allow, which includes protected
/// writes and new network targets. The kernel then decides (a policy rule, a person, or for a
/// full-access run the access mode, after the built-in protected paths). `allowAll` and
/// `:unrestricted` would take the decision away from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Posture {
    /// The `ApprovalMode` (`promptUnmatched`).
    pub(crate) approval_mode: &'static str,
    /// The permission profile id (`:ask-me`).
    pub(crate) profile: &'static str,
}

impl Posture {
    /// The posture of every run.
    pub(crate) const ASK: Self = Self {
        approval_mode: "promptUnmatched",
        profile: ":ask-me",
    };

    /// Whether a `session/start` or `session/resume` result says the session has this posture.
    /// `Err` names what differs: a run does not go on under a posture it did not ask for.
    pub(crate) fn confirmed_by(&self, session: &Value) -> Result<(), String> {
        let profile = session["permissionState"]["profileId"].as_str();
        if profile != Some(self.profile) {
            return Err(format!(
                "Muse opened the session with permission profile {} instead of {}",
                profile.unwrap_or("(none)"),
                self.profile
            ));
        }
        // A resumed session reports the mode it last had, which `session/setApprovalMode`
        // replaces; only a start reports the mode it was given.
        if let Some(mode) = session["approvalMode"]["mode"].as_str()
            && mode != self.approval_mode
            && session["approvalMode"]["source"] == "startup"
        {
            return Err(format!(
                "Muse opened the session with approval mode {mode} instead of {}",
                self.approval_mode
            ));
        }
        Ok(())
    }

    /// Whether `session/setApprovalMode`'s answer says the mode now in force is this one.
    pub(crate) fn mode_applied(&self, result: &Value) -> Result<(), String> {
        match result["effectiveMode"]["mode"].as_str() {
            Some(mode) if mode == self.approval_mode => Ok(()),
            other => Err(format!(
                "Muse's approval mode is {} instead of {}",
                other.unwrap_or("(unknown)"),
                self.approval_mode
            )),
        }
    }
}

/// `config` for a session: Reno's MCP servers and the plugins' as session-scoped stdio servers.
/// `optional`: a server that cannot start must not fail the turn. `None` when there are none.
/// Muse keeps them for the session only; its own settings are not touched.
pub(crate) fn session_config(servers: &[McpServerSpec]) -> Option<Value> {
    if servers.is_empty() {
        return None;
    }
    let mut mcp = serde_json::Map::new();
    for server in servers {
        let env: BTreeMap<&String, &String> = server.env.iter().collect();
        mcp.insert(
            server.name.clone(),
            json!({
                "transport": "stdio",
                "command": server.command,
                "args": server.args,
                "env": env,
                "mode": "optional",
            }),
        );
    }
    Some(json!({ "mcpServers": mcp }))
}

/// The prompt a run opens with: its standing instructions, then the task. Muse's session config
/// has a `developerPrompt`, but a host only serves it behind a gate that no run can see is open;
/// text in the prompt cannot be silently dropped.
pub(crate) fn opening_prompt(instructions: Option<&str>, prompt: &str) -> String {
    match instructions.map(str::trim).filter(|i| !i.is_empty()) {
        Some(instructions) => format!(
            "<instructions>\n{instructions}\n</instructions>\n\n{prompt}"
        ),
        None => prompt.to_owned(),
    }
}

/// Whether `id` is a session id Muse accepts (a UUID).
pub(crate) fn valid_session_id(id: &str) -> bool {
    id.len() == 36
        && id
            .bytes()
            .all(|b| b.is_ascii_hexdigit() || b == b'-')
        && id.matches('-').count() == 4
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn a_run_is_the_pinned_host_and_a_probe_is_read_only() {
        assert_eq!(
            serve_args(Mode::Run, "meta"),
            ["serve", "--provider", "meta"]
        );
        assert_eq!(
            serve_args(Mode::Probe, "meta"),
            [
                "serve",
                "--provider",
                "meta",
                "--disable-shell",
                "--disable-write",
                "--no-session-log"
            ]
        );
    }

    #[test]
    fn no_run_trusts_a_workspace_or_leaves_the_sandbox() {
        for mode in [Mode::Run, Mode::Probe] {
            for refused in ["--trust-workspace", "--disable-sandbox", "--listen", "--join"] {
                assert!(
                    !serve_args(mode, "meta").iter().any(|a| a == refused),
                    "{refused}"
                );
            }
        }
    }

    #[test]
    fn the_posture_asks_for_everything_and_refuses_what_it_did_not_ask_for() {
        let ask = Posture::ASK;
        let started = json!({
            "approvalMode": {"mode": "promptUnmatched", "source": "startup"},
            "permissionState": {"profileId": ":ask-me"}
        });
        assert!(ask.confirmed_by(&started).is_ok());
        let wrong_profile = json!({"permissionState": {"profileId": ":unrestricted"}});
        assert!(ask.confirmed_by(&wrong_profile).unwrap_err().contains(":unrestricted"));
        assert!(ask.confirmed_by(&json!({})).is_err());
        let wrong_mode = json!({
            "approvalMode": {"mode": "allowAll", "source": "startup"},
            "permissionState": {"profileId": ":ask-me"}
        });
        assert!(ask.confirmed_by(&wrong_mode).unwrap_err().contains("allowAll"));
        // A resumed session shows the mode it last had; setApprovalMode is what decides.
        let resumed = json!({
            "approvalMode": {"mode": "allowAll", "source": "replay"},
            "permissionState": {"profileId": ":ask-me"}
        });
        assert!(ask.confirmed_by(&resumed).is_ok());
        assert!(
            ask.mode_applied(&json!({"effectiveMode": {"mode": "promptUnmatched"}}))
                .is_ok()
        );
        assert!(
            ask.mode_applied(&json!({"effectiveMode": {"mode": "allowAll"}}))
                .is_err()
        );
        assert!(ask.mode_applied(&json!({})).is_err());
    }

    #[test]
    fn servers_become_optional_session_scoped_stdio_servers() {
        assert_eq!(session_config(&[]), None);
        let servers = [
            McpServerSpec {
                name: "reno".into(),
                command: "/opt/reno/reno".into(),
                args: vec!["mcp".into()],
                env: [("RENO_RUN".to_owned(), "r1".to_owned())].into(),
            },
            McpServerSpec {
                name: "plugin-focus".into(),
                command: "/opt/focus/server".into(),
                args: vec![],
                env: Default::default(),
            },
        ];
        let config = session_config(&servers).unwrap();
        assert_eq!(
            config["mcpServers"]["reno"],
            json!({
                "transport": "stdio",
                "command": "/opt/reno/reno",
                "args": ["mcp"],
                "env": {"RENO_RUN": "r1"},
                "mode": "optional"
            })
        );
        assert_eq!(config["mcpServers"].as_object().unwrap().len(), 2);
    }

    #[test]
    fn instructions_go_before_the_task_and_blank_ones_are_nothing() {
        assert_eq!(opening_prompt(None, "task"), "task");
        assert_eq!(opening_prompt(Some("  \n"), "task"), "task");
        assert_eq!(
            opening_prompt(Some("be brief"), "task"),
            "<instructions>\nbe brief\n</instructions>\n\ntask"
        );
    }

    #[test]
    fn session_ids_are_uuids() {
        assert!(valid_session_id("01a1ee73-5121-74d0-b05d-e674c72aeaf4"));
        for bad in ["", "abc", "--help", "01a1ee73-5121-74d0-b05d-e674c72aeaf4x", "../x"] {
            assert!(!valid_session_id(bad), "{bad}");
        }
    }
}
