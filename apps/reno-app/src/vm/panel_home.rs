//! The panel's home page, its tokens chip and the Session tab, as plain data: what each row
//! says, whether it can be opened, and why not. Pure functions from run facts, so every state
//! (workspace removed, in place, no repository, live, daemon too old) is a unit test and the
//! panel only draws what these return.
//!
//! Unavailable rows stay in the list, dimmed, with the reason as their detail: they are never
//! hidden.

use reno_proto::{RunStatus, RunSummary, WorkspaceInfo, WorkspaceStatus};

use crate::summary::{self, Problem, RunRepo};
use crate::transcript::Usage;
use crate::ui::icon::Icon;
use crate::vm::changes::Pane;

/// The words of a disabled row whose workspace is gone.
const REMOVED: &str = "workspace removed";
/// The words of a disabled row whose run has no repository.
const NO_REPO: &str = "no repository";

// --- rows ----------------------------------------------------------------------------------------

/// What a home row opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HomeKind {
    Changes,
    Files,
    History,
    Terminal,
    SideChat,
    ForkChat,
}

impl HomeKind {
    pub(crate) const ALL: [Self; 6] = [
        Self::Changes,
        Self::Files,
        Self::History,
        Self::Terminal,
        Self::SideChat,
        Self::ForkChat,
    ];

    /// The key that opens the row (upper case).
    pub(crate) const fn letter(self) -> char {
        match self {
            Self::Changes => 'C',
            Self::Files => 'F',
            Self::History => 'H',
            Self::Terminal => 'T',
            Self::SideChat => 'S',
            Self::ForkChat => 'K',
        }
    }

    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Changes => "Changes",
            Self::Files => "Files",
            Self::History => "History",
            Self::Terminal => "Terminal",
            Self::SideChat => "Side chat",
            Self::ForkChat => "Fork chat",
        }
    }

    /// The row's icon.
    pub(crate) const fn icon(self) -> Icon {
        match self {
            Self::Changes => Icon::Branch,
            Self::Files => Icon::Folder,
            Self::History => Icon::History,
            Self::Terminal => Icon::Terminal,
            Self::SideChat => Icon::Msg,
            Self::ForkChat => Icon::Merge,
        }
    }

    /// What the placeholder tab says the feature will be.
    pub(crate) const fn planned_note(self) -> &'static str {
        match self {
            Self::Changes => "What the run changed.",
            Self::Files => "A read-only tree and viewer over the worktree.",
            Self::History => "The branch's commits as a graph; a row opens that commit's diff.",
            Self::Terminal => {
                "A shell in the worktree. While the run is live it starts sandboxed, with \
                 .git and .reno read-only."
            }
            Self::SideChat => {
                "A new run beside this one, with its own worktree cut from this branch."
            }
            Self::ForkChat => "Like a side chat, started from this run's session.",
        }
    }

    /// Whether the tab behind the row exists yet. Changes and Files do; the UI opens a
    /// placeholder tab for the rest.
    pub(crate) const fn built(self) -> bool {
        matches!(self, Self::Changes | Self::Files)
    }
}

/// The row a pressed letter opens, in either case. Any other key is `None`, so the panel passes
/// it on.
pub(crate) fn letter_to_kind(letter: char) -> Option<HomeKind> {
    let upper = letter.to_ascii_uppercase();
    HomeKind::ALL.into_iter().find(|k| k.letter() == upper)
}

/// One row of the home's Inspect and Work groups.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HomeRow {
    pub(crate) letter: char,
    pub(crate) kind: HomeKind,
    pub(crate) label: &'static str,
    /// The muted live detail; the reason when the row is disabled.
    pub(crate) detail: String,
    pub(crate) enabled: bool,
    /// Why the row cannot be opened (also its tooltip); `None` when it can.
    pub(crate) reason: Option<String>,
    /// Whether its tab exists yet; `false` opens a placeholder.
    pub(crate) built: bool,
}

impl HomeRow {
    fn open(kind: HomeKind, detail: impl Into<String>) -> Self {
        Self {
            letter: kind.letter(),
            kind,
            label: kind.label(),
            detail: detail.into(),
            enabled: true,
            reason: None,
            built: kind.built(),
        }
    }

    fn off(kind: HomeKind, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            enabled: false,
            reason: Some(reason.clone()),
            ..Self::open(kind, reason)
        }
    }
}

// --- the home ------------------------------------------------------------------------------------

/// Whether the executor can start a new run from this run's session. Only Claude Code declares
/// it for now; this is the one place to widen when another executor does.
pub(crate) fn can_fork_session(executor: &str) -> bool {
    executor == "claude-code"
}

/// Where a run works, as far as the home is concerned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    Worktree,
    Removed,
    /// In the repository's own folder, with no worktree.
    SameFolder,
    NoRepository,
    /// Not known yet (the daemon is not connected, or has not answered): the rows look as they
    /// do for a worktree and nothing says "no repository" or "same folder".
    Unknown,
}

/// What the daemon said about the run's workspace record (`workspace.get`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Record {
    /// Not connected, or not answered yet.
    Unread,
    /// Answered: there is a worktree record.
    Present,
    /// Answered, connected: no record. The run works in its own folder (in place, or a read-only
    /// run), because the wire has no flag for it; with no folder at all it is no repository.
    Missing,
    /// A daemon that cannot answer at all.
    OldDaemon,
}

/// The state of the workspace record. `answer` is `Some(has record)` once `workspace.get` was
/// answered.
pub(crate) fn record(connected: bool, supports_changes: bool, answer: Option<bool>) -> Record {
    if !connected {
        return Record::Unread;
    }
    if !supports_changes {
        return Record::OldDaemon;
    }
    match answer {
        Some(true) => Record::Present,
        Some(false) => Record::Missing,
        None => Record::Unread,
    }
}

