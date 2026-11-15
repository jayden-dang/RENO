//! `cargo xtask bench`: the I/O shell around [`crate::stats`].
//!
//! It builds `renod` and `reno` in release, starts the daemon on a temp data directory with
//! `RENO_NO_HARNESSES=1`, drives it through the `reno` CLI, and reads its memory and CPU from
//! the operating system. Method, per scenario:
//!
//! 1. **Idle.** RSS (`ps -o rss=`) once the daemon has been up for the settle time. CPU is the
//!    difference of the process's CPU seconds (`/proc/PID/stat` on Linux, `ps -o time=` elsewhere)
//!    over the idle window, divided by the window.
//! 2. **Runs.** `reno task run --executor mock` N times, a fixed number at once. RSS is sampled
//!    every 100 ms for the peak; the final RSS is the median of three samples after the idle time.
//! 3. **Per live run.** A second daemon whose mock holds until cancelled, 20 runs, each with one
//!    `reno` process following it: (RSS with the runs live minus idle RSS) / 20.
//! 4. **Claim to start.** From the stopped daemon's `events` table, `run.claimed` to the
//!    `run.transitioned` that makes the run `running`, which the dispatcher commits just before it
//!    calls the executor's `start`. The mock starts instantly, so this is the claim-to-spawn cost
//!    of the kernel. Timestamps are whole milliseconds, so the figure has 1 ms resolution.
//! 5. **Append.** `store_bench append`: `JournalWriter::append` plus `flush`, 10,000 events.
//! 6. **Boot.** The data directory of scenario 2 is booted three more times; each is the time from
//!    spawning `renod` until its socket accepts a connection, which is after recovery has run.
//!
//! Only `xtask` itself is allowed to look at the clock and the process table; the numbers it
//! turns into verdicts are computed by pure functions in [`crate::stats`].

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::stats::{
    self, Latency, Measured, Plan, Report, Variant, cpu_percent, median, mib, per_run_mib,
    steady_growth_per_1000,
};

type Result<T> = std::result::Result<T, String>;

/// Ticks per second in `/proc/PID/stat` (`USER_HZ`, 100 on every Linux that matters).
const LINUX_TICKS: f64 = 100.0;
/// How many times the populated store is booted.
const BOOTS: usize = 3;
/// The runs held live in the per-run scenario need this many slots.
const HOLD_SLOTS: usize = 64;

struct Options {
    quick: bool,
    /// Variants to build and measure, as `(label, cargo flags)`.
    variants: Vec<(&'static str, Vec<&'static str>)>,
}

fn parse_args(args: &[String]) -> Result<Options> {
    let mut quick = false;
    let mut compare = false;
    let mut allocator = "default";
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--quick" => quick = true,
            "--compare-allocators" => compare = true,
            "--allocator" => {
                allocator = match it.next().map(String::as_str) {
                    Some("default") => "default",
                    Some("system") => "system",
                    Some("mimalloc") => "mimalloc",
                    other => {
                        return Err(format!(
                            "--allocator wants default|system|mimalloc, got {other:?}"
                        ));
                    }
                };
            }
            other => return Err(format!("unknown bench option {other}")),
        }
    }
    let system = ("system", vec!["--no-default-features"]);
    let mimalloc = (
        "mimalloc",
        vec!["--no-default-features", "--features", "renod/mimalloc"],
    );
    let variants = if compare {
        vec![system, mimalloc]
    } else {
        match allocator {
            "system" => vec![system],
            "mimalloc" => vec![mimalloc],
            _ => vec![("default", Vec::new())],
        }
    };
    Ok(Options { quick, variants })
}

pub(crate) const USAGE: &str = "usage: cargo xtask bench [--quick] [--allocator default|system|mimalloc | --compare-allocators]";

pub(crate) fn run(args: &[String]) -> ExitCode {
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("xtask: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match bench(&options) {
        Ok(report) => {
            if report.passed() {
                ExitCode::SUCCESS
            } else {
                eprintln!("xtask: a budget was missed");
                ExitCode::FAILURE
            }
        }
        Err(message) => {
            eprintln!("xtask: bench failed: {message}");
            ExitCode::from(2)
        }
    }
}

