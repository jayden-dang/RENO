# ROAD-29 app-budgets-and-profiling — notes

Learnings from Zed (`zed-industries/zed` at `2f808b3`, read 2026-11-21) for `frame-change` to
verify and take further. They are pointers, not decisions. Zed paths are relative to the Zed repo.
License: (GPL) = learn the pattern, never copy code; (Apache) = may be ported with its notice.

- Hang detection: `crates/gpui/src/profiler/hang.rs`, `App::start_hang_monitor` in `crates/gpui/src/app.rs` (Apache, present at the pinned gpui `596dfdc`) → a bench scenario in `xtask/src/bench.rs`: no foreground stall over the frame budget while a run streams.
- Input-latency histogram: buckets `FRAME_DURATION_BUCKETS_MS` in `crates/gpui/src/profiler.rs` (Apache), viewer `crates/input_latency_ui` (GPL) → a debug action in `apps/reno-app/`.
- Per-task poll timings with spawn location (`#[track_caller]`): `crates/miniprofiler_ui` (GPL) over gpui's `ProfilingCollector`.
- Allocation counts as a steadier secondary metric than ms: `CountingAllocator` in `crates/bench_metrics` (Apache). Retired-instruction counters need a Linux PMU: check GitHub-hosted runners expose one before relying on it.
- Render benches on the headless pipeline: `#[gpui::bench]` in `crates/gpui_macros/src/bench.rs` (Apache), examples in `crates/benchmarks/benches/` (GPL).
- Baseline vs candidate: `script/gpui-bench-compare` builds two revisions, alternates runs, reports the spread of medians.
- Zed builds its benches in CI but gates none; Reno's budgets already go further.
