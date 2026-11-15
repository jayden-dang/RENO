//! The pure half of `cargo xtask bench`: statistics, parsers for what `ps` and `/proc` print,
//! the budgets from ARCHITECTURE 12, and the report and its table. Nothing here touches the
//! machine, so all of it is unit-tested.

use std::fmt::Write as _;

use serde::Serialize;

/// Nearest-rank percentile of `samples` (`p` in 0..=100). `None` for no samples.
pub(crate) fn percentile(samples: &[f64], p: f64) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (p / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted.get(rank.clamp(1, sorted.len()) - 1).copied()
}

/// The shape of a latency sample set.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Latency {
    pub(crate) n: usize,
    pub(crate) p50: f64,
    pub(crate) p99: f64,
    pub(crate) max: f64,
}

impl Latency {
    pub(crate) fn of(samples: &[f64]) -> Option<Self> {
        Some(Self {
            n: samples.len(),
            p50: percentile(samples, 50.0)?,
            p99: percentile(samples, 99.0)?,
            max: percentile(samples, 100.0)?,
        })
    }
}

/// The median of `samples`, or 0 for none.
pub(crate) fn median(samples: &[f64]) -> f64 {
    percentile(samples, 50.0).unwrap_or(0.0)
}

/// `ps -o rss= -p PID`: kibibytes, with padding.
pub(crate) fn parse_ps_rss(out: &str) -> Option<u64> {
    out.trim().parse().ok()
}

/// CPU seconds from `ps -o time=`: `M:SS.hh`, `H:MM:SS` or `D-HH:MM:SS`.
pub(crate) fn parse_ps_cpu_time(out: &str) -> Option<f64> {
    let text = out.trim();
    let (days, rest) = match text.split_once('-') {
        Some((days, rest)) => (days.parse::<f64>().ok()?, rest),
        None => (0.0, text),
    };
    let mut seconds = 0.0;
    for part in rest.split(':') {
        seconds = seconds * 60.0 + part.parse::<f64>().ok()?;
    }
    Some(days * 86_400.0 + seconds)
}

/// CPU seconds from the text of `/proc/PID/stat` (user plus system ticks, fields 14 and 15).
pub(crate) fn parse_proc_stat_cpu(stat: &str, ticks_per_second: f64) -> Option<f64> {
    // The command name is in parentheses and may hold spaces; the fields start after the last `)`.
    let after = &stat[stat.rfind(')')? + 1..];
    let mut fields = after.split_whitespace();
    // `after` starts at field 3 (state), so fields 14 and 15 are at offsets 11 and 12.
    let utime: f64 = fields.nth(11)?.parse().ok()?;
    let stime: f64 = fields.next()?.parse().ok()?;
    Some((utime + stime) / ticks_per_second)
}

/// Mean CPU use over a window, as a percentage of one core.
pub(crate) fn cpu_percent(cpu_start: f64, cpu_end: f64, wall_seconds: f64) -> f64 {
    if wall_seconds <= 0.0 {
        return 0.0;
    }
    (cpu_end - cpu_start).max(0.0) / wall_seconds * 100.0
}

/// Kibibytes as mebibytes.
pub(crate) fn mib(kib: f64) -> f64 {
    kib / 1024.0
}

/// Extra resident memory per live run, in MiB.
pub(crate) fn per_run_mib(idle_kib: f64, loaded_kib: f64, runs: usize) -> f64 {
    if runs == 0 {
        return 0.0;
    }
    mib(loaded_kib - idle_kib) / runs as f64
}

/// How far `after` is above `before`, as a fraction (0.10 is ten percent).
pub(crate) fn growth(before: f64, after: f64) -> f64 {
    if before <= 0.0 {
        return 0.0;
    }
    (after - before) / before
}

