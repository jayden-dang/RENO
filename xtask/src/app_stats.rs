//! The pure half of `cargo xtask bench --app`: parsers for what `top`, `footprint` and `vmmap`
//! print and for the lines the app prints under `RENO_BENCH=1`, the budgets, and the report.
//! Nothing here touches the machine, so all of it is unit-tested.

use std::fmt::Write as _;

use serde::Serialize;

use crate::stats::percentile;

/// A budget: the number a measurement must stay at or under.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Budget {
    pub(crate) key: &'static str,
    pub(crate) label: &'static str,
    pub(crate) unit: &'static str,
    /// `None`: recorded, no budget.
    pub(crate) limit: Option<f64>,
    /// `Some(road)`: the limit is a target that roadmap item must meet, not a budget the app
    /// holds today. A number past it is reported ("over (ROAD-33)") and does not fail the run.
    pub(crate) target: Option<&'static str>,
    pub(crate) basis: &'static str,
}

/// The one table of app budgets, signed off by the owner on 2026-11-21 (measured on an M5 Pro,
/// macOS 27.2, release build, system allocator). A limit here is read by the report and nowhere
/// else. A number past a budget fails the run, except the one marked as a target.
pub(crate) const BUDGETS: &[Budget] = &[
    Budget {
        key: "cold_start_ms",
        label: "Cold start to first frame (median of 5)",
        unit: "ms",
        limit: Some(400.0),
        target: None,
        basis: "ARCHITECTURE 12",
    },
    Budget {
        key: "idle_cpu_pct",
        label: "Idle CPU (% of one core)",
        unit: "%",
        limit: Some(1.0),
        target: None,
        basis: "no polling; frames only on change",
    },
    Budget {
        key: "idle_wakeups_per_s",
        label: "Idle wakeups",
        unit: "/s",
        limit: Some(1.0),
        target: None,
        basis: "no timers while nothing moves",
    },
    Budget {
        key: "idle_footprint_mib",
        label: "Idle footprint (physical, GPU included)",
        unit: "MiB",
        limit: Some(135.0),
        target: None,
        basis: "measured 108 MiB, +25 %",
    },
    Budget {
        key: "streaming_cpu_pct",
        label: "Streaming CPU (% of one core)",
        unit: "%",
        limit: Some(15.0),
        target: None,
        basis: "plan figure; measured 6 % once the redraw was fixed",
    },
    Budget {
        key: "streaming_footprint_mib",
        label: "Streaming footprint (peak)",
        unit: "MiB",
        limit: Some(260.0),
        target: None,
        basis: "measured 208 MiB, +25 %",
    },
    Budget {
        key: "long_footprint_mib",
        label: "Long transcript footprint (peak)",
        unit: "MiB",
        limit: Some(370.0),
        target: None,
        basis: "measured 294 MiB, +25 %",
    },
    Budget {
        key: "long_frame_p95_ms",
        label: "Long transcript frame time p95",
        unit: "ms",
        limit: Some(16.7),
        target: Some("ROAD-33"),
        basis: "one 60 Hz frame; measured p50 29 ms for 2,000 messages in 4 rows, ROAD-33 makes it",
    },
    Budget {
        key: "gpu_share_mib",
        label: "GPU memory (IOAccelerator, in the footprint)",
        unit: "MiB",
        limit: None,
        target: None,
        basis: "recorded",
    },
];

pub(crate) fn budget(key: &str) -> &'static Budget {
    BUDGETS
        .iter()
        .find(|b| b.key == key)
        .unwrap_or_else(|| panic!("no budget {key}"))
}

// --- parsers ---------------------------------------------------------------------------------

