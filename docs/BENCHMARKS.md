# Benchmarks

`cargo xtask bench` measures the daemon against the budgets in ARCHITECTURE 12. It builds `renod`
and `reno` in release, starts `renod` on a temporary data directory with `RENO_NO_HARNESSES=1`
(only the deterministic `mock` executor), and drives it through the `reno` CLI.

```
cargo xtask bench                       # full: 30 s settle, 60 s idle CPU, 4 batches of 1,000 runs
cargo xtask bench --quick               # 5 s, 10 s, 1,800 warm-up runs, 4 batches of 200 (what CI runs)
cargo xtask bench --compare-allocators  # system and mimalloc, built and measured one after the other
cargo xtask bench --allocator mimalloc  # one named build instead of the default features
```

It writes `target/bench/report.json` and `target/bench/report.txt`, and exits 1 when any number is
past its budget. Benchmarks never run under `cargo test`: the xtask tests are pure, and
`crates/reno-store/examples/store_bench.rs` is only compiled by it.

## Method

| Scenario | How it is measured | Budget |
|---|---|---|
| Idle RSS | `ps -o rss=` (median of 3) after the settle time | <= 40 MiB |
| Idle CPU | CPU seconds of the process at the start and end of the window (`/proc/PID/stat` on Linux, `ps -o time=` on macOS, 10 ms resolution) over the window | < 0.5 % |
| Mock runs, 4 batches | `reno task run --executor mock`, 8 at a time, in 4 batches of 1,000; RSS sampled every 100 ms for the peak; after each batch, once the daemon has idled for 30 s, RSS is the median of 3 samples. The series is reported | **(a) absolute:** RSS after the first 1,000 runs <= 40 MiB. **(b) steady state:** the 3 further batches add <= 1 MiB per 1,000 runs on average, that is (batch 4 - batch 1) / 3 |
| Per live run | A second daemon whose mock holds until cancelled. 20 runs, each followed by one `reno` process. (RSS with all 20 live, median of 5, minus idle RSS) / 20. Includes the daemon's side of one subscriber connection per run | <= 4 MiB |
| Claim to start | From the stopped daemon's `events` table: `run.claimed` to the `run.transitioned` that makes the run `running`, which the dispatcher commits right before it calls the executor's `start`. The mock starts instantly, so this is the kernel's claim-to-spawn cost. Event timestamps are whole milliseconds, so 0 means under 1 ms | p99 < 50 ms |
| Event append | `JournalWriter::append` plus `flush`, one event at a time, 10,000 events after 200 warm-up events, on a temp directory. `flush` is a `write(2)`, with no `fsync`, as in the daemon | p99 < 200 us |
| Boot | Time from spawning `renod` until its socket accepts a connection (after recovery) on the data directory of the runs scenario, three boots; plus a boot of an empty store for comparison | <= 300 ms (ROADMAP threshold for limiting journal repair) |

A first start of a binary the OS has not run before is slow, so the benchmark starts and stops one
throwaway daemon first.

### Two budgets for the runs, and why

One "within 10 % of idle" number cannot tell warm-up from a leak. SQLite's page caches (one writer
and two readers, 2 MiB each) fill as the database grows, and that is a one-off, bounded cost; a
leak is growth that keeps going with run history (ARCH-12). So:

- **(a)** is the daemon budget: RSS after the first 1,000 runs, warm-up included, <= 40 MiB.
- **(b)** is the leak check: what the next 1,000-run batches add, on average, <= 1 MiB each.

`--quick` uses 200-run batches. The caches are still filling at 1,000 runs (a 200-run first batch,
or one that ends at run 1,000, read the fill as 2 to 5 MiB per 1,000 runs), so a quick run first
does 3,800 unmeasured warm-up runs (a couple of seconds), which makes the end of its first batch
the 4,000th run, the same point as batch 4 of a full run. (a) is read there, which is stricter than
at run 1,000 because RSS only rose. (b) is the mean growth per 200-run batch, scaled to 1,000 runs
(times 5). It sees 600 runs instead of 3,000, so it is the noisier of the two: RSS moves by 0.1
to 0.3 MiB between readings with no change in the daemon, which scaling turns into 0.2 to 0.5 MiB
per 1,000 runs. Its limit is therefore **3 MiB per 1,000 runs**, which still fails the 3.8 measured
before the fix; the full run keeps 1 MiB and is the authority. Eleven quick runs, with warm-ups of
1,800 and 3,800, read between 0.49 and 2.21 (0.57, 0.49, 0.52, 0.96, 0.62, 0.52, 1.12, 2.21, 0.96,
0.83 and the first, 2.71, before the warm-up).

## Where the numbers come from

