//! First-run setup (ROAD-38): which agents can take runs on this Mac and what the person can do
//! about the ones that cannot, the sentence that says so, and where a person is in setup. Pure;
//! the three drafts draw it differently and share every rule here.
//!
//! Every choice setup offers applies when it is made (the look changes at once, an install or a
//! sign-in starts at once, the project is the new-thread project at once), so leaving setup at
//! any point, by Skip or by finishing, leaves nothing half-done.

use crate::vm::executors::{ExecutorRow, Group, InstallOffer, JobState, Need};
use crate::vm::registry::SignInChoice;
use reno_proto::JobKind;

/// How an agent that is signed out can be signed in from here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum SignInRoute {
    /// The agent signs itself in, in the browser (`executor.signin.start`).
    Browser(Option<String>),
    /// A command the person runs in Terminal.
    Terminal(String),
}

/// What the person can do on an agent's line: the job's one control.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Action {
    SignIn(SignInRoute),
    Install,
}

impl Action {
    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::SignIn(_) => "Sign in",
            Self::Install => "Install",
        }
    }
}

/// Where an agent stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AgentState {
    Ready,
    NeedsSignIn,
    NotInstalled,
    /// Installed, but too old to run.
    NeedsUpdate,
    /// Not usable and not the daemon's to fix.
    Unusable,
    /// Not checked yet.
    Unknown,
    Signing,
    Installing,
    /// Its install failed: what the daemon said.
    Failed(String),
}

impl AgentState {
    /// The words on the agent's line.
    pub(crate) const fn words(&self) -> &'static str {
        match self {
            Self::Ready => "Ready",
            Self::NeedsSignIn => "Signed out",
            Self::NotInstalled => "Not installed",
            Self::NeedsUpdate => "Too old",
            Self::Unusable => "Cannot be used",
            Self::Unknown => "Not checked yet",
            Self::Signing => "Signing in\u{2026}",
            Self::Installing => "Installing\u{2026}",
            Self::Failed(_) => "Install failed",
        }
    }

    /// What the sentence says of it, after the agent's name.
    const fn phrase(&self) -> &'static str {
        match self {
            Self::Ready => "can take runs",
            Self::NeedsSignIn => "needs signing in",
            Self::NotInstalled => "is not installed",
            Self::NeedsUpdate => "needs an update",
            Self::Unusable => "cannot be used",
            Self::Unknown => "has not been checked yet",
            Self::Signing => "is signing in",
            Self::Installing => "is installing",
            Self::Failed(_) => "did not install",
        }
    }

    pub(crate) fn can_take_runs(&self) -> bool {
        *self == Self::Ready
    }

    /// A job of ours is running on it.
    pub(crate) fn working(&self) -> bool {
        matches!(self, Self::Signing | Self::Installing)
    }
}

/// One agent on this Mac, as setup lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AgentLine {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) version: Option<String>,
    pub(crate) state: AgentState,
    /// The one control the job names, when there is one.
    pub(crate) action: Option<Action>,
}

/// The sign-in an agent's row offers: the browser when the daemon can and the agent signs itself
/// in; else its own command.
fn route_of(row: &ExecutorRow, browser: bool) -> Option<SignInRoute> {
    let choice = row.registry.as_ref().and_then(|r| {
        r.choices.iter().find_map(|c| match c {
            SignInChoice::Browser { method, .. } => Some(method.clone()),
            _ => None,
        })
    });
    match (browser, choice) {
        (true, Some(method)) => Some(SignInRoute::Browser(method)),
        _ => row.sign_in.clone().map(SignInRoute::Terminal),
    }
}

