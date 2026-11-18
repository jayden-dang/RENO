//! The GPUI-free state mirror and its one pure update function (ARCHITECTURE 17.6).
//!
//! [`State`] is plain data: no tokio, no channels, no clock, no I/O. Every change goes through
//! [`State::apply`], which takes one [`Update`] and returns a [`Changed`] describing what moved,
//! so a view can re-render only what it shows. The app wraps a `State` in an `Entity`; tests
//! build one and feed it updates directly.
//!
//! # Idempotence
//!
//! Applying an update that changes nothing returns an empty [`Changed`]: a repeated snapshot, a
//! repeated approval list, and a transcript event whose seq is not above the highest seen are all
//! no-ops. Delivery can therefore repeat or reorder (for example after a resume) without
//! corrupting the mirror.
//!
//! # Transcripts
//!
//! A run's transcript accepts an event only when its seq is strictly above the highest seen. The
//! mirror does not require seqs to be consecutive; making the stream gap-free is the job of
//! [`Client::run_events`](crate::Client::run_events), which the [`Mirror`](crate::Mirror) uses.

use std::collections::{BTreeMap, BTreeSet};

use reno_proto::defs::{DefKind, DefRecord, DefValidateResult, DefsSnapshot};
use reno_proto::{
    Approval, ApprovalRecord, DaemonStatusInfo, ExecEvent, ExecutorInfo, HelloResult, RunId,
    RunStatus, RunSummary, Seq, ThreadId, ThreadInfo, TriggerInfo,
};

/// The state of the connection to the daemon.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ConnectionStatus {
    /// Not yet connected. The initial state.
    #[default]
    Connecting,
    /// The handshake succeeded and the connection is up.
    Connected,
    /// There is no connection. [`State::offline`] says why. A [`Mirror`](crate::Mirror) started
    /// with [`Mirror::supervise`](crate::Mirror::supervise) keeps retrying with a capped backoff
    /// and moves to [`Connecting`](Self::Connecting) for each attempt.
    Disconnected,
}

/// Why there is no connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OfflineKind {
    /// Nothing listens on the daemon's socket.
    NotRunning,
    /// The daemon speaks a protocol this client cannot use.
    Incompatible,
    /// A connection that was up closed.
    Lost,
    /// The daemon was just started and is coming up.
    Starting,
    /// The app started the daemon and it did not come up: it exited, or never answered. The
    /// detail says how and carries the last lines of its logs.
    StartFailed,
    /// Anything else.
    Other,
}

/// What the app knows about being offline. Cleared when the connection is up again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offline {
    /// The cause, for choosing words.
    pub kind: OfflineKind,
    /// The error, for showing.
    pub detail: String,
    /// Failed attempts in a row since the last connection that lasted; 0 right after a loss.
    pub attempt: u32,
}

/// One source the mirror keeps current. Each has its own [`FeedState`], so a screen can say
/// "loading", "empty", "unavailable" and "failed" apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Feed {
    /// `runs.watch`.
    Runs,
    /// `approvals.watch`.
    Approvals,
    /// `triggers.watch`.
    Triggers,
    /// `defs.watch`.
    Defs,
    /// `executors.watch`.
    Executors,
    /// `approval.history`, fetched on demand.
    ApprovalHistory,
    /// `thread.list`, refetched when the runs change.
    Threads,
}

/// Where one [`Feed`] stands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum FeedState {
    /// Nothing has arrived yet. The initial state, and the state after a request starts.
    #[default]
    Loading,
    /// A snapshot arrived (it may be empty).
    Live,
    /// The daemon did not negotiate the feature behind it.
    Unavailable,
    /// The last attempt failed, with the reason.
    Failed(String),
}

/// Whether a filtered run list has more pages on the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunPage {
    /// No page was fetched for this filter.
    NotLoaded,
    /// Pass this as `before` to fetch the next page.
    More(RunId),
    /// Every page was fetched.
    Done,
}

/// The canonical key of a status filter: sorted, no repeats. Empty means all runs.
pub fn filter_key(statuses: &[RunStatus]) -> Vec<RunStatus> {
    let mut key = statuses.to_vec();
    key.sort();
    key.dedup();
    key
}

/// What the app knows about the daemon it is connected to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonInfo {
    /// The handshake result.
    pub hello: HelloResult,
    /// The last `daemon.status`, once fetched.
    pub status: Option<DaemonStatusInfo>,
}

/// One run's events as seen so far.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Transcript {
    /// Events in increasing seq order.
    pub events: Vec<(Seq, ExecEvent)>,
    /// The highest seq in `events`; `None` while empty.
    pub highest: Option<Seq>,
}

impl Transcript {
    /// The seq a resumed `run.events` subscription should start from: the one after the highest
    /// seen, or 1 (the first event of a journal) while the transcript is empty.
    pub fn resume_from(&self) -> Seq {
        self.highest.and_then(Seq::next).unwrap_or(Seq(1))
    }
}

