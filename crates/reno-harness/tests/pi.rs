//! `PiExecutor` integration tests against the fake `pi --mode rpc` in `tests/fixtures/fake-pi.py`
//! (no real `pi` binary, no model, no network, no Pi settings directory involved). The live test
//! against a real Pi is at the bottom, `#[ignore]`d.
//!
//! The fake plays the scenario a prompt names (`scenario:<name>`) with the frame shapes of Pi
//! 1.5.0, and appends every argument vector and command frame it receives to `FAKE_PI_LOG`, which
//! is how these tests see what the driver sent.

#![cfg(unix)]
// Tests unwrap freely, and probe a leaked pid with `kill(pid, 0)`.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::print_stderr,
    unsafe_code
)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use reno_harness::exec::{Decision, ExecControls, ExecError, ExecHandle, Executor};
use reno_harness::{PiExecutor, ProcessRecord};
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
    let path = fixture_dir().join("fake-pi.py");
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755));
    path
}

fn executor() -> PiExecutor {
    PiExecutor::new()
        .with_executable(fake())
        .with_graces(Duration::from_millis(500), Duration::from_millis(300))
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
    let path = dir.join("pi-wrapper");
    std::fs::write(&path, script).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    // A script written while other tests fork can be "text file busy" (ETXTBSY) for a moment: a
    // forked child holds the write descriptor until it execs. Wait that out here, so it is not
    // taken for a Pi that cannot be run.
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

/// A scratch directory for one test: the fake's log and its session files.
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
        self.dir.path().join("fake-pi.log")
    }

    fn sessions(&self) -> PathBuf {
        self.dir.path().join("sessions")
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
            env: [
                ("FAKE_PI_LOG".to_owned(), self.log().display().to_string()),
                (
                    "FAKE_PI_SESSIONS".to_owned(),
                    self.sessions().display().to_string(),
                ),
            ]
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

    /// The command frames the fake received, in order.
    fn commands(&self) -> Vec<Value> {
        self.logged()
            .into_iter()
            .filter_map(|entry| entry.get("cmd").cloned())
            .collect()
    }

    /// The arguments the fake was started with.
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
}

/// A started run.
struct Live {
    stream: BoxStream<'static, ExecEvent>,
    handle: ExecHandle,
    records: mpsc::Receiver<ProcessRecord>,
}

async fn start(executor: &PiExecutor, spec: ExecSpec) -> Live {
    let (controls, handle) = ExecControls::new(8);
    let records = handle.take_process_records().unwrap();
    let stream = executor.start(spec, controls).await.expect("run starts");
    Live {
        stream,
        handle,
        records,
    }
}

/// Drive `live` to its end, answering every question with its last option (or `A`).
async fn drive(live: &mut Live) -> Vec<ExecEvent> {
    drive_with(live, |_| {}).await
}

async fn drive_with(live: &mut Live, mut on_event: impl FnMut(&ExecEvent)) -> Vec<ExecEvent> {
    let mut events = Vec::new();
    let run = async {
        while let Some(event) = live.stream.next().await {
            on_event(&event);
            if let ExecEvent::NeedsInput {
                request_id,
                questions,
            } = &event
            {
                let answers = questions
                    .iter()
                    .map(|q| reno_harness::exec::InputAnswer {
                        question_id: q.id.clone(),
                        labels: vec![q.options.last().cloned().unwrap_or_else(|| "A".into())],
                    })
                    .collect();
                live.handle
                    .answer_input(request_id.clone(), answers)
                    .await
                    .unwrap();
            }
            events.push(event);
        }
    };
    tokio::time::timeout(Duration::from_secs(20), run)
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
        ExecEvent::Started { model, .. } if model == "fake/fake-1"
    ));
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
    assert!(events.contains(&ExecEvent::Usage {
        input_tokens: 12,
        output_tokens: 3,
        cache_read_tokens: 100,
        cache_write_tokens: 5,
    }));
    assert!(events.contains(&ExecEvent::ContextUsage {
        tokens: Some(120),
        window: Some(128_000),
    }));
    let (stop, result, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Completed);
    assert_eq!(result.as_deref(), Some("conf-hello"));
    assert_eq!(diagnostic, None);
}

#[tokio::test]
async fn the_edit_tool_keeps_pis_edits_array_for_the_journal() {
    let bench = Bench::new();
    let events = run(&bench, "scenario:conf-happy").await;
    let edit = events
        .iter()
        .find_map(|e| match e {
            ExecEvent::Tool { call, .. } if call.name == "edit" => Some(call),
            _ => None,
        })
        .unwrap();
    assert_eq!(edit.input["edits"][0]["newText"], "y");
}

#[tokio::test]
async fn the_started_session_is_the_one_pi_reports_and_each_run_gets_its_own() {
    let bench = Bench::new();
    let one = run(&bench, "hello").await;
    let two = run(&bench, "hello").await;
    let session = |events: &[ExecEvent]| match &events[0] {
        ExecEvent::Started { session, .. } => session.clone(),
        other => panic!("{other:?}"),
    };
    assert_ne!(session(&one), session(&two));
    assert_eq!(text(&one), "reply:hello");
}

