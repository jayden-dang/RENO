//! Pure functions from the state mirror to what Home says: the finding pill, the attention
//! counts, the approval card text. No GPUI, no clock (callers pass `now_ms`), so every case is
//! a plain unit test.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use reno_client::State;
use reno_proto::defs::DefKind;
use reno_proto::{
    ActionClass, Approval, ApprovalSubject, CancelledBy, ReportStatus, RunId, RunStatus,
    RunSummary, StopDetail,
};

/// A run title is `"<bot> · <repo>: <what>"` (the protocol has no bot or repository field yet).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TitleParts<'a> {
    pub(crate) bot: Option<&'a str>,
    pub(crate) repo: Option<&'a str>,
    pub(crate) what: &'a str,
}

pub(crate) fn parse_title(title: &str) -> TitleParts<'_> {
    let plain = TitleParts {
        bot: None,
        repo: None,
        what: title,
    };
    let Some((head, what)) = title.split_once(": ") else {
        return plain;
    };
    match head.split_once(" · ") {
        Some((bot, repo)) => TitleParts {
            bot: Some(bot),
            repo: Some(repo),
            what,
        },
        None => plain,
    }
}

/// The repository a run works on, for the details pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunRepo {
    /// The directory's name.
    pub(crate) name: String,
    /// The whole path, when the daemon gave one.
    pub(crate) path: Option<String>,
}

/// The repository of a run: the directory its task was submitted with; only for a run without
/// one (the fake daemon) what its title names.
pub(crate) fn run_repo(run: &RunSummary) -> Option<RunRepo> {
    run_repo_in(run, reno_client::default_data_dir().as_deref())
}

/// The daemon's working directory for a task that names none: `workspace` in its data
/// directory. The wire has no flag for it, so a run is a scratch run when its directory is this
/// one: exactly `<data dir>/workspace` for the data directory this app would use, else a path
/// ending `Application Support/Reno/workspace` (macOS) or `share/reno/workspace` (XDG), the
/// default data directories, for a daemon this app's environment does not describe. A repository
/// that happens to be called `workspace` elsewhere is not matched.
fn is_scratch_dir(cwd: &Path, data_dir: Option<&Path>) -> bool {
    let cwd: PathBuf = cwd.components().collect();
    if data_dir.is_some_and(|dir| cwd == dir.join("workspace")) {
        return true;
    }
    let tail: Vec<_> = cwd
        .components()
        .rev()
        .take(3)
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    matches!(
        tail.as_slice(),
        [workspace, reno, parent]
            if workspace == "workspace"
                && ((reno == "Reno" && parent == "Application Support")
                    || (reno == "reno" && parent == "share"))
    )
}

fn run_repo_in(run: &RunSummary, data_dir: Option<&Path>) -> Option<RunRepo> {
    if run
        .cwd
        .as_deref()
        .is_some_and(|c| is_scratch_dir(c, data_dir))
    {
        return Some(RunRepo {
            name: "Reno scratch".to_owned(),
            path: None,
        });
    }
    if let Some(cwd) = &run.cwd {
        let name = cwd.file_name().map(|n| n.to_string_lossy().into_owned());
        if let Some(name) = name.filter(|n| !n.is_empty()) {
            let path = cwd.to_string_lossy();
            return Some(RunRepo {
                name,
                path: Some(path.trim_end_matches('/').to_owned()),
            });
        }
    }
    parse_title(&run.title).repo.map(|name| RunRepo {
        name: name.to_owned(),
        path: None,
    })
}

/// The counts on the attention pill and in the sidebar.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Attention {
    /// Held actions waiting for a human.
    pub(crate) need: u32,
    /// Runs waiting on a harness question. The app cannot answer yet, but the person is who the
    /// run waits for, so they count toward [`Attention::needs_you`].
    pub(crate) questions: u32,
    /// Runs that are moving on their own.
    pub(crate) running: u32,
    /// Distinct bots the daemon has defined; a run pinned to a deleted bot does not count. Only
    /// when no definitions are known (the fake daemon defines none) the bots the runs name.
    pub(crate) bots: u32,
}

impl Attention {
    /// Everything a run waits on the person for: held actions and questions.
    pub(crate) const fn needs_you(&self) -> u32 {
        self.need.saturating_add(self.questions)
    }
}

/// The bot a run belongs to: the daemon's `bot` pin; only for a run without one (the fake
/// daemon) the bot its title names.
pub(crate) fn run_bot(run: &RunSummary) -> Option<&str> {
    run.bot
        .as_ref()
        .map(|pin| pin.name.as_str())
        .or_else(|| parse_title(&run.title).bot)
}

/// Whether a run counts as "running": the executor is working or about to be.
pub(crate) fn is_live(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Leased | RunStatus::Running | RunStatus::Evaluating
    )
}

/// The runs waiting on a harness question.
pub(crate) fn waiting_questions(state: &State) -> Vec<&RunSummary> {
    state
        .runs()
        .into_iter()
        .filter(|run| run.status == RunStatus::WaitingInput)
        .collect()
}