/// One change to the mirror. The only way to modify a [`State`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Update {
    /// The connection status changed.
    Connection(ConnectionStatus),
    /// The handshake finished, or the daemon status was refreshed.
    Daemon(DaemonInfo),
    /// The whole current run list (`runs.watch`). Runs missing from it are removed from the
    /// list; their transcripts are kept.
    Runs(Vec<RunSummary>),
    /// One run's summary, inserted or replaced (for example after `run.get`).
    RunUpserted(RunSummary),
    /// One event of a run's journal.
    RunEvent {
        /// The run.
        run: RunId,
        /// Position in the run's journal.
        seq: Seq,
        /// The event.
        event: ExecEvent,
    },
    /// The whole pending approval list (`approvals.watch`).
    Approvals(Vec<Approval>),
    /// Every trigger and its state (`triggers.watch`).
    Triggers(Vec<TriggerInfo>),
    /// Every executor and its last health (`executors.watch`).
    Executors(Vec<ExecutorInfo>),
    /// The definitions in force and the file problems (`defs.watch`).
    Defs(DefsSnapshot),
    /// Decided approvals, newest decision first (`approval.history`).
    ApprovalHistory(Vec<ApprovalRecord>),
    /// Every thread (`thread.list`): the conversations, each with its runs and its worktree. A
    /// thread missing from it is removed from the mirror.
    Threads(Vec<ThreadInfo>),
    /// One page of `run.list` for the filter `filter` (see [`filter_key`]). The runs are added to
    /// the paged set; `next` is the cursor for the following page, `None` on the last.
    RunsPage {
        /// The filter the page was fetched with.
        filter: Vec<RunStatus>,
        /// The page.
        runs: Vec<RunSummary>,
        /// The cursor for the next page.
        next: Option<RunId>,
    },
    /// One stored version of a definition with its body (`def.get`).
    DefRecord(DefRecord),
    /// The outcome of `def.validate` for a definition.
    Validation {
        /// The kind.
        kind: DefKind,
        /// The name.
        name: String,
        /// The outcome.
        result: DefValidateResult,
    },
    /// There is no connection (and why). Implies [`ConnectionStatus::Disconnected`].
    Offline(Offline),
    /// One feed changed state: it failed, is unavailable, or started loading.
    Feed(Feed, FeedState),
}

/// What an [`Update`] changed. Empty means nothing did.
///
/// Views check the part they render:
///
/// - `connection`: the connection status changed;
/// - `daemon`: the daemon info changed;
/// - `runs`: the run list changed (a run was added, removed, or its summary differs), which is
///   what a list view shows;
/// - `approvals`: the pending approval list changed;
/// - `triggers`, `defs`, `history`: the trigger list, the definitions, the approval history
///   changed;
/// - `feeds`: some [`Feed`] changed state;
/// - [`Changed::run`]: this run's summary or transcript changed, which is what a run page shows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changed {
    /// The connection status changed.
    pub connection: bool,
    /// The daemon info changed.
    pub daemon: bool,
    /// The run list changed.
    pub runs: bool,
    /// The pending approvals changed.
    pub approvals: bool,
    /// The triggers changed.
    pub triggers: bool,
    /// The executors or their health changed.
    pub executors: bool,
    /// The definitions, their file problems, their stored bodies or validations changed.
    pub defs: bool,
    /// The approval history changed.
    pub history: bool,
    /// The threads changed.
    pub threads: bool,
    /// A [`Feed`] changed state.
    pub feeds: bool,
    touched: BTreeSet<RunId>,
}

impl Changed {
    /// Whether nothing changed.
    pub fn is_empty(&self) -> bool {
        !(self.connection
            || self.daemon
            || self.runs
            || self.approvals
            || self.triggers
            || self.executors
            || self.defs
            || self.history
            || self.threads
            || self.feeds)
            && self.touched.is_empty()
    }

    /// Whether this run's summary or transcript changed.
    pub fn run(&self, id: &RunId) -> bool {
        self.touched.contains(id)
    }

    /// Every run whose summary or transcript changed.
    pub fn touched_runs(&self) -> impl Iterator<Item = &RunId> {
        self.touched.iter()
    }

    /// Adds everything `other` changed to `self`, for a consumer that batches updates.
    pub fn merge(&mut self, other: Changed) {
        self.connection |= other.connection;
        self.daemon |= other.daemon;
        self.runs |= other.runs;
        self.approvals |= other.approvals;
        self.triggers |= other.triggers;
        self.executors |= other.executors;
        self.defs |= other.defs;
        self.history |= other.history;
        self.threads |= other.threads;
        self.feeds |= other.feeds;
        self.touched.extend(other.touched);
    }
}

/// The mirror of the daemon's state.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    connection: ConnectionStatus,
    daemon: Option<DaemonInfo>,
    runs: BTreeMap<RunId, RunSummary>,
    transcripts: BTreeMap<RunId, Transcript>,
    approvals: Vec<Approval>,
    /// Runs known from `run.list` pages and `run.get`, outside the `runs.watch` window.
    fetched: BTreeMap<RunId, RunSummary>,
    pages: BTreeMap<Vec<RunStatus>, Option<RunId>>,
    triggers: Vec<TriggerInfo>,
    executors: Vec<ExecutorInfo>,
    defs: DefsSnapshot,
    def_records: BTreeMap<(DefKind, String, u32), DefRecord>,
    validations: BTreeMap<(DefKind, String), DefValidateResult>,
    history: Vec<ApprovalRecord>,
    threads: BTreeMap<ThreadId, ThreadInfo>,
    /// Which thread each run of `threads` belongs to.
    run_threads: BTreeMap<RunId, ThreadId>,
    offline: Option<Offline>,
    feeds: BTreeMap<Feed, FeedState>,
}

