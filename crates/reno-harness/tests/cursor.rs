//! `CursorExecutor` integration tests against the fake shim in `tests/fixtures/fake-cursor-shim.py`
//! (no node, no `@cursor/sdk`, no model, no network, no Cursor directory involved), and one
//! group against the real shim under Node with the fake SDK. The live test against the real
//! SDK is at the bottom, `#[ignore]`d.
//!
//! The fake plays the scenario a prompt names (`scenario:<name>`) with the frames of the shim's
//! protocol, and appends every argument vector, environment fact and frame it receives to
//! `FAKE_CURSOR_LOG`, which is how these tests see what the driver sent. Every test names its
//! own adapters root (and so its own conversation stores) in a temp directory, and its own
//! credentials: none reads the environment's `CURSOR_API_KEY` or the owner's
//! `~/.cursor/sdk/auth.json`, and none can start a browser.

#![cfg(unix)]
// Tests unwrap freely, and probe a leaked pid with `kill(pid, 0)`.
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use reno_harness::exec::{ExecControls, ExecError, ExecHandle, Executor};
use reno_harness::{CursorExecutor, ProcessRecord};
use reno_proto::{
    AccessMode, ActionClass, ExecEvent, ExecSpec, InstallStatus, McpServerSpec, ModelSource,
    NotInstalledReason, RunId, SandboxLevel, SessionRef, SignedIn, Steering, StopReason,
    VersionGate,
};
use serde_json::Value;
use tokio::sync::mpsc;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn fake() -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = fixture_dir().join("fake-cursor-shim.py");
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    path
}

/// Write `text` as an executable and wait out ETXTBSY: a script written while other tests fork
/// can be "text file busy" for a moment, because a forked child holds the write descriptor until
/// it execs. Waiting here keeps it from being taken for a program that cannot be run.
fn write_executable(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    for _ in 0..200 {
        let ran = std::process::Command::new(path)
            .arg("--probe")
            .current_dir(path.parent().unwrap())
            .output();
        match ran {
            Err(error) if error.raw_os_error() == Some(26) => {
                std::thread::sleep(Duration::from_millis(25));
            }
            _ => break,
        }
    }
}

/// The fake behind a shell wrapper that sets `vars` in its environment: the driver gives a child
/// only an allowlisted environment, so a probe (which takes no `ExecSpec::env`) is steered this
/// way.
fn wrapped(dir: &Path, vars: &[(&str, &str)]) -> PathBuf {
    let mut script = String::from("#!/bin/sh\n");
    for (key, value) in vars {
        script.push_str(&format!("export {key}='{value}'\n"));
    }
    script.push_str(&format!("exec '{}' \"$@\"\n", fake().display()));
    let path = dir.join("cursor-wrapper");
    write_executable(&path, &script);
    path
}

/// A scratch directory for one test: the adapters root, the stores beside it, the fake's log.
struct Bench {
    dir: tempfile::TempDir,
}

impl Bench {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("adapters")
    }

    fn state(&self) -> PathBuf {
        self.dir.path().join("cursor-state")
    }

    fn log(&self) -> PathBuf {
        self.dir.path().join("fake-cursor.log")
    }

    /// The fake as the shim, with its own root, credentials taken as present and short graces.
    fn executor(&self) -> CursorExecutor {
        CursorExecutor::new()
            .with_executable(fake())
            .with_adapters_root(self.root())
            .with_graces(Duration::from_millis(500), Duration::from_millis(300))
    }

    /// A full-access, unrestricted run in the bench.
    fn spec(&self, prompt: &str) -> ExecSpec {
        ExecSpec {
            run_id: RunId::from_suffix("01HZX9").unwrap(),
            prompt: prompt.into(),
            cwd: self.dir.path().to_path_buf(),
            model: None,
            instructions: None,
            effort: None,
            model_options: Default::default(),
            access: AccessMode::FullAccess,
            sandbox: SandboxLevel::Unrestricted,
            resume: None,
            mcp: Vec::new(),
            env: [(
                "FAKE_CURSOR_LOG".to_owned(),
                self.log().display().to_string(),
            )]
            .into(),
            output_schema: None,
            inherit_user_config: false,
        }
    }

    /// Every line the fake logged, as JSON.
    fn logged(&self) -> Vec<Value> {
        std::fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// The frames the fake received, in order.
    fn commands(&self) -> Vec<Value> {
        self.logged()
            .into_iter()
            .filter_map(|entry| entry.get("cmd").cloned())
            .collect()
    }

    /// The first frame the fake received: the run.
    fn run_frame(&self) -> Value {
        self.commands()
            .into_iter()
            .find(|c| c["op"] == "run")
            .expect("a run frame")
    }

    /// Whether the fake was started at all.
    fn spawned(&self) -> bool {
        !self.logged().is_empty()
    }
}

/// A started run.
struct Live {
    stream: BoxStream<'static, ExecEvent>,
    handle: ExecHandle,
    records: mpsc::Receiver<ProcessRecord>,
}

async fn start(executor: &CursorExecutor, spec: ExecSpec) -> Live {
    start_with(executor, spec, 8, |_| {}).await
}

/// Start with a steering queue of `capacity`, after `before` had its turn at the handle (steering
/// queued there is waiting when the run starts).
async fn start_with(
    executor: &CursorExecutor,
    spec: ExecSpec,
    capacity: usize,
    before: impl FnOnce(&ExecHandle),
) -> Live {
    let (controls, handle) = ExecControls::new(capacity);
    let records = handle.take_process_records().unwrap();
    before(&handle);
    let stream = executor.start(spec, controls).await.expect("run starts");
    Live {
        stream,
        handle,
        records,
    }
}

