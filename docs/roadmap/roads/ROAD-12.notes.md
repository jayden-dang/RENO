# ROAD-12 subagent-and-child-events — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- The parent's tool call carries `SubagentSessionInfo{session_id, message_start_index, message_end_index}` under meta key `subagent_session_info`: `crates/acp_thread/src/acp_thread.rs` ~164, ~317-333 (GPL) → `crates/reno-proto/src/exec.rs`, `crates/reno-harness/src/wire/`.
- ACP forbids loading a subagent session on its own, so the parent owns it.
- A child's file changes mirror into the parent's change log (`ActionLog::with_linked_action_log`, `crates/action_log/src/action_log.rs` ~57-79, ~543-589).
- Zed caps nesting at `MAX_SUBAGENT_DEPTH = 1` (`crates/agent/src/thread.rs` ~78).
