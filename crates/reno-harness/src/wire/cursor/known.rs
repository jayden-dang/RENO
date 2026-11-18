//! The frames the shim emits and the tool names the SDK reports that the driver knows by name.
//! One outside these tables is dropped and counted (`Executor::unknown_frames`), the early
//! signal that the SDK or the shim moved. The SDK is a public beta with about weekly releases:
//! a count is expected after a bump, and is what tells a person to re-read the typings.

/// `ev` values of a run's stdout. (`models` also belongs to the `models` mode, which is read
/// by its own code.)
pub(crate) const KNOWN_FRAMES: &[&str] = &[
    "ready", "steered", "text", "thinking", "tool", "usage", "turn", "fatal",
];

/// The SDK's tool vocabulary as the shim forwards it (`toolCall.type`), read off 1.0.32's
/// `tool-call-types.d.ts` and the shim's own handling. A tool outside it still runs and is shown
/// as an `Other` call; it is counted as `tool:<name>`.
pub(crate) const KNOWN_TOOLS: &[&str] = &[
    "shell",
    "read",
    "write",
    "edit",
    "delete",
    "grep",
    "glob",
    "ls",
    "webSearch",
    "webFetch",
    "updateTodos",
    "readTodos",
    "mcp",
    "task",
    "readLints",
    "semSearch",
    "createPlan",
    "generateImage",
    "recordScreen",
    "applyAgentDiff",
    "await",
    "askQuestion",
    // What the shim names a tool the SDK gave no type for.
    "tool",
];

/// Whether `ev` is a frame the driver knows.
pub(crate) fn is_known_frame(ev: &str) -> bool {
    KNOWN_FRAMES.contains(&ev)
}

/// Whether `name` is a tool the driver knows.
pub(crate) fn is_known_tool(name: &str) -> bool {
    KNOWN_TOOLS.contains(&name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tables_have_no_duplicates() {
        for table in [KNOWN_FRAMES, KNOWN_TOOLS] {
            let mut sorted = table.to_vec();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), table.len());
        }
    }

    #[test]
    fn a_future_frame_and_a_future_tool_are_not_known() {
        assert!(is_known_frame("turn") && !is_known_frame("hologram"));
        assert!(is_known_tool("shell") && !is_known_tool("teleport"));
    }
}