/// How long each phase of a run of the benchmark lasts.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub(crate) struct Plan {
    pub(crate) quick: bool,
    /// Seconds to let the daemon settle before reading idle RSS.
    pub(crate) idle_settle_s: u64,
    /// Seconds of idle CPU measurement.
    pub(crate) idle_cpu_s: u64,
    pub(crate) runs: usize,
    pub(crate) concurrency: usize,
    /// Seconds of idle after the runs, before the final RSS.
    pub(crate) post_idle_s: u64,
    pub(crate) live_runs: usize,
    pub(crate) append_events: usize,
}

impl Plan {
    pub(crate) fn new(quick: bool) -> Self {
        if quick {
            Self {
                quick,
                idle_settle_s: 5,
                idle_cpu_s: 10,
                runs: 200,
                concurrency: 8,
                post_idle_s: 5,
                live_runs: 20,
                append_events: 10_000,
            }
        } else {
            Self {
                quick,
                idle_settle_s: 30,
                idle_cpu_s: 60,
                runs: 1_000,
                concurrency: 8,
                post_idle_s: 30,
                live_runs: 20,
                append_events: 10_000,
            }
        }
    }
}

/// The measurements of one daemon build.
#[derive(Debug, Clone, Default, Serialize)]
pub(crate) struct Measured {
    /// Idle RSS after settling, MiB.
    pub(crate) idle_rss_mib: f64,
    /// Mean CPU over the idle window, percent of one core.
    pub(crate) idle_cpu_pct: f64,
    /// RSS before, at the peak of, and after the runs, MiB.
    pub(crate) runs_before_mib: f64,
    pub(crate) runs_peak_mib: f64,
    pub(crate) runs_after_mib: f64,
    pub(crate) runs_wall_s: f64,
    pub(crate) runs_done: usize,
    /// After over before, minus one.
    pub(crate) runs_growth: f64,
    /// Extra RSS per live run, MiB.
    pub(crate) per_run_mib: f64,
    /// Claim to executor start, milliseconds (event timestamps have 1 ms resolution).
    pub(crate) claim_to_start_ms: Option<Latency>,
    /// Boot to answering with the runs' store, milliseconds, one per boot, and an empty store.
    pub(crate) boot_ms: Vec<f64>,
    pub(crate) boot_empty_ms: f64,
}

/// A budget, a measured value and the verdict.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub(crate) struct Check {
    pub(crate) name: &'static str,
    pub(crate) unit: &'static str,
    pub(crate) limit: f64,
    /// `<=` or `<`.
    pub(crate) cmp: &'static str,
    pub(crate) value: f64,
    pub(crate) pass: bool,
}

impl Check {
    pub(crate) fn at_most(name: &'static str, unit: &'static str, limit: f64, value: f64) -> Self {
        Self {
            name,
            unit,
            limit,
            cmp: "<=",
            value,
            pass: value <= limit,
        }
    }

    pub(crate) fn below(name: &'static str, unit: &'static str, limit: f64, value: f64) -> Self {
        Self {
            name,
            unit,
            limit,
            cmp: "<",
            value,
            pass: value < limit,
        }
    }
}

/// The ARCHITECTURE 12 budgets, and the roadmap's boot threshold, against `m` and the append
/// latency in microseconds.
pub(crate) fn checks(m: &Measured, append: Option<&Latency>) -> Vec<Check> {
    let mut out = vec![
        Check::at_most("idle RSS", "MiB", 40.0, m.idle_rss_mib),
        Check::below("idle CPU", "%", 0.5, m.idle_cpu_pct),
        Check::at_most(
            "RSS after the runs vs before",
            "% growth",
            10.0,
            m.runs_growth * 100.0,
        ),
        Check::at_most("RSS per live run", "MiB", 4.0, m.per_run_mib),
    ];
    if let Some(l) = &m.claim_to_start_ms {
        out.push(Check::below("claim to start p99", "ms", 50.0, l.p99));
    }
    if let Some(l) = append {
        out.push(Check::below("event append p99", "us", 200.0, l.p99));
    }
    if let Some(boot) = (!m.boot_ms.is_empty()).then(|| median(&m.boot_ms)) {
        out.push(Check::at_most("boot with ended runs", "ms", 300.0, boot));
    }
    out
}