impl State {
    /// An empty mirror, [`ConnectionStatus::Connecting`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Applies one update and reports what changed. Pure: the result depends only on `self` and
    /// `update`.
    pub fn apply(&mut self, update: Update) -> Changed {
        let mut changed = Changed::default();
        match update {
            Update::Connection(status) => {
                if self.connection != status {
                    self.connection = status;
                    changed.connection = true;
                }
                if status == ConnectionStatus::Connected && self.offline.take().is_some() {
                    changed.connection = true;
                }
            }
            Update::Daemon(info) => {
                if self.daemon.as_ref() != Some(&info) {
                    self.daemon = Some(info);
                    changed.daemon = true;
                }
            }
            Update::Runs(list) => self.replace_runs(list, &mut changed),
            Update::RunUpserted(summary) => {
                if self.runs.contains_key(&summary.run_id) {
                    self.upsert_run(summary, &mut changed);
                } else {
                    self.add_fetched(summary, &mut changed);
                }
            }
            Update::RunEvent { run, seq, event } => {
                let transcript = self.transcripts.entry(run.clone()).or_default();
                if transcript.highest.is_none_or(|highest| seq > highest) {
                    transcript.events.push((seq, event));
                    transcript.highest = Some(seq);
                    changed.touched.insert(run);
                }
            }
            Update::Approvals(mut list) => {
                list.sort_by(|a, b| (a.requested_at_ms, &a.id).cmp(&(b.requested_at_ms, &b.id)));
                if self.approvals != list {
                    self.approvals = list;
                    changed.approvals = true;
                }
                self.set_feed(Feed::Approvals, FeedState::Live, &mut changed);
            }
            Update::Triggers(mut list) => {
                list.sort_by(|a, b| a.name.cmp(&b.name));
                if self.triggers != list {
                    self.triggers = list;
                    changed.triggers = true;
                }
                self.set_feed(Feed::Triggers, FeedState::Live, &mut changed);
            }
            Update::Executors(mut list) => {
                list.sort_by(|a, b| a.descriptor.id.as_str().cmp(b.descriptor.id.as_str()));
                if self.executors != list {
                    self.executors = list;
                    changed.executors = true;
                }
                self.set_feed(Feed::Executors, FeedState::Live, &mut changed);
            }
            Update::Defs(mut snapshot) => {
                snapshot
                    .defs
                    .sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
                if self.defs != snapshot {
                    self.defs = snapshot;
                    changed.defs = true;
                }
                self.set_feed(Feed::Defs, FeedState::Live, &mut changed);
            }
            Update::ApprovalHistory(list) => {
                if self.history != list {
                    self.history = list;
                    changed.history = true;
                }
                self.set_feed(Feed::ApprovalHistory, FeedState::Live, &mut changed);
            }
            Update::Threads(list) => {
                let next: BTreeMap<ThreadId, ThreadInfo> = list
                    .into_iter()
                    .map(|thread| (thread.thread_id.clone(), thread))
                    .collect();
                if self.threads != next {
                    self.run_threads = next
                        .values()
                        .flat_map(|thread| {
                            thread
                                .runs
                                .iter()
                                .map(|run| (run.run_id.clone(), thread.thread_id.clone()))
                        })
                        .collect();
                    // A run whose thread changed is touched, so a view of it re-renders.
                    for thread in next.values() {
                        if self.threads.get(&thread.thread_id) != Some(thread) {
                            changed
                                .touched
                                .extend(thread.runs.iter().map(|run| run.run_id.clone()));
                        }
                    }
                    self.threads = next;
                    changed.threads = true;
                }
                self.set_feed(Feed::Threads, FeedState::Live, &mut changed);
            }
            Update::RunsPage { filter, runs, next } => {
                let key = filter_key(&filter);
                for summary in runs {
                    self.add_fetched(summary, &mut changed);
                }
                if self.pages.get(&key) != Some(&next) {
                    self.pages.insert(key, next);
                    changed.runs = true;
                }
            }
            Update::DefRecord(record) => {
                let key = (record.kind, record.name.clone(), record.version);
                if self.def_records.get(&key) != Some(&record) {
                    self.def_records.insert(key, record);
                    changed.defs = true;
                }
            }
            Update::Validation { kind, name, result } => {
                let key = (kind, name);
                if self.validations.get(&key) != Some(&result) {
                    self.validations.insert(key, result);
                    changed.defs = true;
                }
            }
            Update::Offline(offline) => {
                if self.connection != ConnectionStatus::Disconnected {
                    self.connection = ConnectionStatus::Disconnected;
                    changed.connection = true;
                }
                if self.offline.as_ref() != Some(&offline) {
                    self.offline = Some(offline);
                    changed.connection = true;
                }
            }
            Update::Feed(feed, state) => self.set_feed(feed, state, &mut changed),
        }
        changed
    }

    fn set_feed(&mut self, feed: Feed, state: FeedState, changed: &mut Changed) {
        if self.feed(feed) != state {
            self.feeds.insert(feed, state);
            changed.feeds = true;
        }
    }

