//! The ACP registry group of the Executors page (UI brief 2026-11-18, "Executors · add agents
//! from the ACP registry"): which agents can be added, what each will need, and where an add
//! is. From `executor.registry.list` joined with the live executors feed.
//!
//! The list is one answer (the index with this device's build and readiness); what is added,
//! adding or failed is read from the executors feed, which is live, joined on the entry's
//! executor id. An added agent leaves this group: it is an ordinary executor line then.
//!
//! What the index entry does not carry is never invented. Whether an agent signs in, with
//! keys, in Terminal or in a browser, is known only from the agent itself, which the daemon asks
//! once it is added; [`SignIn::Unknown`] says so, and the counts that depend on it ("runs once
//! added", "needs a sign-in") appear only when some entry is known.
//!
//! Pure: callers pass `now_ms`, so every state is a plain test.

use reno_client::State;
use reno_proto::{
    ExecutorInfo, ExecutorRegistryListResult, ExecutorSource, InstallStatus, JobPhase,
    RegistryDistribution, RegistryEntry, RegistryReadiness,
};

use super::executors::{JobState, JobView, job_view};
use crate::summary;

/// What the app holds of the registry list: the last good answer and how the next one went.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RegistryData {
    /// The last answer. Kept when a later fetch fails: the copy still reads.
    pub(crate) listing: Option<ExecutorRegistryListResult>,
    /// Why the last fetch failed; cleared by the next answer.
    pub(crate) failure: Option<String>,
    /// A fetch is in flight.
    pub(crate) fetching: bool,
}

/// The segmented filter.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Filter {
    #[default]
    All,
    RunsOnceAdded,
    NeedsSignIn,
    CantRun,
}

impl Filter {
    const fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::RunsOnceAdded => "Runs once added",
            Self::NeedsSignIn => "Needs a sign-in",
            Self::CantRun => "Can't run here",
        }
    }

    /// The accessible name of the segment's button.
    pub(crate) const fn key(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::RunsOnceAdded => "runs",
            Self::NeedsSignIn => "sign-in",
            Self::CantRun => "cant-run",
        }
    }
}

/// How an agent signs in, once its own `initialize` has said so.
#[derive(Clone, Debug, PartialEq, Eq)]
// The index entry carries none of this yet: only `Unknown` is produced until the daemon passes
// the agent's auth methods along (branch `registry-signin`, see `sign_in_of`).
#[allow(dead_code)]
pub(crate) enum SignIn {
    /// Not known before the agent is added and asked.
    Unknown,
    /// Nothing to sign in to.
    Nothing,
    /// Env vars it names, kept in Reno's environment for this agent only.
    Keys(Vec<String>),
    /// A command the person runs in Terminal.
    Terminal(String),
    /// The agent's own ACP `authenticate`, in the browser.
    Browser,
}

/// The one place the sign-in kind of a registry agent is decided. Today the index entry has no
/// such fact, so it is always [`SignIn::Unknown`]; `registry-signin` extends this with the
/// agent's auth methods.
pub(crate) fn sign_in_of(_entry: &RegistryEntry) -> SignIn {
    SignIn::Unknown
}

/// Why an agent cannot be added on this Mac.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CantRun {
    NeedsNode(Option<String>),
    NeedsUv(Option<String>),
    NoBuild,
}

impl CantRun {
    /// "needs uv: brew install uv", "needs Node.js", "no build for this Mac".
    pub(crate) fn words(&self) -> String {
        let with = |what: &str, fix: &Option<String>| match fix {
            Some(fix) => format!("needs {what}: {fix}"),
            None => format!("needs {what}"),
        };
        match self {
            Self::NeedsNode(fix) => with("Node.js", fix),
            Self::NeedsUv(fix) => with("uv", fix),
            Self::NoBuild => "no build for this Mac".to_owned(),
        }
    }
}

/// Where a line is in its life as a registry agent. An added agent is not a line here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    Available,
    CantRun(CantRun),
    /// Reno drives it itself, as this executor: listed, never opened.
    BuiltIn {
        as_name: String,
    },
    /// An add is queued or running.
    Adding(JobView),
    /// The add failed: the reason, with Show log and Retry.
    Failed(JobView),
}

/// One key and value of an opened line's facts.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FactRow {
    pub(crate) key: &'static str,
    pub(crate) value: String,
    pub(crate) tone: FactTone,
    /// The value is a link to open.
    pub(crate) link: Option<String>,
    /// Mono (versions, paths).
    pub(crate) mono: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FactTone {
    Plain,
    Warning,
}

/// What a step card's control is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Control {
    Add,
    /// The env editor: only on an added agent's line.
    Keys,
    OpenTerminal(String),
    SignInBrowser,
    Check,
    /// The step has nothing to press ("Nothing to sign in", or not known yet).
    None,
}

/// One numbered step of an opened line: number, title and detail, one control.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct StepCard {
    pub(crate) number: u8,
    pub(crate) title: &'static str,
    pub(crate) detail: String,
    /// The command to show as a chip (Terminal sign-in).
    pub(crate) command: Option<String>,
    pub(crate) control: Control,
    /// "Disabled until step 1 is done."
    pub(crate) enabled: bool,
}