/// The lines setup shows, one per row. `browser` is whether the daemon can sign an agent in
/// itself; `signing` says whether a sign-in of that agent is running now.
pub(crate) fn agent_lines(
    rows: &[ExecutorRow],
    browser: bool,
    signing: &dyn Fn(&str) -> bool,
) -> Vec<AgentLine> {
    rows.iter()
        .map(|row| {
            let installing = row
                .job
                .as_ref()
                .is_some_and(|j| j.kind == JobKind::Install && j.running());
            let failed = row
                .job
                .as_ref()
                .filter(|j| j.kind == JobKind::Install && j.failed())
                .map(|j| match &j.state {
                    JobState::Failed { .. } => j.words(&row.name),
                    _ => String::new(),
                });
            let route = route_of(row, browser);
            let state = if installing {
                AgentState::Installing
            } else if let Some(why) = failed {
                AgentState::Failed(why)
            } else if signing(&row.id) {
                AgentState::Signing
            } else {
                match (row.group, row.need) {
                    (Group::Ready | Group::Test, _) => AgentState::Ready,
                    (Group::Unknown, _) => AgentState::Unknown,
                    (_, Some(Need::SignIn)) => AgentState::NeedsSignIn,
                    (_, Some(Need::Update)) => AgentState::NeedsUpdate,
                    (Group::NotInstalled, _) | (_, Some(Need::Install)) => AgentState::NotInstalled,
                    (_, Some(Need::Setup)) => AgentState::Unusable,
                    (_, None) => AgentState::Ready,
                }
            };
            let action = match &state {
                AgentState::NeedsSignIn => route.map(Action::SignIn),
                AgentState::NotInstalled | AgentState::Failed(_)
                    if matches!(
                        row.install,
                        Some(InstallOffer::Now | InstallOffer::Review | InstallOffer::Legacy)
                    ) =>
                {
                    Some(Action::Install)
                }
                _ => None,
            };
            AgentLine {
                id: row.id.clone(),
                name: row.name.clone(),
                version: row.version_text.clone(),
                state,
                action,
            }
        })
        .collect()
}

/// "A", "A and B", "A, B and C".
fn list(names: &[&str]) -> String {
    match names {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// The finding: how many agents can take runs on this Mac, which, and what blocks each of the
/// others. It is worked out from the lines, so it changes as a sign-in or an install lands.
pub(crate) fn finding(lines: &[AgentLine]) -> String {
    let total = lines.len();
    let ready: Vec<&str> = lines
        .iter()
        .filter(|l| l.state.can_take_runs())
        .map(|l| l.name.as_str())
        .collect();
    let blocked: Vec<String> = lines
        .iter()
        .filter(|l| !l.state.can_take_runs())
        .map(|l| format!("{} {}", l.name, l.state.phrase()))
        .collect();
    let head = match (total, ready.len()) {
        (0, _) => return "No agents were found on this Mac.".to_owned(),
        (1, 0) => return format!("{} {}.", lines[0].name, lines[0].state.phrase()),
        (_, 0) => format!("None of the {total} agents can take runs on this Mac yet."),
        (n, r) if r == n && n == 1 => format!("{} can take runs on this Mac.", ready[0]),
        (n, r) if r == n => format!(
            "All {n} agents can take runs on this Mac: {}.",
            list(&ready)
        ),
        (n, r) => format!(
            "{r} of {n} agents can take runs on this Mac: {}.",
            list(&ready)
        ),
    };
    if blocked.is_empty() {
        head
    } else {
        format!("{head} {}.", capitalize(&blocked.join("; ")))
    }
}

/// The first letter in capitals; names already are.
fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |c| {
        c.to_uppercase().collect::<String>() + chars.as_str()
    })
}

/// How many of the agents cannot take runs and are not on their way.
pub(crate) fn blocked(lines: &[AgentLine]) -> usize {
    lines
        .iter()
        .filter(|l| !l.state.can_take_runs() && !l.state.working())
        .count()
}

/// A project Reno knows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ProjectLine {
    pub(crate) name: String,
    pub(crate) path: std::path::PathBuf,
}