async fn drive(live: &mut Live) -> Vec<ExecEvent> {
    drive_with(live, |_| {}).await
}

async fn drive_with(live: &mut Live, mut on_event: impl FnMut(&ExecEvent)) -> Vec<ExecEvent> {
    let mut events = Vec::new();
    let run = async {
        while let Some(event) = live.stream.next().await {
            on_event(&event);
            events.push(event);
        }
    };
    tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the run ends");
    events
}

async fn run(bench: &Bench, prompt: &str) -> Vec<ExecEvent> {
    let mut live = start(&bench.executor(), bench.spec(prompt)).await;
    drive(&mut live).await
}

fn done(events: &[ExecEvent]) -> (StopReason, Option<String>, Option<String>) {
    let dones: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            ExecEvent::Done {
                stop,
                result,
                diagnostic,
                ..
            } => Some((*stop, result.clone(), diagnostic.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(dones.len(), 1, "exactly one Done: {events:?}");
    assert!(
        matches!(events.last(), Some(ExecEvent::Done { .. })),
        "Done is last: {events:?}"
    );
    dones.into_iter().next().unwrap()
}

fn text(events: &[ExecEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            ExecEvent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn session(events: &[ExecEvent]) -> SessionRef {
    events
        .iter()
        .find_map(|e| match e {
            ExecEvent::Started { session, .. } => Some(session.clone()),
            _ => None,
        })
        .expect("Started")
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

async fn until_gone(pid: i32) -> bool {
    for _ in 0..100 {
        if !alive(pid) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

// ---------------------------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_run_streams_text_tools_and_usage_then_ends_once() {
    let bench = Bench::new();
    let events = run(&bench, "scenario:conf-happy").await;
    assert!(matches!(
        &events[0],
        ExecEvent::Started { model, .. } if model == "composer-2.5"
    ));
    assert_eq!(text(&events), "looking around. conf-hello");
    assert!(events.contains(&ExecEvent::Reasoning {
        text: "thinking it over".into()
    }));
    let classes: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            ExecEvent::Tool { call, .. } => Some((call.name.as_str(), call.class)),
            _ => None,
        })
        .collect();
    assert_eq!(
        classes,
        [
            ("shell", ActionClass::Command),
            ("edit", ActionClass::Edit),
            ("read", ActionClass::Read)
        ]
    );
    // Every result answers a call announced before it.
    assert!(events.iter().any(|e| matches!(
        e,
        ExecEvent::ToolResult { id, ok: true, .. } if id == "c1"
    )));
    // The SDK's input count includes the cache: the event's four counts are disjoint.
    assert!(events.contains(&ExecEvent::Usage {
        input_tokens: 12,
        output_tokens: 3,
        cache_read_tokens: 100,
        cache_write_tokens: 5,
    }));
    let (stop, result, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Completed);
    // The result is the last message: the text after the last tool.
    assert_eq!(result.as_deref(), Some("conf-hello"));
    assert_eq!(diagnostic, None);
}

#[tokio::test]
async fn a_subagents_transcript_is_not_the_runs_reply() {
    let bench = Bench::new();
    let events = run(&bench, "scenario:subagent").await;
    assert_eq!(text(&events), "spawning. main answer");
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ExecEvent::Tool { call, .. } if call.name == "grep")),
        "{events:?}"
    );
    // The spawn itself is shown, and its end.
    assert!(events.iter().any(|e| matches!(
        e,
        ExecEvent::Tool { id, call } if id == "task1" && call.name == "task"
    )));
    assert!(events.iter().any(|e| matches!(
        e,
        ExecEvent::ToolResult { id, ok: true, .. } if id == "task1"
    )));
    let (stop, result, _) = done(&events);
    assert_eq!(stop, StopReason::Completed);
    assert_eq!(result.as_deref(), Some("main answer"));
    assert!(bench.executor().unknown_frames().is_empty());
}

#[tokio::test]
async fn the_started_session_is_the_agent_the_shim_reports_and_each_run_gets_its_own() {
    let bench = Bench::new();
    let one = run(&bench, "hello").await;
    let two = run(&bench, "hello").await;
    assert_ne!(session(&one), session(&two));
    assert!(session(&one).0.starts_with("agent-"));
    assert_eq!(text(&one), "reply:hello");
}

#[tokio::test]
async fn a_run_keeps_its_conversation_in_a_private_store_under_renos_state_and_resumes_it() {
    use std::os::unix::fs::PermissionsExt;
    let bench = Bench::new();
    let exec = bench.executor();
    let first = drive(&mut start(&exec, bench.spec("hello")).await).await;
    let agent = session(&first);
    // The stores are Reno's own, beside the adapters.
    let store_dir = bench.run_frame()["storeDir"].as_str().unwrap().to_owned();
    let store = Path::new(&store_dir);
    assert!(store.starts_with(bench.state().join("agents")), "{store:?}");
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(store), 0o700);
    assert_eq!(
        std::fs::read_to_string(bench.state().join("by-agent").join(&agent.0)).unwrap(),
        store_dir
    );
    // The fake was told where the stores live, never given a default under the home.
    let logged = bench.logged();
    assert_eq!(
        logged[0]["state_dir"].as_str().map(PathBuf::from),
        Some(bench.state())
    );

    let mut spec = bench.spec("scenario:resumed");
    spec.resume = Some(agent.clone());
    let resumed = drive(&mut start(&exec, spec).await).await;
    assert_eq!(session(&resumed), agent);
    assert_eq!(text(&resumed), "back again");
    assert_eq!(done(&resumed).0, StopReason::Completed);
    let frames = bench.commands();
    let runs: Vec<_> = frames.iter().filter(|c| c["op"] == "run").collect();
    assert_eq!(runs[1]["resume"], agent.0);
    assert_eq!(
        runs[1]["storeDir"], store_dir,
        "a resume opens the conversation's own store"
    );
}

