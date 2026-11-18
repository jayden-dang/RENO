//! `MuseExecutor` integration tests against the fake `muse serve` in `tests/fixtures/fake-muse.py`
//! (no real `muse` binary, no model, no network, no Muse configuration or credentials, no
//! Keychain involved). The live test against a real Muse is at the bottom, `#[ignore]`d.
//!
//! The fake plays the scenario a prompt names (`scenario:<name>`) with the frame shapes of Muse
//! Code 1.4.23, and appends every argument vector and every frame it receives to `FAKE_MUSE_LOG`,
//! which is how these tests see what the driver sent. See the fake's own header for which shapes
//! were observed on a real 1.4.23 and which come from zeron's recordings of 1.4.3.

#![cfg(unix)]
// Tests unwrap freely, and probe a leaked pid with `kill(pid, 0)`.
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use reno_harness::exec::{Decision, ExecControls, ExecError, ExecHandle, Executor, InputAnswer};
use reno_harness::{MuseExecutor, ProcessRecord};
use reno_proto::{
    AccessMode, ActionClass, ApprovalSubject, Effort, ExecEvent, ExecSpec, InstallStatus,
    McpServerSpec, ModelSource, NotInstalledReason, RunId, SandboxLevel, SessionRef, SignedIn,
    Steering, StopReason, VersionGate,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn fake() -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = fixture_dir().join("fake-muse.py");
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    path
}

fn executor() -> MuseExecutor {
    MuseExecutor::new()
        .with_executable(fake())
        .with_graces(Duration::from_millis(500), Duration::from_millis(300))
        .with_wake_grace(Duration::from_millis(300))
}

/// The fake behind a shell wrapper that sets `vars` in its environment: the driver gives a child
/// only an allowlisted environment, so a probe (which takes no `ExecSpec::env`) is steered this
/// way.
fn wrapped(dir: &Path, vars: &[(&str, &str)]) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let mut script = String::from("#!/bin/sh\n");
    for (key, value) in vars {
        script.push_str(&format!("export {key}='{value}'\n"));
    }
    script.push_str(&format!("exec '{}' \"$@\"\n", fake().display()));
    let path = dir.join("muse-wrapper");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A script written while other tests fork can be "text file busy" (ETXTBSY) for a moment: a
    // forked child holds the write descriptor until it execs. Wait that out here, so it is not
    // taken for a Muse that cannot be run.
    for _ in 0..200 {
        match std::process::Command::new(&path).arg("--version").output() {
            Err(error) if error.raw_os_error() == Some(26) => {
                std::thread::sleep(Duration::from_millis(25));
            }
            _ => break,
        }
    }
    path
}

/// A scratch directory for one test: the fake's log and a working directory.
struct Bench {
    dir: tempfile::TempDir,
}

impl Bench {
    fn new() -> Self {
        Self {
            dir: tempfile::tempdir().unwrap(),
        }
    }

    fn log(&self) -> PathBuf {
        self.dir.path().join("fake-muse.log")
    }

    fn pidfile(&self) -> PathBuf {
        self.dir.path().join("child.pid")
    }

    /// A supervised, unrestricted run in the bench.
    fn spec(&self, prompt: &str) -> ExecSpec {
        ExecSpec {
            run_id: RunId::from_suffix("01HZX9").unwrap(),
            prompt: prompt.into(),
            cwd: self.dir.path().to_path_buf(),
            model: None,
            instructions: None,
            effort: None,
            model_options: Default::default(),
            access: AccessMode::Supervised,
            sandbox: SandboxLevel::Unrestricted,
            resume: None,
            mcp: Vec::new(),
            env: [
                ("FAKE_MUSE_LOG".to_owned(), self.log().display().to_string()),
                (
                    "FAKE_MUSE_PIDFILE".to_owned(),
                    self.pidfile().display().to_string(),
                ),
            ]
            .into(),
            output_schema: None,
            inherit_user_config: false,
        }
    }

