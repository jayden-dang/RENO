# ROAD-14 plan-and-todo-events — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- Plans are a map keyed by `PlanIdentity`, content `Items` or `Markdown{path}`, with `PlanStats` (in progress, pending, completed, cancelled); when the visible plan goes away the most recently changed one shows: `crates/acp_thread/src/acp_thread.rs` ~3480-3515 (GPL) → `crates/reno-proto/src/exec.rs`.