fn bench(options: &Options) -> Result<Report> {
    let metadata = crate::run_cargo_metadata()?;
    let root = metadata.workspace_root;
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join("target"));
    let out_dir = target.join("bench");
    fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
    let plan = Plan::new(options.quick);

    let store_bench = build_store_bench(&root, &target)?;
    println!("append latency: {} events", plan.append_events);
    let append = append_latency(&store_bench, plan.append_events)?;

    let scratch = std::env::temp_dir().join(format!("reno-bench-{}", std::process::id()));
    let _cleanup = Cleanup(scratch.clone());
    let mut variants = Vec::new();
    for (label, flags) in &options.variants {
        println!("== allocator: {label}");
        let bins = build(&root, &target, &out_dir, label, flags)?;
        let measured = measure(&plan, &bins, &store_bench, &scratch.join(label))?;
        let checks = stats::checks(&plan, &measured, Some(&append));
        variants.push(Variant {
            allocator: (*label).to_owned(),
            measured,
            checks,
        });
    }

    let report = Report {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        plan,
        append_us: Some(append),
        variants,
    };
    let json = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    fs::write(out_dir.join("report.json"), json).map_err(|e| e.to_string())?;
    let table = stats::table(&report);
    fs::write(out_dir.join("report.txt"), &table).map_err(|e| e.to_string())?;
    println!(
        "\n{table}\nreport: {}",
        out_dir.join("report.json").display()
    );
    Ok(report)
}

/// Removes the scratch directory when the benchmark ends, however it ends.
struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Bins {
    renod: PathBuf,
    reno: PathBuf,
}

