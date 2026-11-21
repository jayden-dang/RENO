# ROAD-10 run-control — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- Message queue entries carry a `steer` flag; state `AutoProcess | Paused | AbsorbingCancel`: `crates/agent_ui/src/conversation_view/message_queue.rs` (210 lines, GPL) → `crates/reno-proto/src/exec.rs` `Steering` (exists), `crates/reno-kernel/src/supervisor.rs`.
- "Send now" cancels the turn, then swallows the resulting Stopped event so nothing is sent twice; Stop pauses the queue instead of auto-sending; no auto-send while the owner edits the next message; a failed send goes back to its position and pauses; the queue survives a reload (`QueuedMessageRecovery`, `thread_view.rs` ~86-120).
- A steer entry asks the agent to end the turn at the next boundary (`set_end_turn_at_next_boundary`, `thread_view.rs` ~3064).
- Interrupt vs end: `AcpThread::cancel` ends the turn; `cancel_tool_call` stops one tool call without cancelling the session (`acp_thread.rs` ~7823, ~7831).
