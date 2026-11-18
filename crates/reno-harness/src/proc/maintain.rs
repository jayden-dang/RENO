//! The step runner of installs and updates (EX-1, EX-3, EX-6..EX-9).
//!
//! # What runs, and what does not
//!
//! The commands come from the adapters' compiled-in declarations
//! ([`crate::exec::maintenance`]), never from a client. They run as the person, outside the
//! policy engine: nobody asked an agent to do anything, a person pressed a button or turned
//! auto-update on. They never get stdin, a terminal, or a way to ask for a password.
//!
//! # Two layers
//!
//! - [`CommandRunner`] is the thin seam that spawns one command line and reports its lines and
//!   its exit. [`ShellRunner`](super::shell_runner::ShellRunner) is the real one. Tests inject
//!   [`ScriptedRunner`], which plays a script and **panics on any command it was not given**, and
//!   [`NoRealSpawn`], which panics on every command.
//! - [`run_steps`] is the policy over it: the steps in order, the quiet notice (EX-8), the
//!   ceiling (EX-8), the password refusal (EX-9) and the cancel (EX-7). It uses `tokio::time`
//!   only, so tests pause the clock.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use reno_proto::JobFailure;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

/// The most lines of output a job keeps (EX-6).
pub const LOG_LINES: usize = 500;
/// The longest a kept line is, in characters.
pub const LINE_CHARS: usize = 500;
/// Silence after which a job says so and keeps running (EX-8).
pub const QUIET_AFTER: Duration = Duration::from_secs(5 * 60);
/// The longest a job runs before it is killed (EX-8).
pub const CEILING: Duration = Duration::from_secs(30 * 60);

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// It exited with this status.
    Code(i32),
    /// A signal ended it.
    Signal(i32),
}

/// A command that could not be run at all.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct RunnerError(pub String);

/// Spawns one command line. See the module docs.
#[async_trait]
pub trait CommandRunner: Send + Sync + std::fmt::Debug {
    /// Runs `command` as the user, in a login shell, with no stdin and no terminal, and sends
    /// each line it prints (standard output and error, merged) to `out`. Ends when it does.
    ///
    /// **Dropping the returned future kills the command's whole process group**: that is how a
    /// cancel, a timeout and a daemon shutdown stop it, and nothing is left behind.
    async fn run(&self, command: &str, out: mpsc::Sender<String>) -> Result<Exit, RunnerError>;
}

/// The limits of one job; tests shorten them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Silence after which [`Progress::Quiet`] is reported once.
    pub quiet: Duration,
    /// The longest the whole job may run.
    pub ceiling: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            quiet: QUIET_AFTER,
            ceiling: CEILING,
        }
    }
}

/// What a running job reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// Step `n` of `of` starts.
    Step {
        /// From 1.
        n: u32,
        /// How many there are.
        of: u32,
        /// The command line.
        command: String,
    },
    /// A line of output.
    Line(String),
    /// No output for [`Limits::quiet`]. Reported once per silence; the job keeps running.
    Quiet,
}

/// How a job ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Every step exited with status zero.
    Done,
    /// It did not finish.
    Failed {
        /// Why, as a kind.
        failure: JobFailure,
        /// Why, in words.
        detail: String,
    },
}

fn failed(failure: JobFailure, detail: impl Into<String>) -> Outcome {
    Outcome::Failed {
        failure,
        detail: detail.into(),
    }
}

