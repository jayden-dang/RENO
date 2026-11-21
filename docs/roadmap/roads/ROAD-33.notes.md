# ROAD-33 transcript — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- UI-free thread model emits fine-grained events `NewEntry`, `EntryUpdated(ix)`, `EntriesRemoved(range)`; the view splices or remeasures only that row: `crates/agent_ui/src/conversation_view.rs` (`ListState` ~1708, `handle_thread_event` ~2065) (GPL) → `apps/reno-app/src/transcript.rs`, `vm/convo.rs`.
- Per-entry view-state sidecar (expanded sets, editors, terminal views) kept outside the data: `crates/agent_ui/src/entry_view_state.rs` (GPL).
- Markdown parses on a background task and coalesces (`pending_parse` + `should_reparse`); render uses the last parse: `crates/markdown/src/markdown.rs` `append` ~1019 (GPL) → `crates/reno-markdown/`, `ui/markdown_view.rs`.
- 16 ms reveal buffer targeting ~200 ms instead of re-rendering per chunk: `StreamingTextBuffer`, `crates/acp_thread/src/acp_thread.rs` ~3880 (GPL).
- Tool-call content materialized lazily on restore (`ToolCallSync::{Live, Restore, Materialize}`).
- Reno already uses `ListState`/`uniform_list`; check the event granularity and where markdown parses.
- Measure with `#[gpui::bench]` (see ROAD-29 notes): 10k entries plus a live stream.