/// What the home needs to know about a run beyond its changes pane.
#[derive(Clone, Debug)]
pub(crate) struct HomeFacts<'a> {
    pub(crate) status: RunStatus,
    /// The agent's reported problem on a passed run.
    pub(crate) problem: Option<Problem>,
    /// The executor's id, for its name, glyph and fork capability.
    pub(crate) executor: &'a str,
    /// The folder's name.
    pub(crate) project: Option<&'a str>,
    /// The run's whole directory, for "in <path>, not a worktree".
    pub(crate) path: Option<&'a str>,
    /// What the daemon said about the run's workspace record, and so whether it connected.
    pub(crate) record: Record,
    /// The model by name.
    pub(crate) model: Option<&'a str>,
    pub(crate) bot: Option<&'a str>,
    /// What follows the state word in the finding sentence.
    pub(crate) note: Option<&'a str>,
}

/// The state's colour, which the panel maps to the theme.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum NowTone {
    /// Queued, starting, running, evaluating.
    Running,
    /// Waiting for a question or an approval.
    Waiting,
    Done,
    /// Passed, but the agent reported it is blocked.
    Blocked,
    Failed,
    /// Interrupted or stopped.
    Stopped,
}

pub(crate) fn now_tone(status: RunStatus, problem: Option<Problem>) -> NowTone {
    match (status, problem) {
        (RunStatus::Succeeded, Some(Problem::Failed)) | (RunStatus::Failed, _) => NowTone::Failed,
        (RunStatus::Succeeded, Some(Problem::Blocked)) => NowTone::Blocked,
        (RunStatus::Succeeded, None) => NowTone::Done,
        (RunStatus::WaitingInput | RunStatus::WaitingApproval, _) => NowTone::Waiting,
        (RunStatus::Interrupted | RunStatus::Cancelled, _) => NowTone::Stopped,
        (RunStatus::Queued | RunStatus::Leased | RunStatus::Running | RunStatus::Evaluating, _) => {
            NowTone::Running
        }
    }
}

/// Whether the run still holds its workspace: not finished, stopped or interrupted.
fn holds_workspace(status: RunStatus) -> bool {
    matches!(
        status,
        RunStatus::Queued
            | RunStatus::Leased
            | RunStatus::Running
            | RunStatus::WaitingInput
            | RunStatus::WaitingApproval
            | RunStatus::Evaluating
    )
}

/// The `+N −M` pill that opens Review.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DiffPill {
    pub(crate) additions: u32,
    pub(crate) deletions: u32,
    /// "+72 −73"; a side with nothing is left out ("−3").
    pub(crate) label: String,
}

/// The first line of the home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Where {
    pub(crate) project: Option<String>,
    /// `reno/run_…a000001`.
    pub(crate) branch: Option<String>,
    /// The branch whole, for a tooltip or a copy.
    pub(crate) branch_full: Option<String>,
    /// Only while the changes are known and there are some.
    pub(crate) diff: Option<DiffPill>,
}

/// The finding sentence: a dot, the state word in bold, the rest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Now {
    pub(crate) tone: NowTone,
    pub(crate) lead: String,
    pub(crate) text: String,
}

/// The muted line under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Meta {
    /// The executor's id, for its glyph.
    pub(crate) executor: String,
    /// Executor name, model, bot, then commits / "uncommitted" / "workspace removed"; the ones
    /// not known are left out.
    pub(crate) parts: Vec<String>,
}

