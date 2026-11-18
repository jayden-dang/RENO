//! The frames Pi's RPC mode emits that the driver knows by name, handled or ignored on purpose.
//! A frame outside this table is one the driver has never seen: it is dropped and counted
//! (`Executor::unknown_frames`), the early signal that Pi moved.
//!
//! The names are the stdout records of Pi 1.5.0's `docs/json.md`, `docs/rpc-extension-ui.md`
//! and `dist/core/agent-session.d.ts`, plus the agent-core events (`agent_*`, `turn_*`,
//! `message_*`). `extension_ui_request` methods and the nested `assistantMessageEvent` types are
//! checked separately, so a new dialog kind or content block counts as unknown too.

/// Top-level `type`s that are not unknown.
pub(crate) const KNOWN_FRAMES: &[&str] = &[
    // A reply to a command.
    "response",
    // Run lifecycle.
    "agent_start",
    "agent_end",
    "agent_settled",
    "turn_start",
    "turn_end",
    // Messages and tools.
    "message_start",
    "message_update",
    "message_end",
    "tool_execution_start",
    "tool_execution_update",
    "tool_execution_end",
    // Queue, session and settings chatter.
    "queue_update",
    "entry_appended",
    "session_info_changed",
    "thinking_level_changed",
    // Compaction and retry.
    "compaction_start",
    "compaction_end",
    "auto_retry_start",
    "auto_retry_end",
    "summarization_retry_scheduled",
    "summarization_retry_attempt_start",
    "summarization_retry_finished",
    // RPC only.
    "bash_execution_update",
    "extension_error",
    "extension_ui_request",
];

/// `assistantMessageEvent.type`s of a `message_update`.
pub(crate) const KNOWN_MESSAGE_EVENTS: &[&str] = &[
    "start",
    "text_start",
    "text_delta",
    "text_end",
    "thinking_start",
    "thinking_delta",
    "thinking_end",
    "toolcall_start",
    "toolcall_delta",
    "toolcall_end",
    "done",
    "error",
];

/// `extension_ui_request` methods. The first four are dialogs that wait for an answer; the rest
/// are fire-and-forget.
pub(crate) const DIALOG_METHODS: &[&str] = &["select", "confirm", "input", "editor"];
pub(crate) const FIRE_AND_FORGET_METHODS: &[&str] = &[
    "notify",
    "setStatus",
    "setWidget",
    "setTitle",
    "set_editor_text",
];

/// Whether a top-level frame `kind` is one the driver knows.
pub(crate) fn is_known_frame(kind: &str) -> bool {
    KNOWN_FRAMES.contains(&kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tables_have_no_duplicates() {
        for table in [
            KNOWN_FRAMES,
            KNOWN_MESSAGE_EVENTS,
            DIALOG_METHODS,
            FIRE_AND_FORGET_METHODS,
        ] {
            let mut sorted = table.to_vec();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), table.len());
        }
    }

    #[test]
    fn a_future_frame_is_not_known() {
        assert!(is_known_frame("agent_settled"));
        assert!(!is_known_frame("agent_teleported"));
    }
}