/// The three numbered steps, in the real order: add, sign in / key / nothing, check.
pub(crate) fn steps_of(name: &str, sign_in: &SignIn, added: bool, adding: bool) -> Vec<StepCard> {
    let add = StepCard {
        number: 1,
        title: "Add it",
        detail: format!(
            "Reno installs it in its own folder. Once added, {name} can read and write files and run commands in a run's worktree, under Reno's approvals and access mode."
        ),
        command: None,
        control: Control::Add,
        enabled: !added && !adding,
    };
    let second = match sign_in {
        SignIn::Unknown => (
            "Sign in or give it keys",
            format!(
                "Reno asks {name} how it signs in once it is added, and the step shows here and on its line."
            ),
            None,
            Control::None,
        ),
        SignIn::Nothing => (
            "Nothing to sign in",
            format!("{name} starts without an account or a key."),
            None,
            Control::None,
        ),
        SignIn::Keys(vars) => (
            "Give it its keys",
            format!(
                "{} \u{b7} kept in Reno's environment for this agent only, never in the agent's config, never logged.",
                vars.join(", ")
            ),
            None,
            Control::Keys,
        ),
        SignIn::Terminal(command) => (
            "Sign in, in Terminal",
            "Reno opens Terminal with the agent's own command.".to_owned(),
            Some(command.clone()),
            Control::OpenTerminal(command.clone()),
        ),
        SignIn::Browser => (
            "Sign in, in your browser",
            format!("{name}'s own sign-in opens in your browser."),
            None,
            Control::SignInBrowser,
        ),
    };
    let sign = StepCard {
        number: 2,
        title: second.0,
        detail: second.1,
        command: second.2,
        // Only when there is something to press, and only once added.
        enabled: added && second.3 != Control::None,
        control: second.3,
    };
    let check = StepCard {
        number: 3,
        title: "Check it",
        detail: "Reno starts it once to confirm it answers and is signed in. Only when you ask."
            .to_owned(),
        command: None,
        control: Control::Check,
        enabled: added,
    };
    vec![add, sign, check]
}

/// One agent of the registry group.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RegistryLine {
    /// The registry's id (what add and remove take).
    pub(crate) id: String,
    pub(crate) executor: String,
    pub(crate) name: String,
    pub(crate) version: String,
    pub(crate) description: String,
    /// The registry's one-ink SVG, when it has one.
    pub(crate) icon: Option<String>,
    pub(crate) phase: Phase,
    pub(crate) sign_in: SignIn,
    /// What it needs, in faint at the right of the line.
    pub(crate) needs: String,
    pub(crate) facts: Vec<FactRow>,
    /// The numbered steps of the opened line; none for a built-in or one that cannot run.
    pub(crate) steps: Vec<StepCard>,
    /// The danger line of an opened line that cannot run.
    pub(crate) cant_run: Option<String>,
}

impl RegistryLine {
    pub(crate) fn built_in(&self) -> bool {
        matches!(self.phase, Phase::BuiltIn { .. })
    }

    /// Whether the line opens: a built-in one does not.
    pub(crate) fn opens(&self) -> bool {
        !self.built_in()
    }

    /// What the middle of the line says while an add runs or has failed:
    /// "Adding \u{b7} Downloading 97 MiB", "Add failed".
    pub(crate) fn progress_words(&self) -> Option<String> {
        match &self.phase {
            Phase::Adding(job) => Some(match &job.state {
                JobState::Running { .. } => "Adding".to_owned(),
                _ => "Add queued".to_owned(),
            }),
            Phase::Failed(_) => Some("Add failed".to_owned()),
            _ => None,
        }
    }
}

/// How an agent's fit reads for the filter and the finding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fit {
    Runs,
    NeedsSignIn,
    CantRun,
    /// Can be added, but how it signs in is not known before it is added.
    Unknown,
}

fn fit_of(line: &RegistryLine) -> Option<Fit> {
    if line.built_in() {
        return None;
    }
    Some(match (&line.phase, &line.sign_in) {
        (Phase::CantRun(_), _) => Fit::CantRun,
        (_, SignIn::Nothing) => Fit::Runs,
        (_, SignIn::Unknown) => Fit::Unknown,
        _ => Fit::NeedsSignIn,
    })
}

/// One segment of the filter: "Can't run here 5".
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Segment {
    pub(crate) filter: Filter,
    pub(crate) count: usize,
    pub(crate) pressed: bool,
}

impl Segment {
    pub(crate) fn label(&self) -> String {
        format!("{} {}", self.filter.label(), self.count)
    }
}

/// How the list as a whole is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ListState {
    /// Asked, no answer yet.
    Loading,
    /// No answer and none held: the reason, with Retry.
    Failed(String),
    Ready,
}

