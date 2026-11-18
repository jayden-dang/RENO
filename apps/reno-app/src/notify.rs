//! When the desktop is told something: the pure decision state behind banners and the Dock badge
//! (docs/design/2026-11-18-desktop-notifications-brief.md). No GPUI: a [`Snapshot`] of the state
//! mirror, the time, whether a Reno window is active and the [`Prefs`] go in; [`Effect`]s (post a
//! banner, withdraw one) and the badge label come out. `banners.rs` wires it to the store, the
//! windows and the clock.
//!
//! Ported in spirit from zeron's `desktop_notify.rs`: the first connected snapshot seeds every
//! baseline silently, a finish waits to stand before it rings, a connection is quiet for its first
//! seconds. New here: banners withdraw once what they announced is resolved, one banner per item
//! replaces the previous one, and a burst of banners of one kind rings once.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use reno_client::{ConnectionStatus, State};
use reno_notify::Banner;
use reno_proto::{ApprovalId, RunId, RunStatus, StopDetail, StopReason};

use crate::prefs::Prefs;
use crate::summary;

/// How long a success or a failure stands before it rings: a run retried or resumed meanwhile
/// takes the banner with it.
pub(crate) const FINISH_SETTLE: Duration = Duration::from_secs(2);
/// How long an interruption stands before it rings: the daemon often resumes the run.
pub(crate) const INTERRUPT_SETTLE: Duration = Duration::from_secs(5);
/// How long the connection is down before the daemon banner rings, and how long the app must have
/// been up for it to.
pub(crate) const DAEMON_QUIET: Duration = Duration::from_secs(5);
/// More banners of one kind than this in one pass ring as one.
pub(crate) const BURST_OVER: usize = 3;
/// Longest subject in "Wants to <subject>", in characters.
const SUBJECT_CHARS: usize = 120;
/// Longest run title in a banner's body (macOS shows two lines), in characters.
const WHAT_CHARS: usize = 120;
/// A stop reason longer than this is left out of "Failed: <reason>".
const REASON_CHARS: usize = 60;

pub(crate) const DAEMON_ID: &str = "daemon";

// --- What a banner opens -------------------------------------------------------------------

/// What clicking a banner opens. Banners carry it as a string: `run:<run id>`, `daemon`,
/// `burst:<kind>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Target {
    /// The run's screen.
    Run(RunId),
    /// Home (the daemon banner).
    Home,
    /// The Runs screen (a burst).
    Runs,
}

impl Target {
    pub(crate) fn encode(&self) -> String {
        match self {
            Self::Run(run) => format!("run:{run}"),
            Self::Home => DAEMON_ID.to_owned(),
            Self::Runs => "burst:runs".to_owned(),
        }
    }

    /// The target a banner's string names; `None` for a string this build does not know.
    pub(crate) fn parse(target: &str) -> Option<Self> {
        if target == DAEMON_ID {
            return Some(Self::Home);
        }
        if target.starts_with("burst:") {
            return Some(Self::Runs);
        }
        target
            .strip_prefix("run:")
            .and_then(|run| run.parse().ok())
            .map(Self::Run)
    }
}

// --- What the notifier is shown -------------------------------------------------------------

/// A run, as far as banners care.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunFact {
    pub(crate) id: RunId,
    pub(crate) status: RunStatus,
    pub(crate) title: String,
    pub(crate) bot: Option<String>,
    /// Why it stopped, in words, when the daemon said.
    pub(crate) reason: Option<String>,
    /// A person stopped it (the daemon settles such a run as interrupted before cancelled): that
    /// is a cancel, and a cancel never rings.
    pub(crate) by_person: bool,
}

/// A held action, as far as banners care.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ApprovalFact {
    pub(crate) id: ApprovalId,
    pub(crate) run: RunId,
    /// What is being approved, one line.
    pub(crate) subject: String,
}

/// The state mirror, as far as banners care.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub(crate) runs: Vec<RunFact>,
    pub(crate) approvals: Vec<ApprovalFact>,
    pub(crate) connection: ConnectionStatus,
    /// The run and approval watches have answered since the connection came up, so the lists are
    /// the daemon's, not what was left from before.
    pub(crate) loaded: bool,
}

impl Snapshot {
    /// A connected, loaded snapshot of these facts.
    #[cfg(test)]
    pub(crate) fn new(runs: Vec<RunFact>, approvals: Vec<ApprovalFact>) -> Self {
        Self {
            runs,
            approvals,
            connection: ConnectionStatus::Connected,
            loaded: true,
        }
    }

    pub(crate) fn from_state(state: &State) -> Self {
        use reno_client::{Feed, FeedState};
        let runs = state
            .runs()
            .into_iter()
            .map(|run| RunFact {
                id: run.run_id.clone(),
                status: run.status,
                title: run.title.clone(),
                bot: summary::run_bot(run).map(str::to_owned),
                reason: stop_words(run),
                by_person: summary::stopped_by_person(run),
            })
            .collect();
        let approvals = state
            .approvals()
            .iter()
            .map(|a| ApprovalFact {
                id: a.id.clone(),
                run: a.run_id.clone(),
                subject: summary::subject_line(&a.subject),
            })
            .collect();
        let answered = |feed| !matches!(state.feed(feed), FeedState::Loading);
        Self {
            runs,
            approvals,
            connection: state.connection(),
            loaded: answered(Feed::Runs) && answered(Feed::Approvals),
        }
    }

    fn online(&self) -> bool {
        self.connection == ConnectionStatus::Connected && self.loaded
    }

    fn run(&self, id: &RunId) -> Option<&RunFact> {
        self.runs.iter().find(|r| &r.id == id)
    }

    /// Held actions plus questions: the sidebar's count (`summary::attention(..).needs_you()`).
    pub(crate) fn needs_you(&self) -> usize {
        self.approvals.len()
            + self
                .runs
                .iter()
                .filter(|r| r.status == RunStatus::WaitingInput)
                .count()
    }
}