#[tokio::test]
async fn the_pi_a_run_spawns_is_reported_and_gone_when_the_run_ends() {
    let bench = Bench::new();
    let mut live = start(&executor(), bench.spec("hello")).await;
    let record = live.records.recv().await.expect("a process record");
    assert!(record.pid > 1);
    assert_eq!(record.pgid, record.pid, "its own process group");
    drive(&mut live).await;
    assert!(until_gone(record.pid as i32).await, "pi was left running");
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
// What is sent to Pi: never a write to its settings
// ---------------------------------------------------------------------------------------------

const SETTINGS_WRITERS: [&str; 6] = [
    "set_steering_mode",
    "set_follow_up_mode",
    "set_auto_compaction",
    "set_auto_retry",
    "cycle_model",
    "cycle_thinking_level",
];

#[tokio::test]
async fn no_run_ever_sends_a_command_that_writes_pis_settings() {
    for prompt in [
        "scenario:conf-happy",
        "scenario:steer",
        "scenario:interrupt",
        "scenario:dialogs",
        "/steering all",
        "/autocompact off",
        "/follow-up all",
    ] {
        let bench = Bench::new();
        let mut spec = bench.spec(prompt);
        spec.model = Some("fake/fake-1".into());
        spec.effort = Some(Effort::High);
        let mut live = start(&executor(), spec).await;
        let handle = live.handle.clone();
        let mut steered = false;
        let mut cancelled = false;
        drive_with(&mut live, |event| {
            if matches!(event, ExecEvent::Text { .. }) && !steered {
                steered = true;
                let _ = handle.try_steer("/steering all");
            }
            if matches!(event, ExecEvent::Started { .. })
                && prompt == "scenario:interrupt"
                && !cancelled
            {
                cancelled = true;
                handle.cancel();
            }
        })
        .await;
        let sent: Vec<String> = bench
            .commands()
            .iter()
            .map(|c| c["type"].as_str().unwrap().to_owned())
            .collect();
        for writer in SETTINGS_WRITERS {
            assert!(
                !sent.iter().any(|kind| kind == writer),
                "{prompt}: sent {writer}: {sent:?}"
            );
        }
        assert!(sent.contains(&"prompt".to_owned()), "{prompt}");
    }
}

#[tokio::test]
async fn slash_text_is_the_task_not_a_pi_command() {
    let bench = Bench::new();
    let events = run(&bench, "/compact please").await;
    assert_eq!(text(&events), "reply:/compact please");
    let prompts: Vec<_> = bench
        .commands()
        .into_iter()
        .filter(|c| c["type"] == "prompt")
        .collect();
    assert_eq!(prompts.len(), 1);
    assert_eq!(prompts[0]["message"], "/compact please");
}

#[tokio::test]
async fn the_command_line_isolates_the_owners_tools_and_trusts_nothing() {
    let bench = Bench::new();
    run(&bench, "hello").await;
    let argv = bench.argv();
    assert_eq!(&argv[..3], ["--mode", "rpc", "--no-themes"]);
    for flag in [
        "--no-extensions",
        "--no-skills",
        "--no-prompt-templates",
        "--no-mcp",
    ] {
        assert!(argv.contains(&flag.to_owned()), "{flag}: {argv:?}");
    }
    for flag in [
        "--approve",
        "-a",
        "--no-session",
        "--session-id",
        "--extension",
    ] {
        assert!(!argv.contains(&flag.to_owned()), "{flag}: {argv:?}");
    }
}

#[tokio::test]
async fn a_run_that_inherits_user_config_leaves_the_flags_off() {
    let bench = Bench::new();
    let mut spec = bench.spec("scenario:user-config");
    spec.inherit_user_config = true;
    let mut live = start(&executor(), spec).await;
    let events = drive(&mut live).await;
    assert_eq!(text(&events), "user-config:inherited");
    assert!(!bench.argv().contains(&"--no-extensions".to_owned()));
    let bench = Bench::new();
    let events = run(&bench, "scenario:user-config").await;
    assert_eq!(text(&events), "user-config:isolated");
}

#[tokio::test]
async fn the_childs_environment_carries_only_what_was_allowed() {
    let bench = Bench::new();
    let mut spec = bench.spec("hello");
    spec.env
        .insert("FAKE_PI_LEAK".into(), "from-the-bot".into());
    drive(&mut start(&executor(), spec).await).await;
    let entry = bench.logged().into_iter().next().unwrap();
    assert_eq!(
        entry["leak"], "from-the-bot",
        "the bot's own variables arrive"
    );
    // The run does not set PI_OFFLINE or PI_TELEMETRY: those are the probe's, not the owner's.
    assert!(entry["offline"].is_null() && entry["telemetry"].is_null());
}

// ---------------------------------------------------------------------------------------------
// Model, effort, instructions, resume
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn the_model_and_effort_are_set_on_the_live_process_before_the_prompt() {
    let bench = Bench::new();
    let mut spec = bench.spec("scenario:which-model");
    spec.model = Some("fake/fake-1".into());
    spec.effort = Some(Effort::XHigh);
    let events = drive(&mut start(&executor(), spec).await).await;
    assert_eq!(text(&events), "fake/fake-1/xhigh");
    let kinds: Vec<_> = bench
        .commands()
        .iter()
        .map(|c| c["type"].as_str().unwrap().to_owned())
        .collect();
    let at = |kind: &str| kinds.iter().position(|k| k == kind).unwrap();
    assert!(at("set_model") < at("set_thinking_level"));
    assert!(at("set_thinking_level") < at("prompt"));
}

#[tokio::test]
async fn a_bare_model_id_one_provider_serves_is_found_and_thinking_off_is_an_option() {
    let bench = Bench::new();
    let mut spec = bench.spec("scenario:which-model");
    spec.model = Some("fake-2".into());
    spec.effort = Some(Effort::High);
    spec.model_options
        .insert("pi_thinking".into(), "off".into());
    let events = drive(&mut start(&executor(), spec).await).await;
    assert_eq!(text(&events), "fake/fake-2/off");
}

#[tokio::test]
async fn a_model_pi_does_not_list_ends_the_run_with_the_reason() {
    let bench = Bench::new();
    let mut spec = bench.spec("hello");
    spec.model = Some("fake/nope".into());
    let events = drive(&mut start(&executor(), spec).await).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(diagnostic.unwrap().contains("not available"));
    assert!(
        !bench.commands().iter().any(|c| c["type"] == "prompt"),
        "nothing was asked of a model that was not chosen"
    );
}

#[tokio::test]
async fn instructions_go_in_a_private_file_the_flag_names() {
    let bench = Bench::new();
    let mut spec = bench.spec("hello");
    spec.instructions = Some("Be brief.".into());
    drive(&mut start(&executor(), spec).await).await;
    let argv = bench.argv();
    let at = argv
        .iter()
        .position(|a| a == "--append-system-prompt")
        .unwrap();
    // The text is in a file, not on the command line `ps` shows; the file is gone with the run.
    assert!(!argv.iter().any(|a| a.contains("Be brief")));
    assert!(argv[at + 1].contains("instructions.md"));
    assert!(!Path::new(&argv[at + 1]).exists());
}

#[tokio::test]
async fn a_resume_opens_the_session_by_id_and_says_so_when_its_history_is_gone() {
    let bench = Bench::new();
    let mut spec = bench.spec("scenario:resumed");
    spec.resume = Some(SessionRef("th-conf".into()));
    let events = drive(&mut start(&executor(), spec).await).await;
    assert!(matches!(
        &events[0],
        ExecEvent::Started { session, .. } if session.0 == "th-conf"
    ));
    let argv = bench.argv();
    let at = argv.iter().position(|a| a == "--session-id").unwrap();
    assert_eq!(argv[at + 1], "th-conf");
    // No session file for it: Pi started one with that id.
    assert!(
        text(&events).contains("without the previous context"),
        "{events:?}"
    );
    assert!(text(&events).ends_with("back again"));
    assert_eq!(done(&events).0, StopReason::Completed);

    // With its file on disk the session was found: no notice.
    let bench = Bench::new();
    std::fs::create_dir_all(bench.sessions()).unwrap();
    std::fs::write(bench.sessions().join("th-conf.jsonl"), "{}\n").unwrap();
    let mut spec = bench.spec("scenario:resumed");
    spec.resume = Some(SessionRef("th-conf".into()));
    let events = drive(&mut start(&executor(), spec).await).await;
    assert_eq!(text(&events), "back again");
}

#[tokio::test]
async fn a_session_id_pi_would_refuse_is_refused_before_anything_spawns() {
    let bench = Bench::new();
    for bad in ["--help", "a b", "../x", ""] {
        let mut spec = bench.spec("hello");
        spec.resume = Some(SessionRef(bad.into()));
        let (controls, _handle) = ExecControls::new(4);
        let error = executor().start(spec, controls).await.err().unwrap();
        assert!(matches!(error, ExecError::InvalidSpec(_)), "{bad}: {error}");
    }
    assert!(bench.logged().is_empty(), "nothing was spawned");
}

// ---------------------------------------------------------------------------------------------
// Steering and interruption
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn a_steering_message_goes_as_a_steering_prompt_and_its_turn_ends_the_run() {
    let bench = Bench::new();
    let mut live = start(&executor(), bench.spec("scenario:steer")).await;
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
        .filter(|c| c["streamingBehavior"] == "steer")
        .collect();
    assert_eq!(steers.len(), 1);
    assert_eq!(steers[0]["type"], "prompt");
    assert_eq!(steers[0]["message"], "go left");
}

#[tokio::test]
async fn steering_messages_keep_their_order() {
    let bench = Bench::new();
    let mut live = start(&executor(), bench.spec("scenario:steer-twice")).await;
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
async fn a_steer_that_meets_an_idle_pi_starts_a_run_the_run_waits_for() {
    let bench = Bench::new();
    let mut live = start(&executor(), bench.spec("scenario:steer-after-settle")).await;
    let handle = live.handle.clone();
    let mut sent = false;
    let events = drive_with(&mut live, |event| {
        if matches!(event, ExecEvent::Text { .. }) && !sent {
            sent = true;
            handle.try_steer("late").unwrap();
        }
    })
    .await;
    assert!(text(&events).ends_with("late-run:late"), "{events:?}");
    assert_eq!(done(&events).0, StopReason::Completed);
}

#[tokio::test]
async fn cancel_aborts_pi_and_ends_the_run_interrupted_with_nothing_left_running() {
    let bench = Bench::new();
    let mut live = start(&executor(), bench.spec("scenario:interrupt")).await;
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
    let kinds: Vec<String> = bench
        .commands()
        .iter()
        .map(|c| c["type"].as_str().unwrap().to_owned())
        .collect();
    let at = |kind: &str| kinds.iter().position(|k| k == kind).unwrap();
    assert!(
        at("clear_queue") < at("abort"),
        "the queue is cleared before the abort: {kinds:?}"
    );
    assert!(until_gone(record.pid as i32).await, "pi was left running");
}

#[tokio::test]
async fn a_pi_that_ignores_the_abort_is_ended_anyway() {
    let bench = Bench::new();
    let mut live = start(&executor(), bench.spec("scenario:wedged")).await;
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
    assert!(until_gone(record.pid as i32).await, "pi was left running");
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
    assert!(diagnostic.contains("exit code 7"), "{diagnostic}");
    assert!(diagnostic.contains("out of memory"), "{diagnostic}");
}

#[tokio::test]
async fn a_provider_error_a_recovered_retry_and_a_cut_off_reply_each_end_as_they_should() {
    let bench = Bench::new();
    let (stop, _, diagnostic) = done(&run(&bench, "scenario:error").await);
    assert_eq!(stop, StopReason::Error);
    assert_eq!(diagnostic.as_deref(), Some("provider said no"));
    let (stop, result, _) = done(&run(&bench, "scenario:retry").await);
    assert_eq!(
        stop,
        StopReason::Completed,
        "a retry that succeeds is not a failure"
    );
    assert_eq!(result.as_deref(), Some("recovered"));
    let (stop, _, _) = done(&run(&bench, "scenario:length").await);
    assert_eq!(stop, StopReason::MaxTokens);
}

#[tokio::test]
async fn a_prompt_pi_refuses_ends_the_run_with_pis_words() {
    let bench = Bench::new();
    let events = run(&bench, "scenario:reject").await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert_eq!(
        diagnostic.as_deref(),
        Some("Pi prompt: No API key found for fake")
    );
}

#[tokio::test]
async fn a_prompt_an_extension_handled_ends_after_one_ordered_state_check() {
    let bench = Bench::new();
    let events = run(&bench, "scenario:handled").await;
    assert_eq!(done(&events).0, StopReason::Completed);
    let kinds: Vec<String> = bench
        .commands()
        .iter()
        .map(|c| c["type"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(kinds.last().map(String::as_str), Some("get_state"));
}

#[tokio::test]
async fn work_an_extension_began_is_waited_for_not_cut_off() {
    let bench = Bench::new();
    let events = run(&bench, "scenario:handled-then-run").await;
    assert_eq!(text(&events), "extension-run");
    assert_eq!(done(&events).0, StopReason::Completed);
}

#[tokio::test]
async fn frames_the_driver_has_never_seen_are_counted_and_the_run_goes_on() {
    let bench = Bench::new();
    let exec = executor();
    let mut live = start(&exec, bench.spec("scenario:conf-unknown")).await;
    let events = drive(&mut live).await;
    assert_eq!(done(&events).2, None);
    assert_eq!(
        exec.unknown_frames().into_iter().collect::<Vec<_>>(),
        vec![
            ("message_update:audio_delta".to_owned(), 1),
            ("type:agent_teleported".to_owned(), 1),
        ]
    );
    // A clean run counts nothing.
    let exec = executor();
    drive(&mut start(&exec, bench.spec("scenario:conf-happy")).await).await;
    assert!(exec.unknown_frames().is_empty());
}

// ---------------------------------------------------------------------------------------------
// Dialogs
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn an_extension_dialog_is_a_question_and_the_answer_goes_back_to_pi() {
    let bench = Bench::new();
    let events = run(&bench, "scenario:conf-question").await;
    let question = events
        .iter()
        .find_map(|e| match e {
            ExecEvent::NeedsInput {
                request_id,
                questions,
            } => Some((request_id.clone(), questions.clone())),
            _ => None,
        })
        .unwrap();
    assert_eq!(question.0, "dlg-1");
    assert_eq!(question.1[0].options, ["first", "second"]);
    assert_eq!(text(&events), "answered:second");
    assert_eq!(done(&events).0, StopReason::Completed);
}

#[tokio::test]
async fn confirm_and_input_dialogs_answer_in_their_own_shapes_and_notify_is_dropped() {
    let bench = Bench::new();
    let events = run(&bench, "scenario:dialogs").await;
    // The drive answers the last option of the confirm ("No") and "A" for the free text.
    assert_eq!(text(&events), "confirmed:False typed:A");
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, ExecEvent::NeedsInput { .. }))
            .count(),
        2
    );
}

#[tokio::test]
async fn a_dialog_pi_gives_up_on_is_reported_expired_and_the_run_goes_on() {
    let bench = Bench::new();
    let mut live = start(&executor(), bench.spec("scenario:dialog-timeout")).await;
    // Never answered: Pi's 150 ms timeout lapses first.
    let mut events = Vec::new();
    let run = async {
        while let Some(event) = live.stream.next().await {
            events.push(event);
        }
    };
    tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .unwrap();
    assert!(events.contains(&ExecEvent::InputExpired {
        request_id: "dlg-t".into()
    }));
    assert_eq!(text(&events), "moved-on");
    assert_eq!(done(&events).0, StopReason::Completed);
}

#[tokio::test]
async fn a_declined_question_cancels_the_dialog() {
    let bench = Bench::new();
    let mut live = start(&executor(), bench.spec("scenario:conf-question")).await;
    let mut events = Vec::new();
    let run = async {
        while let Some(event) = live.stream.next().await {
            if let ExecEvent::NeedsInput { request_id, .. } = &event {
                live.handle
                    .answer_input(request_id.clone(), Vec::new())
                    .await
                    .unwrap();
            }
            events.push(event);
        }
    };
    tokio::time::timeout(Duration::from_secs(20), run)
        .await
        .unwrap();
    assert_eq!(text(&events), "answered:cancelled");
}

// ---------------------------------------------------------------------------------------------
// MCP
// ---------------------------------------------------------------------------------------------

fn server(name: &str) -> McpServerSpec {
    McpServerSpec {
        name: name.into(),
        command: format!("/opt/{name}/server"),
        args: vec!["mcp".into()],
        env: [("RENO_RUN".to_owned(), "r1".to_owned())].into(),
    }
}

#[tokio::test]
async fn reno_and_a_plugins_servers_reach_pi_through_one_bridge_in_the_scratch_dir() {
    let bench = Bench::new();
    let mut spec = bench.spec("scenario:mcp-config");
    spec.mcp = vec![server("reno"), server("plugin-focus")];
    let events = drive(&mut start(&executor(), spec).await).await;
    assert_eq!(text(&events), "mcp configured:reno,plugin-focus ext=1");
    let entry = bench.logged().into_iter().next().unwrap();
    let config = entry["mcp_env"].as_str().unwrap();
    let scratch = Path::new(config).parent().unwrap();
    assert!(scratch.starts_with(std::env::temp_dir()), "{config}");
    assert!(
        !config.contains(".pi"),
        "the bridge is Reno's file, not Pi's configuration: {config}"
    );
    // The bridge and its config go with the run.
    assert!(!scratch.exists(), "{} was left behind", scratch.display());
}

#[tokio::test]
async fn a_run_without_servers_loads_no_bridge() {
    let bench = Bench::new();
    let events = run(&bench, "scenario:mcp-config").await;
    assert_eq!(text(&events), "mcp configured: ext=0");
}

// ---------------------------------------------------------------------------------------------
// Refusals and the probe
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn what_pi_cannot_honour_is_refused_before_it_spawns() {
    let bench = Bench::new();
    for access in [AccessMode::Supervised, AccessMode::AutoAcceptEdits] {
        let mut spec = bench.spec("hello");
        spec.access = access;
        let (controls, _h) = ExecControls::new(4);
        let error = executor().start(spec, controls).await.err().unwrap();
        assert!(
            matches!(error, ExecError::UnsupportedAccess { requested, .. } if requested == access)
        );
    }
    for level in [SandboxLevel::ReadOnly, SandboxLevel::WorkspaceWrite] {
        let mut spec = bench.spec("hello");
        spec.sandbox = level;
        let (controls, _h) = ExecControls::new(4);
        let error = executor().start(spec, controls).await.err().unwrap();
        assert!(matches!(error, ExecError::InvalidSpec(why) if why.contains(&level.to_string())));
    }
    let mut spec = bench.spec("hello");
    spec.output_schema = Some(serde_json::json!({"type": "object"}));
    let (controls, _h) = ExecControls::new(4);
    let error = executor().start(spec, controls).await.err().unwrap();
    assert!(matches!(error, ExecError::UnsupportedOutputSchema));
    assert!(bench.logged().is_empty(), "nothing was spawned");
}

#[tokio::test]
async fn a_missing_pi_is_not_installed() {
    let bench = Bench::new();
    let exec = PiExecutor::new().with_executable("/nonexistent/pi");
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

#[tokio::test]
async fn the_probe_reads_version_sign_in_and_models_without_a_prompt() {
    let bench = Bench::new();
    let wrapper = wrapped(
        bench.dir.path(),
        &[("FAKE_PI_LOG", bench.log().to_str().unwrap())],
    );
    let exec = PiExecutor::new().with_executable(wrapper);
    let probe = exec.probe().await;
    assert!(matches!(
        &probe.installed,
        InstallStatus::Installed { version: Some(v), .. } if v == "1.5.0"
    ));
    assert_eq!(probe.version_gate, VersionGate::Ok);
    assert_eq!(probe.signed_in, SignedIn::Yes { account: None });
    assert_eq!(probe.models.source, ModelSource::Live);
    let ids: Vec<_> = probe.models.models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["fake/fake-1", "fake/fake-2"]);
    // fake-1 reasons and names xhigh; fake-2 does not reason.
    assert_eq!(probe.models.models[0].efforts.len(), 5);
    assert!(probe.models.models[1].efforts.is_empty());
    // The descriptor offers what the probe found.
    assert_eq!(exec.descriptor().caps.models.len(), 2);
    // No prompt, no settings write; offline and without telemetry; no session kept.
    let commands = bench.commands();
    assert!(
        commands.iter().all(|c| c["type"] == "get_available_models"),
        "{commands:?}"
    );
    let entry = bench.logged().into_iter().next().unwrap();
    assert_eq!(entry["offline"], "1");
    assert_eq!(entry["telemetry"], "0");
    let argv = bench.argv();
    assert!(argv.contains(&"--no-session".to_owned()));
    assert!(argv.contains(&"--no-extensions".to_owned()));
}

#[tokio::test]
async fn a_pi_with_no_provider_credential_is_signed_out() {
    let bench = Bench::new();
    let wrapper = wrapped(bench.dir.path(), &[("FAKE_PI_NO_MODELS", "1")]);
    let probe = PiExecutor::new().with_executable(wrapper).probe().await;
    assert!(matches!(&probe.signed_in, SignedIn::No { hint } if hint.contains("/login")));
    assert!(probe.models.models.is_empty());
}

#[tokio::test]
async fn the_version_gate_follows_the_checked_range() {
    let bench = Bench::new();
    for (version, expect_old, expect_untested) in [
        ("1.4.0", true, false),
        ("1.5.0", false, false),
        ("1.6.2", false, true),
    ] {
        let wrapper = wrapped(bench.dir.path(), &[("FAKE_PI_VERSION", version)]);
        let probe = PiExecutor::new().with_executable(wrapper).probe().await;
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
    assert_eq!(descriptor.id.as_str(), "pi");
    assert_eq!(descriptor.cli_verified.min, "1.5.0");
    assert_eq!(descriptor.cli_verified.verified, "1.5.0");
    assert_eq!(descriptor.caps.steering, Steering::StepBoundary);
    assert_eq!(descriptor.caps.access_modes, [AccessMode::FullAccess]);
    assert!(descriptor.caps.isolates_user_config);
    let declared = executor().maintenance().unwrap();
    assert!(declared.updaters.is_empty());
}

fn reno_executor(bench: &Bench) -> PiExecutor {
    PiExecutor::reno(bench.sessions())
        .with_executable(fake())
        .with_graces(Duration::from_millis(500), Duration::from_millis(300))
}

#[test]
fn reno_declares_supervised_and_full_access_with_no_sandbox_ceiling() {
    let bench = Bench::new();
    let descriptor = reno_executor(&bench).descriptor();
    assert_eq!(descriptor.id.as_str(), reno_harness::pi::RENO_EXECUTOR_ID);
    assert_eq!(descriptor.id.as_str(), "reno");
    assert_eq!(
        descriptor.caps.access_modes,
        [AccessMode::Supervised, AccessMode::FullAccess]
    );
    assert_eq!(descriptor.caps.sandbox_levels, [SandboxLevel::Unrestricted]);
    assert!(descriptor.caps.resume);
    assert!(descriptor.caps.isolates_user_config);
    assert_eq!(
        descriptor.cli_verified,
        PiExecutor::new().descriptor().cli_verified
    );
}

#[test]
fn reno_flavor_leaves_the_pi_descriptor_as_it_was() {
    let descriptor = PiExecutor::new().descriptor();
    assert_eq!(descriptor.id.as_str(), "pi");
    assert_eq!(descriptor.caps.access_modes, [AccessMode::FullAccess]);
    assert_eq!(descriptor.caps.sandbox_levels, [SandboxLevel::Unrestricted]);
    assert!(descriptor.caps.resume);
    assert!(descriptor.caps.isolates_user_config);
    assert!(PiExecutor::new().maintenance().is_some());
}

#[test]
fn reno_has_no_install_update_or_sign_in_row_of_its_own() {
    let bench = Bench::new();
    assert!(reno_executor(&bench).maintenance().is_none());
}

#[tokio::test]
async fn reno_probe_does_not_ask_pi_for_the_owners_credentials() {
    let bench = Bench::new();
    let wrapper = wrapped(
        bench.dir.path(),
        &[("FAKE_PI_LOG", bench.log().to_str().unwrap())],
    );
    let exec = PiExecutor::reno(bench.sessions()).with_executable(wrapper);
    let probe = exec.probe().await;
    assert!(matches!(
        &probe.installed,
        InstallStatus::Installed { version: Some(v), .. } if v == "1.5.0"
    ));
    assert_eq!(probe.version_gate, VersionGate::Ok);
    assert_eq!(probe.signed_in, SignedIn::Unknown);
    assert_eq!(probe.models.source, ModelSource::Curated);
    assert!(!probe.models.models.is_empty());
    assert_eq!(exec.descriptor().caps.models, probe.models.models);
    assert!(bench.logged().is_empty(), "no Pi was started to be asked");
}

fn bot_spec(bench: &Bench) -> ExecSpec {
    let mut spec = bench.spec("hello");
    spec.env.insert("RENO_BOT".to_owned(), "keeper".to_owned());
    spec
}

#[tokio::test]
async fn reno_refuses_a_run_that_has_no_bot() {
    let bench = Bench::new();
    let (controls, _h) = ExecControls::new(4);
    let error = reno_executor(&bench)
        .start(bench.spec("hello"), controls)
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&error, ExecError::InvalidSpec(why) if why.contains("runs bots only")),
        "{error:?}"
    );
    assert!(bench.logged().is_empty(), "nothing was spawned");
}

