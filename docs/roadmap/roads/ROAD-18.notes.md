# ROAD-18 executor-catalog-depth — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- A connection trait whose optional abilities are gated on the agent's declared capabilities: `load_session`, `resume_session`, `close_session`, `session_list`, `cancel_tool_call`: `crates/acp_thread/src/connection.rs` (GPL) → `crates/reno-harness/src/drivers/acp/`, `wire/acp/`.
- ACP v2 first, v1 fallback on JSON-RPC error `-32022` with `data.supported=[1]`: `crates/agent_servers/src/acp.rs` ~130-170.
- Updates that arrive before the session is registered are buffered, capped at 1024 entries / 16 MB, with a generation counter: `crates/agent_servers/src/acp/session_updates.rs`.
- All inbound traffic goes through one ordered queue with a disconnect barrier; a process exit is held 250 ms to become a meaningful error (`acp.rs` ~245-450).
- A raw JSON-RPC log per connection with a viewer: `crates/agent_servers/src/acp/debug_log.rs`, `crates/acp_tools` (GPL). Fits the transparent-orchestrator vision; check whether run journals keep raw frames.
- A shared e2e suite generic over agents: `crates/agent_servers/src/e2e_tests.rs` `common_e2e_tests!`; a fake ACP agent driving the real client: `acp/v1.rs` ~1859 `FakeAcpAgentServer`.