/// One allocator variant's results.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Variant {
    pub(crate) allocator: String,
    pub(crate) measured: Measured,
    pub(crate) checks: Vec<Check>,
}

/// The whole report, as written to `target/bench/report.json`.
#[derive(Debug, Serialize)]
pub(crate) struct Report {
    pub(crate) os: &'static str,
    pub(crate) arch: &'static str,
    pub(crate) plan: Plan,
    pub(crate) append_us: Option<Latency>,
    pub(crate) variants: Vec<Variant>,
}

impl Report {
    pub(crate) fn passed(&self) -> bool {
        self.variants
            .iter()
            .all(|v| v.checks.iter().all(|c| c.pass))
    }
}

fn mark(pass: bool) -> &'static str {
    if pass { "pass" } else { "FAIL" }
}

/// A short table for people: one block per variant.
pub(crate) fn table(report: &Report) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "reno bench ({}, {}, {} mode: {} runs, idle {} s / cpu {} s)",
        report.os,
        report.arch,
        if report.plan.quick { "quick" } else { "full" },
        report.plan.runs,
        report.plan.idle_settle_s,
        report.plan.idle_cpu_s,
    );
    if let Some(a) = &report.append_us {
        let _ = writeln!(
            out,
            "journal append+flush over {} events: p50 {:.1} us, p99 {:.1} us, max {:.1} us",
            a.n, a.p50, a.p99, a.max
        );
    }
    for variant in &report.variants {
        let m = &variant.measured;
        let _ = writeln!(out, "\nallocator: {}", variant.allocator);
        let _ = writeln!(
            out,
            "  idle {:.1} MiB; runs {:.1} -> peak {:.1} -> {:.1} MiB in {:.1} s ({} done)",
            m.idle_rss_mib,
            m.runs_before_mib,
            m.runs_peak_mib,
            m.runs_after_mib,
            m.runs_wall_s,
            m.runs_done
        );
        if let Some(l) = &m.claim_to_start_ms {
            let _ = writeln!(out, "  claim to start: p50 {} ms, p99 {} ms", l.p50, l.p99);
        }
        if !m.boot_ms.is_empty() {
            let boots: Vec<String> = m.boot_ms.iter().map(|b| format!("{b:.0}")).collect();
            let _ = writeln!(
                out,
                "  boot with {} ended runs: {} ms; an empty store: {:.0} ms",
                m.runs_done,
                boots.join(", "),
                m.boot_empty_ms
            );
        }
        let _ = writeln!(
            out,
            "  {:<32} {:>12}   {:>14}",
            "budget", "measured", "limit"
        );
        for c in &variant.checks {
            let _ = writeln!(
                out,
                "  {:<32} {:>9.2} {:<3}   {:>2} {:>7} {:<4} {}",
                c.name,
                c.value,
                short_unit(c.unit),
                c.cmp,
                c.limit,
                short_unit(c.unit),
                mark(c.pass)
            );
        }
    }
    out
}

