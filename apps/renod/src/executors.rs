//! The executors the daemon registers (the `Executor` port, ARCH-7).
//!
//! - `mock` is always there: a deterministic script for the CLI, the app and the tests. By
//!   default it plays a short canned run and finishes. With `hold` it stops after its first text
//!   and waits to be steered or cancelled, which gives tests and demos a run that stays live
//!   without any timer.
//! - `claude-code` is the native [`ClaudeExecutor`], `codex` the native [`CodexExecutor`], `pi`
//!   the native [`PiExecutor`], `reno` the same driver governed by Reno, `muse` the native [`MuseExecutor`], `cursor` the native
//!   [`CursorExecutor`] and `opencode` the native [`OpencodeExecutor`]; all are always registered, found or not: the kernel probes them (installed, version, signed in)
//!   and refuses a run on one that cannot run. Nothing here starts them.
//! - The agents built on ACP are native [`AcpExecutor`]s, one per agent, each under its own id:
//!   `acp.grok`, `acp.devin`, `acp.hermes`, `acp.fx`, `acp.antigravity`, and `acp.<registry id>`
//!   for each agent added from the ACP registry. They are always registered, like the
//!   others. A registry agent never replaces an executor that is already registered: the
//!   registry lists agents Reno drives itself (`devin`, ...), and its record must not take over
//!   their id.

use std::path::Path;
use std::sync::Arc;

use reno_harness::RegistryAgent;
use reno_harness::acp_registry::Record;
use reno_harness::exec::{Executor, MockExecutor, MockStep};
use reno_harness::usage::{ClaudeUsage, CodexUsage};
use reno_harness::{
    AcpExecutor, ClaudeExecutor, CodexExecutor, CursorExecutor, MuseExecutor, OpencodeExecutor,
    PiExecutor,
};
use reno_kernel::ExecutorRegistry;
use reno_proto::{ActionClass, ExecEvent, ExecutorId, IdError, StopReason, ToolCall};
use serde_json::json;

/// The id of the always-present mock executor.
pub const MOCK_ID: &str = "mock";
/// The id of the Claude Code executor.
pub const CLAUDE_ID: &str = reno_harness::claude::EXECUTOR_ID;
/// The id of the Codex executor.
pub const CODEX_ID: &str = reno_harness::codex::EXECUTOR_ID;
/// The id of the Pi executor.
pub const PI_ID: &str = reno_harness::pi::EXECUTOR_ID;
/// The id of the Reno-governed Pi executor.
pub const RENO_ID: &str = reno_harness::pi::RENO_EXECUTOR_ID;
/// The id of the Muse Code executor.
pub const MUSE_ID: &str = reno_harness::muse::EXECUTOR_ID;
/// The id of the Cursor executor.
pub const CURSOR_ID: &str = reno_harness::cursor::EXECUTOR_ID;
/// The id of the opencode executor.
pub const OPENCODE_ID: &str = reno_harness::opencode::EXECUTOR_ID;

/// The model the mock reports.
pub const MOCK_MODEL: &str = "mock-1";

/// The registry the kernel gets, and the harness executors the daemon must wait for at shutdown.
#[derive(Default)]
pub struct ExecutorSet {
    /// Executors by id.
    pub registry: ExecutorRegistry,
    /// The harness executors in the registry, so shutdown can wait for their tasks.
    pub harnesses: Vec<Arc<dyn Executor>>,
}

impl std::fmt::Debug for ExecutorSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExecutorSet")
            .field("registry", &self.registry)
            .field("harnesses", &self.harnesses.len())
            .finish()
    }
}

impl ExecutorSet {
    /// A set holding only `registry`, for tests that bring their own executors.
    pub fn from_registry(registry: ExecutorRegistry) -> Self {
        Self {
            registry,
            harnesses: Vec::new(),
        }
    }
}

