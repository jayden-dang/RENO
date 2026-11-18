//! The frames Reno writes to the shim, as data.
//!
//! ```text
//! {"op":"run","prompt","cwd","model"?,"modelOptions","resume"?,"mcpServers":[{name,command,args,env}],
//!  "storeDir","inheritUserConfig","silenceTimeoutMs"}      first line; starts the agent and its first turn
//! {"op":"steer","prompt"}                                   input for the live turn (or the next one)
//! {"op":"interrupt"}                                        cancel the live run
//! ```
//!
//! The shim also understands `op:"stop"` (cancel the turn only) and `op:"user"`; Reno has no
//! turn to stop and no parked session to prompt again (a run is one `Done`), so it sends neither.

use std::collections::BTreeMap;

use reno_proto::McpServerSpec;
use serde_json::{Value, json};

/// What a run tells the shim at start.
#[derive(Debug, Clone)]
pub(crate) struct Run<'a> {
    /// The text of the first turn, with any instructions already in front of it.
    pub(crate) prompt: &'a str,
    /// The working directory the SDK's tools run in.
    pub(crate) cwd: &'a str,
    /// The model id, `None` for Cursor's `auto`.
    pub(crate) model: Option<&'a str>,
    /// Typed parameter picks, filled from the catalog.
    pub(crate) model_options: &'a BTreeMap<String, String>,
    /// The agent to resume.
    pub(crate) resume: Option<&'a str>,
    /// The MCP servers the run's agent gets.
    pub(crate) mcp: &'a [McpServerSpec],
    /// The conversation store this run owns.
    pub(crate) store_dir: &'a str,
    /// Load the owner's ambient Cursor settings (user, team, MDM, plugins).
    pub(crate) inherit_user_config: bool,
    /// How long a send may go without one SDK update before the turn is abandoned.
    pub(crate) silence_timeout_ms: u64,
}

impl Run<'_> {
    /// The `run` frame.
    pub(crate) fn frame(&self) -> Value {
        json!({
            "op": "run",
            "prompt": self.prompt,
            "cwd": self.cwd,
            "model": self.model,
            "modelOptions": self.model_options,
            "resume": self.resume,
            "mcpServers": self.mcp.iter().map(|server| json!({
                "name": server.name,
                "command": server.command,
                "args": server.args,
                "env": server.env,
            })).collect::<Vec<_>>(),
            "storeDir": self.store_dir,
            "inheritUserConfig": self.inherit_user_config,
            "silenceTimeoutMs": self.silence_timeout_ms,
        })
    }
}

/// The `steer` frame.
pub(crate) fn steer(prompt: &str) -> Value {
    json!({"op": "steer", "prompt": prompt})
}

/// The `interrupt` frame.
pub(crate) fn interrupt() -> Value {
    json!({"op": "interrupt"})
}

/// The prompt of the first turn: the bot's standing instructions in front of the task.
///
/// The SDK has a `systemPrompt` option, but it replaces Cursor's whole system prompt (the
/// coding-assistant identity, the tool protocol) and is gated per account, so instructions go
/// in front of the first prompt, which is where an executor with no system-prompt channel puts
/// them.
pub(crate) fn first_prompt(instructions: Option<&str>, prompt: &str) -> String {
    match instructions.map(str::trim).filter(|text| !text.is_empty()) {
        Some(instructions) => format!(
            "Standing instructions for this run, from the bot you run as:\n\n{instructions}\n\n---\n\n{prompt}"
        ),
        None => prompt.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_run_frame_carries_everything_the_shim_reads_and_nothing_else() {
        let options = BTreeMap::from([("thinking".to_owned(), "enabled".to_owned())]);
        let mcp = [McpServerSpec {
            name: "reno".into(),
            command: "/path with spaces/reno".into(),
            args: vec!["mcp".into()],
            env: BTreeMap::from([("RENO_RUN".to_owned(), "r1".to_owned())]),
        }];
        let frame = Run {
            prompt: "hi",
            cwd: "/work",
            model: Some("composer-2.5"),
            model_options: &options,
            resume: Some("agent-1"),
            mcp: &mcp,
            store_dir: "/state/agents/x",
            inherit_user_config: true,
            silence_timeout_ms: 1234,
        }
        .frame();
        assert_eq!(
            frame,
            json!({
                "op": "run", "prompt": "hi", "cwd": "/work", "model": "composer-2.5",
                "modelOptions": {"thinking": "enabled"}, "resume": "agent-1",
                "mcpServers": [{"name":"reno","command":"/path with spaces/reno","args":["mcp"],"env":{"RENO_RUN":"r1"}}],
                "storeDir": "/state/agents/x", "inheritUserConfig": true, "silenceTimeoutMs": 1234,
            })
        );
    }

    #[test]
    fn no_model_and_no_resume_are_null() {
        let options = BTreeMap::new();
        let frame = Run {
            prompt: "p",
            cwd: "/w",
            model: None,
            model_options: &options,
            resume: None,
            mcp: &[],
            store_dir: "/s",
            inherit_user_config: false,
            silence_timeout_ms: 1,
        }
        .frame();
        assert!(frame["model"].is_null() && frame["resume"].is_null());
        assert_eq!(frame["mcpServers"], json!([]));
        assert_eq!(frame["inheritUserConfig"], false);
    }

    #[test]
    fn instructions_go_in_front_of_the_task_and_blank_ones_do_not() {
        let with = first_prompt(Some("be brief"), "do it");
        assert!(with.starts_with("Standing instructions"));
        assert!(with.contains("be brief") && with.ends_with("do it"));
        assert_eq!(first_prompt(None, "do it"), "do it");
        assert_eq!(first_prompt(Some("  \n"), "do it"), "do it");
    }

    #[test]
    fn steering_and_interrupt_are_one_op_each() {
        assert_eq!(steer("go"), json!({"op":"steer","prompt":"go"}));
        assert_eq!(interrupt(), json!({"op":"interrupt"}));
    }
}