#[tokio::test]
async fn reno_refuses_a_bot_that_inherits_the_owners_config() {
    let bench = Bench::new();
    let mut spec = bot_spec(&bench);
    spec.inherit_user_config = true;
    let (controls, _h) = ExecControls::new(4);
    let error = reno_executor(&bench)
        .start(spec, controls)
        .await
        .err()
        .unwrap();
    assert!(
        matches!(&error, ExecError::InvalidSpec(why)
            if why.contains("runs bots only") && why.contains("inherit_user_config")),
        "{error:?}"
    );
    assert!(bench.logged().is_empty(), "nothing was spawned");
}

/// A bot's run whose fake plays the gate as `mode`.
fn gate_spec(bench: &Bench, mode: &str) -> ExecSpec {
    let mut spec = bot_spec(bench);
    spec.env.insert("FAKE_PI_GATE".to_owned(), mode.to_owned());
    spec
}

fn arming(bench: &Bench, budget: Duration) -> PiExecutor {
    reno_executor(bench).with_arming_budget(budget)
}

const PLENTY: Duration = Duration::from_secs(10);

fn prompts_sent(bench: &Bench) -> usize {
    bench
        .commands()
        .iter()
        .filter(|cmd| cmd["type"] == "prompt")
        .count()
}

/// The fake's log lines that carry `key`, in order.
fn logged_values(bench: &Bench, key: &str) -> Vec<Value> {
    bench
        .logged()
        .iter()
        .filter_map(|entry| entry.get(key).cloned())
        .collect()
}

async fn gate_run(bench: &Bench, mode: &str, budget: Duration) -> Vec<ExecEvent> {
    let mut live = start(&arming(bench, budget), gate_spec(bench, mode)).await;
    drive(&mut live).await
}

#[tokio::test]
async fn reno_arm_lets_the_prompt_go_only_after_the_gate_is_listed_and_has_said_hello() {
    let bench = Bench::new();
    let events = gate_run(&bench, "ok", PLENTY).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Completed, "{diagnostic:?}");
    assert_eq!(text(&events), "reply:hello");
    let kinds: Vec<String> = bench
        .commands()
        .iter()
        .map(|cmd| cmd["type"].as_str().unwrap().to_owned())
        .collect();
    let listed = kinds.iter().position(|k| k == "get_commands").unwrap();
    let prompted = kinds.iter().position(|k| k == "prompt").unwrap();
    assert!(listed < prompted, "{kinds:?}");
    assert_eq!(prompts_sent(&bench), 1);
}

