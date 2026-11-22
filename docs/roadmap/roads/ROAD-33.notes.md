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

## Frame-time target from ROAD-29 (2026-11-21)

`cargo xtask bench --app` measures a 2,000-message transcript at p50 29.7 ms, p95 31.4 ms per
frame while scrolling. The signed-off target is p95 ≤ 16.7 ms (one 60 Hz frame), and the bench
reports "over (ROAD-33)" until this item lands (`docs/BENCHMARKS.md`, App).

Corrected 2026-11-22. This was read from the code, not profiled:
- The scenario is one turn: 2,000 × (text, tool step).
- The bench opens every fold (`RENO_OPEN_FOLDS=1`).
- The open "Worked for" fold renders all ~4,000 child rows inside one list item
  (`screens/run/transcript.rs`, `worked()`).
- The list skips whole items, never the children of one item, so every frame lays them all out.

So the bench is evidence for one list item per entry, with the children of an open fold as items
of their own. It is not evidence for one row per markdown block. No bench measures a single very
long reply yet.

## Frame, 2026-11-22

The owner's card is [ROAD-33](ROAD-33.md). The frame record is on the ticket.