CI runs `cargo xtask bench --quick` on Linux and fails only when a number is past a budget. The
macOS numbers, and the full 1,000-run ones, are taken locally with `cargo xtask bench`: shared macOS
runners are too noisy to hold a budget, and the macOS allocator is the reason for the allocator
decision below. Each phase's closing change reports the local numbers.

## Run history and memory (T0.9 follow-up)

The runs hub used to reload the whole `runs` table after every transition and `run.list` returned
it all, so memory and frame size grew with history. Now `runs.watch` carries every non-terminal run
(at most `watch_limit`, 500) plus the newest `recent_runs` (50) ended ones, from two indexed,
bounded queries; `run.list` pages (`limit` 100, at most 500, `before` cursor, `next` in the
result); and list and watch summaries cut `prompt` to 280 characters (`run.get` has it whole).

macOS, aarch64, release, system allocator, full mode, RSS after each batch of 1,000 runs:

| | batch 1 | batch 2 | batch 3 | batch 4 | (b) per 1,000 |
|---|---|---|---|---|---|
| before (`3a5196f`, new method) | 24.2 | 30.4 | 34.5 | 35.7 MiB | 3.82 MiB, fails |
| after | 24.1 | 24.6 | 25.2 | 25.3 MiB | 0.43 MiB, passes |

What still grows after the fix: batch 1 to 4 is warm-up, mostly the page caches. A longer
profile (500-run batches, 10 of them, `heap` and `vmmap --summary`) shows RSS flatten near 25 MiB
at about 2,000 runs, then rise about 0.3 MiB per 1,000 runs. The 4 KiB-page (5 KB malloc) count
went from 332 to 948 and stopped there, which is the 3 x 2 MiB cache cap. `leaks` finds no
unreachable memory. The small residual is one 80 B, one 32 B and one 16 B allocation per run
(about 130 B), and one buffer that grew to 1.6 MiB; the release binary is stripped, so the owner
is not identified. Hypothesis: a per-run entry kept in a task, channel or map table. Unverified.

### The mock's log (after Phase 1 regressed the growth to 2.46 MiB per 1,000 runs)

Phase 1 measured 33.2, 36.0, 38.4, 40.6 MiB (2.46 MiB per 1,000 runs, over budget). A release
build with symbols and `MallocStackLogging=1` (`heap`, `malloc_history -allBySize`) showed live
malloc nodes rising by about 18 per run (22,284 after 800 runs, 36,718 after 1,600), every
one of them a clone of the run's `ExecSpec` (args, env, `McpServerSpec` list) made in
`dispatcher::attempt`. The owner was `MockExecutor`'s `MockLog`, which records every spec it
starts, and the daemon's own `mock` executor kept that log for the life of the process. Tests
read the log, so it stays the default; the daemon's mock is now built with `without_log()`.
The residual "one 80 B, 32 B and 16 B allocation per run" above was the same log. No other
structure was found to grow with finished runs (health board, routing, tokens, link and
approval maps were checked in the heap diff and are flat).

Full mode after the fix: 29.3, 31.5, 31.9, 31.9 MiB, **0.86 MiB per 1,000 runs**, every budget
passes. The 3 x 2 MiB page caches fill in the first few thousand runs; a longer profile
(8 batches of 1,000, symbols build) reads 31.5, 31.7, 31.9, 32.0, 32.1, 32.1, 32.1, 32.1 MiB: a
plateau.

## Allocator decision (ARCHITECTURE 17.8)

`mimalloc` (`v2` feature, MIT) is an optional feature of `renod` and `reno-app`
(`--features mimalloc`) that installs it as the global allocator. It is **off by default**: on
Apple silicon, release builds, the system allocator won on every measure (two full runs each).

| macOS, aarch64, release | idle RSS | RSS after 1,000 runs | growth | peak |
|---|---|---|---|---|
| system, run 1 | 14.6 MiB | 26.1 MiB | +78 % | 24.9 MiB |
| system, run 2 | 14.6 MiB | 24.5 MiB | +68 % | 24.6 MiB |
| mimalloc, run 1 | 15.8 MiB | 54.3 MiB | +243 % | 52.0 MiB |
| mimalloc, run 2 | 15.8 MiB | 57.4 MiB | +264 % | 52.8 MiB |

Hypothesis (not isolated): mimalloc returns memory to the OS only while the process allocates, so a daemon that goes quiet
after a burst keeps the burst's pages. Revisit the decision if the daemon gets a periodic allocator
collect, or if the Linux numbers say otherwise. The app was not measured; see "App" below, where its allocator is still to measure.

## App

