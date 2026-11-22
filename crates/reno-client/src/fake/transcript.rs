//! ROAD-33: the fake daemon's transcript demo world, behind [`FakeConfig::transcript_threads`]
//! (super::FakeConfig::transcript_threads).
//!
//! - "fix the ACP v2 handshake": one thread of eight finished runs, one run per turn, 2,000 agent
//!   messages in all (250 a run, each followed by a tool call and its result, as the bench's
//!   burst reads). Each turn is a prompt, a reasoning note, 250 steps (interim text, a tool call
//!   and its result) and a final reply, so each finished turn folds into one "Worked for" fold. The steps vary (searches, reads, edits, builds,
//!   test runs). The file name [`NEEDLE`] occurs exactly [`NEEDLE_HITS`] times in the thread and
//!   its first occurrence is in turn 3, inside a `cargo test` step's output.
//! - "Render check": one finished run whose final reply holds a display formula, an inline
//!   formula, a Mermaid flowchart, a code block, a worktree image, a remote image, a file link,
//!   an inline path and a callout.
//! - [`long_reply`]: the chunks a streaming run says for the prompt `/longreply`.
//! - The extra files of the demo tree those replies point at ([`FILES`]): the image
//!   `docs/graph.png` and `src/supervisor.rs`.

use reno_proto::{ExecEvent, StopReason};
use serde_json::json;

use super::data::{
    self, HOUR, Ids, MINUTE, SeedRun, done, summary, text, tool, tool_ok, tool_output,
};
use super::world::World;

/// The file name the search demo looks for.
pub const NEEDLE: &str = "supervisor.rs";
/// How many times [`NEEDLE`] occurs in the handshake thread.
pub const NEEDLE_HITS: usize = 7;
/// The turn (1-based) whose `cargo test` step holds the first occurrence.
pub const NEEDLE_FIRST_TURN: usize = 3;
/// Turns in the handshake thread.
pub const TURNS: usize = 8;
/// Steps in a turn.
const STEPS: usize = 250;
/// Events in a run: `Started`, one reasoning note, three a step, the final reply and `Done`.
pub const EVENTS_PER_RUN: usize = 1 + 1 + 3 * STEPS + 1 + 1;

/// The handshake thread's runs, first to last; the first one names the thread.
pub const HANDSHAKE_RUNS: [&str; TURNS] = [
    "acp1", "acp2", "acp3", "acp4", "acp5", "acp6", "acp7", "acp8",
];
/// The Render check run.
pub const RENDER_RUN: &str = "render1";

const FIRST_TITLE: &str = "builder \u{b7} reno: fix the ACP v2 handshake";
/// What the seeded turns took, in seconds.
const TOOK: [u64; TURNS] = [192, 341, 543, 267, 178, 375, 450, 130];

const PROMPTS: [&str; TURNS] = [
    "The ACP handshake test fails with a version mismatch against the v2 agents. Find out why the client never settles on a version and fix it.",
    "Add tests for the version negotiation: an agent that offers only v1, one that offers v2, and one that offers none.",
    "The harness tests still hang when a session shuts down. Find what keeps the child process open.",
    "Make the shutdown wait bounded, with a clear error when the child ignores the signal.",
    "Log the negotiated version once per session, at info level, and keep it out of the trace spans.",
    "Update the ACP notes in docs to say what v2 changes for harness authors.",
    "Run the whole workspace once more and clear any warnings this change introduced. Check supervisor.rs for the same unbounded wait.",
    "Write the summary for the pull request description.",
];