/// A size as `footprint` and `vmmap` print it: `2608 KB`, `1.5 GB`, `62.1M`, `848` (bytes).
/// Returns MiB.
pub(crate) fn parse_size_mib(text: &str) -> Option<f64> {
    let text = text.trim();
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let number: f64 = text[..split].parse().ok()?;
    let bytes = match text[split..].trim().to_ascii_uppercase().as_str() {
        "" | "B" => number,
        "K" | "KB" => number * 1024.0,
        "M" | "MB" => number * 1024.0 * 1024.0,
        "G" | "GB" => number * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some(bytes / (1024.0 * 1024.0))
}

/// What `top -l 2 -s SECONDS -pid PID -stats pid,cpu,idlew` said about the process.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub(crate) struct TopSample {
    /// CPU over the last interval.
    pub(crate) cpu_pct: f64,
    /// Idle wakeups per second over the interval between the two samples. `IDLEW` is a count
    /// since the process started (a trailing `+` marks a rising one), so the rate is the
    /// difference between the samples over the time between them.
    pub(crate) idle_wakeups_per_s: f64,
}

/// `seconds` is the interval `top` was asked to wait between its two samples.
pub(crate) fn parse_top(out: &str, pid: u32, seconds: f64) -> Option<TopSample> {
    let mut rows = Vec::new();
    for line in out.lines() {
        let mut parts = line.split_whitespace();
        if parts.next().and_then(|p| p.parse::<u32>().ok()) != Some(pid) {
            continue;
        }
        let cpu = parts.next().and_then(|v| v.parse::<f64>().ok());
        let count = parts
            .next()
            .and_then(|v| v.trim_end_matches(['+', '-']).parse::<f64>().ok());
        if let (Some(cpu), Some(count)) = (cpu, count) {
            rows.push((cpu, count));
        }
    }
    let (first, last) = (rows.first()?, rows.last()?);
    if rows.len() < 2 || seconds <= 0.0 {
        return None;
    }
    Some(TopSample {
        cpu_pct: last.0,
        idle_wakeups_per_s: ((last.1 - first.1) / seconds).max(0.0),
    })
}

/// The physical footprint of a process, and the part of it that is GPU memory.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub(crate) struct Footprint {
    pub(crate) total_mib: f64,
    /// `IOAccelerator` regions: what the GPU driver holds for the process. RSS misses it.
    pub(crate) gpu_mib: f64,
}

/// `footprint PID`: the `Footprint:` figure, and the dirty size of the `IOAccelerator` rows.
pub(crate) fn parse_footprint(out: &str) -> Option<Footprint> {
    let mut total = None;
    let mut gpu = 0.0;
    for line in out.lines() {
        if let Some((_, rest)) = line.split_once("Footprint:") {
            let size: Vec<&str> = rest.split_whitespace().take(2).collect();
            total = parse_size_mib(&size.join(" "));
            continue;
        }
        // `976 KB  0 B  0 B  5  Malloc Small`: dirty, clean, reclaimable, regions, category.
        let tokens: Vec<&str> = line.split_whitespace().collect();
        let Some(category) = tokens.get(7..).map(|t| t.join(" ")) else {
            continue;
        };
        if category.starts_with("IOAccelerator")
            && let Some(dirty) = parse_size_mib(&format!("{} {}", tokens[0], tokens[1]))
        {
            gpu += dirty;
        }
    }
    total.map(|total_mib| Footprint {
        total_mib,
        gpu_mib: gpu,
    })
}

/// `vmmap --summary PID`: `Physical footprint:`, and dirty plus swapped of the `IOAccelerator`
/// rows.
pub(crate) fn parse_vmmap(out: &str) -> Option<Footprint> {
    let mut total = None;
    let mut gpu = 0.0;
    for line in out.lines() {
        if let Some(rest) = line.strip_prefix("Physical footprint:") {
            total = parse_size_mib(rest);
            continue;
        }
        // Columns are separated by two spaces or more: type, virtual, resident, dirty, swapped.
        let cells: Vec<&str> = line
            .split("  ")
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .collect();
        if cells.len() >= 5 && cells[0].starts_with("IOAccelerator") {
            gpu +=
                parse_size_mib(cells[3]).unwrap_or(0.0) + parse_size_mib(cells[4]).unwrap_or(0.0);
        }
    }
    total.map(|total_mib| Footprint {
        total_mib,
        gpu_mib: gpu,
    })
}