    /// Adds a run known from outside the watch window. A run the window holds is left to it.
    fn add_fetched(&mut self, summary: RunSummary, changed: &mut Changed) {
        if self.runs.contains_key(&summary.run_id) {
            return;
        }
        if self.fetched.get(&summary.run_id) != Some(&summary) {
            changed.runs = true;
            changed.touched.insert(summary.run_id.clone());
            self.fetched.insert(summary.run_id.clone(), summary);
        }
    }

    /// Upserts into the window. A fetched copy of the same run is dropped: the window is newer.
    fn upsert_run(&mut self, summary: RunSummary, changed: &mut Changed) {
        self.fetched.remove(&summary.run_id);
        if self.runs.get(&summary.run_id) != Some(&summary) {
            changed.runs = true;
            changed.touched.insert(summary.run_id.clone());
            self.runs.insert(summary.run_id.clone(), summary);
        }
    }

    fn replace_runs(&mut self, list: Vec<RunSummary>, changed: &mut Changed) {
        let incoming: BTreeSet<&RunId> = list.iter().map(|r| &r.run_id).collect();
        let removed: Vec<RunId> = self
            .runs
            .keys()
            .filter(|id| !incoming.contains(id))
            .cloned()
            .collect();
        for id in removed {
            self.runs.remove(&id);
            changed.runs = true;
            changed.touched.insert(id);
        }
        for summary in list {
            self.upsert_run(summary, changed);
        }
        self.set_feed(Feed::Runs, FeedState::Live, changed);
    }

    /// The connection status.
    pub fn connection(&self) -> ConnectionStatus {
        self.connection
    }

    /// The daemon info, once the handshake has finished.
    pub fn daemon(&self) -> Option<&DaemonInfo> {
        self.daemon.as_ref()
    }

    /// One run's summary, from the watch window or from what was fetched.
    pub fn run(&self, id: &RunId) -> Option<&RunSummary> {
        self.runs.get(id).or_else(|| self.fetched.get(id))
    }

    /// The window plus the runs fetched by page or by id, newest first, narrowed to `statuses`
    /// (all when empty).
    pub fn all_runs(&self, statuses: &[RunStatus]) -> Vec<&RunSummary> {
        let mut runs: Vec<&RunSummary> = self
            .runs
            .values()
            .chain(self.fetched.values())
            .filter(|r| statuses.is_empty() || statuses.contains(&r.status))
            .collect();
        runs.sort_by(|a, b| {
            b.created_at_ms
                .cmp(&a.created_at_ms)
                .then_with(|| a.run_id.cmp(&b.run_id))
        });
        runs
    }

    /// Whether the daemon has more pages of runs for this filter.
    pub fn run_page(&self, statuses: &[RunStatus]) -> RunPage {
        match self.pages.get(&filter_key(statuses)) {
            None => RunPage::NotLoaded,
            Some(Some(next)) => RunPage::More(next.clone()),
            Some(None) => RunPage::Done,
        }
    }

    /// Every trigger, by name.
    pub fn triggers(&self) -> &[TriggerInfo] {
        &self.triggers
    }

    /// Whether the connected daemon negotiated `feature`. False until a daemon has answered.
    pub fn supports(&self, feature: &str) -> bool {
        self.daemon
            .as_ref()
            .is_some_and(|d| d.hello.features.iter().any(|f| f == feature))
    }

    /// Every executor with its last health, by id.
    pub fn executors(&self) -> &[ExecutorInfo] {
        &self.executors
    }

    /// Every thread, most recently changed first, then by id.
    pub fn threads(&self) -> Vec<&ThreadInfo> {
        let mut all: Vec<&ThreadInfo> = self.threads.values().collect();
        all.sort_by(|a, b| {
            b.updated_at_ms
                .cmp(&a.updated_at_ms)
                .then_with(|| a.thread_id.cmp(&b.thread_id))
        });
        all
    }

    /// One thread.
    pub fn thread(&self, id: &ThreadId) -> Option<&ThreadInfo> {
        self.threads.get(id)
    }

    /// The thread a run belongs to (`RunSummary` does not carry it: the daemon says it in
    /// `thread.list`). `None` until the threads were fetched, or for a run newer than them.
    pub fn thread_of(&self, run: &RunId) -> Option<&ThreadId> {
        self.run_threads.get(run)
    }

    /// The runs of a thread the mirror knows, oldest first. A run not in the mirror's run
    /// window or fetched pages is left out.
    pub fn thread_runs(&self, id: &ThreadId) -> Vec<&RunSummary> {
        let Some(thread) = self.threads.get(id) else {
            return Vec::new();
        };
        thread
            .runs
            .iter()
            .filter_map(|run| self.run(&run.run_id))
            .collect()
    }

    /// The definitions in force and the file problems.
    pub fn defs(&self) -> &DefsSnapshot {
        &self.defs
    }

    /// A stored version of a definition, once fetched.
    pub fn def_record(&self, kind: DefKind, name: &str, version: u32) -> Option<&DefRecord> {
        self.def_records.get(&(kind, name.to_owned(), version))
    }

    /// The last `def.validate` outcome for a definition.
    pub fn validation(&self, kind: DefKind, name: &str) -> Option<&DefValidateResult> {
        self.validations.get(&(kind, name.to_owned()))
    }

    /// Decided approvals, newest decision first.
    pub fn approval_history(&self) -> &[ApprovalRecord] {
        &self.history
    }

    /// Why there is no connection; `None` while connected or before the first attempt ends.
    pub fn offline(&self) -> Option<&Offline> {
        self.offline.as_ref()
    }

