//! The real `renod` binary, as a process, each test in its own data directory: the data-dir
//! lock, the socket's mode, a signal, and a crash. No network, no real harness (ARCH-16): the
//! daemon is started with the mock executor held (`RENO_MOCK_HOLD`), so a run stays live until
//! it is cancelled, with no timer involved.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::Duration;

use common::{id, is_done, within};
use futures::StreamExt;
use reno_client::{Client, ClientError};
use reno_proto::{CancelledBy, ExecEvent, RunId, RunStatus, StopDetail, StopReason, TaskRunParams};

/// A `renod` process that is killed if the test panics before it is stopped.
struct Renod {
    child: Option<Child>,
    socket: PathBuf,
}

impl Renod {
    fn spawn(data_dir: &Path) -> Self {
        Self::spawn_with(data_dir, &[])
    }

    fn spawn_with(data_dir: &Path, extra: &[&std::ffi::OsStr]) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_renod"))
            .arg("--data-dir")
            .arg(data_dir)
            .args(extra)
            .env("RENO_MOCK_HOLD", "1")
            // Keep the definitions inside the test's own directory.
            .env("RENO_DEFS_DIR", data_dir.join("defs"))
            // Never so much as probe a real harness CLI.
            .env("RENO_NO_HARNESSES", "1")
            .env_remove("RENO_DATA_DIR")
            .env_remove("RENO_SOCKET")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn renod");
        Self {
            child: Some(child),
            socket: data_dir.join("reno.sock"),
        }
    }

    fn pid(&self) -> u32 {
        self.child.as_ref().expect("running").id()
    }

    /// Connects once the daemon answers. Waiting for a process to come up has no event to follow;
    /// this is the one place a test retries.
    async fn client(&mut self) -> Client {
        within(async {
            loop {
                match Client::connect_unix(&self.socket).await {
                    Ok(client) => return client,
                    Err(ClientError::NotRunning(_)) => {}
                    Err(other) => panic!("connect failed: {other}"),
                }
                let child = self.child.as_mut().expect("running");
                if let Some(status) = child.try_wait().expect("try_wait") {
                    panic!("renod exited before it answered: {status}");
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
    }

    fn signal(&self, name: &str) {
        let status = Command::new("kill")
            .arg(format!("-{name}"))
            .arg(self.pid().to_string())
            .status()
            .expect("kill");
        assert!(status.success());
    }

    /// Waits for the process to exit.
    async fn exit(mut self) -> ExitStatus {
        let mut child = self.child.take().expect("running");
        within(async { tokio::task::spawn_blocking(move || child.wait()).await })
            .await
            .expect("join")
            .expect("wait")
    }

    /// SIGKILL, then reap.
    async fn crash(mut self) -> ExitStatus {
        let mut child = self.child.take().expect("running");
        child.kill().expect("kill -9");
        tokio::task::spawn_blocking(move || child.wait())
            .await
            .expect("join")
            .expect("wait")
    }
}

impl Drop for Renod {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn data_dir() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().expect("tempdir");
    let dir = root.path().join("data");
    (root, dir)
}

/// Submits a run on the held mock and waits until it is live.
async fn start_live_run(client: &Client) -> RunId {
    let run = within(client.run_task(TaskRunParams {
        prompt: "stay alive".into(),
        executor: Some(id("mock")),
        bot: None,
        cwd: None,
        model: None,
        in_place: false,
        attachments: Vec::new(),
    }))
    .await
    .expect("task.run");
    let mut events = Box::pin(client.run_events(run.clone(), None));
    while !matches!(
        within(events.next())
            .await
            .expect("event")
            .expect("ok")
            .event,
        ExecEvent::Text { .. }
    ) {}
    run
}

macro_rules! process_test {
    ($(#[$meta:meta])* async fn $name:ident() $body:block) => {
        $(#[$meta])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() $body
    };
}

process_test! {
    async fn the_real_daemon_creates_the_definitions_directory_on_first_start() {
        let (_root, dir) = data_dir();
        let defs = dir.join("defs");
        assert!(!defs.exists());
        let mut daemon = Renod::spawn(&dir);
        let client = daemon.client().await;

        // `reno-client` answers only after the daemon has loaded what it created.
        assert!(defs.join("bots/example.toml").is_file());
        assert!(defs.join("policy.toml").is_file());
        assert!(defs.join("triggers").is_dir());
        let snapshot = within(client.list_defs(None)).await.expect("def.list");
        assert!(snapshot.errors.is_empty(), "{:?}", snapshot.errors);
        assert_eq!(snapshot.defs.len(), 2);
        // With git installed the directory is a repository; without it the daemon still runs.
        let git_installed = Command::new("git").arg("--version").output().is_ok();
        assert_eq!(defs.join(".git").is_dir(), git_installed);

        daemon.signal("TERM");
        assert!(daemon.exit().await.success());
    }
}

process_test! {
    async fn a_second_renod_on_the_same_data_dir_exits_non_zero_with_the_lock_message() {
        let (_root, dir) = data_dir();
        let mut first = Renod::spawn(&dir);
        first.client().await;

        let second = Command::new(env!("CARGO_BIN_EXE_renod"))
            .arg("--data-dir")
            .arg(&dir)
            .env("RENO_NO_HARNESSES", "1")
            .env("RENO_DEFS_DIR", dir.join("defs"))
            .env_remove("RENO_DATA_DIR")
            .env_remove("RENO_SOCKET")
            .stdin(Stdio::null())
            .output()
            .expect("run the second renod");
        assert!(!second.status.success(), "{:?}", second.status);
        let stderr = String::from_utf8_lossy(&second.stderr);
        assert!(stderr.contains("already running"), "{stderr}");
        assert!(stderr.contains("engine.lock"), "{stderr}");
        assert!(stderr.contains(&first.pid().to_string()), "names the holder: {stderr}");

        // The first daemon was not disturbed.
        let client = first.client().await;
        within(client.status()).await.expect("status");
        first.signal("TERM");
        assert!(first.exit().await.success());
    }
}

process_test! {
    async fn the_socket_is_0600_in_a_0700_directory() {
        let (_root, dir) = data_dir();
        let mut daemon = Renod::spawn(&dir);
        daemon.client().await;
        let mode = |p: &Path| std::fs::metadata(p).expect("stat").permissions().mode() & 0o777;
        assert_eq!(mode(&dir.join("reno.sock")), 0o600);
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join("engine.lock")), 0o600);
        // The log directory exists and holds the JSON log.
        let logs: Vec<_> = std::fs::read_dir(dir.join("logs")).expect("logs").collect();
        assert!(!logs.is_empty(), "a log file is created at start");
        daemon.signal("INT");
        assert!(daemon.exit().await.success());
    }
}

process_test! {
    async fn sigterm_runs_the_shutdown_sequence() {
        let (_root, dir) = data_dir();
        let mut daemon = Renod::spawn(&dir);
        let client = daemon.client().await;
        let run = start_live_run(&client).await;

        daemon.signal("TERM");
        assert!(daemon.exit().await.success(), "a clean exit");
        assert!(!dir.join("reno.sock").exists(), "the socket is removed");
        drop(client);

        // The lock was released and the live run ended `interrupted`, with its journal closed and
        // the shutdown named as the reason. Nobody asked it to stop, so the next daemon resumes it.
        let mut next = Renod::spawn(&dir);
        let client = next.client().await;
        let events: Vec<_> =
            within(client.run_events(run.clone(), None).take(5).collect::<Vec<_>>()).await;
        let events: Vec<_> = events.into_iter().map(|e| e.expect("ok").event).collect();
        assert!(
            matches!(
                &events[2],
                ExecEvent::Done {
                    stop: StopReason::Interrupted,
                    reason: Some(StopDetail::Shutdown),
                    ..
                }
            ),
            "{events:?}"
        );
        assert!(
            matches!(&events[3], ExecEvent::Resumed { from_attempt: 1, .. }),
            "{events:?}"
        );
        next.signal("TERM");
        assert!(next.exit().await.success());
    }
}

/// Waits until `count` text events have been seen in the run's journal.
async fn wait_for_texts(client: &Client, run: &RunId, count: usize) {
    let mut events = Box::pin(client.run_events(run.clone(), None));
    let mut seen = 0;
    while seen < count {
        let item = within(events.next()).await.expect("event").expect("ok");
        if matches!(item.event, ExecEvent::Text { .. }) {
            seen += 1;
        }
    }
}

process_test! {
    async fn a_run_the_daemon_was_killed_with_is_resumed_by_the_next_start() {
        let (_root, dir) = data_dir();
        let mut daemon = Renod::spawn(&dir);
        let client = daemon.client().await;
        let run = start_live_run(&client).await;
        // The session reached the database before the run was seen to be live.
        let summary = within(client.get_run(&run)).await.expect("run.get");
        assert_eq!(summary.session.map(|s| s.0), Some("mock-session".to_owned()));
        drop(client);

        let status = daemon.crash().await;
        assert!(!status.success(), "SIGKILL: {status:?}");

        let mut next = Renod::spawn(&dir);
        let client = next.client().await;
        // The next start found the run `running` with no daemon, interrupted it and, with a
        // session and an executor that resumes, put it back: the held mock is live again.
        wait_for_texts(&client, &run, 2).await;
        let live = within(client.get_run(&run)).await.expect("run.get");
        assert_eq!(live.status, RunStatus::Running);
        assert_eq!(live.attempt, 2);
        assert_eq!(live.stop, None);
        let events: Vec<_> =
            within(client.run_events(run.clone(), None).take(6).collect::<Vec<_>>()).await;
        let events: Vec<_> = events.into_iter().map(|e| e.expect("ok").event).collect();
        assert!(
            matches!(
                &events[2],
                ExecEvent::Done { stop: StopReason::Interrupted, reason: Some(StopDetail::Crashed), .. }
            ),
            "recovery's Done names the crash: {events:?}"
        );
        assert!(
            matches!(&events[3], ExecEvent::Resumed { from_attempt: 1, session } if session.0 == "mock-session"),
            "{events:?}"
        );

        // Let the held run finish so the daemon stops with nothing live.
        within(client.steer(&run, "finish".into())).await.expect("steer");
        let events: Vec<_> = within(client.run_events(run.clone(), None).collect::<Vec<_>>()).await;
        assert!(is_done(&events.last().expect("events").as_ref().expect("ok").event, StopReason::Completed));
        assert_eq!(within(client.get_run(&run)).await.expect("run.get").status, RunStatus::Succeeded);
        next.signal("TERM");
        assert!(next.exit().await.success());
    }
}

process_test! {
    async fn a_run_a_person_cancelled_stays_cancelled_through_a_kill_and_is_not_taken_back() {
        let (_root, dir) = data_dir();
        let mut daemon = Renod::spawn(&dir);
        let client = daemon.client().await;
        let run = start_live_run(&client).await;
        within(client.cancel_run(&run, None)).await.expect("run.cancel");
        let events: Vec<_> = within(client.run_events(run.clone(), None).collect::<Vec<_>>()).await;
        assert!(is_done(&events.last().expect("events").as_ref().expect("ok").event, StopReason::Interrupted));
        drop(client);
        daemon.crash().await;

        let mut next = Renod::spawn(&dir);
        let client = next.client().await;
        // Another start did not take the run back: a person's cancel is final, and the run says
        // who ended it. It was never `interrupted`, so nothing offers it for resuming.
        let summary = within(client.get_run(&run)).await.expect("run.get");
        assert_eq!(summary.status, RunStatus::Cancelled);
        assert_eq!(summary.not_resumed, None);
        assert_eq!(
            summary.stop,
            Some(StopDetail::Cancelled { by: CancelledBy::User })
        );
        assert!(within(client.resume_run(&run, None)).await.is_err());
        next.signal("TERM");
        assert!(next.exit().await.success());
    }
}

/// A stand-in for the Claude Code CLI that starts a grandchild (as an MCP server or a tool would
/// be) and then wedges without reading its input, so that only the daemon's process-group kill
/// can end it. The grandchild's pid is written to `pidfile`.
fn wedged_claude(dir: &Path, pidfile: &Path) -> PathBuf {
    let script = dir.join("claude");
    let text = format!(
        r#"#!/bin/sh
case "$1" in --version) echo "2.1.228 (Claude Code)"; exit 0 ;; esac
read -r first
while :; do
  case "$first" in
    *'"request_id":"reno_initialize"'*)
      printf '%s\n' '{{"type":"control_response","response":{{"subtype":"success","request_id":"reno_initialize","response":{{}}}}}}'
      read -r first || exit 1 ;;
    *) break ;;
  esac
done
sleep 300 </dev/null >/dev/null 2>&1 &
printf '%s' "$!" > '{pidfile}'
printf '%s\n' '{{"type":"system","subtype":"init","model":"claude-fable-5","tools":[],"cwd":"/tmp","session_id":"sess-term"}}'
printf '%s\n' '{{"type":"stream_event","parent_tool_use_id":null,"event":{{"type":"content_block_delta","delta":{{"type":"text_delta","text":"working"}}}}}}'
exec sleep 600
"#,
        pidfile = pidfile.display()
    );
    std::fs::write(&script, text).expect("write the fake claude");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    script
}