const FINALS: [&str; TURNS] = [
    "The client sent version 1 and never read the agent's counter-offer. It now takes the lower of the two versions and fails with a named error when they share none.",
    "The negotiation has tests for an agent that offers only v1, one that offers v2 and one that offers nothing; all three pass.",
    "The hang is a shutdown that waits on the child with no limit. A child that ignores the signal holds the session open for good, and the failing test shows it.",
    "The wait is bounded now: the session sends the signal, waits the grace period, then kills the child and reports which step timed out.",
    "The negotiated version is logged once at info level when the session starts, and the spans carry no version field.",
    "The ACP notes say what v2 changes: the counter-offer, the named error and the bounded shutdown.",
    "The workspace builds clean and the tests pass. The same unbounded wait in supervisor.rs was already covered by the bounded shutdown.",
    "Summary for the pull request: the client negotiates the lower common version, fails with a named error when there is none, logs the version once and bounds the shutdown wait in supervisor.rs.",
];

const FILES_POOL: [&str; 10] = [
    "handshake.rs",
    "negotiate.rs",
    "session.rs",
    "transport.rs",
    "framing.rs",
    "version.rs",
    "client.rs",
    "shutdown.rs",
    "error.rs",
    "config.rs",
];

const NOTES: [&str; 12] = [
    "Reading {f} to see where the offered version is used.",
    "Searching for the other callers of the negotiation before I change {f}.",
    "The next check is {f}: it holds the state the handshake leaves behind.",
    "Building again to see whether the change in {f} still compiles.",
    "Running the harness tests that touch {f}.",
    "{f} looks right so far; looking at how its errors reach the caller.",
    "Comparing {f} against what the v2 notes say a client must do.",
    "A small edit to {f}, then another build.",
    "Checking the diff so far, {f} first.",
    "The error in {f} is only reported, never handled; tracing where it ends up.",
    "Going through {f} again with the new version in mind.",
    "Nothing else in {f} depends on the old constant.",
];

fn pool(n: usize) -> &'static str {
    FILES_POOL[n % FILES_POOL.len()]
}

fn path(file: &str) -> String {
    format!("crates/reno-harness/src/acp/{file}")
}

/// The needle's own path in the demo repository.
fn needle_path() -> String {
    path(NEEDLE)
}