/// The "Your own agent\u{2026}" line's form.
pub(crate) fn own_agent_ready(name: &str, command: &str) -> bool {
    !name.trim().is_empty() && !command.trim().is_empty()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RegistryView {
    pub(crate) state: ListState,
    /// The agents in the group, built-in ones left out: "From the ACP registry \u{b7} N".
    pub(crate) count: usize,
    /// "36 agents in the registry \u{b7} 5 can't run on this Mac".
    pub(crate) finding: String,
    pub(crate) segments: Vec<Segment>,
    /// "registry checked 3m ago", or "offline \u{b7} showing the copy from 3m ago".
    pub(crate) checked: Option<String>,
    pub(crate) offline: bool,
    /// Lines after the filter and the search, alphabetical.
    pub(crate) lines: Vec<RegistryLine>,
    /// A search or filter matches nothing: "Nothing matches".
    pub(crate) nothing_matches: bool,
    pub(crate) refreshing: bool,
}

fn ago(now_ms: u64, at_ms: u64) -> String {
    summary::ago_label(now_ms.saturating_sub(at_ms))
}

fn kind_words(entry: &RegistryEntry) -> String {
    match (entry.kind, &entry.host) {
        (Some(RegistryDistribution::Binary), Some(host)) => format!("a download from {host}"),
        (Some(RegistryDistribution::Binary), None) => "a download".to_owned(),
        (Some(RegistryDistribution::Npx), _) => "an npm package, run with Node.js".to_owned(),
        (Some(RegistryDistribution::Uvx), _) => "a Python package, run with uv".to_owned(),
        (None, _) => "no build for this Mac".to_owned(),
    }
}

/// The short form for the faint column: "npm package", "download \u{b7} github.com".
fn kind_short(entry: &RegistryEntry) -> String {
    match (entry.kind, &entry.host) {
        (Some(RegistryDistribution::Binary), Some(host)) => format!("download from {host}"),
        (Some(RegistryDistribution::Binary), None) => "download".to_owned(),
        (Some(RegistryDistribution::Npx), _) => "npm package".to_owned(),
        (Some(RegistryDistribution::Uvx), _) => "Python package".to_owned(),
        (None, _) => String::new(),
    }
}

fn facts_of(entry: &RegistryEntry) -> Vec<FactRow> {
    let plain = |key, value: String| FactRow {
        key,
        value,
        tone: FactTone::Plain,
        link: None,
        mono: false,
    };
    let mut facts = vec![FactRow {
        mono: true,
        ..plain("Version", entry.version.clone())
    }];
    if entry.kind.is_some() {
        facts.push(plain("Installs as", kind_words(entry)));
    }
    match (entry.kind, entry.checksum) {
        (Some(RegistryDistribution::Binary), Some(true)) => {
            facts.push(plain("Checked", "against the registry's sha256".to_owned()))
        }
        (Some(RegistryDistribution::Binary), _) => facts.push(FactRow {
            tone: FactTone::Warning,
            ..plain(
                "Checked",
                "not checked: the registry publishes no checksum".to_owned(),
            )
        }),
        (Some(RegistryDistribution::Npx), _) => facts.push(plain(
            "Checked",
            format!(
                "exact version {} from npm, no install scripts",
                entry.version
            ),
        )),
        (Some(RegistryDistribution::Uvx), _) => facts.push(plain(
            "Checked",
            format!("exact version {} from PyPI", entry.version),
        )),
        (None, _) => {}
    }
    if let Some(link) = entry.repository.as_ref().or(entry.website.as_ref()) {
        facts.push(FactRow {
            link: Some(link.clone()),
            ..plain(
                "Source",
                link.trim_start_matches("https://")
                    .trim_start_matches("http://")
                    .to_owned(),
            )
        });
    }
    facts.push(plain(
        "Models",
        format!("from {}'s own account or keys, not Reno's", entry.name),
    ));
    let by = entry.authors.join(", ");
    let by = match (by.is_empty(), &entry.license) {
        (true, Some(license)) => license.clone(),
        (false, Some(license)) => format!("{by} \u{b7} {license}"),
        (_, None) => by,
    };
    if !by.is_empty() {
        facts.push(plain("By", by));
    }
    if entry.kind.is_some() {
        facts.push(FactRow {
            mono: true,
            ..plain("Lives in", format!("Reno's agents folder, {}/", entry.id))
        });
    }
    facts
}

/// The live executor of an entry, when it has been registered by an add.
fn live<'a>(state: &'a State, entry: &RegistryEntry) -> Option<&'a ExecutorInfo> {
    state
        .executors()
        .iter()
        .find(|e| e.descriptor.id == entry.executor && e.source == ExecutorSource::Registry)
}

fn installed(info: &ExecutorInfo) -> bool {
    matches!(
        info.health.as_ref().map(|h| &h.installed),
        Some(InstallStatus::Installed { .. })
    )
}