pub(crate) fn attention(state: &State) -> Attention {
    let runs = state.runs();
    let mut bots: BTreeSet<&str> = state
        .defs()
        .defs
        .iter()
        .filter(|def| def.kind == DefKind::Bot)
        .map(|def| def.name.as_str())
        .collect();
    if bots.is_empty() {
        // No definitions (the `--fake` demo, or a daemon not yet heard from): the runs are
        // all there is to go on.
        bots = runs.iter().filter_map(|run| run_bot(run)).collect();
    }
    Attention {
        need: count(state.approvals().len()),
        questions: count(
            runs.iter()
                .filter(|run| run.status == RunStatus::WaitingInput)
                .count(),
        ),
        running: count(runs.iter().filter(|run| is_live(run.status)).count()),
        bots: count(bots.len()),
    }
}

fn count(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// "now" for ages: the system clock for a real daemon; for the fake, whose data uses a fixed
/// logical clock, the newest timestamp the state has seen.
pub(crate) fn reference_now(state: &State, system_now_ms: Option<u64>) -> u64 {
    system_now_ms.unwrap_or_else(|| {
        let runs = state.runs().into_iter().map(|r| r.updated_at_ms);
        let approvals = state.approvals().iter().map(|a| a.requested_at_ms);
        runs.chain(approvals).max().unwrap_or(0)
    })
}

/// "6h", "14m", "2d": a short age. Hours are rounded, minutes and days are floored.
pub(crate) fn age_label(age_ms: u64) -> String {
    const MINUTE: u64 = 60_000;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    if age_ms < MINUTE {
        "just now".to_owned()
    } else if age_ms < HOUR - MINUTE / 2 {
        format!("{}m", age_ms / MINUTE)
    } else if age_ms < DAY - HOUR / 2 {
        format!("{}h", (age_ms + HOUR / 2) / HOUR)
    } else {
        format!("{}d", age_ms / DAY)
    }
}

/// "6h ago", "14m ago", or "just now".
pub(crate) fn ago_label(age_ms: u64) -> String {
    match age_label(age_ms).as_str() {
        "just now" => "just now".to_owned(),
        short => format!("{short} ago"),
    }
}

/// "in 18h", "in 40m", or "due now" for a time at or before `now_ms`.
pub(crate) fn until_label(due_ms: u64, now_ms: u64) -> String {
    if due_ms <= now_ms {
        return "due now".to_owned();
    }
    match age_label(due_ms - now_ms).as_str() {
        "just now" => "in under a minute".to_owned(),
        short => format!("in {short}"),
    }
}

/// The state pill's words.
pub(crate) fn status_label(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Queued => "queued",
        RunStatus::Leased => "starting",
        RunStatus::Running => "running",
        RunStatus::WaitingInput => "needs input",
        RunStatus::WaitingApproval => "held for you",
        RunStatus::Evaluating => "evaluating",
        RunStatus::Succeeded => "passed",
        RunStatus::Failed => "failed",
        RunStatus::Interrupted => "interrupted",
        RunStatus::Cancelled => "stopped",
    }
}

/// Whether `run` is interrupted because a person stopped it. The daemon settles such a run as
/// `interrupted` for a moment before it is `cancelled`; to a person it is already stopped, not
/// waiting to resume. Budget, shutdown and crash interruptions are not: those can resume.
pub(crate) fn stopped_by_person(run: &RunSummary) -> bool {
    run.status == RunStatus::Interrupted
        && matches!(
            run.stop,
            Some(StopDetail::Cancelled {
                by: CancelledBy::User
            })
        )
}

/// How a finished run's agent said the work went wrong, when the run itself still counts as
/// passed: the harness completed, but the agent reported `failed` or `blocked`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Problem {
    Failed,
    Blocked,
}

/// The problem the agent reported on a run that `succeeded`. A run that already failed, was
/// stopped or is still going keeps its own state: its pill is already right, or a mid-run
/// report is not yet the outcome.
pub(crate) fn reported_problem(run: &RunSummary) -> Option<Problem> {
    if run.status != RunStatus::Succeeded {
        return None;
    }
    match run.report.as_ref()?.status {
        ReportStatus::Failed => Some(Problem::Failed),
        ReportStatus::Blocked => Some(Problem::Blocked),
        ReportStatus::Done | ReportStatus::Progress => None,
    }
}

/// The pill's words for a run in `status` with an agent-reported `problem`.
pub(crate) fn status_words(status: RunStatus, problem: Option<Problem>) -> &'static str {
    match problem {
        Some(Problem::Failed) => "reported failed",
        Some(Problem::Blocked) => "blocked",
        None => status_label(status),
    }
}

/// The state pill's words for a known run: [`status_label`], except that a person's stop reads
/// "stopping…" while the daemon is still settling it, and a passed run whose agent reported a
/// failure or a block says so instead of "passed".
pub(crate) fn run_status_label(run: &RunSummary) -> &'static str {
    if stopped_by_person(run) {
        "stopping\u{2026}"
    } else {
        status_words(run.status, reported_problem(run))
    }
}

/// The reason in a withdrawn approval's message ("withdrawn: the run was cancelled"), or `None`
/// for any other message: an approval closed because its run ended, not one nobody answered.
pub(crate) fn withdrawn_reason(message: Option<&str>) -> Option<&str> {
    message?.strip_prefix("withdrawn:").map(str::trim)
}