    fn logged(&self) -> Vec<Value> {
        std::fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// Every frame the fake received, in order.
    fn frames(&self) -> Vec<Value> {
        self.logged()
            .into_iter()
            .filter_map(|entry| entry.get("frame").cloned())
            .collect()
    }

    /// The methods of those frames.
    fn methods(&self) -> Vec<String> {
        self.frames()
            .iter()
            .filter_map(|f| f["method"].as_str().map(str::to_owned))
            .collect()
    }

    /// The params of the frames that named `method`.
    fn params(&self, method: &str) -> Vec<Value> {
        self.frames()
            .into_iter()
            .filter(|f| f["method"] == method)
            .map(|f| f["params"].clone())
            .collect()
    }

    fn argv(&self) -> Vec<String> {
        self.logged()
            .iter()
            .find_map(|entry| entry.get("argv"))
            .map(|argv| {
                argv.as_array()
                    .unwrap()
                    .iter()
                    .map(|a| a.as_str().unwrap().to_owned())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The environment the fake was started with.
    fn env(&self) -> Value {
        self.logged()
            .iter()
            .find_map(|entry| entry.get("env"))
            .cloned()
            .unwrap_or(Value::Null)
    }
}

/// A started run.
struct Live {
    stream: BoxStream<'static, ExecEvent>,
    handle: ExecHandle,
    records: mpsc::Receiver<ProcessRecord>,
}

async fn start(executor: &MuseExecutor, spec: ExecSpec) -> Live {
    let (controls, handle) = ExecControls::new(8);
    let records = handle.take_process_records().unwrap();
    let stream = executor.start(spec, controls).await.expect("run starts");
    Live {
        stream,
        handle,
        records,
    }
}

/// How the test answers what the run asks.
#[derive(Clone)]
struct Answers {
    decision: Decision,
    message: Option<String>,
    /// The labels given to every question; `None` declines it.
    input: Option<Vec<String>>,
}

impl Default for Answers {
    fn default() -> Self {
        Self {
            decision: Decision::Allow,
            message: None,
            input: Some(vec!["Changed".into()]),
        }
    }
}

async fn drive(live: &mut Live) -> Vec<ExecEvent> {
    drive_with(live, &Answers::default(), |_| {}).await
}

async fn drive_with(
    live: &mut Live,
    answers: &Answers,
    mut on_event: impl FnMut(&ExecEvent),
) -> Vec<ExecEvent> {
    let mut events = Vec::new();
    let run = async {
        while let Some(event) = live.stream.next().await {
            on_event(&event);
            match &event {
                ExecEvent::NeedsInput {
                    request_id,
                    questions,
                } => {
                    let given = answers
                        .input
                        .as_ref()
                        .map(|labels| {
                            questions
                                .iter()
                                .map(|q| InputAnswer {
                                    question_id: q.id.clone(),
                                    labels: labels.clone(),
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    live.handle
                        .answer_input(request_id.clone(), given)
                        .await
                        .unwrap();
                }
                ExecEvent::NeedsApproval { request_id, .. } => {
                    live.handle
                        .decide_with_message(
                            request_id.clone(),
                            answers.decision,
                            answers.message.clone(),
                        )
                        .await
                        .unwrap();
                }
                _ => {}
            }
            events.push(event);
        }
    };
    tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the run ends");
    events
}

async fn run(bench: &Bench, prompt: &str) -> Vec<ExecEvent> {
    let mut live = start(&executor(), bench.spec(prompt)).await;
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

fn approvals(events: &[ExecEvent]) -> Vec<(&str, &ApprovalSubject)> {
    events
        .iter()
        .filter_map(|e| match e {
            ExecEvent::NeedsApproval {
                request_id,
                subject,
            } => Some((request_id.as_str(), subject)),
            _ => None,
        })
        .collect()
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
async fn a_turn_recorded_on_a_real_muse_plays_as_text_and_ends_once() {
    // `echo-1.4.23.ndjson` is the notification stream of a real 1.4.23 over its own echo provider.
    let bench = Bench::new();
    let events = run(&bench, "scenario:echo").await;
    assert!(matches!(
        &events[0],
        ExecEvent::Started { model, session } if model == "default" && !session.0.is_empty()
    ));
    assert_eq!(text(&events), "echo: hello reno");
    // The housekeeping items (reminder children, the prompt itself) show nothing.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, ExecEvent::Tool { .. } | ExecEvent::Reasoning { .. }))
    );
    let (stop, result, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Completed);
    assert_eq!(result.as_deref(), Some("echo: hello reno"));
    assert_eq!(diagnostic, None);
}

#[tokio::test]
async fn a_run_streams_text_tools_and_usage_then_ends_once() {
    let bench = Bench::new();
    let events = run(&bench, "scenario:conf-happy").await;
    assert_eq!(text(&events), "conf-hello");
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
            ("bash", ActionClass::Command),
            ("edit", ActionClass::Edit),
            ("read", ActionClass::Read)
        ]
    );
    assert!(events.iter().any(|e| matches!(
        e,
        ExecEvent::ToolResult { id, ok: true, output: Some(o), .. } if id == "t1" && o.text == "hi"
    )));
    // The prompt counted once (1000), less the cache's 800 reads.
    assert!(events.contains(&ExecEvent::Usage {
        input_tokens: 200,
        output_tokens: 40,
        cache_read_tokens: 800,
        cache_write_tokens: 0,
    }));
    assert!(events.contains(&ExecEvent::ContextUsage {
        tokens: Some(1040),
        window: Some(128_000),
    }));
    let (stop, result, _) = done(&events);
    assert_eq!(stop, StopReason::Completed);
    assert_eq!(result.as_deref(), Some("conf-hello"));
}

#[tokio::test]
async fn a_run_failing_inside_muse_ends_in_error_with_its_words() {
    let bench = Bench::new();
    let events = run(&bench, "scenario:failed").await;
    let (stop, result, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert_eq!(result, None);
    assert_eq!(diagnostic.as_deref(), Some("monthly quota used up"));
}

#[tokio::test]
async fn the_started_session_is_the_one_the_run_asked_for_and_each_run_gets_its_own() {
    let bench = Bench::new();
    let one = run(&bench, "hello").await;
    let two = run(&bench, "hello").await;
    let session = |events: &[ExecEvent]| match &events[0] {
        ExecEvent::Started { session, .. } => session.clone(),
        other => panic!("{other:?}"),
    };
    assert_ne!(session(&one), session(&two));
    let started = bench.params("session/start");
    assert_eq!(started.len(), 2);
    assert_eq!(session(&one).0, started[0]["sessionId"]);
    // A session id is a UUIDv7, as every command id.
    let id = uuid::Uuid::parse_str(&session(&one).0).unwrap();
    assert_eq!(id.get_version_num(), 7);
}

#[tokio::test]
async fn the_muse_a_run_spawns_is_reported_and_gone_when_the_run_ends() {
    let bench = Bench::new();
    let mut live = start(&executor(), bench.spec("hello")).await;
    let record = live.records.recv().await.expect("a process record");
    assert!(record.pid > 1);
    assert_eq!(record.pgid, record.pid, "its own process group");
    drive(&mut live).await;
    assert!(until_gone(record.pid as i32).await, "muse was left running");
}

#[tokio::test]
async fn what_a_tool_started_goes_with_the_run() {
    let bench = Bench::new();
    let events = run(&bench, "scenario:child").await;
    assert_eq!(done(&events).0, StopReason::Completed);
    let child: i32 = std::fs::read_to_string(bench.pidfile())
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(until_gone(child).await, "a tool's process was left running");
}

// ---------------------------------------------------------------------------------------------
// What is sent to Muse: never a write to its configuration
// ---------------------------------------------------------------------------------------------

/// The methods a run may send, and nothing else.
const ALLOWED: [&str; 12] = [
    "initialize",
    "initialized",
    "account/read",
    "model/list",
    "session/start",
    "session/resume",
    "session/setApprovalMode",
    "turn/start",
    "turn/interrupt",
    "approval/decide",
    "userInput/answer",
    "userInput/cancel",
];

/// The 1.4.23 methods that write Muse's settings, credentials, trust, plugins or skills, or that
/// the schema calls durable without saying where.
const CONFIG_WRITERS: [&str; 23] = [
    "account/loginStart",
    "account/loginCancel",
    "account/logout",
    "settings/writeMany",
    "settings/writeValue",
    "mcpServer/oauthLogin",
    "mcpServer/oauthLoginCancel",
    "mcpServer/oauthLogout",
    "skill/setActivation",
    "skill/import",
    "plugin/install",
    "plugin/marketplaceAdd",
    "plugin/marketplaceRemove",
    "plugin/marketplaceUpdate",
    "computerUseSettings/update",
    "computerUseSetup/start",
    "feedback/submit",
    "usage/refresh",
    "session/setModel",
    "session/setPermissionProfile",
    "session/setReasoningEffort",
    "session/rename",
    "host/shutdown",
];

fn assert_only_allowed(bench: &Bench) {
    assert!(
        bench.logged().iter().all(|e| e.get("refused").is_none()),
        "the fake refused a frame that writes Muse's config"
    );
    for method in bench.methods() {
        assert!(!CONFIG_WRITERS.contains(&method.as_str()), "{method}");
        assert!(
            ALLOWED.contains(&method.as_str()),
            "{method} is not a method a run may send"
        );
    }
}

#[tokio::test]
async fn no_run_ever_sends_a_method_that_writes_musess_configuration() {
    // Every kind of run the fake can play: the logs of all of them hold only allowed methods.
    let bench = Bench::new();
    for scenario in [
        "scenario:conf-happy",
        "scenario:approval",
        "scenario:staged",
        "scenario:question",
        "scenario:failed",
        "scenario:subagent",
        "scenario:idle-steer",
    ] {
        let mut spec = bench.spec(scenario);
        spec.effort = Some(Effort::High);
        spec.model = Some("muse-spark-1.3".into());
        spec.instructions = Some("be brief".into());
        spec.mcp = vec![server("reno")];
        let mut live = start(&executor(), spec).await;
        let mut sent = false;
        let handle = live.handle.clone();
        drive_with(&mut live, &Answers::default(), |event| {
            if matches!(event, ExecEvent::Text { .. }) && !sent {
                sent = true;
                let _ = handle.try_steer("a steering message");
            }
        })
        .await;
    }
    // A cancelled run, too.
    let mut live = start(&executor(), bench.spec("scenario:interrupt")).await;
    let handle = live.handle.clone();
    drive_with(&mut live, &Answers::default(), |event| {
        if matches!(event, ExecEvent::Started { .. }) {
            handle.cancel();
        }
    })
    .await;
    assert_only_allowed(&bench);
    assert!(bench.methods().len() > 20, "the runs sent something");
}

#[tokio::test]
async fn the_fake_fails_a_run_that_sends_a_config_writer() {
    // The guard of the guard: were the driver to send one, the fake would end the run loudly. The
    // writer refuses it first, so this is shown by sending to the fake directly.
    let bench = Bench::new();
    let output = std::process::Command::new(fake())
        .arg("serve")
        .env("FAKE_MUSE_LOG", bench.log())
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .and_then(|mut child| {
            use std::io::Write;
            let frame = json!({"jsonrpc": "2.0", "id": 1, "method": "settings/writeValue",
                               "params": {"key": "x", "value": 1}});
            writeln!(child.stdin.take().unwrap(), "{frame}")?;
            child.wait_with_output()
        })
        .unwrap();
    assert_eq!(output.status.code(), Some(99));
    assert!(
        bench
            .logged()
            .iter()
            .any(|e| e["refused"] == "settings/writeValue")
    );
}

#[tokio::test]
async fn the_command_line_is_the_pinned_host_and_trusts_nothing() {
    let bench = Bench::new();
    run(&bench, "hello").await;
    assert_eq!(bench.argv(), ["serve", "--provider", "meta"]);
    for refused in [
        "--trust-workspace",
        "--disable-sandbox",
        "--listen",
        "--join",
    ] {
        assert!(!bench.argv().iter().any(|a| a == refused), "{refused}");
    }
}

#[tokio::test]
async fn the_childs_environment_stops_the_launchers_update_and_carries_only_what_was_allowed() {
    let bench = Bench::new();
    let mut spec = bench.spec("hello");
    // A bot's own variables arrive, but cannot turn the launcher's self-update back on.
    spec.env.insert("MUSE_NO_AUTO_UPDATE".into(), "0".into());
    spec.env.insert("BOT_SECRET".into(), "s3".into());
    let mut live = start(&executor(), spec).await;
    drive(&mut live).await;
    let env = bench.env();
    assert_eq!(env["MUSE_NO_AUTO_UPDATE"], "1");
    assert_eq!(env["BOT_SECRET"], "s3");
    // Nothing of the daemon's own that is not on the allowlist.
    for name in [
        "META_API_KEY",
        "MUSE_SYNC_UPDATE",
        "MUSE_LAUNCHER_INSTALL",
        "GITHUB_TOKEN",
        "TBH_CREDENTIAL_BACKEND",
    ] {
        assert!(env.get(name).is_none(), "{name} reached the child");
    }
}

// ---------------------------------------------------------------------------------------------
// The session: posture, model, effort, instructions, servers
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn every_run_asks_for_prompt_unmatched_and_ask_me_in_either_access_mode() {
    for access in [AccessMode::Supervised, AccessMode::FullAccess] {
        let bench = Bench::new();
        let mut spec = bench.spec("hello");
        spec.access = access;
        let mut live = start(&executor(), spec).await;
        drive(&mut live).await;
        let started = &bench.params("session/start")[0];
        assert_eq!(started["approvalMode"], "promptUnmatched", "{access:?}");
        assert_eq!(started["permissionProfile"], ":ask-me", "{access:?}");
        assert_eq!(started["providerId"], "meta");
        // The canonical path of the working directory (macOS /tmp is a link).
        assert_eq!(
            started["workspaceRoot"],
            std::fs::canonicalize(bench.dir.path())
                .unwrap()
                .to_str()
                .unwrap()
        );
        assert!(started.get("modelId").is_none());
        assert!(started.get("config").is_none());
    }
}

#[tokio::test]
async fn a_posture_muse_did_not_report_back_is_refused_before_the_prompt() {
    for (posture, wanted) in [("profile", ":unrestricted"), ("mode", "allowAll")] {
        let bench = Bench::new();
        let mut spec = bench.spec("hello");
        spec.env.insert("FAKE_MUSE_POSTURE".into(), posture.into());
        let mut live = start(&executor(), spec).await;
        let events = drive(&mut live).await;
        let (stop, _, diagnostic) = done(&events);
        assert_eq!(stop, StopReason::Error, "{posture}");
        assert!(diagnostic.unwrap().contains(wanted), "{posture}");
        assert!(
            bench.params("turn/start").is_empty(),
            "no prompt was sent under {posture}"
        );
    }
}

#[tokio::test]
async fn the_model_the_effort_and_the_instructions_ride_the_session_and_the_prompt() {
    let bench = Bench::new();
    let mut spec = bench.spec("do the thing");
    spec.model = Some("muse-spark-1.3".into());
    spec.effort = Some(Effort::XHigh);
    spec.instructions = Some("be brief".into());
    let mut live = start(&executor(), spec).await;
    let events = drive(&mut live).await;
    assert_eq!(done(&events).0, StopReason::Completed);
    assert_eq!(
        bench.params("session/start")[0]["modelId"],
        "muse-spark-1.3"
    );
    let turn = &bench.params("turn/start")[0];
    assert_eq!(turn["reasoningEffort"], "xhigh");
    assert_eq!(turn["ifBusy"], "queue");
    assert_eq!(
        turn["input"][0]["text"],
        "<instructions>\nbe brief\n</instructions>\n\ndo the thing"
    );
    // The model is chosen at the session's start, never with `session/setModel`.
    assert!(!bench.methods().contains(&"session/setModel".to_owned()));
}

#[tokio::test]
async fn the_default_model_is_no_model() {
    let bench = Bench::new();
    let mut spec = bench.spec("hello");
    spec.model = Some("default".into());
    let mut live = start(&executor(), spec).await;
    drive(&mut live).await;
    assert!(bench.params("session/start")[0].get("modelId").is_none());
}

fn server(name: &str) -> McpServerSpec {
    McpServerSpec {
        name: name.into(),
        command: format!("/opt/{name}/server"),
        args: vec!["mcp".into()],
        env: [("RENO_RUN".to_owned(), "r1".to_owned())].into(),
    }
}

#[tokio::test]
async fn reno_and_a_plugins_servers_reach_muse_in_the_session_config_with_no_file() {
    let bench = Bench::new();
    let mut spec = bench.spec("scenario:mcp-config");
    spec.mcp = vec![server("reno"), server("plugin-focus")];
    let mut live = start(&executor(), spec).await;
    let events = drive(&mut live).await;
    assert!(
        text(&events).contains("mcp configured:reno,plugin-focus"),
        "{events:?}"
    );
    let config = &bench.params("session/start")[0]["config"]["mcpServers"];
    assert_eq!(
        config["reno"],
        json!({"transport": "stdio", "command": "/opt/reno/server", "args": ["mcp"],
               "env": {"RENO_RUN": "r1"}, "mode": "optional"})
    );
    // Asked for at the handshake, and nothing was written to the working directory.
    assert_eq!(
        bench.params("initialize")[0]["capabilities"]["requestedCapabilities"],
        json!(["sessionMcp"])
    );
    let left: Vec<_> = std::fs::read_dir(bench.dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .filter(|n| n != "fake-muse.log" && n != "child.pid")
        .collect();
    assert!(left.is_empty(), "{left:?}");
}

#[tokio::test]
async fn a_muse_that_cannot_take_session_servers_fails_a_run_that_needs_them() {
    let bench = Bench::new();
    let mut spec = bench.spec("scenario:mcp-config");
    spec.mcp = vec![server("reno")];
    spec.env.insert("FAKE_MUSE_NO_MCP".into(), "1".into());
    let mut live = start(&executor(), spec).await;
    let events = drive(&mut live).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(diagnostic.unwrap().contains("session MCP servers"));
    assert!(bench.params("session/start").is_empty());
}

#[tokio::test]
async fn a_muse_older_than_the_checked_one_ends_the_run_before_a_session() {
    let bench = Bench::new();
    let mut spec = bench.spec("hello");
    spec.env.insert("FAKE_MUSE_VERSION".into(), "1.4.3".into());
    let mut live = start(&executor(), spec).await;
    let events = drive(&mut live).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(diagnostic.unwrap().contains("too old"));
    assert!(bench.params("session/start").is_empty());
}

// ---------------------------------------------------------------------------------------------
// Resume
// ---------------------------------------------------------------------------------------------

const SESSION: &str = "01a1ee73-5121-74d0-b05d-e674c72aeaf4";

#[tokio::test]
async fn a_resume_loads_the_session_by_id_and_sets_this_runs_approval_mode() {
    let bench = Bench::new();
    let mut spec = bench.spec("scenario:resumed");
    spec.resume = Some(SessionRef(SESSION.into()));
    let mut live = start(&executor(), spec).await;
    let events = drive(&mut live).await;
    assert!(matches!(&events[0], ExecEvent::Started { session, .. } if session.0 == SESSION));
    assert_eq!(text(&events), "back again");
    assert_eq!(done(&events).0, StopReason::Completed);
    let methods = bench.methods();
    let at = |m: &str| methods.iter().position(|x| x == m).unwrap();
    assert!(!methods.contains(&"session/start".to_owned()));
    // The retained mode (the fake reports allowAll, as a real host does) is replaced before the
    // prompt.
    assert!(at("session/resume") < at("session/setApprovalMode"));
    assert!(at("session/setApprovalMode") < at("turn/start"));
    let resume = &bench.params("session/resume")[0];
    assert_eq!(resume["sessionId"], SESSION);
    assert_eq!(resume["permissionProfile"], ":ask-me");
    assert_eq!(resume["excludeItems"], true);
    assert_eq!(
        bench.params("session/setApprovalMode")[0]["mode"],
        "promptUnmatched"
    );
    assert_eq!(bench.params("turn/start")[0]["sessionId"], SESSION);
}

#[tokio::test]
async fn a_session_muse_no_longer_has_starts_a_new_one_and_says_so() {
    let bench = Bench::new();
    let mut spec = bench.spec("scenario:resumed");
    spec.resume = Some(SessionRef(SESSION.into()));
    spec.env.insert("FAKE_MUSE_GONE".into(), SESSION.into());
    let mut live = start(&executor(), spec).await;
    let events = drive(&mut live).await;
    let started = events
        .iter()
        .find_map(|e| match e {
            ExecEvent::Started { session, .. } => Some(session.0.clone()),
            _ => None,
        })
        .unwrap();
    assert_ne!(started, SESSION);
    assert!(text(&events).contains("did not find session"), "{events:?}");
    assert_eq!(done(&events).0, StopReason::Completed);
}

#[tokio::test]
async fn a_session_id_muse_would_refuse_is_refused_before_anything_spawns() {
    let bench = Bench::new();
    let mut spec = bench.spec("hello");
    spec.resume = Some(SessionRef("--help".into()));
    let (controls, _h) = ExecControls::new(4);
    let error = executor().start(spec, controls).await.err().unwrap();
    assert!(matches!(error, ExecError::InvalidSpec(_)));
    assert!(bench.logged().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Approvals
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn every_approval_goes_to_the_kernel_with_its_class_and_gets_a_single_use_answer() {
    let bench = Bench::new();
    let mut live = start(&executor(), bench.spec("scenario:approval")).await;
    let events = drive(&mut live).await;
    let asked = approvals(&events);
    let classes: Vec<_> = asked.iter().map(|(_, s)| s.action).collect();
    assert_eq!(
        classes,
        [
            ActionClass::Command,
            ActionClass::Edit,
            ActionClass::McpTool,
            ActionClass::Network,
            ActionClass::Read,
            ActionClass::Other
        ]
    );
    assert_eq!(asked[0].1.command.as_deref(), Some("git status"));
    assert_eq!(asked[1].1.paths, [PathBuf::from("src/lib.rs")]);
    assert!(asked[0].0.ends_with("#0"));
    // The kernel allowed all six; a kind of request Reno does not read is refused whatever it
    // decided.
    assert_eq!(
        text(&events),
        "bash=allow edit=allow mcp=allow net=allow read=allow \
         other=deny:Reno does not approve a kind of request it does not know."
    );
    assert_eq!(done(&events).0, StopReason::Completed);
    // Only the single-use choices were ever chosen, never the broader ones the host offered.
    let chosen: Vec<_> = bench
        .params("approval/decide")
        .iter()
        .map(|p| p["choiceId"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        chosen,
        [
            "allow_once",
            "allow_once",
            "allow_once",
            "allow_once",
            "allow_once",
            "abort"
        ]
    );
}

#[tokio::test]
async fn a_refusal_is_the_single_use_reject_and_carries_the_reason() {
    let bench = Bench::new();
    let answers = Answers {
        decision: Decision::Deny,
        message: Some("not in this repo".into()),
        input: None,
    };
    let mut live = start(&executor(), bench.spec("scenario:conf-approval-subjects")).await;
    let events = drive_with(&mut live, &answers, |_| {}).await;
    assert_eq!(
        text(&events),
        "bash=deny:not in this repo edit=deny:not in this repo"
    );
    let decided = bench.params("approval/decide");
    assert!(
        decided.iter().all(|p| p["choiceId"] == "abort"),
        "{decided:?}"
    );
}

#[tokio::test]
async fn a_full_access_run_still_asks_the_kernel_which_decides_it() {
    let bench = Bench::new();
    let mut spec = bench.spec("scenario:conf-approval-subjects");
    spec.access = AccessMode::FullAccess;
    let mut live = start(&executor(), spec).await;
    let events = drive(&mut live).await;
    assert_eq!(
        approvals(&events).len(),
        2,
        "the kernel was asked: {events:?}"
    );
    assert_eq!(
        bench.params("session/start")[0]["approvalMode"],
        "promptUnmatched"
    );
}

#[tokio::test]
async fn a_staged_command_raises_one_approval_per_stage() {
    let bench = Bench::new();
    let mut live = start(&executor(), bench.spec("scenario:staged")).await;
    let events = drive(&mut live).await;
    let asked = approvals(&events);
    assert_eq!(asked.len(), 2, "{events:?}");
    assert_ne!(asked[0].0, asked[1].0);
    assert_eq!(asked[0].1.command.as_deref(), Some("mkdir -p out"));
    assert_eq!(asked[1].1.command.as_deref(), Some("touch out/x"));
    assert_eq!(text(&events), "stages: 0/allow_once 1/allow_once");
    let decided = bench.params("approval/decide");
    assert_eq!(decided[0]["requirementId"]["sourceIndex"], 0);
    assert_eq!(decided[1]["requirementId"]["sourceIndex"], 1);
}

#[tokio::test]
async fn an_approval_with_only_broad_grants_on_offer_is_never_allowed_and_ends_the_run() {
    let bench = Bench::new();
    let mut live = start(&executor(), bench.spec("scenario:only-broad")).await;
    let events = drive(&mut live).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(diagnostic.unwrap().contains("no answer"));
    assert!(
        bench.params("approval/decide").is_empty(),
        "nothing broader than once was chosen"
    );
}

#[tokio::test]
async fn approvals_past_the_open_limit_are_refused_not_left_waiting() {
    let bench = Bench::new();
    let mut live = start(&executor(), bench.spec("scenario:flood")).await;
    let mut events = Vec::new();
    let mut open = Vec::new();
    let mut said = String::new();
    let run = async {
        while let Some(event) = live.stream.next().await {
            match &event {
                ExecEvent::NeedsApproval { request_id, .. } => open.push(request_id.clone()),
                ExecEvent::Text { text } => {
                    said.push_str(text);
                    // Nothing is answered until Muse has been told no to what did not fit.
                    if said.contains("flood-over:2") {
                        for id in open.drain(..) {
                            live.handle.decide(id, Decision::Allow).await.unwrap();
                        }
                    }
                }
                _ => {}
            }
            events.push(event);
        }
    };
    tokio::time::timeout(Duration::from_secs(30), run)
        .await
        .expect("the run ends");
    assert_eq!(
        approvals(&events).len(),
        16,
        "sixteen were put to the kernel"
    );
    assert!(
        text(&events).ends_with("allowed:16 refused:abort,abort"),
        "{events:?}"
    );
    assert_eq!(done(&events).0, StopReason::Completed);
}

// ---------------------------------------------------------------------------------------------
// Questions
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_question_reaches_the_person_and_the_answer_goes_back_in_muses_shape() {
    let bench = Bench::new();
    let mut live = start(&executor(), bench.spec("scenario:question")).await;
    let events = drive(&mut live).await;
    let ask = events
        .iter()
        .find_map(|e| match e {
            ExecEvent::NeedsInput { questions, .. } => Some(questions.clone()),
            _ => None,
        })
        .unwrap();
    assert_eq!(ask[0].prompt, "Scope: Which files?");
    assert_eq!(ask[0].options, ["All", "Changed"]);
    assert_eq!(text(&events), "answered:Changed");
    let answered = &bench.params("userInput/answer")[0];
    assert_eq!(answered["answers"][0]["questionId"], "q1");
    assert_eq!(answered["answers"][0]["selectedLabel"], "Changed");
}

#[tokio::test]
async fn a_declined_question_is_muses_own_decline() {
    let bench = Bench::new();
    let answers = Answers {
        input: None,
        ..Answers::default()
    };
    let mut live = start(&executor(), bench.spec("scenario:question")).await;
    let events = drive_with(&mut live, &answers, |_| {}).await;
    assert!(text(&events).starts_with("dismissed:"), "{events:?}");
    assert_eq!(bench.params("userInput/cancel").len(), 1);
    assert!(bench.params("userInput/answer").is_empty());
}

// ---------------------------------------------------------------------------------------------
// Steering and interruption
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_steering_message_goes_as_a_steer_that_never_rebinds_the_workspace() {
    let bench = Bench::new();
    let mut live = start(&executor(), bench.spec("scenario:steer")).await;
    let handle = live.handle.clone();
    let mut sent = false;
    let events = drive_with(&mut live, &Answers::default(), |event| {
        if matches!(event, ExecEvent::Text { .. }) && !sent {
            sent = true;
            handle.try_steer("go left").unwrap();
        }
    })
    .await;
    assert!(text(&events).ends_with("steered:go left"), "{events:?}");
    assert_eq!(done(&events).0, StopReason::Completed);
    let turns = bench.params("turn/start");
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[1]["ifBusy"], "steer");
    assert_eq!(turns[1]["input"][0]["text"], "go left");
    assert!(turns[1].get("workspaceRoots").is_none());
    assert!(turns[0].get("workspaceRoots").is_some());
}

#[tokio::test]
async fn a_steer_that_meets_an_idle_muse_starts_a_turn_the_run_waits_for() {
    let bench = Bench::new();
    let mut live = start(&executor(), bench.spec("scenario:idle-steer")).await;
    let handle = live.handle.clone();
    let mut sent = false;
    let events = drive_with(&mut live, &Answers::default(), |event| {
        if matches!(event, ExecEvent::Text { .. }) && !sent {
            sent = true;
            handle.try_steer("late").unwrap();
        }
    })
    .await;
    assert!(text(&events).ends_with("steered:late"), "{events:?}");
    assert_eq!(done(&events).0, StopReason::Completed);
}

#[tokio::test]
async fn a_steer_muse_refuses_does_not_hang_a_run_that_was_otherwise_over() {
    let bench = Bench::new();
    let mut live = start(&executor(), bench.spec("scenario:steer-refused")).await;
    let handle = live.handle.clone();
    let mut sent = false;
    let events = drive_with(&mut live, &Answers::default(), |event| {
        if matches!(event, ExecEvent::Text { .. }) && !sent {
            sent = true;
            handle.try_steer("too late").unwrap();
        }
    })
    .await;
    assert!(
        text(&events).contains("[Muse did not take the steering message"),
        "{events:?}"
    );
    assert_eq!(done(&events).0, StopReason::Completed);
}

#[tokio::test]
async fn cancel_interrupts_the_turn_and_ends_the_run_interrupted_with_nothing_left_running() {
    let bench = Bench::new();
    let mut live = start(&executor(), bench.spec("scenario:interrupt")).await;
    let record = live.records.recv().await.unwrap();
    let handle = live.handle.clone();
    let mut cancelled = false;
    let events = drive_with(&mut live, &Answers::default(), |event| {
        if matches!(event, ExecEvent::Started { .. }) && !cancelled {
            cancelled = true;
            handle.cancel();
        }
    })
    .await;
    assert_eq!(done(&events).0, StopReason::Interrupted);
    assert!(!bench.params("turn/interrupt").is_empty());
    assert!(until_gone(record.pid as i32).await, "muse was left running");
}

#[tokio::test]
async fn a_muse_that_ignores_the_interrupt_is_ended_anyway() {
    let bench = Bench::new();
    let mut spec = bench.spec("scenario:interrupt");
    spec.env
        .insert("FAKE_MUSE_IGNORE_INTERRUPT".into(), "1".into());
    let mut live = start(&executor(), spec).await;
    let record = live.records.recv().await.unwrap();
    let handle = live.handle.clone();
    let mut cancelled = false;
    let events = drive_with(&mut live, &Answers::default(), |event| {
        if matches!(event, ExecEvent::Text { .. }) && !cancelled {
            cancelled = true;
            handle.cancel();
        }
    })
    .await;
    assert_eq!(done(&events).0, StopReason::Interrupted);
    assert!(until_gone(record.pid as i32).await, "muse was left running");
}

#[tokio::test]
async fn a_cancel_before_the_session_opens_ends_the_run_interrupted() {
    let bench = Bench::new();
    let mut live = start(&executor(), bench.spec("hello")).await;
    live.handle.cancel();
    let events = drive(&mut live).await;
    assert_eq!(done(&events).0, StopReason::Interrupted);
}

// ---------------------------------------------------------------------------------------------
// How a run ends
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_crash_is_an_error_with_the_exit_and_the_last_stderr() {
    let bench = Bench::new();
    let events = run(&bench, "scenario:conf-crash").await;
    let (stop, result, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert_eq!(result, None);
    let diagnostic = diagnostic.unwrap();
    assert!(diagnostic.contains("simulated crash"), "{diagnostic}");
}

#[tokio::test]
async fn a_prompt_muse_refuses_ends_the_run_with_its_words() {
    let bench = Bench::new();
    let events = run(&bench, "scenario:refuse").await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(diagnostic.unwrap().contains("refused by the fake"));
}

#[tokio::test]
async fn background_work_is_waited_for_and_a_childs_own_words_are_not_the_runs() {
    let bench = Bench::new();
    let events = run(&bench, "scenario:subagent").await;
    let words = text(&events);
    assert!(words.ends_with("subagent said Hello"), "{events:?}");
    assert!(!words.contains("child-secret-text"), "{words}");
    assert_eq!(done(&events).0, StopReason::Completed);
    let tools: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            ExecEvent::Tool { call, .. } => Some(call.name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(tools, ["subagent"], "shown once, not again when it ended");
    assert!(events.iter().any(|e| matches!(
        e,
        ExecEvent::ToolResult { ok: true, output: Some(o), .. } if o.text == "Hello"
    )));
    assert_eq!(
        done(&events).1.as_deref(),
        Some("subagent said Hello"),
        "the result is the last turn's"
    );
}

#[tokio::test]
async fn background_work_that_ends_without_a_turn_of_muses_own_ends_the_run_after_a_grace() {
    let bench = Bench::new();
    let events = run(&bench, "scenario:subagent-no-wake").await;
    assert_eq!(done(&events).0, StopReason::Completed);
    assert!(events.iter().any(|e| matches!(
        e,
        ExecEvent::ToolResult { output: Some(o), .. } if o.text == "done"
    )));
}

#[tokio::test]
async fn a_cancel_stops_a_run_that_is_waiting_on_background_work() {
    let bench = Bench::new();
    let mut live = start(&executor(), bench.spec("scenario:subagent-hang")).await;
    let record = live.records.recv().await.unwrap();
    let handle = live.handle.clone();
    let mut cancelled = false;
    let events = drive_with(&mut live, &Answers::default(), |event| {
        if matches!(event, ExecEvent::Text { .. }) && !cancelled {
            cancelled = true;
            let handle = handle.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(400)).await;
                handle.cancel();
            });
        }
    })
    .await;
    assert_eq!(done(&events).0, StopReason::Interrupted);
    assert!(until_gone(record.pid as i32).await, "muse was left running");
}

#[tokio::test]
async fn frames_the_driver_has_never_seen_are_counted_and_the_run_goes_on() {
    let bench = Bench::new();
    let exec = executor();
    let mut live = start(&exec, bench.spec("scenario:conf-unknown")).await;
    let events = drive(&mut live).await;
    assert_eq!(done(&events).0, StopReason::Completed);
    assert_eq!(done(&events).1.as_deref(), Some("unknown-done"));
    let counted = exec.unknown_frames();
    assert_eq!(
        counted.get("method:turn/teleported"),
        Some(&1),
        "{counted:?}"
    );
    assert_eq!(counted.get("item:hologram"), Some(&1), "{counted:?}");
    assert_eq!(counted.len(), 2, "only what the fake made up: {counted:?}");
    // A clean run counts nothing.
    let clean = executor();
    let mut live = start(&clean, bench.spec("scenario:conf-happy")).await;
    drive(&mut live).await;
    assert!(clean.unknown_frames().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Before a run
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn what_muse_cannot_honour_is_refused_before_it_spawns() {
    let bench = Bench::new();
    let mut spec = bench.spec("hello");
    spec.access = AccessMode::AutoAcceptEdits;
    let (controls, _h) = ExecControls::new(4);
    let error = executor().start(spec, controls).await.err().unwrap();
    assert!(matches!(
        error,
        ExecError::UnsupportedAccess {
            requested: AccessMode::AutoAcceptEdits,
            ..
        }
    ));
    for level in [SandboxLevel::ReadOnly, SandboxLevel::WorkspaceWrite] {
        let mut spec = bench.spec("hello");
        spec.sandbox = level;
        let (controls, _h) = ExecControls::new(4);
        let error = executor().start(spec, controls).await.err().unwrap();
        assert!(matches!(error, ExecError::InvalidSpec(why) if why.contains(&level.to_string())));
    }
    let mut spec = bench.spec("hello");
    spec.output_schema = Some(json!({"type": "object"}));
    let (controls, _h) = ExecControls::new(4);
    let error = executor().start(spec, controls).await.err().unwrap();
    assert!(matches!(error, ExecError::UnsupportedOutputSchema));
    assert!(bench.logged().is_empty(), "nothing was spawned");
}

#[tokio::test]
async fn a_missing_muse_is_not_installed() {
    let bench = Bench::new();
    let exec = MuseExecutor::new().with_executable("/nonexistent/muse");
    let (controls, _h) = ExecControls::new(4);
    let error = exec
        .start(bench.spec("hello"), controls)
        .await
        .err()
        .unwrap();
    assert!(matches!(error, ExecError::NotInstalled(_)));
    let probe = exec.probe().await;
    assert!(matches!(
        probe.installed,
        InstallStatus::NotInstalled {
            why: NotInstalledReason::Unusable { .. }
        }
    ));
}

// ---------------------------------------------------------------------------------------------
// Discovery
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_probe_reads_version_sign_in_and_models_without_a_prompt() {
    let bench = Bench::new();
    let wrapper = wrapped(
        bench.dir.path(),
        &[("FAKE_MUSE_LOG", bench.log().to_str().unwrap())],
    );
    let exec = MuseExecutor::new().with_executable(wrapper);
    let probe = exec.probe().await;
    assert!(matches!(
        &probe.installed,
        InstallStatus::Installed { version: Some(v), .. } if v == "1.4.23"
    ));
    assert_eq!(probe.version_gate, VersionGate::Ok);
    assert_eq!(
        probe.signed_in,
        SignedIn::Yes {
            account: Some("fake@example.com".into())
        }
    );
    assert_eq!(probe.models.source, ModelSource::Live);
    let ids: Vec<_> = probe.models.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["muse-spark-1.2", "muse-spark-1.3"]);
    assert_eq!(probe.models.models[0].efforts.len(), 5);
    // The descriptor offers what the probe found, and the version the host reported.
    assert_eq!(exec.descriptor().caps.models.len(), 2);
    assert!(exec.descriptor().adapter_version.contains("CLI 1.4.23"));
    // A read-only host, the handshake and two queries: no session, no prompt, no write.
    assert_eq!(
        bench.methods(),
        ["initialize", "initialized", "account/read", "model/list"]
    );
    assert_only_allowed(&bench);
    assert_eq!(
        bench.argv(),
        [
            "serve",
            "--provider",
            "meta",
            "--disable-shell",
            "--disable-write",
            "--no-session-log"
        ]
    );
    // The version came from the wire: the fake was never asked `--version`, which on a real
    // install is a start of the launcher script.
    assert_eq!(bench.env()["MUSE_NO_AUTO_UPDATE"], "1");
    assert_eq!(
        bench.params("initialize")[0]["capabilities"]["experimentalApi"],
        true
    );
}

#[tokio::test]
async fn a_logged_out_muse_is_signed_out_and_lists_no_models() {
    let bench = Bench::new();
    let wrapper = wrapped(bench.dir.path(), &[("FAKE_MUSE_ACCOUNT", "out")]);
    let probe = MuseExecutor::new().with_executable(wrapper).probe().await;
    assert!(matches!(&probe.signed_in, SignedIn::No { hint } if hint.contains("muse login")));
}

#[tokio::test]
async fn a_host_that_cannot_be_asked_leaves_the_sign_in_unknown() {
    let bench = Bench::new();
    let wrapper = wrapped(bench.dir.path(), &[("FAKE_MUSE_SCHEMA", "9")]);
    let probe = MuseExecutor::new().with_executable(wrapper).probe().await;
    assert_eq!(probe.signed_in, SignedIn::Unknown);
    assert!(matches!(probe.installed, InstallStatus::Installed { .. }));
    assert_eq!(probe.models.source, ModelSource::Curated);
}

#[tokio::test]
async fn the_version_gate_follows_the_checked_range() {
    let bench = Bench::new();
    for (version, expect_old, expect_untested) in [
        ("1.4.3", true, false),
        ("1.4.23", false, false),
        ("1.5.2", false, true),
    ] {
        let wrapper = wrapped(bench.dir.path(), &[("FAKE_MUSE_VERSION", version)]);
        let probe = MuseExecutor::new().with_executable(wrapper).probe().await;
        assert_eq!(
            matches!(probe.version_gate, VersionGate::TooOld { .. }),
            expect_old,
            "{version}"
        );
        assert_eq!(
            matches!(probe.version_gate, VersionGate::Untested { .. }),
            expect_untested,
            "{version}"
        );
    }
}

#[test]
fn the_descriptor_matches_what_the_driver_does() {
    let descriptor = executor().descriptor();
    assert_eq!(descriptor.id.as_str(), "muse");
    assert_eq!(descriptor.cli_verified.min, "1.4.23");
    assert_eq!(descriptor.cli_verified.verified, "1.4.23");
    assert_eq!(descriptor.caps.steering, Steering::StepBoundary);
    assert_eq!(
        descriptor.caps.access_modes,
        [AccessMode::Supervised, AccessMode::FullAccess]
    );
    assert_eq!(descriptor.caps.sandbox_levels, [SandboxLevel::Unrestricted]);
    assert!(!descriptor.caps.isolates_user_config);
    let declared = executor().maintenance().unwrap();
    assert!(declared.updaters.is_empty());
}

// ---------------------------------------------------------------------------------------------
// Live (a real Muse over its own `echo` provider: a mock model that sends nothing anywhere)
// ---------------------------------------------------------------------------------------------

/// Drives a real `muse serve --provider echo` in a throwaway `XDG_CONFIG_HOME`,
/// `XDG_DATA_HOME` and `HOME`, with `MUSE_NO_AUTO_UPDATE=1` and `TBH_CREDENTIAL_BACKEND=file`
/// (a name read off the binary: it is what kept the 1.4.23 probes off the Keychain). The echo
/// provider answers `echo: <prompt>` and calls no tool, so this plays the handshake, the posture
/// check, one whole turn and the teardown, and no approval, question, steer or subagent.
///
/// Opt in with `RENO_MUSE_LIVE=1` and `--ignored`; never run by default:
///
/// ```text
/// RENO_MUSE_LIVE=1 cargo test -p reno-harness --test muse -- --ignored --nocapture
/// ```
#[tokio::test]
#[ignore = "needs Muse Code 1.4.23 on this machine and RENO_MUSE_LIVE=1"]
async fn a_real_muse_plays_a_run_over_its_echo_provider() {
    if std::env::var_os("RENO_MUSE_LIVE").is_none() {
        eprintln!("RENO_MUSE_LIVE is not set: skipped");
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let bench = Bench::new();
    let home = bench.dir.path().join("home");
    let config = bench.dir.path().join("config");
    let data = bench.dir.path().join("data");
    let work = bench.dir.path().join("work");
    for dir in [&home, &config, &data, &work] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let real = reno_harness::muse::resolve_muse_executable().expect("Muse installed");
    let wrapper = bench.dir.path().join("muse-echo");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexport HOME='{}' XDG_CONFIG_HOME='{}' XDG_DATA_HOME='{}'\n\
             export MUSE_NO_AUTO_UPDATE=1 TBH_CREDENTIAL_BACKEND=file\nexec '{}' \"$@\"\n",
            home.display(),
            config.display(),
            data.display(),
            real.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let exec = MuseExecutor::new()
        .with_executable(&wrapper)
        .with_provider("echo");
    let mut spec = bench.spec("hello reno");
    spec.cwd = work.clone();
    spec.env.clear();
    let mut live = start(&exec, spec).await;
    let events = drive(&mut live).await;
    eprintln!("{events:#?}");
    assert_eq!(text(&events), "echo: hello reno");
    let (stop, result, _) = done(&events);
    assert_eq!(stop, StopReason::Completed);
    assert_eq!(result.as_deref(), Some("echo: hello reno"));
    // Muse wrote its sessions and logs under its own data directory, and no settings or
    // credentials file into the config directory.
    assert!(data.join("muse").exists());
    fn files_under(dir: &Path, found: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files_under(&path, found);
            } else {
                found.push(path);
            }
        }
    }
    let mut written = Vec::new();
    files_under(&config, &mut written);
    assert!(
        written.is_empty(),
        "Muse wrote a file into its config dir: {written:?}"
    );
}