/// One step: the interim text, the call and its result.
fn step(turn: usize, n: usize) -> [ExecEvent; 3] {
    let f = pool(turn * 3 + n);
    let note = NOTES[(turn * 5 + n * 7) % NOTES.len()].replace("{f}", f);
    let id = format!("s{n}");
    // The turn-3 test run that shows where the hang is, and what follows it.
    if turn == NEEDLE_FIRST_TURN && n == 30 {
        let mut out = String::from("running 31 tests\n");
        for i in 0..28 {
            out.push_str(&format!("test acp::handshake::tests::case_{i:02} ... ok\n"));
        }
        out.push_str(
            "test acp::shutdown::tests::child_exits_on_signal ... ok\n\
test acp::shutdown::tests::child_ignores_signal ... FAILED\n\
test acp::shutdown::tests::child_exits_by_itself ... ok\n\n\
failures:\n\n\
---- acp::shutdown::tests::child_ignores_signal stdout ----\n",
        );
        out.push_str(&format!(
            "thread 'acp::shutdown::tests::child_ignores_signal' panicked at {}:118:9:\n\
timed out waiting for the child to exit\n\n\
failures:\n    acp::shutdown::tests::child_ignores_signal\n\n\
test result: FAILED. 30 passed; 1 failed; 0 ignored; finished in 60.02s\n",
            needle_path()
        ));
        return [
            text("Running the shutdown tests with the change in place."),
            tool(
                &id,
                "bash",
                json!({ "command": "cargo test -p reno-harness acp::" }),
            ),
            tool_output(&id, false, "1 test failed", &out),
        ];
    }
    if turn == NEEDLE_FIRST_TURN && n == 31 {
        return [
            text(&format!(
                "The failure is in {NEEDLE}: it waits on the child with no limit, so a child that ignores the signal holds the test for a minute."
            )),
            tool(&id, "edit", json!({ "file_path": needle_path() })),
            tool_ok(&id, "ok"),
        ];
    }
    if turn == 4 && n == 12 {
        return [
            text("Reading the wait loop before I bound it."),
            tool(&id, "read", json!({ "file_path": needle_path() })),
            tool_ok(&id, "96 lines"),
        ];
    }
    let (call, result) = match (turn * 7 + n) % 8 {
        0 => {
            let pattern = ["handshake", "negotiate", "protocol_version", "grace"][n % 4];
            let hits = 2 + (turn + n) % 5;
            let out: String = (0..hits)
                .map(|i| {
                    format!(
                        "{}:{}:    {pattern}(&mut self)\n",
                        path(pool(turn + n + i)),
                        10 + (n * 3 + i * 17) % 180
                    )
                })
                .collect();
            (
                tool(
                    &id,
                    "bash",
                    json!({ "command": format!("rg -n \"{pattern}\" crates/reno-harness/src") }),
                ),
                tool_output(&id, true, &format!("{hits} matches"), &out),
            )
        }
        1 | 6 => (
            tool(&id, "read", json!({ "file_path": path(f) })),
            tool_ok(&id, &format!("{} lines", 40 + (turn * 31 + n * 13) % 220)),
        ),
        2 => (
            tool(&id, "edit", json!({ "file_path": path(f) })),
            tool_ok(&id, "ok"),
        ),
        3 => {
            let crates = 3 + (turn + n) % 4;
            let mut out: String = (0..crates)
                .map(|i| format!("   Compiling reno-part{i} v0.4.{}\n", turn + i))
                .collect();
            out.push_str("   Compiling reno-harness v0.4.1\n    Finished `dev` profile in ");
            out.push_str(&format!("{}.{}s\n", 4 + n % 9, n % 10));
            (
                tool(
                    &id,
                    "bash",
                    json!({ "command": "cargo build -p reno-harness" }),
                ),
                tool_output(&id, true, "built", &out),
            )
        }
        4 => {
            let passed = 20 + (turn * 3 + n) % 12;
            let mut out = format!("running {passed} tests\n");
            for i in 0..4 {
                out.push_str(&format!(
                    "test acp::{}::tests::case_{:02} ... ok\n",
                    pool(n + i).trim_end_matches(".rs"),
                    n + i
                ));
            }
            out.push_str(&format!("\ntest result: ok. {passed} passed; 0 failed\n"));
            (
                tool(
                    &id,
                    "bash",
                    json!({ "command": "cargo test -p reno-harness" }),
                ),
                tool_output(&id, true, &format!("{passed} passed"), &out),
            )
        }
        5 => (
            tool(&id, "bash", json!({ "command": "git diff --stat" })),
            tool_output(
                &id,
                true,
                "2 files changed",
                &format!(
                    " {} | {} +++--\n {} | {} ++-\n 2 files changed, 9 insertions(+), 4 deletions(-)\n",
                    path(f),
                    3 + n % 9,
                    path(pool(n + 1)),
                    2 + n % 5
                ),
            ),
        ),
        _ => (
            tool(
                &id,
                "bash",
                json!({ "command": "cargo clippy -p reno-harness -- -D warnings" }),
            ),
            tool_ok(&id, "no warnings"),
        ),
    };
    [text(&note), call, result]
}

/// The journal of one turn's run.
fn journal(turn: usize) -> Vec<ExecEvent> {
    let mut events = Vec::with_capacity(EVENTS_PER_RUN);
    events.push(data::started());
    events.push(ExecEvent::Reasoning {
        text: format!(
            "Turn {turn}: keep the change small, check each claim against the code before I make it, and run the tests that touch it."
        ),
    });
    for n in 1..=STEPS {
        events.extend(step(turn, n));
    }
    events.push(text(FINALS[turn - 1]));
    events.push(done(StopReason::Completed, None, None));
    events
}