/// A rule that decided an approval, named the way a person would: an unnamed policy rule the
/// daemon already worded (`policy rule 8 (command ...)`) is shown as sent; an older daemon's
/// index (`rules[7]`) is "policy rule 8"; a standing rule the person made
/// (`standing:rule_<id>`) is "your rule"; and a rule with a name of its own is "rule <name>".
/// Anything unrecognised is shown as sent, so a daemon that later sends nicer text still reads.
pub(crate) fn rule_words(name: &str) -> String {
    if name.starts_with("standing:") {
        return "your rule".to_owned();
    }
    if name == "builtin:reno-mcp" {
        return "Reno's own tools".to_owned();
    }
    if name.starts_with("policy rule ") {
        return name.to_owned();
    }
    let index = name
        .strip_prefix("rules[")
        .and_then(|rest| rest.strip_suffix(']'))
        .and_then(|digits| digits.parse::<usize>().ok());
    match index.and_then(|i| i.checked_add(1)) {
        Some(number) => format!("policy rule {number}"),
        None => format!("rule {name}"),
    }
}

/// An executor's display name, for every place the UI names one.
///
/// The wire carries only the id, so the name is made here: the known ones by table, an ACP
/// agent (`acp.grok`, `acp.own.mine`) from the last part of its id ("Grok", "Mine"), and any
/// other id with its words capitalised. The id stays on screen as secondary text.
pub(crate) fn executor_name(id: &str) -> String {
    match id {
        "claude-code" => "Claude Code".to_owned(),
        "codex" => "Codex".to_owned(),
        "muse" => "Muse Code".to_owned(),
        "opencode" => "OpenCode".to_owned(),
        other => {
            let tail = other.strip_prefix("acp.").unwrap_or(other);
            title_case(tail.rsplit('.').next().unwrap_or(tail))
        }
    }
}

/// "gemini-cli" becomes "Gemini Cli".
fn title_case(id: &str) -> String {
    id.split(['-', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            chars.next().map_or_else(String::new, |first| {
                first.to_uppercase().chain(chars).collect()
            })
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// "06:00 UTC": the time of day of a timestamp. The app has no time-zone data yet, so the zone is
/// said out loud.
pub(crate) fn clock_utc(ms: u64) -> String {
    let minutes = ms / 60_000 % (24 * 60);
    format!("{:02}:{:02} UTC", minutes / 60, minutes % 60)
}

/// "6h 26m", "14m 05s", "9s": how long something took.
pub(crate) fn duration_label(ms: u64) -> String {
    let secs = ms / 1_000;
    let (h, m, s) = (secs / 3_600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}h {m:02}m")
    } else if m > 0 {
        format!("{m}m {s:02}s")
    } else {
        format!("{s}s")
    }
}

/// Reads one shell word from all of `text`: quotes (`'..'`, `".."`) and backslashes are removed
/// and adjacent pieces join, as in `'it'\''s'`. `None` when `text` is not exactly one word or a
/// quote is left open.
fn single_shell_word(text: &str) -> Option<String> {
    let mut word = String::new();
    let mut chars = text.trim().chars();
    while let Some(c) = chars.next() {
        match c {
            '\'' => loop {
                match chars.next()? {
                    '\'' => break,
                    inner => word.push(inner),
                }
            },
            '"' => loop {
                match chars.next()? {
                    '"' => break,
                    '\\' => match chars.next()? {
                        escaped @ ('"' | '\\' | '$' | '`') => word.push(escaped),
                        other => {
                            word.push('\\');
                            word.push(other);
                        }
                    },
                    inner => word.push(inner),
                }
            },
            '\\' => word.push(chars.next()?),
            c if c.is_whitespace() => return None,
            c => word.push(c),
        }
    }
    Some(word)
}

/// The command inside a shell wrapper: `/bin/zsh -lc 'touch x'` is `touch x`. Codex runs every
/// command as `<shell> -lc '<command>'`, which would otherwise fill the screen and hide the
/// command. Only zsh, bash and sh with a lone `-c` command are unwrapped; anything else (extra
/// arguments, an unclosed quote, an empty command) comes back as it was.
pub(crate) fn unwrap_shell(command: &str) -> String {
    let unwrapped = (|| {
        let rest = command.trim_start();
        let (shell, mut rest) = rest.split_once(char::is_whitespace)?;
        if !matches!(shell.rsplit('/').next()?, "zsh" | "bash" | "sh") {
            return None;
        }
        // Flags such as `-l`, `-i`, `-lc`; the one holding `c` is followed by the command.
        loop {
            rest = rest.trim_start();
            let (flag, after) = rest.split_once(char::is_whitespace)?;
            let letters = flag.strip_prefix('-')?;
            if letters.is_empty() || !letters.chars().all(|c| c.is_ascii_alphabetic()) {
                return None;
            }
            rest = after;
            if letters.contains('c') {
                break;
            }
        }
        single_shell_word(rest).filter(|inner| !inner.trim().is_empty())
    })();
    unwrapped.unwrap_or_else(|| command.to_owned())
}

/// What kind of action an approval asks for, as one phrase.
pub(crate) fn action_word(subject: &ApprovalSubject) -> &'static str {
    let is_push = subject
        .command
        .as_deref()
        .map(unwrap_shell)
        .is_some_and(|c| c.trim_start().starts_with("git push"));
    match subject.action {
        ActionClass::Command if is_push => "push",
        ActionClass::Command => "command",
        ActionClass::Network => "network request",
        ActionClass::Edit => "edit",
        ActionClass::Read => "read",
        ActionClass::McpTool => "tool call",
        ActionClass::Other => "action",
    }
}

/// The exact thing being approved, one line: the command, or `GET <url>` for a fetch, or the
/// tool name and its raw input.
pub(crate) fn subject_line(subject: &ApprovalSubject) -> String {
    if let Some(command) = &subject.command {
        return unwrap_shell(command);
    }
    if let Some(url) = subject.raw.get("url").and_then(|v| v.as_str()) {
        return format!("GET {url}");
    }
    if subject.raw.is_null() {
        subject.tool.clone()
    } else {
        format!("{} {}", subject.tool, subject.raw)
    }
}

/// The bot that owns an approval's run, when its title names one.
pub(crate) fn approval_bot<'a>(state: &'a State, approval: &Approval) -> Option<&'a str> {
    state.run(&approval.run_id).and_then(run_bot)
}

