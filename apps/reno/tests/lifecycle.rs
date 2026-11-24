//! `reno daemon start | stop | status | restart | logs | install | uninstall` as real processes,
//! each test in its own data directory with its own LaunchAgents directory and a fake
//! `launchctl` (a script that plays launchd's part): nothing here ever touches the user's real
//! `~/Library/LaunchAgents` or launchd. No network and no real harness (ARCH-16).

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use rustix::process::{Pid, getpgid, getsid};
use serde_json::Value;

const RENO: &str = env!("CARGO_BIN_EXE_reno");

/// A launchd, as a shell script: `bootstrap` loads the plist's job and runs it (as `RunAtLoad`
/// does), `kickstart` runs it if it is not running, `bootout` stops it with SIGTERM and unloads
/// it, `print` says whether it is loaded (113 when not). Every call is appended to `calls`.
const FAKE_LAUNCHCTL: &str = r#"#!/bin/sh
state="$FAKE_LAUNCHD_DIR"
echo "$*" >> "$state/calls"
running() { [ -f "$state/pid" ] && kill -0 "$(cat "$state/pid")" 2>/dev/null; }
run_job() {
  plist="$1"
  set -- $(grep -E '^		<string>' "$plist" | head -5 | sed -e 's|^		<string>||' -e 's|</string>$||' | tr '\n' ' ')
  nohup "$@" >/dev/null 2>&1 &
  echo $! > "$state/pid"
}
case "$1" in
  bootstrap) cp "$3" "$state/loaded"; run_job "$3" ;;
  kickstart) running || run_job "$state/loaded" ;;
  bootout)
    [ -f "$state/loaded" ] || { echo "Boot-out failed: 3: No such process" >&2; exit 3; }
    if running; then
      kill -TERM "$(cat "$state/pid")"
      while running; do sleep 0.05; done
    fi
    rm -f "$state/loaded" "$state/pid" ;;
  print) [ -f "$state/loaded" ] || { echo "Could not find service" >&2; exit 113; } ;;
  *) echo "unexpected: $*" >&2; exit 64 ;;
esac
"#;

struct World {
    root: tempfile::TempDir,
    data_dir: PathBuf,
}

fn ensure_renod() {
    static BUILD: std::sync::Once = std::sync::Once::new();
    BUILD.call_once(|| {
        // Always: a `renod` left over from an earlier build would test old code. Building it is
        // a no-op when it is current.
        let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
        let status = Command::new(cargo)
            .args(["build", "--package", "renod"])
            .status()
            .expect("run cargo");
        assert!(status.success(), "cargo build --package renod failed");
    });
}

impl World {
    fn new() -> Self {
        ensure_renod();
        let root = tempfile::tempdir().expect("tempdir");
        let data_dir = root.path().join("data");
        std::fs::create_dir_all(data_dir.join("workspace")).expect("scratch");
        std::fs::create_dir_all(root.path().join("launchd")).expect("launchd state");
        let launchctl = root.path().join("launchctl");
        std::fs::write(&launchctl, FAKE_LAUNCHCTL).expect("fake launchctl");
        std::fs::set_permissions(&launchctl, std::fs::Permissions::from_mode(0o755))
            .expect("chmod");
        Self { root, data_dir }
    }

    fn agents_dir(&self) -> PathBuf {
        self.root.path().join("LaunchAgents")
    }