/// The handshake thread's runs, then the Render check run.
pub(super) fn runs() -> Ids<Vec<SeedRun>> {
    let mut runs = Vec::new();
    for (at, name) in HANDSHAKE_RUNS.iter().enumerate() {
        let turn = at + 1;
        let age = (TURNS as u64 - at as u64 + 1) * 2 * HOUR;
        let title = if turn == 1 {
            FIRST_TITLE.to_owned()
        } else {
            PROMPTS[at].to_owned()
        };
        let mut run = summary(
            name,
            "claude-code",
            reno_proto::RunStatus::Succeeded,
            Some(StopReason::Completed),
            &title,
            age,
            age - TOOK[at] * 1000,
        )?;
        run.prompt = Some(PROMPTS[at].to_owned());
        run.cwd = Some(std::path::PathBuf::from("/demo/reno"));
        run.session = Some(reno_proto::SessionRef("sess-acp".to_owned()));
        runs.push(SeedRun {
            summary: run,
            journal: journal(turn),
        });
    }
    let mut render = summary(
        RENDER_RUN,
        "claude-code",
        reno_proto::RunStatus::Succeeded,
        Some(StopReason::Completed),
        "builder \u{b7} reno: Render check",
        3 * HOUR,
        3 * HOUR - 4 * MINUTE,
    )?;
    render.prompt = Some("Show every kind of content the transcript has to draw.".to_owned());
    render.cwd = Some(std::path::PathBuf::from("/demo/reno"));
    runs.push(SeedRun {
        summary: render,
        journal: vec![
            data::started(),
            text(RENDER_REPLY),
            done(StopReason::Completed, None, None),
        ],
    });
    Ok(runs)
}

/// Makes the handshake runs one thread, each following the one before.
pub(super) fn link_thread(world: &mut World) {
    world.chain_thread(&HANDSHAKE_RUNS);
}

/// The Render check run's final reply.
pub const RENDER_REPLY: &str = r#"Here is every kind of content the transcript draws.

A display formula:

$$\int_0^1 x^2\,dx = \tfrac13$$

And an inline one: the area under $x^2$ on the unit interval.

```mermaid
flowchart LR
    A[Client] -->|hello v2| B[Handshake]
    B -->|offer| C{Agent}
    C -->|counter v1| D[Negotiate]
    D -->|agreed| E[Session]
```

The negotiation in Rust:

```rust
fn negotiate(ours: u32, theirs: Option<u32>) -> Result<u32, HandshakeError> {
    let theirs = theirs.ok_or(HandshakeError::NoCounterOffer)?;
    let agreed = ours.min(theirs);
    if agreed == 0 {
        return Err(HandshakeError::NoCommonVersion { ours, theirs });
    }
    Ok(agreed)
}
```

The build graph from the worktree:

![build graph](docs/graph.png)

And one from the network:

![status](https://example.com/status.png)

The wait is in [supervisor](src/supervisor.rs:42), at `src/supervisor.rs:42`.

> [!NOTE]
> The remote image is shown as a link, and nothing is fetched.
"#;

/// The demo tree's extra files when [`FakeConfig::transcript_threads`] is on, as
/// `(path, bytes)`.
pub(super) fn files() -> Vec<(&'static str, Vec<u8>)> {
    vec![
        ("docs/graph.png", GRAPH_PNG.to_vec()),
        ("src/supervisor.rs", SUPERVISOR.as_bytes().to_vec()),
    ]
}

/// A 96 by 48 PNG of four boxes joined by lines, the build graph the Render check shows.
const GRAPH_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x60, 0x00, 0x00, 0x00, 0x30, 0x08, 0x02, 0x00, 0x00, 0x00, 0x61, 0x74, 0xe8,
    0x98, 0x00, 0x00, 0x00, 0x9c, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0xed, 0xda, 0x31, 0x0e, 0x40,
    0x30, 0x14, 0x06, 0xe0, 0xde, 0xc0, 0xfd, 0xcc, 0x66, 0xa3, 0x53, 0x3a, 0x88, 0xc1, 0x28, 0x62,
    0x94, 0x38, 0xc0, 0x8b, 0x92, 0x22, 0xfa, 0x25, 0xff, 0xdc, 0xf2, 0xb5, 0xcb, 0x9f, 0xbe, 0xb4,
    0xac, 0x9b, 0x04, 0x49, 0x08, 0x00, 0x01, 0x02, 0x04, 0x08, 0xd0, 0x91, 0x66, 0xe8, 0xaf, 0xa5,
    0x22, 0xa0, 0x71, 0x9e, 0x72, 0x03, 0x08, 0x10, 0x20, 0x40, 0x80, 0xbe, 0x0d, 0xd4, 0x76, 0x67,
    0xe3, 0x06, 0x45, 0x01, 0x04, 0x08, 0x10, 0xa0, 0x0a, 0x80, 0xca, 0x35, 0xa6, 0xd2, 0x5d, 0xec,
    0xf2, 0xfa, 0xc1, 0x5e, 0xe9, 0xc9, 0x73, 0x2e, 0x71, 0xc2, 0xf7, 0xbf, 0x3c, 0xfe, 0x0b, 0x40,
    0x80, 0x00, 0x01, 0xfa, 0x0b, 0xd0, 0x2b, 0x5d, 0xc9, 0x0d, 0x02, 0x04, 0x08, 0x10, 0x20, 0x40,
    0xba, 0x58, 0x46, 0x17, 0xd3, 0xe6, 0x01, 0x01, 0x02, 0x04, 0x08, 0x90, 0x77, 0x31, 0x2f, 0xab,
    0x80, 0x00, 0x01, 0x02, 0x64, 0x46, 0xd1, 0x94, 0xab, 0x31, 0x60, 0x40, 0x80, 0x00, 0xd5, 0x9b,
    0x1d, 0x39, 0xdc, 0xd4, 0x71, 0xea, 0x87, 0x71, 0x0c, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e,
    0x44, 0xae, 0x42, 0x60, 0x82,
];

/// The file the replies link to: about 60 lines, the wait at line 42.
const SUPERVISOR: &str = "//! Runs the agent child process and ends it when the session closes.
//! One supervisor per session; it is dropped when the session ends.

use std::process::Child;
use std::time::Duration;

/// How long a child gets to leave after the signal.
const GRACE: Duration = Duration::from_secs(5);

/// What went wrong ending a child.
#[derive(Debug)]
pub enum ShutdownError {
    /// The child was still there after the grace period.
    TimedOut,
    /// The system refused the signal.
    Signal(std::io::Error),
}

/// Owns the child of one session.
pub struct Supervisor {
    child: Child,
    name: String,
}

impl Supervisor {
    pub fn new(child: Child, name: impl Into<String>) -> Self {
        Self {
            child,
            name: name.into(),
        }
    }

    /// Whether the child has already left.
    pub fn exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }

    /// Asks the child to leave and waits for it.
    pub fn shutdown(&mut self) -> Result<(), ShutdownError> {
        self.child.kill().map_err(ShutdownError::Signal)?;
        // Waits as long as the child takes: there is no limit yet.
        self.child.wait().map_err(ShutdownError::Signal)?;
        let _ = GRACE;
        Ok(())
    }

    /// Ends the child at once.
    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }

    /// How many bytes the child has written so far.
    pub fn written(&self) -> u64 {
        0
    }
}
";