fn short_unit(unit: &str) -> &str {
    unit.split(' ').next().unwrap_or(unit)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_use_nearest_rank() {
        let samples: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&samples, 50.0), Some(50.0));
        assert_eq!(percentile(&samples, 99.0), Some(99.0));
        assert_eq!(percentile(&samples, 100.0), Some(100.0));
        assert_eq!(percentile(&samples, 0.0), Some(1.0));
        assert_eq!(percentile(&[], 50.0), None);
        assert_eq!(percentile(&[7.0], 99.0), Some(7.0));
    }

    #[test]
    fn percentiles_ignore_input_order() {
        assert_eq!(percentile(&[9.0, 1.0, 5.0], 50.0), Some(5.0));
        assert_eq!(Latency::of(&[3.0, 1.0, 2.0]).map(|l| l.max), Some(3.0));
        assert_eq!(median(&[]), 0.0);
    }

    #[test]
    fn ps_rss_is_kibibytes() {
        assert_eq!(parse_ps_rss("  18432\n"), Some(18432));
        assert_eq!(parse_ps_rss("\n"), None);
    }

    #[test]
    fn ps_cpu_time_formats() {
        assert_eq!(parse_ps_cpu_time(" 0:00.45\n"), Some(0.45));
        assert_eq!(parse_ps_cpu_time("1:02:03"), Some(3723.0));
        assert_eq!(parse_ps_cpu_time("2-00:00:01"), Some(172_801.0));
        assert_eq!(parse_ps_cpu_time("nonsense"), None);
    }

    #[test]
    fn proc_stat_handles_spaces_in_the_name() {
        let stat = "123 (re (no) d) S 1 123 123 0 -1 4194560 100 0 0 0 250 50 0 0 20 0 5 0 99 1 2";
        assert_eq!(parse_proc_stat_cpu(stat, 100.0), Some(3.0));
        assert_eq!(parse_proc_stat_cpu("garbage", 100.0), None);
    }

    #[test]
    fn cpu_percent_is_a_share_of_one_core() {
        assert!((cpu_percent(1.0, 1.3, 60.0) - 0.5).abs() < 1e-9);
        assert_eq!(cpu_percent(2.0, 1.0, 60.0), 0.0);
        assert_eq!(cpu_percent(0.0, 1.0, 0.0), 0.0);
    }

    #[test]
    fn per_run_memory_and_growth() {
        assert!((per_run_mib(20_480.0, 20_480.0 + 20.0 * 2048.0, 20) - 2.0).abs() < 1e-9);
        assert_eq!(per_run_mib(1.0, 2.0, 0), 0.0);
        assert!((growth(100.0, 110.0) - 0.1).abs() < 1e-9);
        assert_eq!(growth(0.0, 5.0), 0.0);
    }

    #[test]
    fn budgets_are_inclusive_or_strict_as_written() {
        assert!(Check::at_most("a", "x", 40.0, 40.0).pass);
        assert!(!Check::below("b", "x", 0.5, 0.5).pass);
        assert!(!Check::at_most("a", "x", 40.0, 40.1).pass);
    }

    #[test]
    fn checks_cover_every_budget_and_flag_a_miss() {
        let mut m = Measured {
            idle_rss_mib: 30.0,
            idle_cpu_pct: 0.1,
            runs_growth: 0.05,
            per_run_mib: 1.0,
            claim_to_start_ms: Latency::of(&[1.0, 2.0, 3.0]),
            boot_ms: vec![100.0, 120.0, 110.0],
            ..Measured::default()
        };
        let append = Latency::of(&[10.0, 20.0]);
        let all = checks(&m, append.as_ref());
        assert_eq!(all.len(), 7);
        assert!(all.iter().all(|c| c.pass));
        m.per_run_mib = 4.5;
        m.boot_ms = vec![400.0];
        let failed: Vec<_> = checks(&m, None)
            .into_iter()
            .filter(|c| !c.pass)
            .map(|c| c.name)
            .collect();
        assert_eq!(failed, ["RSS per live run", "boot with ended runs"]);
    }

    #[test]
    fn quick_mode_is_shorter_and_smaller() {
        let (quick, full) = (Plan::new(true), Plan::new(false));
        assert!(quick.runs < full.runs && quick.idle_cpu_s < full.idle_cpu_s);
        assert_eq!(
            (full.runs, full.live_runs, full.append_events),
            (1000, 20, 10_000)
        );
    }

    #[test]
    fn the_table_names_each_variant_and_mark() {
        let measured = Measured::default();
        let report = Report {
            os: "macos",
            arch: "aarch64",
            plan: Plan::new(true),
            append_us: Latency::of(&[5.0]),
            variants: vec![Variant {
                allocator: "system".into(),
                checks: checks(&measured, None),
                measured,
            }],
        };
        let text = table(&report);
        assert!(text.contains("allocator: system"));
        assert!(text.contains("idle RSS"));
        assert!(text.contains("FAIL") || text.contains("pass"));
        assert!(!report.passed() || report.variants[0].checks.iter().all(|c| c.pass));
    }
}