/// Runs `steps` in order, reporting to `progress`. Stops at the first step that fails, a cancel
/// (`cancel`), the ceiling or a password prompt, and kills what runs.
pub async fn run_steps(
    runner: &dyn CommandRunner,
    steps: &[String],
    limits: Limits,
    cancel: &CancellationToken,
    mut progress: impl FnMut(Progress),
) -> Outcome {
    let deadline = Instant::now() + limits.ceiling;
    let total = u32::try_from(steps.len()).unwrap_or(u32::MAX);
    for (at, command) in steps.iter().enumerate() {
        progress(Progress::Step {
            n: u32::try_from(at + 1).unwrap_or(u32::MAX),
            of: total,
            command: command.clone(),
        });
        let (tx, mut lines) = mpsc::channel::<String>(256);
        let mut run = std::pin::pin!(runner.run(command, tx));
        let mut last_line: Option<String> = None;
        let mut quiet_at = Instant::now() + limits.quiet;
        let mut said_quiet = false;
        let mut open = true;
        let exit = loop {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return failed(JobFailure::Cancelled, "cancelled"),
                () = tokio::time::sleep_until(deadline) => {
                    return failed(
                        JobFailure::TimedOut,
                        format!("still running after {} min, so it was stopped", limits.ceiling.as_secs() / 60),
                    );
                }
                line = lines.recv(), if open => match line {
                    Some(line) => {
                        if let Some(refused) = take_line(command, line, &mut last_line, &mut progress) {
                            return refused;
                        }
                        quiet_at = Instant::now() + limits.quiet;
                        said_quiet = false;
                    }
                    None => open = false,
                },
                exit = &mut run => break exit,
                () = tokio::time::sleep_until(quiet_at), if !said_quiet => {
                    said_quiet = true;
                    progress(Progress::Quiet);
                }
            }
        };
        // What the command printed just before it ended.
        while let Ok(line) = lines.try_recv() {
            if let Some(refused) = take_line(command, line, &mut last_line, &mut progress) {
                return refused;
            }
        }
        match exit {
            Ok(Exit::Code(0)) => {}
            Ok(Exit::Code(code)) => {
                return failed(
                    JobFailure::ExitCode,
                    with_last_line(format!("`{command}` exited with status {code}"), &last_line),
                );
            }
            Ok(Exit::Signal(signal)) => {
                return failed(
                    JobFailure::ExitCode,
                    with_last_line(
                        format!("`{command}` was ended by signal {signal}"),
                        &last_line,
                    ),
                );
            }
            Err(error) => return failed(JobFailure::Broken, error.to_string()),
        }
    }
    Outcome::Done
}

fn with_last_line(reason: String, last: &Option<String>) -> String {
    match last {
        Some(line) => format!("{reason}: {line}"),
        None => reason,
    }
}

/// Records one line; `Some` when it is a password prompt, which ends the job.
fn take_line(
    command: &str,
    line: String,
    last: &mut Option<String>,
    progress: &mut impl FnMut(Progress),
) -> Option<Outcome> {
    if asks_for_password(&line) {
        return Some(failed(
            JobFailure::NeedsPassword,
            format!(
                "`{command}` asked for a password (sudo). Reno never types a password for you: \
                 run it yourself in Terminal."
            ),
        ));
    }
    *last = Some(line.clone());
    progress(Progress::Line(line));
    None
}

/// What sudo and Homebrew print when they need a password, lower case. UNVERIFIED against a real
/// refusal on a Mac: the strings are sudo's and Homebrew's documented messages.
const PASSWORD_HINTS: &[&str] = &[
    "[sudo] password",
    "sudo: a password is required",
    "sudo: a terminal is required",
    "sudo: no tty present",
    "need sudo access",
];

/// Whether `line` is a password prompt or sudo's refusal to ask for one.
pub fn asks_for_password(line: &str) -> bool {
    let lower = line.trim().to_lowercase();
    lower.ends_with("password:") || PASSWORD_HINTS.iter().any(|hint| lower.contains(hint))
}