/// What Reno found in a project's folder, in words: what it is and where runs will work.
pub(crate) fn found_words(name: &str, facts: &reno_client::places::ProjectFacts) -> String {
    if facts.git {
        format!(
            "{name} \u{b7} a git repository on {} \u{b7} runs get their own worktree",
            facts.branch.as_deref().unwrap_or("its default branch")
        )
    } else {
        format!("{name} \u{b7} not a git repository \u{b7} runs work in the folder itself")
    }
}

/// The five steps of the wizard.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    Welcome,
    Look,
    Agents,
    Project,
    Done,
}

impl Step {
    pub(crate) const ALL: [Self; 5] = [
        Self::Welcome,
        Self::Look,
        Self::Agents,
        Self::Project,
        Self::Done,
    ];

    pub(crate) fn index(self) -> usize {
        Self::ALL.iter().position(|s| *s == self).unwrap_or(0)
    }

    pub(crate) fn next(self) -> Self {
        Self::ALL[(self.index() + 1).min(Self::ALL.len() - 1)]
    }

    pub(crate) fn back(self) -> Self {
        Self::ALL[self.index().saturating_sub(1)]
    }

    pub(crate) fn parse(word: &str) -> Option<Self> {
        match word {
            "welcome" => Some(Self::Welcome),
            "look" => Some(Self::Look),
            "agents" => Some(Self::Agents),
            "project" => Some(Self::Project),
            "done" => Some(Self::Done),
            _ => None,
        }
    }
}

/// The four things setup covers, for the checklist and the one page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Item {
    Appearance,
    Agents,
    Project,
    Notifications,
}

impl Item {
    pub(crate) const ALL: [Self; 4] = [
        Self::Appearance,
        Self::Agents,
        Self::Project,
        Self::Notifications,
    ];

    pub(crate) const fn title(self) -> &'static str {
        match self {
            Self::Appearance => "Appearance",
            Self::Agents => "Agents",
            Self::Project => "First project",
            Self::Notifications => "Notifications",
        }
    }

    pub(crate) fn parse(word: &str) -> Option<Self> {
        match word {
            "appearance" | "look" => Some(Self::Appearance),
            "agents" => Some(Self::Agents),
            "project" => Some(Self::Project),
            "notifications" => Some(Self::Notifications),
            _ => None,
        }
    }
}

/// Where a person is in setup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Setup {
    /// The wizard's step.
    pub(crate) step: Step,
    /// The checklist's open item.
    pub(crate) open_item: Option<Item>,
    /// The project chosen, by name.
    pub(crate) project: Option<String>,
    /// The look was looked at (a choice was made, or "Keep this look").
    pub(crate) looked: bool,
    /// Notifications were answered.
    pub(crate) notified: bool,
    /// The checklist card was put away.
    pub(crate) card_dismissed: bool,
}

impl Setup {
    pub(crate) fn new() -> Self {
        Self {
            step: Step::Welcome,
            open_item: None,
            project: None,
            looked: false,
            notified: false,
            card_dismissed: false,
        }
    }

    /// Whether `item` is done. Agents are done when none is blocked.
    pub(crate) fn is_done(&self, item: Item, blocked_agents: usize) -> bool {
        match item {
            Item::Appearance => self.looked,
            Item::Agents => blocked_agents == 0,
            Item::Project => self.project.is_some(),
            Item::Notifications => self.notified,
        }
    }

    pub(crate) fn done_count(&self, blocked_agents: usize) -> usize {
        Item::ALL
            .iter()
            .filter(|i| self.is_done(**i, blocked_agents))
            .count()
    }

    /// "2 of 4".
    pub(crate) fn progress(&self, blocked_agents: usize) -> String {
        format!("{} of {}", self.done_count(blocked_agents), Item::ALL.len())
    }

    pub(crate) fn all_done(&self, blocked_agents: usize) -> bool {
        self.done_count(blocked_agents) == Item::ALL.len()
    }

    /// Opens `item`, or closes it when it is open.
    pub(crate) fn toggle(&mut self, item: Item) {
        self.open_item = if self.open_item == Some(item) {
            None
        } else {
            Some(item)
        };
    }
}