fn alive(pid: u32) -> bool {
    use rustix::io::Errno;
    use rustix::process::{Pid, test_kill_process};
    let pid = Pid::from_raw(i32::try_from(pid).expect("pid")).expect("pid");
    !matches!(test_kill_process(pid), Err(Errno::SRCH))
}

process_test! {
    async fn sigterm_leaves_no_agent_process_behind() {
        let (root, dir) = data_dir();
        let pidfile = root.path().join("grandchild.pid");
        let claude = wedged_claude(root.path(), &pidfile);
        let mut daemon = Renod::spawn_with(
            &dir,
            &["--claude-executable".as_ref(), claude.as_os_str()],
        );
        let client = daemon.client().await;
        let run = within(client.run_task(TaskRunParams {
            prompt: "hold".into(),
            executor: Some(id("claude-code")),
            bot: None,
            cwd: None,
            model: None,
            in_place: false,
            attachments: Vec::new(),
        }))
        .await
        .expect("task.run");
        let mut events = Box::pin(client.run_events(run.clone(), None));
        while !matches!(
            within(events.next()).await.expect("event").expect("ok").event,
            ExecEvent::Text { .. }
        ) {}
        let grandchild: u32 = std::fs::read_to_string(&pidfile)
            .expect("the grandchild's pid")
            .trim()
            .parse()
            .expect("a pid");
        assert!(alive(grandchild), "the agent's child is running");
        drop(events);
        drop(client);

        daemon.signal("TERM");
        assert!(daemon.exit().await.success(), "a clean exit");

        // The agent's process group was ended before the daemon let go of the lock.
        assert!(!alive(grandchild), "pid {grandchild} outlived the daemon");
        assert!(!dir.join("reno.sock").exists());
        let mut next = Renod::spawn(&dir);
        let client = next.client().await;
        assert_eq!(
            within(client.get_run(&run)).await.expect("run.get").status,
            RunStatus::Interrupted
        );
        next.signal("TERM");
        assert!(next.exit().await.success());
    }
}