/// The line of one entry, or `None` when it is not in this group: the person's own agent, a
/// delisted one, or one that is added (an ordinary executor line now).
fn line_of(state: &State, entry: &RegistryEntry, now_ms: u64) -> Option<RegistryLine> {
    if entry.own || entry.delisted {
        return None;
    }
    let info = live(state, entry);
    let phase = if let Some(built_in) = &entry.built_in {
        Phase::BuiltIn {
            as_name: summary::executor_name(built_in.as_str()),
        }
    } else if let Some(info) = info {
        if installed(info) {
            return None;
        }
        match info.job.as_ref() {
            Some(job) if matches!(job.phase, JobPhase::Failed { .. }) => {
                Phase::Failed(job_view(job, now_ms, info.maintenance.as_ref()))
            }
            Some(job) => Phase::Adding(job_view(job, now_ms, info.maintenance.as_ref())),
            None => available_or_cant_run(entry),
        }
    } else {
        available_or_cant_run(entry)
    };
    let sign_in = sign_in_of(entry);
    let needs = match &phase {
        Phase::CantRun(why) => why.words(),
        Phase::BuiltIn { .. } => String::new(),
        _ => kind_short(entry),
    };
    let cant_run = match &phase {
        Phase::CantRun(why) => Some(why.words()),
        _ => None,
    };
    let steps = if matches!(
        phase,
        Phase::Available | Phase::Adding(_) | Phase::Failed(_)
    ) {
        steps_of(
            &entry.name,
            &sign_in,
            false,
            matches!(phase, Phase::Adding(_)),
        )
    } else {
        vec![]
    };
    Some(RegistryLine {
        id: entry.id.clone(),
        executor: entry.executor.to_string(),
        name: entry.name.clone(),
        version: entry.version.clone(),
        description: entry.description.clone(),
        icon: entry.icon.clone(),
        phase,
        sign_in,
        needs,
        facts: facts_of(entry),
        steps,
        cant_run,
    })
}

fn available_or_cant_run(entry: &RegistryEntry) -> Phase {
    match entry.readiness {
        RegistryReadiness::Ready => Phase::Available,
        RegistryReadiness::NeedsNode => Phase::CantRun(CantRun::NeedsNode(entry.fix.clone())),
        RegistryReadiness::NeedsUv => Phase::CantRun(CantRun::NeedsUv(entry.fix.clone())),
        RegistryReadiness::NoBuild => Phase::CantRun(CantRun::NoBuild),
    }
}

fn segments(lines: &[RegistryLine], active: Filter) -> Vec<Segment> {
    let count = |fit: Fit| lines.iter().filter(|l| fit_of(l) == Some(fit)).count();
    let all = lines.iter().filter(|l| !l.built_in()).count();
    let (runs, needs, cant) = (
        count(Fit::Runs),
        count(Fit::NeedsSignIn),
        count(Fit::CantRun),
    );
    // The sign-in segments exist only when the index says how agents sign in.
    let known = runs + needs > 0;
    let mut out = vec![Segment {
        filter: Filter::All,
        count: all,
        pressed: active == Filter::All,
    }];
    if known {
        out.push(Segment {
            filter: Filter::RunsOnceAdded,
            count: runs,
            pressed: active == Filter::RunsOnceAdded,
        });
        out.push(Segment {
            filter: Filter::NeedsSignIn,
            count: needs,
            pressed: active == Filter::NeedsSignIn,
        });
    }
    out.push(Segment {
        filter: Filter::CantRun,
        count: cant,
        pressed: active == Filter::CantRun,
    });
    out
}

fn finding(lines: &[RegistryLine]) -> String {
    let total = lines.iter().filter(|l| !l.built_in()).count();
    let count = |fit: Fit| lines.iter().filter(|l| fit_of(l) == Some(fit)).count();
    let mut out = format!(
        "{total} {} in the registry",
        if total == 1 { "agent" } else { "agents" }
    );
    for (n, what) in [
        (count(Fit::Runs), "run once added"),
        (count(Fit::NeedsSignIn), "need a sign-in or a key"),
        (count(Fit::CantRun), "can't run on this Mac"),
    ] {
        if n > 0 {
            out.push_str(&format!(" \u{b7} {n} {what}"));
        }
    }
    out
}

fn matches_query(line: &RegistryLine, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.split_whitespace().all(|word| {
        line.name.to_lowercase().contains(word)
            || line.id.to_lowercase().contains(word)
            || line.description.to_lowercase().contains(word)
    })
}

fn matches_filter(line: &RegistryLine, filter: Filter) -> bool {
    match filter {
        // Built-in agents stay in the list under All only.
        Filter::All => true,
        Filter::RunsOnceAdded => fit_of(line) == Some(Fit::Runs),
        Filter::NeedsSignIn => fit_of(line) == Some(Fit::NeedsSignIn),
        Filter::CantRun => fit_of(line) == Some(Fit::CantRun),
    }
}