/// A short reason a run stopped, in words: what the daemon says on a refused start, else the
/// harness's stop reason when it is one a person can use.
fn stop_words(run: &reno_proto::RunSummary) -> Option<String> {
    if let Some(StopDetail::StartRefused { detail, .. }) = &run.stop {
        return Some(detail.clone());
    }
    match run.stop_reason? {
        StopReason::Refusal => Some("the model refused".to_owned()),
        StopReason::MaxTokens => Some("hit its output limit".to_owned()),
        StopReason::Completed | StopReason::Interrupted | StopReason::Error => None,
    }
}

// --- What comes out ------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Effect {
    Post(Banner),
    Withdraw(String),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Outcome {
    pub(crate) effects: Vec<Effect>,
    /// The Dock badge as it should be now: `None` is blank.
    pub(crate) badge: Option<String>,
}

/// The three kinds of banner a burst coalesces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Kind {
    NeedsYou,
    Finished,
    Problem,
}

impl Kind {
    pub(crate) const fn burst_word(self) -> &'static str {
        match self {
            Self::NeedsYou => "needs",
            Self::Finished => "finished",
            Self::Problem => "stopped",
        }
    }

    fn burst_title(self, n: usize) -> String {
        match self {
            Self::NeedsYou => format!("{n} runs need you"),
            Self::Finished => format!("{n} runs finished"),
            Self::Problem => format!("{n} runs stopped"),
        }
    }

    fn burst_id(self) -> String {
        format!("burst:{}", self.burst_word())
    }
}

/// What the badge says: the count of what needs you, blank at 0, `99+` above 99; blank while the
/// connection is down (a stale number is worse than none) and when the preference is off.
pub(crate) fn badge_label(snapshot: &Snapshot, prefs: &Prefs) -> Option<String> {
    if !prefs.dock_badge || snapshot.connection != ConnectionStatus::Connected {
        return None;
    }
    match snapshot.needs_you() {
        0 => None,
        n if n > 99 => Some("99+".to_owned()),
        n => Some(n.to_string()),
    }
}

/// `keeper · Fix the flaky test`: the bot and what the run does, cut to 80 characters. The title
/// alone for a run with no bot. A title that already leads with `<bot> · <repo>: ` loses that
/// lead, the bot being named once.
/// Who a banner is about: the run's bot, else "A run". Banner titles start with it ("keeper
/// finished"), short enough that macOS never cuts what happened.
pub(crate) fn who(run: Option<&RunFact>) -> String {
    run.and_then(|run| run.bot.clone())
        .unwrap_or_else(|| "A run".to_owned())
}

/// What the run is doing: its title without the bot prefix, on one line, for the banner's body.
pub(crate) fn what(run: &RunFact) -> String {
    let parts = summary::parse_title(&run.title);
    let what = if parts.bot.is_some() {
        parts.what
    } else {
        run.title.as_str()
    };
    one_line(what, WHAT_CHARS)
}

/// `text` with its first letter in capitals: a reason that reads mid-sentence ("the model
/// refused") as a line of its own.
fn upper_first(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map_or_else(String::new, |first| {
        first.to_uppercase().chain(chars).collect()
    })
}

/// `text` on one line (whitespace runs become one space), cut to `max` characters with `…`.
fn one_line(text: &str, max: usize) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= max {
        return line;
    }
    let mut cut: String = line.chars().take(max.saturating_sub(1)).collect();
    cut.truncate(cut.trim_end().len());
    cut.push('\u{2026}');
    cut
}

// --- The notifier --------------------------------------------------------------------------

/// A success, failure or interruption waiting to stand.
#[derive(Clone, Copy, Debug)]
struct Settling {
    status: RunStatus,
    due: Instant,
}

/// One banner that is due, before preferences and coalescing.
struct Due {
    kind: Kind,
    banner: Banner,
}

pub(crate) struct Notifier {
    up_since: Instant,
    seeded: bool,
    /// The status of every run seen, one baseline each.
    runs: HashMap<RunId, RunStatus>,
    /// The held actions of the last connected snapshot, with their runs.
    approvals: HashMap<ApprovalId, RunId>,
    /// Every held action ever seen: an id already seen never rings twice.
    seen_approvals: HashSet<ApprovalId>,
    settling: BTreeMap<RunId, Settling>,
    ever_connected: bool,
    online: bool,
    down_since: Option<Instant>,
    /// This outage is dealt with: the daemon banner rang, or was held. It is never replayed.
    daemon_handled: bool,
    daemon_posted: bool,
    burst_needs_posted: bool,
}

impl Notifier {
    /// A notifier for an app that started at `now`.
    pub(crate) fn new(now: Instant) -> Self {
        Self {
            up_since: now,
            seeded: false,
            runs: HashMap::new(),
            approvals: HashMap::new(),
            seen_approvals: HashSet::new(),
            settling: BTreeMap::new(),
            ever_connected: false,
            online: false,
            down_since: None,
            daemon_handled: false,
            daemon_posted: false,
            burst_needs_posted: false,
        }
    }

