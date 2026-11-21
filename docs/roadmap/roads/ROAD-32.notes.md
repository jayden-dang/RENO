# ROAD-32 sidebar-and-sessions — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- Thread index as its own table keyed by Reno's own thread id, with the harness session id optional (a draft has none): `crates/agent_ui/src/thread_metadata_store.rs` `ThreadMetadata` ~350 (GPL).
- The live thread rebuilds its row on every update through a write queue deduped by id; reads come from an in-memory map (~1296, ~597-720).
- Sidebar groups by project, merges terminal entries, filters with fuzzy match and highlight positions, renames inline: `crates/sidebar/src/sidebar.rs` `rebuild_contents` ~1512, ~1987-2031, ~3562 (GPL).
- A notification dot when a thread goes Running → Completed while it is not the focused one: `sidebar.rs` ~1908-1963.
- A separate archive view with time buckets and search: `crates/sidebar/src/threads_archive_view.rs`.
- Backend fields live in ROAD-9 notes.