    fn launchd_calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.root.path().join("launchd/calls"))
            .map(|text| text.lines().map(str::to_owned).collect())
            .unwrap_or_default()
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(RENO);
        command
            .arg("--data-dir")
            .arg(&self.data_dir)
            .args(args)
            .env_remove("RENO_DATA_DIR")
            .env_remove("RENO_SOCKET")
            .env_remove("RENO_RENOD")
            .env("RENO_DEFS_DIR", self.root.path().join("defs"))
            .env("RENO_NO_HARNESSES", "1")
            .env("RENO_LAUNCH_AGENTS_DIR", self.agents_dir())
            .env("RENO_LAUNCHCTL", self.root.path().join("launchctl"))
            .env("FAKE_LAUNCHD_DIR", self.root.path().join("launchd"))
            .current_dir(self.data_dir.join("workspace"))
            .stdin(Stdio::null());
        command
    }

    fn reno(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("run reno")
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.reno(args);
        assert!(out.status.success(), "{args:?}: {}", text(&out.stderr));
        text(&out.stdout)
    }

    fn json(&self, args: &[&str]) -> Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        serde_json::from_str(&self.ok(&all)).expect("one JSON document")
    }

    fn pid(&self) -> u32 {
        let status = self.json(&["daemon", "status"]);
        assert_eq!(status["running"], true, "{status}");
        u32::try_from(status["pid"].as_u64().unwrap()).unwrap()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        // A test that failed halfway must not leave a daemon behind, detached or "launchd's".
        let _ = self.command(&["daemon", "stop"]).output();
        if let Ok(pid) = std::fs::read_to_string(self.root.path().join("launchd/pid")) {
            let _ = Command::new("kill").args(["-KILL", pid.trim()]).status();
        }
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn pid_of(raw: u32) -> Pid {
    Pid::from_raw(i32::try_from(raw).unwrap()).unwrap()
}

#[test]
fn start_is_idempotent_and_leaves_the_callers_session() {
    let world = World::new();
    let said = world.ok(&["daemon", "start"]);
    assert!(said.starts_with("started renod (pid "), "{said}");
    let pid = world.pid();

    // Its own session and process group: the terminal's hangup never reaches it.
    assert_eq!(getsid(Some(pid_of(pid))).unwrap(), pid_of(pid));
    assert_eq!(getpgid(Some(pid_of(pid))).unwrap(), pid_of(pid));

    let again = world.ok(&["daemon", "start"]);
    assert!(again.starts_with("renod is already running"), "{again}");
    assert_eq!(world.pid(), pid);
    let json = world.json(&["daemon", "start"]);
    assert_eq!(json["started"], false);

    let status = world.json(&["daemon", "status"]);
    for key in ["version", "uptimeSecs", "socket", "pid"] {
        assert!(!status[key].is_null(), "{key} in {status}");
    }
    assert!(status.get("launchAgent").is_none(), "no agent is installed");
    assert!(world.launchd_calls().is_empty(), "launchd was not involved");
}

#[test]
fn restart_replaces_the_process_and_starts_one_that_is_not_running() {
    let world = World::new();
    let said = world.ok(&["daemon", "restart"]);
    assert!(said.contains("was not running; started it"), "{said}");
    let first = world.pid();
    let said = world.ok(&["daemon", "restart"]);
    assert!(said.contains("restarted renod"), "{said}");
    assert_ne!(world.pid(), first);
}

#[test]
fn a_daemon_that_dies_on_start_is_reported_with_its_stderr() {
    let world = World::new();
    let renod = world.root.path().join("broken-renod");
    std::fs::write(
        &renod,
        "#!/bin/sh\necho 'renod: cannot open the store' >&2\nexit 7\n",
    )
    .unwrap();
    std::fs::set_permissions(&renod, std::fs::Permissions::from_mode(0o755)).unwrap();
    let out = world
        .command(&["daemon", "start"])
        .env("RENO_RENOD", &renod)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let error = text(&out.stderr);
    assert!(error.contains("renod exited"), "{error}");
    assert!(error.contains("cannot open the store"), "{error}");
    assert!(error.contains("logs"), "{error}");
}

#[test]
fn logs_shows_the_tail_as_text_and_as_json() {
    let world = World::new();
    world.ok(&["daemon", "start"]);
    let shown = world.ok(&["daemon", "logs", "-n", "100"]);
    assert!(shown.contains("renod is serving"), "{shown}");
    assert!(!shown.contains("\"fields\""), "rendered, not raw: {shown}");
    let raw = world.ok(&["--json", "daemon", "logs", "-n", "100"]);
    let line = raw
        .lines()
        .find(|l| l.contains("renod is serving"))
        .unwrap();
    let parsed: Value = serde_json::from_str(line).expect("a JSON line");
    assert_eq!(parsed["level"], "INFO");
    // The file is the rotated log's current file, in the log directory.
    assert!(world.data_dir.join("logs/renod.log").is_file());
}

#[test]
fn logs_follow_prints_new_lines_when_the_file_changes() {
    let world = World::new();
    world.ok(&["daemon", "start"]);
    let mut follow = world
        .command(&["daemon", "logs", "-f", "-n", "0"])
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn logs -f");
    let (lines, received) = mpsc::channel();
    let stdout = follow.stdout.take().unwrap();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if lines.send(line).is_err() {
                return;
            }
        }
    });

    // Wait until the follower is following. It may start after any given write, so append a
    // numbered marker every half second until one comes out: that one was written after the
    // follower began, and so is everything the daemon writes from here on.
    let log = world.data_dir.join("logs/renod.log");
    let mut seen = Vec::new();
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    let mut n = 0;
    let ready = 'wait: loop {
        if std::time::Instant::now() > deadline {
            break false;
        }
        n += 1;
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
            writeln!(
                file,
                "{{\"level\":\"INFO\",\"fields\":{{\"message\":\"follow-test marker {n}\"}}}}"
            )
            .unwrap();
        }
        while let Ok(line) = received.recv_timeout(Duration::from_millis(500)) {
            let done = line.contains("follow-test marker");
            seen.push(line);
            if done {
                break 'wait true;
            }
        }
    };
    assert!(
        ready,
        "the follower never showed an appended line: {seen:?}"
    );

    // Stopping the daemon logs "shutting down" and "stopped".
    world.ok(&["daemon", "stop"]);
    let found = loop {
        match received.recv_timeout(Duration::from_secs(60)) {
            Ok(line) => {
                let done = line.contains("stopped");
                seen.push(line);
                if done {
                    break true;
                }
            }
            Err(_) => break false,
        }
    };
    follow.kill().ok();
    follow.wait().ok();
    assert!(found, "no new lines arrived: {seen:?}");
    assert!(seen.iter().any(|l| l.contains("shutting down")), "{seen:?}");
}