#[tokio::test]
async fn a_conversation_reno_does_not_hold_ends_the_run_with_the_reason_and_spawns_nothing() {
    let bench = Bench::new();
    let mut spec = bench.spec("hello");
    spec.resume = Some(SessionRef("agent-not-ours".into()));
    let events = drive(&mut start(&bench.executor(), spec).await).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(
        diagnostic.unwrap().contains("cannot be resumed"),
        "{events:?}"
    );
    assert!(!bench.spawned());
}

#[tokio::test]
async fn an_agent_id_that_could_leave_the_store_directory_is_refused_before_anything_spawns() {
    let bench = Bench::new();
    for bad in ["../x", "a/b", "", ".hidden", "a b"] {
        let mut spec = bench.spec("hello");
        spec.resume = Some(SessionRef(bad.into()));
        let (controls, _handle) = ExecControls::new(8);
        let refused = bench.executor().start(spec, controls).await;
        assert!(matches!(refused, Err(ExecError::InvalidSpec(_))), "{bad:?}");
    }
    assert!(!bench.spawned());
    assert!(!bench.state().exists());
}

#[tokio::test]
async fn the_shim_a_run_spawns_is_reported_and_gone_when_the_run_ends() {
    let bench = Bench::new();
    let mut live = start(&bench.executor(), bench.spec("hello")).await;
    let record = live.records.recv().await.expect("a process record");
    assert!(record.pid > 1);
    assert_eq!(record.pgid, record.pid, "its own process group");
    drive(&mut live).await;
    assert!(
        until_gone(record.pid as i32).await,
        "the shim was left running"
    );
}

#[tokio::test]
async fn what_a_tool_started_goes_with_the_run() {
    let bench = Bench::new();
    let events = run(&bench, "scenario:child").await;
    assert_eq!(done(&events).0, StopReason::Completed);
    let child = bench
        .logged()
        .iter()
        .find_map(|entry| entry["child"].as_i64())
        .unwrap() as i32;
    assert!(until_gone(child).await, "a tool's process was left running");
}

// ---------------------------------------------------------------------------------------------
// What is sent to the shim, and what it is run with
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_run_frame_carries_the_task_and_names_no_login_browser_or_default_store() {
    let bench = Bench::new();
    run(&bench, "do the thing").await;
    let frame = bench.run_frame();
    assert_eq!(frame["prompt"], "do the thing");
    assert_eq!(frame["cwd"], bench.dir.path().to_str().unwrap());
    assert!(frame["model"].is_null() && frame["resume"].is_null());
    assert_eq!(frame["inheritUserConfig"], false);
    assert_eq!(frame["mcpServers"], serde_json::json!([]));
    assert_eq!(frame["silenceTimeoutMs"], 600_000);
    let first = &bench.logged()[0];
    assert!(first["argv"].as_array().unwrap().is_empty());
    assert_eq!(first["no_open_browser"], "1");
    assert_eq!(
        first["cwd"]
            .as_str()
            .map(PathBuf::from)
            .unwrap()
            .canonicalize()
            .unwrap(),
        bench.dir.path().canonicalize().unwrap()
    );
}

#[tokio::test]
async fn the_childs_environment_carries_only_what_was_allowed() {
    let bench = Bench::new();
    let mut spec = bench.spec("hello");
    // A bot's secret for this run, and nothing of the daemon's own Claude session.
    spec.env
        .insert("CURSOR_API_KEY".into(), "key-for-this-bot".into());
    drive(&mut start(&bench.executor(), spec).await).await;
    let first = &bench.logged()[0];
    assert_eq!(first["api_key"], true);
    assert_eq!(first["claudecode"], false);
    assert_eq!(first["no_open_browser"], "1");
    assert!(first["leak"].is_null());
}

#[tokio::test]
async fn instructions_go_in_front_of_the_first_prompt() {
    let bench = Bench::new();
    let mut spec = bench.spec("the task");
    spec.instructions = Some("always answer in French".into());
    drive(&mut start(&bench.executor(), spec).await).await;
    let prompt = bench.run_frame()["prompt"].as_str().unwrap().to_owned();
    let at_instructions = prompt.find("always answer in French").unwrap();
    let at_task = prompt.find("the task").unwrap();
    assert!(at_instructions < at_task, "{prompt}");
}

#[tokio::test]
async fn a_run_that_inherits_user_config_says_so_and_one_that_does_not_never_does() {
    let bench = Bench::new();
    for (inherit, scenario) in [(false, "isolated"), (true, "inherited")] {
        let mut spec = bench.spec("scenario:user-config");
        spec.inherit_user_config = inherit;
        let events = drive(&mut start(&bench.executor(), spec).await).await;
        assert_eq!(text(&events), format!("user-config:{scenario}"));
    }
    let runs: Vec<_> = bench
        .commands()
        .into_iter()
        .filter(|c| c["op"] == "run")
        .map(|c| c["inheritUserConfig"].clone())
        .collect();
    assert_eq!(runs, [false, true]);
}