#[tokio::test]
async fn reno_arm_a_gate_that_connects_and_closes_without_a_hello_fails_the_run() {
    let bench = Bench::new();
    let events = gate_run(&bench, "no-hello", PLENTY).await;
    let (stop, result, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(result.is_none());
    let why = diagnostic.unwrap();
    assert!(
        why.contains("did not arm") && why.contains("sent no hello"),
        "{why}"
    );
    assert_eq!(prompts_sent(&bench), 0);
}

#[tokio::test]
async fn reno_arm_a_gate_that_never_connects_fails_the_run_when_the_budget_ends() {
    let bench = Bench::new();
    let events = gate_run(&bench, "never-connect", Duration::from_millis(500)).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    let why = diagnostic.unwrap();
    assert!(
        why.contains("did not arm") && why.contains("in time"),
        "{why}"
    );
    assert_eq!(prompts_sent(&bench), 0);
}

#[tokio::test]
async fn reno_arm_a_hello_with_another_nonce_fails_the_run_without_echoing_either() {
    let bench = Bench::new();
    let events = gate_run(&bench, "wrong-nonce", PLENTY).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    let why = diagnostic.unwrap();
    assert!(
        why.contains("did not arm") && why.contains("nonce"),
        "{why}"
    );
    let nonce = logged_values(&bench, "gate_nonce")[0]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(!why.contains(&nonce), "{why}");
    assert_eq!(prompts_sent(&bench), 0);
}

#[tokio::test]
async fn reno_arm_a_pi_that_does_not_list_the_gate_command_fails_the_run() {
    let bench = Bench::new();
    let events = gate_run(&bench, "no-command", PLENTY).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    let why = diagnostic.unwrap();
    assert!(
        why.contains("did not arm") && why.contains("reno-gate"),
        "{why}"
    );
    assert_eq!(prompts_sent(&bench), 0);
}

#[tokio::test]
async fn reno_arm_a_failed_arming_starts_no_run_and_is_a_stream_not_a_refusal() {
    let bench = Bench::new();
    let (controls, _handle) = ExecControls::new(4);
    let stream = arming(&bench, PLENTY)
        .start(gate_spec(&bench, "no-hello"), controls)
        .await;
    let mut stream = stream.expect("a failed arming is the attempt's own Done, not an ExecError");
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ExecEvent::Started { .. })),
        "{events:?}"
    );
    done(&events);
}

#[tokio::test]
async fn reno_arm_a_second_connection_is_closed_unserved_and_none_is_accepted_once_armed() {
    let bench = Bench::new();
    let events = gate_run(&bench, "second-client", PLENTY).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Completed, "{diagnostic:?}");
    let second = logged_values(&bench, "gate_second");
    assert!(
        second == ["closed"] || second == ["refused"],
        "the second client was served: {second:?}"
    );
    assert_eq!(logged_values(&bench, "gate_late"), ["refused"]);
}

#[tokio::test]
async fn reno_arm_a_resumed_run_arms_again_with_a_new_socket_and_a_new_nonce() {
    let bench = Bench::new();
    let exec = arming(&bench, PLENTY);
    for attempt in 0..2 {
        let mut spec = gate_spec(&bench, "ok");
        if attempt == 1 {
            spec.resume = Some(SessionRef("th-conf".into()));
        }
        let mut live = start(&exec, spec).await;
        let events = drive(&mut live).await;
        let (stop, _, diagnostic) = done(&events);
        assert_eq!(stop, StopReason::Completed, "{diagnostic:?}");
    }
    assert_eq!(prompts_sent(&bench), 2);
    let nonces = logged_values(&bench, "gate_nonce");
    let sockets = logged_values(&bench, "gate_socket");
    assert_eq!(nonces.len(), 2);
    assert_ne!(nonces[0], nonces[1]);
    assert_ne!(sockets[0], sockets[1]);
    assert!(
        logged_values(&bench, "gate_late")
            .iter()
            .all(|late| late == "refused")
    );
}

#[tokio::test]
async fn reno_arm_a_bot_key_that_lists_no_model_fails_before_any_prompt_naming_the_secret() {
    const VALUE: &str = "sk-secret-value-9876";
    let bench = Bench::new();
    let mut spec = gate_spec(&bench, "ok");
    spec.env.insert("OPENAI".to_owned(), VALUE.to_owned());
    spec.env
        .insert("FAKE_PI_NO_MODELS".to_owned(), "1".to_owned());
    let mut live = start(&arming(&bench, PLENTY), spec).await;
    let events = drive(&mut live).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    let why = diagnostic.unwrap();
    let (secrets, hint) = why.split_once("; Pi reads").expect(&why);
    assert!(secrets.contains("OPENAI"), "{why}");
    assert!(hint.contains("OPENAI_API_KEY"), "{why}");
    assert!(!why.contains(VALUE), "{why}");
    assert_eq!(prompts_sent(&bench), 0);
}

#[tokio::test]
async fn reno_arm_a_model_pi_does_not_list_still_fails_before_any_prompt() {
    let bench = Bench::new();
    let mut spec = gate_spec(&bench, "ok");
    spec.model = Some("fake/not-a-model".into());
    let mut live = start(&arming(&bench, PLENTY), spec).await;
    let events = drive(&mut live).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(diagnostic.unwrap().contains("not-a-model"));
    assert_eq!(prompts_sent(&bench), 0);
}

#[tokio::test]
async fn reno_arm_an_attempt_that_fails_to_arm_leaves_no_scratch_behind() {
    let bench = Bench::new();
    let events = gate_run(&bench, "no-hello", PLENTY).await;
    done(&events);
    let socket = logged_values(&bench, "gate_socket")[0]
        .as_str()
        .unwrap()
        .to_owned();
    let scratch = Path::new(&socket).parent().unwrap().parent().unwrap();
    assert!(!scratch.exists(), "{} was left behind", scratch.display());
}

#[tokio::test]
async fn reno_still_refuses_an_access_mode_it_does_not_declare() {
    let bench = Bench::new();
    let mut spec = bench.spec("hello");
    spec.access = AccessMode::AutoAcceptEdits;
    let (controls, _h) = ExecControls::new(4);
    let error = reno_executor(&bench)
        .start(spec, controls)
        .await
        .err()
        .unwrap();
    assert!(matches!(
        error,
        ExecError::UnsupportedAccess {
            requested: AccessMode::AutoAcceptEdits,
            ..
        }
    ));
}

// ---------------------------------------------------------------------------------------------
// The calls: each one is held for the ladder before it runs
// ---------------------------------------------------------------------------------------------

/// A bot's run whose fake plays the gate and the tool loop of `scenario`.
fn call_spec(bench: &Bench, scenario: &str) -> ExecSpec {
    let mut spec = gate_spec(bench, "ok");
    spec.prompt = format!("scenario:{scenario}");
    spec
}

type Verdict = (Decision, Option<String>);

/// Drive `live` to its end, answering each `NeedsApproval` as `verdict` says.
async fn drive_deciding(
    live: &mut Live,
    mut verdict: impl FnMut(&ApprovalSubject) -> Verdict,
) -> Vec<ExecEvent> {
    let mut events = Vec::new();
    let run = async {
        while let Some(event) = live.stream.next().await {
            if let ExecEvent::NeedsApproval {
                request_id,
                subject,
            } = &event
            {
                let (decision, reason) = verdict(subject);
                live.handle
                    .decide_with_message(request_id.clone(), decision, reason)
                    .await
                    .unwrap();
            }
            events.push(event);
        }
    };
    tokio::time::timeout(Duration::from_secs(60), run)
        .await
        .expect("the run ends");
    events
}

fn allow_all(_: &ApprovalSubject) -> Verdict {
    (Decision::Allow, None)
}

fn raised(events: &[ExecEvent]) -> Vec<(&str, &ApprovalSubject)> {
    events
        .iter()
        .filter_map(|event| match event {
            ExecEvent::NeedsApproval {
                request_id,
                subject,
            } => Some((request_id.as_str(), subject)),
            _ => None,
        })
        .collect()
}

fn call_id(subject: &ApprovalSubject) -> &str {
    subject.raw["toolCallId"].as_str().unwrap()
}

fn position(events: &[ExecEvent], wanted: impl Fn(&ExecEvent) -> bool) -> usize {
    events.iter().position(wanted).unwrap()
}

/// Every call was raised after its `Tool` event and before its `ToolResult`.
fn assert_each_call_is_raised_between_its_tool_and_its_result(events: &[ExecEvent]) {
    for (_, subject) in raised(events) {
        let id = call_id(subject);
        let started = position(
            events,
            |e| matches!(e, ExecEvent::Tool { id: t, .. } if t == id),
        );
        let asked = position(
            events,
            |e| matches!(e, ExecEvent::NeedsApproval { subject, .. } if call_id(subject) == id),
        );
        let ended = position(
            events,
            |e| matches!(e, ExecEvent::ToolResult { id: t, .. } if t == id),
        );
        assert!(
            started < asked && asked < ended,
            "{id}: {started} {asked} {ended}"
        );
    }
}

fn ran(bench: &Bench) -> Vec<Value> {
    logged_values(bench, "ran")
}

fn gate_decisions(bench: &Bench) -> Vec<Value> {
    logged_values(bench, "gate_decision")
}