/// What `/longreply` says: several thousand words with headings, code blocks and math, as the
/// chunks a model would stream. Each chunk is appended to the one message, so a chunk ending
/// a paragraph ends in a blank line and a code block or a display formula is one chunk.
pub(super) fn long_reply() -> Vec<String> {
    const SUBJECTS: [&str; 8] = [
        "The client",
        "A session",
        "The negotiation",
        "The transport layer",
        "Each frame",
        "The agent",
        "The error path",
        "Every retry",
    ];
    const VERBS: [&str; 8] = [
        "keeps",
        "reports",
        "waits for",
        "hands back",
        "checks",
        "records",
        "defers",
        "reuses",
    ];
    const OBJECTS: [&str; 8] = [
        "the version both sides offered",
        "a named error with the step that failed",
        "the counter-offer from the other side",
        "the state the handshake leaves behind",
        "a bounded wait instead of an open one",
        "the bytes it has read so far",
        "one line in the log for the whole session",
        "the connection it was given",
    ];
    const CLAUSES: [&str; 8] = [
        "so the next step starts from what is known",
        "because a silent failure is harder to find than a loud one",
        "which keeps the tests short and the output readable",
        "and nothing is retried that could change the answer",
        "so a person reading the log sees the same story twice",
        "even when the other side closes the connection early",
        "while the rest of the session carries on undisturbed",
        "and the result is the same on every run of the suite",
    ];
    const TOPICS: [&str; 12] = [
        "Why the version is negotiated",
        "What the counter-offer carries",
        "How a session starts",
        "Where the wait is bounded",
        "What an error says",
        "How frames are cut",
        "Why logs are quiet",
        "What the tests pin down",
        "How the transport closes",
        "What changes for authors",
        "What stays the same",
        "What is left to do",
    ];
    let mut seed = 7usize;
    let mut next = |n: usize| {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345) % 2_147_483_648;
        (seed >> 8) % n
    };
    let mut chunks = Vec::new();
    for (section, topic) in TOPICS.iter().enumerate() {
        chunks.push(format!("## {}. {topic}\n\n", section + 1));
        for paragraph in 0..4 {
            for _ in 0..6 {
                chunks.push(format!(
                    "{} {} {} {}. ",
                    SUBJECTS[next(8)],
                    VERBS[next(8)],
                    OBJECTS[next(8)],
                    CLAUSES[next(8)]
                ));
            }
            chunks.push("\n\n".to_owned());
            if paragraph == 1 {
                chunks.push(format!(
                    "```rust\nfn step_{section}(input: &[u8]) -> Result<usize, Error> {{\n    let head = input.get(..4).ok_or(Error::Short)?;\n    let len = u32::from_be_bytes(head.try_into().map_err(|_| Error::Short)?);\n    Ok(len as usize + {section})\n}}\n```\n\n"
                ));
            }
            if paragraph == 2 {
                chunks.push(format!(
                    "The cost of step {section} grows with the input, as\n\n$$T_{{{section}}}(n) = \\sum_{{k=1}}^{{n}} \\frac{{k}}{{{}}} = \\frac{{n(n+1)}}{{{}}}$$\n\nso a doubling of the input is a little more than a doubling of the time, and $T_{{{section}}}(1) = \\tfrac1{{{}}}$ is the floor. ",
                    section + 2,
                    2 * (section + 2),
                    section + 2
                ));
                chunks.push("\n\n".to_owned());
            }
        }
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every piece of text an event holds, in the order it is read.
    fn texts(events: &[ExecEvent]) -> Vec<String> {
        events
            .iter()
            .map(|e| serde_json::to_string(e).unwrap_or_default())
            .collect()
    }

    fn handshake() -> Vec<SeedRun> {
        let mut all = runs().unwrap();
        all.truncate(TURNS);
        all
    }

    #[test]
    fn the_handshake_thread_has_2000_messages_in_8_turns() {
        let runs = handshake();
        assert_eq!(runs.len(), TURNS);
        let mut messages = 0;
        for run in &runs {
            assert_eq!(run.journal.len(), EVENTS_PER_RUN);
            assert!(matches!(run.journal.last(), Some(ExecEvent::Done { .. })));
            // An agent message that a tool call follows is one of the 2,000; the final reply is
            // followed by Done.
            messages += run
                .journal
                .windows(2)
                .filter(|w| {
                    matches!(w[0], ExecEvent::Text { .. }) && matches!(w[1], ExecEvent::Tool { .. })
                })
                .count();
        }
        assert_eq!(messages, 2_000);
        assert_eq!(EVENTS_PER_RUN, 754);
    }

    #[test]
    fn the_steps_vary_and_every_output_is_under_the_cut() {
        let runs = handshake();
        let mut commands = std::collections::BTreeSet::new();
        for event in runs.iter().flat_map(|r| &r.journal) {
            match event {
                ExecEvent::ToolResult {
                    output: Some(output),
                    ..
                } => assert!(output.text.len() < reno_proto::ToolOutput::MAX_BYTES),
                ExecEvent::Tool { call, .. } => {
                    commands.insert(call.name.clone());
                }
                _ => {}
            }
        }
        assert!(commands.len() >= 3, "{commands:?}");
    }

    #[test]
    fn the_needle_occurs_seven_times_and_first_in_turn_3s_cargo_test() {
        let runs = handshake();
        let mut hits = Vec::new();
        for (at, run) in runs.iter().enumerate() {
            // The prompt is read before the journal.
            let mut parts = vec![run.summary.prompt.clone().unwrap_or_default()];
            parts.extend(texts(&run.journal));
            for (n, part) in parts.iter().enumerate() {
                for _ in part.matches(NEEDLE) {
                    hits.push((at + 1, n));
                }
            }
        }
        assert_eq!(hits.len(), NEEDLE_HITS, "{hits:?}");
        let (turn, event) = hits[0];
        assert_eq!(turn, NEEDLE_FIRST_TURN);
        // Part 0 is the prompt, so the journal index is one less.
        let journal = &runs[turn - 1].journal;
        let ExecEvent::ToolResult { id, output, .. } = &journal[event - 1] else {
            panic!(
                "the first occurrence is not in a tool result: {:?}",
                journal[event - 1]
            );
        };
        assert!(output.as_ref().is_some_and(|o| o.text.contains(NEEDLE)));
        let command = journal.iter().find_map(|e| match e {
            ExecEvent::Tool { id: tool_id, call } if tool_id == id => {
                call.input["command"].as_str().map(str::to_owned)
            }
            _ => None,
        });
        assert!(command.is_some_and(|c| c.starts_with("cargo test")));
    }

    #[test]
    fn the_render_check_reply_has_every_kind_of_content() {
        for want in [
            "$$\\int_0^1 x^2\\,dx = \\tfrac13$$",
            "$x^2$",
            "```mermaid\nflowchart LR",
            "```rust",
            "![build graph](docs/graph.png)",
            "![status](https://example.com/status.png)",
            "[supervisor](src/supervisor.rs:42)",
            "`src/supervisor.rs:42`",
            "> [!NOTE]",
        ] {
            assert!(RENDER_REPLY.contains(want), "missing {want}");
        }
        let flow = RENDER_REPLY
            .split("```mermaid")
            .nth(1)
            .and_then(|s| s.split("```").next())
            .unwrap_or_default();
        assert_eq!(flow.matches("-->|").count(), 4, "four labelled edges");
        assert_eq!(runs().unwrap().len(), TURNS + 1);
    }

    #[test]
    fn the_demo_tree_serves_the_image_and_the_linked_file() {
        let files = files();
        let png = &files
            .iter()
            .find(|(p, _)| *p == "docs/graph.png")
            .unwrap()
            .1;
        assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n");
        assert!(png.ends_with(&[0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82]));
        let source = &files
            .iter()
            .find(|(p, _)| *p == "src/supervisor.rs")
            .unwrap()
            .1;
        let text = String::from_utf8_lossy(source);
        assert!(text.lines().count() >= 42);
        // The link points at the unbounded wait.
        assert!(
            text.lines()
                .nth(41)
                .is_some_and(|l| l.contains("child.wait()"))
        );
    }

    #[test]
    fn the_long_reply_is_thousands_of_words_with_balanced_code_and_math() {
        let chunks = long_reply();
        let whole: String = chunks.concat();
        assert!(whole.split_whitespace().count() > 3_000);
        assert_eq!(whole.matches("```").count() % 2, 0);
        assert_eq!(whole.matches("$$").count() % 2, 0);
        assert!(whole.contains("```rust") && whole.contains("$$"));
        assert!(chunks.len() > 200, "it streams in many steps");
        // A code block or a formula is never cut between chunks.
        for chunk in &chunks {
            assert_eq!(chunk.matches("```").count() % 2, 0);
            assert_eq!(chunk.matches("$$").count() % 2, 0);
        }
    }
}