#[tokio::test]
async fn the_model_and_its_options_reach_the_shim_with_the_catalogs_defaults_filled_in() {
    let bench = Bench::new();
    let exec = bench.executor();
    // The picker's catalog is what the last probe listed.
    let probe = exec.probe().await;
    assert_eq!(probe.models.source, ModelSource::Live);

    let run_with = |model: &str, options: &[(&str, &str)]| {
        let mut spec = bench.spec("hello");
        spec.model = Some(model.into());
        spec.model_options = options
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        spec
    };
    for spec in [
        run_with("auto-smart", &[]),
        run_with("auto-smart", &[("optimize_for", "intelligence")]),
        // A model whose catalog default is a Max Mode variant gets none: the backend can reject
        // the very variant its catalog marks default.
        run_with("grok-4.7", &[]),
        run_with("not-in-the-catalog", &[]),
    ] {
        drive(&mut start(&exec, spec).await).await;
    }
    let sent: Vec<(String, Value)> = bench
        .commands()
        .into_iter()
        .filter(|c| c["op"] == "run")
        .map(|c| {
            (
                c["model"].as_str().unwrap().to_owned(),
                c["modelOptions"].clone(),
            )
        })
        .collect();
    assert_eq!(
        sent,
        [
            (
                "auto-smart".to_owned(),
                serde_json::json!({"optimize_for": "balanced"})
            ),
            (
                "auto-smart".to_owned(),
                serde_json::json!({"optimize_for": "intelligence"})
            ),
            ("grok-4.7".to_owned(), serde_json::json!({})),
            ("not-in-the-catalog".to_owned(), serde_json::json!({})),
        ]
    );
}

#[tokio::test]
async fn servers_reach_the_shim_by_name_with_their_environment() {
    let bench = Bench::new();
    let mut spec = bench.spec("scenario:mcp-config");
    spec.mcp = vec![
        McpServerSpec {
            name: "reno".into(),
            command: "/path with spaces/reno".into(),
            args: vec!["mcp".into()],
            env: [("RENO_RUN".to_owned(), "r1".to_owned())].into(),
        },
        McpServerSpec {
            name: "plugin-focus".into(),
            command: "/opt/focus/server".into(),
            args: Vec::new(),
            env: Default::default(),
        },
    ];
    let events = drive(&mut start(&bench.executor(), spec).await).await;
    assert_eq!(text(&events), "mcp configured:reno,plugin-focus");
    let servers = bench.run_frame()["mcpServers"].clone();
    assert_eq!(servers[0]["command"], "/path with spaces/reno");
    assert_eq!(servers[0]["env"]["RENO_RUN"], "r1");
    assert_eq!(servers[1]["name"], "plugin-focus");
}

// ---------------------------------------------------------------------------------------------
// Steering
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_steering_message_goes_to_the_shim_and_its_turn_ends_the_run() {
    let bench = Bench::new();
    let mut live = start(&bench.executor(), bench.spec("scenario:steer")).await;
    let handle = live.handle.clone();
    let mut sent = false;
    let events = drive_with(&mut live, |event| {
        if matches!(event, ExecEvent::Text { .. }) && !sent {
            sent = true;
            handle.try_steer("go left").unwrap();
        }
    })
    .await;
    assert!(text(&events).ends_with("steered:go left"), "{events:?}");
    assert_eq!(done(&events).0, StopReason::Completed);
    let steers: Vec<_> = bench
        .commands()
        .into_iter()
        .filter(|c| c["op"] == "steer")
        .collect();
    assert_eq!(steers.len(), 1);
    assert_eq!(steers[0]["prompt"], "go left");
}

#[tokio::test]
async fn steering_messages_keep_their_order() {
    let bench = Bench::new();
    let mut live = start(&bench.executor(), bench.spec("scenario:steer-twice")).await;
    let handle = live.handle.clone();
    let mut sent = false;
    let events = drive_with(&mut live, |event| {
        if matches!(event, ExecEvent::Text { .. }) && !sent {
            sent = true;
            handle.try_steer("one").unwrap();
            handle.try_steer("two").unwrap();
        }
    })
    .await;
    assert!(text(&events).ends_with("got:one|two"), "{events:?}");
}

#[tokio::test]
async fn a_turn_that_ends_before_its_steering_was_consumed_does_not_end_the_run() {
    let bench = Bench::new();
    let mut live = start(&bench.executor(), bench.spec("scenario:steer-race")).await;
    let handle = live.handle.clone();
    let mut sent = false;
    let events = drive_with(&mut live, |event| {
        if matches!(event, ExecEvent::Text { .. }) && !sent {
            sent = true;
            handle.try_steer("late").unwrap();
        }
    })
    .await;
    // The shim ended its first turn, then ran the input as a follow-up of the same run.
    assert_eq!(text(&events), "firstlate-run:late", "{events:?}");
    let (stop, result, _) = done(&events);
    assert_eq!(stop, StopReason::Completed);
    assert_eq!(result.as_deref(), Some("late-run:late"));
}