/// The mock executor: the canned script, or the hold script when `hold` is set. It keeps no log
/// of what it received, which would grow with every run the daemon plays.
pub fn mock_executor(hold: bool) -> Result<MockExecutor, IdError> {
    mock_with(mock_script(hold))
}

fn mock_with(steps: Vec<MockStep>) -> Result<MockExecutor, IdError> {
    Ok(MockExecutor::new(MOCK_ID.parse()?)
        .with_steps(steps)
        .without_log())
}

/// The mock's script.
///
/// Canned: `Started`, three text deltas, a tool call and its result, a last text, `Done`.
/// Hold: `Started` and one text, then wait for a steering prompt (or a cancel, which ends the
/// run `interrupted`), then the same ending.
pub fn mock_script(hold: bool) -> Vec<MockStep> {
    let mut steps = vec![
        MockStep::started("mock-session", MOCK_MODEL),
        MockStep::text("Hello from the mock executor. "),
    ];
    if hold {
        steps.push(MockStep::AwaitSteer);
    }
    steps.extend([
        MockStep::text("I am reading the task "),
        MockStep::text("and pretending to work on it.\n"),
        MockStep::Emit(ExecEvent::Tool {
            id: "mock-call-1".into(),
            call: ToolCall::new("mock_tool", json!({ "note": "canned" }), ActionClass::Other),
        }),
        MockStep::Emit(ExecEvent::ToolResult {
            id: "mock-call-1".into(),
            ok: true,
            summary: Some("the mock tool finished".into()),
            output: None,
        }),
        MockStep::text("All done."),
        MockStep::Emit(ExecEvent::Done {
            stop: StopReason::Completed,
            result: Some("mock run complete".into()),
            diagnostic: None,
            reason: None,
        }),
    ]);
    steps
}

/// A bench-only script for the mock (`renod --stream-mock`), which the app benchmark plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamMock {
    /// Text deltas at a steady rate, about [`STREAM_EVENTS_PER_SECOND`] a second for
    /// [`STREAM_SECONDS`] seconds: markdown with code blocks and a few tool steps. It is the
    /// load of a run being watched.
    Steady,
    /// [`BURST_MESSAGES`] messages, each followed by a tool step, with no waiting: a long
    /// finished run to open.
    Burst,
}

/// The rate of [`StreamMock::Steady`].
pub const STREAM_EVENTS_PER_SECOND: u64 = 20;
/// How long [`StreamMock::Steady`] plays.
pub const STREAM_SECONDS: u64 = 60;
/// How many messages [`StreamMock::Burst`] plays.
pub const BURST_MESSAGES: usize = 2_000;
/// A tool step comes this many events apart in [`StreamMock::Steady`].
const STREAM_TOOL_EVERY: u64 = 200;

/// What the streaming mock says, over and over. Every code block closes inside one pass.
const STREAM_MARKDOWN: &str = "## Reading the change\n\nI am going through the module **one function at a time**, checking each against `the spec`.\n\n- the parser keeps its state in one struct\n- errors carry the offset where they began\n- nothing here touches the clock\n\n```rust\nfn offset(input: &str, at: usize) -> Option<usize> {\n    let rest = input.get(at..)?;\n    rest.find('\\n').map(|n| at + n)\n}\n```\n\nThat is the part to keep. The next step is to run the tests again.\n\n";