impl Default for Setup {
    fn default() -> Self {
        Self::new()
    }
}

/// The connecting gate stays up at least this long, so a daemon that answers at once does not
/// make it flash (ms).
pub(crate) const GATE_MIN_MS: u64 = 1_200;
/// How long the gate takes to fade into the app (ms).
pub(crate) const GATE_FADE_MS: u64 = 650;

/// A step of the gate's checklist shows as done no sooner than this after the gate went up (ms),
/// so a daemon that answers at once still reads as three things being done, one after another:
/// Starting renod, Loading threads, Reading agents.
pub(crate) const GATE_STEP_MS: [u64; 3] = [300, 700, 1_100];

/// Which of the gate's three steps are done: each when what it waits for has arrived and its
/// moment has come.
pub(crate) fn gate_steps(connected: bool, threads: bool, agents: bool, up_ms: u64) -> [bool; 3] {
    [
        connected && up_ms >= GATE_STEP_MS[0],
        threads && up_ms >= GATE_STEP_MS[1],
        agents && up_ms >= GATE_STEP_MS[2],
    ]
}

/// Whether the gate may lift: it has been up long enough, the daemon answered one way or the
/// other (a daemon that is not there must show Home's offline notice and Retry, so the gate
/// never hides that), and a hook is not holding it.
pub(crate) fn gate_lifts(
    up_ms: u64,
    connection: reno_client::ConnectionStatus,
    held: bool,
) -> bool {
    !held && up_ms >= GATE_MIN_MS && connection != reno_client::ConnectionStatus::Connecting
}