#[tokio::test]
async fn a_burst_of_steering_is_held_back_not_dropped_and_every_turn_runs_in_order() {
    let bench = Bench::new();
    // The shim rejects overlapping SDK turns and echoes the exact order. All 200 are waiting
    // when the run starts: the queue behind the pipe holds 64, so the rest must wait their turn.
    let mut live = start_with(
        &bench.executor(),
        bench.spec("scenario:burst"),
        256,
        |handle| {
            for n in 0..200 {
                handle.try_steer(format!("ITEM-{n}")).unwrap();
            }
        },
    )
    .await;
    let events = drive(&mut live).await;
    let mut texts: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            ExecEvent::Text { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    // The first turn's own text lands whenever it settles; the steering is in order around it.
    let at = texts
        .iter()
        .position(|t| t == "INITIAL")
        .expect("the first turn spoke");
    texts.remove(at);
    assert_eq!(
        texts,
        (0..200).map(|n| format!("ITEM-{n}")).collect::<Vec<_>>()
    );
    assert_eq!(done(&events).0, StopReason::Completed, "{events:?}");
}

// ---------------------------------------------------------------------------------------------
// Ending the run
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn cancel_interrupts_the_shim_and_ends_the_run_with_nothing_left_running() {
    let bench = Bench::new();
    let mut live = start(&bench.executor(), bench.spec("scenario:interrupt")).await;
    let record = live.records.recv().await.unwrap();
    let handle = live.handle.clone();
    let mut cancelled = false;
    let events = drive_with(&mut live, |event| {
        if matches!(event, ExecEvent::Started { .. }) && !cancelled {
            cancelled = true;
            handle.cancel();
        }
    })
    .await;
    assert_eq!(done(&events).0, StopReason::Interrupted);
    assert!(bench.commands().iter().any(|c| c["op"] == "interrupt"));
    assert!(
        until_gone(record.pid as i32).await,
        "the shim was left running"
    );
}

#[tokio::test]
async fn a_fatal_racing_the_cancel_still_ends_the_run_once_and_interrupted() {
    let bench = Bench::new();
    let mut live = start(&bench.executor(), bench.spec("scenario:interrupt-fatal")).await;
    let handle = live.handle.clone();
    let mut cancelled = false;
    let events = drive_with(&mut live, |event| {
        if matches!(event, ExecEvent::Started { .. }) && !cancelled {
            cancelled = true;
            handle.cancel();
        }
    })
    .await;
    assert_eq!(done(&events).0, StopReason::Interrupted);
}

#[tokio::test]
async fn a_shim_that_ignores_the_interrupt_and_sigterm_is_ended_anyway() {
    let bench = Bench::new();
    let mut live = start(&bench.executor(), bench.spec("scenario:hung-cancel")).await;
    let record = live.records.recv().await.unwrap();
    let handle = live.handle.clone();
    let mut cancelled = false;
    let events = drive_with(&mut live, |event| {
        if matches!(event, ExecEvent::Started { .. }) && !cancelled {
            cancelled = true;
            handle.cancel();
        }
    })
    .await;
    assert_eq!(done(&events).0, StopReason::Interrupted);
    assert!(
        until_gone(record.pid as i32).await,
        "the shim was left running"
    );
}

#[tokio::test]
async fn a_crash_is_an_error_with_the_exit_and_the_last_stderr() {
    let bench = Bench::new();
    let events = run(&bench, "scenario:crash").await;
    let (stop, result, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert_eq!(result, None);
    let diagnostic = diagnostic.unwrap();
    assert!(diagnostic.contains("exit code 3"), "{diagnostic}");
    assert!(diagnostic.contains("shim exploded"), "{diagnostic}");
}

#[tokio::test]
async fn a_shim_that_dies_while_a_descendant_holds_its_stdout_open_still_ends_the_run() {
    let bench = Bench::new();
    let events = run(&bench, "scenario:orphan-holds-stdout").await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(diagnostic.unwrap().contains("exited unexpectedly"));
}

#[tokio::test]
async fn a_fatal_and_a_failed_turn_each_end_the_run_with_their_words() {
    let bench = Bench::new();
    let events = run(&bench, "scenario:fatal").await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(diagnostic.unwrap().contains("CURSOR_API_KEY"));
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ExecEvent::Started { .. }))
    );

    let events = run(&bench, "scenario:turn-error").await;
    let (stop, result, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert_eq!(result, None);
    assert_eq!(diagnostic.as_deref(), Some("model exploded"));
}

#[tokio::test]
async fn a_diagnostic_never_carries_a_credential() {
    let bench = Bench::new();
    let events = run(&bench, "scenario:leaky").await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    let diagnostic = diagnostic.unwrap();
    assert!(diagnostic.contains("[REDACTED]"), "{diagnostic}");
    assert!(!diagnostic.contains("secret-token-123"), "{diagnostic}");
}

#[tokio::test]
async fn frames_and_tools_the_driver_has_never_seen_are_counted_and_the_run_goes_on() {
    let bench = Bench::new();
    let exec = bench.executor();
    let mut live = start(&exec, bench.spec("scenario:conf-unknown")).await;
    let events = drive(&mut live).await;
    assert_eq!(done(&events).0, StopReason::Completed);
    assert_eq!(done(&events).1.as_deref(), Some("unknown-done"));
    let counted: Vec<_> = exec.unknown_frames().into_iter().collect();
    assert_eq!(
        counted,
        [
            ("ev:hologram".to_owned(), 1),
            ("tool:teleport".to_owned(), 1)
        ]
    );
}

// ---------------------------------------------------------------------------------------------
// What the driver refuses, and what it never does
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn what_cursor_cannot_honour_is_refused_before_it_spawns() {
    let bench = Bench::new();
    let refused = |spec: ExecSpec| {
        let exec = bench.executor();
        async move {
            let (controls, _handle) = ExecControls::new(8);
            exec.start(spec, controls).await.err()
        }
    };
    for access in [AccessMode::Supervised, AccessMode::AutoAcceptEdits] {
        let mut spec = bench.spec("hello");
        spec.access = access;
        assert!(matches!(
            refused(spec).await,
            Some(ExecError::UnsupportedAccess { .. })
        ));
    }
    let mut spec = bench.spec("hello");
    spec.sandbox = SandboxLevel::WorkspaceWrite;
    assert!(matches!(
        refused(spec).await,
        Some(ExecError::InvalidSpec(_))
    ));
    let mut spec = bench.spec("hello");
    spec.output_schema = Some(serde_json::json!({"type": "object"}));
    assert!(matches!(
        refused(spec).await,
        Some(ExecError::UnsupportedOutputSchema)
    ));
    assert!(!bench.spawned());
}

