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

use reno_proto::{Approval, DaemonStatusInfo, ExecEvent, HelloResult, RunId, RunSummary, Seq};

/// The state of the connection to the daemon.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum ConnectionStatus {
    /// Not yet connected. The initial state.
    #[default]
    Connecting,
    /// The handshake succeeded and the connection is up.
    Connected,
    /// The connection closed. Reconnecting is out of scope for Phase 0; this is final.
    Disconnected,
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
    /// The seq a resumed `run.events` subscription should start from.
    pub fn resume_from(&self) -> Seq {
        self.highest.and_then(Seq::next).unwrap_or(Seq(0))
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
    touched: BTreeSet<RunId>,
}

impl Changed {
    /// Whether nothing changed.
    pub fn is_empty(&self) -> bool {
        !(self.connection || self.daemon || self.runs || self.approvals) && self.touched.is_empty()
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
            }
            Update::Daemon(info) => {
                if self.daemon.as_ref() != Some(&info) {
                    self.daemon = Some(info);
                    changed.daemon = true;
                }
            }
            Update::Runs(list) => self.replace_runs(list, &mut changed),
            Update::RunUpserted(summary) => self.upsert_run(summary, &mut changed),
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
            }
        }
        changed
    }

    fn upsert_run(&mut self, summary: RunSummary, changed: &mut Changed) {
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
    }

    /// The connection status.
    pub fn connection(&self) -> ConnectionStatus {
        self.connection
    }

    /// The daemon info, once the handshake has finished.
    pub fn daemon(&self) -> Option<&DaemonInfo> {
        self.daemon.as_ref()
    }

    /// One run's summary.
    pub fn run(&self, id: &RunId) -> Option<&RunSummary> {
        self.runs.get(id)
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
            run_id: run_id(name),
            task_id: TaskId::from_suffix(name).unwrap(),
            executor: "claude-code".parse::<ExecutorId>().unwrap(),
            status,
            attempt: 1,
            title: format!("run {name}"),
            model: None,
            stop_reason: None,
            created_at_ms: created,
            updated_at_ms: created,
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
    fn empty_transcript_resumes_from_zero() {
        assert_eq!(Transcript::default().resume_from(), Seq(0));
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
    fn changed_merge_unions() {
        let mut s = State::new();
        let mut c = s.apply(event("a", 0, "x"));
        c.merge(s.apply(Update::Approvals(vec![approval("a", "a", 1)])));
        c.merge(s.apply(event("b", 0, "y")));
        assert!(c.approvals && c.run(&run_id("a")) && c.run(&run_id("b")) && !c.runs);
        assert_eq!(c.touched_runs().count(), 2);
    }
}