/// What the registry group shows, or `None` for a daemon without `executor.registry`: the whole
/// group is hidden then.
pub(crate) fn build(
    state: &State,
    data: &RegistryData,
    filter: Filter,
    query: &str,
    now_ms: u64,
) -> Option<RegistryView> {
    if !state.supports(reno_proto::FEATURE_EXECUTOR_REGISTRY) {
        return None;
    }
    let Some(listing) = &data.listing else {
        return Some(RegistryView {
            state: match &data.failure {
                Some(reason) if !data.fetching => ListState::Failed(reason.clone()),
                _ => ListState::Loading,
            },
            count: 0,
            finding: String::new(),
            segments: vec![],
            checked: None,
            offline: false,
            lines: vec![],
            nothing_matches: false,
            refreshing: data.fetching,
        });
    };
    let mut all: Vec<RegistryLine> = listing
        .entries
        .iter()
        .filter_map(|entry| line_of(state, entry, now_ms))
        .collect();
    all.sort_by(|a, b| {
        a.name
            .to_lowercase()
            .cmp(&b.name.to_lowercase())
            .then_with(|| a.id.cmp(&b.id))
    });
    let offline = listing.offline || data.failure.is_some();
    let checked = Some(if offline {
        format!(
            "offline \u{b7} showing the copy from {}",
            ago(now_ms, listing.fetched_at_ms)
        )
    } else {
        format!("registry checked {}", ago(now_ms, listing.fetched_at_ms))
    });
    let segments = segments(&all, filter);
    // A filter that no longer exists (its segment is hidden) shows everything.
    let filter = if segments.iter().any(|s| s.filter == filter) {
        filter
    } else {
        Filter::All
    };
    let lines: Vec<RegistryLine> = all
        .iter()
        .filter(|l| matches_filter(l, filter) && matches_query(l, query))
        .cloned()
        .collect();
    Some(RegistryView {
        state: ListState::Ready,
        count: all.iter().filter(|l| !l.built_in()).count(),
        finding: finding(&all),
        segments,
        checked,
        offline,
        nothing_matches: lines.is_empty() && !all.is_empty(),
        lines,
        refreshing: data.fetching,
    })
}

/// "Remove Kilo? Its install and its keys are deleted from this Mac."
pub(crate) fn remove_words(name: &str) -> String {
    format!("Remove {name}? Its install and its keys are deleted from this Mac.")
}

/// What the line says when the daemon refuses the removal: a run is queued or live on it.
pub(crate) fn remove_refused_words(name: &str) -> String {
    format!("Can't remove {name} while a run on it is queued or live.")
}

/// The toast when an add ends: "Cline added. It can take runs." / "... One step needs you: sign in."
pub(crate) fn added_words(name: &str, sign_in: bool) -> String {
    if sign_in {
        format!("{name} added. One step needs you: sign in.")
    } else {
        format!("{name} added. It can take runs.")
    }
}

#[cfg(test)]
mod tests {
    use reno_client::Update;
    use reno_proto::{ExecutorId, ExecutorJob, JobKind};

    use super::*;

    const NOW: u64 = 1_000 * 3_600_000;
    const MIN: u64 = 60_000;

    fn entry(id: &str, name: &str) -> RegistryEntry {
        RegistryEntry {
            id: id.to_owned(),
            executor: format!("acp.{id}").parse().unwrap(),
            name: name.to_owned(),
            version: "1.0.0".to_owned(),
            description: format!("{name} the agent"),
            authors: vec!["Augment Code".to_owned()],
            license: Some("proprietary".to_owned()),
            website: None,
            repository: Some("https://github.com/augmentcode/auggie".to_owned()),
            icon: None,
            kind: Some(RegistryDistribution::Npx),
            readiness: RegistryReadiness::Ready,
            fix: None,
            host: None,
            checksum: None,
            built_in: None,
            added: None,
            adding: false,
            delisted: false,
            own: false,
        }
    }

    fn listing(entries: Vec<RegistryEntry>) -> RegistryData {
        RegistryData {
            listing: Some(ExecutorRegistryListResult {
                entries,
                fetched_at_ms: NOW - 3 * MIN,
                offline: false,
                error: None,
            }),
            ..RegistryData::default()
        }
    }

    fn with_registry() -> State {
        let mut state = State::new();
        state.apply(Update::Daemon(reno_client::DaemonInfo {
            hello: reno_proto::HelloResult {
                server_version: "t".into(),
                protocol_version: reno_proto::PROTOCOL_VERSION,
                features: vec![reno_proto::FEATURE_EXECUTOR_REGISTRY.to_owned()],
                pid: 1,
            },
            status: None,
        }));
        state
    }

    fn view(state: &State, data: &RegistryData) -> RegistryView {
        build(state, data, Filter::All, "", NOW).unwrap()
    }

    fn names(view: &RegistryView) -> Vec<&str> {
        view.lines.iter().map(|l| l.name.as_str()).collect()
    }