#[tokio::test]
async fn a_signed_out_cursor_is_refused_before_it_spawns_and_the_message_names_the_variable() {
    let bench = Bench::new();
    let signed_out = bench.executor().with_auth(false, None);
    let (controls, _handle) = ExecControls::new(8);
    let error = signed_out
        .start(bench.spec("hello"), controls)
        .await
        .err()
        .expect("refused");
    match error {
        ExecError::NotSignedIn(why) => assert!(why.contains("CURSOR_API_KEY"), "{why}"),
        other => panic!("{other:?}"),
    }
    assert!(!bench.spawned());

    // A bot's own secret is the key for the run that carries it.
    let mut spec = bench.spec("hello");
    spec.env.insert("CURSOR_API_KEY".into(), "bot-key".into());
    let events = drive(&mut start(&signed_out, spec).await).await;
    assert_eq!(done(&events).0, StopReason::Completed);
}

#[tokio::test]
async fn a_fake_shim_with_no_named_root_cannot_reach_the_owners_directories() {
    let bench = Bench::new();
    let no_root = CursorExecutor::new().with_executable(fake());
    let (controls, _handle) = ExecControls::new(8);
    let refused = no_root.start(bench.spec("hello"), controls).await;
    assert!(matches!(refused, Err(ExecError::InvalidSpec(_))));
    assert!(!bench.spawned());
}

// ---------------------------------------------------------------------------------------------
// Probe and install
// ---------------------------------------------------------------------------------------------

fn executor_at(bench: &Bench, program: PathBuf) -> CursorExecutor {
    CursorExecutor::new()
        .with_executable(program)
        .with_adapters_root(bench.root())
        .with_graces(Duration::from_millis(500), Duration::from_millis(300))
}

fn models_asked(bench: &Bench) -> usize {
    bench
        .logged()
        .iter()
        .filter(|e| e["argv"] == serde_json::json!(["models"]))
        .count()
}

#[tokio::test]
async fn the_probe_reads_the_pin_sign_in_and_models_without_a_prompt() {
    let bench = Bench::new();
    let wrapper = wrapped(
        bench.dir.path(),
        &[("FAKE_CURSOR_LOG", bench.log().to_str().unwrap())],
    );
    let exec = executor_at(&bench, wrapper.clone());
    let probe = exec.probe().await;
    assert_eq!(
        probe.installed,
        InstallStatus::Installed {
            path: wrapper,
            version: Some("1.0.32".into())
        }
    );
    assert_eq!(probe.version_gate, VersionGate::Ok);
    assert_eq!(probe.signed_in, SignedIn::Yes { account: None });
    assert_eq!(probe.models.source, ModelSource::Live);
    // The bare `default` twin of Auto is not offered twice.
    let ids: Vec<_> = probe.models.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["auto-smart", "claude-fable-5", "grok-4.7"]);
    assert_eq!(models_asked(&bench), 1);
    // No run frame, no prompt, nothing written.
    assert!(bench.commands().is_empty());
    assert!(!bench.root().exists() && !bench.state().exists());
    // The descriptor offers what the probe listed.
    assert_eq!(exec.descriptor().caps.models, probe.models.models);
}