/// The script of a [`StreamMock`].
pub fn stream_script(kind: StreamMock) -> Vec<MockStep> {
    let mut steps = vec![MockStep::started("mock-session", MOCK_MODEL)];
    match kind {
        StreamMock::Steady => {
            let events = STREAM_EVENTS_PER_SECOND * STREAM_SECONDS;
            let gap = std::time::Duration::from_millis(1000 / STREAM_EVENTS_PER_SECOND);
            let words: Vec<&str> = STREAM_MARKDOWN.split_inclusive(' ').collect();
            let mut word = 0;
            for n in 1..=events {
                steps.push(MockStep::Sleep(gap));
                if n % STREAM_TOOL_EVERY == 0 {
                    let id = format!("stream-call-{n}");
                    steps.push(stream_tool(&id));
                    steps.push(stream_tool_result(&id));
                } else {
                    steps.push(MockStep::text(words[word % words.len()]));
                    word += 1;
                }
            }
        }
        StreamMock::Burst => {
            for n in 0..BURST_MESSAGES {
                steps.push(MockStep::text(format!(
                    "Message {n}: I looked at `item_{n}` and it reads as expected. \
                     Nothing to change here, so on to the next.\n"
                )));
                let id = format!("burst-call-{n}");
                steps.push(stream_tool(&id));
                steps.push(stream_tool_result(&id));
            }
        }
    }
    steps.push(MockStep::Emit(ExecEvent::Done {
        stop: StopReason::Completed,
        result: Some("stream mock complete".into()),
        diagnostic: None,
        reason: None,
    }));
    steps
}

fn stream_tool(id: &str) -> MockStep {
    MockStep::Emit(ExecEvent::Tool {
        id: id.into(),
        call: ToolCall::new("mock_tool", json!({ "note": "stream" }), ActionClass::Other),
    })
}

fn stream_tool_result(id: &str) -> MockStep {
    MockStep::Emit(ExecEvent::ToolResult {
        id: id.into(),
        ok: true,
        summary: Some("the mock tool finished".into()),
        output: None,
    })
}

/// Claude Code, reading the plan usage of the login the CLI holds (read only, see
/// `reno_harness::usage`).
fn claude() -> ClaudeExecutor {
    let executor = ClaudeExecutor::new();
    match ClaudeUsage::from_env() {
        Some(usage) => executor.with_usage_source(Arc::new(usage)),
        None => executor,
    }
}

/// Codex, likewise.
fn codex() -> CodexExecutor {
    let executor = CodexExecutor::new();
    match CodexUsage::from_env() {
        Some(usage) => executor.with_usage_source(Arc::new(usage)),
        None => executor,
    }
}

/// Every harness executor Reno drives itself, built and not probed. `reno` keeps its Pi sessions
/// under the data directory's `harness/.reno/pi-sessions`.
pub fn harness_candidates(data_dir: &Path) -> Vec<Arc<dyn Executor>> {
    vec![
        Arc::new(claude()),
        Arc::new(codex()),
        Arc::new(PiExecutor::new()),
        Arc::new(PiExecutor::reno(data_dir.join("harness/.reno/pi-sessions"))),
        Arc::new(MuseExecutor::new()),
        Arc::new(CursorExecutor::new()),
        Arc::new(OpencodeExecutor::new()),
        Arc::new(AcpExecutor::grok()),
        Arc::new(AcpExecutor::devin()),
        Arc::new(AcpExecutor::hermes()),
        Arc::new(AcpExecutor::fx()),
        Arc::new(AcpExecutor::antigravity()),
    ]
}

/// Builds the registry: `mock`, plus every harness Reno can drive, installed or not. Nothing is
/// probed or started here: the kernel probes each executor when the daemon starts, and an
/// executor that is not installed, too old or signed out stays listed (with the reason) and is
/// refused a run before anything is spawned. With `harnesses` false only `mock` is registered
/// and no CLI is looked for, let alone started.
pub fn discover(
    hold_mock: bool,
    stream_mock: Option<StreamMock>,
    harnesses: bool,
    data_dir: &Path,
) -> Result<ExecutorSet, IdError> {
    let mock = match stream_mock {
        Some(kind) => mock_with(stream_script(kind))?,
        None => mock_executor(hold_mock)?,
    };
    let mut set = ExecutorSet {
        registry: ExecutorRegistry::new().with(Arc::new(mock)),
        harnesses: Vec::new(),
    };
    if !harnesses {
        return Ok(set);
    }
    let mut candidates = harness_candidates(data_dir);
    let taken: Vec<ExecutorId> = candidates
        .iter()
        .map(|executor| executor.descriptor().id)
        .chain([MOCK_ID.parse()?])
        .collect();
    candidates.extend(
        registry_executors(reno_harness::acp_registry::records(), &taken)
            .into_iter()
            .map(|executor| Arc::new(executor) as Arc<dyn Executor>),
    );
    for executor in candidates {
        set.registry
            .insert(executor.descriptor().id, Arc::clone(&executor));
        set.harnesses.push(executor);
    }
    Ok(set)
}