process_test! {
    async fn a_kill_dash_nine_is_recovered_on_the_next_start() {
        let (_root, dir) = data_dir();
        let mut daemon = Renod::spawn(&dir);
        let client = daemon.client().await;
        let run = start_live_run(&client).await;
        drop(client);

        let status = daemon.crash().await;
        assert!(!status.success(), "killed, not a clean exit: {status}");
        assert!(dir.join("reno.sock").exists(), "a crash leaves its socket file behind");

        let mut next = Renod::spawn(&dir);
        let client = next.client().await;

        // The run the crash cut short was interrupted, and recovery closed its journal's first
        // attempt with a Done that names the crash. (It is resumed afterwards: see
        // `a_run_the_daemon_was_killed_with_is_resumed_by_the_next_start`.)
        let events: Vec<_> =
            within(client.run_events(run.clone(), None).take(3).collect::<Vec<_>>()).await;
        let events: Vec<_> = events.into_iter().map(|e| e.expect("event")).collect();
        assert!(matches!(events[0].event, ExecEvent::Started { .. }));
        assert!(matches!(
            events[2].event,
            ExecEvent::Done {
                stop: StopReason::Interrupted,
                reason: Some(StopDetail::Crashed),
                ..
            }
        ));
        for (index, item) in events.iter().enumerate() {
            assert_eq!(item.seq.0, index as u64 + 1, "no hole in the journal");
        }

        // The store is intact: the old run is listed, and a new one starts and can be stopped.
        let listed = within(client.list_runs(reno_proto::RunListParams::default())).await.unwrap();
        assert_eq!(listed.len(), 1);
        let fresh = start_live_run(&client).await;
        within(client.cancel_run(&fresh, None)).await.expect("cancel");
        let tail: Vec<_> = within(client.run_events(fresh.clone(), None).collect::<Vec<_>>()).await;
        let last = tail.last().unwrap().as_ref().unwrap();
        assert!(is_done(&last.event, StopReason::Interrupted));
        assert_eq!(
            within(client.get_run(&fresh)).await.unwrap().status,
            RunStatus::Cancelled
        );
        next.signal("TERM");
        assert!(next.exit().await.success());
    }
}
