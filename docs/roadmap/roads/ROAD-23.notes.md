# ROAD-23 lead-answers-child-questions — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- A child's permission requests surface on the parent (`subagents_awaiting_permission`, `crates/agent_ui/src/conversation_view.rs` ~508, GPL) → `apps/reno/src/mcp.rs`. The request lifecycle itself is in ROAD-1 notes.
