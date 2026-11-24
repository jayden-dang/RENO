//! `cargo xtask bench --app`: the I/O shell around [`crate::app_stats`], for the desktop app.
//!
//! It builds `renod`, `reno` and `reno-app` in release, starts its own `renod` on a temporary data
//! directory with `RENO_NO_HARNESSES=1` (the owner's daemon and files are never touched), runs the
//! app against it with `RENO_BENCH=1`, and reads the app's memory and CPU from the operating
//! system. macOS only; it opens a real window. Method, per scenario:
//!
//! 1. **Cold start.** Spawn the app; time from the spawn until it prints `first-frame`. One
//!    throwaway start (the OS inspects a binary it has not run), then five; the median.
//! 2. **Idle.** The app on Home. Settle, then a window: CPU from the process's CPU seconds
//!    (`ps -o time=`), idle wakeups per second from `top -l 2 -s <window> -stats idlew` (no sudo),
//!    and the physical footprint from `footprint`, which counts the GPU memory RSS misses.
//! 3. **Streaming.** `renod --stream-mock` plays about 20 events a second for 60 s; the app is
//!    opened on that run. CPU over a window inside the run, and the footprint sampled through it.
//! 4. **Long transcript.** `renod --stream-mock=burst` finishes a run of 2,000 messages. The app
//!    opens it with every fold open and scrolls it from the end to the start and back, one step a
//!    frame; the time between frames gives p95, and the footprint is sampled while it scrolls.
//! 5. **Long reply.** Finish one long Markdown reply, then use the same scripted scroll.
//! 6. **Find while streaming.** Open Find before releasing the 2,000-message mock burst and
//!    measure frames until that run's Done event has been drawn.
//! 7. **GPU memory.** The `IOAccelerator` share of the footprint, taken from the idle window.
//!
//! GPUI draws no frame for a window that is covered, so the window must stay on screen and
//! uncovered. The app prints `first-frame` once and `frames <n>` while it draws; where drawing
//! matters a scenario with no frame is reported "window not drawn" and has no number.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitCode, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::app_stats::{self, ColdStart, Footprint, LongTranscript, Report, Scenario, Variant};
use crate::bench::{
    Bins, Daemon, DaemonOptions, cargo_build, cpu_seconds, pause, reno_command, socket,
};
use crate::stats::{cpu_percent, median};

type Result<T> = std::result::Result<T, String>;

/// How long the first frame may take before the window is called not drawn.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the scripted scroll may take, from the app's start.
const SCROLL_TIMEOUT: Duration = Duration::from_secs(240);

pub(crate) const USAGE: &str =
    "usage: cargo xtask bench --app [--quick] [--idle-only] [--stream-only] [--compare-allocators]";

/// What to build and measure.
#[derive(Debug, PartialEq, Eq)]
struct Options {
    quick: bool,
    /// Cold start and idle only: no scenario that streams or scrolls, which need the window drawn
    /// and so keep it in front of everything for minutes.
    idle_only: bool,
    /// Streaming only, to count frames while a source is switched off (`RENO_BENCH_OFF`).
    stream_only: bool,
    /// `(label, cargo flags for reno-app)`.
    variants: Vec<(&'static str, Vec<&'static str>)>,
}

fn parse_args(args: &[String]) -> Result<Options> {
    let mut quick = false;
    let mut idle_only = false;
    let mut stream_only = false;
    let mut compare = false;
    for arg in args {
        match arg.as_str() {
            "--quick" => quick = true,
            "--idle-only" => idle_only = true,
            "--stream-only" => stream_only = true,
            "--compare-allocators" => compare = true,
            other => return Err(format!("unknown bench --app option {other}")),
        }
    }
    let system = ("system", Vec::new());
    let variants = if compare {
        vec![system, ("mimalloc", vec!["--features", "mimalloc"])]
    } else {
        vec![system]
    };
    Ok(Options {
        quick,
        idle_only,
        stream_only,
        variants,
    })
}

