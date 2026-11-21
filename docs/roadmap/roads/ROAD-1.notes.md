# ROAD-1 answer-harness-questions — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- Each permission request gets a UUID and a `oneshot` responder kept in an ordered map; the tool call moves to `WaitingForConfirmation`: `crates/acp_thread/src/acp_thread.rs` `request_permission` ~6794, `authorize_tool_call` ~7025 (GPL) → `crates/reno-kernel/src/supervisor/approval.rs`, `crates/reno-harness/src/wire/acp/permission.rs`.
- Outcomes `Cancelled | InterruptedByFollowUp | Selected{option_id, kind, params}`; cancelling a run answers every pending request with `Cancelled` (`cancel` ~7823).
- The agent's `$/cancelRequest` removes the card (`responder.cancellation()`), and `answers_permission_requests_on_cancel` lets an agent answer them itself: `crates/acp_thread/src/connection.rs` ~146.
- Child questions bubble to the parent (`subagents_awaiting_permission`, `conversation_view.rs` ~508), see ROAD-23.
- Reno already answers an unanswered request as `cancelled` (`permission.rs:204-241`); check the race paths: cancel while waiting, the agent's cancel, a follow-up that interrupts.
