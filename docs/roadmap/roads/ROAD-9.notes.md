# ROAD-9 thread-names-pins-archive — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- A user rename writes `title_override`, separate from the generated title, so a later agent title never clobbers it: `crates/agent_ui/src/thread_metadata_store.rs` ~351-354, ~754, ~773 (GPL) → `crates/reno-proto/src/thread.rs` `ThreadInfo`.
- Archive is a flag; archiving can snapshot and remove the git worktree with a link table so restore works (~1091-1171); archived rows keep their old paths because the project may be gone (~1351).
- A thread started with no folder is archived by default so it never vanishes (~1371).
- Zed has no per-thread pin in this build (pin is per workspace row); Reno needs its own column.
- `interacted_at` counts queueing a message but not its firing (~360).