#[tokio::test]
async fn reno_calls_every_kind_of_call_is_raised_with_what_the_gate_sent() {
    let bench = Bench::new();
    let mut live = start(&reno_executor(&bench), call_spec(&bench, "gate-coverage")).await;
    let events = drive_deciding(&mut live, allow_all).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Completed, "{diagnostic:?}");
    let expected: [(&str, &str, ActionClass, Option<&str>, Value); 14] = [
        (
            "g-read",
            "read",
            ActionClass::Read,
            None,
            json!({"path": "a.txt"}),
        ),
        (
            "g-grep",
            "grep",
            ActionClass::Read,
            None,
            json!({"pattern": "needle", "path": "src", "glob": "*.rs"}),
        ),
        (
            "g-find",
            "find",
            ActionClass::Read,
            None,
            json!({"pattern": "*.md"}),
        ),
        ("g-ls", "ls", ActionClass::Read, None, json!({})),
        (
            "g-bash",
            "bash",
            ActionClass::Command,
            None,
            json!({"command": "echo hi > canary.txt"}),
        ),
        (
            "g-edit",
            "edit",
            ActionClass::Edit,
            None,
            json!({"path": "a.txt", "edits": [{"oldText": "x", "newText": "y"}]}),
        ),
        (
            "g-write",
            "write",
            ActionClass::Edit,
            None,
            json!({"path": "b.txt", "content": "hello"}),
        ),
        (
            "g-mcp",
            "mcp__reno__ping",
            ActionClass::McpTool,
            None,
            json!({"message": "hi"}),
        ),
        (
            "g-sib1",
            "read",
            ActionClass::Read,
            None,
            json!({"path": "sib1.txt"}),
        ),
        (
            "g-sib2",
            "read",
            ActionClass::Read,
            None,
            json!({"path": "sib2.txt"}),
        ),
        (
            "g-sib3",
            "read",
            ActionClass::Read,
            None,
            json!({"path": "sib3.txt"}),
        ),
        (
            "g-code",
            "codemode",
            ActionClass::Other,
            None,
            json!({"code": "await ping(); await read()"}),
        ),
        (
            "g-code/1",
            "mcp__reno__ping",
            ActionClass::McpTool,
            Some("g-code"),
            json!({"message": "nested"}),
        ),
        (
            "g-code/2",
            "read",
            ActionClass::Read,
            Some("g-code"),
            json!({"path": "deep.txt"}),
        ),
    ];
    let asked = raised(&events);
    assert_eq!(asked.len(), expected.len(), "one request per call");
    for (id, tool, class, parent, input) in expected {
        let (request_id, subject) = asked
            .iter()
            .find(|(_, subject)| call_id(subject) == id)
            .unwrap_or_else(|| panic!("{id} was never raised"));
        assert!(request_id.starts_with("gate-"), "{request_id}");
        assert_eq!(subject.tool, tool, "{id}");
        assert_eq!(subject.action, class, "{id}");
        assert_eq!(subject.raw["parentToolCallId"], json!(parent), "{id}");
        assert_eq!(subject.raw["toolName"], tool, "{id}");
        assert_eq!(subject.raw["input"], input, "{id}");
    }
    let bash = asked
        .iter()
        .find(|(_, s)| call_id(s) == "g-bash")
        .unwrap()
        .1;
    assert_eq!(bash.command.as_deref(), Some("echo hi > canary.txt"));
    let order: Vec<&str> = asked.iter().map(|(_, s)| call_id(s)).collect();
    assert_eq!(
        &order[8..11],
        ["g-sib1", "g-sib2", "g-sib3"],
        "siblings go one after another: {order:?}"
    );
    for window in order[8..11].windows(2) {
        let first_result = position(
            &events,
            |e| matches!(e, ExecEvent::ToolResult { id, .. } if id == window[0]),
        );
        let second_asked = position(
            &events,
            |e| matches!(e, ExecEvent::NeedsApproval { subject, .. } if call_id(subject) == window[1]),
        );
        assert!(first_result < second_asked, "{window:?}");
    }
    assert_each_call_is_raised_between_its_tool_and_its_result(&events);
    assert!(
        events.iter().any(|e| matches!(
            e,
            ExecEvent::Usage {
                output_tokens: 3,
                ..
            }
        )),
        "per-turn usage is still reported: {events:?}"
    );
    assert_eq!(text(&events), "gate-covered");
}

#[tokio::test]
async fn reno_calls_an_allowed_call_runs_with_the_arguments_it_was_judged_on() {
    let bench = Bench::new();
    let mut live = start(&reno_executor(&bench), call_spec(&bench, "gate-coverage")).await;
    let events = drive_deciding(&mut live, allow_all).await;
    done(&events);
    let ran = ran(&bench);
    let asked = raised(&events);
    assert_eq!(ran.len(), asked.len());
    for (_, subject) in asked {
        let did = ran
            .iter()
            .find(|r| r["call_id"] == call_id(subject))
            .unwrap();
        assert_eq!(did["tool"], subject.raw["toolName"]);
        assert_eq!(did["input"], subject.raw["input"]);
    }
    assert!(bench.dir.path().join("canary.txt").exists());
}

#[tokio::test]
async fn reno_calls_a_denied_bash_runs_nothing_and_the_bot_reads_the_reason() {
    let bench = Bench::new();
    let mut live = start(&reno_executor(&bench), call_spec(&bench, "gate-coverage")).await;
    let events = drive_deciding(&mut live, |subject| {
        if subject.tool == "bash" {
            (Decision::Deny, Some("bash is not allowed here".to_owned()))
        } else {
            allow_all(subject)
        }
    })
    .await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Completed, "{diagnostic:?}");
    assert!(
        !bench.dir.path().join("canary.txt").exists(),
        "the command ran"
    );
    assert!(ran(&bench).iter().all(|r| r["tool"] != "bash"));
    let result = events
        .iter()
        .find_map(|e| match e {
            ExecEvent::ToolResult {
                id,
                ok,
                summary,
                output,
            } if id == "g-bash" => Some((*ok, summary.clone(), output.clone())),
            _ => None,
        })
        .unwrap();
    assert!(!result.0, "a denied call is an error result");
    assert!(
        result
            .1
            .as_deref()
            .is_some_and(|s| s.contains("bash is not allowed here"))
            && result
                .2
                .as_ref()
                .is_some_and(|o| o.text.contains("bash is not allowed here")),
        "{result:?}"
    );
}

#[tokio::test]
async fn reno_calls_two_hundred_calls_run_to_the_end_with_no_cap() {
    let bench = Bench::new();
    let mut live = start(&reno_executor(&bench), call_spec(&bench, "gate-200")).await;
    let events = drive_deciding(&mut live, allow_all).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Completed, "{diagnostic:?}");
    assert_eq!(text(&events), "gate-200-done");
    assert_eq!(raised(&events).len(), 200);
    assert_eq!(ran(&bench).len(), 200);
    assert_each_call_is_raised_between_its_tool_and_its_result(&events);
}

/// Start `scenario` and read events until `count` calls are open; the rest is for the caller.
async fn until_open(live: &mut Live, count: usize) -> (Vec<ExecEvent>, Vec<String>) {
    let mut events = Vec::new();
    let mut open = Vec::new();
    let wait = async {
        while open.len() < count {
            let event = live.stream.next().await.expect("the run is still going");
            if let ExecEvent::NeedsApproval { request_id, .. } = &event {
                open.push(request_id.clone());
            }
            events.push(event);
        }
    };
    tokio::time::timeout(Duration::from_secs(20), wait)
        .await
        .expect("the calls are raised");
    (events, open)
}

async fn finish(live: &mut Live, mut events: Vec<ExecEvent>) -> Vec<ExecEvent> {
    events.extend(drive(live).await);
    events
}

fn scratch_of(bench: &Bench) -> PathBuf {
    let socket = logged_values(bench, "gate_socket")[0]
        .as_str()
        .unwrap()
        .to_owned();
    Path::new(&socket)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf()
}

#[tokio::test]
async fn reno_calls_a_cancel_denies_every_open_request_and_leaves_no_scratch() {
    let bench = Bench::new();
    let mut live = start(&reno_executor(&bench), call_spec(&bench, "gate-hold")).await;
    let (before, open) = until_open(&mut live, 3).await;
    assert_eq!(open.len(), 3);
    live.handle.cancel();
    let events = finish(&mut live, before).await;
    let (stop, _, _) = done(&events);
    assert_eq!(stop, StopReason::Interrupted);
    let decisions = gate_decisions(&bench);
    assert_eq!(decisions.len(), 3, "{decisions:?}");
    assert!(
        decisions.iter().all(|d| d["decision"] == "deny"),
        "{decisions:?}"
    );
    let mut ids: Vec<u64> = decisions
        .iter()
        .map(|d| d["id"].as_u64().unwrap())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, [1, 2, 3]);
    assert!(!scratch_of(&bench).exists());
}

#[tokio::test]
async fn reno_calls_a_call_answered_just_before_a_cancel_is_decided_only_once() {
    let bench = Bench::new();
    let mut live = start(&reno_executor(&bench), call_spec(&bench, "gate-hold")).await;
    let (before, open) = until_open(&mut live, 3).await;
    live.handle
        .decide(open[0].clone(), Decision::Allow)
        .await
        .unwrap();
    live.handle.cancel();
    let events = finish(&mut live, before).await;
    done(&events);
    let mut ids: Vec<u64> = gate_decisions(&bench)
        .iter()
        .map(|d| d["id"].as_u64().unwrap())
        .collect();
    ids.sort_unstable();
    assert_eq!(ids, [1, 2, 3], "one decision for each call, none twice");
}

#[tokio::test]
async fn reno_calls_a_call_made_while_the_run_is_being_stopped_is_denied_not_raised() {
    let bench = Bench::new();
    let mut live = start(&reno_executor(&bench), call_spec(&bench, "gate-late")).await;
    let (before, _) = until_open(&mut live, 1).await;
    live.handle.cancel();
    let events = finish(&mut live, before).await;
    let (stop, _, _) = done(&events);
    assert_eq!(stop, StopReason::Interrupted);
    assert_eq!(raised(&events).len(), 1, "the late call was never raised");
    let decisions = gate_decisions(&bench);
    assert_eq!(decisions.len(), 2, "{decisions:?}");
    assert!(decisions.iter().all(|d| d["decision"] == "deny"));
}

#[tokio::test]
async fn reno_calls_a_gate_that_sends_a_seventeenth_open_call_has_it_denied_and_the_run_ends() {
    let bench = Bench::new();
    let mut live = start(&reno_executor(&bench), call_spec(&bench, "gate-flood")).await;
    let events = drive(&mut live).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(
        diagnostic.unwrap().contains("too many calls"),
        "the end names the cause"
    );
    assert_eq!(
        raised(&events).len(),
        16,
        "only the first sixteen are raised"
    );
    let decisions = gate_decisions(&bench);
    assert_eq!(decisions.len(), 17, "{decisions:?}");
    assert!(decisions.iter().all(|d| d["decision"] == "deny"));
    let excess = decisions.iter().find(|d| d["id"] == 17).unwrap();
    assert!(excess["reason"].as_str().unwrap().contains("too many"));
    assert!(!scratch_of(&bench).exists());
}

#[tokio::test]
async fn reno_calls_a_call_sent_before_its_start_frame_is_raised_after_its_tool_event() {
    let bench = Bench::new();
    let mut live = start(&reno_executor(&bench), call_spec(&bench, "gate-call-first")).await;
    let events = drive_deciding(&mut live, allow_all).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Completed, "{diagnostic:?}");
    assert_eq!(raised(&events).len(), 3);
    assert_each_call_is_raised_between_its_tool_and_its_result(&events);
}

#[tokio::test]
async fn reno_calls_a_child_with_a_blank_or_relative_home_is_not_started() {
    for home in ["", "relative/home"] {
        let bench = Bench::new();
        let mut spec = call_spec(&bench, "gate-home");
        spec.env.insert("HOME".to_owned(), home.to_owned());
        let mut live = start(&reno_executor(&bench), spec).await;
        let events = drive(&mut live).await;
        let (stop, _, diagnostic) = done(&events);
        assert_eq!(stop, StopReason::Error, "{home:?}");
        assert!(
            diagnostic.unwrap().contains("absolute HOME"),
            "{home:?} names the cause"
        );
        assert!(bench.logged().is_empty(), "{home:?}: no Pi was spawned");
    }
}

const CREDENTIAL_ENDING: &str =
    "a credential file appeared in the run's Pi agent directory (auth.json)";

fn denied_decisions(bench: &Bench) -> Vec<Value> {
    logged_values(bench, "gate_decision")
        .into_iter()
        .filter(|decision| decision["decision"] == "deny")
        .collect()
}