    /// When the next pending banner is due, if one is: call [`Notifier::step`] again then.
    pub(crate) fn next_due(&self) -> Option<Instant> {
        let settles = self
            .online
            .then(|| self.settling.values().map(|s| s.due).min())
            .flatten();
        let daemon = self.daemon_due();
        match (settles, daemon) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn daemon_due(&self) -> Option<Instant> {
        if self.daemon_handled {
            return None;
        }
        // Down for `DAEMON_QUIET`, and the app up that long (an outage seen in the app's first
        // seconds may only be it still connecting).
        self.down_since
            .map(|down| (down + DAEMON_QUIET).max(self.up_since + DAEMON_QUIET))
    }

    /// One pass: the snapshot, the time, whether a Reno window is active, the preferences.
    pub(crate) fn step(
        &mut self,
        snapshot: &Snapshot,
        now: Instant,
        active: bool,
        prefs: &Prefs,
    ) -> Outcome {
        let mut withdraws: Vec<String> = Vec::new();
        let mut due: Vec<Due> = Vec::new();
        let allowed = prefs.notifications && !(prefs.background_only && active);

        self.watch_connection(snapshot, now, allowed, &mut withdraws, &mut due);
        self.online = snapshot.online();
        if self.online {
            if self.seeded {
                self.diff(snapshot, now, &mut withdraws, &mut due);
                self.fire_settled(snapshot, now, &mut due);
            } else {
                self.seed(snapshot);
            }
        }
        if self.burst_needs_posted && snapshot.needs_you() == 0 && self.online {
            self.burst_needs_posted = false;
            withdraws.push(Kind::NeedsYou.burst_id());
        }

        let mut effects: Vec<Effect> = withdraws.into_iter().map(Effect::Withdraw).collect();
        effects.extend(self.deliver(due, prefs, allowed));
        Outcome {
            effects,
            badge: badge_label(snapshot, prefs),
        }
    }

    /// The person opened `run` in the app: every banner for it goes.
    pub(crate) fn run_opened(&self, run: &RunId) -> Vec<Effect> {
        let mut ids = vec![
            format!("question:{run}"),
            format!("done:{run}"),
            format!("problem:{run}"),
        ];
        let mut held: Vec<&ApprovalId> = self
            .approvals
            .iter()
            .filter(|(_, r)| *r == run)
            .map(|(id, _)| id)
            .collect();
        held.sort();
        ids.extend(held.into_iter().map(|id| format!("approval:{id}")));
        ids.into_iter().map(Effect::Withdraw).collect()
    }

    /// The daemon banner: quiet for the first seconds of the app and of an outage, withdrawn when
    /// the connection is back.
    fn watch_connection(
        &mut self,
        snapshot: &Snapshot,
        now: Instant,
        allowed: bool,
        withdraws: &mut Vec<String>,
        due: &mut Vec<Due>,
    ) {
        if snapshot.connection == ConnectionStatus::Connected {
            self.ever_connected = true;
            self.down_since = None;
            self.daemon_handled = false;
            if std::mem::take(&mut self.daemon_posted) {
                withdraws.push(DAEMON_ID.to_owned());
            }
            return;
        }
        if self.ever_connected && self.down_since.is_none() {
            self.down_since = Some(now);
        }
        if self.daemon_due().is_some_and(|at| at <= now) {
            self.daemon_handled = true;
            if allowed {
                self.daemon_posted = true;
                due.push(Due {
                    kind: Kind::Problem,
                    banner: Banner {
                        id: DAEMON_ID.to_owned(),
                        title: "Reno daemon unreachable".to_owned(),
                        subtitle: None,
                        body: "Runs keep going in the daemon if it is still up; Reno reconnects on its own."
                            .to_owned(),
                        target: Some(Target::Home.encode()),
                    },
                });
            }
        }
    }

    /// The first connected snapshot: every baseline, silently.
    fn seed(&mut self, snapshot: &Snapshot) {
        self.seeded = true;
        for run in &snapshot.runs {
            self.runs.insert(run.id.clone(), run.status);
        }
        for approval in &snapshot.approvals {
            self.approvals
                .insert(approval.id.clone(), approval.run.clone());
            self.seen_approvals.insert(approval.id.clone());
        }
    }

    fn diff(
        &mut self,
        snapshot: &Snapshot,
        now: Instant,
        withdraws: &mut Vec<String>,
        due: &mut Vec<Due>,
    ) {
        // Held actions: a new id rings; one that left was decided (anywhere), so its banner goes.
        let mut current: HashMap<ApprovalId, RunId> = HashMap::new();
        for approval in &snapshot.approvals {
            current.insert(approval.id.clone(), approval.run.clone());
            if self.seen_approvals.insert(approval.id.clone()) {
                let run = snapshot.run(&approval.run);
                due.push(Due {
                    kind: Kind::NeedsYou,
                    banner: Banner {
                        id: format!("approval:{}", approval.id),
                        title: format!("{} needs you", who(run)),
                        subtitle: Some(format!(
                            "Wants to {}",
                            one_line(&approval.subject, SUBJECT_CHARS)
                        )),
                        body: run.map(what).unwrap_or_default(),
                        target: Some(Target::Run(approval.run.clone()).encode()),
                    },
                });
            }
        }
        let mut gone: Vec<&ApprovalId> = self
            .approvals
            .keys()
            .filter(|id| !current.contains_key(*id))
            .collect();
        gone.sort();
        withdraws.extend(gone.into_iter().map(|id| format!("approval:{id}")));
        self.approvals = current;

        // Runs: one baseline each; a changed status is news.
        for run in &snapshot.runs {
            let before = self.runs.insert(run.id.clone(), run.status);
            if before == Some(run.status) {
                continue;
            }
            self.settling.remove(&run.id);
            if before == Some(RunStatus::WaitingInput) {
                withdraws.push(format!("question:{}", run.id));
            }
            match run.status {
                RunStatus::WaitingInput => due.push(Due {
                    kind: Kind::NeedsYou,
                    banner: Banner {
                        id: format!("question:{}", run.id),
                        title: format!("{} has a question", who(Some(run))),
                        subtitle: None,
                        body: what(run),
                        target: Some(Target::Run(run.id.clone()).encode()),
                    },
                }),
                RunStatus::Succeeded | RunStatus::Failed => self.settle(run, FINISH_SETTLE, now),
                RunStatus::Interrupted => self.settle(run, INTERRUPT_SETTLE, now),
                // A cancel is a person's own doing, and the rest are not endings.
                _ => {}
            }
        }
    }

    fn settle(&mut self, run: &RunFact, wait: Duration, now: Instant) {
        self.settling.insert(
            run.id.clone(),
            Settling {
                status: run.status,
                due: now + wait,
            },
        );
    }

    /// The runs that stood long enough ring, if they still are what they were.
    fn fire_settled(&mut self, snapshot: &Snapshot, now: Instant, due: &mut Vec<Due>) {
        let ready: Vec<RunId> = self
            .settling
            .iter()
            .filter(|(_, s)| s.due <= now)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ready {
            let Some(settled) = self.settling.remove(&id) else {
                continue;
            };
            let Some(run) = snapshot.run(&id) else {
                continue;
            };
            if run.status != settled.status
                || (run.status == RunStatus::Interrupted && run.by_person)
            {
                continue;
            }
            let who = who(Some(run));
            let target = Some(Target::Run(id.clone()).encode());
            let (kind, banner_id, title, subtitle) = match run.status {
                RunStatus::Succeeded => (
                    Kind::Finished,
                    format!("done:{id}"),
                    format!("{who} finished"),
                    None,
                ),
                RunStatus::Failed => (
                    Kind::Problem,
                    format!("problem:{id}"),
                    format!("{who} failed"),
                    run.reason
                        .as_deref()
                        .filter(|reason| reason.chars().count() <= REASON_CHARS)
                        .map(|reason| upper_first(&one_line(reason, REASON_CHARS))),
                ),
                _ => (
                    Kind::Problem,
                    format!("problem:{id}"),
                    format!("{who} was interrupted"),
                    None,
                ),
            };
            due.push(Due {
                kind,
                banner: Banner {
                    id: banner_id,
                    title,
                    subtitle,
                    body: what(run),
                    target,
                },
            });
        }
    }

    /// What is due becomes banners: nothing when banners are off or held (a held banner is gone,
    /// never replayed), and more than [`BURST_OVER`] of one kind become one.
    fn deliver(&mut self, due: Vec<Due>, prefs: &Prefs, allowed: bool) -> Vec<Effect> {
        if !allowed {
            return Vec::new();
        }
        let mut by_kind: BTreeMap<Kind, Vec<Banner>> = BTreeMap::new();
        for item in due {
            let wanted = item.kind != Kind::Finished || prefs.finished;
            if wanted || item.banner.id == DAEMON_ID {
                by_kind.entry(item.kind).or_default().push(item.banner);
            }
        }
        let mut effects = Vec::new();
        for (kind, banners) in by_kind {
            let ringing: Vec<Banner> = banners
                .iter()
                .filter(|b| b.id != DAEMON_ID)
                .cloned()
                .collect();
            let daemon = banners.into_iter().filter(|b| b.id == DAEMON_ID);
            if ringing.len() > BURST_OVER {
                if kind == Kind::NeedsYou {
                    self.burst_needs_posted = true;
                }
                effects.push(Effect::Post(Banner {
                    id: kind.burst_id(),
                    title: kind.burst_title(ringing.len()),
                    subtitle: None,
                    body: String::new(),
                    target: Some(kind.burst_id()),
                }));
            } else {
                effects.extend(ringing.into_iter().map(Effect::Post));
            }
            effects.extend(daemon.map(Effect::Post));
        }
        effects
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn run_id(s: &str) -> RunId {
        RunId::from_suffix(s).unwrap()
    }

    fn appr_id(s: &str) -> ApprovalId {
        ApprovalId::from_suffix(s).unwrap()
    }

    fn run(name: &str, status: RunStatus) -> RunFact {
        RunFact {
            id: run_id(name),
            status,
            title: format!("Task {name}"),
            bot: Some("keeper".to_owned()),
            reason: None,
            by_person: false,
        }
    }

    fn approval(id: &str, run: &str, subject: &str) -> ApprovalFact {
        ApprovalFact {
            id: appr_id(id),
            run: run_id(run),
            subject: subject.to_owned(),
        }
    }

    fn snap(runs: Vec<RunFact>, approvals: Vec<ApprovalFact>) -> Snapshot {
        Snapshot::new(runs, approvals)
    }

    fn with_connection(mut s: Snapshot, connection: ConnectionStatus) -> Snapshot {
        s.connection = connection;
        s
    }

    /// A notifier that has seeded on `runs` and `approvals`, at `t0`; the app has been up 10 s.
    struct Rig {
        n: Notifier,
        t0: Instant,
        prefs: Prefs,
        active: bool,
    }

    impl Rig {
        fn seeded(runs: Vec<RunFact>, approvals: Vec<ApprovalFact>) -> Self {
            let start = Instant::now();
            let mut rig = Self {
                n: Notifier::new(start),
                t0: start + Duration::from_secs(10),
                prefs: Prefs::default(),
                active: false,
            };
            let out = rig.at(0, &snap(runs, approvals));
            assert!(
                out.effects.is_empty(),
                "the seed is silent: {:?}",
                out.effects
            );
            rig
        }

        fn at(&mut self, secs: u64, s: &Snapshot) -> Outcome {
            self.n.step(
                s,
                self.t0 + Duration::from_millis(secs * 1000),
                self.active,
                &self.prefs,
            )
        }

        fn at_ms(&mut self, ms: u64, s: &Snapshot) -> Outcome {
            self.n.step(
                s,
                self.t0 + Duration::from_millis(ms),
                self.active,
                &self.prefs,
            )
        }
    }

    fn posts(out: &Outcome) -> Vec<&Banner> {
        out.effects
            .iter()
            .filter_map(|e| match e {
                Effect::Post(b) => Some(b),
                Effect::Withdraw(_) => None,
            })
            .collect()
    }

    fn withdrawn(out: &Outcome) -> Vec<&str> {
        out.effects
            .iter()
            .filter_map(|e| match e {
                Effect::Withdraw(id) => Some(id.as_str()),
                Effect::Post(_) => None,
            })
            .collect()
    }

    // --- The events table ---

    #[test]
    fn a_new_approval_rings_at_once_with_the_runs_label_and_subject() {
        let mut rig = Rig::seeded(vec![run("a", RunStatus::WaitingApproval)], vec![]);
        let s = snap(
            vec![run("a", RunStatus::WaitingApproval)],
            vec![approval("p1", "a", "git push -u origin HEAD")],
        );
        let out = rig.at(1, &s);
        assert_eq!(
            posts(&out),
            vec![&Banner {
                id: format!("approval:{}", appr_id("p1")),
                title: "keeper needs you".to_owned(),
                subtitle: Some("Wants to git push -u origin HEAD".to_owned()),
                body: "Task a".to_owned(),
                target: Some(format!("run:{}", run_id("a"))),
            }]
        );
        assert_eq!(out.badge.as_deref(), Some("1"));
        // The same snapshot again rings nothing: one baseline per item.
        assert!(posts(&rig.at(2, &s)).is_empty());
    }

    #[test]
    fn an_approval_subject_is_one_line_cut_to_120_characters() {
        let mut rig = Rig::seeded(vec![run("a", RunStatus::Running)], vec![]);
        let long = format!("echo {}\nsecond line", "x".repeat(300));
        let out = rig.at(
            1,
            &snap(
                vec![run("a", RunStatus::Running)],
                vec![approval("p1", "a", &long)],
            ),
        );
        let subtitle = posts(&out)[0].subtitle.clone().unwrap();
        assert!(!subtitle.contains('\n'));
        let subject = subtitle.strip_prefix("Wants to ").unwrap();
        assert_eq!(subject.chars().count(), 120);
        assert!(subject.ends_with('\u{2026}'));
    }

    #[test]
    fn a_run_entering_waiting_input_rings_a_question() {
        let mut rig = Rig::seeded(vec![run("a", RunStatus::Running)], vec![]);
        let out = rig.at(1, &snap(vec![run("a", RunStatus::WaitingInput)], vec![]));
        assert_eq!(
            posts(&out),
            vec![&Banner {
                id: format!("question:{}", run_id("a")),
                title: "keeper has a question".to_owned(),
                subtitle: None,
                body: "Task a".to_owned(),
                target: Some(format!("run:{}", run_id("a"))),
            }]
        );
        assert_eq!(out.badge.as_deref(), Some("1"));
    }

    #[test]
    fn a_success_rings_after_two_seconds_and_not_before() {
        let mut rig = Rig::seeded(vec![run("a", RunStatus::Running)], vec![]);
        let done = snap(vec![run("a", RunStatus::Succeeded)], vec![]);
        assert!(rig.at(1, &done).effects.is_empty());
        assert_eq!(rig.n.next_due(), Some(rig.t0 + Duration::from_secs(3)));
        assert!(rig.at_ms(2999, &done).effects.is_empty());
        let out = rig.at(3, &done);
        assert_eq!(
            posts(&out),
            vec![&Banner {
                id: format!("done:{}", run_id("a")),
                title: "keeper finished".to_owned(),
                subtitle: None,
                body: "Task a".to_owned(),
                target: Some(format!("run:{}", run_id("a"))),
            }]
        );
        assert_eq!(rig.n.next_due(), None);
        assert!(posts(&rig.at(9, &done)).is_empty(), "it rings once");
    }

    #[test]
    fn a_failure_rings_after_two_seconds_with_a_short_reason() {
        let mut rig = Rig::seeded(
            vec![run("a", RunStatus::Running), run("b", RunStatus::Running)],
            vec![],
        );
        let mut a = run("a", RunStatus::Failed);
        a.reason = Some("the model refused".to_owned());
        let mut b = run("b", RunStatus::Failed);
        b.reason = Some("x".repeat(200));
        let s = snap(vec![a, b], vec![]);
        rig.at(1, &s);
        let out = rig.at(3, &s);
        let lines: Vec<_> = posts(&out)
            .iter()
            .map(|b| (b.title.as_str(), b.subtitle.as_deref()))
            .collect();
        assert_eq!(
            lines,
            vec![
                ("keeper failed", Some("The model refused")),
                ("keeper failed", None)
            ]
        );
        assert_eq!(posts(&out)[0].id, format!("problem:{}", run_id("a")));
    }

    #[test]
    fn an_interruption_rings_after_five_seconds() {
        let mut rig = Rig::seeded(vec![run("a", RunStatus::Running)], vec![]);
        let s = snap(vec![run("a", RunStatus::Interrupted)], vec![]);
        rig.at(1, &s);
        assert!(rig.at(5, &s).effects.is_empty());
        let out = rig.at(6, &s);
        assert_eq!(posts(&out).len(), 1);
        assert_eq!(posts(&out)[0].title, "keeper was interrupted");
        assert_eq!(posts(&out)[0].id, format!("problem:{}", run_id("a")));
    }

    #[test]
    fn a_cancel_never_rings_and_neither_does_a_stop_the_person_asked_for() {
        let mut rig = Rig::seeded(
            vec![run("a", RunStatus::Running), run("b", RunStatus::Running)],
            vec![],
        );
        let mut stopping = run("b", RunStatus::Interrupted);
        stopping.by_person = true;
        let s = snap(vec![run("a", RunStatus::Cancelled), stopping], vec![]);
        rig.at(1, &s);
        assert!(rig.at(20, &s).effects.is_empty());
        // The daemon settles it as cancelled a moment later.
        let s = snap(
            vec![
                run("a", RunStatus::Cancelled),
                run("b", RunStatus::Cancelled),
            ],
            vec![],
        );
        assert!(rig.at(21, &s).effects.is_empty());
        assert!(rig.at(30, &s).effects.is_empty());
        assert_eq!(rig.n.next_due(), None);
    }

    #[test]
    fn a_pending_banner_is_dropped_when_the_run_leaves_the_status() {
        let mut rig = Rig::seeded(
            vec![run("a", RunStatus::Running), run("b", RunStatus::Running)],
            vec![],
        );
        // An interrupted run the daemon resumes; a failed run retried.
        rig.at(
            1,
            &snap(
                vec![
                    run("a", RunStatus::Interrupted),
                    run("b", RunStatus::Failed),
                ],
                vec![],
            ),
        );
        let back = snap(
            vec![run("a", RunStatus::Running), run("b", RunStatus::Queued)],
            vec![],
        );
        rig.at(2, &back);
        assert_eq!(rig.n.next_due(), None, "both pending banners are gone");
        assert!(rig.at(30, &back).effects.is_empty());
    }

    #[test]
    fn a_pending_banner_for_a_run_that_is_no_longer_there_is_dropped() {
        let mut rig = Rig::seeded(vec![run("a", RunStatus::Running)], vec![]);
        rig.at(1, &snap(vec![run("a", RunStatus::Succeeded)], vec![]));
        assert!(rig.at(5, &snap(vec![], vec![])).effects.is_empty());
    }

    #[test]
    fn a_run_new_after_the_seed_counts_as_entering_its_status() {
        let mut rig = Rig::seeded(vec![], vec![]);
        let out = rig.at(1, &snap(vec![run("a", RunStatus::WaitingInput)], vec![]));
        assert_eq!(posts(&out).len(), 1);
    }

    // --- The seed and reconnects ---

    #[test]
    fn the_seed_is_silent_for_runs_and_approvals_already_there() {
        let mut n = Notifier::new(Instant::now());
        let s = snap(
            vec![
                run("a", RunStatus::WaitingInput),
                run("b", RunStatus::Succeeded),
            ],
            vec![approval("p1", "a", "ls")],
        );
        let out = n.step(
            &s,
            Instant::now() + Duration::from_secs(60),
            false,
            &Prefs::default(),
        );
        assert!(out.effects.is_empty());
        assert_eq!(n.next_due(), None, "a success at the seed is not pending");
        assert_eq!(
            out.badge.as_deref(),
            Some("2"),
            "the badge still counts them"
        );
    }

    #[test]
    fn nothing_is_seeded_until_the_snapshot_is_connected_and_loaded() {
        let mut n = Notifier::new(Instant::now());
        let now = Instant::now();
        let mut early = snap(vec![], vec![]);
        early.loaded = false;
        n.step(&early, now, false, &Prefs::default());
        // A held action arrives with the first real list: it is the seed, not news.
        let out = n.step(
            &snap(vec![], vec![approval("p1", "a", "ls")]),
            now + Duration::from_secs(1),
            false,
            &Prefs::default(),
        );
        assert!(out.effects.is_empty());
    }

    #[test]
    fn what_happened_while_disconnected_rings_after_the_reconnect_once() {
        let mut rig = Rig::seeded(vec![run("a", RunStatus::Running)], vec![]);
        let down = with_connection(
            snap(vec![run("a", RunStatus::Running)], vec![]),
            ConnectionStatus::Disconnected,
        );
        assert!(rig.at(1, &down).effects.is_empty());
        // Back, with a held action that appeared meanwhile, and a question.
        let back = snap(
            vec![run("a", RunStatus::WaitingInput)],
            vec![approval("p1", "b", "ls")],
        );
        let out = rig.at(2, &back);
        assert_eq!(posts(&out).len(), 2);
        // Another drop and return does not ring the same ids again.
        rig.at(
            3,
            &with_connection(back.clone(), ConnectionStatus::Disconnected),
        );
        assert!(posts(&rig.at(4, &back)).is_empty());
    }

    // --- Withdraw ---

    #[test]
    fn a_decided_approval_withdraws_its_banner_and_clears_the_badge() {
        let held = snap(
            vec![run("a", RunStatus::WaitingApproval)],
            vec![approval("p1", "a", "ls"), approval("p2", "a", "pwd")],
        );
        let mut rig = Rig::seeded(held.runs.clone(), vec![]);
        let out = rig.at(1, &held);
        assert_eq!(out.badge.as_deref(), Some("2"));
        let after = snap(held.runs.clone(), vec![approval("p2", "a", "pwd")]);
        let out = rig.at(2, &after);
        assert_eq!(withdrawn(&out), vec![format!("approval:{}", appr_id("p1"))]);
        assert_eq!(out.badge.as_deref(), Some("1"));
        let out = rig.at(3, &snap(held.runs, vec![]));
        assert_eq!(withdrawn(&out), vec![format!("approval:{}", appr_id("p2"))]);
        assert_eq!(out.badge, None);
    }

    #[test]
    fn a_run_leaving_waiting_input_withdraws_its_question() {
        let mut rig = Rig::seeded(vec![run("a", RunStatus::Running)], vec![]);
        rig.at(1, &snap(vec![run("a", RunStatus::WaitingInput)], vec![]));
        let out = rig.at(2, &snap(vec![run("a", RunStatus::Running)], vec![]));
        assert_eq!(withdrawn(&out), vec![format!("question:{}", run_id("a"))]);
    }

    #[test]
    fn the_daemon_banner_is_withdrawn_when_the_connection_returns() {
        let mut rig = Rig::seeded(vec![], vec![]);
        let down = with_connection(snap(vec![], vec![]), ConnectionStatus::Disconnected);
        rig.at(1, &down);
        let out = rig.at(7, &down);
        assert_eq!(posts(&out).len(), 1);
        let out = rig.at(8, &snap(vec![], vec![]));
        assert_eq!(withdrawn(&out), vec!["daemon"]);
        assert!(withdrawn(&rig.at(9, &snap(vec![], vec![]))).is_empty());
    }

    #[test]
    fn opening_a_run_withdraws_every_banner_for_it() {
        let held = snap(
            vec![
                run("a", RunStatus::WaitingApproval),
                run("b", RunStatus::WaitingApproval),
            ],
            vec![approval("p1", "a", "ls"), approval("p2", "b", "pwd")],
        );
        let mut rig = Rig::seeded(held.runs.clone(), vec![]);
        rig.at(1, &held);
        let effects = rig.n.run_opened(&run_id("a"));
        let ids: Vec<&str> = effects
            .iter()
            .map(|e| match e {
                Effect::Withdraw(id) => id.as_str(),
                Effect::Post(_) => panic!("opening a run posts nothing"),
            })
            .collect();
        assert!(ids.contains(&format!("question:{}", run_id("a")).as_str()));
        assert!(ids.contains(&format!("done:{}", run_id("a")).as_str()));
        assert!(ids.contains(&format!("problem:{}", run_id("a")).as_str()));
        assert!(ids.contains(&format!("approval:{}", appr_id("p1")).as_str()));
        assert!(!ids.contains(&format!("approval:{}", appr_id("p2")).as_str()));
    }

    // --- The connection ---

    #[test]
    fn the_daemon_rings_after_five_seconds_down_once_and_never_before_having_been_up() {
        let mut rig = Rig::seeded(vec![], vec![]);
        let down = with_connection(snap(vec![], vec![]), ConnectionStatus::Disconnected);
        let connecting = with_connection(snap(vec![], vec![]), ConnectionStatus::Connecting);
        rig.at(1, &down);
        assert_eq!(rig.n.next_due(), Some(rig.t0 + Duration::from_secs(6)));
        // The retry's `Connecting` does not restart the count.
        assert!(rig.at_ms(4000, &connecting).effects.is_empty());
        assert!(rig.at_ms(5999, &down).effects.is_empty());
        let out = rig.at(6, &down);
        assert_eq!(
            posts(&out),
            vec![&Banner {
                id: "daemon".to_owned(),
                title: "Reno daemon unreachable".to_owned(),
                subtitle: None,
                body:
                    "Runs keep going in the daemon if it is still up; Reno reconnects on its own."
                        .to_owned(),
                target: Some("daemon".to_owned()),
            }]
        );
        assert!(posts(&rig.at(30, &down)).is_empty(), "once per outage");
        assert_eq!(out.badge, None, "no badge while down");
    }

    #[test]
    fn a_short_outage_never_rings() {
        let mut rig = Rig::seeded(vec![], vec![]);
        let down = with_connection(snap(vec![], vec![]), ConnectionStatus::Disconnected);
        rig.at(1, &down);
        rig.at(4, &snap(vec![], vec![]));
        assert_eq!(rig.n.next_due(), None);
        assert!(rig.at(20, &snap(vec![], vec![])).effects.is_empty());
    }

    #[test]
    fn a_daemon_that_was_never_reached_does_not_ring() {
        let mut n = Notifier::new(Instant::now());
        let now = Instant::now();
        let down = with_connection(snap(vec![], vec![]), ConnectionStatus::Disconnected);
        n.step(&down, now, false, &Prefs::default());
        let out = n.step(
            &down,
            now + Duration::from_secs(60),
            false,
            &Prefs::default(),
        );
        assert!(out.effects.is_empty());
        assert_eq!(n.next_due(), None);
    }

    #[test]
    fn an_outage_in_the_apps_first_seconds_waits_for_the_app_to_be_up_five_seconds_too() {
        let start = Instant::now();
        let mut n = Notifier::new(start);
        let prefs = Prefs::default();
        n.step(
            &snap(vec![], vec![]),
            start + Duration::from_millis(100),
            false,
            &prefs,
        );
        let down = with_connection(snap(vec![], vec![]), ConnectionStatus::Disconnected);
        n.step(&down, start + Duration::from_millis(200), false, &prefs);
        assert_eq!(n.next_due(), Some(start + Duration::from_millis(5200)));
        assert!(
            n.step(&down, start + Duration::from_millis(5100), false, &prefs)
                .effects
                .is_empty()
        );
        let out = n.step(&down, start + Duration::from_millis(5200), false, &prefs);
        assert_eq!(posts(&out).len(), 1);
    }

    // --- Coalescing ---

    #[test]
    fn more_than_three_of_a_kind_ring_as_one() {
        let running: Vec<RunFact> = ["a", "b", "c", "d"]
            .iter()
            .map(|n| run(n, RunStatus::Running))
            .collect();
        let mut rig = Rig::seeded(running.clone(), vec![]);
        let questions = snap(
            running
                .iter()
                .map(|r| run(&r.id.to_string()[4..], RunStatus::WaitingInput))
                .collect(),
            vec![],
        );
        let out = rig.at(1, &questions);
        assert_eq!(
            posts(&out),
            vec![&Banner {
                id: "burst:needs".to_owned(),
                title: "4 runs need you".to_owned(),
                subtitle: None,
                body: String::new(),
                target: Some("burst:needs".to_owned()),
            }]
        );
        // When nothing needs you any more the burst banner goes.
        let out = rig.at(2, &snap(running, vec![]));
        assert!(withdrawn(&out).contains(&"burst:needs"));
    }

    #[test]
    fn exactly_three_ring_one_by_one() {
        let running: Vec<RunFact> = ["a", "b", "c"]
            .iter()
            .map(|n| run(n, RunStatus::Running))
            .collect();
        let mut rig = Rig::seeded(running, vec![]);
        let done: Vec<RunFact> = ["a", "b", "c"]
            .iter()
            .map(|n| run(n, RunStatus::Succeeded))
            .collect();
        rig.at(1, &snap(done.clone(), vec![]));
        assert_eq!(posts(&rig.at(3, &snap(done, vec![]))).len(), 3);
    }

    #[test]
    fn a_daemon_restart_that_interrupts_every_run_rings_once() {
        let names = ["a", "b", "c", "d", "e"];
        let live: Vec<RunFact> = names.iter().map(|n| run(n, RunStatus::Running)).collect();
        let mut rig = Rig::seeded(live, vec![]);
        let stopped: Vec<RunFact> = names
            .iter()
            .map(|n| run(n, RunStatus::Interrupted))
            .collect();
        let s = snap(stopped, vec![]);
        rig.at(1, &s);
        let out = rig.at(6, &s);
        assert_eq!(posts(&out).len(), 1);
        assert_eq!(posts(&out)[0].title, "5 runs stopped");
        assert_eq!(posts(&out)[0].id, "burst:stopped");
    }

    #[test]
    fn finished_runs_coalesce_apart_from_problems() {
        let names = ["a", "b", "c", "d", "e"];
        let live: Vec<RunFact> = names.iter().map(|n| run(n, RunStatus::Running)).collect();
        let mut rig = Rig::seeded(live, vec![]);
        let mut mixed: Vec<RunFact> = names[..4]
            .iter()
            .map(|n| run(n, RunStatus::Succeeded))
            .collect();
        mixed.push(run("e", RunStatus::Failed));
        let s = snap(mixed, vec![]);
        rig.at(1, &s);
        let out = rig.at(3, &s);
        let titles: Vec<&str> = posts(&out).iter().map(|b| b.title.as_str()).collect();
        assert_eq!(titles.len(), 2, "{titles:?}");
        assert!(titles.contains(&"4 runs finished"));
        assert!(titles.contains(&"keeper failed"));
    }

    // --- Preferences and the window ---

    #[test]
    fn with_banners_off_nothing_rings_but_the_badge_stays() {
        let mut rig = Rig::seeded(vec![run("a", RunStatus::Running)], vec![]);
        rig.prefs.notifications = false;
        let out = rig.at(1, &snap(vec![run("a", RunStatus::WaitingInput)], vec![]));
        assert!(posts(&out).is_empty());
        assert_eq!(out.badge.as_deref(), Some("1"));
        // Turned back on, the banner it skipped is not replayed.
        rig.prefs.notifications = true;
        assert!(
            rig.at(2, &snap(vec![run("a", RunStatus::WaitingInput)], vec![]))
                .effects
                .is_empty()
        );
    }

    #[test]
    fn finished_off_skips_only_finishes() {
        let mut rig = Rig::seeded(
            vec![run("a", RunStatus::Running), run("b", RunStatus::Running)],
            vec![],
        );
        rig.prefs.finished = false;
        let s = snap(
            vec![run("a", RunStatus::Succeeded), run("b", RunStatus::Failed)],
            vec![],
        );
        rig.at(1, &s);
        let out = rig.at(3, &s);
        assert_eq!(posts(&out).len(), 1);
        assert_eq!(posts(&out)[0].id, format!("problem:{}", run_id("b")));
    }

    #[test]
    fn background_only_holds_banners_while_a_window_is_active_and_never_replays_them() {
        let mut rig = Rig::seeded(
            vec![run("a", RunStatus::Running), run("b", RunStatus::Running)],
            vec![],
        );
        rig.active = true;
        let s = snap(
            vec![
                run("a", RunStatus::WaitingInput),
                run("b", RunStatus::Succeeded),
            ],
            vec![approval("p1", "a", "ls")],
        );
        assert!(posts(&rig.at(1, &s)).is_empty(), "held");
        assert!(
            posts(&rig.at(3, &s)).is_empty(),
            "the settled finish is held too"
        );
        // The window goes to the background: the held ones do not come back.
        rig.active = false;
        assert!(rig.at(4, &s).effects.is_empty());
        assert!(rig.at(30, &s).effects.is_empty());
        // News after that rings.
        let more = snap(
            vec![
                run("a", RunStatus::WaitingInput),
                run("b", RunStatus::Succeeded),
            ],
            vec![approval("p1", "a", "ls"), approval("p2", "a", "pwd")],
        );
        assert_eq!(posts(&rig.at(31, &more)).len(), 1);
    }

    #[test]
    fn background_only_off_rings_with_the_window_active() {
        let mut rig = Rig::seeded(vec![run("a", RunStatus::Running)], vec![]);
        rig.active = true;
        rig.prefs.background_only = false;
        let out = rig.at(1, &snap(vec![run("a", RunStatus::WaitingInput)], vec![]));
        assert_eq!(posts(&out).len(), 1);
    }

    #[test]
    fn a_held_daemon_banner_is_not_replayed() {
        let mut rig = Rig::seeded(vec![], vec![]);
        rig.active = true;
        let down = with_connection(snap(vec![], vec![]), ConnectionStatus::Disconnected);
        rig.at(1, &down);
        assert!(posts(&rig.at(7, &down)).is_empty());
        rig.active = false;
        assert!(posts(&rig.at(30, &down)).is_empty());
        assert_eq!(rig.n.next_due(), None);
    }

    // --- The badge ---

    #[test]
    fn the_badge_counts_what_needs_you_blank_at_zero_and_99_plus() {
        let prefs = Prefs::default();
        let none = snap(vec![run("a", RunStatus::Running)], vec![]);
        assert_eq!(badge_label(&none, &prefs), None);
        let some = snap(
            vec![run("a", RunStatus::WaitingInput)],
            vec![approval("p1", "a", "ls"), approval("p2", "a", "pwd")],
        );
        assert_eq!(badge_label(&some, &prefs).as_deref(), Some("3"));
        let approvals: Vec<_> = (0..99)
            .map(|i| approval(&format!("p{i}"), "a", "ls"))
            .collect();
        assert_eq!(
            badge_label(&snap(vec![], approvals.clone()), &prefs).as_deref(),
            Some("99")
        );
        let mut over = approvals;
        over.push(approval("pz", "a", "ls"));
        assert_eq!(
            badge_label(&snap(vec![], over), &prefs).as_deref(),
            Some("99+")
        );
    }

    #[test]
    fn the_badge_clears_while_disconnected_and_with_the_preference_off() {
        let mut prefs = Prefs::default();
        let held = snap(vec![], vec![approval("p1", "a", "ls")]);
        assert_eq!(badge_label(&held, &prefs).as_deref(), Some("1"));
        let down = with_connection(held.clone(), ConnectionStatus::Disconnected);
        assert_eq!(badge_label(&down, &prefs), None);
        let connecting = with_connection(held.clone(), ConnectionStatus::Connecting);
        assert_eq!(badge_label(&connecting, &prefs), None);
        prefs.dock_badge = false;
        assert_eq!(badge_label(&held, &prefs), None);
    }

    // --- Labels and targets ---

    #[test]
    fn a_banner_says_who_in_its_title_and_the_run_title_cut_to_120_in_its_body() {
        let mut r = run("a", RunStatus::Running);
        r.title = "Fix the flaky test".to_owned();
        assert_eq!(who(Some(&r)), "keeper");
        assert_eq!(what(&r), "Fix the flaky test");
        r.bot = None;
        assert_eq!(who(Some(&r)), "A run");
        assert_eq!(who(None), "A run");
        r.bot = Some("keeper".to_owned());
        r.title = "keeper \u{b7} porch: Fix the flaky test".to_owned();
        assert_eq!(what(&r), "Fix the flaky test", "the bot is named once");
        r.title = "w".repeat(200);
        let body = what(&r);
        assert_eq!(body.chars().count(), 120);
        assert!(body.ends_with('\u{2026}'));
        r.title = "two\nlines".to_owned();
        assert_eq!(what(&r), "two lines");
    }

    #[test]
    fn targets_round_trip() {
        let id = run_id("a");
        assert_eq!(
            Target::parse(&Target::Run(id.clone()).encode()),
            Some(Target::Run(id))
        );
        assert_eq!(Target::parse("daemon"), Some(Target::Home));
        assert_eq!(Target::parse("burst:finished"), Some(Target::Runs));
        assert_eq!(Target::parse("run:not-an-id"), None);
        assert_eq!(Target::parse("whatever"), None);
    }
}