/// How long each scenario runs.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Plan {
    /// Starts measured, after the throwaway.
    cold_starts: usize,
    idle_settle_s: u64,
    idle_s: u64,
    /// After the first frame, before the streaming window opens: the run is opened and filling.
    stream_settle_s: u64,
    stream_s: u64,
}

impl Plan {
    fn new(quick: bool) -> Self {
        if quick {
            Self {
                cold_starts: 3,
                idle_settle_s: 5,
                idle_s: 20,
                stream_settle_s: 4,
                stream_s: 20,
            }
        } else {
            Self {
                cold_starts: 5,
                idle_settle_s: 30,
                idle_s: 120,
                stream_settle_s: 4,
                stream_s: 50,
            }
        }
    }
}

pub(crate) fn run(args: &[String]) -> ExitCode {
    let options = match parse_args(args) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("xtask: {message}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if !cfg!(target_os = "macos") {
        eprintln!(
            "xtask: the app benchmark runs on macOS only (it needs a window and `footprint`)"
        );
        return ExitCode::from(2);
    }
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
            eprintln!("xtask: app bench failed: {message}");
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

    // A display that sleeps mid-run stops drawing the window, and a full run outlasts the usual
    // ten minutes of the display's idle timer. `caffeinate` keeps it awake and exits with us.
    let _awake = Command::new("/usr/bin/caffeinate")
        .args(["-d", "-i", "-w", &std::process::id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok();

    println!(
        "The benchmark brings Reno to the front for a few minutes. Leave the machine alone\n\
         meanwhile: a window that is hidden, covered or behind a full-screen app is not drawn, and the\n\
         scenarios that need frames are then marked \"window not drawn\" instead of reporting a number.\n"
    );

    // The daemon: not the thing measured, built once, with the system allocator.
    cargo_build(
        &root,
        &["-p", "renod", "-p", "reno", "--no-default-features"],
    )?;
    let bin_dir = out_dir.join("bin").join("app");
    fs::create_dir_all(&bin_dir).map_err(|e| e.to_string())?;
    let copy = |name: &str, to: &Path| -> Result<PathBuf> {
        let dest = to.join(name);
        fs::copy(target.join("release").join(name), &dest).map_err(|e| format!("{name}: {e}"))?;
        Ok(dest)
    };
    let bins = Bins {
        renod: copy("renod", &bin_dir)?,
        reno: copy("reno", &bin_dir)?,
    };

    let scratch = std::env::temp_dir().join(format!("reno-app-bench-{}", std::process::id()));
    let _cleanup = Cleanup(scratch.clone());
    let mut variants = Vec::new();
    for (label, flags) in &options.variants {
        println!("== allocator: {label}");
        let mut args = vec!["-p", "reno-app"];
        args.extend_from_slice(flags);
        cargo_build(&root, &args)?;
        let dir = out_dir.join("bin").join(format!("app-{label}"));
        fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let app = copy("reno-app", &dir)?;
        variants.push(measure(
            &plan,
            options.idle_only,
            options.stream_only,
            label,
            &app,
            &bins,
            &scratch.join(label),
        )?);
    }

    let report = Report {
        os: std::env::consts::OS,
        arch: std::env::consts::ARCH,
        quick: options.quick,
        variants,
    };
    let json = serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?;
    fs::write(out_dir.join("app-report.json"), json).map_err(|e| e.to_string())?;
    let table = app_stats::table(&report);
    fs::write(out_dir.join("app-report.txt"), &table).map_err(|e| e.to_string())?;
    println!(
        "\n{table}\nreport: {}",
        out_dir.join("app-report.json").display()
    );
    Ok(report)
}

struct Cleanup(PathBuf);

impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

// --- the app ---------------------------------------------------------------------------------

/// What the app has printed so far.
#[derive(Default)]
struct AppLog {
    first_frame_ms: Option<f64>,
    first_frame_at: Option<Instant>,
    frames: u64,
    find_ready: bool,
    scroll_rows: Option<usize>,
    scroll_frames: Option<Vec<f64>>,
}

/// One `reno-app` process, on its own data directory, reading its stdout.
struct App {
    child: Child,
    pid: u32,
    spawned: Instant,
    log: Arc<Mutex<AppLog>>,
    reader: Option<JoinHandle<()>>,
}

impl App {
    /// Starts the app against the daemon whose files are in `dir`. `extra` is the screen to open.
    fn start(binary: &Path, dir: &Path, extra: &[(&str, &str)]) -> Result<Self> {
        let mut command = Command::new(binary);
        // No RENO_* variable of the benchmarker's own reaches the app.
        for (name, _) in std::env::vars() {
            if name.starts_with("RENO_") && name != "RENO_BENCH_OFF" {
                command.env_remove(name);
            }
        }
        command
            .env("RENO_BENCH", "1")
            .env("RENO_DATA_DIR", dir)
            .env("RENO_SOCKET", socket(dir))
            // Never start a daemon of its own, or reach the owner's launchd one.
            .env("RENO_NO_AUTOSTART", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (key, value) in extra {
            command.env(key, value);
        }
        let spawned = Instant::now();
        let mut child = command
            .spawn()
            .map_err(|e| format!("cannot start {}: {e}", binary.display()))?;
        let pid = child.id();
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let log = Arc::new(Mutex::new(AppLog::default()));
        let sink = Arc::clone(&log);
        let reader = thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(mut log) = sink.lock() else { break };
                if let Some(ms) = app_stats::parse_first_frame(&line) {
                    log.first_frame_ms.get_or_insert(ms);
                    log.first_frame_at.get_or_insert_with(Instant::now);
                } else if let Some(n) = app_stats::parse_frames(&line) {
                    log.frames = n;
                } else if line == "find-ready" {
                    log.find_ready = true;
                } else if let Some(n) = app_stats::parse_scroll_rows(&line) {
                    log.scroll_rows = Some(n);
                } else if let Some(frames) = app_stats::parse_scroll_frames(&line) {
                    log.scroll_frames = Some(frames);
                }
            }
        });
        Ok(Self {
            child,
            pid,
            spawned,
            log,
            reader: Some(reader),
        })
    }

    fn with_log<T>(&self, read: impl FnOnce(&AppLog) -> T) -> T {
        read(
            &self
                .log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// Waits for the first frame: `(spawn to line in ms, the app's own figure)`, or `None` when
    /// the window did not draw one in time.
    fn first_frame(&mut self) -> Result<Option<(f64, f64)>> {
        loop {
            let seen = self.with_log(|log| log.first_frame_at.zip(log.first_frame_ms));
            if let Some((at, app_ms)) = seen {
                return Ok(Some(((at - self.spawned).as_secs_f64() * 1e3, app_ms)));
            }
            if let Some(status) = self.child.try_wait().map_err(|e| e.to_string())? {
                return Err(format!("reno-app exited early: {status}"));
            }
            if self.spawned.elapsed() > FIRST_FRAME_TIMEOUT {
                return Ok(None);
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    fn frames(&self) -> u64 {
        self.with_log(|log| log.frames)
    }

    fn stop(mut self) {
        self.kill();
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.kill();
    }
}

// --- the machine -----------------------------------------------------------------------------

/// The app's physical footprint now: `footprint`, or `vmmap --summary` when that says nothing.
fn footprint(pid: u32) -> Result<Footprint> {
    let pid_text = pid.to_string();
    let run = |program: &str, args: &[&str]| -> String {
        Command::new(program)
            .args(args)
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    app_stats::parse_footprint(&run("/usr/bin/footprint", &[&pid_text]))
        .or_else(|| app_stats::parse_vmmap(&run("/usr/bin/vmmap", &["--summary", &pid_text])))
        .ok_or_else(|| format!("cannot read the footprint of {pid}"))
}

/// `top` over `seconds`: its last sample is the whole window.
fn top_window(pid: u32, seconds: u64) -> Result<app_stats::TopSample> {
    let output = Command::new("/usr/bin/top")
        .args([
            "-l",
            "2",
            "-s",
            &seconds.to_string(),
            "-pid",
            &pid.to_string(),
            "-stats",
            "pid,cpu,idlew",
        ])
        .output()
        .map_err(|e| format!("top: {e}"))?;
    let text = String::from_utf8_lossy(&output.stdout);
    app_stats::parse_top(&text, pid, seconds as f64)
        .ok_or_else(|| format!("cannot read top's output: {text}"))
}

/// A data directory the app and daemon share, marked as set up so the first-run wizard does not
/// cover Home.
fn data_dir(scratch: &Path, name: &str) -> Result<PathBuf> {
    let dir = scratch.join(name);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    fs::write(dir.join("app-settings.json"), r#"{"onboarded": true}"#)
        .map_err(|e| e.to_string())?;
    Ok(dir)
}

// --- the scenarios ---------------------------------------------------------------------------

fn measure(
    plan: &Plan,
    idle_only: bool,
    stream_only: bool,
    label: &str,
    app: &Path,
    bins: &Bins,
    scratch: &Path,
) -> Result<Variant> {
    // Idle and cold start share a quiet daemon.
    let quiet = data_dir(scratch, "quiet")?;
    let (daemon, _) = Daemon::start(bins, &quiet, DaemonOptions::default())?;
    let (cold_start, idle) = if stream_only {
        (
            Scenario::Skipped("not run (--stream-only)".into()),
            Scenario::Skipped("not run (--stream-only)".into()),
        )
    } else {
        (cold_start(plan, app, &quiet)?, idle(plan, app, &quiet)?)
    };
    daemon.stop()?;

    let streaming = if idle_only {
        Scenario::Skipped("not run (--idle-only)".into())
    } else {
        streaming(plan, app, bins, &data_dir(scratch, "stream")?)?
    };
    let transcript = |kind: TranscriptScenario| {
        if idle_only || stream_only {
            let option = if idle_only {
                "--idle-only"
            } else {
                "--stream-only"
            };
            return Ok(Scenario::Skipped(format!("not run ({option})")));
        }
        transcript_scenario(app, bins, &data_dir(scratch, kind.name())?, kind)
    };
    let long_transcript = transcript(TranscriptScenario::LongTranscript)?;
    let long_reply = transcript(TranscriptScenario::LongReply)?;
    let find_streaming = transcript(TranscriptScenario::FindStreaming)?;
    Ok(Variant {
        allocator: label.to_owned(),
        cold_start,
        idle,
        streaming,
        long_transcript,
        long_reply,
        find_streaming,
    })
}

fn not_drawn<T>(what: &str) -> Scenario<T> {
    Scenario::NotDrawn(format!(
        "no frame within {} s: {what}",
        FIRST_FRAME_TIMEOUT.as_secs()
    ))
}

fn cold_start(plan: &Plan, app: &Path, dir: &Path) -> Result<Scenario<ColdStart>> {
    println!("cold start: one throwaway, then {}", plan.cold_starts);
    let mut spawn_to_frame = Vec::new();
    let mut app_ms = Vec::new();
    for n in 0..=plan.cold_starts {
        let mut started = App::start(app, dir, &[])?;
        let frame = started.first_frame()?;
        started.stop();
        let Some((spawned, own)) = frame else {
            return Ok(not_drawn("cold start"));
        };
        if n > 0 {
            spawn_to_frame.push(spawned);
            app_ms.push(own);
        }
        pause(1);
    }
    Ok(Scenario::Drawn(ColdStart {
        median_ms: median(&spawn_to_frame),
        spawn_to_frame_ms: spawn_to_frame,
        app_ms,
    }))
}

fn idle(plan: &Plan, app: &Path, dir: &Path) -> Result<Scenario<app_stats::Load>> {
    println!(
        "idle: settling {} s, then {} s",
        plan.idle_settle_s, plan.idle_s
    );
    let mut started = App::start(app, dir, &[])?;
    if started.first_frame()?.is_none() {
        return Ok(not_drawn("idle"));
    }
    pause(plan.idle_settle_s);
    let cpu0 = cpu_seconds(started.pid)?;
    let wall = Instant::now();
    let top = top_window(started.pid, plan.idle_s)?;
    let cpu1 = cpu_seconds(started.pid)?;
    let footprint = footprint(started.pid)?;
    let seconds = wall.elapsed().as_secs_f64();
    started.stop();
    Ok(Scenario::Drawn(app_stats::Load {
        seconds,
        cpu_pct: cpu_percent(cpu0, cpu1, seconds),
        idle_wakeups_per_s: Some(top.idle_wakeups_per_s),
        footprint_mib: footprint.total_mib,
        footprint_peak_mib: footprint.total_mib,
        gpu_mib: footprint.gpu_mib,
        frames: None,
    }))
}

/// The run id on the first line of `reno --json task run`.
fn run_id(line: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    ["id", "run", "run_id", "runId"]
        .iter()
        .find_map(|key| value.get(key).and_then(|v| v.as_str()))
        .map(str::to_owned)
        .or_else(|| value.as_str().map(str::to_owned))
}

/// A `reno task run` that follows its run until the run ends.
struct Follower {
    child: Child,
    run: String,
}

impl Follower {
    fn start(bins: &Bins, dir: &Path, prompt: &str) -> Result<Self> {
        let mut child = reno_command(bins, dir)
            .args(["--json", "task", "run", "--executor", "mock", prompt])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| e.to_string())?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let mut lines = BufReader::new(stdout).lines();
        let first = lines
            .next()
            .ok_or("reno printed nothing")?
            .map_err(|e| e.to_string())?;
        let run = run_id(&first).ok_or_else(|| format!("no run id in {first:?}"))?;
        // Drain the rest so the pipe never fills.
        thread::spawn(move || for _ in lines {});
        Ok(Self { child, run })
    }

    fn wait(mut self) -> Result<()> {
        let status = self.child.wait().map_err(|e| e.to_string())?;
        status
            .success()
            .then_some(())
            .ok_or_else(|| format!("reno task run: {status}"))
    }
}

impl Drop for Follower {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn streaming(
    plan: &Plan,
    app: &Path,
    bins: &Bins,
    dir: &Path,
) -> Result<Scenario<app_stats::Load>> {
    println!(
        "streaming: a {} s window inside the 60 s stream",
        plan.stream_s
    );
    let (daemon, _) = Daemon::start(
        bins,
        dir,
        DaemonOptions {
            stream_mock: Some("steady"),
            ..DaemonOptions::default()
        },
    )?;
    let follower = Follower::start(bins, dir, "stream")?;
    let mut started = App::start(
        app,
        dir,
        &[(
            "RENO_OPEN",
            &format!("run_{}", follower.run.trim_start_matches("run_")),
        )],
    )?;
    if started.first_frame()?.is_none() {
        return Ok(not_drawn("streaming"));
    }
    pause(plan.stream_settle_s);
    let frames0 = started.frames();
    let cpu0 = cpu_seconds(started.pid)?;
    let wall = Instant::now();
    let mut samples = vec![footprint(started.pid)?];
    while wall.elapsed() < Duration::from_secs(plan.stream_s) {
        thread::sleep(Duration::from_secs(5));
        samples.push(footprint(started.pid)?);
    }
    let cpu1 = cpu_seconds(started.pid)?;
    let seconds = wall.elapsed().as_secs_f64();
    let frames = started.frames().saturating_sub(frames0);
    started.stop();
    drop(follower);
    daemon.stop()?;
    // A covered window draws nothing, and a covered window's CPU is a fake low number.
    if frames < plan.stream_s {
        return Ok(Scenario::NotDrawn(format!(
            "{frames} frames in {seconds:.0} s while a run streamed"
        )));
    }
    let last = *samples.last().ok_or("no footprint samples")?;
    let peak = samples.iter().map(|f| f.total_mib).fold(0.0_f64, f64::max);
    Ok(Scenario::Drawn(app_stats::Load {
        seconds,
        cpu_pct: cpu_percent(cpu0, cpu1, seconds),
        idle_wakeups_per_s: None,
        footprint_mib: last.total_mib,
        footprint_peak_mib: peak,
        gpu_mib: last.gpu_mib,
        frames: Some(frames),
    }))
}

#[derive(Clone, Copy)]
enum TranscriptScenario {
    LongTranscript,
    LongReply,
    FindStreaming,
}

impl TranscriptScenario {
    fn name(self) -> &'static str {
        match self {
            Self::LongTranscript => "long_transcript",
            Self::LongReply => "long_reply",
            Self::FindStreaming => "find_streaming",
        }
    }

    fn mock(self) -> &'static str {
        match self {
            Self::LongTranscript => "burst",
            Self::LongReply => "longreply",
            Self::FindStreaming => "findburst",
        }
    }
}

fn release_find_burst(dir: &Path, run: &str) -> Result<()> {
    let run = run.parse().map_err(|e| format!("find burst run ID: {e}"))?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())?;
    runtime.block_on(async {
        tokio::time::timeout(Duration::from_secs(5), async {
            let client = reno_client::Client::connect_unix(socket(dir))
                .await
                .map_err(|e| e.to_string())?;
            let summary = client.get_run(&run).await.map_err(|e| e.to_string())?;
            if summary.executor.as_str() != "mock" {
                return Err("Find benchmark can only release the isolated mock run".to_owned());
            }
            client
                .steer(&run, "Start the benchmark burst".to_owned())
                .await
                .map_err(|e| e.to_string())
        })
        .await
        .map_err(|_| "timed out releasing the Find benchmark burst".to_owned())?
    })
}

fn transcript_scenario(
    app: &Path,
    bins: &Bins,
    dir: &Path,
    kind: TranscriptScenario,
) -> Result<Scenario<LongTranscript>> {
    let name = kind.name();
    println!("{name}: collecting transcript frame times and footprint");
    let (daemon, _) = Daemon::start(
        bins,
        dir,
        DaemonOptions {
            stream_mock: Some(kind.mock()),
            ..DaemonOptions::default()
        },
    )?;
    let follower = Follower::start(bins, dir, name)?;
    let run = format!("run_{}", follower.run.trim_start_matches("run_"));
    let mut options = vec![("RENO_OPEN", run.as_str())];
    let running = if matches!(kind, TranscriptScenario::FindStreaming) {
        options.extend([
            ("RENO_BENCH_FIND", "looked at"),
            ("RENO_BENCH_FIND_CAPTURE", "1"),
        ]);
        // Keep the mock run on its isolated daemon until the app has activated Find.
        Some(follower)
    } else {
        follower.wait()?;
        options.extend([("RENO_OPEN_FOLDS", "1"), ("RENO_BENCH_SCROLL", "1")]);
        None
    };
    let mut started = App::start(app, dir, &options)?;
    if started.first_frame()?.is_none() {
        return Ok(not_drawn(name));
    }
    if running.is_some() {
        let ready = Instant::now();
        while !started.with_log(|log| log.find_ready) {
            if let Some(status) = started.child.try_wait().map_err(|e| e.to_string())? {
                return Err(format!("reno-app exited before opening Find: {status}"));
            }
            if ready.elapsed() > FIRST_FRAME_TIMEOUT {
                return Ok(Scenario::NotDrawn("the Find strip did not draw".into()));
            }
            thread::sleep(Duration::from_millis(20));
        }
        release_find_burst(dir, &run)?;
    }
    let mut before = None;
    let mut peak = 0.0_f64;
    let mut gpu = 0.0_f64;
    let begun = Instant::now();
    let (rows, frames) = loop {
        let done = started.with_log(|log| log.scroll_rows.zip(log.scroll_frames.clone()));
        if let Some(done) = done {
            break done;
        }
        if let Some(status) = started.child.try_wait().map_err(|e| e.to_string())? {
            return Err(format!("reno-app exited during {name}: {status}"));
        }
        if begun.elapsed() > SCROLL_TIMEOUT {
            let drawn = started.frames();
            started.stop();
            daemon.stop()?;
            return Ok(Scenario::NotDrawn(format!(
                "{name} did not finish in {} s ({drawn} frames drawn)",
                SCROLL_TIMEOUT.as_secs()
            )));
        }
        let now = footprint(started.pid)?;
        before.get_or_insert(now.total_mib);
        peak = peak.max(now.total_mib);
        gpu = gpu.max(now.gpu_mib);
        thread::sleep(Duration::from_secs(2));
    };
    let end = footprint(started.pid)?;
    let lifetime_peak = Command::new("/usr/bin/vmmap")
        .args(["--summary", &started.pid.to_string()])
        .output()
        .ok()
        .and_then(|output| app_stats::parse_vmmap_peak(&String::from_utf8_lossy(&output.stdout)));
    peak = peak.max(end.total_mib).max(lifetime_peak.unwrap_or(0.0));
    gpu = gpu.max(end.gpu_mib);
    started.stop();
    if let Some(running) = running {
        if rows != 2_000 {
            drop(running);
            daemon.stop()?;
            return Ok(Scenario::NotDrawn(format!(
                "expected 2,000 streamed messages, saw {rows}"
            )));
        }
        running.wait()?;
    }
    daemon.stop()?;
    if matches!(kind, TranscriptScenario::LongTranscript) && rows < 2_000 {
        return Ok(Scenario::NotDrawn(format!(
            "expected the open long transcript, saw {rows} rows"
        )));
    }
    let Some(frame_ms) = app_stats::frame_latency(&frames).filter(|_| frames.len() >= 20) else {
        return Ok(Scenario::NotDrawn(format!(
            "{} frames in {name}",
            frames.len()
        )));
    };
    Ok(Scenario::Drawn(LongTranscript {
        rows,
        footprint_before_mib: before.unwrap_or(end.total_mib),
        footprint_peak_mib: peak,
        gpu_mib: gpu,
        frames: frames.len(),
        frame_ms,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn default_is_the_system_allocator_alone() {
        let options = parse_args(&args(&[])).expect("parse");
        assert!(!options.quick);
        assert_eq!(options.variants, vec![("system", Vec::new())]);
    }

    #[test]
    fn compare_builds_the_app_with_and_without_mimalloc() {
        let options = parse_args(&args(&["--quick", "--compare-allocators"])).expect("parse");
        assert!(options.quick);
        let labels: Vec<_> = options.variants.iter().map(|v| v.0).collect();
        assert_eq!(labels, ["system", "mimalloc"]);
        assert_eq!(options.variants[1].1, ["--features", "mimalloc"]);
    }

    #[test]
    fn idle_only_leaves_out_the_scenarios_that_need_the_window_drawn() {
        assert!(!parse_args(&args(&[])).expect("parse").idle_only);
        assert!(
            parse_args(&args(&["--idle-only"]))
                .expect("parse")
                .idle_only
        );
    }

    #[test]
    fn bad_options_are_refused() {
        assert!(parse_args(&args(&["--nope"])).is_err());
        assert!(parse_args(&args(&["--allocator", "system"])).is_err());
    }

    #[test]
    fn quick_is_shorter_than_full_and_full_is_the_plans_figures() {
        let quick = Plan::new(true);
        let full = Plan::new(false);
        assert!(quick.idle_s < full.idle_s && quick.idle_settle_s < full.idle_settle_s);
        assert_eq!(
            (full.cold_starts, full.idle_settle_s, full.idle_s),
            (5, 30, 120)
        );
        assert!(
            full.stream_s + full.stream_settle_s < 60,
            "inside the 60 s stream"
        );
        assert!(quick.stream_s + quick.stream_settle_s < 60);
    }

    #[test]
    fn the_run_id_is_read_from_the_clis_first_json_line() {
        assert_eq!(
            run_id(r#"{"run":"run_001abc"}"#).as_deref(),
            Some("run_001abc")
        );
        assert_eq!(run_id(r#""run_001abc""#).as_deref(), Some("run_001abc"));
        assert_eq!(run_id("not json"), None);
    }
}