#[tokio::test]
async fn reno_watch_a_gate_that_closes_mid_run_ends_the_run_as_lost() {
    let bench = Bench::new();
    let mut live = start(&reno_executor(&bench), call_spec(&bench, "gate-drop")).await;
    let events = drive(&mut live).await;
    let (stop, result, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(result.is_none());
    assert_eq!(
        diagnostic.unwrap(),
        "Reno lost its watch over the run: the gate's connection closed"
    );
    assert!(raised(&events).is_empty());
}

#[tokio::test]
async fn reno_watch_a_credential_written_to_the_agent_dir_ends_the_run_at_its_next_call() {
    let bench = Bench::new();
    let mut live = start(&reno_executor(&bench), call_spec(&bench, "gate-auth-json")).await;
    let events = drive_deciding(&mut live, allow_all).await;
    let (stop, result, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(result.is_none());
    assert_eq!(diagnostic.unwrap(), CREDENTIAL_ENDING);
    assert!(
        raised(&events).is_empty(),
        "the call never reached the ladder"
    );
    let denied = denied_decisions(&bench);
    assert_eq!(denied.len(), 1, "{denied:?}");
    assert!(
        denied[0]["reason"].as_str().unwrap().contains("auth.json"),
        "{denied:?}"
    );
    assert!(
        logged_values(&bench, "ran").is_empty(),
        "the call did not run"
    );
    assert!(!bench.dir.path().join("canary.txt").exists());
}

#[tokio::test]
async fn reno_watch_a_reno_run_sends_neither_a_settings_writer_nor_the_host_bash() {
    let bench = Bench::new();
    let mut live = start(&reno_executor(&bench), call_spec(&bench, "gate-coverage")).await;
    let events = drive_deciding(&mut live, allow_all).await;
    assert_eq!(done(&events).0, StopReason::Completed);
    let sent: Vec<String> = bench
        .commands()
        .iter()
        .map(|c| c["type"].as_str().unwrap().to_owned())
        .collect();
    assert!(sent.contains(&"prompt".to_owned()), "{sent:?}");
    for refused in SETTINGS_WRITERS.iter().chain(&["bash"]) {
        assert!(
            !sent.iter().any(|kind| kind == refused),
            "{refused}: {sent:?}"
        );
    }
}

#[tokio::test]
async fn reno_watch_a_garbled_auth_file_ends_the_run_too() {
    let bench = Bench::new();
    let mut live = start(
        &reno_executor(&bench),
        call_spec(&bench, "gate-auth-garbled"),
    )
    .await;
    let events = drive_deciding(&mut live, allow_all).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert_eq!(diagnostic.unwrap(), CREDENTIAL_ENDING);
    assert!(raised(&events).is_empty());
    assert_eq!(denied_decisions(&bench).len(), 1);
}

#[tokio::test]
async fn reno_watch_a_missing_auth_file_and_pis_own_empty_object_do_not_end_the_run() {
    let bench = Bench::new();
    let mut live = start(&reno_executor(&bench), call_spec(&bench, "gate-auth-empty")).await;
    let events = drive_deciding(&mut live, allow_all).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Completed, "{diagnostic:?}");
    assert_eq!(raised(&events).len(), 3);
    assert!(denied_decisions(&bench).is_empty());
}

#[tokio::test]
async fn reno_watch_a_credential_present_at_arming_fails_the_attempt_before_any_prompt() {
    let bench = Bench::new();
    let events = gate_run(&bench, "auth-preseed", PLENTY).await;
    let (stop, result, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(result.is_none());
    assert_eq!(diagnostic.unwrap(), CREDENTIAL_ENDING);
    assert_eq!(prompts_sent(&bench), 0);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, ExecEvent::Started { .. })),
        "{events:?}"
    );
}

#[tokio::test]
async fn reno_calls_a_path_under_the_home_is_judged_against_the_childs_own_home() {
    let bench = Bench::new();
    let mut live = start(&reno_executor(&bench), call_spec(&bench, "gate-home")).await;
    let events = drive_deciding(&mut live, allow_all).await;
    done(&events);
    let home = std::env::var("HOME").unwrap();
    assert_eq!(
        raised(&events)[0].1.paths,
        [Path::new(&home).join(".reno/secret")]
    );

    let mut spec = call_spec(&bench, "gate-home");
    spec.env.insert("HOME".to_owned(), "/bots/home".to_owned());
    let mut live = start(&reno_executor(&bench), spec).await;
    let events = drive_deciding(&mut live, allow_all).await;
    done(&events);
    assert_eq!(
        raised(&events)[0].1.paths,
        [PathBuf::from("/bots/home/.reno/secret")]
    );
}

// ---------------------------------------------------------------------------------------------
// A run's session, kept for its resumes and only that long
// ---------------------------------------------------------------------------------------------

fn session_dir_of(bench: &Bench, run: &RunId) -> PathBuf {
    bench.sessions().join(run.as_str())
}

/// A bot's run of `prompt` on the fake, with the gate in its default mode.
fn bot_spec_for(bench: &Bench, prompt: &str) -> ExecSpec {
    let mut spec = bench.spec(prompt);
    spec.env.insert("RENO_BOT".to_owned(), "keeper".to_owned());
    spec
}

fn run_named(suffix: &str) -> RunId {
    RunId::from_suffix(suffix).unwrap()
}

#[tokio::test]
async fn reno_session_a_closed_run_loses_its_session_only_after_its_pi_has_exited() {
    let bench = Bench::new();
    // A long grace: Pi stays up for about a second after the `Done`, however slow the machine.
    let exec =
        reno_executor(&bench).with_graces(Duration::from_millis(500), Duration::from_secs(2));
    let spec = bot_spec_for(&bench, "scenario:wedged");
    let run = spec.run_id.clone();
    let mut live = start(&exec, spec).await;
    let record = live.records.recv().await.unwrap();
    let handle = live.handle.clone();
    let mut cancelled = false;
    let mut events = Vec::new();
    // Read up to the `Done` and no further: the attempt is still ending its Pi.
    while let Some(event) = live.stream.next().await {
        if matches!(event, ExecEvent::Started { .. }) && !cancelled {
            cancelled = true;
            handle.cancel();
        }
        let last = matches!(event, ExecEvent::Done { .. });
        events.push(event);
        if last {
            break;
        }
    }
    assert_eq!(done(&events).0, StopReason::Interrupted);
    assert!(session_dir_of(&bench, &run).is_dir());
    assert!(
        alive(record.pid as i32),
        "the attempt is still ending its Pi"
    );

    exec.run_closed(&run).await;

    assert!(
        !alive(record.pid as i32),
        "Pi was still running when the session went"
    );
    assert!(!session_dir_of(&bench, &run).exists());
}

#[tokio::test]
async fn reno_session_a_run_keeps_its_session_until_it_is_closed() {
    let bench = Bench::new();
    let exec = reno_executor(&bench);
    let spec = bot_spec_for(&bench, "hello");
    let run = spec.run_id.clone();
    let mut live = start(&exec, spec).await;
    drive(&mut live).await;
    assert!(
        session_dir_of(&bench, &run).is_dir(),
        "the session outlives the attempt"
    );

    let never_started = run_named("01HZXA");
    std::fs::create_dir_all(session_dir_of(&bench, &never_started)).unwrap();
    exec.run_closed(&never_started).await;
    assert!(!session_dir_of(&bench, &never_started).exists());
    assert!(
        session_dir_of(&bench, &run).is_dir(),
        "another run's session stays"
    );

    exec.run_closed(&run).await;
    assert!(!session_dir_of(&bench, &run).exists());
    exec.run_closed(&run).await;
}

#[tokio::test]
async fn reno_session_closing_a_run_whose_dir_is_a_link_removes_the_link_and_not_what_it_names() {
    let bench = Bench::new();
    let exec = reno_executor(&bench);
    let run = run_named("01HZXB");
    let outside = bench.dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("precious"), "keep").unwrap();
    std::fs::create_dir_all(bench.sessions()).unwrap();
    std::os::unix::fs::symlink(&outside, session_dir_of(&bench, &run)).unwrap();

    exec.run_closed(&run).await;

    assert!(std::fs::symlink_metadata(session_dir_of(&bench, &run)).is_err());
    assert_eq!(
        std::fs::read_to_string(outside.join("precious")).unwrap(),
        "keep"
    );
}

#[tokio::test]
async fn reno_session_the_start_up_sweep_removes_unknown_and_closed_dirs_and_keeps_the_kept() {
    use std::collections::HashSet;
    let bench = Bench::new();
    let exec = reno_executor(&bench);
    let kept = [run_named("01HZXC"), run_named("01HZXD")];
    let gone = [run_named("01HZXE"), run_named("01HZXF")];
    for run in kept.iter().chain(&gone) {
        let dir = session_dir_of(&bench, run);
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("nested").join("s.jsonl"), "{}\n").unwrap();
    }
    let outside = bench.dir.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("precious"), "keep").unwrap();
    let linked = run_named("01HZXG");
    std::os::unix::fs::symlink(&outside, session_dir_of(&bench, &linked)).unwrap();
    std::fs::write(bench.sessions().join("stray.txt"), "x").unwrap();

    let keep: HashSet<RunId> = kept.iter().cloned().collect();
    exec.retain_runs(&keep).await;

    for run in &kept {
        assert!(session_dir_of(&bench, run).join("nested/s.jsonl").is_file());
    }
    for run in &gone {
        assert!(!session_dir_of(&bench, run).exists(), "{run} was kept");
    }
    assert!(
        std::fs::symlink_metadata(session_dir_of(&bench, &linked)).is_err(),
        "the link goes"
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("precious")).unwrap(),
        "keep"
    );
    assert!(
        bench.sessions().join("stray.txt").is_file(),
        "only session dirs are the sweep's"
    );
}

#[tokio::test]
async fn reno_session_a_sweep_with_no_sessions_root_does_nothing() {
    let bench = Bench::new();
    reno_executor(&bench).retain_runs(&Default::default()).await;
    assert!(!bench.sessions().exists());
}

#[tokio::test]
async fn reno_session_a_sessions_root_that_is_a_link_is_not_swept_or_closed_through() {
    let bench = Bench::new();
    let elsewhere = bench.dir.path().join("elsewhere");
    let run = run_named("01HZXI");
    std::fs::create_dir_all(elsewhere.join(run.as_str())).unwrap();
    let precious = elsewhere.join(run.as_str()).join("precious");
    std::fs::write(&precious, "keep").unwrap();
    std::os::unix::fs::symlink(&elsewhere, bench.sessions()).unwrap();
    let exec = reno_executor(&bench);

    exec.retain_runs(&Default::default()).await;
    exec.run_closed(&run).await;

    assert_eq!(std::fs::read_to_string(&precious).unwrap(), "keep");
}