#[test]
fn install_dry_run_prints_the_plist_and_changes_nothing() {
    let world = World::new();
    let said = world.ok(&["daemon", "install", "--dry-run"]);
    assert!(said.contains("<string>dev.reno.renod</string>"), "{said}");
    assert!(said.contains("<key>SuccessfulExit</key>"), "{said}");
    assert!(said.contains("launchctl bootstrap gui/"), "{said}");
    assert!(!world.agents_dir().exists(), "nothing was written");
    assert!(world.launchd_calls().is_empty(), "launchctl was not run");
    let json = world.json(&["daemon", "install", "--dry-run"]);
    assert_eq!(json["dryRun"], true);
    assert_eq!(json["label"], "dev.reno.renod");
}

#[test]
fn install_hands_the_daemon_to_launchd_and_uninstall_takes_it_back() {
    let world = World::new();
    world.ok(&["daemon", "start"]);
    let by_hand = world.pid();

    let said = world.ok(&["daemon", "install"]);
    let plist_path = world.agents_dir().join("dev.reno.renod.plist");
    assert!(
        said.contains(&format!("wrote {}", plist_path.display())),
        "{said}"
    );
    assert!(
        said.contains("<key>KeepAlive</key>"),
        "prints what it wrote: {said}"
    );
    assert!(said.contains("stopped renod (pid "), "{said}");
    assert_eq!(
        std::fs::metadata(&plist_path).unwrap().permissions().mode() & 0o777,
        0o644
    );
    let written = std::fs::read_to_string(&plist_path).unwrap();
    assert!(
        said.contains(written.trim_end()),
        "the printed plist is the written one"
    );
    #[cfg(target_os = "macos")]
    {
        let lint = Command::new("plutil")
            .arg("-lint")
            .arg(&plist_path)
            .output()
            .unwrap();
        assert!(lint.status.success(), "{}", text(&lint.stdout));
    }

    // launchd now owns a different process, and `status` says so.
    let owned = world.pid();
    assert_ne!(owned, by_hand);
    assert!(world.ok(&["daemon", "status"]).contains("launch agent: "));
    let calls = world.launchd_calls();
    assert!(
        calls.iter().any(|c| c.starts_with("bootstrap gui/")),
        "{calls:?}"
    );

    // Stopped by hand, a start goes through launchd and spawns nothing itself.
    world.ok(&["daemon", "stop"]);
    let said = world.ok(&["daemon", "start"]);
    assert!(said.contains("through launchd"), "{said}");
    assert!(
        world
            .launchd_calls()
            .iter()
            .any(|c| c.starts_with("kickstart gui/"))
    );
    assert_eq!(
        world.json(&["daemon", "status"])["launchAgent"],
        plist_path.to_str().unwrap()
    );

    // Reinstalling replaces the loaded job: bootout, then bootstrap.
    world.ok(&["daemon", "install"]);
    let calls = world.launchd_calls();
    let bootouts = calls.iter().filter(|c| c.starts_with("bootout ")).count();
    assert_eq!(bootouts, 1, "{calls:?}");

    let said = world.ok(&["daemon", "uninstall"]);
    assert!(said.contains("launchctl bootout gui/"), "{said}");
    assert!(
        said.contains(&format!("deleted {}", plist_path.display())),
        "{said}"
    );
    assert!(!plist_path.exists());
    let status = world.reno(&["--json", "daemon", "status"]);
    assert_eq!(status.status.code(), Some(1), "stopped by the bootout");
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["running"], false);
    assert!(status.get("launchAgent").is_none());
}

#[test]
fn uninstall_with_nothing_installed_says_so() {
    let world = World::new();
    let said = world.ok(&["daemon", "uninstall"]);
    assert!(said.contains("no LaunchAgent is installed"), "{said}");
    assert!(world.launchd_calls().len() <= 1, "at most a `print`");
}