    fn executor(id: &str, installed: bool, job: Option<ExecutorJob>) -> ExecutorInfo {
        let mut info = crate::vm::executors::tests_support::info(id, None);
        info.source = ExecutorSource::Registry;
        info.health = Some(reno_proto::ExecutorHealth {
            installed: if installed {
                InstallStatus::Installed {
                    path: "/a".into(),
                    version: Some("1.0.0".into()),
                }
            } else {
                InstallStatus::NotInstalled {
                    why: reno_proto::NotInstalledReason::NotFound { hint: "x".into() },
                }
            },
            version_gate: reno_proto::VersionGate::Unchecked,
            signed_in: reno_proto::SignedIn::Unknown,
            models: reno_proto::ModelCatalog::curated(vec![]),
            checked_at_ms: NOW,
            plan_usage: None,
        });
        info.job = job;
        info
    }

    fn job(phase: JobPhase) -> ExecutorJob {
        ExecutorJob {
            id: "j".into(),
            kind: JobKind::Install,
            phase,
            started_at_ms: NOW - 7_000,
        }
    }

    #[test]
    fn an_old_daemon_hides_the_whole_group() {
        let state = State::new();
        assert!(
            build(
                &state,
                &listing(vec![entry("a", "A")]),
                Filter::All,
                "",
                NOW
            )
            .is_none()
        );
    }

    #[test]
    fn loading_failed_and_ready_say_which() {
        let state = with_registry();
        let loading = build(&state, &RegistryData::default(), Filter::All, "", NOW).unwrap();
        assert_eq!(loading.state, ListState::Loading);
        let fetching = RegistryData {
            fetching: true,
            ..RegistryData::default()
        };
        assert_eq!(view(&state, &fetching).state, ListState::Loading);
        let failed = RegistryData {
            failure: Some("registry fetch timed out".into()),
            ..RegistryData::default()
        };
        assert_eq!(
            view(&state, &failed).state,
            ListState::Failed("registry fetch timed out".into())
        );
        assert_eq!(
            view(&state, &listing(vec![entry("a", "A")])).state,
            ListState::Ready
        );
    }

    #[test]
    fn a_failed_refresh_keeps_the_copy_and_says_offline() {
        let state = with_registry();
        let mut data = listing(vec![entry("a", "A")]);
        data.failure = Some("could not reach the registry".into());
        let v = view(&state, &data);
        assert_eq!(v.state, ListState::Ready);
        assert!(v.offline);
        assert_eq!(
            v.checked.as_deref(),
            Some("offline \u{b7} showing the copy from 3m ago")
        );
        assert_eq!(names(&v), ["A"]);
    }

    #[test]
    fn the_daemon_reporting_offline_shows_the_cached_copys_age() {
        let state = with_registry();
        let mut data = listing(vec![entry("a", "A")]);
        data.listing.as_mut().unwrap().offline = true;
        let v = view(&state, &data);
        assert!(v.offline);
        assert_eq!(
            v.checked.as_deref(),
            Some("offline \u{b7} showing the copy from 3m ago")
        );
        let online = view(&state, &listing(vec![entry("a", "A")]));
        assert!(!online.offline);
        assert_eq!(online.checked.as_deref(), Some("registry checked 3m ago"));
    }

    #[test]
    fn lines_are_alphabetical_and_built_ins_stay_but_do_not_count() {
        let state = with_registry();
        let mut claude = entry("claude-acp", "Claude Agent");
        claude.built_in = Some("claude-code".parse().unwrap());
        let data = listing(vec![entry("zed", "Zed"), claude, entry("amp", "Amp")]);
        let v = view(&state, &data);
        assert_eq!(names(&v), ["Amp", "Claude Agent", "Zed"]);
        assert_eq!(v.count, 2, "N leaves the built-in agent out");
        assert_eq!(v.finding, "2 agents in the registry");
        let built_in = &v.lines[1];
        assert_eq!(
            built_in.phase,
            Phase::BuiltIn {
                as_name: "Claude Code".into()
            }
        );
        assert!(!built_in.opens() && built_in.steps.is_empty());
        // Under a filter the built-in line is gone: it stays under All only.
        let cant = build(&state, &data, Filter::CantRun, "", NOW).unwrap();
        assert!(cant.lines.is_empty());
    }

    #[test]
    fn what_cannot_run_here_is_counted_and_says_why() {
        let state = with_registry();
        let mut uv = entry("fast", "Fast");
        uv.kind = Some(RegistryDistribution::Uvx);
        uv.readiness = RegistryReadiness::NeedsUv;
        uv.fix = Some("brew install uv".into());
        let mut node = entry("gem", "Gem");
        node.readiness = RegistryReadiness::NeedsNode;
        let mut none = entry("win", "Win");
        none.kind = None;
        none.readiness = RegistryReadiness::NoBuild;
        let data = listing(vec![uv, node, none, entry("ok", "Ok")]);
        let v = view(&state, &data);
        assert_eq!(
            v.finding,
            "4 agents in the registry \u{b7} 3 can't run on this Mac"
        );
        let words: Vec<_> = v.lines.iter().filter_map(|l| l.cant_run.clone()).collect();
        assert_eq!(
            words,
            [
                "needs uv: brew install uv",
                "needs Node.js",
                "no build for this Mac"
            ]
        );
        let cant = build(&state, &data, Filter::CantRun, "", NOW).unwrap();
        assert_eq!(names(&cant), ["Fast", "Gem", "Win"]);
        // Its opened line has no steps: a danger line instead.
        assert!(cant.lines.iter().all(|l| l.steps.is_empty()));
        assert!(v.lines.iter().find(|l| l.name == "Ok").unwrap().steps.len() == 3);
    }