#[tokio::test]
async fn reno_session_a_sessions_root_that_is_a_link_or_not_a_directory_fails_the_launch() {
    for plant in ["link", "file"] {
        let bench = Bench::new();
        let elsewhere = bench.dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        if plant == "link" {
            std::os::unix::fs::symlink(&elsewhere, bench.sessions()).unwrap();
        } else {
            std::fs::write(bench.sessions(), "not a directory").unwrap();
        }
        let mut live = start(&reno_executor(&bench), bot_spec_for(&bench, "hello")).await;
        let events = drive(&mut live).await;
        let (stop, _, diagnostic) = done(&events);
        assert_eq!(stop, StopReason::Error, "{plant}");
        assert!(
            diagnostic.unwrap().contains("sessions"),
            "{plant}: names the cause"
        );
        assert!(bench.logged().is_empty(), "{plant}: no Pi was spawned");
        assert_eq!(
            std::fs::read_dir(&elsewhere).unwrap().count(),
            0,
            "{plant}: nothing was created through it"
        );
    }
}

#[tokio::test]
async fn reno_session_the_sweep_leaves_what_is_not_a_run_alone() {
    let bench = Bench::new();
    let exec = reno_executor(&bench);
    let gone = run_named("01HZXJ");
    std::fs::create_dir_all(session_dir_of(&bench, &gone)).unwrap();
    for other in ["notes", "run", "run_", "run_a-b", "thr_01HZXK", ".hidden"] {
        std::fs::create_dir_all(bench.sessions().join(other).join("inner")).unwrap();
    }

    exec.retain_runs(&Default::default()).await;

    assert!(!session_dir_of(&bench, &gone).exists());
    for other in ["notes", "run", "run_", "run_a-b", "thr_01HZXK", ".hidden"] {
        assert!(
            bench.sessions().join(other).join("inner").is_dir(),
            "{other} is not a run's session"
        );
    }
}

#[tokio::test]
async fn reno_session_a_regular_file_at_a_runs_path_is_left_by_the_sweep_and_by_closing() {
    let bench = Bench::new();
    let exec = reno_executor(&bench);
    let run = run_named("01HZXL");
    std::fs::create_dir_all(bench.sessions()).unwrap();
    std::fs::write(session_dir_of(&bench, &run), "not a session").unwrap();

    exec.retain_runs(&Default::default()).await;
    assert!(session_dir_of(&bench, &run).is_file(), "the sweep");

    exec.run_closed(&run).await;
    assert!(session_dir_of(&bench, &run).is_file(), "closing the run");
}

#[tokio::test]
async fn reno_session_the_owners_executor_closes_and_sweeps_nothing() {
    let bench = Bench::new();
    let run = run_named("01HZXH");
    std::fs::create_dir_all(session_dir_of(&bench, &run)).unwrap();
    let owner = executor();
    owner.run_closed(&run).await;
    owner.retain_runs(&Default::default()).await;
    assert!(session_dir_of(&bench, &run).is_dir());
}

#[tokio::test]
async fn reno_session_a_resumed_attempt_names_the_same_session_dir_and_finds_its_session() {
    let bench = Bench::new();
    let exec = reno_executor(&bench);
    let first = bot_spec_for(&bench, "hello");
    let run = first.run_id.clone();
    let mut live = start(&exec, first).await;
    let events = drive(&mut live).await;
    done(&events);
    let session = events
        .iter()
        .find_map(|event| match event {
            ExecEvent::Started { session, .. } => Some(session.clone()),
            _ => None,
        })
        .expect("a started session");

    let mut second = bot_spec_for(&bench, "again");
    second.resume = Some(session);
    // A daemon that started again has no memory of the first attempt.
    let mut live = start(&reno_executor(&bench), second).await;
    let events = drive(&mut live).await;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Completed, "{diagnostic:?}");

    let wanted = session_dir_of(&bench, &run).display().to_string();
    let argv: Vec<Vec<String>> = logged_values(&bench, "argv")
        .iter()
        .map(|argv| {
            argv.as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap().to_owned())
                .collect()
        })
        .collect();
    assert_eq!(argv.len(), 2);
    for argv in &argv {
        let at = argv.iter().position(|a| a == "--session-dir").unwrap();
        assert_eq!(argv[at + 1], wanted);
    }
    assert!(
        !text(&events).contains("did not find session"),
        "the second attempt found its session: {}",
        text(&events)
    );
}

// ---------------------------------------------------------------------------------------------
// Live (a real Pi with a local mock provider; no model, no network, no API spend)
// ---------------------------------------------------------------------------------------------

