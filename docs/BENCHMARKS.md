# Benchmarks

`cargo xtask bench` measures the daemon against the budgets in ARCHITECTURE 12. It builds `renod`
and `reno` in release, starts `renod` on a temporary data directory with `RENO_NO_HARNESSES=1`
(only the deterministic `mock` executor), and drives it through the `reno` CLI.

```
cargo xtask bench                       # full: 30 s settle, 60 s idle CPU, 1,000 runs
cargo xtask bench --quick               # 5 s, 10 s, 200 runs (what CI runs)
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
| 1,000 mock runs | `reno task run --executor mock`, 8 at a time; RSS sampled every 100 ms for the peak; the "after" RSS is the median of 3 samples once it has idled for 30 s | after within 10 % of before |
| Per live run | A second daemon whose mock holds until cancelled. 20 runs, each followed by one `reno` process. (RSS with all 20 live, median of 5, minus idle RSS) / 20. Includes the daemon's side of one subscriber connection per run | <= 4 MiB |
| Claim to start | From the stopped daemon's `events` table: `run.claimed` to the `run.transitioned` that makes the run `running`, which the dispatcher commits right before it calls the executor's `start`. The mock starts instantly, so this is the kernel's claim-to-spawn cost. Event timestamps are whole milliseconds, so 0 means under 1 ms | p99 < 50 ms |
| Event append | `JournalWriter::append` plus `flush`, one event at a time, 10,000 events after 200 warm-up events, on a temp directory. `flush` is a `write(2)`, with no `fsync`, as in the daemon | p99 < 200 us |
| Boot | Time from spawning `renod` until its socket accepts a connection (after recovery) on the data directory of the runs scenario, three boots; plus a boot of an empty store for comparison | <= 300 ms (ROADMAP threshold for limiting journal repair) |

A first start of a binary the OS has not run before is slow, so the benchmark starts and stops one
throwaway daemon first.

## Where the numbers come from

CI runs `cargo xtask bench --quick` on Linux and fails only when a number is past a budget. The
macOS numbers, and the full 1,000-run ones, are taken locally with `cargo xtask bench`: shared macOS
runners are too noisy to hold a budget, and the macOS allocator is the reason for the allocator
decision below. Each phase's closing change reports the local numbers.

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
collect, or if the Linux numbers say otherwise. The app was not measured.