/// The finding pill's content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Finding {
    /// Something is held: the bold lead, the muted detail, and the run "Open" goes to.
    Held {
        lead: String,
        detail: String,
        run: RunId,
    },
    /// Nothing needs the human.
    Calm { text: String },
}

impl Finding {
    #[cfg(test)]
    /// The whole sentence, for tests and accessibility labels.
    pub(crate) fn sentence(&self) -> String {
        match self {
            Self::Held { lead, detail, .. } => format!("{lead} {detail}"),
            Self::Calm { text } => text.clone(),
        }
    }
}

pub(crate) fn finding(state: &State, now_ms: u64) -> Finding {
    let counts = attention(state);
    let Some(oldest) = state.approvals().first() else {
        if let Some(asking) = state
            .runs()
            .into_iter()
            .filter(|run| run.status == RunStatus::WaitingInput)
            .min_by_key(|run| run.updated_at_ms)
        {
            let lead = match counts.questions {
                1 => "1 question is waiting for you".to_owned(),
                n => format!("{n} questions are waiting for you"),
            };
            let who = run_bot(asking).map_or_else(|| "a run".to_owned(), str::to_owned);
            return Finding::Held {
                lead,
                detail: format!("\u{2014} {who} asks{}.", in_project(asking)),
                run: asking.run_id.clone(),
            };
        }
        let text = match counts.running {
            0 => "Nothing needs you — all quiet.".to_owned(),
            1 => "Nothing needs you — 1 run is moving on its own.".to_owned(),
            n => format!("Nothing needs you — {n} runs are moving on their own."),
        };
        return Finding::Calm { text };
    };
    let lead = if counts.need == 1 {
        "1 action is held for you".to_owned()
    } else {
        format!("{} actions are held for you", counts.need)
    };
    let who = approval_bot(state, oldest)
        .map_or_else(|| "the oldest".to_owned(), |bot| format!("{bot}'s"));
    let what = action_word(&oldest.subject);
    let waited = age_label(now_ms.saturating_sub(oldest.requested_at_ms));
    let place = state
        .run(&oldest.run_id)
        .map_or_else(String::new, in_project);
    let detail = if waited == "just now" {
        format!("— {who} {what}{place} was just held.")
    } else {
        format!("— {who} {what}{place} has waited {waited}.")
    };
    Finding::Held {
        lead,
        detail,
        run: oldest.run_id.clone(),
    }
}