/// Drives a real `pi` against a throwaway agent directory and the mock provider in
/// `fixtures/pi-rpc-probe.ts`. Opt in with `RENO_PI_LIVE=1` and `--ignored`; never run by
/// default. The agent directory is a temp dir, so nothing under `~/.pi` is read or written.
#[tokio::test]
#[ignore = "needs a Pi >= 1.5.0 on this machine and RENO_PI_LIVE=1"]
async fn a_real_pi_plays_a_run_against_its_mock_provider() {
    if std::env::var_os("RENO_PI_LIVE").is_none() {
        eprintln!("RENO_PI_LIVE is not set: skipped");
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let bench = Bench::new();
    let agent = bench.dir.path().join("agent");
    let extensions = agent.join("extensions");
    std::fs::create_dir_all(&extensions).unwrap();
    std::fs::write(
        agent.join("settings.json"),
        r#"{"retry":{"enabled":false}}"#,
    )
    .unwrap();
    std::fs::write(
        extensions.join("probe.ts"),
        std::fs::read(fixture_dir().join("pi-rpc-probe.ts")).unwrap(),
    )
    .unwrap();
    let real = reno_harness::pi::resolve_pi_executable().expect("Pi installed");
    let wrapper = bench.dir.path().join("pi-probe");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexport PI_CODING_AGENT_DIR='{}' PI_OFFLINE=1 PI_TELEMETRY=0\nexec '{}' \"$@\"\n",
            agent.display(),
            real.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let exec = PiExecutor::new().with_executable(wrapper);
    for (prompt, stop) in [
        ("hello", StopReason::Completed),
        ("/probe-noop", StopReason::Completed),
        ("error", StopReason::Error),
    ] {
        let mut spec = bench.spec(prompt);
        spec.env.clear();
        spec.env.insert("HOME".to_owned(), live_home(&bench));
        // The probe extension is a user extension: this run inherits it.
        spec.inherit_user_config = true;
        // Pin the mock model: whatever provider keys the environment holds, the prompt goes
        // nowhere else.
        spec.model = Some("reno-probe/mock".into());
        let events = drive(&mut start(&exec, spec).await).await;
        assert_eq!(done(&events).0, stop, "{prompt}: {events:?}");
        if prompt == "hello" {
            assert!(text(&events).contains("MOCK:hello"), "{events:?}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Live: a real Pi behind Reno's gate (the mock provider of the probe; no model, no API spend)
// ---------------------------------------------------------------------------------------------

/// The live tests below drive a real `pi` as a Reno run, in the shape of the first live test: a
/// wrapper exports only `PI_OFFLINE` and `PI_TELEMETRY` and loads the probe with `-e` ahead of
/// the driver's own arguments, so the gate is still the last extension loaded. The wrapper never
/// sets `PI_CODING_AGENT_DIR`: the run's own agent directory is what is under test, so nothing
/// under `~/.pi` is read or written. Opt in with `RENO_PI_LIVE=1` and `--ignored`; never run by
/// default.
fn live_reno_is_enabled() -> bool {
    if std::env::var("RENO_PI_LIVE").as_deref() == Ok("1") {
        return true;
    }
    eprintln!("RENO_PI_LIVE is not set to 1: skipped");
    false
}

fn live_reno_executor(bench: &Bench) -> PiExecutor {
    use std::os::unix::fs::PermissionsExt;
    let real = reno_harness::pi::resolve_pi_executable().expect("Pi installed");
    let wrapper = bench.dir.path().join("pi-reno-probe");
    std::fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nexport PI_OFFLINE=1 PI_TELEMETRY=0\nexec '{}' -e '{}' \"$@\"\n",
            real.display(),
            fixture_dir().join("pi-rpc-probe.ts").display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();
    PiExecutor::reno(bench.sessions()).with_executable(wrapper)
}

/// A bot's run of the probe's `prompt`, on the mock model, in a directory with files to act on.
fn live_reno_spec(bench: &Bench, prompt: &str) -> ExecSpec {
    std::fs::write(bench.dir.path().join("a.txt"), "needle in a haystack\n").unwrap();
    std::fs::write(bench.dir.path().join("edit-me.txt"), "before\n").unwrap();
    let mut spec = bench.spec(prompt);
    spec.env.clear();
    spec.env.insert("RENO_BOT".to_owned(), "keeper".to_owned());
    spec.env.insert("HOME".to_owned(), live_home(bench));
    spec.model = Some("reno-probe/mock".into());
    spec
}

/// A throwaway home inside the bench, so a live run never reads or writes the owner's.
fn live_home(bench: &Bench) -> String {
    let home = bench.dir.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    home.display().to_string()
}

/// How long each call took from its `NeedsApproval` to its `ToolResult`, with every call allowed
/// the moment it is raised.
struct Allowed {
    events: Vec<ExecEvent>,
    latencies: Vec<Duration>,
}

async fn drive_allowing(live: &mut Live) -> Allowed {
    let mut events = Vec::new();
    let mut asked = std::collections::HashMap::new();
    let mut latencies = Vec::new();
    let run = async {
        while let Some(event) = live.stream.next().await {
            match &event {
                ExecEvent::NeedsApproval {
                    request_id,
                    subject,
                } => {
                    asked.insert(call_id(subject).to_owned(), std::time::Instant::now());
                    live.handle
                        .decide_with_message(request_id.clone(), Decision::Allow, None)
                        .await
                        .unwrap();
                }
                ExecEvent::ToolResult { id, .. } => {
                    if let Some(at) = asked.remove(id) {
                        latencies.push(at.elapsed());
                    }
                }
                _ => {}
            }
            events.push(event);
        }
    };
    tokio::time::timeout(Duration::from_secs(120), run)
        .await
        .expect("the run ends");
    Allowed { events, latencies }
}

fn probe_lines(bench: &Bench, file: &str) -> Vec<Value> {
    std::fs::read_to_string(bench.dir.path().join(file))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[tokio::test]
#[ignore = "needs a Pi >= 1.5.0 on this machine and RENO_PI_LIVE=1"]
async fn a_real_pi_behind_the_gate_arms_and_every_kind_of_call_reaches_a_decision() {
    if !live_reno_is_enabled() {
        return;
    }
    let bench = Bench::new();
    let exec = live_reno_executor(&bench);
    let mut live = start(&exec, live_reno_spec(&bench, "script-tools")).await;
    let allowed = drive_allowing(&mut live).await;
    let events = allowed.events;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Completed, "{diagnostic:?}");
    assert!(text(&events).contains("SCRIPTED-DONE"), "{events:?}");

    // Armed against Pi's own agent directory, holding its own `{}` auth file.
    let agent = &probe_lines(&bench, "probe-agent.jsonl")[0];
    let agent_dir = agent["agentDir"]
        .as_str()
        .expect("Pi has an agent directory");
    let home = std::env::var("HOME").unwrap();
    assert!(
        !Path::new(agent_dir).starts_with(Path::new(&home).join(".pi")),
        "the run used the owner's Pi directory: {agent_dir}"
    );
    let auth = agent["auth"]
        .as_str()
        .expect("Pi wrote no auth.json of its own before the prompt");
    assert_eq!(serde_json::from_str::<Value>(auth).unwrap(), json!({}));

    // Every scripted kind reached a decision, in the class the ladder reads it by.
    let calls: Vec<_> = raised(&events);
    let find = |name: &str| {
        calls
            .iter()
            .find(|(_, subject)| subject.tool == name && subject.raw["parentToolCallId"].is_null())
            .unwrap_or_else(|| panic!("no {name} call was raised: {calls:?}"))
            .1
    };
    for (name, class) in [
        ("read", ActionClass::Read),
        ("edit", ActionClass::Edit),
        ("write", ActionClass::Edit),
        ("bash", ActionClass::Command),
        ("mcp__probe__ping", ActionClass::McpTool),
        ("codemode", ActionClass::Other),
    ] {
        assert_eq!(find(name).action, class, "{name}");
    }

    // The nested calls of the codemode script, each made by it.
    let script = call_id(find("codemode")).to_owned();
    let nested: Vec<&str> = calls
        .iter()
        .filter(|(_, subject)| subject.raw["parentToolCallId"] == script.as_str())
        .map(|(_, subject)| subject.tool.as_str())
        .collect();
    assert!(nested.contains(&"read"), "{nested:?}");
    assert!(nested.contains(&"mcp__probe__ping"), "{nested:?}");

    // Each was raised after its start and before its result, and ended.
    assert_each_call_is_raised_between_its_tool_and_its_result(&events);
    assert_eq!(allowed.latencies.len(), calls.len());

    // The gate is the last `tool_call` handler: it judged the command as the probe's handler
    // before it left it, and that is what ran.
    assert_eq!(
        find("bash").command.as_deref(),
        Some("echo MUTATED > bash-ran.txt")
    );
    assert_eq!(
        std::fs::read_to_string(bench.dir.path().join("bash-ran.txt")).unwrap(),
        "MUTATED\n"
    );
    assert_eq!(
        std::fs::read_to_string(bench.dir.path().join("edit-me.txt")).unwrap(),
        "after\n"
    );
}

#[tokio::test]
#[ignore = "needs a Pi >= 1.5.0 on this machine and RENO_PI_LIVE=1"]
async fn a_real_pi_behind_the_gate_ends_the_run_when_a_credential_is_planted() {
    if !live_reno_is_enabled() {
        return;
    }
    let bench = Bench::new();
    let exec = live_reno_executor(&bench);
    let mut live = start(&exec, live_reno_spec(&bench, "script-plant")).await;
    let events = drive_allowing(&mut live).await.events;
    let (stop, result, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Error);
    assert!(result.is_none());
    assert_eq!(diagnostic.unwrap(), CREDENTIAL_ENDING);
    let tools: Vec<&str> = raised(&events)
        .into_iter()
        .map(|(_, subject)| subject.tool.as_str())
        .collect();
    assert_eq!(
        tools,
        ["probe_plant_credential"],
        "the call after the plant never reached the ladder"
    );
}

#[tokio::test]
#[ignore = "needs a Pi >= 1.5.0 on this machine and RENO_PI_LIVE=1"]
async fn a_real_pi_behind_the_gate_continues_its_session_after_pi_is_killed() {
    if !live_reno_is_enabled() {
        return;
    }
    let bench = Bench::new();
    let exec = live_reno_executor(&bench);
    let mut live = start(&exec, live_reno_spec(&bench, "script-hold")).await;
    let record = tokio::time::timeout(Duration::from_secs(30), live.records.recv())
        .await
        .expect("pi is reported")
        .expect("a process record");
    let pid = record.pid as i32;

    // Run until the second model call is held, which is after the first call's result.
    let held = bench.dir.path().join("probe-hold-started");
    let mut events = Vec::new();
    let run = async {
        while !held.exists() {
            tokio::select! {
                event = live.stream.next() => {
                    let event = event.expect("the run goes on until Pi is killed");
                    if let ExecEvent::NeedsApproval { request_id, .. } = &event {
                        live.handle
                            .decide_with_message(request_id.clone(), Decision::Allow, None)
                            .await
                            .unwrap();
                    }
                    events.push(event);
                }
                () = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(90), run)
        .await
        .expect("the model call is held");
    let session = events
        .iter()
        .find_map(|event| match event {
            ExecEvent::Started { session, .. } => Some(session.clone()),
            _ => None,
        })
        .expect("a started session");

    assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0, "Pi was alive");
    while let Some(event) = tokio::time::timeout(Duration::from_secs(30), live.stream.next())
        .await
        .expect("the killed run ends")
    {
        events.push(event);
    }
    assert_ne!(done(&events).0, StopReason::Completed);
    assert!(until_gone(pid).await, "Pi was left running");

    // A daemon that started again has no memory of the first attempt.
    let exec = live_reno_executor(&bench);
    let mut resumed = live_reno_spec(&bench, "resume-check");
    resumed.resume = Some(session.clone());
    let events = drive_allowing(&mut start(&exec, resumed).await)
        .await
        .events;
    let (stop, _, diagnostic) = done(&events);
    assert_eq!(stop, StopReason::Completed, "{diagnostic:?}");
    assert!(text(&events).contains("MOCK:resume-check"), "{events:?}");
    assert!(
        events.iter().any(|event| matches!(
            event,
            ExecEvent::Started { session: again, .. } if *again == session
        )),
        "the same session: {events:?}"
    );
    let contexts = probe_lines(&bench, "probe-contexts.jsonl");
    let model_call = contexts
        .iter()
        .find(|line| line["text"] == "resume-check")
        .expect("the resumed prompt reached the model");
    assert!(
        model_call["messages"].as_u64().unwrap() > 1,
        "the model saw the first attempt's history: {contexts:?}"
    );
}

/// A measurement, not a check: it states no bound on the time a call takes to come back.
#[tokio::test]
#[ignore = "needs a Pi >= 1.5.0 on this machine and RENO_PI_LIVE=1"]
async fn a_real_pi_behind_the_gate_reports_how_long_two_hundred_allowed_calls_take() {
    if !live_reno_is_enabled() {
        return;
    }
    let bench = Bench::new();
    let exec = live_reno_executor(&bench);
    let mut live = start(&exec, live_reno_spec(&bench, "script-200")).await;
    let allowed = drive_allowing(&mut live).await;
    let (stop, _, diagnostic) = done(&allowed.events);
    assert_eq!(stop, StopReason::Completed, "{diagnostic:?}");
    assert_eq!(raised(&allowed.events).len(), 200);
    let mut latencies = allowed.latencies;
    assert_eq!(latencies.len(), 200, "every call ended");
    latencies.sort();
    let at = |per_cent: usize| latencies[(latencies.len() - 1) * per_cent / 100];
    eprintln!(
        "NeedsApproval to ToolResult over 200 allowed calls: min {:?}, median {:?}, p95 {:?}, max {:?}",
        latencies[0],
        at(50),
        at(95),
        latencies[latencies.len() - 1]
    );
}

// ---------------------------------------------------------------------------------------------
// The Reno harness's Pi is its own process group, reported at spawn and ended with the run
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn reno_group_pi_leads_its_own_group_and_is_reported_at_spawn() {
    let bench = Bench::new();
    let mut live = start(&reno_executor(&bench), gate_spec(&bench, "ok")).await;
    let record = tokio::time::timeout(Duration::from_secs(10), live.records.recv())
        .await
        .expect("pi is reported")
        .expect("a process record");
    assert!(record.pid > 1);
    assert_eq!(record.pgid, record.pid, "its own process group");
    let pid = record.pid as i32;
    assert_eq!(unsafe { libc::getpgid(pid) }, pid, "the live group leader");
    assert_ne!(
        unsafe { libc::getpgid(0) },
        pid,
        "not the daemon's own group"
    );
    // Only the normal end: which drop ends a dropped run is the next test's concern.
    drive(&mut live).await;
    assert!(until_gone(pid).await, "pi was left running");
}

/// Kills a pid on drop, so a failing test never leaves a tool's process running.
struct KillOnDrop(i32);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        unsafe { libc::kill(self.0, libc::SIGKILL) };
    }
}

#[tokio::test]
async fn reno_group_dropping_the_run_ends_pi_and_what_a_tool_started() {
    let bench = Bench::new();
    let mut spec = gate_spec(&bench, "ok");
    spec.prompt = "scenario:child-hold".to_owned();
    let mut live = start(&reno_executor(&bench), spec).await;
    let record = tokio::time::timeout(Duration::from_secs(10), live.records.recv())
        .await
        .expect("pi is reported")
        .expect("a process record");
    let pid = record.pid as i32;
    let mut found = None;
    for _ in 0..100 {
        found = bench
            .logged()
            .iter()
            .find_map(|entry| entry["child"].as_i64());
        if found.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let grandchild = found.expect("the fake started a tool process") as i32;
    let _cleanup = KillOnDrop(grandchild);
    assert!(alive(pid), "pi runs until the run is dropped");
    assert!(alive(grandchild), "the tool process runs");
    let Live {
        stream,
        handle,
        records,
    } = live;
    drop(stream);
    drop(handle);
    drop(records);
    assert!(until_gone(pid).await, "pi outlived its dropped run");
    assert!(
        until_gone(grandchild).await,
        "a tool's process outlived its dropped run"
    );
}

/// A daemon that stops without winding its runs down aborts their tasks: only the child's own
/// drop is left to end the group.
#[test]
fn reno_group_a_runtime_that_stops_mid_run_still_ends_pi_and_its_tool_process() {
    let bench = Bench::new();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let (live, pid, grandchild) = runtime.block_on(async {
        let mut spec = gate_spec(&bench, "ok");
        spec.prompt = "scenario:child-hold".to_owned();
        let mut live = start(&reno_executor(&bench), spec).await;
        let record = tokio::time::timeout(Duration::from_secs(10), live.records.recv())
            .await
            .expect("pi is reported")
            .expect("a process record");
        let mut found = None;
        for _ in 0..100 {
            found = bench
                .logged()
                .iter()
                .find_map(|entry| entry["child"].as_i64());
            if found.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let grandchild = found.expect("the fake started a tool process") as i32;
        (live, record.pid as i32, grandchild)
    });
    let _cleanup = KillOnDrop(grandchild);
    assert!(alive(pid) && alive(grandchild));
    // Held across the runtime's end so the run's handle is not what ends it.
    std::mem::forget(live);
    drop(runtime);
    let gone = |pid: i32| {
        (0..100).any(|_| {
            if alive(pid) {
                std::thread::sleep(Duration::from_millis(50));
                false
            } else {
                true
            }
        })
    };
    assert!(gone(pid), "pi outlived the stopped runtime");
    assert!(
        gone(grandchild),
        "a tool's process outlived the stopped runtime"
    );
}