impl Meta {
    pub(crate) fn text(&self) -> String {
        self.parts.join(" \u{b7} ")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Home {
    pub(crate) place: Where,
    pub(crate) now: Now,
    pub(crate) meta: Meta,
    /// Changes, Files, History.
    pub(crate) inspect: Vec<HomeRow>,
    /// Terminal, Side chat, Fork chat.
    pub(crate) work: Vec<HomeRow>,
}

impl Home {
    pub(crate) fn rows(&self) -> impl Iterator<Item = &HomeRow> {
        self.inspect.iter().chain(&self.work)
    }

    pub(crate) fn row(&self, kind: HomeKind) -> Option<&HomeRow> {
        self.rows().find(|r| r.kind == kind)
    }
}

/// `reno/run_01hx…a000001` as `reno/run_…a000001`: the run id keeps its last 7 characters.
pub(crate) fn short_branch(branch: &str) -> String {
    const KEEP: usize = 7;
    let Some(at) = branch.find("run_") else {
        return branch.to_owned();
    };
    let (head, id) = branch.split_at(at + "run_".len());
    let chars: Vec<char> = id.chars().collect();
    if chars.len() <= KEEP + 1 {
        return branch.to_owned();
    }
    let tail: String = chars[chars.len() - KEEP..].iter().collect();
    format!("{head}\u{2026}{tail}")
}

fn pane_branch(pane: &Pane) -> Option<&str> {
    match pane {
        Pane::Removed { branch } | Pane::Empty { branch } => Some(branch),
        Pane::Files(o) => Some(&o.branch),
        Pane::Hidden | Pane::Unsupported | Pane::Loading | Pane::Failed(_) => None,
    }
}

fn place(pane: &Pane, facts: &HomeFacts<'_>) -> Place {
    match pane {
        Pane::Removed { .. } => Place::Removed,
        Pane::Hidden | Pane::Unsupported => match (facts.record, facts.path.is_some()) {
            (Record::Unread, _) => Place::Unknown,
            (Record::Missing, true) => Place::SameFolder,
            (Record::Missing, false) => Place::NoRepository,
            (Record::OldDaemon | Record::Present, _) => Place::Worktree,
        },
        _ => Place::Worktree,
    }
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

fn line_counts(pane: &Pane) -> Option<(u32, u32)> {
    let Pane::Files(o) = pane else { return None };
    Some((
        o.rows.iter().map(|r| r.additions).sum(),
        o.rows.iter().map(|r| r.deletions).sum(),
    ))
}

fn diff_pill(pane: &Pane) -> Option<DiffPill> {
    let (additions, deletions) = line_counts(pane)?;
    let label = [
        (additions > 0).then(|| format!("+{additions}")),
        (deletions > 0).then(|| format!("\u{2212}{deletions}")),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" ");
    (!label.is_empty()).then_some(DiffPill {
        additions,
        deletions,
        label,
    })
}

fn changes_row(pane: &Pane, place: Place) -> HomeRow {
    let kind = HomeKind::Changes;
    match (pane, place) {
        (_, Place::NoRepository) => HomeRow::off(kind, NO_REPO),
        (Pane::Removed { .. }, _) => HomeRow::off(kind, REMOVED),
        (Pane::Hidden, Place::Unknown) => HomeRow::open(kind, "reading\u{2026}"),
        (Pane::Hidden, _) => HomeRow::off(kind, "not a worktree"),
        (Pane::Unsupported, _) => HomeRow::off(kind, "daemon too old"),
        (Pane::Loading, _) => HomeRow::open(kind, "reading\u{2026}"),
        (Pane::Failed(_), _) => HomeRow::open(kind, "could not read"),
        (Pane::Empty { .. }, _) => HomeRow::open(kind, "no changes"),
        (Pane::Files(o), _) => {
            let (additions, deletions) = line_counts(pane).unwrap_or_default();
            HomeRow::open(
                kind,
                format!(
                    "{} \u{b7} +{additions} \u{2212}{deletions}",
                    plural(o.rows.len(), "file", "files")
                ),
            )
        }
    }
}

fn gated(kind: HomeKind, place: Place, detail: impl Into<String>) -> HomeRow {
    match place {
        Place::Removed => HomeRow::off(kind, REMOVED),
        Place::NoRepository => HomeRow::off(kind, NO_REPO),
        Place::Worktree | Place::SameFolder | Place::Unknown => HomeRow::open(kind, detail),
    }
}

fn meta(facts: &HomeFacts<'_>, pane: &Pane) -> Meta {
    let tail = match pane {
        Pane::Removed { .. } => Some(REMOVED.to_owned()),
        Pane::Files(o) if o.commits == "no commits yet" => Some("uncommitted".to_owned()),
        Pane::Files(o) => Some(o.commits.clone()),
        _ => None,
    };
    let parts = [
        Some(summary::executor_name(facts.executor)),
        facts.model.map(str::to_owned),
        facts.bot.map(str::to_owned),
        tail,
    ]
    .into_iter()
    .flatten()
    .collect();
    Meta {
        executor: facts.executor.to_owned(),
        parts,
    }
}

/// The home of a run's panel.
pub(crate) fn home(facts: &HomeFacts<'_>, pane: &Pane) -> Home {
    let place = place(pane, facts);
    let branch_full = pane_branch(pane).map(str::to_owned);

    let terminal = match place {
        Place::SameFolder => gated(
            HomeKind::Terminal,
            place,
            match facts.path {
                Some(path) => format!("in {path}, not a worktree"),
                None => "in the folder, not a worktree".to_owned(),
            },
        ),
        _ if holds_workspace(facts.status) => gated(
            HomeKind::Terminal,
            place,
            "in the worktree \u{b7} protected files locked while it runs",
        ),
        _ => gated(HomeKind::Terminal, place, "in the worktree"),
    };
    let fork = if can_fork_session(facts.executor) {
        HomeRow::open(
            HomeKind::ForkChat,
            "continue from this session in a new run",
        )
    } else {
        HomeRow::off(
            HomeKind::ForkChat,
            format!(
                "{} can't fork a session",
                summary::executor_name(facts.executor)
            ),
        )
    };

    Home {
        place: Where {
            project: facts.project.map(str::to_owned),
            branch: branch_full.as_deref().map(short_branch),
            branch_full: branch_full.clone(),
            diff: diff_pill(pane),
        },
        now: Now {
            tone: now_tone(facts.status, facts.problem),
            lead: summary::status_words(facts.status, facts.problem).to_owned(),
            text: facts.note.unwrap_or_default().to_owned(),
        },
        meta: meta(facts, pane),
        inspect: vec![
            changes_row(pane, place),
            gated(
                HomeKind::Files,
                place,
                match place {
                    Place::SameFolder => "browse the folder \u{b7} read-only",
                    _ => "browse the worktree \u{b7} read-only",
                },
            ),
            gated(
                HomeKind::History,
                place,
                format!(
                    "commits on {}",
                    branch_full
                        .as_deref()
                        .map_or_else(|| "the branch".to_owned(), short_branch)
                ),
            ),
        ],
        work: vec![
            terminal,
            HomeRow::open(HomeKind::SideChat, "a new run beside this one"),
            fork,
        ],
    }
}

/// What follows the state word in the finding sentence: who holds what and for how long, the
/// agent's own report when it reported a problem, or what the branch holds once the run ended.
/// Ends with a full stop, or is empty when there is nothing to add.
pub(crate) fn finding_note(
    run: &RunSummary,
    held: Option<&reno_proto::Approval>,
    result: Option<&str>,
    now_ms: u64,
) -> String {
    let who = summary::run_bot(run).unwrap_or("It");
    match run.status {
        RunStatus::WaitingApproval => {
            let Some(approval) = held else {
                return String::new();
            };
            let verb = if approval.subject.command.is_some() {
                "wants to run"
            } else {
                "wants to use"
            };
            let what = summary::subject_line(&approval.subject);
            match summary::age_label(now_ms.saturating_sub(approval.requested_at_ms)).as_str() {
                "just now" => format!("{who} {verb} {what} \u{2014} just held."),
                age => format!("{who} {verb} {what} \u{2014} held {age}."),
            }
        }
        RunStatus::WaitingInput => format!("{who} is waiting for your answer."),
        _ => {
            if let (Some(report), Some(_)) = (&run.report, summary::reported_problem(run)) {
                return report.summary.clone();
            }
            result.map(|line| format!("{line}.")).unwrap_or_default()
        }
    }
}

// --- the tokens chip -----------------------------------------------------------------------------

/// The chip's label: input plus output tokens, the cache left out. "999", "1.4k", "38k", "1.2m";
/// cut, never rounded up, so a label never reads larger than the count.
pub(crate) fn usage_chip(usage: Usage) -> String {
    let n = usage.total();
    let scaled = |unit: u64, suffix: &str| {
        let tenths = n / (unit / 10);
        if tenths < 100 {
            let (whole, tenth) = (tenths / 10, tenths % 10);
            if tenth == 0 {
                format!("{whole}{suffix}")
            } else {
                format!("{whole}.{tenth}{suffix}")
            }
        } else {
            format!("{}{suffix}", tenths / 10)
        }
    };
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => scaled(1_000, "k"),
        _ => scaled(1_000_000, "m"),
    }
}

/// 12345 becomes "12,345".
pub(crate) fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

// --- the Session tab -----------------------------------------------------------------------------

/// One `kv` row: a label, a value and a smaller line under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct KvRow {
    pub(crate) label: &'static str,
    pub(crate) value: String,
    pub(crate) sub: Option<String>,
}

impl KvRow {
    fn new(label: &'static str, value: impl Into<String>) -> Self {
        Self {
            label,
            value: value.into(),
            sub: None,
        }
    }