    /// Where one feed stands.
    pub fn feed(&self, feed: Feed) -> FeedState {
        self.feeds.get(&feed).cloned().unwrap_or_default()
    }

    /// Every known run, in a stable order: newest first, then by id.
    pub fn runs(&self) -> Vec<&RunSummary> {
        let mut runs: Vec<&RunSummary> = self.runs.values().collect();
        runs.sort_by(|a, b| {
            b.created_at_ms
                .cmp(&a.created_at_ms)
                .then_with(|| a.run_id.cmp(&b.run_id))
        });
        runs
    }

    /// One run's transcript; `None` when no event of the run has been seen.
    pub fn transcript(&self, id: &RunId) -> Option<&Transcript> {
        self.transcripts.get(id)
    }

    /// The pending approvals, oldest first.
    pub fn approvals(&self) -> &[Approval] {
        &self.approvals
    }

    /// The pending approvals of one run.
    pub fn approvals_for<'a>(&'a self, run: &'a RunId) -> impl Iterator<Item = &'a Approval> {
        self.approvals.iter().filter(move |a| &a.run_id == run)
    }
}

#[cfg(test)]
mod tests {
    use reno_proto::{ActionClass, ApprovalSubject, ExecutorId, RunStatus, TaskId};

    use super::*;

    fn run_id(s: &str) -> RunId {
        RunId::from_suffix(s).unwrap()
    }

    fn summary(name: &str, status: RunStatus, created: u64) -> RunSummary {
        RunSummary {
            model_options: Default::default(),
            effort: None,
            run_id: run_id(name),
            task_id: TaskId::from_suffix(name).unwrap(),
            executor: "claude-code".parse::<ExecutorId>().unwrap(),
            status,
            attempt: 1,
            title: format!("run {name}"),
            prompt: None,
            model: None,
            stop_reason: None,
            bot: None,
            cwd: None,
            created_at_ms: created,
            updated_at_ms: created,
            stop: None,
            not_resumed: None,
            session: None,
            report: None,
        }
    }

    fn text(s: &str) -> ExecEvent {
        ExecEvent::Text { text: s.into() }
    }

    fn event(run: &str, seq: u64, s: &str) -> Update {
        Update::RunEvent {
            run: run_id(run),
            seq: Seq(seq),
            event: text(s),
        }
    }

    fn approval(id: &str, run: &str, at: u64) -> Approval {
        Approval {
            id: reno_proto::ApprovalId::from_suffix(id).unwrap(),
            run_id: run_id(run),
            subject: ApprovalSubject {
                action: ActionClass::Command,
                tool: "bash".into(),
                command: Some("git push".into()),
                paths: vec![],
                raw: serde_json::Value::Null,
            },
            requested_at_ms: at,
        }
    }

    #[test]
    fn starts_connecting_and_empty() {
        let s = State::new();
        assert_eq!(s.connection(), ConnectionStatus::Connecting);
        assert!(s.daemon().is_none());
        assert!(s.runs().is_empty());
        assert!(s.approvals().is_empty());
    }

    #[test]
    fn connection_change_is_reported_once() {
        let mut s = State::new();
        let c = s.apply(Update::Connection(ConnectionStatus::Connected));
        assert!(c.connection && !c.runs && !c.approvals && !c.daemon);
        assert!(
            s.apply(Update::Connection(ConnectionStatus::Connected))
                .is_empty()
        );
        let c = s.apply(Update::Connection(ConnectionStatus::Disconnected));
        assert!(c.connection);
        assert_eq!(s.connection(), ConnectionStatus::Disconnected);
    }

    #[test]
    fn run_upsert_inserts_then_replaces() {
        let mut s = State::new();
        let a = summary("a", RunStatus::Running, 10);
        let c = s.apply(Update::RunUpserted(a.clone()));
        assert!(c.runs && c.run(&a.run_id) && !c.approvals && !c.connection);
        assert_eq!(s.run(&a.run_id), Some(&a));

        assert!(s.apply(Update::RunUpserted(a.clone())).is_empty());

        let done = summary("a", RunStatus::Succeeded, 10);
        let c = s.apply(Update::RunUpserted(done.clone()));
        assert!(c.runs && c.run(&a.run_id));
        assert_eq!(
            s.run(&a.run_id).map(|r| r.status),
            Some(RunStatus::Succeeded)
        );
    }

    #[test]
    fn run_list_replaces_and_is_newest_first() {
        let mut s = State::new();
        let c = s.apply(Update::Runs(vec![
            summary("a", RunStatus::Running, 10),
            summary("b", RunStatus::Queued, 30),
            summary("c", RunStatus::Failed, 20),
        ]));
        assert!(c.runs);
        let order: Vec<_> = s.runs().iter().map(|r| r.run_id.to_string()).collect();
        assert_eq!(order, ["run_b", "run_c", "run_a"]);

        // Same snapshot, different arrival order: nothing changes.
        let again = s.apply(Update::Runs(vec![
            summary("c", RunStatus::Failed, 20),
            summary("a", RunStatus::Running, 10),
            summary("b", RunStatus::Queued, 30),
        ]));
        assert!(again.is_empty());

        // `a` changed, `c` is gone.
        let c = s.apply(Update::Runs(vec![
            summary("a", RunStatus::Succeeded, 10),
            summary("b", RunStatus::Queued, 30),
        ]));
        assert!(c.runs && c.run(&run_id("a")) && c.run(&run_id("c")) && !c.run(&run_id("b")));
        assert!(s.run(&run_id("c")).is_none());
    }

