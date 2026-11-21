# ROAD-21 delegation-tools — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- The `spawn_agent` tool's doc comment is a good delegation prompt: self-contained messages, disjoint write sets, no duplicated work, reuse `session_id` for a follow-up: `crates/agent/src/tools/spawn_agent_tool.rs` (GPL) → `apps/reno/src/mcp.rs`.
- Input is label, message, optional session id, optional model.