/// " in porch": the folder the run was sent in, by name (no disk read); empty without one.
fn in_project(run: &RunSummary) -> String {
    run.cwd
        .as_deref()
        .and_then(|cwd| cwd.file_name())
        .map_or_else(String::new, |name| {
            format!(" in {}", name.to_string_lossy())
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use reno_client::Update;
    use reno_proto::{ApprovalId, RunId, RunSummary, TaskId};
    use serde_json::json;

    use super::*;

    const HOUR: u64 = 3_600_000;
    const NOW: u64 = 1_000 * HOUR;

    fn run(name: &str, status: RunStatus, title: &str) -> RunSummary {
        RunSummary {
            model_options: Default::default(),
            effort: None,
            run_id: RunId::from_suffix(name).unwrap(),
            task_id: TaskId::from_suffix(name).unwrap(),
            executor: "claude-code".parse().unwrap(),
            status,
            attempt: 1,
            title: title.to_owned(),
            prompt: None,
            model: None,
            stop_reason: None,
            bot: None,
            cwd: None,
            created_at_ms: NOW - HOUR,
            updated_at_ms: NOW - 1000,
            stop: None,
            report: None,
            not_resumed: None,
            session: None,
        }
    }

    #[test]
    fn a_shell_wrapper_is_unwrapped_for_display_and_only_when_it_is_exactly_one() {
        let cases = [
            ("/bin/zsh -lc 'touch dogfood-x.txt'", "touch dogfood-x.txt"),
            ("zsh -c \"git status\"", "git status"),
            ("/usr/bin/bash -lc 'ls -la'", "ls -la"),
            ("sh -c 'echo hi'", "echo hi"),
            ("/bin/zsh -l -c 'pwd'", "pwd"),
            ("  /bin/zsh   -lc   'pwd'  ", "pwd"),
            // Quotes inside: the shell-escaped quote, double quotes within singles, and back.
            (r"/bin/zsh -lc 'echo '\''hi'\'''", "echo 'hi'"),
            (r#"/bin/zsh -lc 'echo "a b"'"#, r#"echo "a b""#),
            (r#"bash -c "echo \"a b\" \$HOME""#, r#"echo "a b" $HOME"#),
            ("/bin/zsh -lc 'a; b | c'", "a; b | c"),
            ("/bin/zsh -lc unquoted", "unquoted"),
        ];
        for (raw, shown) in cases {
            assert_eq!(unwrap_shell(raw), shown, "{raw}");
        }
        // Not a wrapper, or not one this reads with certainty: shown as it came.
        for raw in [
            "git push -u origin x",
            "zsh script.sh",
            "/bin/zsh -lc",
            "/bin/zsh -lc ''",
            "/bin/zsh -lc 'unclosed",
            "/bin/zsh -lc 'a' extra",
            "/bin/fish -c 'x'",
            "/bin/zsh --login -c 'x'",
            "xzsh -c 'x'",
            "",
        ] {
            assert_eq!(unwrap_shell(raw), raw, "{raw}");
        }
    }

    #[test]
    fn a_wrapped_command_shows_inside_but_a_rule_still_names_it_exactly() {
        let wrapped = "/bin/zsh -lc 'git push origin x'";
        let card = approval("w1", "k1", HOUR, Some(wrapped));
        assert_eq!(subject_line(&card.subject), "git push origin x");
        assert_eq!(action_word(&card.subject), "push");
        assert!(
            crate::vm::rules::hint(&card.subject, reno_proto::RememberScope::Run, None)
                .contains(&format!("`{wrapped}`")),
            "the standing rule matches the raw subject"
        );
    }

    #[test]
    fn a_rule_is_named_in_words_a_person_uses() {
        assert_eq!(rule_words("rules[7]"), "policy rule 8");
        assert_eq!(rule_words("rules[0]"), "policy rule 1");
        assert_eq!(rule_words("standing:rule_001a2b"), "your rule");
        assert_eq!(rule_words("standing:"), "your rule");
        // A name someone gave the rule, or a form this code does not know, is shown as it came.
        assert_eq!(rule_words("never reset hard"), "rule never reset hard");
        assert_eq!(rule_words("rules[x]"), "rule rules[x]");
        assert_eq!(rule_words("rules[7]x"), "rule rules[7]x");
        let huge = "rules[99999999999999999999]";
        assert_eq!(rule_words(huge), format!("rule {huge}"));
    }

    #[test]
    fn renos_own_tools_rule_reads_as_renos_own_tools() {
        assert_eq!(rule_words("builtin:reno-mcp"), "Reno's own tools");
    }

    #[test]
    fn a_rule_the_daemon_already_named_is_shown_as_sent() {
        let named = "policy rule 8 (command `git checkout -- Cargo.lock`)";
        assert_eq!(rule_words(named), named);
        assert_eq!(rule_words("policy rule 1"), "policy rule 1");
    }

    #[test]
    fn a_passed_run_whose_agent_reported_failure_or_a_block_does_not_read_as_passed() {
        use reno_proto::{ReportStatus, RunReport};
        let reported = |status: RunStatus, report: ReportStatus| {
            let mut r = run("r", status, "t");
            r.report = Some(RunReport {
                status: report,
                summary: "could not reach the registry".to_owned(),
            });
            r
        };
        let failed = reported(RunStatus::Succeeded, ReportStatus::Failed);
        assert_eq!(reported_problem(&failed), Some(Problem::Failed));
        assert_eq!(run_status_label(&failed), "reported failed");
        let blocked = reported(RunStatus::Succeeded, ReportStatus::Blocked);
        assert_eq!(reported_problem(&blocked), Some(Problem::Blocked));
        assert_eq!(run_status_label(&blocked), "blocked");
        // Done and progress reports, and no report, leave the pill alone.
        for fine in [ReportStatus::Done, ReportStatus::Progress] {
            let r = reported(RunStatus::Succeeded, fine);
            assert_eq!(reported_problem(&r), None);
            assert_eq!(run_status_label(&r), "passed");
        }
        assert_eq!(reported_problem(&run("n", RunStatus::Succeeded, "t")), None);
        // A run that already failed, or was stopped, keeps its own state.
        for status in [RunStatus::Failed, RunStatus::Cancelled, RunStatus::Running] {
            let r = reported(status, ReportStatus::Failed);
            assert_eq!(reported_problem(&r), None, "{status:?}");
            assert_eq!(run_status_label(&r), status_label(status));
        }
    }

    #[test]
    fn a_withdrawn_approval_message_is_told_from_one_a_person_gave() {
        assert_eq!(
            withdrawn_reason(Some("withdrawn: the run was cancelled")),
            Some("the run was cancelled")
        );
        assert_eq!(
            withdrawn_reason(Some("withdrawn: budget spent")),
            Some("budget spent")
        );
        assert_eq!(withdrawn_reason(Some("use the staging bucket")), None);
        assert_eq!(withdrawn_reason(None), None);
    }

    #[test]
    fn only_a_persons_stop_reads_as_stopping() {
        let with = |stop: Option<StopDetail>| RunSummary {
            stop,
            ..run("a", RunStatus::Interrupted, "t")
        };
        let by_user = with(Some(StopDetail::Cancelled {
            by: CancelledBy::User,
        }));
        assert!(stopped_by_person(&by_user));
        assert_eq!(run_status_label(&by_user), "stopping\u{2026}");
        for resumable in [
            None,
            Some(StopDetail::Shutdown),
            Some(StopDetail::Crashed),
            Some(StopDetail::Cancelled {
                by: CancelledBy::Pause,
            }),
        ] {
            let r = with(resumable);
            assert!(!stopped_by_person(&r), "{:?}", r.stop);
            assert_eq!(run_status_label(&r), "interrupted");
        }
        let cancelled = RunSummary {
            status: RunStatus::Cancelled,
            ..by_user
        };
        assert!(!stopped_by_person(&cancelled));
        assert_eq!(run_status_label(&cancelled), "stopped");
    }

    fn approval(name: &str, run_name: &str, age: u64, command: Option<&str>) -> Approval {
        let raw = match command {
            Some(c) => json!({ "command": c }),
            None => json!({ "url": "https://x.example/feed" }),
        };
        Approval {
            id: ApprovalId::from_suffix(name).unwrap(),
            run_id: RunId::from_suffix(run_name).unwrap(),
            subject: ApprovalSubject {
                action: if command.is_some() {
                    ActionClass::Command
                } else {
                    ActionClass::Network
                },
                tool: "bash".to_owned(),
                command: command.map(str::to_owned),
                paths: vec![],
                raw,
            },
            requested_at_ms: NOW - age,
        }
    }

    fn state_with(runs: Vec<RunSummary>, approvals: Vec<Approval>) -> State {
        let mut state = State::new();
        state.apply(Update::Runs(runs));
        state.apply(Update::Approvals(approvals));
        state
    }

    #[test]
    fn title_parts() {
        let p = parse_title("keeper · atlas-api: bump deps");
        assert_eq!(
            (p.bot, p.repo, p.what),
            (Some("keeper"), Some("atlas-api"), "bump deps")
        );
        let p = parse_title("just a title");
        assert_eq!((p.bot, p.repo, p.what), (None, None, "just a title"));
        let p = parse_title("note: no bot");
        assert_eq!((p.bot, p.what), (None, "note: no bot"));
    }

    #[test]
    fn a_run_in_the_daemons_scratch_directory_is_not_a_repository() {
        let in_dir = |cwd: &str, data: Option<&str>| {
            let mut r = run("s1", RunStatus::Running, "Fix the build");
            r.cwd = Some(cwd.into());
            run_repo_in(&r, data.map(Path::new)).expect("a row").name
        };
        let mac = "/Users/x/Library/Application Support/Reno/workspace";
        assert_eq!(in_dir(mac, None), "Reno scratch");
        assert_eq!(in_dir(&format!("{mac}/"), None), "Reno scratch");
        assert_eq!(
            in_dir("/home/x/.local/share/reno/workspace", None),
            "Reno scratch"
        );
        // A data directory this app knows is exact, wherever it is.
        assert_eq!(in_dir("/tmp/r/workspace", Some("/tmp/r")), "Reno scratch");
        // Other folders named workspace, and folders inside the scratch one, are real.
        assert_eq!(in_dir("/Users/x/Developer/workspace", None), "workspace");
        assert_eq!(in_dir("/Users/x/reno/workspace", None), "workspace");
        assert_eq!(in_dir("/tmp/r/workspace", Some("/tmp/other")), "workspace");
        assert_eq!(in_dir(&format!("{mac}/job"), None), "job");
    }

    #[test]
    fn a_real_run_names_its_repository_from_its_working_directory() {
        let mut real = run("r1", RunStatus::Running, "Fix the build");
        real.cwd = Some("/Users/x/Developer/experiments/porch/".into());
        let repo = run_repo(&real).expect("a repository");
        assert_eq!(repo.name, "porch");
        assert_eq!(
            repo.path.as_deref(),
            Some("/Users/x/Developer/experiments/porch")
        );
        // The fake daemon has no working directory: its title says it.
        let demo = run(
            "r2",
            RunStatus::Running,
            "keeper \u{b7} atlas-api: bump deps",
        );
        let repo = run_repo(&demo).expect("a repository");
        assert_eq!((repo.name.as_str(), repo.path), ("atlas-api", None));
        // Neither: no row.
        assert!(run_repo(&run("r3", RunStatus::Running, "plain")).is_none());
        // The directory wins over a title that merely looks like the demo's.
        let mut both = run("r4", RunStatus::Running, "keeper \u{b7} other: x");
        both.cwd = Some("/w/porch".into());
        assert_eq!(run_repo(&both).expect("repo").name, "porch");
    }

    #[test]
    fn relative_labels() {
        assert_eq!(ago_label(5_000), "just now");
        assert_eq!(ago_label(14 * 60_000), "14m ago");
        assert_eq!(until_label(NOW + 18 * HOUR, NOW), "in 18h");
        assert_eq!(until_label(NOW + 40 * 60_000, NOW), "in 40m");
        assert_eq!(until_label(NOW + 10_000, NOW), "in under a minute");
        assert_eq!(until_label(NOW, NOW), "due now");
        assert_eq!(until_label(NOW - 1, NOW), "due now");
    }

    #[test]
    fn age_labels() {
        assert_eq!(age_label(10_000), "just now");
        assert_eq!(age_label(14 * 60_000), "14m");
        assert_eq!(age_label(59 * 60_000 + 40_000), "1h");
        assert_eq!(age_label(5 * HOUR + 55 * 60_000), "6h");
        assert_eq!(age_label(6 * HOUR), "6h");
        assert_eq!(age_label(47 * HOUR), "1d");
        assert_eq!(age_label(50 * HOUR), "2d");
    }

    #[test]
    fn finding_two_held_names_the_oldest() {
        let state = state_with(
            vec![
                run("k1", RunStatus::WaitingApproval, "keeper · atlas-api: bump"),
                run(
                    "s1",
                    RunStatus::WaitingApproval,
                    "scout · web-console: fetch",
                ),
            ],
            vec![
                approval("a1", "k1", 6 * HOUR, Some("git push -u origin x")),
                approval("a2", "s1", 2 * HOUR, None),
            ],
        );
        let f = finding(&state, NOW);
        assert_eq!(
            f.sentence(),
            "2 actions are held for you — keeper's push has waited 6h."
        );
        assert!(matches!(f, Finding::Held { .. }));
    }

    #[test]
    fn finding_names_the_project_and_the_run_to_open() {
        let mut keeper = run("k1", RunStatus::WaitingApproval, "keeper · porch: bump");
        keeper.cwd = Some(std::path::PathBuf::from("/w/porch"));
        let state = state_with(
            vec![keeper],
            vec![approval(
                "a1",
                "k1",
                3 * 60_000,
                Some("git push -u origin HEAD"),
            )],
        );
        let f = finding(&state, NOW);
        assert_eq!(
            f.sentence(),
            "1 action is held for you — keeper's push in porch has waited 3m."
        );
        let Finding::Held { run, .. } = f else {
            panic!("held")
        };
        assert_eq!(run.to_string(), "run_k1");
    }

    #[test]
    fn finding_one_held_is_singular_and_names_the_kind() {
        let state = state_with(
            vec![run("s1", RunStatus::WaitingApproval, "scout · web: fetch")],
            vec![approval("a2", "s1", 14 * 60_000, None)],
        );
        assert_eq!(
            finding(&state, NOW).sentence(),
            "1 action is held for you — scout's network request has waited 14m."
        );
    }

    #[test]
    fn finding_unknown_run_and_fresh_hold() {
        let state = state_with(vec![], vec![approval("a1", "gone", 5_000, Some("ls"))]);
        assert_eq!(
            finding(&state, NOW).sentence(),
            "1 action is held for you — the oldest command was just held."
        );
    }

    #[test]
    fn finding_calm_variants() {
        assert_eq!(
            finding(&State::new(), NOW).sentence(),
            "Nothing needs you — all quiet."
        );
        let one = state_with(
            vec![run("b1", RunStatus::Running, "builder · reno: x")],
            vec![],
        );
        assert_eq!(
            finding(&one, NOW).sentence(),
            "Nothing needs you — 1 run is moving on its own."
        );
        let two = state_with(
            vec![
                run("b1", RunStatus::Running, "builder · reno: x"),
                run("b2", RunStatus::Evaluating, "builder · reno: y"),
                run("b3", RunStatus::Succeeded, "builder · reno: z"),
            ],
            vec![],
        );
        let f = finding(&two, NOW);
        assert_eq!(
            f.sentence(),
            "Nothing needs you — 2 runs are moving on their own."
        );
        assert!(matches!(f, Finding::Calm { .. }));
    }

    fn pinned(name: &str, status: RunStatus, title: &str, bot: &str) -> RunSummary {
        let mut r = run(name, status, title);
        r.bot = Some(reno_proto::defs::BotPin {
            name: bot.to_owned(),
            version: 1,
        });
        r
    }

    #[test]
    fn a_real_run_names_its_bot_from_the_field_not_the_title() {
        let r = pinned(
            "s1",
            RunStatus::Running,
            "dogfood: hello from the app",
            "scout",
        );
        assert_eq!(run_bot(&r), Some("scout"));
        // The pin wins over a title that happens to carry the fake shape.
        let r = pinned("s2", RunStatus::Running, "keeper \u{b7} atlas: x", "scout");
        assert_eq!(run_bot(&r), Some("scout"));
        // No pin: the fake daemon's title shape still names one.
        let r = run("f1", RunStatus::Running, "keeper \u{b7} atlas: x");
        assert_eq!(run_bot(&r), Some("keeper"));
        assert_eq!(
            run_bot(&run("n1", RunStatus::Running, "plain: title")),
            None
        );
    }

    #[test]
    fn attention_bots_are_the_defined_ones_once_definitions_are_known() {
        let mut state = state_with(
            vec![pinned("s1", RunStatus::Running, "dogfood: hi", "scout")],
            vec![],
        );
        assert_eq!(attention(&state).bots, 1);
        let def = |name: &str| reno_proto::defs::DefSummary {
            kind: reno_proto::defs::DefKind::Bot,
            name: name.to_owned(),
            version: 1,
            hash: "0".repeat(64),
            created_at_ms: 0,
        };
        state.apply(Update::Defs(reno_proto::defs::DefsSnapshot {
            defs: vec![def("scout"), def("reviewer"), def("keeper")],
            errors: vec![],
        }));
        // Three defined, one of them already running: three.
        assert_eq!(attention(&state).bots, 3);
        // An old run pinned to a deleted bot does not count.
        state.apply(Update::Runs(vec![
            pinned("s1", RunStatus::Running, "dogfood: hi", "scout"),
            pinned("s2", RunStatus::Succeeded, "old: x", "builder"),
        ]));
        assert_eq!(attention(&state).bots, 3);
    }

    #[test]
    fn approval_bot_uses_the_pin() {
        let state = state_with(
            vec![pinned(
                "s1",
                RunStatus::WaitingApproval,
                "dogfood: hi",
                "scout",
            )],
            vec![approval("a1", "s1", HOUR, Some("ls"))],
        );
        assert_eq!(approval_bot(&state, &state.approvals()[0]), Some("scout"));
    }

    #[test]
    fn a_waiting_question_is_counted_and_not_called_running() {
        let state = state_with(
            vec![
                run(
                    "q1",
                    RunStatus::WaitingInput,
                    "keeper \u{b7} atlas-api: ask",
                ),
                run("b1", RunStatus::Running, "builder \u{b7} reno: b"),
            ],
            vec![],
        );
        let counts = attention(&state);
        assert_eq!((counts.need, counts.questions, counts.running), (0, 1, 1));
        assert_eq!(counts.needs_you(), 1);
        assert_eq!(
            finding(&state, NOW).sentence(),
            "1 question is waiting for you \u{2014} keeper asks."
        );
    }

    #[test]
    fn attention_counts() {
        let state = state_with(
            vec![
                run("k1", RunStatus::WaitingApproval, "keeper · atlas-api: a"),
                run("b1", RunStatus::Running, "builder · reno: b"),
                run("b2", RunStatus::Leased, "builder · reno: c"),
                run("r1", RunStatus::Succeeded, "reviewer · web: d"),
                run("q1", RunStatus::Queued, "no bot here"),
            ],
            vec![approval("a1", "k1", HOUR, Some("git push"))],
        );
        assert_eq!(
            attention(&state),
            Attention {
                need: 1,
                questions: 0,
                running: 2,
                bots: 3
            }
        );
        assert_eq!(attention(&State::new()), Attention::default());
    }

    #[test]
    fn reference_now_uses_the_newest_timestamp_or_the_clock() {
        let state = state_with(
            vec![run("b1", RunStatus::Running, "builder · reno: x")],
            vec![approval("a1", "b1", 6 * HOUR, Some("ls"))],
        );
        assert_eq!(reference_now(&state, None), NOW - 1000);
        assert_eq!(reference_now(&state, Some(42)), 42);
        assert_eq!(reference_now(&State::new(), None), 0);
    }

    #[test]
    fn subject_lines_show_the_exact_action() {
        let push = approval(
            "a1",
            "k1",
            HOUR,
            Some("git push -u origin reno/keeper/deps-1115"),
        );
        assert_eq!(
            subject_line(&push.subject),
            "git push -u origin reno/keeper/deps-1115"
        );
        assert_eq!(action_word(&push.subject), "push");
        let ls = approval("a2", "k1", HOUR, Some("ls -la"));
        assert_eq!(action_word(&ls.subject), "command");
        let net = approval("a3", "s1", HOUR, None);
        assert_eq!(subject_line(&net.subject), "GET https://x.example/feed");
        assert_eq!(action_word(&net.subject), "network request");
    }

    #[test]
    fn executors_are_named_not_shown_as_ids() {
        assert_eq!(executor_name("claude-code"), "Claude Code");
        assert_eq!(executor_name("codex"), "Codex");
        assert_eq!(executor_name("pi"), "Pi");
        assert_eq!(executor_name("muse"), "Muse Code");
        assert_eq!(executor_name("cursor"), "Cursor");
        assert_eq!(executor_name("opencode"), "OpenCode");
        assert_eq!(executor_name("acp.grok"), "Grok");
        assert_eq!(executor_name("acp.own.mine"), "Mine");
        assert_eq!(executor_name("acp.gemini-cli"), "Gemini Cli");
        assert_eq!(executor_name("mock"), "Mock");
    }
}