    #[test]
    fn run_ties_break_by_id() {
        let mut s = State::new();
        s.apply(Update::Runs(vec![
            summary("z", RunStatus::Queued, 5),
            summary("m", RunStatus::Queued, 5),
        ]));
        let order: Vec<_> = s.runs().iter().map(|r| r.run_id.to_string()).collect();
        assert_eq!(order, ["run_m", "run_z"]);
    }

    #[test]
    fn transcript_appends_in_order() {
        let mut s = State::new();
        let c = s.apply(event("a", 0, "one"));
        assert!(c.run(&run_id("a")) && !c.runs && !c.approvals);
        s.apply(event("a", 1, "two"));
        s.apply(event("b", 0, "other run"));
        let t = s.transcript(&run_id("a")).unwrap();
        assert_eq!(t.events, vec![(Seq(0), text("one")), (Seq(1), text("two"))]);
        assert_eq!(t.highest, Some(Seq(1)));
        assert_eq!(t.resume_from(), Seq(2));
        assert_eq!(s.transcript(&run_id("b")).unwrap().events.len(), 1);
    }

    #[test]
    fn duplicate_seq_is_ignored() {
        let mut s = State::new();
        s.apply(event("a", 0, "one"));
        let c = s.apply(event("a", 0, "one again"));
        assert!(c.is_empty());
        let t = s.transcript(&run_id("a")).unwrap();
        assert_eq!(t.events, vec![(Seq(0), text("one"))]);
    }

    #[test]
    fn out_of_order_seq_is_ignored() {
        let mut s = State::new();
        s.apply(event("a", 0, "zero"));
        s.apply(event("a", 2, "two"));
        let c = s.apply(event("a", 1, "late"));
        assert!(c.is_empty());
        let t = s.transcript(&run_id("a")).unwrap();
        assert_eq!(t.highest, Some(Seq(2)));
        assert_eq!(t.events.len(), 2);
    }

    #[test]
    fn empty_transcript_resumes_from_the_first_seq() {
        // Journals count from 1.
        assert_eq!(Transcript::default().resume_from(), Seq(1));
    }

    #[test]
    fn approvals_replace_sorted_and_idempotent() {
        let mut s = State::new();
        let c = s.apply(Update::Approvals(vec![
            approval("b", "a", 20),
            approval("a", "a", 10),
        ]));
        assert!(c.approvals && !c.runs && !c.connection && c.touched_runs().next().is_none());
        let ids: Vec<_> = s.approvals().iter().map(|a| a.id.to_string()).collect();
        assert_eq!(ids, ["appr_a", "appr_b"]);

        assert!(
            s.apply(Update::Approvals(vec![
                approval("a", "a", 10),
                approval("b", "a", 20)
            ]))
            .is_empty()
        );

        let c = s.apply(Update::Approvals(vec![approval("b", "a", 20)]));
        assert!(c.approvals);
        assert_eq!(s.approvals().len(), 1);
        assert_eq!(s.approvals_for(&run_id("a")).count(), 1);
        assert_eq!(s.approvals_for(&run_id("zzz")).count(), 0);

        let c = s.apply(Update::Approvals(vec![]));
        assert!(c.approvals);
        assert!(s.approvals().is_empty());
    }

    #[test]
    fn daemon_info_change_is_reported() {
        let mut s = State::new();
        let info = DaemonInfo {
            hello: HelloResult {
                server_version: "1".into(),
                protocol_version: 1,
                features: vec![],
                pid: 7,
            },
            status: None,
        };
        assert!(s.apply(Update::Daemon(info.clone())).daemon);
        assert!(s.apply(Update::Daemon(info.clone())).is_empty());
        assert_eq!(s.daemon(), Some(&info));
    }

    #[test]
    fn feeds_start_loading_and_a_snapshot_makes_them_live() {
        let mut s = State::new();
        for feed in [Feed::Runs, Feed::Approvals, Feed::Triggers, Feed::Defs] {
            assert_eq!(s.feed(feed), FeedState::Loading);
        }
        let c = s.apply(Update::Triggers(vec![]));
        assert!(
            c.feeds && !c.triggers,
            "an empty list changes the feed state only"
        );
        assert_eq!(s.feed(Feed::Triggers), FeedState::Live);
        assert!(s.apply(Update::Triggers(vec![])).is_empty());
        let c = s.apply(Update::Feed(Feed::Defs, FeedState::Failed("boom".into())));
        assert!(c.feeds);
        assert_eq!(s.feed(Feed::Defs), FeedState::Failed("boom".into()));
        let c = s.apply(Update::Defs(DefsSnapshot::default()));
        assert!(c.feeds && !c.defs);
        assert_eq!(s.feed(Feed::Defs), FeedState::Live);
        s.apply(Update::Feed(Feed::Approvals, FeedState::Unavailable));
        assert_eq!(s.feed(Feed::Approvals), FeedState::Unavailable);
    }

