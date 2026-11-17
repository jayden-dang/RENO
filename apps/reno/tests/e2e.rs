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
        // `reno` runs in the daemon's own scratch directory: a run's default working directory
        // is the current one, and a write run in a git repository (the repository this test
        // lives in) would get a worktree and a branch there.
        std::fs::create_dir_all(data_dir.join("workspace")).expect("scratch directory");
        Self {
            _root: root,
            data_dir,
            hold_mock: false,
        }
    }

    /// Where this world's bots, triggers and policy live.
    fn defs_dir(&self) -> PathBuf {
        self._root.path().join("defs")
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(RENO);
        command
            .arg("--data-dir")
            .arg(&self.data_dir)
            .args(args)
            .env_remove("RENO_DATA_DIR")
            .env_remove("RENO_SOCKET")
            // Keep the definitions inside the test's own directory; `reno daemon start` passes
            // this on to `renod`.
            .env("RENO_DEFS_DIR", self.defs_dir())
            .env_remove("RENO_LOG")
            .env_remove("RENO_MOCK_HOLD")
            // Never look at, let alone run, the real launchd.
            .env(
                "RENO_LAUNCH_AGENTS_DIR",
                self._root.path().join("LaunchAgents"),
            )
            .env("RENO_LAUNCHCTL", "/usr/bin/false")
            // `reno daemon start` passes its environment on to `renod`: never probe a real
            // harness CLI from a test.
            .env("RENO_NO_HARNESSES", "1")
            // Secrets live in the daemon's memory, never the real Keychain.
            .env("RENO_MEMORY_SECRETS", "1")
            .current_dir(self.data_dir.join("workspace"))
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
fn run_list_limit_and_all() {
    let world = World::new();
    world.start_daemon();
    for prompt in ["one", "two", "three"] {
        let run = world.reno(&["task", "run", "--executor", "mock", prompt]);
        assert!(run.status.success(), "{}", text(&run.stderr));
    }

    let limited = world.json(&["run", "list", "--limit", "2"]);
    let titles: Vec<_> = limited["runs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["title"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(titles, ["three", "two"], "newest first");
    assert!(limited["next"].is_string(), "{limited}");
    let text_out = text(&world.reno(&["run", "list", "--limit", "2"]).stdout);
    assert!(text_out.contains("more runs exist"), "{text_out}");

    let all = world.json(&["run", "list", "--all"]);
    assert_eq!(all["runs"].as_array().unwrap().len(), 3);
    assert!(all.get("next").is_none(), "{all}");
    // The default page (20) holds all three too.
    assert_eq!(world.runs().len(), 3);
    // A zero limit is refused by the argument parser.
    assert!(
        !world
            .reno(&["run", "list", "--limit", "0"])
            .status
            .success()
    );
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
    // Standing rules: none yet, an unknown one cannot be revoked, and a scope needs a known name.
    assert_eq!(
        text(&world.reno(&["rule", "list"]).stdout).trim(),
        "no standing rules"
    );
    assert_eq!(
        world.json(&["rule", "list", "--all"])["rules"],
        serde_json::json!([])
    );
    let unknown = world.reno(&["rule", "revoke", "rule_nothere"]);
    assert_eq!(unknown.status.code(), Some(1));
    assert!(
        text(&unknown.stderr).contains("not found"),
        "{}",
        text(&unknown.stderr)
    );
    let remembered = world.reno(&[
        "approve",
        "appr_nothere",
        "--remember",
        "bot",
        "--expires-in",
        "60",
    ]);
    assert_eq!(remembered.status.code(), Some(1));
    assert!(
        text(&remembered.stderr).contains("not found"),
        "{}",
        text(&remembered.stderr)
    );
    let bad_scope = world.reno(&["approve", "appr_nothere", "--remember", "forever"]);
    assert_eq!(bad_scope.status.code(), Some(2));
    let orphan = world.reno(&["approve", "appr_nothere", "--expires-in", "60"]);
    assert_eq!(orphan.status.code(), Some(2));
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

#[test]
fn an_interrupted_run_is_resumed_from_the_command_line() {
    let mut world = World::new();
    world.hold_mock = true;
    world.start_daemon();
    let mut task = world
        .command(&["task", "run", "wait for me"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn task run");
    read_until(&mut task, "Hello from the mock executor.");
    let id = world.runs()[0]["runId"].as_str().unwrap().to_owned();

    // A live run is not resumable, and the refusal says why in words and as a typed reason.
    let early = world.reno(&["run", "resume", &id]);
    assert_eq!(early.status.code(), Some(1));
    assert!(
        text(&early.stderr).contains("cannot be resumed"),
        "{}",
        text(&early.stderr)
    );
    assert_eq!(
        world.reno(&["run", "resume", "run_nothere"]).status.code(),
        Some(1)
    );

    assert!(world.reno(&["run", "cancel", &id]).status.success());
    task.wait_with_output().expect("task run ends");

    // The run says who stopped it and that the daemon will not take it back.
    let shown = text(&world.reno(&["run", "show", &id]).stdout);
    assert!(shown.contains("stopped: the run was cancelled"), "{shown}");
    assert!(
        shown.contains("not resumed: it was cancelled on request"),
        "{shown}"
    );
    let json = world.json(&["run", "show", &id]);
    assert_eq!(json["run"]["stop"]["type"], "cancelled");
    assert_eq!(json["run"]["notResumed"]["type"], "cancelled");
    assert_eq!(json["run"]["session"], "mock-session");

    let resumed = world.reno(&["run", "resume", &id, "--prompt", "carry on"]);
    assert!(resumed.status.success(), "{}", text(&resumed.stderr));
    assert!(
        text(&resumed.stdout).starts_with(&format!("resuming {id} as attempt 2")),
        "{}",
        text(&resumed.stdout)
    );
    // Following the run shows the resume in its transcript, and the second attempt is live.
    // The first attempt's Done is seq 3, so the resume is seq 4: follow from there.
    let mut follow = world
        .command(&["run", "tail", &id, "--from", "4"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn run tail");
    read_until(&mut follow, "resumed: attempt 2 continues the session");
    assert_eq!(world.runs()[0]["attempt"], 2);
    assert!(world.reno(&["run", "cancel", &id]).status.success());
    let rest = follow.wait_with_output().expect("tail ends");
    assert!(text(&rest.stdout).contains("done (interrupted)"));

    assert!(world.reno(&["daemon", "stop"]).status.success());
}

#[test]
fn run_tail_of_a_resumed_run_continues_past_the_first_done() {
    let mut world = World::new();
    world.hold_mock = true;
    world.start_daemon();
    let mut task = world
        .command(&["task", "run", "wait for me"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn task run");
    read_until(&mut task, "Hello from the mock executor.");
    let id = world.runs()[0]["runId"].as_str().unwrap().to_owned();
    assert!(world.reno(&["run", "cancel", &id]).status.success());
    task.wait_with_output().expect("task run ends");
    assert!(world.reno(&["run", "resume", &id]).status.success());

    // From the start: the first attempt's Done is followed by the resume, which is followed.
    let mut follow = world
        .command(&["run", "tail", &id])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn run tail");
    read_until(&mut follow, "resumed: attempt 2 continues the session");
    assert!(world.reno(&["run", "cancel", &id]).status.success());
    let rest = follow.wait_with_output().expect("tail ends");
    let all = text(&rest.stdout);
    assert!(all.contains("done (interrupted)"), "{all}");
    assert_eq!(rest.status.code(), Some(1), "{all}");

    // Once both attempts are over, the whole transcript shows both Dones.
    let full = text(&world.reno(&["run", "tail", &id]).stdout);
    assert_eq!(full.matches("done (interrupted)").count(), 2, "{full}");
    assert!(full.contains("resumed: attempt 2"), "{full}");

    assert!(world.reno(&["daemon", "stop"]).status.success());
}

// ---- definitions --------------------------------------------------------------------------

fn write_def(dir: &Path, rel: &str, text: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

const TESTER: &str = "name = \"tester\"\ninstructions = \"test things\"\nexecutors = [\"mock\"]\n";

#[test]
fn defs_check_prints_every_error_and_exits_non_zero_without_a_daemon() {
    let world = World::new();
    let dir = world.defs_dir();
    write_def(
        &dir,
        "bots/keeper.toml",
        "name = \"keeper\"\ninstructions = \"x\"\nexecutors = [\"claude-code\", \"codex\", \"x\"]\ntriggers = [\"nightly\", \"ghost\"]\n",
    );
    write_def(
        &dir,
        "triggers/nightly.toml",
        "name = \"nightly\"\n[when]\nkind = \"cron\"\ncron = \"61 2 * * *\"\ntimezone = \"Asia/Ho_Chi_Minh\"\n[target]\nbot = \"keeper\"\nprompt = \"go\"\n",
    );
    write_def(&dir, "policy.toml", "mode = 1\n");

    let out = world.reno(&["defs", "check"]);
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert_eq!(
        text(&out.stdout),
        format!(
            "bots/keeper.toml: executors[2]: unknown executor \"x\"\n\
             bots/keeper.toml: triggers[1]: unknown trigger \"ghost\"\n\
             triggers/nightly.toml: when.cron: \"61 2 * * *\" is not a cron expression: Component error: Number out of bounds. (a cron has five fields: minute hour day-of-month month day-of-week)\n\
             policy.toml: line 1, column 1: unknown field `mode`, expected one of `default`, `approval_ttl`, `rules`\n\
             4 error(s) in {}\n",
            dir.display()
        )
    );

    let json = {
        let out = world.reno(&["--json", "defs", "check"]);
        assert_eq!(out.status.code(), Some(1));
        serde_json::from_slice::<Value>(&out.stdout).expect("one JSON document")
    };
    assert_eq!(json["ok"], false);
    assert_eq!(json["errors"].as_array().unwrap().len(), 4);
    assert_eq!(json["errors"][0]["file"], "bots/keeper.toml");
    assert_eq!(json["errors"][0]["path"], "executors[2]");

    // `--executor` teaches it an executor that a daemon has.
    write_def(
        &dir,
        "bots/keeper.toml",
        "name = \"keeper\"\ninstructions = \"x\"\nexecutors = [\"mystery\"]\n",
    );
    write_def(
        &dir,
        "triggers/nightly.toml",
        "name = \"nightly\"\n[when]\nkind = \"manual\"\n[target]\nbot = \"keeper\"\nprompt = \"go\"\n",
    );
    write_def(&dir, "policy.toml", "");
    let refused = world.reno(&["defs", "check"]);
    assert_eq!(refused.status.code(), Some(1));
    let accepted = world.reno(&["defs", "check", "--executor", "mystery"]);
    assert!(accepted.status.success(), "{}", text(&accepted.stdout));
    assert_eq!(
        text(&accepted.stdout),
        format!("ok: 3 definition(s) in {}\n", dir.display())
    );
    let json = world.json(&["defs", "check", "--executor", "mystery"]);
    assert_eq!(json["ok"], true);
    assert_eq!(json["definitions"], 3);
}

/// The daemon registers the ACP agents (`acp.devin`, ...) and the ones added from the registry,
/// installed or not, so a bot that lists one is valid there; the offline check says the same.
#[test]
fn defs_check_accepts_the_acp_executor_ids_the_daemon_accepts() {
    let world = World::new();
    let dir = world.defs_dir();
    write_def(
        &dir,
        "bots/builder.toml",
        "name = \"builder\"\ninstructions = \"x\"\nexecutors = [\"acp.devin\", \"acp.gemini\"]\n",
    );
    let out = world.reno(&["defs", "check"]);
    assert!(out.status.success(), "{}", text(&out.stdout));
    assert_eq!(
        text(&out.stdout),
        format!("ok: 1 definition(s) in {}\n", dir.display())
    );
}

const POLICY: &str = r#"
default = "ask"

[[rules]]
note = "cargo is fine"
effect = "allow"
match = { action = "command", command = "cargo *", scope = ["keeper"] }

[[rules]]
note = "outside the worktree"
effect = "deny"
match = { action = "edit", path = "/**" }
"#;

const CARGO: &str = r#"{"action":"command","tool":"bash","command":"cargo test"}"#;

#[test]
fn policy_check_evaluates_offline_and_names_the_rule() {
    let world = World::new();
    write_def(&world.defs_dir(), "policy.toml", POLICY);
    // No daemon is running, and none is needed.
    let out = world.reno(&["policy", "check", "--bot", "keeper", CARGO]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "allow (rule: cargo is fine)\n");

    // Scope: another bot, and no bot at all, fall to the default.
    let out = world.reno(&["policy", "check", "--bot", "scout", CARGO]);
    assert_eq!(text(&out.stdout), "ask (default)\n");
    let out = world.reno(&["policy", "check", CARGO]);
    assert_eq!(text(&out.stdout), "ask (default)\n");

    // The worktree decides what is outside it, including through `..`.
    let escape = r#"{"action":"edit","tool":"write","paths":["../elsewhere.txt"]}"#;
    let out = world.reno(&["policy", "check", "--cwd", "/work/tree", escape]);
    assert_eq!(
        text(&out.stdout),
        "deny (rule: outside the worktree): denied by policy: outside the worktree\n"
    );
    let inside = r#"{"action":"edit","tool":"write","paths":["src/lib.rs"]}"#;
    let out = world.reno(&["policy", "check", "--cwd", "/work/tree", inside]);
    assert_eq!(text(&out.stdout), "ask (default)\n");

    let json = world.json(&["policy", "check", "--bot", "keeper", CARGO]);
    assert_eq!(json["effect"], "allow");
    assert_eq!(json["rule"]["name"], "cargo is fine");
    assert_eq!(json["rule"]["index"], 0);
    assert!(json["policy"].as_str().unwrap().ends_with("policy.toml"));
    let json = world.json(&["policy", "check", CARGO]);
    assert_eq!(
        (json["effect"].as_str(), json["rule"].is_null()),
        (Some("ask"), true)
    );
}

#[test]
fn policy_check_without_a_policy_file_uses_the_default_and_says_so() {
    let world = World::new();
    std::fs::create_dir_all(world.defs_dir()).unwrap();
    let out = world.reno(&["policy", "check", CARGO]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    let said = text(&out.stdout);
    assert!(
        said.contains("no policy.toml") && said.ends_with("ask (default)\n"),
        "{said}"
    );
}

#[test]
fn policy_check_fails_on_a_bad_subject_or_a_bad_policy() {
    let world = World::new();
    write_def(&world.defs_dir(), "policy.toml", POLICY);
    let out = world.reno(&["policy", "check", "{\"tool\":\"x\"}"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("not a valid approval subject"),
        "{}",
        text(&out.stderr)
    );

    write_def(
        &world.defs_dir(),
        "policy.toml",
        "approval_ttl = \"soon\"\n",
    );
    let out = world.reno(&["policy", "check", CARGO]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("approval_ttl"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn approval_history_is_empty_on_a_fresh_daemon() {
    let world = World::new();
    world.start_daemon();
    let out = world.reno(&["approval", "history"]);
    assert!(out.status.success(), "{}", text(&out.stderr));
    assert_eq!(text(&out.stdout), "no approvals have been decided\n");
    let json = world.json(&["approval", "history", "--limit", "5"]);
    assert_eq!(json["approvals"].as_array().unwrap().len(), 0);
}

#[test]
fn defs_check_reports_a_directory_that_is_not_there() {
    let world = World::new();
    let out = world.reno(&["defs", "check"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(
        text(&out.stderr).contains("is not a directory"),
        "{}",
        text(&out.stderr)
    );
}

#[test]
fn a_fresh_daemon_has_example_definitions_that_check_clean() {
    let world = World::new();
    world.start_daemon();
    let check = world.reno(&["defs", "check"]);
    assert!(check.status.success(), "{}", text(&check.stdout));
    assert!(
        text(&check.stdout).starts_with("ok: 2 definition(s)"),
        "{}",
        text(&check.stdout)
    );

    let listed = text(&world.reno(&["defs", "list"]).stdout);
    let lines: Vec<&str> = listed.lines().collect();
    assert_eq!(lines.len(), 2, "{listed}");
    assert!(
        lines[0].starts_with("bot      example") && lines[0].contains(" v1 "),
        "{listed}"
    );
    assert!(
        lines[1].starts_with("policy   policy") && lines[1].contains(" v1 "),
        "{listed}"
    );

    let json = world.json(&["defs", "list", "--kind", "policy"]);
    assert_eq!(json["defs"].as_array().unwrap().len(), 1);
    assert_eq!(json["defs"][0]["kind"], "policy");
    assert_eq!(json["errors"].as_array().map_or(0, Vec::len), 0);
}

#[test]
fn defs_show_prints_a_version_as_toml() {
    let world = World::new();
    world.start_daemon();
    let shown = world.reno(&["defs", "show", "bot", "example"]);
    let out = text(&shown.stdout);
    assert!(shown.status.success(), "{}", text(&shown.stderr));
    let first = out.lines().next().unwrap();
    assert!(first.starts_with("# bot example v1, latest  "), "{first}");
    let body: toml::Table = toml::from_str(&out).expect("the body is TOML");
    assert_eq!(body["name"].as_str(), Some("example"));
    assert_eq!(body["access"].as_str(), Some("supervised"));
    assert_eq!(body["budget"]["max_wall_time"].as_str(), Some("45m"));

    let record = world.json(&["defs", "show", "bot", "example", "--version", "1"]);
    assert_eq!(record["version"], 1);
    assert_eq!(record["latest"], true);
    assert_eq!(record["body"]["name"], "example");
    let policy = world.json(&["defs", "show", "policy", "policy"]);
    assert_eq!(policy["body"]["default"], "ask");

    for args in [
        ["defs", "show", "bot", "ghost"],
        ["defs", "show", "trigger", "example"],
    ] {
        let missing = world.reno(&args);
        assert_eq!(missing.status.code(), Some(1));
        assert!(
            text(&missing.stderr).contains("not found"),
            "{}",
            text(&missing.stderr)
        );
    }
    let version = world.reno(&["defs", "show", "bot", "example", "--version", "7"]);
    assert_eq!(version.status.code(), Some(1));
    assert!(
        text(&version.stderr).contains("version 7 not found"),
        "{}",
        text(&version.stderr)
    );
    // Not a kind: refused by the argument parser.
    assert_eq!(
        world.reno(&["defs", "show", "flow", "x"]).status.code(),
        Some(2)
    );
}

#[test]
fn defs_list_says_so_when_the_files_have_errors() {
    let world = World::new();
    write_def(&world.defs_dir(), "bots/tester.toml", TESTER);
    // `daemon start` returns once the daemon answers, which is after its first load.
    world.start_daemon();
    write_def(
        &world.defs_dir(),
        "bots/tester.toml",
        &TESTER.replace("\"mock\"", "\"nope\""),
    );
    // The daemon reads the edit when its watcher reports it. There is no event to follow across
    // a process boundary, so this asks again until the errors show, with a failsafe.
    let mut listed = String::new();
    for _ in 0..1200 {
        listed = text(&world.reno(&["defs", "list"]).stdout);
        if listed.contains("error(s)") {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(listed.starts_with("bot      tester"), "{listed}");
    assert!(
        listed.contains("1 error(s); the definitions above are the last good set"),
        "{listed}"
    );
    assert!(
        listed.contains("bots/tester.toml: executors[0]: unknown executor \"nope\""),
        "{listed}"
    );
}

#[test]
fn a_task_run_as_a_bot_is_pinned_to_the_bots_version() {
    let world = World::new();
    // A directory that already has definitions is not filled with examples.
    write_def(&world.defs_dir(), "bots/tester.toml", TESTER);
    world.start_daemon();

    let run = world.reno(&["task", "run", "--bot", "tester", "hello"]);
    assert!(run.status.success(), "{}", text(&run.stderr));
    let runs = world.runs();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0]["executor"], "mock", "the bot's first executor");
    assert_eq!(runs[0]["bot"]["name"], "tester");
    assert_eq!(runs[0]["bot"]["version"], 1);
    let listed = world.json(&["defs", "list"]);
    assert_eq!(
        listed["defs"].as_array().unwrap().len(),
        1,
        "no examples were added"
    );

    let unknown = world.reno(&["task", "run", "--bot", "ghost", "hello"]);
    assert_eq!(unknown.status.code(), Some(1));
    assert!(
        text(&unknown.stderr).contains("not found"),
        "{}",
        text(&unknown.stderr)
    );
}

#[test]
fn trigger_list_and_fire_through_the_cli() {
    let world = World::new();
    write_def(&world.defs_dir(), "bots/tester.toml", TESTER);
    write_def(
        &world.defs_dir(),
        "triggers/poke.toml",
        "name = \"poke\"\n[when]\nkind = \"manual\"\n[target]\nbot = \"tester\"\nprompt = \"poked\"\n",
    );
    write_def(
        &world.defs_dir(),
        "triggers/nightly.toml",
        "name = \"nightly\"\n[when]\nkind = \"cron\"\ncron = \"0 2 * * *\"\ntimezone = \"UTC\"\n[target]\nbot = \"tester\"\nprompt = \"nightly\"\ncwd = \"~/code/app\"\n",
    );
    world.start_daemon();

    let list = world.reno(&["trigger", "list"]);
    assert!(list.status.success(), "{}", text(&list.stderr));
    let shown = text(&list.stdout);
    assert!(
        shown.contains("nightly") && shown.contains("scheduled") && shown.contains("next "),
        "{shown}"
    );
    assert!(
        shown.contains("poke") && shown.contains("manual"),
        "{shown}"
    );
    assert!(shown.contains("cwd ~/code/app"), "{shown}");
    assert_eq!(
        shown.matches("cwd").count(),
        1,
        "only nightly names one: {shown}"
    );

    let fired = world.reno(&["trigger", "fire", "poke"]);
    assert!(fired.status.success(), "{}", text(&fired.stderr));
    assert!(
        text(&fired.stdout).starts_with("poke fired: run run_"),
        "{}",
        text(&fired.stdout)
    );
    let json = world.json(&["trigger", "list"]);
    let poke = json["triggers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["name"] == "poke")
        .unwrap();
    assert!(poke["lastRunId"].as_str().unwrap().starts_with("run_"));
    assert_eq!(world.runs().len(), 1);

    let ghost = world.reno(&["trigger", "fire", "ghost"]);
    assert_eq!(ghost.status.code(), Some(1));
    assert!(
        text(&ghost.stderr).contains("not found"),
        "{}",
        text(&ghost.stderr)
    );
}

#[test]
fn pause_resume_and_budget_through_the_cli() {
    let world = World::new();
    write_def(&world.defs_dir(), "bots/tester.toml", TESTER);
    world.start_daemon();

    let budget = world.json(&["budget"]);
    assert_eq!(budget["paused"], false);
    let tester = budget["bots"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["bot"] == "tester")
        .unwrap()
        .clone();
    assert_eq!(tester["maxConcurrentRuns"], 1);
    assert_eq!(tester["paused"], false);
    let text_out = text(&world.reno(&["budget"]).stdout);
    assert!(
        text_out.starts_with("running") && text_out.contains("tester: 0/1 live"),
        "{text_out}"
    );

    // Pausing one bot, then everything.
    let one = world.reno(&["pause", "--bot", "tester"]);
    assert!(one.status.success(), "{}", text(&one.stderr));
    assert!(
        text(&one.stdout).starts_with("paused bot tester"),
        "{}",
        text(&one.stdout)
    );
    let all = world.json(&["pause"]);
    assert_eq!(all["changed"], true);
    assert_eq!(world.json(&["pause"])["changed"], false, "already paused");
    let shown = text(&world.reno(&["budget"]).stdout);
    assert!(
        shown.starts_with("PAUSED") && shown.contains("tester (paused)"),
        "{shown}"
    );
    let status = text(&world.reno(&["daemon", "status"]).stdout);
    assert!(status.contains("paused: nothing new starts"), "{status}");
    assert_eq!(world.json(&["daemon", "status"])["paused"], true);

    // A run submitted now is queued, and stays so until the resume.
    let queued = world.json(&["budget"]);
    assert_eq!(queued["paused"], true);

    let ghost = world.reno(&["pause", "--bot", "ghost"]);
    assert_eq!(ghost.status.code(), Some(1));
    assert!(
        text(&ghost.stderr).contains("not found"),
        "{}",
        text(&ghost.stderr)
    );

    let resumed = world.reno(&["resume"]);
    assert!(resumed.status.success(), "{}", text(&resumed.stderr));
    assert_eq!(text(&resumed.stdout).trim(), "resumed everything");
    let again = world.reno(&["resume"]);
    assert_eq!(text(&again.stdout).trim(), "everything was not paused");
    assert_eq!(world.json(&["resume", "--bot", "tester"])["changed"], true);
    assert_eq!(world.json(&["budget"])["paused"], false);
    assert_eq!(world.json(&["daemon", "status"])["paused"], false);
}

#[test]
fn executor_list_and_refresh_through_the_cli() {
    let world = World::new();
    world.start_daemon();

    // A refresh waits for the probe, so what follows is the daemon's own answer, not a race with
    // its start-up probe.
    let refreshed = world.reno(&["executor", "refresh"]);
    assert!(refreshed.status.success(), "{}", text(&refreshed.stderr));
    let shown = text(&refreshed.stdout);
    assert!(
        shown.starts_with("mock") && shown.contains("installed") && shown.contains("signed in"),
        "{shown}"
    );

    let json = world.json(&["executor", "list"]);
    let mock = &json["executors"][0];
    assert_eq!(mock["descriptor"]["id"], "mock");
    assert_eq!(mock["health"]["installed"]["type"], "installed");
    assert_eq!(mock["health"]["signedIn"]["type"], "yes");
    assert_eq!(mock["health"]["models"]["source"], "curated");
    assert!(mock["health"]["checkedAtMs"].as_u64().unwrap() > 0);

    let one = world.reno(&["executor", "refresh", "mock"]);
    assert!(one.status.success(), "{}", text(&one.stderr));
    assert_eq!(text(&one.stdout).lines().count(), 1);

    let ghost = world.reno(&["executor", "refresh", "nowhere"]);
    assert_eq!(ghost.status.code(), Some(1));
    assert!(
        text(&ghost.stderr).contains("not found"),
        "{}",
        text(&ghost.stderr)
    );
}

#[test]
fn executor_frames_through_the_cli_say_none_for_a_clean_executor() {
    let world = World::new();
    world.start_daemon();
    let shown = world.reno(&["executor", "frames"]);
    assert!(shown.status.success(), "{}", text(&shown.stderr));
    assert!(
        text(&shown.stdout).contains("no unrecognised frames"),
        "{}",
        text(&shown.stdout)
    );
    let json = world.json(&["executor", "frames", "mock"]);
    assert_eq!(json["executors"][0]["id"], "mock");
    assert_eq!(json["executors"][0]["frames"], serde_json::json!([]));
    let ghost = world.reno(&["executor", "frames", "nowhere"]);
    assert_eq!(ghost.status.code(), Some(1));
}

// ---- workspaces ---------------------------------------------------------------------------

/// A hermetic `git`: nothing from the user's own configuration leaks in.
fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .output()
        .expect("git runs");
    assert!(out.status.success(), "git {args:?}: {}", text(&out.stderr));
    text(&out.stdout).trim().to_owned()
}

#[test]
fn workspace_list_show_and_clean_through_the_cli() {
    let mut world = World::new();
    world.hold_mock = true;
    let repo = world._root.path().canonicalize().unwrap().join("proj");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "-q", "-b", "main"]);
    std::fs::write(repo.join("README.md"), "hi\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-qm", "one"]);
    let index_before = std::fs::read(repo.join(".git/index")).unwrap();
    world.start_daemon();

    let empty = world.reno(&["workspace", "list"]);
    assert_eq!(text(&empty.stdout).trim(), "no workspaces");

    let cwd = repo.to_str().unwrap();
    let mut task = world
        .command(&["task", "run", "--cwd", cwd, "work in a worktree"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn task run");
    read_until(&mut task, "Hello from the mock executor.");
    let run = world.runs()[0]["runId"].as_str().unwrap().to_owned();

    let shown = world.json(&["workspace", "show", &run]);
    assert_eq!(shown["status"], "active");
    assert_eq!(shown["branch"], format!("reno/{run}"));
    let path = PathBuf::from(shown["path"].as_str().unwrap());
    assert!(path.starts_with(world.data_dir.join("worktrees")));
    assert!(path.join("README.md").exists());
    let detail = text(&world.reno(&["workspace", "show", &run]).stdout);
    assert!(
        detail.contains("worktree") && detail.contains("base"),
        "{detail}"
    );

    // The agent commits in its worktree, and the run is cancelled.
    std::fs::write(path.join("agent.txt"), "x\n").unwrap();
    git(&path, &["add", "agent.txt"]);
    git(&path, &["commit", "-qm", "agent"]);
    assert!(world.reno(&["run", "cancel", &run]).status.success());
    task.wait_with_output().expect("task run ends");

    let listed = text(&world.reno(&["workspace", "list"]).stdout);
    assert!(
        listed.contains(&run) && listed.contains("kept") && listed.contains("1 commit(s)"),
        "{listed}"
    );
    let only = world.json(&["workspace", "list", "--status", "removed"]);
    assert!(only["workspaces"].as_array().unwrap().is_empty());

    // Unmerged commits are kept by every clean, and removed only by --force.
    for args in [
        vec!["workspace", "clean", &run],
        vec!["workspace", "clean", "--merged"],
        vec!["workspace", "clean", "--all-clean"],
    ] {
        let out = world.reno(&args);
        assert!(out.status.success(), "{}", text(&out.stderr));
        assert!(path.exists(), "{args:?}");
    }
    let kept = text(&world.reno(&["workspace", "clean", &run]).stdout);
    assert!(kept.contains("kept") && kept.contains("--force"), "{kept}");

    let forced = world.json(&["workspace", "clean", &run, "--force"]);
    assert_eq!(forced["items"][0]["outcome"], "removed");
    assert!(!path.exists());
    assert_eq!(git(&repo, &["branch", "--list", "reno/*"]), "");
    assert_eq!(
        std::fs::read(repo.join(".git/index")).unwrap(),
        index_before
    );

    // A target is required, and an unknown run is not found.
    assert!(!world.reno(&["workspace", "clean"]).status.success());
    let ghost = world.reno(&["workspace", "show", "run_ghost"]);
    assert_eq!(ghost.status.code(), Some(1));
    assert!(
        text(&ghost.stderr).contains("has no workspace"),
        "{}",
        text(&ghost.stderr)
    );
}

#[test]
fn executor_install_refuses_what_has_nothing_to_install_and_never_fetches() {
    let world = World::new();
    world.start_daemon();

    // The mock is installed already: the daemon says so and runs no install.
    let mock = world.reno(&["executor", "install", "mock"]);
    assert_eq!(mock.status.code(), Some(1));
    let err = text(&mock.stderr);
    assert!(err.contains("nothing to install"), "{err}");

    let ghost = world.reno(&["executor", "install", "nowhere"]);
    assert_eq!(ghost.status.code(), Some(1));
    assert!(
        text(&ghost.stderr).contains("not found"),
        "{}",
        text(&ghost.stderr)
    );
}

#[test]
fn run_show_explains_which_executor_was_chosen_and_why() {
    let world = World::new();
    world.start_daemon();

    // No `--executor`: the daemon's router chooses, and says so on stderr, keeping stdout the
    // transcript.
    let run = world.reno(&["task", "run", "hi"]);
    assert!(run.status.success(), "{}", text(&run.stderr));
    let said = text(&run.stderr);
    assert!(said.contains("routed to mock (default order)"), "{said}");
    assert!(
        !text(&run.stdout).contains("routed to"),
        "{}",
        text(&run.stdout)
    );

    // --json: the run id and its decision on the first line.
    let streamed = world.reno(&["--json", "task", "run", "again"]);
    let out = text(&streamed.stdout);
    let first: Value = serde_json::from_str(out.lines().next().unwrap()).unwrap();
    assert_eq!(first["routing"]["chosen"], "mock");
    assert_eq!(first["routing"]["ruleMatched"]["type"], "default");

    let id = world.runs()[0]["runId"].as_str().unwrap().to_owned();
    let shown = text(&world.reno(&["run", "show", &id]).stdout);
    assert!(shown.contains("routing: mock (default order)"), "{shown}");
    assert!(shown.contains("ranked: mock"), "{shown}");
    let json = world.json(&["run", "show", &id]);
    assert_eq!(json["run"]["executor"], "mock");
    assert_eq!(json["routing"]["chosen"], "mock");
    assert_eq!(json["routing"]["ranked"][0], "mock");

    // A model asked for by an alias is shown with the one the run started with.
    let aliased = world.reno(&["task", "run", "--model", "sonnet", "aliased"]);
    assert!(aliased.status.success(), "{}", text(&aliased.stderr));
    let id = world.runs()[0]["runId"].as_str().unwrap().to_owned();
    let shown = text(&world.reno(&["run", "show", &id]).stdout);
    assert!(
        shown.contains("model: sonnet (started as mock-1)"),
        "{shown}"
    );
    let json = world.json(&["run", "show", &id]);
    assert_eq!(json["run"]["model"], "sonnet");
    assert_eq!(json["startedModel"], "mock-1");

    // A pinned executor is its own rule.
    let pinned = world.reno(&["task", "run", "--executor", "mock", "pinned"]);
    assert!(
        text(&pinned.stderr).contains("routed to mock (the task named it)"),
        "{}",
        text(&pinned.stderr)
    );

    // An unknown run is a short error.
    let missing = world.reno(&["run", "show", "run_nothere"]);
    assert_eq!(missing.status.code(), Some(1));
    assert!(
        text(&missing.stderr).contains("not found"),
        "{}",
        text(&missing.stderr)
    );
}

#[test]
fn secrets_are_set_from_stdin_listed_by_name_and_deleted() {
    use std::io::Write;

    const VALUE: &str = "ghp_exampleExampleExample1234";
    let world = World::new();
    world.start_daemon();

    let set = |value: &str| {
        let mut child = world
            .command(&["secret", "set", "GITHUB_TOKEN", "--from-stdin"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("run reno");
        child
            .stdin
            .take()
            .unwrap()
            .write_all(value.as_bytes())
            .unwrap();
        child.wait_with_output().expect("reno finished")
    };
    let first = set(&format!("{VALUE}\n"));
    assert!(first.status.success(), "{}", text(&first.stderr));
    assert_eq!(text(&first.stdout).trim(), "stored GITHUB_TOKEN");
    let second = set(&format!("{VALUE}\n"));
    assert_eq!(text(&second.stdout).trim(), "replaced GITHUB_TOKEN");
    for out in [&first, &second] {
        assert!(!text(&out.stdout).contains(VALUE) && !text(&out.stderr).contains(VALUE));
    }

    let listed = world.reno(&["secret", "list"]);
    assert!(listed.status.success(), "{}", text(&listed.stderr));
    assert_eq!(text(&listed.stdout).trim(), "GITHUB_TOKEN  set");
    let json = world.json(&["secret", "list"]);
    assert_eq!(
        json["secrets"],
        serde_json::json!([{"name": "GITHUB_TOKEN", "set": true}])
    );
    assert!(!json.to_string().contains(VALUE));

    // Without --from-stdin a value is read only from a terminal; a pipe is refused.
    let refused = world.reno(&["secret", "set", "OTHER_TOKEN"]);
    assert!(!refused.status.success());
    assert!(
        text(&refused.stderr).contains("--from-stdin"),
        "{}",
        text(&refused.stderr)
    );
    // An empty value and a bad name store nothing.
    assert!(!set("\n").status.success());
    let bad = world.reno(&["secret", "set", "lower", "--from-stdin"]);
    assert!(!bad.status.success());
    assert!(text(&bad.stderr).contains("not a valid secret name"));

    let deleted = world.reno(&["secret", "delete", "GITHUB_TOKEN"]);
    assert_eq!(text(&deleted.stdout).trim(), "deleted GITHUB_TOKEN");
    let again = world.reno(&["secret", "delete", "GITHUB_TOKEN"]);
    assert_eq!(text(&again.stdout).trim(), "GITHUB_TOKEN was not set");
    assert_eq!(
        world.json(&["secret", "list"])["secrets"],
        serde_json::json!([])
    );
}

/// The one file under `dir` called `token`: a live run's token file.
fn find_token_file(dir: &Path) -> Option<PathBuf> {
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(found) = find_token_file(&path) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|name| name == "token") {
            return Some(path);
        }
    }
    None
}

#[test]
fn a_denial_with_a_message_reaches_the_history_the_journal_and_the_agent() {
    use std::io::{BufRead, BufReader, Write};

    let mut world = World::new();
    world.hold_mock = true;
    world.start_daemon();
    let mut task = world
        .command(&["task", "run", "wait for me"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn task run");
    read_until(&mut task, "Hello from the mock executor.");
    let run = world.runs()[0]["runId"].as_str().unwrap().to_owned();

    // Be the agent: ask for approval through the run's own MCP server (`reno mcp`).
    let token_file = find_token_file(&world.data_dir.join("run")).expect("the run's token file");
    let mut mcp = Command::new(RENO)
        .arg("mcp")
        .env("RENO_SOCKET", world.socket())
        .env("RENO_RUN_ID", &run)
        .env("RENO_TOKEN_FILE", &token_file)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn reno mcp");
    let mut stdin = mcp.stdin.take().unwrap();
    let mut lines = BufReader::new(mcp.stdout.take().unwrap()).lines();
    let mut send = |value: Value| {
        writeln!(stdin, "{value}").unwrap();
        stdin.flush().unwrap();
    };
    send(
        serde_json::json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {
        "protocolVersion": "2025-06-18", "capabilities": {},
        "clientInfo": {"name": "test", "version": "0"}}}),
    );
    loop {
        let line = lines.next().expect("a reply").unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        if reply["id"] == 1 {
            break;
        }
    }
    send(serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
    send(
        serde_json::json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {
        "name": "request_approval",
        "arguments": {"action": "command", "reason": "ship it", "command": "git push origin main"}}}),
    );

    // Wait for the held request, then deny it with a reason.
    let id = {
        let mut found = None;
        for _ in 0..200 {
            let listed = world.json(&["approval", "list"]);
            if let Some(first) = listed["approvals"].as_array().and_then(|a| a.first()) {
                found = Some(first["id"].as_str().unwrap().to_owned());
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        found.expect("a held approval")
    };
    let reason = "not before the review";
    let denied = world.reno(&["deny", &id, "--message", reason]);
    assert!(denied.status.success(), "{}", text(&denied.stderr));
    assert_eq!(text(&denied.stdout).trim(), format!("denied {id}"));

    // The agent was told why.
    let answer = loop {
        let line = lines.next().expect("the answer").unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        if reply["id"] == 2 {
            break reply;
        }
    };
    let said = answer["result"]["content"][0]["text"].as_str().unwrap();
    let said: Value = serde_json::from_str(said).unwrap();
    assert_eq!(said["decision"], "denied");
    assert_eq!(said["message"], reason);

    // The history says it, in words and as data.
    let history = text(&world.reno(&["approval", "history"]).stdout);
    assert!(history.contains(&format!("({reason})")), "{history}");
    let json = world.json(&["approval", "history"]);
    assert_eq!(json["approvals"][0]["message"], reason);

    // So does the run's journal.
    assert!(world.reno(&["run", "cancel", &run]).status.success());
    task.wait_with_output().expect("task run ends");
    let tail = world.reno(&["--json", "run", "tail", &run]);
    let journal = text(&tail.stdout);
    let resolved = journal
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|item| item.to_string().contains("approval_resolved"))
        .unwrap_or_else(|| panic!("no ApprovalResolved in {journal}"));
    assert!(resolved.to_string().contains(reason), "{resolved}");
    drop((stdin, lines));
    let _ = mcp.wait();

    // An approval takes no message.
    let refused = world.reno(&["approve", &id, "--message", "x"]);
    assert_eq!(refused.status.code(), Some(2));
    // An empty reason is refused before the daemon is asked.
    let empty = world.reno(&["deny", &id, "-m", "  "]);
    assert_eq!(empty.status.code(), Some(1));
    assert!(text(&empty.stderr).contains("--message is empty"));
}