#[tokio::test]
async fn a_cursor_with_no_credential_is_signed_out_and_is_never_asked_for_its_models() {
    let bench = Bench::new();
    let wrapper = wrapped(
        bench.dir.path(),
        &[("FAKE_CURSOR_LOG", bench.log().to_str().unwrap())],
    );
    let exec =
        executor_at(&bench, wrapper).with_auth(false, Some(bench.dir.path().join("none.json")));
    let probe = exec.probe().await;
    match &probe.signed_in {
        SignedIn::No { hint } => {
            assert!(hint.contains("CURSOR_API_KEY"), "{hint}");
            assert!(hint.contains("cursor-agent login"), "{hint}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(probe.models.source, ModelSource::Curated);
    assert_eq!(models_asked(&bench), 0, "no network call while signed out");
}

#[tokio::test]
async fn a_stored_sdk_login_signs_in_when_unexpired_is_only_read_and_expired_does_not() {
    use std::os::unix::fs::PermissionsExt;
    let bench = Bench::new();
    let wrapper = wrapped(
        bench.dir.path(),
        &[("FAKE_CURSOR_LOG", bench.log().to_str().unwrap())],
    );
    let auth = bench.dir.path().join("auth.json");
    let write = |expires: u64| {
        std::fs::write(
            &auth,
            format!(r#"{{"version":1,"apiKey":"k","email":"owner@example.test","apiKeyExpiresAtMs":{expires}}}"#),
        )
        .unwrap();
        std::fs::set_permissions(&auth, std::fs::Permissions::from_mode(0o600)).unwrap();
    };
    write(u64::MAX / 2);
    let before = std::fs::read(&auth).unwrap();
    let modified = std::fs::metadata(&auth).unwrap().modified().unwrap();
    let exec = executor_at(&bench, wrapper).with_auth(false, Some(auth.clone()));
    let probe = exec.probe().await;
    assert_eq!(
        probe.signed_in,
        SignedIn::Yes {
            account: Some("owner@example.test".into())
        }
    );
    assert_eq!(models_asked(&bench), 1);
    // Read only: the file is as it was.
    assert_eq!(std::fs::read(&auth).unwrap(), before);
    assert_eq!(
        std::fs::metadata(&auth).unwrap().modified().unwrap(),
        modified
    );

    write(1);
    let probe = exec.probe().await;
    match probe.signed_in {
        SignedIn::No { hint } => assert!(hint.contains("expired"), "{hint}"),
        other => panic!("{other:?}"),
    }
    assert_eq!(
        models_asked(&bench),
        1,
        "no second listing for an expired key"
    );
}

#[tokio::test]
async fn a_credential_cursor_refuses_is_signed_out_and_a_listing_that_failed_is_unknown() {
    let bench = Bench::new();
    for (mode, refused) in [("auth", true), ("net", false), ("none", false)] {
        let wrapper = wrapped(
            bench.dir.path(),
            &[
                ("FAKE_CURSOR_LOG", bench.log().to_str().unwrap()),
                ("FAKE_CURSOR_MODELS", mode),
            ],
        );
        let probe = executor_at(&bench, wrapper).probe().await;
        match (&probe.signed_in, refused) {
            (SignedIn::No { hint }, true) => {
                assert!(hint.contains("rejected"), "{hint}");
                assert!(hint.contains("CURSOR_API_KEY"), "{hint}");
            }
            (SignedIn::Unknown, false) => {}
            (other, _) => panic!("{mode}: {other:?}"),
        }
        assert_eq!(probe.models.source, ModelSource::Curated, "{mode}");
    }
}

#[tokio::test]
async fn a_probe_that_cannot_list_models_keeps_the_last_live_list() {
    let bench = Bench::new();
    // The fake lists models until a flag file exists, then fails the way a dropped network does.
    let flag = bench.dir.path().join("offline");
    let wrapper = bench.dir.path().join("flaky");
    write_executable(
        &wrapper,
        &format!(
            "#!/bin/sh\nexport FAKE_CURSOR_LOG='{}'\n[ -e '{}' ] && export FAKE_CURSOR_MODELS=net\nexec '{}' \"$@\"\n",
            bench.log().display(),
            flag.display(),
            fake().display()
        ),
    );
    let exec = executor_at(&bench, wrapper);
    let live = exec.probe().await;
    assert_eq!(live.models.source, ModelSource::Live);
    std::fs::write(&flag, "").unwrap();
    let offline = exec.probe().await;
    assert_eq!(offline.signed_in, SignedIn::Unknown);
    // What the picker had stays, and says it is not fresh.
    assert_eq!(offline.models.models, live.models.models);
    assert_eq!(models_asked(&bench), 2);
}

#[tokio::test]
async fn a_managed_sdk_that_is_not_installed_is_needs_install_and_a_run_never_installs_it() {
    let bench = Bench::new();
    let exec = CursorExecutor::new()
        .with_adapters_root(bench.root())
        .with_auth(true, None);
    let probe = exec.probe().await;
    match probe.installed {
        InstallStatus::NotInstalled {
            why: NotInstalledReason::NeedsInstall { what },
        } => assert!(what.contains("@cursor/sdk@1.0.32"), "{what}"),
        other => panic!("{other:?}"),
    }
    assert!(!exec.installed());
    let (controls, _handle) = ExecControls::new(8);
    let refused = exec.start(bench.spec("hello"), controls).await;
    assert!(matches!(refused, Err(ExecError::NotInstalled(_))));
    // Neither the probe nor the refused run put anything on disk.
    assert!(!bench.root().exists() && !bench.state().exists());
}

#[tokio::test]
async fn the_descriptor_matches_what_the_driver_does() {
    let bench = Bench::new();
    let exec = bench.executor();
    let descriptor = exec.descriptor();
    assert_eq!(descriptor.id.as_str(), "cursor");
    assert_eq!(descriptor.cli_verified.min, "1.0.32");
    assert_eq!(descriptor.cli_verified.verified, "1.0.32");
    assert_eq!(descriptor.caps.steering, Steering::StepBoundary);
    assert_eq!(descriptor.caps.access_modes, [AccessMode::FullAccess]);
    assert_eq!(descriptor.caps.sandbox_levels, [SandboxLevel::Unrestricted]);
    assert!(descriptor.caps.isolates_user_config && descriptor.caps.resume);
    let declared = exec.maintenance().unwrap();
    assert!(declared.managed && declared.updaters.is_empty());
}

// ---------------------------------------------------------------------------------------------
// The real shim under Node, over a fake SDK, through the managed install's layout
// ---------------------------------------------------------------------------------------------

fn node_installed() -> bool {
    let found = std::process::Command::new("node")
        .arg("--version")
        .output()
        .is_ok();
    if !found {
        eprintln!("node is not installed: the real-shim tests did not run");
    }
    found
}

/// An `npm` that installs the fake SDK where the real one would go, and records its arguments.
fn fake_npm(bench: &Bench) -> (PathBuf, PathBuf) {
    let log = bench.dir.path().join("npm.log");
    let npm = bench.dir.path().join("npm");
    write_executable(
        &npm,
        &format!(
            "#!/bin/sh\necho \"$@\" >> '{log}'\nmkdir -p node_modules/@cursor/sdk\n\
             echo '{{\"type\":\"module\",\"exports\":\"./index.mjs\"}}' > node_modules/@cursor/sdk/package.json\n\
             cp '{sdk}' node_modules/@cursor/sdk/index.mjs\n",
            log = log.display(),
            sdk = fixture_dir().join("fake-cursor-sdk.mjs").display(),
        ),
    );
    // The wait in `write_executable` ran it once with `--probe`.
    std::fs::remove_file(&log).ok();
    (npm, log)
}

#[tokio::test]
async fn the_explicit_install_puts_the_pinned_sdk_and_the_shim_in_renos_adapter_dir() {
    if !node_installed() {
        return;
    }
    let bench = Bench::new();
    let (npm, npm_log) = fake_npm(&bench);
    let exec = CursorExecutor::new()
        .with_adapters_root(bench.root())
        .with_npm(npm)
        .with_auth(true, None)
        .with_graces(Duration::from_millis(500), Duration::from_millis(300));
    assert!(!exec.installed());
    exec.install().await.unwrap();
    assert!(exec.installed());
    let args = std::fs::read_to_string(&npm_log).unwrap();
    assert!(args.contains("--ignore-scripts"), "{args}");
    assert!(args.contains("@cursor/sdk@1.0.32"), "{args}");
    let probe = exec.probe().await;
    match probe.installed {
        InstallStatus::Installed { path, version } => {
            assert_eq!(version.as_deref(), Some("1.0.32"));
            assert!(
                path.starts_with(bench.root().join("cursor__sdk").join("1.0.32")),
                "{path:?}"
            );
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            assert!(
                name.starts_with("reno-cursor-shim-") && name.ends_with(".mjs"),
                "{name}"
            );
        }
        other => panic!("{other:?}"),
    }
    // Idempotent: a second install, which is what a managed update job runs, changes nothing.
    exec.install().await.unwrap();
    assert_eq!(
        std::fs::read_to_string(&npm_log).unwrap().lines().count(),
        1
    );
}

#[tokio::test]
async fn the_real_shim_runs_resumes_and_cancels_over_the_fake_sdk() {
    if !node_installed() {
        return;
    }
    let bench = Bench::new();
    let (npm, _) = fake_npm(&bench);
    let exec = CursorExecutor::new()
        .with_adapters_root(bench.root())
        .with_npm(npm)
        .with_auth(true, None)
        .with_graces(Duration::from_secs(2), Duration::from_secs(1));
    exec.install().await.unwrap();

    let mut spec = bench.spec("normal");
    spec.env.clear();
    let events = drive(&mut start(&exec, spec.clone()).await).await;
    assert_eq!(text(&events), "retained-conversation-history");
    let (stop, result, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Completed, "{diagnostic:?}");
    assert_eq!(result.as_deref(), Some("retained-conversation-history"));
    let agent = session(&events);
    assert_eq!(agent.0, "agent-fixture");

    // The conversation is found again through the index the shim wrote under Reno's state.
    spec.resume = Some(agent.clone());
    let resumed = drive(&mut start(&exec, spec.clone()).await).await;
    assert_eq!(session(&resumed), agent);
    assert_eq!(done(&resumed).0, StopReason::Completed);

    // One the index does not know is refused, not opened in the SDK's own store.
    spec.resume = Some(SessionRef("agent-unknown".into()));
    let refused = drive(&mut start(&exec, spec.clone()).await).await;
    let (stop, _, diagnostic) = done(&refused);
    assert_eq!(stop, StopReason::Error);
    assert!(diagnostic.unwrap().contains("cannot be resumed"));

    // A run that never ends is interrupted, and its process group goes with it.
    spec.resume = None;
    spec.prompt = "hang".into();
    let mut live = start(&exec, spec).await;
    let record = live.records.recv().await.unwrap();
    let handle = live.handle.clone();
    let mut cancelled = false;
    let events = drive_with(&mut live, |event| {
        if matches!(event, ExecEvent::Text { .. }) && !cancelled {
            cancelled = true;
            handle.cancel();
        }
    })
    .await;
    assert_eq!(done(&events).0, StopReason::Interrupted);
    assert!(until_gone(record.pid as i32).await, "node was left running");
    exec.shutdown().await;
}

// ---------------------------------------------------------------------------------------------
// Live (the real SDK; a model call, network and a key)
// ---------------------------------------------------------------------------------------------

/// Installs the pinned SDK into a throwaway adapters root with the real `npm`, then plays one
/// real run. Opt in with `RENO_CURSOR_LIVE=1`, a `CURSOR_API_KEY` in the environment and
/// `--ignored`; never run by default, and not run by the author of this driver (no key was
/// available): it is the check that the SDK pin still speaks what the shim expects. Nothing under
/// `~/.cursor` or Reno's own directories is read or written: the root is a temp dir, and the key
/// comes from the environment only.
#[tokio::test]
#[ignore = "needs node, npm, network, a CURSOR_API_KEY and RENO_CURSOR_LIVE=1; spends a model call"]
async fn a_real_sdk_plays_a_run() {
    if std::env::var_os("RENO_CURSOR_LIVE").is_none()
        || std::env::var_os("CURSOR_API_KEY").is_none_or(|k| k.is_empty())
    {
        eprintln!("RENO_CURSOR_LIVE and CURSOR_API_KEY are not both set: skipped");
        return;
    }
    let bench = Bench::new();
    let exec = CursorExecutor::new()
        .with_adapters_root(bench.root())
        .with_auth(true, None);
    exec.install().await.expect("the pinned SDK installs");
    let probe = exec.probe().await;
    assert!(
        matches!(probe.installed, InstallStatus::Installed { .. }),
        "{probe:?}"
    );
    assert!(matches!(probe.signed_in, SignedIn::Yes { .. }), "{probe:?}");
    assert!(!probe.models.models.is_empty());

    let mut spec = bench
        .spec("Reply with exactly the word PONG and nothing else. Use no tool and read no file.");
    // The key is the environment's, which the driver passes to the child itself.
    spec.env.clear();
    let events = drive(&mut start(&exec, spec).await).await;
    let (stop, result, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Completed, "{diagnostic:?} {events:?}");
    assert!(result.unwrap_or_default().contains("PONG"), "{events:?}");
    assert!(events.iter().any(|e| matches!(e, ExecEvent::Usage { .. })));
    exec.shutdown().await;
}
