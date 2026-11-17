//! Pure functions from the state mirror to what Home says: the finding pill, the attention
//! counts, the approval card text. No GPUI, no clock (callers pass `now_ms`), so every case is
//! a plain unit test.

use std::collections::BTreeSet;

use reno_client::State;
use reno_proto::defs::DefKind;
use reno_proto::{
    ActionClass, Approval, ApprovalSubject, CancelledBy, RunStatus, RunSummary, StopDetail,
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
    /// Runs that are moving on their own.
    pub(crate) running: u32,
    /// Distinct bots the daemon has defined; a run pinned to a deleted bot does not count. Only
    /// when no definitions are known (the fake daemon defines none) the bots the runs name.
    pub(crate) bots: u32,
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

/// The state pill's words for a known run: [`status_label`], except that a person's stop reads
/// "stopping…" while the daemon is still settling it.
pub(crate) fn run_status_label(run: &RunSummary) -> &'static str {
    if stopped_by_person(run) {
        "stopping\u{2026}"
    } else {
        status_label(run.status)
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

/// What kind of action an approval asks for, as one phrase.
pub(crate) fn action_word(subject: &ApprovalSubject) -> &'static str {
    let is_push = subject
        .command
        .as_deref()
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
        return command.clone();
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
    /// Something is held: the bold lead and the muted detail.
    Held { lead: String, detail: String },
    /// Nothing needs the human.
    Calm { text: String },
}

impl Finding {
    #[cfg(test)]
    /// The whole sentence, for tests and accessibility labels.
    pub(crate) fn sentence(&self) -> String {
        match self {
            Self::Held { lead, detail } => format!("{lead} {detail}"),
            Self::Calm { text } => text.clone(),
        }
    }
}

pub(crate) fn finding(state: &State, now_ms: u64) -> Finding {
    let counts = attention(state);
    let Some(oldest) = state.approvals().first() else {
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
    let detail = if waited == "just now" {
        format!("— {who} {what} was just held.")
    } else {
        format!("— {who} {what} has waited {waited}.")
    };
    Finding::Held { lead, detail }
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
            not_resumed: None,
            session: None,
        }
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
        assert_eq!(executor_name("acp.grok"), "Grok");
        assert_eq!(executor_name("acp.own.mine"), "Mine");
        assert_eq!(executor_name("acp.gemini-cli"), "Gemini Cli");
        assert_eq!(executor_name("mock"), "Mock");
    }
}