/// One line of output as it is kept: escape sequences gone, only what follows the last carriage
/// return (a progress bar redraws itself), trimmed and bounded. `None` for an empty line.
pub fn clean_line(raw: &str) -> Option<String> {
    let last = raw.rsplit('\r').find(|part| !part.trim().is_empty())?;
    let mut out = String::with_capacity(last.len().min(LINE_CHARS * 4));
    let mut chars = last.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI: ESC [ ... final byte; OSC and the rest: ESC then one character.
            match chars.next() {
                Some('[') => {
                    for next in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&next) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    for next in chars.by_ref() {
                        if next == '\u{7}' {
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if !c.is_control() || c == '\t' {
            out.push(c);
        }
    }
    let out = out.trim();
    if out.is_empty() {
        return None;
    }
    Some(out.chars().take(LINE_CHARS).collect())
}

/// The tail of a job's output: the last [`LOG_LINES`] lines, and whether older ones were dropped.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LogTail {
    lines: VecDeque<String>,
    truncated: bool,
}

impl LogTail {
    /// Keeps `line`, dropping the oldest when there are more than [`LOG_LINES`].
    pub fn push(&mut self, line: String) {
        if self.lines.len() == LOG_LINES {
            self.lines.pop_front();
            self.truncated = true;
        }
        self.lines.push_back(line);
    }

    /// The lines, oldest first, and whether any older ones were dropped.
    pub fn snapshot(&self) -> (Vec<String>, bool) {
        (self.lines.iter().cloned().collect(), self.truncated)
    }
}

/// A runner for tests that must never spawn anything: it panics on every command.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoRealSpawn;

#[async_trait]
impl CommandRunner for NoRealSpawn {
    async fn run(&self, command: &str, _out: mpsc::Sender<String>) -> Result<Exit, RunnerError> {
        panic!("a test tried to run a real command: {command}");
    }
}

/// One thing a scripted command does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Beat {
    /// Prints a line.
    Line(String),
    /// Lets this much (tokio) time pass.
    Wait(Duration),
    /// Ends with this status.
    Exit(i32),
    /// Ends by this signal.
    Signal(i32),
    /// Cannot be started.
    CannotStart(String),
}

/// A runner that plays scripts, for tests. A command with no script left **panics**, so a test
/// can never reach a real process through it. A script that ends without an `Exit` runs for
/// ever (until it is cancelled, which drops it).
#[derive(Debug, Default, Clone)]
pub struct ScriptedRunner {
    inner: Arc<Mutex<Scripts>>,
}

#[derive(Debug, Default)]
struct Scripts {
    scripts: HashMap<String, VecDeque<Vec<Beat>>>,
    started: Vec<String>,
    stopped: Vec<String>,
}

impl ScriptedRunner {
    /// An empty runner.
    pub fn new() -> Self {
        Self::default()
    }

    /// Plays `beats` the next time `command` runs (once; add more for more runs).
    #[must_use]
    pub fn script(self, command: &str, beats: Vec<Beat>) -> Self {
        self.lock()
            .scripts
            .entry(command.to_owned())
            .or_default()
            .push_back(beats);
        self
    }

    /// The commands started so far, in order.
    pub fn started(&self) -> Vec<String> {
        self.lock().started.clone()
    }

    /// The commands whose run was dropped before it ended: what a real runner would have killed.
    pub fn stopped(&self) -> Vec<String> {
        self.lock().stopped.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Scripts> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

struct StopGuard {
    inner: Arc<Mutex<Scripts>>,
    command: String,
    ended: bool,
}

impl Drop for StopGuard {
    fn drop(&mut self) {
        if !self.ended {
            self.inner
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .stopped
                .push(self.command.clone());
        }
    }
}

#[async_trait]
impl CommandRunner for ScriptedRunner {
    async fn run(&self, command: &str, out: mpsc::Sender<String>) -> Result<Exit, RunnerError> {
        let beats = {
            let mut scripts = self.lock();
            let beats = scripts
                .scripts
                .get_mut(command)
                .and_then(VecDeque::pop_front);
            let Some(beats) = beats else {
                drop(scripts);
                panic!("a test tried to run a command it did not script: {command}");
            };
            scripts.started.push(command.to_owned());
            beats
        };
        let mut guard = StopGuard {
            inner: Arc::clone(&self.inner),
            command: command.to_owned(),
            ended: false,
        };
        for beat in beats {
            match beat {
                Beat::Line(line) => {
                    let _ = out.send(line).await;
                }
                Beat::Wait(time) => tokio::time::sleep(time).await,
                Beat::Exit(code) => {
                    guard.ended = true;
                    return Ok(Exit::Code(code));
                }
                Beat::Signal(signal) => {
                    guard.ended = true;
                    return Ok(Exit::Signal(signal));
                }
                Beat::CannotStart(why) => {
                    guard.ended = true;
                    return Err(RunnerError(why));
                }
            }
        }
        std::future::pending().await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const SECOND: Duration = Duration::from_secs(1);

    fn steps(commands: &[&str]) -> Vec<String> {
        commands.iter().map(|c| (*c).to_owned()).collect()
    }

    async fn run(
        runner: &ScriptedRunner,
        commands: &[&str],
        cancel: &CancellationToken,
    ) -> (Outcome, Vec<Progress>) {
        let mut seen = Vec::new();
        let outcome = run_steps(runner, &steps(commands), Limits::default(), cancel, |p| {
            seen.push(p)
        })
        .await;
        (outcome, seen)
    }

    #[tokio::test(start_paused = true)]
    async fn steps_run_in_order_and_report_their_lines() {
        let runner = ScriptedRunner::new()
            .script(
                "one",
                vec![Beat::Line("a".into()), Beat::Wait(SECOND), Beat::Exit(0)],
            )
            .script("two", vec![Beat::Line("b".into()), Beat::Exit(0)]);
        let (outcome, seen) = run(&runner, &["one", "two"], &CancellationToken::new()).await;
        assert_eq!(outcome, Outcome::Done);
        assert_eq!(
            seen,
            [
                Progress::Step {
                    n: 1,
                    of: 2,
                    command: "one".into()
                },
                Progress::Line("a".into()),
                Progress::Step {
                    n: 2,
                    of: 2,
                    command: "two".into()
                },
                Progress::Line("b".into()),
            ]
        );
        assert_eq!(runner.started(), ["one", "two"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failing_step_stops_the_job_and_says_why_in_words() {
        let runner = ScriptedRunner::new().script(
            "one",
            vec![
                Beat::Line("curl: (6) could not resolve".into()),
                Beat::Exit(6),
            ],
        );
        let (outcome, _) = run(&runner, &["one", "never"], &CancellationToken::new()).await;
        let Outcome::Failed { failure, detail } = outcome else {
            panic!("expected a failure");
        };
        assert_eq!(failure, JobFailure::ExitCode);
        assert_eq!(
            detail,
            "`one` exited with status 6: curl: (6) could not resolve"
        );
        // The second step was never scripted: it would have panicked had it started.
        assert_eq!(runner.started(), ["one"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_password_prompt_fails_at_once_and_kills_the_step() {
        for prompt in [
            "Password:",
            "[sudo] password for jayden:",
            "sudo: a terminal is required to read the password",
            "==> Need sudo access on macOS (e.g. the user jayden needs to be an Administrator)!",
        ] {
            let runner = ScriptedRunner::new().script(
                "brew install",
                vec![
                    Beat::Line(prompt.into()),
                    Beat::Wait(Duration::from_secs(3600)),
                ],
            );
            let (outcome, _) = run(&runner, &["brew install"], &CancellationToken::new()).await;
            let Outcome::Failed { failure, .. } = outcome else {
                panic!("expected a failure for {prompt}");
            };
            assert_eq!(failure, JobFailure::NeedsPassword, "{prompt}");
            assert_eq!(runner.stopped(), ["brew install"]);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancel_kills_the_step_and_ends_the_job_cancelled() {
        let runner = ScriptedRunner::new().script("long", vec![Beat::Line("x".into())]);
        let cancel = CancellationToken::new();
        let trigger = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(SECOND).await;
            trigger.cancel();
        });
        let (outcome, _) = run(&runner, &["long"], &cancel).await;
        assert_eq!(outcome, failed(JobFailure::Cancelled, "cancelled"));
        assert_eq!(runner.stopped(), ["long"]);
    }

    #[tokio::test(start_paused = true)]
    async fn five_minutes_of_silence_is_reported_once_and_the_job_keeps_running() {
        let runner = ScriptedRunner::new().script(
            "quiet",
            vec![
                Beat::Line("start".into()),
                Beat::Wait(Duration::from_secs(10 * 60)),
                Beat::Line("late".into()),
                Beat::Exit(0),
            ],
        );
        let (outcome, seen) = run(&runner, &["quiet"], &CancellationToken::new()).await;
        assert_eq!(outcome, Outcome::Done);
        let quiet: Vec<_> = seen.iter().filter(|p| **p == Progress::Quiet).collect();
        assert_eq!(quiet.len(), 1);
        // The notice comes between the two lines, and the late line still arrives.
        let at = seen.iter().position(|p| *p == Progress::Quiet).unwrap();
        assert_eq!(seen[at - 1], Progress::Line("start".into()));
        assert_eq!(seen[at + 1], Progress::Line("late".into()));
    }

    #[tokio::test(start_paused = true)]
    async fn output_resets_the_quiet_clock() {
        let beats: Vec<Beat> = (0..4)
            .flat_map(|n| {
                [
                    Beat::Wait(Duration::from_secs(4 * 60)),
                    Beat::Line(format!("tick {n}")),
                ]
            })
            .chain([Beat::Exit(0)])
            .collect();
        let runner = ScriptedRunner::new().script("steady", beats);
        let (_, seen) = run(&runner, &["steady"], &CancellationToken::new()).await;
        assert!(!seen.contains(&Progress::Quiet));
    }

    #[tokio::test(start_paused = true)]
    async fn the_ceiling_kills_a_job_that_never_ends() {
        let runner = ScriptedRunner::new().script("hung", vec![Beat::Line("x".into())]);
        let (outcome, _) = run(&runner, &["hung"], &CancellationToken::new()).await;
        let Outcome::Failed { failure, detail } = outcome else {
            panic!("expected a failure");
        };
        assert_eq!(failure, JobFailure::TimedOut);
        assert!(detail.contains("30 min"), "{detail}");
        assert_eq!(runner.stopped(), ["hung"]);
    }

    #[tokio::test(start_paused = true)]
    async fn the_ceiling_counts_the_whole_job_not_each_step() {
        let wait = Duration::from_secs(20 * 60);
        let runner = ScriptedRunner::new()
            .script("a", vec![Beat::Wait(wait), Beat::Exit(0)])
            .script("b", vec![Beat::Wait(wait), Beat::Exit(0)]);
        let (outcome, _) = run(&runner, &["a", "b"], &CancellationToken::new()).await;
        assert!(matches!(
            outcome,
            Outcome::Failed {
                failure: JobFailure::TimedOut,
                ..
            }
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn a_command_that_cannot_start_is_broken_not_failed() {
        let runner = ScriptedRunner::new()
            .script("x", vec![Beat::CannotStart("no shell".into())])
            .script("y", vec![Beat::Signal(9)]);
        let (outcome, _) = run(&runner, &["x"], &CancellationToken::new()).await;
        assert_eq!(outcome, failed(JobFailure::Broken, "no shell"));
        let (outcome, _) = run(&runner, &["y"], &CancellationToken::new()).await;
        assert_eq!(
            outcome,
            failed(JobFailure::ExitCode, "`y` was ended by signal 9")
        );
    }

    #[test]
    #[should_panic(expected = "did not script")]
    fn an_unscripted_command_panics() {
        let runner = ScriptedRunner::new();
        let (tx, _rx) = mpsc::channel(1);
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let _ = rt.block_on(runner.run("brew upgrade", tx));
    }

    #[test]
    fn lines_are_cleaned_and_bounded() {
        assert_eq!(
            clean_line("\u{1b}[1;32m==>\u{1b}[0m Downloading").as_deref(),
            Some("==> Downloading")
        );
        // A progress bar redraws with carriage returns: the last drawing is the line.
        assert_eq!(
            clean_line("10%\r50%\r100% done\r").as_deref(),
            Some("100% done")
        );
        assert_eq!(clean_line("   \r\n"), None);
        assert_eq!(
            clean_line(&"x".repeat(5000)).unwrap().chars().count(),
            LINE_CHARS
        );
    }

    #[test]
    fn the_log_keeps_the_last_five_hundred_lines() {
        let mut log = LogTail::default();
        for n in 0..LOG_LINES + 3 {
            log.push(format!("line {n}"));
        }
        let (lines, truncated) = log.snapshot();
        assert!(truncated);
        assert_eq!(lines.len(), LOG_LINES);
        assert_eq!(lines[0], "line 3");
        assert_eq!(lines[LOG_LINES - 1], format!("line {}", LOG_LINES + 2));
    }
}
