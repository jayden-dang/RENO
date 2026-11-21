# ROAD-49 journaled-flows — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- Deterministic simulation: a seeded scheduler with a fake clock, randomized task order, timers that cost no wall time, and a panic when nothing can run instead of a hang; a failure prints `SEED=N`: `crates/scheduler/src/test_scheduler.rs` (Apache) → `crates/reno-kernel/` tests over the `Clock` port.
- Randomized multi-client plans saved and replayed (`SAVE_PLAN` / `LOAD_PLAN`), with DB failure injection: `crates/collab/tests/integration/randomized_test_helpers.rs`, `db_tests.rs` (GPL).
- For Reno: seeded sequences of start, cancel, answer, daemon restart, client disconnect, sleep/wake and executor crash, checking fences, journals and that no task is orphaned.