    #[test]
    fn offline_sets_disconnected_and_connected_clears_it() {
        let mut s = State::new();
        let offline = Offline {
            kind: OfflineKind::NotRunning,
            detail: "no daemon".into(),
            attempt: 2,
        };
        let c = s.apply(Update::Offline(offline.clone()));
        assert!(c.connection);
        assert_eq!(s.connection(), ConnectionStatus::Disconnected);
        assert_eq!(s.offline(), Some(&offline));
        assert!(s.apply(Update::Offline(offline.clone())).is_empty());
        // A retry in progress keeps the reason on screen.
        s.apply(Update::Connection(ConnectionStatus::Connecting));
        assert_eq!(s.offline(), Some(&offline));
        let c = s.apply(Update::Connection(ConnectionStatus::Connected));
        assert!(c.connection);
        assert!(s.offline().is_none());
    }

    #[test]
    fn executors_are_sorted_idempotent_and_make_their_feed_live() {
        use reno_proto::{ExecutorCaps, ExecutorDescriptor, Steering, VersionRange};
        let executor = |id: &str| ExecutorInfo {
            descriptor: ExecutorDescriptor {
                id: id.parse().unwrap(),
                contract_version: 1,
                adapter_version: "t".into(),
                cli_verified: VersionRange {
                    min: "0".into(),
                    verified: "0".into(),
                },
                caps: ExecutorCaps {
                    live_effort: None,
                    live_model_options: None,
                    steering: Steering::None,
                    resume: false,
                    access_modes: vec![],
                    sandbox_levels: vec![],
                    mcp_injection: false,
                    structured_output: false,
                    deterministic_turn_end: false,
                    plan_mode: false,
                    models: vec![],
                    model_switch: false,
                    isolates_user_config: false,
                    model_aliases: vec![],
                },
            },
            health: None,
            maintenance: None,
            job: None,
        };
        let mut s = State::new();
        assert_eq!(s.feed(Feed::Executors), FeedState::Loading);
        let c = s.apply(Update::Executors(vec![executor("mock"), executor("codex")]));
        assert!(c.executors && c.feeds && !c.is_empty());
        assert_eq!(s.feed(Feed::Executors), FeedState::Live);
        assert_eq!(s.executors()[0].descriptor.id.as_str(), "codex");
        let same = s.apply(Update::Executors(vec![executor("codex"), executor("mock")]));
        assert!(same.is_empty());
        let c = s.apply(Update::Executors(vec![]));
        assert!(c.executors, "an executor that went away is a change");
        assert_eq!(s.feed(Feed::Executors), FeedState::Live);
        let mut merged = Changed::default();
        merged.merge(Changed {
            executors: true,
            ..Changed::default()
        });
        assert!(merged.executors);
    }