    #[test]
    fn segments_hide_the_sign_in_counts_until_the_index_says_how_agents_sign_in() {
        let state = with_registry();
        let v = view(&state, &listing(vec![entry("a", "A"), entry("b", "B")]));
        let labels: Vec<String> = v.segments.iter().map(Segment::label).collect();
        assert_eq!(labels, ["All 2", "Can't run here 0"]);
        assert!(v.segments[0].pressed && !v.segments[1].pressed);

        // With the facts, all four segments and the finding of the brief.
        let line = |id: &str, sign_in: SignIn, cant: bool| {
            let mut l = line_of(&state, &entry(id, id), NOW).unwrap();
            l.sign_in = sign_in;
            if cant {
                l.phase = Phase::CantRun(CantRun::NoBuild);
            }
            l
        };
        let lines = vec![
            line("a", SignIn::Nothing, false),
            line("b", SignIn::Keys(vec!["K".into()]), false),
            line("c", SignIn::Terminal("c login".into()), false),
            line("d", SignIn::Browser, false),
            line("e", SignIn::Unknown, true),
        ];
        let labels: Vec<String> = segments(&lines, Filter::NeedsSignIn)
            .iter()
            .map(Segment::label)
            .collect();
        assert_eq!(
            labels,
            [
                "All 5",
                "Runs once added 1",
                "Needs a sign-in 3",
                "Can't run here 1"
            ]
        );
        assert_eq!(
            finding(&lines),
            "5 agents in the registry \u{b7} 1 run once added \u{b7} 3 need a sign-in or a key \u{b7} 1 can't run on this Mac"
        );
    }

    #[test]
    fn a_search_matches_name_id_and_description_and_says_when_nothing_does() {
        let state = with_registry();
        let data = listing(vec![entry("auggie", "Auggie CLI"), entry("kilo", "Kilo")]);
        let by = |q: &str| build(&state, &data, Filter::All, q, NOW).unwrap();
        assert_eq!(names(&by("AUG")), ["Auggie CLI"]);
        assert_eq!(names(&by("kilo the")), ["Kilo"]);
        let none = by("zzz");
        assert!(none.lines.is_empty() && none.nothing_matches);
        assert!(!by("").nothing_matches);
        // The counts do not follow the search.
        assert_eq!(none.count, 2);
    }

    #[test]
    fn an_added_agent_leaves_the_group_and_an_adding_one_shows_its_progress() {
        let mut state = with_registry();
        let data = listing(vec![entry("a", "A"), entry("b", "B"), entry("c", "C")]);
        state.apply(Update::Executors(vec![
            executor("acp.a", true, None),
            executor(
                "acp.b",
                false,
                Some(job(JobPhase::Running {
                    step: 1,
                    steps: 1,
                    last_line: Some("Downloading 97 MiB \u{b7} checking sha256".into()),
                    quiet_since_ms: None,
                })),
            ),
        ]));
        let v = view(&state, &data);
        assert_eq!(names(&v), ["B", "C"], "A is added: an ordinary line now");
        let b = &v.lines[0];
        assert_eq!(b.progress_words().as_deref(), Some("Adding"));
        let Phase::Adding(job) = &b.phase else {
            panic!("adding");
        };
        assert!(matches!(
            &job.state,
            JobState::Running { clock: Some(c), last_line: Some(l), .. }
                if c == "0:07" && l.contains("97 MiB")
        ));
        // Step 1 is not offered twice while it runs.
        assert!(!b.steps[0].enabled);
        assert!(v.lines[1].steps[0].enabled);
        assert_eq!(v.count, 2);
    }

    #[test]
    fn a_failed_add_keeps_its_line_with_the_reason() {
        let mut state = with_registry();
        let data = listing(vec![entry("b", "B")]);
        state.apply(Update::Executors(vec![executor(
            "acp.b",
            false,
            Some(job(JobPhase::Failed {
                failure: reno_proto::JobFailure::ExitCode,
                detail: "the download failed a sha256 check".into(),
                finished_at_ms: NOW,
            })),
        )]));
        let v = view(&state, &data);
        let line = &v.lines[0];
        assert_eq!(line.progress_words().as_deref(), Some("Add failed"));
        let Phase::Failed(job) = &line.phase else {
            panic!("failed");
        };
        assert!(matches!(&job.state, JobState::Failed { detail, .. } if detail.contains("sha256")));
    }

    #[test]
    fn own_delisted_and_added_entries_are_not_in_the_group() {
        let state = with_registry();
        let mut own = entry("own.mine", "mine");
        own.own = true;
        let mut old = entry("old", "Old");
        old.delisted = true;
        let v = view(&state, &listing(vec![own, old, entry("a", "A")]));
        assert_eq!(names(&v), ["A"]);
    }