/// One executor for each registry agent this device has added, except those whose id is already
/// `taken` (the executors Reno drives itself, `mock`): a registry record must never replace an
/// executor that is registered under the same id.
fn registry_executors(records: Vec<Record>, taken: &[ExecutorId]) -> Vec<AcpExecutor> {
    records
        .into_iter()
        .filter_map(|record| {
            // The registry also lists agents Reno drives itself (`claude-acp`, `codex-acp`,
            // `devin`, ...): they are never added from it, so a record for one is not an agent
            // of its own.
            if reno_harness::acp_registry::built_in(&record.id).is_some() {
                tracing::warn!(agent = %record.id, "the registry id of an executor Reno drives itself; skipped");
                return None;
            }
            let Some(agent) = RegistryAgent::new(&record.id) else {
                tracing::warn!(agent = %record.id, "not a registry agent id; skipped");
                return None;
            };
            let executor = match AcpExecutor::registry(agent, &record.name) {
                Ok(executor) => executor,
                Err(error) => {
                    tracing::warn!(agent = %record.id, %error, "no executor id for the agent; skipped");
                    return None;
                }
            };
            let id = executor.descriptor().id;
            if taken.contains(&id) {
                tracing::warn!(agent = %record.id, executor = %id, "an executor already has this id; the registry agent is skipped");
                return None;
            }
            Some(executor)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use reno_proto::ExecutorId;

    use super::*;

    #[test]
    fn built_in_ids_are_valid() {
        for name in [
            MOCK_ID,
            CLAUDE_ID,
            CODEX_ID,
            PI_ID,
            MUSE_ID,
            CURSOR_ID,
            OPENCODE_ID,
            RENO_ID,
        ] {
            assert_eq!(name.parse::<ExecutorId>().expect("valid").as_str(), name);
        }
    }

    #[test]
    fn harness_candidates_hold_one_pi_and_one_reno() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ids: Vec<String> = harness_candidates(dir.path())
            .iter()
            .map(|executor| executor.descriptor().id.as_str().to_owned())
            .collect();
        assert_eq!(ids.iter().filter(|id| *id == PI_ID).count(), 1);
        assert_eq!(ids.iter().filter(|id| *id == RENO_ID).count(), 1);
        assert_eq!(RENO_ID, "reno");
        assert!(
            !dir.path().join("harness").exists(),
            "construction writes nothing"
        );
    }

    #[test]
    fn without_harnesses_only_the_mock_is_registered_and_nothing_is_probed() {
        let set = discover(
            false,
            None,
            false,
            std::path::Path::new("/nonexistent-data-dir"),
        )
        .expect("discover");
        let ids = set.registry.ids();
        let ids: Vec<_> = ids.iter().map(ExecutorId::as_str).collect();
        assert_eq!(ids, [MOCK_ID]);
        assert!(set.harnesses.is_empty());
    }

    fn record(id: &str) -> Record {
        serde_json::from_value(json!({
            "id": id, "name": id, "version": "1.0.0", "kind": "binary", "program": "/bin/true",
        }))
        .expect("a record")
    }

    #[test]
    fn built_in_acp_ids_are_valid_and_distinct() {
        let ids: Vec<ExecutorId> = [
            AcpExecutor::grok(),
            AcpExecutor::devin(),
            AcpExecutor::hermes(),
            AcpExecutor::fx(),
            AcpExecutor::antigravity(),
        ]
        .iter()
        .map(|executor| executor.descriptor().id)
        .collect();
        let names: Vec<&str> = ids.iter().map(ExecutorId::as_str).collect();
        assert_eq!(
            names,
            [
                "acp.grok",
                "acp.devin",
                "acp.hermes",
                "acp.fx",
                "acp.antigravity"
            ]
        );
    }

    #[test]
    fn a_registry_agent_never_replaces_an_executor_with_its_id() {
        let taken: Vec<ExecutorId> = ["mock", "claude-code", "acp.grok", "acp.devin"]
            .iter()
            .map(|id| id.parse().expect("id"))
            .collect();
        let executors = registry_executors(
            vec![
                record("grok"),
                record("devin"),
                record("gemini"),
                record("own.mine"),
            ],
            &taken,
        );
        let ids: Vec<String> = executors
            .iter()
            .map(|executor| executor.descriptor().id.to_string())
            .collect();
        assert_eq!(ids, ["acp.gemini", "acp.own.mine"]);
    }

    /// Counts of what a script plays and how long it sleeps.
    fn shape(script: &[MockStep]) -> (usize, usize, usize, std::time::Duration) {
        let mut texts = 0;
        let mut tools = 0;
        let mut dones = 0;
        let mut slept = std::time::Duration::ZERO;
        for step in script {
            match step {
                MockStep::Emit(ExecEvent::Text { .. }) => texts += 1,
                MockStep::Emit(ExecEvent::Tool { .. }) => tools += 1,
                MockStep::Emit(ExecEvent::Done { .. }) => dones += 1,
                MockStep::Sleep(d) => slept += *d,
                _ => {}
            }
        }
        (texts, tools, dones, slept)
    }

    #[test]
    fn the_steady_stream_plays_about_twenty_events_a_second_for_a_minute() {
        let script = stream_script(StreamMock::Steady);
        let (texts, tools, dones, slept) = shape(&script);
        assert_eq!(dones, 1);
        assert!(matches!(
            script.last(),
            Some(MockStep::Emit(ExecEvent::Done { .. }))
        ));
        assert!((3..=8).contains(&tools), "a few tool steps, got {tools}");
        let events = texts + tools;
        assert!((1100..=1300).contains(&events), "events: {events}");
        let seconds = slept.as_secs_f64();
        assert!((55.0..=65.0).contains(&seconds), "sleeps {seconds} s");
        let rate = events as f64 / seconds;
        assert!((18.0..=22.0).contains(&rate), "rate {rate}/s");
    }

    #[test]
    fn the_steady_stream_is_markdown_with_whole_code_blocks() {
        let script = stream_script(StreamMock::Steady);
        let text: String = script
            .iter()
            .filter_map(|s| match s {
                MockStep::Emit(ExecEvent::Text { text }) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        let fences = text.matches("```").count();
        assert!(fences >= 2, "has code blocks");
        assert_eq!(fences % 2, 0, "every block is closed");
        assert!(text.contains("\n- "), "has a list");
        assert!(text.contains("\n## "), "has headings");
    }

    #[test]
    fn the_burst_is_two_thousand_messages_split_by_tool_steps_with_no_waiting() {
        let script = stream_script(StreamMock::Burst);
        let (texts, tools, dones, slept) = shape(&script);
        assert_eq!(texts, BURST_MESSAGES);
        assert_eq!(tools, BURST_MESSAGES, "a tool step after each message");
        assert_eq!(dones, 1);
        assert_eq!(slept, std::time::Duration::ZERO);
    }

    #[test]
    fn both_scripts_end_with_exactly_one_done() {
        for hold in [false, true] {
            let script = mock_script(hold);
            let dones = script
                .iter()
                .filter(|s| matches!(s, MockStep::Emit(ExecEvent::Done { .. })))
                .count();
            assert_eq!(dones, 1);
            assert!(matches!(
                script.last(),
                Some(MockStep::Emit(ExecEvent::Done { .. }))
            ));
            assert_eq!(
                script.iter().any(|s| matches!(s, MockStep::AwaitSteer)),
                hold
            );
        }
    }
}