    #[test]
    fn triggers_and_defs_are_sorted_and_idempotent() {
        use reno_proto::defs::DefSummary;
        let trigger = |name: &str| TriggerInfo {
            name: name.into(),
            kind: "manual".into(),
            bot: "keeper".into(),
            version: 1,
            status: reno_proto::TriggerStatus::Manual,
            next_due_at_ms: None,
            last_fired_at_ms: None,
            last_run_id: None,
            failures: 0,
            backoff_until_ms: None,
            error: None,
            cwd: None,
        };
        let mut s = State::new();
        let c = s.apply(Update::Triggers(vec![trigger("b"), trigger("a")]));
        assert!(c.triggers);
        assert_eq!(s.triggers()[0].name, "a");
        let same = s.apply(Update::Triggers(vec![trigger("a"), trigger("b")]));
        assert!(same.is_empty());
        let def = |kind, name: &str| DefSummary {
            kind,
            name: name.into(),
            version: 1,
            hash: "0".repeat(64),
            created_at_ms: 1,
        };
        let snapshot = DefsSnapshot {
            defs: vec![
                def(DefKind::Trigger, "t"),
                def(DefKind::Bot, "z"),
                def(DefKind::Bot, "a"),
            ],
            errors: vec![],
        };
        assert!(s.apply(Update::Defs(snapshot.clone())).defs);
        let names: Vec<_> = s.defs().defs.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, ["a", "z", "t"]);
        assert!(s.apply(Update::Defs(snapshot)).is_empty());
    }

    #[test]
    fn fetched_runs_join_the_merged_list_but_not_the_window() {
        let mut s = State::new();
        s.apply(Update::Runs(vec![summary("w", RunStatus::Running, 30)]));
        let c = s.apply(Update::RunsPage {
            filter: vec![RunStatus::Failed, RunStatus::Failed],
            runs: vec![
                summary("w", RunStatus::Failed, 30),
                summary("old", RunStatus::Failed, 5),
            ],
            next: Some(run_id("old")),
        });
        assert!(c.runs);
        // The window's copy wins, and the window itself is untouched.
        assert_eq!(s.runs().len(), 1);
        assert_eq!(s.run(&run_id("w")).unwrap().status, RunStatus::Running);
        let all: Vec<_> = s
            .all_runs(&[])
            .iter()
            .map(|r| r.run_id.to_string())
            .collect();
        assert_eq!(all, ["run_w", "run_old"]);
        assert_eq!(s.all_runs(&[RunStatus::Failed]).len(), 1);
        assert_eq!(
            s.run_page(&[RunStatus::Failed]),
            RunPage::More(run_id("old"))
        );
        assert_eq!(s.run_page(&[]), RunPage::NotLoaded);
        // The same page again changes nothing.
        let again = s.apply(Update::RunsPage {
            filter: vec![RunStatus::Failed],
            runs: vec![summary("old", RunStatus::Failed, 5)],
            next: Some(run_id("old")),
        });
        assert!(again.is_empty());
        // A fetched run that enters the window moves there.
        s.apply(Update::Runs(vec![
            summary("w", RunStatus::Running, 30),
            summary("old", RunStatus::Succeeded, 5),
        ]));
        assert_eq!(s.runs().len(), 2);
        assert_eq!(s.all_runs(&[]).len(), 2);
        // `run.get` of a run outside the window is kept as fetched.
        s.apply(Update::RunUpserted(summary("far", RunStatus::Failed, 1)));
        assert!(s.run(&run_id("far")).is_some());
        assert_eq!(s.runs().len(), 2);
    }

    #[test]
    fn definition_records_and_validations_are_cached() {
        let mut s = State::new();
        let record = DefRecord {
            kind: DefKind::Bot,
            name: "keeper".into(),
            version: 1,
            hash: "0".repeat(64),
            body: serde_json::json!({"access": "supervised"}),
            created_at_ms: 1,
            latest: true,
        };
        assert!(s.apply(Update::DefRecord(record.clone())).defs);
        assert!(s.apply(Update::DefRecord(record.clone())).is_empty());
        assert_eq!(s.def_record(DefKind::Bot, "keeper", 1), Some(&record));
        assert!(s.def_record(DefKind::Bot, "keeper", 2).is_none());
        let result = DefValidateResult {
            valid: true,
            errors: vec![],
            hash: None,
        };
        let update = || Update::Validation {
            kind: DefKind::Bot,
            name: "keeper".into(),
            result: result.clone(),
        };
        assert!(s.apply(update()).defs);
        assert!(s.apply(update()).is_empty());
        assert_eq!(s.validation(DefKind::Bot, "keeper"), Some(&result));
    }

    #[test]
    fn history_replaces_and_marks_its_feed() {
        let mut s = State::new();
        assert_eq!(s.feed(Feed::ApprovalHistory), FeedState::Loading);
        let c = s.apply(Update::ApprovalHistory(vec![]));
        assert!(c.feeds && !c.history);
        assert_eq!(s.feed(Feed::ApprovalHistory), FeedState::Live);
    }

    #[test]
    fn changed_merge_unions() {
        let mut s = State::new();
        let mut c = s.apply(event("a", 0, "x"));
        c.merge(s.apply(Update::Approvals(vec![approval("a", "a", 1)])));
        c.merge(s.apply(event("b", 0, "y")));
        assert!(c.approvals && c.run(&run_id("a")) && c.run(&run_id("b")) && !c.runs);
        assert_eq!(c.touched_runs().count(), 2);
    }

    fn thread(name: &str, runs: &[&str], updated: u64) -> ThreadInfo {
        ThreadInfo {
            thread_id: ThreadId::from_suffix(name).unwrap(),
            status: reno_proto::ThreadStatus::Idle,
            cwd: None,
            project: None,
            worktree: None,
            branch: None,
            base_sha: None,
            workspace: None,
            runs: runs
                .iter()
                .map(|r| reno_proto::ThreadRun {
                    run_id: run_id(r),
                    parent_run: None,
                    start_sha: None,
                })
                .collect(),
            idle_expires_at_ms: None,
            created_at_ms: 0,
            updated_at_ms: updated,
        }
    }

    #[test]
    fn threads_say_which_thread_a_run_is_in_and_a_repeat_changes_nothing() {
        let mut s = State::new();
        assert_eq!(s.feed(Feed::Threads), FeedState::Loading);
        let list = vec![thread("t1", &["a", "b"], 5), thread("t2", &["c"], 9)];
        let c = s.apply(Update::Threads(list.clone()));
        assert!(c.threads && c.feeds);
        assert!(
            c.run(&run_id("a")) && c.run(&run_id("c")),
            "its runs are touched"
        );
        assert_eq!(s.feed(Feed::Threads), FeedState::Live);
        // Newest change first.
        let order: Vec<_> = s
            .threads()
            .iter()
            .map(|t| t.thread_id.to_string())
            .collect();
        assert_eq!(order, ["thr_t2", "thr_t1"]);
        let t1 = ThreadId::from_suffix("t1").unwrap();
        assert_eq!(s.thread_of(&run_id("b")), Some(&t1));
        assert_eq!(s.thread_of(&run_id("zzz")), None);
        assert_eq!(s.thread(&t1).unwrap().runs.len(), 2);

        // Delivering the same list again is a no-op.
        assert!(s.apply(Update::Threads(list)).is_empty());

        // A thread missing from the next list is gone, with its runs' mapping.
        let c = s.apply(Update::Threads(vec![thread("t2", &["c"], 9)]));
        assert!(c.threads && !c.run(&run_id("a")));
        assert_eq!(s.thread_of(&run_id("a")), None);
        assert!(s.thread(&t1).is_none());
        // Only the runs the mirror knows are listed.
        s.apply(Update::RunUpserted(summary("c", RunStatus::Running, 1)));
        let t2 = ThreadId::from_suffix("t2").unwrap();
        assert_eq!(s.thread_runs(&t2).len(), 1);
        assert!(s.thread_runs(&t1).is_empty());
    }
}