    fn with_sub(mut self, sub: Option<String>) -> Self {
        self.sub = sub;
        self
    }
}

/// What the Session tab needs beyond the run itself.
#[derive(Clone, Debug)]
pub(crate) struct SessionFacts<'a> {
    pub(crate) run: &'a RunSummary,
    /// The model in use by name (`"Opus 4 · claude-opus-4"`), as the Run screen words it.
    pub(crate) model: Option<String>,
    /// The bot's access mode in words; `"…"` while its definition is not read.
    pub(crate) access: Option<String>,
    /// [`summary::run_repo`] of the run.
    pub(crate) repo: Option<RunRepo>,
    /// `workspace.get`, when the run has one.
    pub(crate) workspace: Option<&'a WorkspaceInfo>,
    pub(crate) usage: Option<Usage>,
    /// Cache tokens, when the harness reports them; no event carries them yet.
    pub(crate) cache_tokens: Option<u64>,
    /// Whether the run's executor could not take it and its health is shown.
    pub(crate) executor_health: bool,
    pub(crate) now_ms: u64,
}

/// Today's details pane, regrouped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SessionSections {
    /// The executor-health block (with its actions) goes above Run.
    pub(crate) executor_health: bool,
    pub(crate) tone: NowTone,
    /// State, Report, Bot, Executor, Access, Attempt, Started, Model, Effort.
    pub(crate) run: Vec<KvRow>,
    /// Repository, Branch.
    pub(crate) workspace: Vec<KvRow>,
    /// Tokens, Cache.
    pub(crate) usage: Vec<KvRow>,
}