/// Whether setup shows at all: only when it was not finished or skipped, or a hook asks.
pub(crate) fn should_show(onboarded: bool, forced: bool) -> bool {
    forced || !onboarded
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(name: &str, state: AgentState) -> AgentLine {
        AgentLine {
            id: name.to_lowercase(),
            name: name.to_owned(),
            version: None,
            state,
            action: None,
        }
    }

    #[test]
    fn the_finding_names_who_can_take_runs_and_what_blocks_the_others() {
        let lines = [
            line("Claude Code", AgentState::Ready),
            line("Codex", AgentState::Ready),
            line("Pi", AgentState::Ready),
            line("Grok", AgentState::NeedsSignIn),
            line("Cursor", AgentState::NotInstalled),
        ];
        assert_eq!(
            finding(&lines),
            "3 of 5 agents can take runs on this Mac: Claude Code, Codex and Pi. \
             Grok needs signing in; Cursor is not installed."
        );
        assert_eq!(blocked(&lines), 2);
    }

    #[test]
    fn the_finding_follows_the_agents_as_they_get_ready() {
        let mut lines = vec![
            line("Claude Code", AgentState::Ready),
            line("Codex", AgentState::Ready),
            line("Grok", AgentState::NeedsSignIn),
            line("Cursor", AgentState::NotInstalled),
        ];
        assert_eq!(
            finding(&lines),
            "2 of 4 agents can take runs on this Mac: Claude Code and Codex. \
             Grok needs signing in; Cursor is not installed."
        );
        lines[2].state = AgentState::Signing;
        assert_eq!(blocked(&lines), 1, "an agent on its way is not blocked");
        assert!(finding(&lines).contains("Grok is signing in"));
        lines[2].state = AgentState::Ready;
        lines[3].state = AgentState::Ready;
        assert_eq!(
            finding(&lines),
            "All 4 agents can take runs on this Mac: Claude Code, Codex, Grok and Cursor."
        );
        assert_eq!(blocked(&lines), 0);
    }

    #[test]
    fn the_finding_has_words_for_none_one_and_nothing() {
        assert_eq!(finding(&[]), "No agents were found on this Mac.");
        assert_eq!(
            finding(&[line("Codex", AgentState::NeedsSignIn)]),
            "Codex needs signing in."
        );
        assert_eq!(
            finding(&[line("Codex", AgentState::Ready)]),
            "Codex can take runs on this Mac."
        );
        assert_eq!(
            finding(&[
                line("Codex", AgentState::NeedsSignIn),
                line("Pi", AgentState::NeedsUpdate)
            ]),
            "None of the 2 agents can take runs on this Mac yet. \
             Codex needs signing in; Pi needs an update."
        );
    }

    #[test]
    fn what_was_found_says_where_runs_will_work() {
        use reno_client::places::ProjectFacts;
        let git = ProjectFacts {
            git: true,
            branch: Some("main".into()),
        };
        assert_eq!(
            found_words("reno", &git),
            "reno \u{b7} a git repository on main \u{b7} runs get their own worktree"
        );
        let plain = ProjectFacts {
            git: false,
            branch: None,
        };
        assert_eq!(
            found_words("notes", &plain),
            "notes \u{b7} not a git repository \u{b7} runs work in the folder itself"
        );
    }

    #[test]
    fn the_wizard_walks_five_steps_and_stops_at_both_ends() {
        assert_eq!(Step::Welcome.back(), Step::Welcome);
        assert_eq!(Step::Welcome.next(), Step::Look);
        assert_eq!(Step::Project.next(), Step::Done);
        assert_eq!(Step::Done.next(), Step::Done);
        assert_eq!(Step::Done.index(), 4);
        assert_eq!(Step::parse("agents"), Some(Step::Agents));
        assert_eq!(Step::parse("sideways"), None);
    }

    #[test]
    fn progress_counts_what_is_done() {
        let mut setup = Setup::new();
        assert_eq!(setup.progress(2), "0 of 4");
        assert_eq!(setup.progress(0), "1 of 4", "no blocked agent is done");
        setup.looked = true;
        setup.project = Some("reno".into());
        assert_eq!(setup.progress(2), "2 of 4");
        setup.notified = true;
        assert!(!setup.all_done(2));
        assert!(setup.all_done(0));
    }

    #[test]
    fn an_item_opens_and_closes_and_one_is_open_at_a_time() {
        let mut setup = Setup::new();
        setup.toggle(Item::Agents);
        assert_eq!(setup.open_item, Some(Item::Agents));
        setup.toggle(Item::Project);
        assert_eq!(setup.open_item, Some(Item::Project));
        setup.toggle(Item::Project);
        assert_eq!(setup.open_item, None);
    }

    #[test]
    fn the_gate_checklist_goes_one_step_at_a_time_and_waits_for_what_it_needs() {
        assert_eq!(gate_steps(true, true, true, 0), [false, false, false]);
        assert_eq!(gate_steps(true, true, true, 500), [true, false, false]);
        assert_eq!(gate_steps(true, true, true, 800), [true, true, false]);
        assert_eq!(gate_steps(true, true, true, 1_200), [true, true, true]);
        assert_eq!(
            gate_steps(false, true, true, 5_000),
            [false, true, true],
            "a step is not done before its answer has come"
        );
    }

    #[test]
    fn the_gate_lifts_when_the_daemon_has_answered_and_it_has_been_seen() {
        use reno_client::ConnectionStatus::{Connected, Connecting, Disconnected};
        assert!(
            !gate_lifts(0, Connected, false),
            "not before it can be read"
        );
        assert!(
            !gate_lifts(GATE_MIN_MS, Connecting, false),
            "not while connecting"
        );
        assert!(gate_lifts(GATE_MIN_MS, Connected, false));
        assert!(
            gate_lifts(GATE_MIN_MS, Disconnected, false),
            "offline must show Home's notice, not hide behind the gate"
        );
        assert!(!gate_lifts(60_000, Connected, true), "a hook holds it");
    }

    #[test]
    fn setup_shows_until_it_is_finished_or_skipped_and_a_hook_can_force_it() {
        assert!(should_show(false, false));
        assert!(!should_show(true, false));
        assert!(should_show(true, true));
    }
}