`cargo xtask bench --app` measures `reno-app` the way the daemon is measured, but only on a Mac, by
hand, and never in CI or under `cargo test`: shared runners are too noisy to hold a UI budget. It
builds `renod`, `reno` and `reno-app` in release, starts its own `renod` on a temporary data
directory (mock executor only), and starts the app against it through `RENO_DATA_DIR`. It never
uses `--fake`, because the fake daemon runs inside the app and would be counted as the app's cost,
and it never reaches the installed daemon. It writes `target/bench/app-report.json` and
`app-report.txt`, and exits 1 when a number is past a budget.

```
cargo xtask bench --app                       # full: 30 s settle, 120 s idle, 60 s streaming, long transcript
cargo xtask bench --app --quick               # shorter windows, 3 cold starts
cargo xtask bench --app --quick --idle-only   # cold start and idle only (no scenario that streams or scrolls)
cargo xtask bench --app --compare-allocators  # system and mimalloc, built and measured one after the other
```

**The bench opens a real window and brings it to the front.** GPUI draws no frame for a window that
is covered, hidden or behind a full-screen app, and a covered window's CPU is a falsely low number.
Under `RENO_BENCH=1`, and only then, the app activates itself and orders its window to the front
when it opens. The scenarios that need frames take the screen for a few minutes: do not use the
machine meanwhile. Where frames still do not arrive, the scenario says "window not drawn" and
reports no number.

### Method

| Scenario | How it is measured | Proposed budget |
|---|---|---|
| Cold start to first frame | Spawn the app and time from the spawn to the first frame. Under `RENO_BENCH=1` the app prints `first-frame <ms>` after its first paint (and the same time from the start of `main`). Median of 5 after one throwaway start | < 400 ms |
| Idle CPU | CPU seconds of the process (`ps -o time=`) over the idle window, after the settle time, on Home with nothing live | < 1 % of a core |
| Idle wakeups | `top -l 2 -s <window> -stats idlew`, no sudo. `IDLEW` is a running count since the process started, so the rate is the difference between the two samples over the window | < 1 per second |
| Idle footprint | `footprint` (or `vmmap --summary`): physical footprint, which includes the GPU's `IOAccelerator` memory that RSS misses | baseline + 25 %, at most 200 MiB |
| Streaming | `renod --stream-mock`, a bench-only script that emits text deltas at a steady rate (about 20 events a second for 60 s, markdown with code blocks); the app shows that run. CPU and peak footprint over the window; fails as "window not drawn" if it drew fewer frames than seconds | CPU <= 15 % · footprint <= 350 MiB |
| Long transcript | `renod --stream-mock=burst` finishes a run of 2,000 messages; under `RENO_BENCH=1` with `RENO_OPEN=run_<id>` the app scrolls the transcript from end to start and back by a fixed step a frame. Peak footprint and frame time p95 | footprint: baseline + 25 %; frame time recorded, it informs the transcript's row model |
| Allocator | `--compare-allocators`: idle plus streaming, once with the system allocator and once with mimalloc | decision recorded, as for the daemon |
| GPU memory | The `IOAccelerator` share of the footprint, on its own | recorded, no budget |

The budgets are proposed, not signed off. Each is the measured baseline plus a stated margin, and
none is looser than the figures in ARCHITECTURE 12. The report prints numbers and marks the
budgets "proposed" until they are signed off after the first full measurement.

### Measured (quick, idle only)

2026-11-20, Apple M5 Pro, macOS 27.0.1, release build of the `feat/app-bench` branch, system
allocator, `cargo xtask bench --app --quick --idle-only` (5 s settle, 20 s idle window).

| Metric | Measured | Proposed budget |
|---|---|---|
| Cold start to first frame (median of 3) | 142 ms (the app's own clock from `main`: 126 to 132 ms) | < 400 ms |
| Idle CPU | 0.4 % of a core | < 1 % |
| Idle wakeups | 0.05 per second | < 1 per second |
| Idle footprint | 53 MiB | baseline + 25 %, at most 200 MiB |
| GPU memory in the footprint | 4 MiB | recorded |
| Streaming CPU and peak footprint | to measure | <= 15 % · <= 350 MiB |
| Long transcript footprint and frame time p95 | to measure | footprint: baseline + 25 % |
| App allocator (system against mimalloc) | to measure | decision recorded |

Two things in the first, hand-run numbers were the bench's, not the app's, and are fixed:

- An earlier run reported 17 idle wakeups a second. `IDLEW` is a count since the process started,
  and the bench read it as a rate. Idle, the app wakes about once every 5 to 20 s; no timer runs on
  Home with nothing live (the 250 ms activity ticker exists only with `--fake`).
- That run also had its window behind another app, so it drew nothing: 329 ms to the first frame
  and 32 MiB were the numbers of a window that was not on screen. With the window in front they
  are 142 ms and 53 MiB.

Quick mode's 5 s settle is short, which is why its idle CPU (0.4 %) reads above the 0.0 to 0.1 %
a settled window shows; the full run settles for 30 s.
