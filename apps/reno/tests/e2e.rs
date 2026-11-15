//! The CLI and the daemon as real processes, each test in its own data directory. No network, no
//! real harness (ARCH-16): the runs are the mock executor's.
//!
//! `reno daemon start` finds `renod` next to the `reno` binary, so the two are built together.
//! `cargo test --workspace` builds both; when only this package is tested, `renod` is built
//! first.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::Once;

use serde_json::Value;

const RENO: &str = env!("CARGO_BIN_EXE_reno");

/// Builds `renod` beside `reno` if it is not there yet.
fn ensure_renod() {
    static BUILD: Once = Once::new();
    BUILD.call_once(|| {
        let beside = Path::new(RENO).with_file_name("renod");
        if beside.exists() {
            return;
        }
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = Command::new(cargo)
            .args(["build", "--package", "renod"])
            .status()
            .expect("run cargo");
        assert!(status.success(), "cargo build --package renod failed");
        assert!(
            beside.exists(),
            "renod was not built at {}",
            beside.display()
        );
    });
}

/// One test's world: a data directory, and a way to run `reno` in it.
struct World {
    _root: tempfile::TempDir,
    data_dir: PathBuf,
    hold_mock: bool,
}

impl World {
    fn new() -> Self {
        ensure_renod();
        let root = tempfile::tempdir().expect("tempdir");
        let data_dir = root.path().join("data");
        Self {
            _root: root,
            data_dir,
            hold_mock: false,
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(RENO);
        command
            .arg("--data-dir")
            .arg(&self.data_dir)
            .args(args)
            .env_remove("RENO_DATA_DIR")
            .env_remove("RENO_SOCKET")
            .env_remove("RENO_LOG")
            .env_remove("RENO_MOCK_HOLD")
            .stdin(Stdio::null());
        if self.hold_mock {
            // `reno daemon start` passes its environment on to `renod`.
            command.env("RENO_MOCK_HOLD", "1");
        }
        command
    }

    fn reno(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run reno")
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let out = self.reno(&all);
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        serde_json::from_slice(&out.stdout).expect("one JSON document")
    }

    fn socket(&self) -> PathBuf {
        self.data_dir.join("reno.sock")
    }

    fn start_daemon(&self) -> u32 {
        let out = self.reno(&["daemon", "start"]);
        assert!(out.status.success(), "daemon start: {}", text(&out.stderr));
        let said = text(&out.stdout);
        assert!(said.starts_with("started renod (pid "), "{said}");
        self.pid()
    }

    fn pid(&self) -> u32 {
        let status = self.json(&["daemon", "status"]);
        assert_eq!(status["running"], true);
        status["pid"].as_u64().expect("pid") as u32
    }

    fn runs(&self) -> Vec<Value> {
        self.json(&["run", "list"])["runs"]
            .as_array()
            .expect("runs")
            .clone()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        // A test that failed halfway must not leave a daemon behind.
        let _ = self.command(&["daemon", "stop"]).output();
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn alive(pid: u32) -> bool {
    Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .expect("kill -0")
        .success()
}

#[test]
fn start_run_list_and_stop() {
    let world = World::new();

    // Nothing is running yet.
    let status = world.reno(&["daemon", "status"]);
    assert_eq!(status.status.code(), Some(1));
    assert!(text(&status.stdout).starts_with("not running"));
    let down = world.reno(&["run", "list"]);
    assert_eq!(down.status.code(), Some(1));
    assert!(
        text(&down.stderr).contains("reno daemon start"),
        "{}",
        text(&down.stderr)
    );

    let pid = world.start_daemon();
    assert!(alive(pid));
    let again = world.reno(&["daemon", "start"]);
    assert!(again.status.success());
    assert!(
        text(&again.stdout).starts_with("renod is already running"),
        "{}",
        text(&again.stdout)
    );

    // Its socket is private.
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(world.socket())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    // A task streams its transcript and exits 0.
    let run = world.reno(&["task", "run", "--executor", "mock", "hi"]);
    let out = text(&run.stdout);
    assert!(run.status.success(), "{out}\n{}", text(&run.stderr));
    assert!(out.contains("Hello from the mock executor."), "{out}");
    assert!(out.contains("> mock_tool"), "{out}");
    assert!(out.contains("ok: the mock tool finished"), "{out}");
    assert!(out.trim_end().ends_with("done (completed)"), "{out}");

    // The run is listed as succeeded, with its prompt.
    let runs = world.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["status"], "succeeded");
    assert_eq!(runs[0]["title"], "hi");
    assert_eq!(runs[0]["prompt"], "hi");
    assert_eq!(runs[0]["executor"], "mock");
    let id = runs[0]["runId"].as_str().unwrap().to_owned();
    let listed = world.reno(&["run", "list"]);
    let line = text(&listed.stdout);
    assert!(
        line.contains(&id) && line.contains("succeeded") && line.ends_with("hi\n"),
        "{line}"
    );

    // `run tail` replays it, and --from starts later, inclusive.
    let tail = text(&world.reno(&["run", "tail", &id]).stdout);
    assert_eq!(tail, out);
    let late = text(&world.reno(&["run", "tail", &id, "--from", "5"]).stdout);
    assert!(!late.contains("Hello from"), "{late}");
    assert!(late.contains("done (completed)"), "{late}");

    // --json: the run id first, then one event per line, ending in Done.
    let streamed = world.reno(&["--json", "task", "run", "again"]);
    assert!(streamed.status.success());
    let lines: Vec<Value> = text(&streamed.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("each line is JSON"))
        .collect();
    assert!(lines[0]["runId"].as_str().unwrap().starts_with("run_"));
    let events = &lines[1..];
    let seqs: Vec<u64> = events.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert_eq!(seqs, (1..=events.len() as u64).collect::<Vec<_>>());
    assert_eq!(events[0]["event"]["type"], "started");
    let last = &events.last().unwrap()["event"];
    assert_eq!(
        (last["type"].clone(), last["stop"].clone()),
        ("done".into(), "completed".into())
    );
    assert_eq!(world.runs().len(), 2);

    // Errors are short and exit 1.
    let bad = world.reno(&["task", "run", "--executor", "nope", "hi"]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(
        text(&bad.stderr).contains("no executor `nope`"),
        "{}",
        text(&bad.stderr)
    );
    let cancel = world.reno(&["run", "cancel", &id]);
    assert_eq!(cancel.status.code(), Some(1));
    assert!(
        text(&cancel.stderr).contains("cannot be cancelled"),
        "{}",
        text(&cancel.stderr)
    );
    let nothing = world.reno(&["approve", "appr_nothere"]);
    assert_eq!(nothing.status.code(), Some(1));
    assert!(
        text(&nothing.stderr).contains("not found"),
        "{}",
        text(&nothing.stderr)
    );
    assert!(text(&world.reno(&["approval", "list"]).stdout).contains("no approvals"));
    assert_eq!(
        world.reno(&["run", "tail", "nonsense"]).status.code(),
        Some(2)
    );

    // Stop: the process is gone and so is the socket.
    let stop = world.reno(&["daemon", "stop"]);
    assert!(stop.status.success(), "{}", text(&stop.stderr));
    assert!(
        text(&stop.stdout).starts_with("stopped renod"),
        "{}",
        text(&stop.stdout)
    );
    assert!(!alive(pid), "renod {pid} is still running");
    assert!(!world.socket().exists(), "the socket file is left behind");
    assert!(text(&world.reno(&["daemon", "stop"]).stdout).contains("not running"));
}

/// Reads `child`'s stdout until it holds `needle`; the reader is the only thing that waits.
fn read_until(child: &mut Child, needle: &str) -> String {
    let stdout = child.stdout.as_mut().expect("piped stdout");
    let mut seen = String::new();
    let mut chunk = [0_u8; 256];
    while !seen.contains(needle) {
        let n = stdout.read(&mut chunk).expect("read stdout");
        assert!(n > 0, "the stream ended before {needle:?}: {seen}");
        seen.push_str(&String::from_utf8_lossy(&chunk[..n]));
    }
    seen
}

#[test]
fn a_cancelled_run_ends_the_task_command_non_zero() {
    let mut world = World::new();
    world.hold_mock = true;
    world.start_daemon();

    // The held mock stops after its first text, so this command keeps streaming.
    let mut task = world
        .command(&["task", "run", "wait for me"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn task run");
    let seen = read_until(&mut task, "Hello from the mock executor.");

    let runs = world.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["status"], "running");
    let id = runs[0]["runId"].as_str().unwrap().to_owned();
    let cancelled = world.reno(&["run", "cancel", &id]);
    assert!(cancelled.status.success(), "{}", text(&cancelled.stderr));
    assert!(text(&cancelled.stdout).starts_with(&format!("cancelling {id}")));

    let rest = task.wait_with_output().expect("task run ends");
    assert_eq!(
        rest.status.code(),
        Some(1),
        "an interrupted run is not success"
    );
    let all = format!("{seen}{}", text(&rest.stdout));
    assert!(all.contains("done (interrupted)"), "{all}");
    assert_eq!(world.runs()[0]["status"], "interrupted");
    assert_eq!(world.runs()[0]["stopReason"], "interrupted");

    let stop = world.reno(&["daemon", "stop"]);
    assert!(stop.status.success());
}