fn cargo_build(root: &Path, args: &[&str]) -> Result<()> {
    let status = Command::new("cargo")
        .current_dir(root)
        .arg("build")
        .arg("--release")
        .args(args)
        .status()
        .map_err(|e| format!("cannot run cargo: {e}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| format!("cargo build {args:?} failed"))
}

fn build(root: &Path, target: &Path, out: &Path, label: &str, flags: &[&str]) -> Result<Bins> {
    let mut args = vec!["-p", "renod", "-p", "reno"];
    args.extend_from_slice(flags);
    cargo_build(root, &args)?;
    let dir = out.join("bin").join(label);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let copy = |name: &str| -> Result<PathBuf> {
        let to = dir.join(name);
        fs::copy(target.join("release").join(name), &to).map_err(|e| format!("{name}: {e}"))?;
        Ok(to)
    };
    Ok(Bins {
        renod: copy("renod")?,
        reno: copy("reno")?,
    })
}

fn build_store_bench(root: &Path, target: &Path) -> Result<PathBuf> {
    cargo_build(root, &["-p", "reno-store", "--example", "store_bench"])?;
    Ok(target.join("release/examples/store_bench"))
}

fn json_from(program: &Path, args: &[&str]) -> Result<serde_json::Value> {
    let output = Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("{}: {e}", program.display()))?;
    if !output.status.success() {
        return Err(format!(
            "{} {args:?}: {}",
            program.display(),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("{}: {e}", program.display()))
}

fn numbers(value: &serde_json::Value, key: &str) -> Result<Vec<f64>> {
    value
        .get(key)
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("no `{key}` in the helper's output"))
        .map(|items| items.iter().filter_map(serde_json::Value::as_f64).collect())
}

fn append_latency(store_bench: &Path, events: usize) -> Result<Latency> {
    let out = json_from(store_bench, &["append", &events.to_string()])?;
    Latency::of(&numbers(&out, "samplesUs")?).ok_or_else(|| "no append samples".to_owned())
}

// --- the machine -----------------------------------------------------------------------------

fn ps(pid: u32, column: &str) -> Result<String> {
    let output = Command::new("ps")
        .args(["-o", column, "-p", &pid.to_string()])
        .output()
        .map_err(|e| format!("ps: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "ps -o {column} -p {pid} failed: the process is gone"
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn rss_kib(pid: u32) -> Result<f64> {
    let text = ps(pid, "rss=")?;
    stats::parse_ps_rss(&text)
        .map(|kib| kib as f64)
        .ok_or_else(|| format!("cannot read RSS from {text:?}"))
}

fn cpu_seconds(pid: u32) -> Result<f64> {
    if cfg!(target_os = "linux") {
        let stat = fs::read_to_string(format!("/proc/{pid}/stat")).map_err(|e| e.to_string())?;
        stats::parse_proc_stat_cpu(&stat, LINUX_TICKS).ok_or_else(|| "bad /proc stat".to_owned())
    } else {
        let text = ps(pid, "time=")?;
        stats::parse_ps_cpu_time(&text).ok_or_else(|| format!("cannot read CPU time from {text:?}"))
    }
}

fn pause(seconds: u64) {
    thread::sleep(Duration::from_secs(seconds));
}

/// The median of `count` RSS samples, `gap` apart, in KiB.
fn rss_median(pid: u32, count: usize, gap: Duration) -> Result<f64> {
    let mut samples = Vec::new();
    for n in 0..count {
        if n > 0 {
            thread::sleep(gap);
        }
        samples.push(rss_kib(pid)?);
    }
    Ok(median(&samples))
}

/// Samples a process's RSS in the background, for its peak.
struct Sampler {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<f64>,
}

impl Sampler {
    fn start(pid: u32) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            let mut peak = 0.0_f64;
            while !flag.load(Ordering::Relaxed) {
                if let Ok(kib) = rss_kib(pid) {
                    peak = peak.max(kib);
                }
                thread::sleep(Duration::from_millis(100));
            }
            peak
        });
        Self { stop, handle }
    }

    /// The highest RSS seen, in KiB.
    fn finish(self) -> f64 {
        self.stop.store(true, Ordering::Relaxed);
        self.handle.join().unwrap_or(0.0)
    }
}

// --- the daemon ------------------------------------------------------------------------------

struct Daemon {
    child: Option<Child>,
    pid: u32,
}

#[derive(Clone, Copy)]
struct DaemonOptions {
    hold: bool,
    max_concurrent: Option<usize>,
}

fn socket(dir: &Path) -> PathBuf {
    dir.join("reno.sock")
}

impl Daemon {
    /// Starts `renod` and returns it with the time until its socket accepted a connection.
    fn start(bins: &Bins, dir: &Path, options: DaemonOptions) -> Result<(Self, Duration)> {
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        let mut command = Command::new(&bins.renod);
        command
            .arg("--data-dir")
            .arg(dir)
            .arg("--socket")
            .arg(socket(dir))
            .env("RENO_NO_HARNESSES", "1")
            .env_remove("RENO_LOG")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if options.hold {
            command.env("RENO_MOCK_HOLD", "1");
        } else {
            command.env_remove("RENO_MOCK_HOLD");
        }
        match options.max_concurrent {
            Some(n) => command.env("RENO_MAX_CONCURRENT", n.to_string()),
            None => command.env_remove("RENO_MAX_CONCURRENT"),
        };
        let started = Instant::now();
        let child = command
            .spawn()
            .map_err(|e| format!("cannot start renod: {e}"))?;
        let pid = child.id();
        let mut daemon = Self {
            child: Some(child),
            pid,
        };
        loop {
            if UnixStream::connect(socket(dir)).is_ok() {
                return Ok((daemon, started.elapsed()));
            }
            if let Some(status) = daemon
                .child
                .as_mut()
                .and_then(|c| c.try_wait().ok().flatten())
            {
                daemon.child = None;
                return Err(format!("renod exited while starting: {status}"));
            }
            if started.elapsed() > Duration::from_secs(60) {
                return Err("renod did not answer within 60 s".to_owned());
            }
            thread::sleep(Duration::from_millis(1));
        }
    }

    /// SIGTERM, then wait for the process: the graceful shutdown the markers belong to.
    fn stop(mut self) -> Result<()> {
        let mut child = self.child.take().ok_or("daemon already gone")?;
        let _ = Command::new("kill")
            .args(["-TERM", &self.pid.to_string()])
            .status();
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => return Ok(()),
                Ok(Some(status)) => return Err(format!("renod exited with {status}")),
                Ok(None) if started.elapsed() > Duration::from_secs(40) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("renod did not stop within 40 s".to_owned());
                }
                Ok(None) => thread::sleep(Duration::from_millis(20)),
                Err(e) => return Err(e.to_string()),
            }
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn reno_command(bins: &Bins, dir: &Path) -> Command {
    let mut command = Command::new(&bins.reno);
    command
        .env("RENO_DATA_DIR", dir)
        .env("RENO_SOCKET", socket(dir))
        .current_dir(dir)
        .stdin(Stdio::null());
    command
}

// --- the scenarios ---------------------------------------------------------------------------

fn measure(plan: &Plan, bins: &Bins, store_bench: &Path, scratch: &Path) -> Result<Measured> {
    let mut m = Measured::default();
    let main_dir = scratch.join("main");

    // A first start of a binary the system has not run before is slow (the OS inspects it), and
    // would be read as the daemon's boot time. Start and stop both once on a throwaway directory.
    let (warm, _) = Daemon::start(
        bins,
        &scratch.join("warm"),
        DaemonOptions {
            hold: false,
            max_concurrent: None,
        },
    )?;
    warm.stop()?;
    let _ = reno_command(bins, &scratch.join("warm"))
        .arg("--version")
        .output();

    // 1. Idle.
    println!(
        "idle: settling {} s, then {} s of CPU",
        plan.idle_settle_s, plan.idle_cpu_s
    );
    let (daemon, first_boot) = Daemon::start(
        bins,
        &main_dir,
        DaemonOptions {
            hold: false,
            max_concurrent: None,
        },
    )?;
    m.boot_empty_ms = first_boot.as_secs_f64() * 1e3;
    pause(plan.idle_settle_s);
    m.idle_rss_mib = mib(rss_median(daemon.pid, 3, Duration::from_millis(500))?);
    let cpu0 = cpu_seconds(daemon.pid)?;
    let window = Instant::now();
    pause(plan.idle_cpu_s);
    let cpu1 = cpu_seconds(daemon.pid)?;
    m.idle_cpu_pct = cpu_percent(cpu0, cpu1, window.elapsed().as_secs_f64());

    // 2. The runs.
    println!(
        "runs: {} batches of {} mock runs, {} at a time",
        1 + plan.extra_batches,
        plan.runs,
        plan.concurrency
    );
    m.runs_before_mib = mib(rss_kib(daemon.pid)?);
    if plan.warmup_runs > 0 {
        m.runs_done += run_many(bins, &main_dir, plan.warmup_runs, plan.concurrency)?;
    }
    let mut peak = 0.0_f64;
    for batch in 0..=plan.extra_batches {
        let sampler = Sampler::start(daemon.pid);
        let wall = Instant::now();
        m.runs_done += run_many(bins, &main_dir, plan.runs, plan.concurrency)?;
        if batch == 0 {
            m.runs_wall_s = wall.elapsed().as_secs_f64();
        }
        peak = peak.max(sampler.finish());
        pause(plan.post_idle_s);
        let after = rss_median(daemon.pid, 3, Duration::from_millis(500))?;
        m.runs_series_mib.push(mib(after));
    }
    m.runs_peak_mib = mib(peak);
    m.runs_after_mib = m.runs_series_mib.first().copied().unwrap_or_default();
    m.runs_steady_mib_per_1000 = steady_growth_per_1000(&m.runs_series_mib, plan.runs);
    daemon.stop()?;

    // 4. Claim to start, from the events the daemon left.
    let db = main_dir.join("reno.db");
    let deltas = json_from(store_bench, &["claim-latency", &db.to_string_lossy()])?;
    m.claim_to_start_ms = Latency::of(&numbers(&deltas, "deltasMs")?);

    // 6. Boot with the ended runs in the store.
    println!("boot: {BOOTS} boots with {} ended runs", m.runs_done);
    for _ in 0..BOOTS {
        let (daemon, boot) = Daemon::start(
            bins,
            &main_dir,
            DaemonOptions {
                hold: false,
                max_concurrent: None,
            },
        )?;
        m.boot_ms.push(boot.as_secs_f64() * 1e3);
        daemon.stop()?;
    }

    // 3. Per live run, on a daemon whose mock holds.
    println!("live runs: {} held runs", plan.live_runs);
    m.per_run_mib = per_live_run(plan, bins, &scratch.join("hold"))?;
    Ok(m)
}

/// Runs `total` mock runs through the CLI, `concurrency` at a time. Returns how many finished.
fn run_many(bins: &Bins, dir: &Path, total: usize, concurrency: usize) -> Result<usize> {
    let next = AtomicUsize::new(0);
    let done = AtomicUsize::new(0);
    let first_error: Mutex<Option<String>> = Mutex::new(None);
    thread::scope(|scope| {
        for _ in 0..concurrency {
            scope.spawn(|| {
                while next.fetch_add(1, Ordering::Relaxed) < total {
                    let output = reno_command(bins, dir)
                        .args(["task", "run", "--executor", "mock", "bench"])
                        .stdout(Stdio::null())
                        .output();
                    match output {
                        Ok(out) if out.status.success() => {
                            done.fetch_add(1, Ordering::Relaxed);
                        }
                        Ok(out) => record(
                            &first_error,
                            String::from_utf8_lossy(&out.stderr).into_owned(),
                        ),
                        Err(e) => record(&first_error, e.to_string()),
                    }
                }
            });
        }
    });
    let finished = done.load(Ordering::Relaxed);
    if finished != total {
        let why = first_error
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_default();
        return Err(format!("only {finished} of {total} runs finished: {why}"));
    }
    Ok(finished)
}

fn record(slot: &Mutex<Option<String>>, message: String) {
    if let Ok(mut slot) = slot.lock() {
        slot.get_or_insert(message);
    }
}

/// One `reno task run` that the held mock keeps alive.
struct Live {
    child: Child,
    ready: Arc<AtomicBool>,
    reader: Option<JoinHandle<()>>,
}

fn per_live_run(plan: &Plan, bins: &Bins, dir: &Path) -> Result<f64> {
    let (daemon, _) = Daemon::start(
        bins,
        dir,
        DaemonOptions {
            hold: true,
            max_concurrent: Some(HOLD_SLOTS),
        },
    )?;
    pause(plan.idle_settle_s.min(10));
    let idle = rss_median(daemon.pid, 3, Duration::from_millis(500))?;

    let mut live = Vec::new();
    for _ in 0..plan.live_runs {
        // `--json`: the run id, then one line per event. The hold script says `Started` and one
        // text and waits, so the third line means the run is live and holding.
        let mut child = reno_command(bins, dir)
            .args(["--json", "task", "run", "--executor", "mock", "hold"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let ready = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&ready);
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let reader = thread::spawn(move || {
            for (n, line) in BufReader::new(stdout).lines().enumerate() {
                if line.is_err() {
                    break;
                }
                if n + 1 >= 3 {
                    flag.store(true, Ordering::Relaxed);
                }
            }
        });
        live.push(Live {
            child,
            ready,
            reader: Some(reader),
        });
    }
    let started = Instant::now();
    while !live.iter().all(|l| l.ready.load(Ordering::Relaxed)) {
        if started.elapsed() > Duration::from_secs(90) {
            kill_all(&mut live);
            return Err("the held runs did not all start within 90 s".to_owned());
        }
        thread::sleep(Duration::from_millis(50));
    }
    pause(3);
    let loaded = rss_median(daemon.pid, 5, Duration::from_millis(500));
    kill_all(&mut live);
    let loaded = loaded?;
    daemon.stop()?;
    Ok(per_run_mib(idle, loaded, plan.live_runs))
}

fn kill_all(live: &mut [Live]) {
    for l in live.iter_mut() {
        let _ = l.child.kill();
        let _ = l.child.wait();
        if let Some(reader) = l.reader.take() {
            let _ = reader.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn default_is_one_variant_with_default_features() {
        let options = parse_args(&args(&[])).expect("parse");
        assert!(!options.quick);
        assert_eq!(options.variants.len(), 1);
        assert!(options.variants[0].1.is_empty());
    }

    #[test]
    fn compare_builds_system_and_mimalloc_without_defaults() {
        let options = parse_args(&args(&["--quick", "--compare-allocators"])).expect("parse");
        assert!(options.quick);
        let labels: Vec<_> = options.variants.iter().map(|v| v.0).collect();
        assert_eq!(labels, ["system", "mimalloc"]);
        assert!(
            options
                .variants
                .iter()
                .all(|v| v.1.contains(&"--no-default-features"))
        );
    }

    #[test]
    fn bad_options_are_refused() {
        assert!(parse_args(&args(&["--nope"])).is_err());
        assert!(parse_args(&args(&["--allocator", "jemalloc"])).is_err());
        assert!(parse_args(&args(&["--allocator"])).is_err());
    }
}
