//! `PiExecutor` integration tests against the fake `pi --mode rpc` in `tests/fixtures/fake-pi.py`
//! (no real `pi` binary, no model, no network, no Pi settings directory involved). The live test
//! against a real Pi is at the bottom, `#[ignore]`d.
//!
//! The fake plays the scenario a prompt names (`scenario:<name>`) with the frame shapes of Pi
//! 1.5.0, and appends every argument vector and command frame it receives to `FAKE_PI_LOG`, which
//! is how these tests see what the driver sent.

#![cfg(unix)]
// Tests unwrap freely, and probe a leaked pid with `kill(pid, 0)`.
#![allow(clippy::unwrap_used, clippy::expect_used, unsafe_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use futures::StreamExt;
use futures::stream::BoxStream;
use reno_harness::exec::{ExecControls, ExecError, ExecHandle, Executor};
use reno_harness::{PiExecutor, ProcessRecord};
use reno_proto::{
    AccessMode, ActionClass, Effort, ExecEvent, ExecSpec, InstallStatus, McpServerSpec,
    ModelSource, NotInstalledReason, RunId, SandboxLevel, SessionRef, SignedIn, Steering,
    StopReason, VersionGate,
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
