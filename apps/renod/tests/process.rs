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
use reno_proto::{ExecEvent, RunId, RunStatus, StopReason, TaskRunParams};

/// A `renod` process that is killed if the test panics before it is stopped.
struct Renod {
    child: Option<Child>,
    socket: PathBuf,
}

impl Renod {
    fn spawn(data_dir: &Path) -> Self {
        let child = Command::new(env!("CARGO_BIN_EXE_renod"))
            .arg("--data-dir")
            .arg(data_dir)
            .env("RENO_MOCK_HOLD", "1")
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
        executor: id("mock"),
        cwd: None,
        model: None,
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
    async fn a_second_renod_on_the_same_data_dir_exits_non_zero_with_the_lock_message() {
        let (_root, dir) = data_dir();
        let mut first = Renod::spawn(&dir);
        first.client().await;

        let second = Command::new(env!("CARGO_BIN_EXE_renod"))
            .arg("--data-dir")
            .arg(&dir)
            .env("RENO_NO_HARNESSES", "1")
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
        daemon.signal("TERM");
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

        // The lock was released and the live run ended `interrupted`, with its journal closed.
        let mut next = Renod::spawn(&dir);
        let client = next.client().await;
        assert_eq!(
            within(client.get_run(&run)).await.expect("run.get").status,
            RunStatus::Interrupted
        );
        let events: Vec<_> = within(client.run_events(run, None).collect::<Vec<_>>()).await;
        let last = events.last().expect("events").as_ref().expect("ok");
        assert!(is_done(&last.event, StopReason::Interrupted));
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

        // The run the crash cut short is interrupted, and its journal ends in a Done.
        assert_eq!(
            within(client.get_run(&run)).await.expect("run.get").status,
            RunStatus::Interrupted
        );
        let events: Vec<_> = within(client.run_events(run.clone(), None).collect::<Vec<_>>()).await;
        let events: Vec<_> = events.into_iter().map(|e| e.expect("event")).collect();
        assert!(matches!(events[0].event, ExecEvent::Started { .. }));
        assert!(is_done(&events.last().unwrap().event, StopReason::Interrupted));
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
            RunStatus::Interrupted
        );
        next.signal("TERM");
        assert!(next.exit().await.success());
    }
}