    #[test]
    fn the_facts_of_an_opened_line_say_what_the_agent_is() {
        let state = with_registry();
        let mut bin = entry("goose", "goose");
        bin.kind = Some(RegistryDistribution::Binary);
        bin.host = Some("github.com".into());
        bin.checksum = Some(false);
        let line = line_of(&state, &bin, NOW).unwrap();
        let fact = |key: &str| line.facts.iter().find(|f| f.key == key).unwrap().clone();
        assert_eq!(fact("Version").value, "1.0.0");
        assert_eq!(fact("Installs as").value, "a download from github.com");
        let checked = fact("Checked");
        assert_eq!(
            checked.value,
            "not checked: the registry publishes no checksum"
        );
        assert_eq!(checked.tone, FactTone::Warning);
        assert_eq!(
            fact("Source").link.as_deref(),
            Some("https://github.com/augmentcode/auggie")
        );
        assert_eq!(
            fact("Models").value,
            "from goose's own account or keys, not Reno's"
        );
        assert_eq!(fact("By").value, "Augment Code \u{b7} proprietary");

        bin.checksum = Some(true);
        let line = line_of(&state, &bin, NOW).unwrap();
        let checked = line.facts.iter().find(|f| f.key == "Checked").unwrap();
        assert_eq!(checked.value, "against the registry's sha256");
        assert_eq!(checked.tone, FactTone::Plain);

        let npm = line_of(&state, &entry("auggie", "Auggie CLI"), NOW).unwrap();
        let installs = npm.facts.iter().find(|f| f.key == "Installs as").unwrap();
        assert_eq!(installs.value, "an npm package, run with Node.js");
        let checked = npm.facts.iter().find(|f| f.key == "Checked").unwrap();
        assert_eq!(
            checked.value,
            "exact version 1.0.0 from npm, no install scripts"
        );
    }

    #[test]
    fn the_steps_are_numbered_in_the_real_order_and_gate_on_add() {
        let steps = steps_of(
            "Auggie CLI",
            &SignIn::Terminal("auggie login".into()),
            false,
            false,
        );
        assert_eq!(
            steps
                .iter()
                .map(|s| (s.number, s.title))
                .collect::<Vec<_>>(),
            [(1, "Add it"), (2, "Sign in, in Terminal"), (3, "Check it")]
        );
        assert!(
            steps[0]
                .detail
                .contains("Auggie CLI can read and write files")
        );
        assert!(steps[0].enabled && !steps[1].enabled && !steps[2].enabled);
        assert_eq!(
            steps[1].control,
            Control::OpenTerminal("auggie login".into())
        );
        assert_eq!(steps[1].command.as_deref(), Some("auggie login"));
        // Once added, the others wake up and Add is spent.
        let added = steps_of(
            "Auggie CLI",
            &SignIn::Terminal("auggie login".into()),
            true,
            false,
        );
        assert!(!added[0].enabled && added[1].enabled && added[2].enabled);
    }

    #[test]
    fn step_two_is_each_of_its_kinds() {
        let two = |sign_in: SignIn| steps_of("X", &sign_in, true, false).remove(1);
        let nothing = two(SignIn::Nothing);
        assert_eq!(nothing.title, "Nothing to sign in");
        assert!(!nothing.enabled && nothing.control == Control::None);
        let keys = two(SignIn::Keys(vec!["ANTHROPIC_API_KEY".into(), "B".into()]));
        assert_eq!(keys.title, "Give it its keys");
        assert!(keys.detail.starts_with("ANTHROPIC_API_KEY, B"));
        assert!(keys.detail.contains("never logged") && keys.enabled);
        assert_eq!(keys.control, Control::Keys);
        assert_eq!(two(SignIn::Browser).title, "Sign in, in your browser");
        assert_eq!(two(SignIn::Browser).control, Control::SignInBrowser);
        // Before the agent says how it signs in, step 2 says so and offers nothing.
        let unknown = two(SignIn::Unknown);
        assert!(unknown.detail.contains("once it is added") && !unknown.enabled);
    }

    #[test]
    fn the_index_entry_alone_never_claims_a_sign_in_kind() {
        assert_eq!(sign_in_of(&entry("a", "A")), SignIn::Unknown);
    }

    #[test]
    fn the_words_of_remove_added_and_the_own_form() {
        assert_eq!(
            remove_words("Kilo"),
            "Remove Kilo? Its install and its keys are deleted from this Mac."
        );
        assert_eq!(
            remove_refused_words("Kilo"),
            "Can't remove Kilo while a run on it is queued or live."
        );
        assert_eq!(
            added_words("Cline", false),
            "Cline added. It can take runs."
        );
        assert_eq!(
            added_words("Cline", true),
            "Cline added. One step needs you: sign in."
        );
        assert!(own_agent_ready("mine", "my-agent --acp"));
        assert!(!own_agent_ready(" ", "my-agent") && !own_agent_ready("mine", ""));
        let _: ExecutorId = "acp.x".parse().unwrap();
    }
}