pub(crate) fn session(facts: &SessionFacts<'_>) -> SessionSections {
    let run = facts.run;
    let problem = summary::reported_problem(run);
    let end = if run.status.is_terminal() {
        run.updated_at_ms
    } else {
        facts.now_ms
    };
    let id = run.executor.as_str();
    let name = summary::executor_name(id);

    let mut rows = vec![KvRow::new("State", summary::run_status_label(run))];
    if let (Some(report), Some(_)) = (&run.report, problem) {
        rows.push(KvRow::new("Report", report.summary.clone()));
    }
    if let Some(bot) = summary::run_bot(run) {
        rows.push(KvRow::new("Bot", bot));
    }
    rows.push(KvRow::new("Executor", name.clone()).with_sub((id != name).then(|| id.to_owned())));
    if let Some(access) = &facts.access {
        rows.push(KvRow::new("Access", access.clone()));
    }
    if run.attempt > 1 {
        rows.push(KvRow::new("Attempt", run.attempt.to_string()));
    }
    rows.push(KvRow::new(
        "Started",
        format!(
            "{} \u{b7} {}",
            summary::clock_utc(run.created_at_ms),
            summary::duration_label(end.saturating_sub(run.created_at_ms))
        ),
    ));
    if let Some(model) = &facts.model {
        rows.push(KvRow::new("Model", model.clone()));
    }
    if let Some(effort) = run.effort {
        rows.push(KvRow::new("Effort", effort.as_str()));
    }

    let mut workspace = Vec::new();
    if let Some(repo) = &facts.repo {
        workspace.push(KvRow::new("Repository", repo.name.clone()).with_sub(repo.path.clone()));
    }
    if let Some(info) = facts.workspace {
        let sub = if info.status == WorkspaceStatus::Removed {
            REMOVED.to_owned()
        } else {
            info.cwd().display().to_string()
        };
        workspace.push(KvRow::new("Branch", info.branch.clone()).with_sub(Some(sub)));
    }

    let mut usage = vec![match facts.usage {
        Some(u) => KvRow::new("Tokens", format!("{} tokens", group_digits(u.total()))).with_sub(
            Some(format!(
                "{} in \u{b7} {} out",
                group_digits(u.input),
                group_digits(u.output)
            )),
        ),
        None => KvRow::new("Tokens", "none yet"),
    }];
    if let Some(cache) = facts.cache_tokens {
        usage.push(KvRow::new(
            "Cache",
            format!("{} tokens", group_digits(cache)),
        ));
    }

    SessionSections {
        executor_health: facts.executor_health,
        tone: now_tone(run.status, problem),
        run: rows,
        workspace,
        usage,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::path::PathBuf;

    use reno_proto::{
        ChangeKind, ChangedFile, ReportStatus, RunId, RunReport, TaskId, WorkspaceInfo,
    };

    use super::*;
    use crate::vm::changes::{Overview, file_rows};

    const BRANCH: &str = "reno/run_01jabcdefghx000a000001";

    fn file(path: &str, additions: u32, deletions: u32) -> ChangedFile {
        ChangedFile {
            path: path.to_owned(),
            from: None,
            kind: ChangeKind::Modified,
            additions,
            deletions,
            binary: false,
        }
    }

    fn files_pane(files: &[ChangedFile], commits: &str) -> Pane {
        Pane::Files(Overview {
            branch: BRANCH.to_owned(),
            commits: commits.to_owned(),
            stats: String::new(),
            rows: file_rows(files),
            path: None,
        })
    }

    fn facts(status: RunStatus) -> HomeFacts<'static> {
        HomeFacts {
            status,
            problem: None,
            executor: "claude-code",
            project: Some("porch"),
            path: Some("/work/porch"),
            record: Record::Present,
            model: Some("Opus 4"),
            bot: Some("reviewer"),
            note: Some("Waiting on you."),
        }
    }

    fn row(h: &Home, kind: HomeKind) -> &HomeRow {
        h.row(kind).unwrap()
    }

    #[test]
    fn the_rows_are_in_the_briefs_order_with_their_letters() {
        let h = home(&facts(RunStatus::Running), &Pane::Loading);
        let order: Vec<_> = h.rows().map(|r| (r.letter, r.label)).collect();
        assert_eq!(
            order,
            [
                ('C', "Changes"),
                ('F', "Files"),
                ('H', "History"),
                ('T', "Terminal"),
                ('S', "Side chat"),
                ('K', "Fork chat"),
            ]
        );
        assert_eq!(h.inspect.len(), 3);
        assert_eq!(h.work.len(), 3);
    }

    #[test]
    fn changes_says_its_counts_or_its_state_words() {
        let pane = files_pane(&[file("a.rs", 70, 70), file("b.rs", 2, 3)], "3 commits");
        let h = home(&facts(RunStatus::Succeeded), &pane);
        let c = row(&h, HomeKind::Changes);
        assert_eq!(c.detail, "2 files \u{b7} +72 \u{2212}73");
        assert!(c.enabled && c.reason.is_none());

        let one = files_pane(&[file("a.rs", 72, 73)], "3 commits");
        let h = home(&facts(RunStatus::Succeeded), &one);
        assert_eq!(
            row(&h, HomeKind::Changes).detail,
            "1 file \u{b7} +72 \u{2212}73"
        );

        let words = [
            (Pane::Loading, "reading\u{2026}", true),
            (Pane::Failed("x".into()), "could not read", true),
            (
                Pane::Empty {
                    branch: BRANCH.into(),
                },
                "no changes",
                true,
            ),
            (
                Pane::Removed {
                    branch: BRANCH.into(),
                },
                "workspace removed",
                false,
            ),
            (Pane::Unsupported, "daemon too old", false),
        ];
        for (pane, detail, enabled) in words {
            let h = home(&facts(RunStatus::Succeeded), &pane);
            let c = row(&h, HomeKind::Changes);
            assert_eq!(
                (c.detail.as_str(), c.enabled),
                (detail, enabled),
                "{pane:?}"
            );
            assert_eq!(c.reason.is_some(), !enabled);
        }
    }

    #[test]
    fn a_removed_workspace_dims_files_history_terminal_and_changes() {
        let pane = Pane::Removed {
            branch: BRANCH.into(),
        };
        let h = home(&facts(RunStatus::Succeeded), &pane);
        for kind in [
            HomeKind::Changes,
            HomeKind::Files,
            HomeKind::History,
            HomeKind::Terminal,
        ] {
            let r = row(&h, kind);
            assert!(!r.enabled, "{kind:?}");
            assert_eq!(r.reason.as_deref(), Some("workspace removed"));
            assert_eq!(r.detail, "workspace removed");
        }
        // The rows stay; the others are untouched.
        assert!(row(&h, HomeKind::SideChat).enabled);
        assert_eq!(h.rows().count(), 6);
        assert_eq!(h.meta.parts.last().unwrap(), "workspace removed");
    }

    #[test]
    fn no_folder_and_no_record_dims_changes_files_history_and_terminal() {
        let mut f = facts(RunStatus::Succeeded);
        f.record = Record::Missing;
        f.path = None;
        let h = home(&f, &Pane::Hidden);
        for kind in [
            HomeKind::Changes,
            HomeKind::Files,
            HomeKind::History,
            HomeKind::Terminal,
        ] {
            let r = row(&h, kind);
            assert!(!r.enabled, "{kind:?}");
            assert_eq!(r.reason.as_deref(), Some("no repository"));
        }
    }

    #[test]
    fn an_old_daemon_is_unknown_not_no_repository() {
        assert_eq!(record(true, false, None), Record::OldDaemon);
        assert_eq!(record(true, true, Some(true)), Record::Present);
        assert_eq!(record(true, true, Some(false)), Record::Missing);
        assert_eq!(record(true, true, None), Record::Unread);

        let mut f = facts(RunStatus::Succeeded);
        f.record = record(true, false, None);
        let h = home(&f, &Pane::Unsupported);
        let changes = row(&h, HomeKind::Changes);
        assert_eq!(changes.detail, "daemon too old");
        assert!(!changes.enabled);
        for kind in [HomeKind::Files, HomeKind::History, HomeKind::Terminal] {
            assert!(row(&h, kind).enabled, "{kind:?}");
        }
    }

    #[test]
    fn before_the_daemon_connects_nothing_says_same_folder_or_no_repository() {
        assert_eq!(record(false, false, None), Record::Unread);
        assert_eq!(record(false, true, Some(false)), Record::Unread);
        for status in [RunStatus::Running, RunStatus::Succeeded] {
            let mut f = facts(status);
            f.record = record(false, false, None);
            let h = home(&f, &Pane::Hidden);
            let changes = row(&h, HomeKind::Changes);
            assert_eq!(changes.detail, "reading\u{2026}");
            assert!(changes.enabled);
            for kind in [HomeKind::Files, HomeKind::History, HomeKind::Terminal] {
                let r = row(&h, kind);
                assert!(r.enabled, "{kind:?}");
                assert!(r.reason.is_none(), "{kind:?}");
                assert!(
                    !r.detail.contains("not a worktree"),
                    "{kind:?} {}",
                    r.detail
                );
                assert!(!r.detail.contains("repository"), "{kind:?} {}", r.detail);
            }
        }
    }

    #[test]
    fn a_connected_daemon_with_no_record_and_a_folder_says_in_place() {
        let mut f = facts(RunStatus::Succeeded);
        f.record = record(true, true, Some(false));
        let h = home(&f, &Pane::Hidden);
        assert_eq!(
            row(&h, HomeKind::Terminal).detail,
            "in /work/porch, not a worktree"
        );
        let changes = row(&h, HomeKind::Changes);
        assert_eq!(changes.reason.as_deref(), Some("not a worktree"));
        assert!(!changes.enabled);
        assert!(row(&h, HomeKind::Files).enabled);
    }

    #[test]
    fn terminal_wording_follows_live_in_place_and_ended() {
        let t = |status, pane: &Pane| {
            let h = home(&facts(status), pane);
            row(&h, HomeKind::Terminal).clone()
        };
        let wt = files_pane(&[file("a", 1, 1)], "1 commit");
        for live in [
            RunStatus::Queued,
            RunStatus::Leased,
            RunStatus::Running,
            RunStatus::WaitingInput,
            RunStatus::WaitingApproval,
            RunStatus::Evaluating,
        ] {
            assert_eq!(
                t(live, &wt).detail,
                "in the worktree \u{b7} protected files locked while it runs",
                "{live:?}"
            );
        }
        for ended in [
            RunStatus::Succeeded,
            RunStatus::Failed,
            RunStatus::Cancelled,
            RunStatus::Interrupted,
        ] {
            assert_eq!(t(ended, &wt).detail, "in the worktree", "{ended:?}");
        }
        // In place: its own words, live or not, and still openable.
        for status in [RunStatus::Running, RunStatus::Succeeded] {
            let mut f = facts(status);
            f.record = Record::Missing;
            let h = home(&f, &Pane::Hidden);
            let r = row(&h, HomeKind::Terminal).clone();
            assert_eq!(r.detail, "in /work/porch, not a worktree");
            assert!(r.enabled);
        }
        // In place: there is no worktree to compare, so Changes is off for that reason.
        let mut f = facts(RunStatus::Running);
        f.record = Record::Missing;
        let h = home(&f, &Pane::Hidden);
        assert_eq!(
            row(&h, HomeKind::Changes).reason.as_deref(),
            Some("not a worktree")
        );
        assert!(row(&h, HomeKind::Files).enabled);
    }

    #[test]
    fn files_and_history_say_what_they_open() {
        let pane = files_pane(&[file("a", 1, 1)], "1 commit");
        let h = home(&facts(RunStatus::Succeeded), &pane);
        assert_eq!(
            row(&h, HomeKind::Files).detail,
            "browse the worktree \u{b7} read-only"
        );
        assert_eq!(
            row(&h, HomeKind::History).detail,
            "commits on reno/run_\u{2026}a000001"
        );
        // Before the branch is known.
        let h = home(&facts(RunStatus::Running), &Pane::Loading);
        assert_eq!(row(&h, HomeKind::History).detail, "commits on the branch");
    }

    #[test]
    fn fork_chat_needs_an_executor_that_declares_session_fork() {
        let h = home(&facts(RunStatus::Running), &Pane::Loading);
        let k = row(&h, HomeKind::ForkChat);
        assert!(k.enabled);
        assert_eq!(k.detail, "continue from this session in a new run");

        for (id, name) in [
            ("codex", "Codex"),
            ("acp.grok", "Grok"),
            ("opencode", "OpenCode"),
        ] {
            let mut f = facts(RunStatus::Running);
            f.executor = id;
            let h = home(&f, &Pane::Loading);
            let k = row(&h, HomeKind::ForkChat);
            assert!(!k.enabled);
            assert_eq!(k.detail, format!("{name} can't fork a session"));
            assert_eq!(k.reason.as_deref(), Some(k.detail.as_str()));
            assert!(!can_fork_session(id));
        }
        assert!(can_fork_session("claude-code"));
    }

    #[test]
    fn only_changes_and_files_have_a_tab_built() {
        let h = home(&facts(RunStatus::Running), &Pane::Loading);
        for r in h.rows() {
            assert_eq!(
                r.built,
                matches!(r.kind, HomeKind::Changes | HomeKind::Files),
                "{:?}",
                r.kind
            );
        }
        // A disabled row says the same.
        let pane = Pane::Removed {
            branch: BRANCH.into(),
        };
        let h = home(&facts(RunStatus::Succeeded), &pane);
        assert!(row(&h, HomeKind::Files).built);
        assert!(!row(&h, HomeKind::History).built);
        assert_eq!(h.rows().filter(|r| r.built).count(), 2);
    }

    #[test]
    fn letters_open_rows_in_either_case_and_others_pass() {
        for (c, kind) in [
            ('C', HomeKind::Changes),
            ('F', HomeKind::Files),
            ('H', HomeKind::History),
            ('T', HomeKind::Terminal),
            ('S', HomeKind::SideChat),
            ('K', HomeKind::ForkChat),
        ] {
            assert_eq!(letter_to_kind(c), Some(kind));
            assert_eq!(letter_to_kind(c.to_ascii_lowercase()), Some(kind));
            assert_eq!(kind.letter(), c);
        }
        for c in ['a', 'B', 'x', 'Z', '1', ' ', '\u{e9}', '\u{212a}'] {
            assert_eq!(letter_to_kind(c), None, "{c:?}");
        }
    }

    #[test]
    fn the_where_line_shortens_the_branch_and_shows_the_diff_pill() {
        let pane = files_pane(&[file("a.rs", 72, 73)], "2 commits");
        let h = home(&facts(RunStatus::Running), &pane);
        assert_eq!(h.place.project.as_deref(), Some("porch"));
        assert_eq!(h.place.branch.as_deref(), Some("reno/run_\u{2026}a000001"));
        assert_eq!(h.place.branch_full.as_deref(), Some(BRANCH));
        let pill = h.place.diff.unwrap();
        assert_eq!((pill.additions, pill.deletions), (72, 73));
        assert_eq!(pill.label, "+72 \u{2212}73");

        let only_dels = files_pane(&[file("a.rs", 0, 3)], "2 commits");
        let h = home(&facts(RunStatus::Running), &only_dels);
        assert_eq!(h.place.diff.unwrap().label, "\u{2212}3");

        for pane in [
            Pane::Loading,
            Pane::Hidden,
            Pane::Empty {
                branch: BRANCH.into(),
            },
        ] {
            assert!(home(&facts(RunStatus::Running), &pane).place.diff.is_none());
        }
        let h = home(&facts(RunStatus::Running), &Pane::Loading);
        assert!(h.place.branch.is_none());
    }

    #[test]
    fn a_branch_is_shortened_only_when_it_is_long() {
        assert_eq!(short_branch(BRANCH), "reno/run_\u{2026}a000001");
        assert_eq!(short_branch("reno/run_a000001"), "reno/run_a000001");
        assert_eq!(short_branch("feature/x"), "feature/x");
        assert_eq!(short_branch(""), "");
    }

    #[test]
    fn now_has_a_tone_a_lead_and_a_text() {
        let cases = [
            (RunStatus::Running, None, NowTone::Running, "running"),
            (RunStatus::Queued, None, NowTone::Running, "queued"),
            (
                RunStatus::WaitingInput,
                None,
                NowTone::Waiting,
                "needs input",
            ),
            (
                RunStatus::WaitingApproval,
                None,
                NowTone::Waiting,
                "held for you",
            ),
            (RunStatus::Succeeded, None, NowTone::Done, "passed"),
            (
                RunStatus::Succeeded,
                Some(Problem::Blocked),
                NowTone::Blocked,
                "blocked",
            ),
            (
                RunStatus::Succeeded,
                Some(Problem::Failed),
                NowTone::Failed,
                "reported failed",
            ),
            (RunStatus::Failed, None, NowTone::Failed, "failed"),
            (
                RunStatus::Interrupted,
                None,
                NowTone::Stopped,
                "interrupted",
            ),
            (RunStatus::Cancelled, None, NowTone::Stopped, "stopped"),
        ];
        for (status, problem, tone, lead) in cases {
            let mut f = facts(status);
            f.problem = problem;
            let h = home(&f, &Pane::Loading);
            assert_eq!(
                (h.now.tone, h.now.lead.as_str()),
                (tone, lead),
                "{status:?}"
            );
            assert_eq!(h.now.text, "Waiting on you.");
        }
        let mut f = facts(RunStatus::Running);
        f.note = None;
        assert_eq!(home(&f, &Pane::Loading).now.text, "");
    }

    #[test]
    fn the_meta_line_lists_what_is_known_then_the_commits() {
        let pane = files_pane(&[file("a", 1, 1)], "3 commits");
        let h = home(&facts(RunStatus::Running), &pane);
        assert_eq!(h.meta.executor, "claude-code");
        assert_eq!(
            h.meta.text(),
            "Claude Code \u{b7} Opus 4 \u{b7} reviewer \u{b7} 3 commits"
        );

        let uncommitted = files_pane(&[file("a", 1, 1)], "no commits yet");
        assert_eq!(
            home(&facts(RunStatus::Running), &uncommitted)
                .meta
                .parts
                .last()
                .unwrap(),
            "uncommitted"
        );

        let mut f = facts(RunStatus::Running);
        f.model = None;
        f.bot = None;
        assert_eq!(home(&f, &Pane::Loading).meta.text(), "Claude Code");
    }

    // --- the tokens chip ---

    fn chip(input: u64, output: u64) -> String {
        usage_chip(Usage { input, output })
    }

    #[test]
    fn the_chip_counts_input_and_output_and_abbreviates() {
        for (total, label) in [
            (0, "0"),
            (999, "999"),
            (1_000, "1k"),
            (1_434, "1.4k"),
            (9_999, "9.9k"),
            (10_000, "10k"),
            (38_214, "38k"),
            (999_999, "999k"),
            (1_000_000, "1m"),
            (1_550_000, "1.5m"),
            (25_000_000, "25m"),
        ] {
            assert_eq!(chip(total, 0), label, "{total}");
        }
        // Input and output add up.
        assert_eq!(chip(1_000, 434), "1.4k");
        assert_eq!(chip(u64::MAX, 1), chip(u64::MAX, 0));
    }

    // --- the Session tab ---

    fn run(status: RunStatus) -> RunSummary {
        RunSummary {
            model_options: Default::default(),
            effort: None,
            run_id: RunId::from_suffix("a000001").unwrap(),
            task_id: TaskId::from_suffix("a000001").unwrap(),
            executor: "claude-code".parse().unwrap(),
            status,
            attempt: 1,
            title: "fix the thing".to_owned(),
            prompt: None,
            model: None,
            stop_reason: None,
            bot: None,
            cwd: None,
            created_at_ms: 60 * 60_000,
            updated_at_ms: 2 * 60 * 60_000,
            stop: None,
            report: None,
            not_resumed: None,
            session: None,
        }
    }

    fn session_facts(run: &RunSummary) -> SessionFacts<'_> {
        SessionFacts {
            run,
            model: None,
            access: None,
            repo: None,
            workspace: None,
            usage: None,
            cache_tokens: None,
            executor_health: false,
            now_ms: 3 * 60 * 60_000,
        }
    }

    fn labels(rows: &[KvRow]) -> Vec<&'static str> {
        rows.iter().map(|r| r.label).collect()
    }

    fn info(status: WorkspaceStatus) -> WorkspaceInfo {
        WorkspaceInfo {
            run_id: RunId::from_suffix("a000001").unwrap(),
            repo: PathBuf::from("/work/porch"),
            repo_key: "porch-1".to_owned(),
            path: PathBuf::from("/wt/porch/run"),
            subdir: String::new(),
            branch: BRANCH.to_owned(),
            base_sha: "abc".to_owned(),
            status,
            changes: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn the_session_tab_groups_run_workspace_and_usage() {
        let mut r = run(RunStatus::Succeeded);
        r.bot = Some(reno_proto::defs::BotPin {
            name: "reviewer".to_owned(),
            version: 1,
        });
        r.effort = Some(reno_proto::Effort::High);
        r.attempt = 2;
        r.report = Some(RunReport {
            status: ReportStatus::Blocked,
            summary: "needs a key".to_owned(),
        });
        let ws = info(WorkspaceStatus::Kept);
        let mut f = session_facts(&r);
        f.model = Some("Opus 4 \u{b7} claude-opus-4".to_owned());
        f.access = Some("Supervised".to_owned());
        f.repo = Some(RunRepo {
            name: "porch".to_owned(),
            path: Some("/work/porch".to_owned()),
        });
        f.workspace = Some(&ws);
        f.usage = Some(Usage {
            input: 12_345,
            output: 678,
        });
        f.cache_tokens = Some(1_000_000);
        let s = session(&f);

        assert_eq!(
            labels(&s.run),
            [
                "State", "Report", "Bot", "Executor", "Access", "Attempt", "Started", "Model",
                "Effort"
            ]
        );
        assert_eq!(s.run[0].value, "blocked");
        assert_eq!(s.run[1].value, "needs a key");
        assert_eq!(s.run[2].value, "reviewer");
        assert_eq!(s.run[3].value, "Claude Code");
        assert_eq!(s.run[3].sub.as_deref(), Some("claude-code"));
        assert_eq!(s.run[6].value, "01:00 UTC \u{b7} 1h 00m");
        assert_eq!(s.run[8].value, "high");
        assert_eq!(s.tone, NowTone::Blocked);

        assert_eq!(labels(&s.workspace), ["Repository", "Branch"]);
        assert_eq!(s.workspace[0].sub.as_deref(), Some("/work/porch"));
        assert_eq!(s.workspace[1].value, BRANCH);
        assert_eq!(s.workspace[1].sub.as_deref(), Some("/wt/porch/run"));

        assert_eq!(labels(&s.usage), ["Tokens", "Cache"]);
        assert_eq!(s.usage[0].value, "13,023 tokens");
        assert_eq!(s.usage[0].sub.as_deref(), Some("12,345 in \u{b7} 678 out"));
        assert_eq!(s.usage[1].value, "1,000,000 tokens");
        assert!(!s.executor_health);
    }

    #[test]
    fn the_session_tab_leaves_out_what_is_not_known() {
        let r = run(RunStatus::Running);
        let s = session(&session_facts(&r));
        assert_eq!(labels(&s.run), ["State", "Executor", "Started"]);
        // A live run's clock runs to now.
        assert_eq!(s.run[2].value, "01:00 UTC \u{b7} 2h 00m");
        assert!(s.workspace.is_empty());
        assert_eq!(s.usage, vec![KvRow::new("Tokens", "none yet")]);
        assert_eq!(s.tone, NowTone::Running);
        // A report on a run with no problem is not shown.
        let mut done = run(RunStatus::Succeeded);
        done.report = Some(RunReport {
            status: ReportStatus::Done,
            summary: "all good".to_owned(),
        });
        assert!(!labels(&session(&session_facts(&done)).run).contains(&"Report"));
    }

    #[test]
    fn the_session_tab_flags_executor_health_and_a_removed_workspace() {
        let r = run(RunStatus::Failed);
        let ws = info(WorkspaceStatus::Removed);
        let mut f = session_facts(&r);
        f.executor_health = true;
        f.workspace = Some(&ws);
        let s = session(&f);
        assert!(s.executor_health);
        assert_eq!(s.workspace[0].sub.as_deref(), Some("workspace removed"));
    }

    // --- the finding's words ---

    fn held(command: &str, at_ms: u64) -> reno_proto::Approval {
        reno_proto::Approval {
            id: reno_proto::ApprovalId::from_suffix("a1").unwrap(),
            run_id: RunId::from_suffix("a000001").unwrap(),
            subject: reno_proto::ApprovalSubject {
                action: reno_proto::ActionClass::Command,
                tool: "Bash".to_owned(),
                command: Some(command.to_owned()),
                paths: Vec::new(),
                raw: serde_json::Value::Null,
            },
            requested_at_ms: at_ms,
        }
    }

    #[test]
    fn a_held_action_reads_who_wants_what_and_for_how_long() {
        let mut r = run(RunStatus::WaitingApproval);
        r.title = "keeper \u{b7} porch: push".to_owned();
        let a = held("/bin/zsh -lc 'git push -u origin HEAD'", 0);
        assert_eq!(
            finding_note(&r, Some(&a), None, 3 * 60_000),
            "keeper wants to run git push -u origin HEAD \u{2014} held 3m."
        );
        // Held for under a minute, and with no bot named.
        r.title = "fix the thing".to_owned();
        assert_eq!(
            finding_note(&r, Some(&a), None, 10_000),
            "It wants to run git push -u origin HEAD \u{2014} just held."
        );
    }

    #[test]
    fn a_wait_for_an_answer_and_a_run_still_going_say_little() {
        assert_eq!(
            finding_note(&run(RunStatus::WaitingInput), None, None, 0),
            "It is waiting for your answer."
        );
        assert_eq!(finding_note(&run(RunStatus::Running), None, None, 0), "");
    }

    #[test]
    fn an_ended_run_says_its_report_or_what_its_branch_holds() {
        let mut r = run(RunStatus::Succeeded);
        let result = "Branch reno/x has 1 commit, 1 file changed (+72 \u{2212}73)";
        assert_eq!(
            finding_note(&r, None, Some(result), 0),
            format!("{result}.")
        );
        // The agent's own words come first when it reported a problem.
        r.report = Some(RunReport {
            status: ReportStatus::Blocked,
            summary: "Cargo.lock is committed but not pushed.".to_owned(),
        });
        assert_eq!(
            finding_note(&r, None, Some(result), 0),
            "Cargo.lock is committed but not pushed."
        );
        assert_eq!(finding_note(&run(RunStatus::Failed), None, None, 0), "");
    }
}
