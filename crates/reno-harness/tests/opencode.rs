//! `OpencodeExecutor` integration tests against the fake `opencode serve` in
//! `tests/fixtures/fake-opencode.py` (no real `opencode` binary, no model, no network, nothing of
//! opencode's own config or auth involved). The live test against a real opencode is at the
//! bottom, `#[ignore]`d.
//!
//! The fake speaks the 1.x wire (version 1.19.5) or the 2.x wire (2.3.2), whichever the test
//! names; every test that does not need one generation runs both. It plays the scenario a prompt
//! names (`scenario:<name>`) and appends every request it gets, the arguments and the interesting
//! environment it was started with to `FAKE_OPENCODE_LOG`, which is how these tests see what the
//! driver sent.

#![cfg(unix)]
// Tests unwrap freely, and probe a leaked pid with `kill(pid, 0)`.
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]

use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use reno_harness::exec::{Decision, ExecControls, ExecError, ExecHandle, Executor, InputAnswer};
use reno_harness::{OpencodeExecutor, ProcessRecord};
use reno_proto::{
    AccessMode, ActionClass, Effort, ExecEvent, ExecSpec, McpServerSpec, ModelSource, RunId,
    SandboxLevel, SessionRef, SignedIn, Steering, StopReason, VersionGate,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;

const V1: &str = "1.19.5";
const V2: &str = "2.3.2";
const BOTH: [&str; 2] = [V1, V2];

fn fixture_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

fn fake() -> PathBuf {
    fixture_dir().join("fake-opencode.py")
}

/// A scratch directory for one test: the fake's log and sessions, the run's working directory and
/// the wrapper scripts.
struct Bench {
    dir: tempfile::TempDir,
}

impl Bench {
    fn new() -> Self {
        let bench = Self {
            dir: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(bench.work()).unwrap();
        bench
    }

    fn log(&self) -> PathBuf {
        self.dir.path().join("fake-opencode.log")
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("fake-root")
    }

    fn work(&self) -> PathBuf {
        self.dir.path().join("work")
    }

    /// The fake as `version`, behind a shell wrapper that sets its environment: the driver gives
    /// a child only an allowlisted environment, so what the fake needs to be told is exported by
    /// the wrapper itself.
    fn exe(&self, version: &str, vars: &[(&str, &str)]) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let mut script = String::from("#!/bin/sh\n");
        let mut all = vec![
            ("FAKE_OPENCODE_VERSION", version.to_owned()),
            ("FAKE_OPENCODE_LOG", self.log().display().to_string()),
            ("FAKE_OPENCODE_ROOT", self.root().display().to_string()),
        ];
        all.extend(vars.iter().map(|(k, v)| (*k, (*v).to_owned())));
        for (key, value) in all {
            script.push_str(&format!("export {key}='{value}'\n"));
        }
        script.push_str(&format!("exec python3 '{}' \"$@\"\n", fake().display()));
        let name = format!("opencode-{}", version.replace('.', "_"));
        let path = self.dir.path().join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A script written while other tests fork can be "text file busy" (ETXTBSY) for a moment:
        // a forked child holds the write descriptor until it execs. Wait that out here, so it is
        // not taken for an opencode that cannot be run.
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

    fn executor(&self, version: &str, vars: &[(&str, &str)]) -> OpencodeExecutor {
        OpencodeExecutor::new()
            .with_executable(self.exe(version, vars))
            .with_graces(Duration::from_millis(1500), Duration::from_millis(300))
            .with_probe_budget(Duration::from_secs(20))
            .with_startup_timeout(Duration::from_secs(20))
            .with_read_back_budget(Duration::from_millis(1500))
    }

    /// A full-access, unrestricted run in the bench.
    fn spec(&self, prompt: &str) -> ExecSpec {
        ExecSpec {
            run_id: RunId::from_suffix("01HZX9").unwrap(),
            prompt: prompt.into(),
            cwd: self.work(),
            model: None,
            instructions: None,
            effort: None,
            model_options: Default::default(),
            access: AccessMode::FullAccess,
            sandbox: SandboxLevel::Unrestricted,
            resume: None,
            mcp: Vec::new(),
            env: Default::default(),
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

    /// Every request the fake got, in order.
    fn requests(&self) -> Vec<Value> {
        self.logged()
            .into_iter()
            .filter_map(|entry| entry.get("req").cloned())
            .collect()
    }

    /// The requests of `method` whose path contains `part`.
    fn calls(&self, method: &str, part: &str) -> Vec<Value> {
        self.requests()
            .into_iter()
            .filter(|r| r["m"] == method && r["p"].as_str().unwrap().contains(part))
            .collect()
    }

    /// What the fake was started with (the first start).
    fn started(&self) -> Value {
        self.logged()
            .into_iter()
            .find(|entry| entry.get("argv").is_some())
            .expect("the fake was started")
    }

    fn prompts(&self) -> Vec<Value> {
        self.logged()
            .into_iter()
            .filter_map(|entry| entry.get("prompt").cloned())
            .collect()
    }
}

/// A started run.
struct Live {
    stream: BoxStream<'static, ExecEvent>,
    handle: ExecHandle,
    records: mpsc::Receiver<ProcessRecord>,
}

async fn start(executor: &OpencodeExecutor, spec: ExecSpec) -> Live {
    let (controls, handle) = ExecControls::new(8);
    let records = handle.take_process_records().unwrap();
    let stream = executor.start(spec, controls).await.expect("run starts");
    Live {
        stream,
        handle,
        records,
    }
}

/// How a run answers what it is asked.
#[derive(Clone, Copy)]
struct Answers {
    decision: Decision,
}

impl Answers {
    fn allow() -> Self {
        Self {
            decision: Decision::Allow,
        }
    }

    fn deny() -> Self {
        Self {
            decision: Decision::Deny,
        }
    }
}

/// Drive `live` to its end: approvals get `answers.decision`, a question its last option of each.
async fn drive(live: &mut Live, answers: Answers) -> Vec<ExecEvent> {
    drive_with(live, answers, |_| {}).await
}

async fn drive_with(
    live: &mut Live,
    answers: Answers,
    mut on_event: impl FnMut(&ExecEvent),
) -> Vec<ExecEvent> {
    let mut events = Vec::new();
    let run = async {
        while let Some(event) = live.stream.next().await {
            on_event(&event);
            match &event {
                ExecEvent::NeedsApproval { request_id, .. } => {
                    live.handle
                        .decide(request_id.clone(), answers.decision)
                        .await
                        .unwrap();
                }
                ExecEvent::NeedsInput {
                    request_id,
                    questions,
                } => {
                    let given = questions
                        .iter()
                        .map(|q| InputAnswer {
                            question_id: q.id.clone(),
                            labels: if q.multi_select {
                                q.options.clone()
                            } else {
                                vec![q.options.last().cloned().unwrap_or_else(|| "A".into())]
                            },
                        })
                        .collect();
                    live.handle
                        .answer_input(request_id.clone(), given)
                        .await
                        .unwrap();
                }
                _ => {}
            }
            events.push(event);
        }
    };
    tokio::time::timeout(Duration::from_secs(40), run)
        .await
        .expect("the run ends");
    events
}

async fn run(bench: &Bench, version: &str, prompt: &str) -> Vec<ExecEvent> {
    let mut live = start(&bench.executor(version, &[]), bench.spec(prompt)).await;
    drive(&mut live, Answers::allow()).await
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

fn session_of(events: &[ExecEvent]) -> String {
    match &events[0] {
        ExecEvent::Started { session, .. } => session.0.clone(),
        other => panic!("{other:?}"),
    }
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

fn mcp(name: &str) -> McpServerSpec {
    McpServerSpec {
        name: name.into(),
        command: "/opt/reno/reno".into(),
        args: vec!["mcp".into()],
        env: [("RENO_RUN".to_owned(), "r1".to_owned())].into(),
    }
}

// ---------------------------------------------------------------------------------------------
// The run
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_run_streams_text_reasoning_and_usage_then_ends_once() {
    for version in BOTH {
        let bench = Bench::new();
        let events = run(&bench, version, "scenario:text").await;
        assert!(
            matches!(&events[0], ExecEvent::Started { session, model } if session.0.starts_with("ses_") && model == "default"),
            "{version}: {events:?}"
        );
        // A snapshot after deltas sends only what it adds: the text is never doubled.
        assert_eq!(text(&events), "Hello from opencode", "{version}");
        assert!(events.contains(&ExecEvent::Reasoning {
            text: "thinking about it".into()
        }));
        // Four disjoint counts; the output is the reply and the reasoning.
        let usage: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, ExecEvent::Usage { .. }))
            .collect();
        assert_eq!(
            usage,
            [&ExecEvent::Usage {
                input_tokens: 10,
                output_tokens: 25,
                cache_read_tokens: 7,
                cache_write_tokens: 3,
            }],
            "{version}"
        );
        assert!(
            events.contains(&ExecEvent::ContextUsage {
                tokens: Some(40),
                window: Some(200_000),
            }),
            "{version}: {events:?}"
        );
        let (stop, result, diagnostic) = done(&events);
        assert_eq!(stop, StopReason::Completed, "{version}");
        assert_eq!(result.as_deref(), Some("Hello from opencode"));
        assert_eq!(diagnostic, None);
    }
}

#[tokio::test]
async fn a_tool_opens_once_closes_with_its_output_and_an_mcp_tool_is_named_for_the_kernel() {
    for version in BOTH {
        let bench = Bench::new();
        let mut spec = bench.spec("scenario:tools");
        spec.mcp = vec![mcp("reno")];
        let mut live = start(&bench.executor(version, &[]), spec).await;
        let events = drive(&mut live, Answers::allow()).await;
        let tools: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                ExecEvent::Tool { id, call } => Some((id.clone(), call.name.clone(), call.class)),
                _ => None,
            })
            .collect();
        assert_eq!(tools.len(), 2, "{version}: {events:?}");
        assert_eq!(tools[0].1, "bash");
        assert_eq!(tools[0].2, ActionClass::Command);
        assert_eq!(tools[1].1, "mcp__reno__report", "{version}");
        assert_eq!(tools[1].2, ActionClass::McpTool);
        assert!(events.iter().any(|e| matches!(
            e,
            ExecEvent::ToolResult { id, ok: true, output: Some(o), .. } if *id == tools[0].0 && o.text == "a.txt\nb.txt"
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            ExecEvent::ToolResult { id, ok: false, summary: Some(s), .. } if *id == tools[1].0 && s == "boom: no"
        )));
        assert_eq!(done(&events).0, StopReason::Completed);
    }
}

#[tokio::test]
async fn the_server_is_started_the_way_the_driver_promises() {
    for version in BOTH {
        let bench = Bench::new();
        let mut spec = bench.spec("scenario:text");
        spec.env.insert("FAKE_RUN_SECRET".into(), "hunter2".into());
        let mut live = start(&bench.executor(version, &[]), spec).await;
        // The server is reported at once, as a process record the supervisor can sweep.
        let record = tokio::time::timeout(Duration::from_secs(20), live.records.recv())
            .await
            .unwrap()
            .expect("the server is reported");
        assert!(record.pid > 0);
        let events = drive(&mut live, Answers::allow()).await;
        assert_eq!(done(&events).0, StopReason::Completed);
        let started = bench.started();
        let argv: Vec<&str> = started["argv"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a.as_str().unwrap())
            .collect();
        assert_eq!(argv[0], "serve");
        assert_eq!(
            argv[argv.iter().position(|a| *a == "--hostname").unwrap() + 1],
            "127.0.0.1"
        );
        assert_ne!(
            argv[argv.iter().position(|a| *a == "--port").unwrap() + 1],
            "0"
        );
        let env = &started["env"];
        // opencode's own autoupdate is off, by environment.
        assert_eq!(env["OPENCODE_DISABLE_AUTOUPDATE"], "1");
        assert_eq!(env["OPENCODE_SERVER_USERNAME"], "opencode");
        assert_eq!(env["FAKE_RUN_SECRET"], "hunter2");
        // The question tool is switched on for 1.x only, by environment.
        assert_eq!(
            env.get("OPENCODE_ENABLE_QUESTION_TOOL").is_some(),
            version == V1
        );
        // The password is in the environment and nowhere else.
        assert_eq!(started["password_set"], true);
        assert_eq!(started["password_in_argv"], false);
        // A daemon's own variables never reach it.
        assert!(env.get("GITHUB_TOKEN").is_none());
        assert_eq!(
            started["cwd"].as_str().map(|c| c.ends_with("work")),
            Some(true)
        );
        // Every request carried the password, and was scoped to the run's directory.
        let requests = bench.requests();
        assert!(requests.iter().all(|r| r["auth"] == true), "{version}");
        let dir = bench.work().display().to_string();
        assert!(
            bench
                .calls(
                    "POST",
                    if version == V1 {
                        "/session"
                    } else {
                        "/api/session"
                    }
                )
                .iter()
                .any(|r| r["dir"] == dir),
            "{version}"
        );
        // The bus was live before the first prompt: it has no replay.
        assert!(bench.prompts()[0]["subscribers"].as_u64().unwrap() >= 1);
    }
}

#[tokio::test]
async fn nothing_that_writes_opencodes_config_or_auth_is_ever_sent() {
    for version in BOTH {
        for (prompt, access) in [
            ("scenario:permission", AccessMode::Supervised),
            ("scenario:question", AccessMode::FullAccess),
            ("scenario:child", AccessMode::AutoAcceptEdits),
            ("scenario:retry", AccessMode::FullAccess),
        ] {
            let bench = Bench::new();
            let mut spec = bench.spec(prompt);
            spec.access = access;
            spec.mcp = vec![mcp("reno")];
            let mut live = start(&bench.executor(version, &[]), spec).await;
            let events = drive(&mut live, Answers::allow()).await;
            assert_eq!(
                done(&events).0,
                StopReason::Completed,
                "{version} {prompt}: {events:?}"
            );
            for request in bench.requests() {
                let (method, path) = (
                    request["m"].as_str().unwrap(),
                    request["p"].as_str().unwrap(),
                );
                assert!(matches!(method, "GET" | "POST"), "{method} {path}");
                for writer in [
                    "/config",
                    "/auth",
                    "/credential",
                    "/integration",
                    "/global/upgrade",
                    "/oauth",
                    "/permission/saved",
                ] {
                    assert!(!path.contains(writer), "{method} {path}");
                }
                assert!(
                    !(method == "POST" && (path == "/mcp" || path == "/api/mcp")),
                    "{method} {path}"
                );
            }
            // The only config there is rode the environment: no file was named for opencode.
            let env = &bench.started()["env"];
            assert!(env.get("OPENCODE_CONFIG").is_none());
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Access modes: the rules are in the inline config and read back before the prompt
// ---------------------------------------------------------------------------------------------

fn inline_config(bench: &Bench) -> Option<Value> {
    bench.started()["env"]
        .get("OPENCODE_CONFIG_CONTENT")
        .map(|c| serde_json::from_str(c.as_str().unwrap()).unwrap())
}

#[tokio::test]
async fn full_access_adds_no_rules_and_v2_with_nothing_to_say_sets_no_config_at_all() {
    let bench = Bench::new();
    let events = run(&bench, V2, "scenario:text").await;
    assert_eq!(done(&events).0, StopReason::Completed);
    assert!(inline_config(&bench).is_none());
    // No restricted mode, no MCP servers: nothing to read back.
    assert!(bench.calls("GET", "/api/agent/").is_empty());
    let bench = Bench::new();
    run(&bench, V1, "scenario:text").await;
    // 1.x opens the question tool and nothing else.
    assert_eq!(
        inline_config(&bench).unwrap(),
        json!({"permission": {"question": "allow"}})
    );
}

#[tokio::test]
async fn supervised_and_auto_accept_ask_for_commands_and_the_network_and_say_so_to_the_server() {
    for version in BOTH {
        for access in [AccessMode::Supervised, AccessMode::AutoAcceptEdits] {
            let bench = Bench::new();
            let mut spec = bench.spec("scenario:text");
            spec.access = access;
            let mut live = start(&bench.executor(version, &[]), spec).await;
            let events = drive(&mut live, Answers::allow()).await;
            assert_eq!(
                done(&events).0,
                StopReason::Completed,
                "{version} {access:?}"
            );
            let config = inline_config(&bench).unwrap();
            let rules = if version == V1 {
                config["permission"].clone()
            } else {
                config["permissions"].clone()
            };
            let asked = |action: &str| {
                if version == V1 {
                    rules[action] == "ask"
                } else {
                    rules
                        .as_array()
                        .unwrap()
                        .iter()
                        .any(|r| r["action"] == action && r["effect"] == "ask")
                }
            };
            for action in ["bash", "webfetch", "websearch"] {
                assert!(asked(action), "{version} {access:?} {action}");
            }
            assert_eq!(
                asked("edit"),
                access == AccessMode::Supervised,
                "{version} {access:?}"
            );
            // The agent was read back from the server before any session or prompt.
            let order: Vec<String> = bench
                .requests()
                .iter()
                .map(|r| format!("{} {}", r["m"].as_str().unwrap(), r["p"].as_str().unwrap()))
                .collect();
            let read = order
                .iter()
                .position(|r| {
                    r == if version == V1 {
                        "GET /agent"
                    } else {
                        "GET /api/agent/build"
                    }
                })
                .unwrap_or_else(|| panic!("{version}: no read-back in {order:?}"));
            let created = order
                .iter()
                .position(|r| r.starts_with("POST") && r.contains("session"))
                .unwrap();
            assert!(read < created, "{order:?}");
            // And the run names the agent that was read back.
            if version == V1 {
                assert_eq!(
                    bench.calls("POST", "prompt_async")[0]["body"]["agent"],
                    "build"
                );
            }
        }
    }
}

#[tokio::test]
async fn rules_that_are_not_in_force_end_the_run_before_it_prompts() {
    for version in BOTH {
        let bench = Bench::new();
        let mut spec = bench.spec("scenario:text");
        spec.access = AccessMode::Supervised;
        let mut live = start(
            &bench.executor(version, &[("FAKE_OPENCODE_RULES", "ignore")]),
            spec,
        )
        .await;
        let started = std::time::Instant::now();
        let events = drive(&mut live, Answers::allow()).await;
        let (stop, _, diagnostic) = done(&events);
        assert_eq!(stop, StopReason::Error, "{version}");
        assert!(diagnostic.unwrap().contains("not in force"), "{version}");
        assert!(bench.prompts().is_empty(), "no prompt was sent");
        assert!(
            bench.calls("POST", "session").is_empty(),
            "no session was made"
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, ExecEvent::Started { .. }))
        );
        // It waited its budget for them to show, not longer.
        assert!(started.elapsed() < Duration::from_secs(40));
    }
}

#[tokio::test]
async fn rules_that_show_a_moment_after_the_server_answers_are_waited_for() {
    let bench = Bench::new();
    let mut spec = bench.spec("scenario:text");
    spec.access = AccessMode::Supervised;
    let mut live = start(
        &bench.executor(V2, &[("FAKE_OPENCODE_RULES", "late")]),
        spec,
    )
    .await;
    let events = drive(&mut live, Answers::allow()).await;
    assert_eq!(done(&events).0, StopReason::Completed, "{events:?}");
    assert!(bench.calls("GET", "/api/agent/build").len() >= 2);
}

#[tokio::test]
async fn the_runs_mcp_servers_are_registered_in_the_inline_config_and_read_back() {
    for version in BOTH {
        let bench = Bench::new();
        let mut spec = bench.spec("scenario:mcp-config");
        spec.mcp = vec![mcp("reno"), mcp("plugin-focus")];
        let mut live = start(&bench.executor(version, &[]), spec).await;
        let events = drive(&mut live, Answers::allow()).await;
        assert!(
            text(&events).contains("mcp configured:reno,plugin-focus"),
            "{version}: {events:?}"
        );
        let config = inline_config(&bench).unwrap();
        let servers = if version == V1 {
            &config["mcp"]
        } else {
            &config["mcp"]["servers"]
        };
        assert_eq!(servers["reno"]["command"], json!(["/opt/reno/reno", "mcp"]));
        assert_eq!(servers["reno"]["environment"]["RENO_RUN"], "r1");
        assert_eq!(servers["reno"]["type"], "local");
        // 1.x says `enabled`, 2.x `disabled`.
        if version == V1 {
            assert_eq!(servers["reno"]["enabled"], true);
        } else {
            assert_eq!(servers["reno"]["disabled"], false);
        }
        assert!(
            !bench.calls("GET", "mcp").is_empty(),
            "{version}: the registration was read back"
        );
    }
}

#[tokio::test]
async fn a_server_that_did_not_register_the_mcp_server_ends_the_run_before_it_prompts() {
    let bench = Bench::new();
    let mut spec = bench.spec("scenario:text");
    spec.mcp = vec![mcp("reno")];
    let mut live = start(&bench.executor(V1, &[("FAKE_OPENCODE_MCP", "none")]), spec).await;
    let events = drive(&mut live, Answers::allow()).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(
        diagnostic
            .unwrap()
            .contains("did not register the MCP server reno")
    );
    assert!(bench.prompts().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Approvals and questions
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_permission_is_an_approval_answered_once_or_reject_never_always() {
    for version in BOTH {
        for (answers, word, said) in [
            (Answers::allow(), "once", "ran it"),
            (Answers::deny(), "reject", "refused"),
        ] {
            let bench = Bench::new();
            let mut spec = bench.spec("scenario:permission");
            spec.access = AccessMode::Supervised;
            let mut live = start(&bench.executor(version, &[]), spec).await;
            let events = drive(&mut live, answers).await;
            let ask = events
                .iter()
                .find_map(|e| match e {
                    ExecEvent::NeedsApproval {
                        request_id,
                        subject,
                    } => Some((request_id.clone(), subject.clone())),
                    _ => None,
                })
                .unwrap();
            assert_eq!(ask.0, "per_1");
            assert_eq!(ask.1.action, ActionClass::Command, "{version}");
            assert_eq!(ask.1.tool, "bash");
            assert_eq!(ask.1.command.as_deref(), Some("rm -rf build"));
            assert!(text(&events).contains(said), "{version} {word}: {events:?}");
            assert_eq!(done(&events).0, StopReason::Completed);
            let replies = bench.calls("POST", "per_1");
            let body = &replies.last().unwrap()["body"];
            if version == V1 {
                assert_eq!(*body, json!({"reply": word}));
                assert_eq!(replies.last().unwrap()["p"], "/permission/per_1/reply");
            } else {
                assert_eq!(*body, json!({"decision": word}));
            }
            assert!(
                !serde_json::to_string(&bench.requests())
                    .unwrap()
                    .contains("always")
            );
        }
    }
}

#[tokio::test]
async fn a_subagents_permission_is_put_to_the_person_too() {
    for version in BOTH {
        let bench = Bench::new();
        let mut spec = bench.spec("scenario:child");
        spec.access = AccessMode::Supervised;
        let mut live = start(&bench.executor(version, &[]), spec).await;
        let events = drive(&mut live, Answers::allow()).await;
        let class = events.iter().find_map(|e| match e {
            ExecEvent::NeedsApproval { subject, .. } => {
                Some((subject.action, subject.paths.clone()))
            }
            _ => None,
        });
        assert_eq!(
            class,
            Some((ActionClass::Edit, vec![PathBuf::from("/w/a.rs")])),
            "{version}"
        );
        assert_eq!(done(&events).0, StopReason::Completed, "{version}");
        // The reply went to the subagent's session on 2.x, where the route carries it.
        if version == V2 {
            assert!(
                bench.calls("POST", "/permission/per_c/reply")[0]["p"]
                    .as_str()
                    .unwrap()
                    .contains("ses_child")
            );
        }
    }
}

#[tokio::test]
async fn the_question_tool_is_needs_input_and_the_answers_reach_the_server() {
    for version in BOTH {
        let bench = Bench::new();
        let mut live = start(
            &bench.executor(version, &[]),
            bench.spec("scenario:question"),
        )
        .await;
        let events = drive(&mut live, Answers::allow()).await;
        let questions = events
            .iter()
            .find_map(|e| match e {
                ExecEvent::NeedsInput { questions, .. } => Some(questions.clone()),
                _ => None,
            })
            .unwrap();
        assert_eq!(questions.len(), 2);
        assert_eq!(questions[0].prompt, "Which colour?");
        assert_eq!(questions[0].options, ["red", "blue"]);
        assert!(questions[1].multi_select);
        // The first question answered `blue` (the last option), the second with both.
        let said = text(&events);
        if version == V1 {
            assert_eq!(said, r#"you said [["blue"], ["S", "L"]]"#);
            assert_eq!(
                bench.calls("POST", "/question/que_1/reply")[0]["body"],
                json!({"answers": [["blue"], ["S", "L"]]})
            );
        } else {
            assert_eq!(said, r#"you said {"q0": "blue", "q1": ["S", "L"]}"#);
            assert_eq!(
                bench.calls("POST", "/form/que_1/reply")[0]["body"],
                json!({"answer": {"q0": "blue", "q1": ["S", "L"]}})
            );
        }
        assert_eq!(done(&events).0, StopReason::Completed);
    }
}

#[tokio::test]
async fn a_question_that_is_declined_is_rejected_on_1x() {
    let bench = Bench::new();
    let mut live = start(&bench.executor(V1, &[]), bench.spec("scenario:question")).await;
    let mut events = Vec::new();
    while let Some(event) = live.stream.next().await {
        if let ExecEvent::NeedsInput { request_id, .. } = &event {
            live.handle
                .answer_input(request_id.clone(), Vec::new())
                .await
                .unwrap();
        }
        events.push(event);
    }
    assert_eq!(done(&events).0, StopReason::Completed);
    assert_eq!(bench.calls("POST", "/question/que_1/reject").len(), 1);
}

// ---------------------------------------------------------------------------------------------
// How a run ends
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn provider_retries_are_reported_from_the_third_attempt_and_the_run_still_completes() {
    for version in BOTH {
        let bench = Bench::new();
        let events = run(&bench, version, "scenario:retry").await;
        let said = text(&events);
        assert_eq!(
            said.matches("opencode is retrying (attempt 3)").count(),
            1,
            "{version}: {said}"
        );
        assert!(said.contains("recovered"));
        assert_eq!(done(&events).0, StopReason::Completed);
        assert!(
            bench.calls("POST", "abort").is_empty() && bench.calls("POST", "interrupt").is_empty()
        );
    }
}

#[tokio::test]
async fn an_endless_retry_loop_is_aborted_at_the_eighth_attempt_and_ends_in_error() {
    for version in BOTH {
        let bench = Bench::new();
        let events = run(&bench, version, "scenario:retry-forever").await;
        let (stop, _, diagnostic) = done(&events);
        assert_eq!(stop, StopReason::Error, "{version}");
        assert!(
            diagnostic
                .unwrap()
                .contains("kept failing after 8 attempts")
        );
        assert!(text(&events).contains("Giving up after 8 provider retries"));
        assert_eq!(
            bench
                .calls(
                    "POST",
                    if version == V1 {
                        "/abort"
                    } else {
                        "/interrupt"
                    }
                )
                .len(),
            1,
            "{version}"
        );
    }
}

#[tokio::test]
async fn a_session_error_ends_the_run_in_error_unless_output_came_first() {
    for version in BOTH {
        let bench = Bench::new();
        let events = run(&bench, version, "scenario:error").await;
        let (stop, result, diagnostic) = done(&events);
        assert_eq!((stop, result), (StopReason::Error, None), "{version}");
        assert_eq!(diagnostic.as_deref(), Some("invalid api key"), "{version}");
        let bench = Bench::new();
        let events = run(&bench, version, "scenario:error-after-text").await;
        let (stop, result, _) = done(&events);
        assert_eq!(stop, StopReason::Completed, "{version}");
        assert_eq!(result.as_deref(), Some("partial answer"));
    }
}

#[tokio::test]
async fn a_turn_that_fails_at_once_is_seen_because_the_bus_was_live_before_the_prompt() {
    for version in BOTH {
        let bench = Bench::new();
        let events = run(&bench, version, "scenario:fast-fail").await;
        let (stop, _, diagnostic) = done(&events);
        assert_eq!(stop, StopReason::Error);
        assert_eq!(diagnostic.as_deref(), Some("model not found"));
    }
}

#[tokio::test]
async fn a_prompt_the_session_never_answers_hits_the_stall_bound() {
    let bench = Bench::new();
    let exec = bench
        .executor(V1, &[])
        .with_stall_bound(Some(Duration::from_millis(400)));
    let mut live = start(&exec, bench.spec("scenario:silent")).await;
    let events = drive(&mut live, Answers::allow()).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    let diagnostic = diagnostic.unwrap();
    assert!(
        diagnostic.contains("made no progress") && diagnostic.contains("provider"),
        "{diagnostic}"
    );
    assert_eq!(
        bench.calls("POST", "/abort").len(),
        1,
        "the session was aborted"
    );
}

#[tokio::test]
async fn a_rejected_prompt_ends_the_run_with_the_servers_reason() {
    let bench = Bench::new();
    let mut live = start(
        &bench.executor(V1, &[("FAKE_OPENCODE_PROMPT", "400")]),
        bench.spec("scenario:text"),
    )
    .await;
    let events = drive(&mut live, Answers::allow()).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(diagnostic.unwrap().contains("model not found"));
}

#[tokio::test]
async fn a_prompt_whose_answer_was_lost_is_not_posted_twice() {
    let bench = Bench::new();
    let mut live = start(
        &bench.executor(V1, &[("FAKE_OPENCODE_PROMPT", "lose-ack")]),
        bench.spec("scenario:text"),
    )
    .await;
    let events = drive(&mut live, Answers::allow()).await;
    assert_eq!(done(&events).0, StopReason::Completed, "{events:?}");
    assert_eq!(bench.prompts().len(), 1, "the session already had it");
}

#[tokio::test]
async fn cancel_aborts_the_session_ends_interrupted_and_takes_the_server_down() {
    for version in BOTH {
        let bench = Bench::new();
        let mut live = start(&bench.executor(version, &[]), bench.spec("scenario:hang")).await;
        let record = tokio::time::timeout(Duration::from_secs(20), live.records.recv())
            .await
            .unwrap()
            .unwrap();
        let mut events = Vec::new();
        while let Some(event) = live.stream.next().await {
            if matches!(event, ExecEvent::Text { .. }) {
                live.handle.cancel();
            }
            events.push(event);
        }
        let (stop, result, _) = done(&events);
        assert_eq!((stop, result), (StopReason::Interrupted, None), "{version}");
        assert_eq!(
            bench
                .calls(
                    "POST",
                    if version == V1 {
                        "/abort"
                    } else {
                        "/interrupt"
                    }
                )
                .len(),
            1,
            "{version}"
        );
        assert!(until_gone(record.pid).await, "the server outlived the run");
    }
}

#[tokio::test]
async fn an_abort_nobody_acknowledges_still_ends_within_the_grace() {
    let bench = Bench::new();
    let mut live = start(
        &bench.executor(V1, &[]),
        bench.spec("scenario:hang-no-abort-ack"),
    )
    .await;
    let begun = std::time::Instant::now();
    let mut events = Vec::new();
    while let Some(event) = live.stream.next().await {
        if matches!(event, ExecEvent::Text { .. }) {
            live.handle.cancel();
        }
        events.push(event);
    }
    assert_eq!(done(&events).0, StopReason::Interrupted);
    assert!(
        begun.elapsed() < Duration::from_secs(10),
        "{:?}",
        begun.elapsed()
    );
}

#[tokio::test]
async fn a_cancel_before_the_prompt_never_prompts() {
    let bench = Bench::new();
    let (controls, handle) = ExecControls::new(8);
    handle.cancel();
    let stream = bench
        .executor(V1, &[])
        .start(bench.spec("scenario:text"), controls)
        .await
        .unwrap();
    let events: Vec<ExecEvent> = stream.collect().await;
    assert_eq!(done(&events).0, StopReason::Interrupted);
    assert!(bench.prompts().is_empty());
}

#[tokio::test]
async fn a_server_that_dies_mid_turn_reports_its_crash_with_secrets_redacted() {
    for version in BOTH {
        let bench = Bench::new();
        let events = run(&bench, version, "scenario:die").await;
        let (stop, _, diagnostic) = done(&events);
        assert_eq!(stop, StopReason::Error, "{version}");
        let diagnostic = diagnostic.unwrap();
        assert!(
            diagnostic.contains("exited unexpectedly") && diagnostic.contains("out of memory"),
            "{diagnostic}"
        );
        assert!(!diagnostic.contains("supersecretkey"), "{diagnostic}");
    }
}

#[tokio::test]
async fn a_server_that_cannot_start_ends_the_run_with_its_stderr_redacted() {
    let bench = Bench::new();
    let mut live = start(
        &bench.executor(V1, &[("FAKE_OPENCODE_STARTUP", "exit")]),
        bench.spec("scenario:text"),
    )
    .await;
    let events = drive(&mut live, Answers::allow()).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    let diagnostic = diagnostic.unwrap();
    assert!(diagnostic.contains("cannot start"), "{diagnostic}");
    assert!(!diagnostic.contains("sk-secretvalue123"), "{diagnostic}");
}

#[tokio::test]
async fn a_locked_database_at_startup_is_retried() {
    let bench = Bench::new();
    let mut live = start(
        &bench.executor(V1, &[("FAKE_OPENCODE_STARTUP", "locked-once")]),
        bench.spec("scenario:text"),
    )
    .await;
    let events = drive(&mut live, Answers::allow()).await;
    assert_eq!(done(&events).0, StopReason::Completed, "{events:?}");
    let starts = bench
        .logged()
        .iter()
        .filter(|e| e.get("argv").is_some())
        .count();
    assert_eq!(starts, 2);
}

#[tokio::test]
async fn a_bus_that_drops_mid_turn_is_reconnected_and_a_request_asked_in_the_gap_is_listed() {
    for version in BOTH {
        let bench = Bench::new();
        let mut spec = bench.spec("scenario:gap");
        spec.access = AccessMode::Supervised;
        let mut live = start(&bench.executor(version, &[]), spec).await;
        let events = drive(&mut live, Answers::allow()).await;
        assert_eq!(
            done(&events).0,
            StopReason::Completed,
            "{version}: {events:?}"
        );
        assert!(
            text(&events).contains("after the gap: once"),
            "{version}: {events:?}"
        );
        let ids: Vec<_> = events
            .iter()
            .filter(|e| matches!(e, ExecEvent::NeedsApproval { .. }))
            .collect();
        assert_eq!(ids.len(), 1, "asked once");
        assert!(!bench.calls("GET", "permission").is_empty());
    }
}

#[tokio::test]
async fn a_turn_that_ends_while_the_bus_is_down_is_found_ended_from_the_status_route() {
    for version in BOTH {
        let bench = Bench::new();
        let events = run(&bench, version, "scenario:gap-idle").await;
        assert_eq!(
            done(&events).0,
            StopReason::Completed,
            "{version}: {events:?}"
        );
        assert_eq!(text(&events), "finished in the gap");
    }
}

#[tokio::test]
async fn a_bus_that_goes_quiet_while_the_server_still_answers_ends_the_run() {
    let bench = Bench::new();
    let exec = bench
        .executor(V1, &[("FAKE_OPENCODE_BUS", "silent")])
        .with_bus_bounds(
            Duration::from_millis(300),
            Duration::from_millis(500),
            Duration::from_millis(900),
        );
    let mut live = start(&exec, bench.spec("scenario:hang")).await;
    let events = drive(&mut live, Answers::allow()).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(
        stop,
        StopReason::Error,
        "{events:?} {:?}",
        bench
            .logged()
            .iter()
            .filter(|e| e.get("req").is_none())
            .collect::<Vec<_>>()
    );
    assert!(diagnostic.unwrap().contains("stopped responding"));
}

#[tokio::test]
async fn a_tool_process_the_server_started_goes_with_the_run() {
    let bench = Bench::new();
    let mut live = start(
        &bench.executor(V1, &[]),
        bench.spec("scenario:tool-process"),
    )
    .await;
    let mut events = Vec::new();
    while let Some(event) = live.stream.next().await {
        if matches!(event, ExecEvent::Tool { .. }) {
            live.handle.cancel();
        }
        events.push(event);
    }
    assert_eq!(done(&events).0, StopReason::Interrupted);
    let pid: i32 = std::fs::read_to_string(bench.root().join("tool.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(until_gone(pid).await, "the tool's process outlived the run");
}

// ---------------------------------------------------------------------------------------------
// Steering, resume, models
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_steering_message_goes_out_as_the_next_prompt_when_the_turn_settles() {
    for version in BOTH {
        let bench = Bench::new();
        let mut live = start(&bench.executor(version, &[]), bench.spec("scenario:steer")).await;
        let mut events = Vec::new();
        while let Some(event) = live.stream.next().await {
            if matches!(event, ExecEvent::Text { .. })
                && events.iter().all(|e| !matches!(e, ExecEvent::Text { .. }))
            {
                live.handle.steer("redirect please").await.unwrap();
            }
            events.push(event);
        }
        assert_eq!(done(&events).0, StopReason::Completed, "{version}");
        assert!(
            text(&events).contains("steered:redirect please"),
            "{version}: {events:?}"
        );
        let prompts = bench.prompts();
        assert_eq!(prompts.len(), 2, "{version}");
        assert_eq!(prompts[0]["sid"], prompts[1]["sid"], "the same session");
        // The result is the last turn's.
        assert_eq!(done(&events).1.as_deref(), Some("steered:redirect please"));
    }
}

#[tokio::test]
async fn a_session_resumes_across_servers_and_an_unknown_one_starts_over_with_a_note() {
    for version in BOTH {
        let bench = Bench::new();
        let first = run(&bench, version, "scenario:text").await;
        let session = session_of(&first);
        let mut spec = bench.spec("scenario:resumed");
        spec.resume = Some(SessionRef(session.clone()));
        let mut live = start(&bench.executor(version, &[]), spec).await;
        let events = drive(&mut live, Answers::allow()).await;
        assert_eq!(session_of(&events), session, "{version}");
        assert!(text(&events).contains("back again"));
        assert_eq!(
            bench
                .calls(
                    "POST",
                    if version == V1 {
                        "/session"
                    } else {
                        "/api/session"
                    }
                )
                .iter()
                .filter(|r| !r["p"].as_str().unwrap().contains("prompt"))
                .count(),
            1,
            "only the first run created a session"
        );
        // A session the server does not have.
        let mut spec = bench.spec("scenario:resumed");
        spec.resume = Some(SessionRef("ses_doesnotexist".into()));
        let mut live = start(&bench.executor(version, &[]), spec).await;
        let events = drive(&mut live, Answers::allow()).await;
        assert_ne!(session_of(&events), "ses_doesnotexist");
        assert!(
            text(&events).contains("did not find session ses_doesnotexist"),
            "{events:?}"
        );
        assert_eq!(done(&events).0, StopReason::Completed);
    }
}

#[tokio::test]
async fn a_session_id_that_is_not_one_is_refused_before_anything_starts() {
    let bench = Bench::new();
    for bad in ["../x", "th-conf", "ses_a/b", "ses_"] {
        let mut spec = bench.spec("scenario:text");
        spec.resume = Some(SessionRef(bad.into()));
        let (controls, _handle) = ExecControls::new(8);
        let refused = bench
            .executor(V1, &[])
            .start(spec, controls)
            .await
            .err()
            .unwrap();
        assert!(
            matches!(refused, ExecError::InvalidSpec(_)),
            "{bad}: {refused}"
        );
    }
    assert!(
        bench
            .logged()
            .iter()
            .all(|e| e.get("argv").is_none() || e.get("argv") == Some(&json!(["--version"])))
    );
}

#[tokio::test]
async fn a_model_and_an_effort_ride_the_prompt_on_1x_and_the_session_on_2x() {
    for version in BOTH {
        let bench = Bench::new();
        let mut spec = bench.spec("scenario:text");
        spec.model = Some("opencode/reasoner".into());
        spec.effort = Some(Effort::High);
        let mut live = start(&bench.executor(version, &[]), spec).await;
        let events = drive(&mut live, Answers::allow()).await;
        assert!(
            matches!(&events[0], ExecEvent::Started { model, .. } if model == "opencode/reasoner")
        );
        assert_eq!(done(&events).0, StopReason::Completed);
        if version == V1 {
            let body = &bench.calls("POST", "prompt_async")[0]["body"];
            assert_eq!(
                body["model"],
                json!({"providerID": "opencode", "modelID": "reasoner"})
            );
            assert_eq!(body["variant"], "high");
        } else {
            let body = &bench.calls("POST", "/model")[0]["body"];
            assert_eq!(
                *body,
                json!({"model": {"providerID": "opencode", "id": "reasoner", "variant": "high"}})
            );
        }
    }
}

#[tokio::test]
async fn a_model_the_server_does_not_list_ends_the_run_before_a_session_is_made() {
    for version in BOTH {
        let bench = Bench::new();
        let mut spec = bench.spec("scenario:text");
        spec.model = Some("nobody/nothing".into());
        let mut live = start(&bench.executor(version, &[]), spec).await;
        let events = drive(&mut live, Answers::allow()).await;
        let (stop, _, diagnostic) = done(&events);
        assert_eq!(stop, StopReason::Error);
        assert!(diagnostic.unwrap().contains("lists no model"));
        assert!(bench.calls("POST", "session").is_empty());
    }
}

#[tokio::test]
async fn the_agent_option_names_the_plan_agent_on_the_prompt_or_the_session() {
    for version in BOTH {
        let bench = Bench::new();
        let mut spec = bench.spec("scenario:text");
        spec.model_options.insert("agent".into(), "plan".into());
        let mut live = start(&bench.executor(version, &[]), spec).await;
        let events = drive(&mut live, Answers::allow()).await;
        assert_eq!(done(&events).0, StopReason::Completed);
        if version == V1 {
            assert_eq!(
                bench.calls("POST", "prompt_async")[0]["body"]["agent"],
                "plan"
            );
        } else {
            assert_eq!(
                bench.calls("POST", "/api/session")[0]["body"]["agent"],
                "plan"
            );
        }
    }
}

#[tokio::test]
async fn instructions_go_in_front_of_the_first_prompt() {
    let bench = Bench::new();
    let mut spec = bench.spec("scenario:text");
    spec.instructions = Some("Be brief.".into());
    let mut live = start(&bench.executor(V1, &[]), spec).await;
    drive(&mut live, Answers::allow()).await;
    assert_eq!(bench.prompts()[0]["text"], "Be brief.\n\nscenario:text");
}

#[tokio::test]
async fn a_frame_the_table_lacks_is_counted_by_kind() {
    for version in BOTH {
        let bench = Bench::new();
        let exec = bench.executor(version, &[]);
        let mut live = start(&exec, bench.spec("scenario:unknown")).await;
        let events = drive(&mut live, Answers::allow()).await;
        assert_eq!(done(&events).0, StopReason::Completed);
        let counted = exec.unknown_frames();
        assert_eq!(counted.len(), 1, "{version}: {counted:?}");
        let (kind, count) = counted.iter().next().unwrap();
        assert!(
            kind.ends_with("session.teleported") && *count == 1,
            "{kind}"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// Refusals before anything starts
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn what_the_executor_cannot_honour_is_refused_before_a_process_starts() {
    let bench = Bench::new();
    let exec = bench.executor(V1, &[]);
    let refuse = |spec: ExecSpec| {
        let exec = &exec;
        async move {
            let (controls, _handle) = ExecControls::new(8);
            exec.start(spec, controls).await.err().expect("refused")
        }
    };
    let mut spec = bench.spec("x");
    spec.sandbox = SandboxLevel::ReadOnly;
    assert!(matches!(refuse(spec).await, ExecError::InvalidSpec(why) if why.contains("Read-only")));
    let mut spec = bench.spec("x");
    spec.output_schema = Some(json!({"type": "object"}));
    assert!(matches!(
        refuse(spec).await,
        ExecError::UnsupportedOutputSchema
    ));
    assert!(
        bench
            .logged()
            .iter()
            .all(|e| e.get("argv") != Some(&json!(["serve"]))),
        "nothing was started"
    );
}

#[tokio::test]
async fn an_opencode_older_than_the_checked_one_is_refused_and_a_newer_one_runs() {
    let bench = Bench::new();
    let (controls, _handle) = ExecControls::new(8);
    let refused = bench
        .executor("1.18.31", &[])
        .start(bench.spec("x"), controls)
        .await
        .err()
        .unwrap();
    assert!(
        matches!(refused, ExecError::NotInstalled(why) if why.contains("too old") && why.contains("1.19.5"))
    );
    let bench = Bench::new();
    let events = run(&bench, "2.9.0", "scenario:text").await;
    assert_eq!(done(&events).0, StopReason::Completed);
}

#[tokio::test]
async fn a_missing_executable_is_not_installed() {
    let exec = OpencodeExecutor::new().with_executable("/nonexistent/opencode");
    let (controls, _handle) = ExecControls::new(8);
    let bench = Bench::new();
    let refused = exec.start(bench.spec("x"), controls).await.err().unwrap();
    assert!(matches!(refused, ExecError::NotInstalled(_)));
    let probe = exec.probe().await;
    assert!(probe.ineligible().is_some());
}

// ---------------------------------------------------------------------------------------------
// The probe and the descriptor
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_probe_lists_the_models_of_connected_providers_without_a_prompt_and_reaps_its_server() {
    for version in BOTH {
        let bench = Bench::new();
        let exec = bench.executor(version, &[]);
        let probe = exec.probe().await;
        assert!(
            matches!(probe.signed_in, SignedIn::Yes { .. }),
            "{version}: {probe:?}"
        );
        assert_eq!(probe.version_gate, VersionGate::Ok, "{version}");
        assert_eq!(probe.models.source, ModelSource::Live);
        let ids: Vec<&str> = probe.models.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            ["opencode/big-pickle", "opencode/reasoner"],
            "{version}"
        );
        let reasoner = &probe.models.models[1];
        assert_eq!(reasoner.efforts, [Effort::Low, Effort::High]);
        // The agents are an option of each model, the plan agent among them.
        let agent = &reasoner.options[0];
        assert_eq!(agent.id, "agent");
        assert!(agent.choices.iter().any(|c| c.id == "plan"));
        assert!(
            !agent
                .choices
                .iter()
                .any(|c| c.id == "general" || c.id == "title")
        );
        // The descriptor now carries them, and the CLI version.
        let descriptor = exec.descriptor();
        assert_eq!(descriptor.caps.models.len(), 2);
        assert!(descriptor.adapter_version.contains(version));
        // The probe's server asked no model anything, was offline for its catalog, and is gone.
        assert!(bench.prompts().is_empty());
        let started = bench.started();
        assert_eq!(started["env"]["OPENCODE_DISABLE_MODELS_FETCH"], "1");
        assert_eq!(started["env"]["OPENCODE_DISABLE_AUTOUPDATE"], "1");
        assert!(until_gone(i32::try_from(started["pid"].as_i64().unwrap()).unwrap()).await);
        assert!(
            bench.requests().iter().all(|r| r["m"] == "GET"),
            "a probe only reads"
        );
    }
}

#[tokio::test]
async fn a_server_with_no_connected_provider_is_signed_out() {
    for version in BOTH {
        let bench = Bench::new();
        let probe = bench
            .executor(version, &[("FAKE_OPENCODE_CONNECTED", "none")])
            .probe()
            .await;
        assert!(
            matches!(probe.signed_in, SignedIn::No { .. }),
            "{version}: {probe:?}"
        );
        assert!(probe.ineligible().is_some());
    }
}

#[tokio::test]
async fn an_untested_version_is_reported_and_an_old_one_is_not_started() {
    let bench = Bench::new();
    let probe = bench.executor("3.0.0", &[]).probe().await;
    assert!(
        matches!(probe.version_gate, VersionGate::Untested { .. }),
        "{probe:?}"
    );
    let bench = Bench::new();
    let probe = bench.executor("1.18.31", &[]).probe().await;
    assert!(matches!(probe.version_gate, VersionGate::TooOld { .. }));
    assert!(
        bench.logged().iter().all(|e| e.get("argv").is_none()),
        "a too old opencode is not asked"
    );
}

#[test]
fn the_descriptor_claims_what_the_driver_does_and_the_verified_range_it_was_run_on() {
    let descriptor = OpencodeExecutor::new().descriptor();
    assert_eq!(descriptor.id.to_string(), "opencode");
    assert_eq!(descriptor.cli_verified.min, "1.19.5");
    assert_eq!(descriptor.cli_verified.verified, "2.3.2");
    let caps = descriptor.caps;
    assert_eq!(caps.steering, Steering::TurnBoundary);
    assert_eq!(caps.access_modes.len(), 3);
    assert!(caps.resume && caps.mcp_injection && caps.deterministic_turn_end);
    assert!(!caps.plan_mode && !caps.structured_output && !caps.isolates_user_config);
}

// ---------------------------------------------------------------------------------------------
// Live
// ---------------------------------------------------------------------------------------------

/// Drives the real `opencode` on this machine against a THROWAWAY home: its own HOME and XDG
/// directories (nothing under `~/.config/opencode` or `~/.local/share/opencode` is read or written),
/// opencode's anonymous free tier (`opencode/big-pickle`, a real network call to opencode.ai) and
/// no credential. Opt in with `RENO_OPENCODE_LIVE=1` and `--ignored`; never run by default.
/// Always checks the handshake and the read-back of the rules and the MCP registration (no model
/// is asked anything); with `RENO_OPENCODE_LIVE_PROMPT=1` as well it plays a real prompt.
///
/// `OPENCODE_EXECUTABLE` picks the binary (a 2.x one from the registry's archive, for example).
#[tokio::test]
#[ignore = "needs opencode on this machine and RENO_OPENCODE_LIVE=1"]
async fn a_real_opencode_hands_shakes_reads_back_its_rules_and_plays_a_free_model_run() {
    if std::env::var_os("RENO_OPENCODE_LIVE").is_none() {
        eprintln!("RENO_OPENCODE_LIVE is not set: skipped");
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let bench = Bench::new();
    let real = reno_harness::opencode::resolve_opencode_executable().expect("opencode installed");
    let home = bench.dir.path().join("home");
    for sub in ["home", "cfg", "data", "state", "cache"] {
        std::fs::create_dir_all(bench.dir.path().join(sub)).unwrap();
    }
    let wrapper = bench.dir.path().join("opencode-live");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexport HOME='{h}' XDG_CONFIG_HOME='{c}' XDG_DATA_HOME='{d}' XDG_STATE_HOME='{s}' XDG_CACHE_HOME='{k}'\nexec '{real}' \"$@\"\n",
            h = home.display(),
            c = bench.dir.path().join("cfg").display(),
            d = bench.dir.path().join("data").display(),
            s = bench.dir.path().join("state").display(),
            k = bench.dir.path().join("cache").display(),
            real = real.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let exec = OpencodeExecutor::new().with_executable(wrapper);
    let probe = exec.probe().await;
    eprintln!("probe: {probe:?}");
    assert!(
        matches!(probe.signed_in, SignedIn::Yes { .. }),
        "the anonymous tier is a sign-in: {probe:?}"
    );
    assert!(
        probe
            .models
            .models
            .iter()
            .any(|m| m.id == "opencode/big-pickle")
    );
    // Supervised: the rules are read back from the real server, and the MCP server registered. No
    // prompt is sent unless asked for.
    let mut spec = bench.spec("Reply with the single word: pong");
    spec.access = AccessMode::Supervised;
    spec.mcp = vec![McpServerSpec {
        name: "reno".into(),
        command: "/usr/bin/true".into(),
        args: Vec::new(),
        env: Default::default(),
    }];
    spec.model = Some("opencode/big-pickle".into());
    if std::env::var_os("RENO_OPENCODE_LIVE_PROMPT").is_none() {
        // A model that is not there ends the run after the read-back and before any session: the
        // handshake, the rules and the MCP registration are proven without a model call.
        spec.model = Some("opencode/no-such-model".into());
    }
    let mut live = start(&exec, spec).await;
    let events = drive(&mut live, Answers::deny()).await;
    eprintln!("{events:#?}");
    let (stop, _, diagnostic) = done(&events);
    if std::env::var_os("RENO_OPENCODE_LIVE_PROMPT").is_none() {
        assert!(diagnostic.unwrap().contains("lists no model"));
    } else {
        assert_eq!(stop, StopReason::Completed, "{events:?}");
        assert!(text(&events).to_lowercase().contains("pong"), "{events:?}");
    }
}