/// `first-frame <ms>`.
pub(crate) fn parse_first_frame(line: &str) -> Option<f64> {
    line.strip_prefix("first-frame ")?.trim().parse().ok()
}

/// `frames <n>`.
pub(crate) fn parse_frames(line: &str) -> Option<u64> {
    line.strip_prefix("frames ")?.trim().parse().ok()
}

/// `scroll-rows <n>`.
pub(crate) fn parse_scroll_rows(line: &str) -> Option<usize> {
    line.strip_prefix("scroll-rows ")?.trim().parse().ok()
}

/// `scroll-frames <ms>,<ms>,...`.
pub(crate) fn parse_scroll_frames(line: &str) -> Option<Vec<f64>> {
    let rest = line.strip_prefix("scroll-frames ")?;
    Some(
        rest.split(',')
            .filter_map(|v| v.trim().parse().ok())
            .collect(),
    )
}

// --- the report ------------------------------------------------------------------------------

/// A scenario that was measured, or the reason it was not.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "status", content = "value", rename_all = "snake_case")]
pub(crate) enum Scenario<T> {
    Drawn(T),
    /// The window was not drawn: the number would be a fake low one.
    NotDrawn(String),
}

impl<T> Scenario<T> {
    pub(crate) fn value(&self) -> Option<&T> {
        match self {
            Self::Drawn(v) => Some(v),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct ColdStart {
    /// Spawn to the `first-frame` line, per start after the throwaway.
    pub(crate) spawn_to_frame_ms: Vec<f64>,
    /// The app's own figure, from the start of `main`.
    pub(crate) app_ms: Vec<f64>,
    pub(crate) median_ms: f64,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Load {
    pub(crate) seconds: f64,
    pub(crate) cpu_pct: f64,
    pub(crate) idle_wakeups_per_s: Option<f64>,
    pub(crate) footprint_mib: f64,
    pub(crate) footprint_peak_mib: f64,
    pub(crate) gpu_mib: f64,
    /// Frames drawn in the window (where the app reports them).
    pub(crate) frames: Option<u64>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct LongTranscript {
    pub(crate) rows: usize,
    pub(crate) footprint_before_mib: f64,
    pub(crate) footprint_peak_mib: f64,
    pub(crate) gpu_mib: f64,
    pub(crate) frames: usize,
    pub(crate) frame_ms: FrameStats,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Variant {
    pub(crate) allocator: String,
    pub(crate) cold_start: Scenario<ColdStart>,
    pub(crate) idle: Scenario<Load>,
    pub(crate) streaming: Scenario<Load>,
    pub(crate) long_transcript: Scenario<LongTranscript>,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Report {
    pub(crate) os: &'static str,
    pub(crate) arch: &'static str,
    pub(crate) quick: bool,
    pub(crate) variants: Vec<Variant>,
}

/// One row of the table: a number, its budget and how it stands.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Line {
    pub(crate) key: &'static str,
    pub(crate) measured: Option<f64>,
    pub(crate) limit: Option<f64>,
    pub(crate) target: Option<&'static str>,
    pub(crate) over: bool,
    pub(crate) note: Option<String>,
}

fn line(key: &'static str, scenario_note: Option<String>, measured: Option<f64>) -> Line {
    let Budget { limit, target, .. } = *budget(key);
    let over = matches!((measured, limit), (Some(m), Some(l)) if m > l);
    Line {
        key,
        measured,
        limit,
        target,
        over,
        note: scenario_note,
    }
}

impl Line {
    /// Over a budget the app holds today.
    fn fails(&self) -> bool {
        self.over && self.target.is_none()
    }
}

fn not_drawn<T>(s: &Scenario<T>) -> Option<String> {
    match s {
        Scenario::Drawn(_) => None,
        Scenario::NotDrawn(why) => Some(format!("window not drawn: {why}")),
    }
}

/// Every number of a variant, in table order, with its budget.
pub(crate) fn lines(v: &Variant) -> Vec<Line> {
    vec![
        line(
            "cold_start_ms",
            not_drawn(&v.cold_start),
            v.cold_start.value().map(|c| c.median_ms),
        ),
        line(
            "idle_cpu_pct",
            not_drawn(&v.idle),
            v.idle.value().map(|l| l.cpu_pct),
        ),
        line(
            "idle_wakeups_per_s",
            not_drawn(&v.idle),
            v.idle.value().and_then(|l| l.idle_wakeups_per_s),
        ),
        line(
            "idle_footprint_mib",
            not_drawn(&v.idle),
            v.idle.value().map(|l| l.footprint_mib),
        ),
        line(
            "streaming_cpu_pct",
            not_drawn(&v.streaming),
            v.streaming.value().map(|l| l.cpu_pct),
        ),
        line(
            "streaming_footprint_mib",
            not_drawn(&v.streaming),
            v.streaming.value().map(|l| l.footprint_peak_mib),
        ),
        line(
            "long_footprint_mib",
            not_drawn(&v.long_transcript),
            v.long_transcript.value().map(|l| l.footprint_peak_mib),
        ),
        line(
            "long_frame_p95_ms",
            not_drawn(&v.long_transcript),
            v.long_transcript.value().map(|l| l.frame_ms.p95),
        ),
        line(
            "gpu_share_mib",
            not_drawn(&v.idle),
            v.idle.value().map(|l| l.gpu_mib),
        ),
    ]
}

impl Report {
    /// Whether the run passes: no number may be over its budget. A number over a target (a limit
    /// another roadmap item must meet) is reported, not failed. A scenario that was not drawn is
    /// not a pass of anything, but it is not a miss either: the table says so.
    pub(crate) fn passed(&self) -> bool {
        self.variants
            .first()
            .is_none_or(|v| lines(v).iter().all(|l| !l.fails()))
    }
}

fn number(value: Option<f64>, unit: &str) -> String {
    match value {
        Some(v) => format!("{v:.1} {unit}"),
        None => "-".to_owned(),
    }
}

/// The text report.
pub(crate) fn table(report: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "Reno app benchmark ({} {}, {} mode)",
        report.os,
        report.arch,
        if report.quick { "quick" } else { "full" }
    );
    for v in &report.variants {
        let _ = writeln!(out, "\nallocator: {}", v.allocator);
        let _ = writeln!(
            out,
            "{:<48} {:>12}  {:<30} verdict",
            "metric", "measured", "budget"
        );
        for l in lines(v) {
            let b = budget(l.key);
            let budget_text = match (l.limit, l.target) {
                (Some(limit), Some(road)) => format!("<= {limit} {} (target, {road})", b.unit),
                (Some(limit), None) => format!("<= {limit} {}", b.unit),
                (None, _) => "no budget".to_owned(),
            };
            let verdict = match (&l.note, l.over, l.target) {
                (Some(note), _, _) => note.clone(),
                (None, true, Some(road)) => format!("over ({road})"),
                (None, true, None) => "OVER".to_owned(),
                (None, false, _) => "ok".to_owned(),
            };
            let _ = writeln!(
                out,
                "{:<48} {:>12}  {:<30} {}",
                b.label,
                number(l.measured, b.unit),
                budget_text,
                verdict
            );
        }
        let _ = writeln!(out, "\nbasis of each budget");
        for l in lines(v) {
            let b = budget(l.key);
            let _ = writeln!(out, "  {}: {}", b.label, b.basis);
        }
        let _ = writeln!(out, "\ndetail");
        if let Some(c) = v.cold_start.value() {
            let _ = writeln!(
                out,
                "  cold start: spawn to frame {} ms; the app's own clock (from main) {} ms",
                list(&c.spawn_to_frame_ms),
                list(&c.app_ms)
            );
        }
        for (name, s) in [("idle", &v.idle), ("streaming", &v.streaming)] {
            if let Some(l) = s.value() {
                let _ = writeln!(
                    out,
                    "  {name}: {:.0} s, CPU {:.2} %, wakeups {}, footprint {:.1} MiB (peak {:.1}), GPU {:.1} MiB{}",
                    l.seconds,
                    l.cpu_pct,
                    l.idle_wakeups_per_s
                        .map_or("-".to_owned(), |w| format!("{w:.2}/s")),
                    l.footprint_mib,
                    l.footprint_peak_mib,
                    l.gpu_mib,
                    l.frames
                        .map_or(String::new(), |f| format!(", {f} frames drawn"))
                );
            }
        }
        if let Some(l) = v.long_transcript.value() {
            let _ = writeln!(
                out,
                "  long transcript: {} rows, footprint {:.1} MiB before the scroll, peak {:.1}, GPU {:.1} MiB; {} frames: p50 {:.1} ms, p95 {:.1}, p99 {:.1}, max {:.1}",
                l.rows,
                l.footprint_before_mib,
                l.footprint_peak_mib,
                l.gpu_mib,
                l.frames,
                l.frame_ms.p50,
                l.frame_ms.p95,
                l.frame_ms.p99,
                l.frame_ms.max
            );
        }
    }
    out
}

fn list(values: &[f64]) -> String {
    values
        .iter()
        .map(|v| format!("{v:.0}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The p95 of frame intervals, as a `Latency`.
pub(crate) fn frame_latency(intervals_ms: &[f64]) -> Option<FrameStats> {
    Some(FrameStats {
        p50: percentile(intervals_ms, 50.0)?,
        p95: percentile(intervals_ms, 95.0)?,
        p99: percentile(intervals_ms, 99.0)?,
        max: percentile(intervals_ms, 100.0)?,
    })
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct FrameStats {
    pub(crate) p50: f64,
    pub(crate) p95: f64,
    pub(crate) p99: f64,
    pub(crate) max: f64,
}

#[cfg(test)]
mod tests {
    use super::*;

    const FOOTPRINT: &str = "\
======================================================================
reno-app [4242]: 64-bit    Footprint: 187 MB (16384 bytes per page)
======================================================================

  Dirty      Clean  Reclaimable    Regions    Category
    ---        ---          ---        ---    ---
  96 MB        0 B          0 B        143    IOAccelerator
  50 MB        0 B          0 B          9    Malloc Small
 2.5 MB        0 B          0 B          1    IOAccelerator (graphics)
 976 KB        0 B          0 B          5    Malloc Tiny
    ---        ---          ---        ---    ---
 187 MB     592 KB          0 B        542    TOTAL
";

    const VMMAP: &str = "\
Physical footprint:         187.2M
Physical footprint (peak):  190.0M

REGION TYPE                        SIZE     SIZE     SIZE     SIZE     SIZE     SIZE     SIZE    COUNT (non-coalesced)
===========                     ======= ========    =====  ======= ========   ======    =====  ======= 
IOAccelerator                      512M    90.0M    80.0M    16.0M       0K       0K       0K      143 
IOAccelerator (reserved)          64.0M       0K       0K       0K       0K       0K       0K        2 
Malloc Small                      16.0M     944K     944K       0K       0K       0K       0K        6 
TOTAL                            386.8G   135.9M    2624K       0K       0K      16K       0K      356 
";

    #[test]
    fn sizes_read_in_every_unit_footprint_and_vmmap_use() {
        assert_eq!(parse_size_mib("2608 KB"), Some(2608.0 / 1024.0));
        assert_eq!(parse_size_mib("2608K"), Some(2608.0 / 1024.0));
        assert_eq!(parse_size_mib("1.5 GB"), Some(1536.0));
        assert_eq!(parse_size_mib("62.1M"), Some(62.1));
        assert_eq!(parse_size_mib("1048576"), Some(1.0));
        assert_eq!(parse_size_mib("0 B"), Some(0.0));
        assert_eq!(parse_size_mib("lots"), None);
        assert_eq!(parse_size_mib(""), None);
    }

    #[test]
    fn top_gives_the_cpu_of_the_last_sample_and_the_wakeups_between_the_two() {
        // IDLEW is a running count since the process started, not a rate: 40 wakeups in the
        // 20 s between the samples is 2 a second, whatever the count was at the start.
        let out = "\
PID    %CPU IDLEW
4242   12.5 17
Processes: 400 total
PID    %CPU IDLEW
4242   0.3  57+
";
        assert_eq!(
            parse_top(out, 4242, 20.0),
            Some(TopSample {
                cpu_pct: 0.3,
                idle_wakeups_per_s: 2.0
            })
        );
        assert_eq!(parse_top(out, 7, 20.0), None, "another pid is not ours");
        assert_eq!(parse_top("PID %CPU IDLEW\n", 4242, 20.0), None);
        assert_eq!(parse_top("4242 abc 1", 4242, 20.0), None);
        let one = "PID %CPU IDLEW\n4242 0.3 9\n";
        assert_eq!(
            parse_top(one, 4242, 20.0),
            None,
            "one sample has no interval"
        );
    }

    #[test]
    fn footprint_reads_the_total_and_the_gpu_rows() {
        let f = parse_footprint(FOOTPRINT).expect("parses");
        assert_eq!(f.total_mib, 187.0);
        assert_eq!(f.gpu_mib, 96.0 + 2.5, "both IOAccelerator rows, no other");
    }

    #[test]
    fn footprint_with_no_gpu_rows_has_no_gpu_share() {
        let out = "x [1]: 64-bit    Footprint: 2608 KB (16384 bytes per page)\n 976 KB 0 B 0 B 5 Malloc Small\n";
        let f = parse_footprint(out).expect("parses");
        assert_eq!(f.gpu_mib, 0.0);
        assert!((f.total_mib - 2608.0 / 1024.0).abs() < 1e-9);
        assert_eq!(parse_footprint("nothing here"), None);
    }

    #[test]
    fn vmmap_reads_the_physical_footprint_and_the_gpu_rows() {
        let f = parse_vmmap(VMMAP).expect("parses");
        assert_eq!(f.total_mib, 187.2);
        assert_eq!(
            f.gpu_mib,
            80.0 + 16.0,
            "dirty plus swapped of the IOAccelerator rows"
        );
        assert_eq!(parse_vmmap("Process: zsh"), None);
    }

    #[test]
    fn the_apps_lines_parse() {
        assert_eq!(parse_first_frame("first-frame 123.4"), Some(123.4));
        assert_eq!(parse_first_frame("frames 4"), None);
        assert_eq!(parse_frames("frames 120"), Some(120));
        assert_eq!(parse_frames("first-frame 1.0"), None);
        assert_eq!(parse_scroll_rows("scroll-rows 4001"), Some(4001));
        assert_eq!(
            parse_scroll_frames("scroll-frames 8.30,8.41,16.9"),
            Some(vec![8.30, 8.41, 16.9])
        );
        assert_eq!(parse_scroll_frames("scroll-frames "), Some(vec![]));
        assert_eq!(parse_scroll_frames("nope 1,2"), None);
    }

    #[test]
    fn frame_stats_use_nearest_rank() {
        let intervals: Vec<f64> = (1..=100).map(f64::from).collect();
        let s = frame_latency(&intervals).expect("stats");
        assert_eq!((s.p50, s.p95, s.p99, s.max), (50.0, 95.0, 99.0, 100.0));
        assert_eq!(frame_latency(&[]), None);
    }

    fn load(cpu: f64, footprint: f64) -> Load {
        Load {
            seconds: 10.0,
            cpu_pct: cpu,
            idle_wakeups_per_s: Some(0.2),
            footprint_mib: footprint,
            footprint_peak_mib: footprint,
            gpu_mib: 30.0,
            frames: None,
        }
    }

    fn variant(idle: Scenario<Load>) -> Variant {
        Variant {
            allocator: "system".into(),
            cold_start: Scenario::Drawn(ColdStart {
                spawn_to_frame_ms: vec![300.0],
                app_ms: vec![290.0],
                median_ms: 300.0,
            }),
            idle,
            streaming: Scenario::Drawn(load(5.0, 150.0)),
            long_transcript: Scenario::NotDrawn("not run".into()),
        }
    }

    fn report(v: Variant) -> Report {
        Report {
            os: "macos",
            arch: "aarch64",
            quick: true,
            variants: vec![v],
        }
    }

    #[test]
    fn a_number_past_its_budget_fails_the_run() {
        let v = variant(Scenario::Drawn(load(4.0, 120.0)));
        let l = lines(&v);
        let idle_cpu = l.iter().find(|l| l.key == "idle_cpu_pct").unwrap();
        assert!(idle_cpu.over, "4 % is past 1 %");
        assert!(!report(v).passed());
    }

    #[test]
    fn the_signed_off_limits_are_these() {
        let limits: Vec<(&str, Option<f64>)> = BUDGETS.iter().map(|b| (b.key, b.limit)).collect();
        assert_eq!(
            limits,
            [
                ("cold_start_ms", Some(400.0)),
                ("idle_cpu_pct", Some(1.0)),
                ("idle_wakeups_per_s", Some(1.0)),
                ("idle_footprint_mib", Some(135.0)),
                ("streaming_cpu_pct", Some(15.0)),
                ("streaming_footprint_mib", Some(260.0)),
                ("long_footprint_mib", Some(370.0)),
                ("long_frame_p95_ms", Some(16.7)),
                ("gpu_share_mib", None),
            ]
        );
    }

    #[test]
    fn the_long_transcript_frame_time_is_a_target_that_does_not_fail_the_run() {
        let mut v = variant(Scenario::Drawn(load(0.2, 100.0)));
        v.long_transcript = Scenario::Drawn(LongTranscript {
            rows: 4,
            footprint_before_mib: 138.0,
            footprint_peak_mib: 300.0,
            gpu_mib: 6.0,
            frames: 4000,
            frame_ms: FrameStats {
                p50: 29.0,
                p95: 30.0,
                p99: 33.0,
                max: 54.0,
            },
        });
        let l = lines(&v);
        let frame = l.iter().find(|l| l.key == "long_frame_p95_ms").unwrap();
        assert!(frame.over, "30 ms is past 16.7 ms");
        assert!(
            report(v.clone()).passed(),
            "a target is reported, not failed"
        );
        let text = table(&report(v));
        assert!(text.contains("over (ROAD-33)"), "{text}");
        assert!(text.contains("target, ROAD-33"), "{text}");
        assert!(!text.contains("proposed"), "{text}");
    }

    #[test]
    fn a_scenario_that_was_not_drawn_has_no_number_and_says_so() {
        let v = variant(Scenario::NotDrawn("no first frame in 20 s".into()));
        let l = lines(&v);
        let idle = l.iter().find(|l| l.key == "idle_footprint_mib").unwrap();
        assert_eq!(idle.measured, None);
        assert!(!idle.over);
        assert!(idle.note.as_deref().unwrap().contains("window not drawn"));
        let text = table(&report(v));
        assert!(text.contains("window not drawn: no first frame in 20 s"));
    }

    #[test]
    fn every_budget_is_in_the_table_once() {
        let mut keys: Vec<&str> = BUDGETS.iter().map(|b| b.key).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), BUDGETS.len());
        let v = variant(Scenario::Drawn(load(0.2, 100.0)));
        for l in lines(&v) {
            let _ = budget(l.key);
        }
    }
}
