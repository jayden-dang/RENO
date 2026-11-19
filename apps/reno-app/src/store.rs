//! The store: one GPUI `Entity` that owns the `reno_client::State` mirror (ARCHITECTURE 17.6).
//!
//! # How the mirror is bridged
//!
//! `reno-client` runs on tokio and hands over a bounded `futures` channel of `Update`s
//! (ARCH-11, ARCH-12). The store never blocks on tokio:
//!
//! 1. A supervised `Mirror` is started on tokio (`gpui_tokio::Tokio::spawn`), because it spawns
//!    its tasks there. It connects, reconnects with a capped backoff when the daemon goes away,
//!    and keeps every feed current (runs, approvals, triggers, definitions).
//! 2. [`Store::attach`] reads the channel on GPUI's own executor with a task the store owns. A
//!    `futures` receiver needs no tokio context, so each `next().await` wakes the foreground
//!    task directly.
//! 3. Each update goes through the pure `State::apply`, and `notify` is called only for what the
//!    returned `Changed` says a view shows. A transcript-only change redraws nothing unless it
//!    is the open run's.
//!
//! Actions (`decide`, `run_task`, `steer`, `cancel_run`, `fire_trigger`, paging, definition
//! reads) run on tokio and report back, either as a state update (the answer to a read) or as a
//! [`StoreEvent`] (a failure to show). Nothing polls (ARCH-10): what has no watch is fetched when
//! a screen opens, when the connection comes back, or when a watch says it changed.
//!
//! Dropping the store drops its task and the `Mirror`, which cancels its tracked tasks.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use gpui::{AppContext as _, Context, EventEmitter, Task};
use gpui_tokio::Tokio;
use reno_client::answers::{AnswerError, AnswerRequest, QuestionDesk, QuestionRules};
use reno_client::fake::{FakeConfig, FakeDaemon};
use reno_client::launcher::{DaemonKeeper, KeeperConfig, Launcher};
use reno_client::{
    Changed, Client, ClientError, ConnectionStatus, Connector, Feed, FeedState, Mirror,
    MirrorConfig, Offline, OfflineKind, State, Transcript, Update, Updates,
};
use reno_proto::defs::DefKind;
use reno_proto::{
    ApprovalDecision, ApprovalId, ExecutorId, ExecutorInfo, ExecutorRegistryListResult,
    FiringOutcome, JobFailure, JobKind, JobPhase, RoutingDecision, RunId, RunStatus, SignedIn,
    TaskRunParams, ThreadId, WorkspaceDiffResult, WorkspaceInfo,
};

mod signin;
pub(crate) use signin::{SigninPhase, SigninView};

use crate::summary;
use crate::vm;
use crate::vm::capabilities::Capabilities;
use crate::vm::changes::{Loaded, Patch, ReviewScope};
use crate::vm::registry::RegistryData;

/// How many finished runs one page of All runs asks for.
pub(crate) const RUNS_PAGE: u32 = 20;

/// Things the shell reacts to that are not state: a started run, a failed action.
#[derive(Clone, Debug)]
pub(crate) enum StoreEvent {
    RunStarted(RunId),
    /// Show this run: what needs you is answered inside it (ui-brief 2026-11-17).
    ShowRun(RunId),
    /// A trigger was fired by hand: what became of it.
    TriggerFired {
        name: String,
        outcome: FiringOutcome,
        run: Option<RunId>,
    },
    /// An executor's adapter was installed and probed again.
    ExecutorInstalled(String),
    /// An install or update ended well: the executor's own name, what it did, the version it is
    /// on now, and whether a sign-in is the one step left.
    ExecutorJobDone {
        name: String,
        kind: JobKind,
        version: Option<String>,
        sign_in: bool,
        /// The executor was added from the ACP registry: an install reads "added".
        registry: bool,
    },
    /// A person stopped an install or update.
    ExecutorJobStopped(String),
    /// Reno looked again after a sign-in in Terminal and the executor is still signed out.
    StillSignedOut(String),
    Failed(String),
    /// A task or steer sent with this ticket did not go through (`Failed` says why): the
    /// composer that sent it gets its draft back.
    SendFailed(u64),
    /// Why a send with this ticket was refused, in the daemon's words (follows `SendFailed`).
    SendRefused {
        ticket: u64,
        why: String,
    },
    /// `thread.close` answered: what became of the worktree.
    ThreadClosed,
}

/// What the daemon says about where one run went.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum RunRouting {
    /// Asked, no answer yet.
    Loading,
    /// The decision; `None` for a run made before routing existed.
    Ready(Option<RoutingDecision>),
    Failed(String),
}

/// The last lines of output of an executor's job, as `executor.job.log` answered.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct JobLog {
    pub(crate) lines: Vec<String>,
    /// Older lines were dropped.
    pub(crate) truncated: bool,
}

/// What a new executor list says about jobs that ended since `before`: done (the job is gone and
/// the health shows the result) or stopped by a person. A failure is shown on the line.
fn job_events(
    before: &[ExecutorInfo],
    after: &[ExecutorInfo],
    registry: Option<&ExecutorRegistryListResult>,
) -> Vec<StoreEvent> {
    let live = |info: &ExecutorInfo| {
        info.job
            .as_ref()
            .filter(|j| !matches!(j.phase, JobPhase::Failed { .. }))
            .map(|j| (j.kind, matches!(j.phase, JobPhase::Queued { .. })))
    };
    let mut events = Vec::new();
    for new in after {
        let Some(old) = before.iter().find(|o| o.descriptor.id == new.descriptor.id) else {
            continue;
        };
        let Some((kind, was_queued)) = live(old) else {
            continue;
        };
        let from_registry = new.source == reno_proto::ExecutorSource::Registry;
        let name = registry
            .filter(|_| from_registry)
            .and_then(|l| l.entries.iter().find(|e| e.executor == new.descriptor.id))
            .map_or_else(
                || summary::executor_name(new.descriptor.id.as_str()),
                |e| e.name.clone(),
            );
        match &new.job {
            None => {
                let health = new.health.as_ref();
                events.push(StoreEvent::ExecutorJobDone {
                    name,
                    kind,
                    version: health.and_then(|h| h.installed.version().map(str::to_owned)),
                    sign_in: kind == JobKind::Install
                        && health.is_some_and(|h| matches!(h.signed_in, SignedIn::No { .. })),
                    registry: from_registry,
                });
            }
            // Cancelling an update that only waited is not worth a toast: the line says so.
            Some(job)
                if !was_queued
                    && matches!(
                        job.phase,
                        JobPhase::Failed {
                            failure: JobFailure::Cancelled,
                            ..
                        }
                    ) =>
            {
                events.push(StoreEvent::ExecutorJobStopped(name));
            }
            Some(_) => {}
        }
    }
    events
}

/// What the daemon said about a run's workspace: where it is, and what changed in it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RunChanges {
    /// `workspace.get`; `Ready(None)` for a run that never had a workspace.
    pub(crate) workspace: Loaded<Option<WorkspaceInfo>>,
    /// `workspace.diff` without a path: the list of files.
    pub(crate) diff: Loaded<WorkspaceDiffResult>,
}

/// A run's routing, and what it was fetched for: the decision changes while the run starts (a
/// fallback), so it is fetched again when the run's executor or status changes.
struct RoutingEntry {
    key: Option<(ExecutorId, RunStatus)>,
    routing: RunRouting,
}

pub(crate) struct Store {
    state: State,
    /// Keeps the mirror's tasks alive; dropping it cancels them.
    mirror: Option<Mirror>,
    /// The fake daemon of `--fake`, kept alive for as long as the store.
    fake: Option<Arc<FakeDaemon>>,
    /// Which not-yet controls show: set from the connection (the fake turns them on).
    capabilities: Capabilities,
    /// Where the real daemon's socket is, shown in Settings.
    socket: Option<PathBuf>,
    /// Approvals with a decision in flight (their buttons are disabled).
    deciding: BTreeSet<ApprovalId>,
    /// Triggers being fired by hand (their buttons are disabled).
    firing: BTreeSet<String>,
    /// Executors being probed again: `None` is all of them (their buttons are disabled).
    checking: BTreeSet<Option<ExecutorId>>,
    /// Executors being installed.
    installing: BTreeSet<ExecutorId>,
    /// Executors with a job request in flight (their buttons are disabled).
    starting: BTreeSet<ExecutorId>,
    /// Output of each executor's current or last job, fetched on demand.
    job_logs: BTreeMap<ExecutorId, JobLog>,
    /// Executors whose log is shown: it is fetched again when their job changes (ARCH-12).
    log_open: BTreeSet<ExecutorId>,
    /// Log fetches in flight.
    log_fetching: BTreeSet<ExecutorId>,
    /// The ACP registry's list: the last answer and how the next one went. Read when the
    /// Executors screen opens, on Refresh, after a reconnect and when a registry executor
    /// appears, goes or ends a job (ARCH-10: nothing polls).
    registry: RegistryData,
    /// The Executors screen has been opened: the list is read as soon as a daemon with the
    /// registry is connected, and again after a reconnect.
    registry_wanted: bool,
    /// Registry removals in flight, by registry id.
    registry_removing: BTreeSet<String>,
    /// Why a removal was refused, shown on the agent's line until the next try.
    registry_refused: BTreeMap<String, String>,
    /// Sign-ins of added agents (`executor.signin.start`) that run or failed, by executor.
    signins: BTreeMap<ExecutorId, signin::SigninRun>,
    /// Executors with a sign-in command open in Terminal, looked at again when the person is back.
    signin_terminals: BTreeSet<ExecutorId>,
    /// Pages of finished runs being asked for.
    paging: bool,
    /// How many finished runs one page asks for.
    runs_page: u32,
    /// All runs is showing: live runs' transcripts are followed for their tails.
    follow_live: bool,
    /// Runs being fetched with `run.get`.
    fetching_runs: BTreeSet<RunId>,
    /// Definition versions being fetched with `def.get`.
    fetching_defs: BTreeSet<(DefKind, String, u32)>,
    /// Definitions being validated.
    validating: BTreeSet<(DefKind, String)>,
    /// The standing rules, as `rule.list` last said; `None` until it has. They have no watch, so
    /// they are read when Settings opens and after a decision that makes one or a revoke.
    rules: Option<Result<Vec<reno_proto::StandingRule>, String>>,
    /// A page asked for the standing rules: they are read as soon as a daemon that has them is
    /// connected, even when it was asked before the daemon said what it supports.
    rules_wanted: bool,
    /// Rules being revoked (their buttons are disabled).
    revoking: BTreeSet<reno_proto::RuleId>,
    /// Ages use the system clock (a real daemon) or the state's newest timestamp (the fake).
    system_clock: bool,
    /// The clock reading when the last `daemon.status` arrived, to carry its uptime forward.
    status_seen_at_ms: u64,
    /// The run the Run screen shows: its transcript changes notify.
    open: Option<RunId>,
    /// What each run changed, as `workspace.get` and `workspace.diff` last said.
    changes: BTreeMap<(RunId, ReviewScope), RunChanges>,
    /// Runs whose changes are being read, and those asked again meanwhile.
    reading_changes: BTreeSet<(RunId, ReviewScope)>,
    reread_changes: BTreeSet<(RunId, ReviewScope)>,
    /// The patch of one file of a run, by `(run, path)`.
    patches: BTreeMap<(RunId, ReviewScope, String), Loaded<Patch>>,
    /// Patches whose diff moved on since they were read: they stay on screen, and are read again
    /// quietly the next time they are asked for.
    stale_patches: BTreeSet<(RunId, ReviewScope, String)>,
    /// How many patch reads were sent (tests assert a quiet refetch happened).
    #[cfg(test)]
    patch_reads: usize,
    /// Routing decisions by run, from `task.run` and `run.routing`.
    routings: BTreeMap<RunId, RoutingEntry>,
    /// Tasks and steers sent and not yet answered.
    sending: u32,
    /// Repository roots of the folders runs were submitted in (the sidebar's projects).
    roots: crate::local::Roots,
    /// When you last opened each run; kept on disk next to the socket for the real daemon.
    seen: crate::vm::projects::Seen,
    seen_file: Option<PathBuf>,
    /// What the composer's picker remembers; kept on disk next to the socket for the real
    /// daemon, in memory only for the fake one.
    composer: crate::vm::composer_prefs::ComposerPrefs,
    composer_file: Option<PathBuf>,
    /// When the last `run.configure` of each run takes hold, as the daemon answered.
    configured: BTreeMap<RunId, ConfigApplied>,
    /// The run on screen: it is seen as it changes.
    looking: Option<RunId>,
    /// The thread `task.run` named for a run, before `thread.list` says it.
    thread_hints: BTreeMap<RunId, ThreadId>,
    /// What `thread.close` last answered, by thread.
    closes: BTreeMap<ThreadId, reno_proto::ThreadCloseResult>,
    /// Threads being closed (their button is disabled).
    closing: BTreeSet<ThreadId>,
    /// The reason of the last failed request, for a sender that shows it itself.
    last_failure: Option<String>,
    /// The boot task and then the update consumer. Owned here so it dies with the store.
    bridge: Option<Task<()>>,
}

impl EventEmitter<StoreEvent> for Store {}

/// When a run's last changes of effort and of model options take hold.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct ConfigApplied {
    pub(crate) effort: Option<reno_proto::ConfigTiming>,
    pub(crate) options: Option<reno_proto::ConfigTiming>,
}

impl Store {
    /// A store that has not connected yet.
    pub(crate) fn connecting() -> Self {
        Self {
            state: State::new(),
            mirror: None,
            fake: None,
            capabilities: Capabilities::REAL,
            socket: None,
            deciding: BTreeSet::new(),
            firing: BTreeSet::new(),
            checking: BTreeSet::new(),
            installing: BTreeSet::new(),
            starting: BTreeSet::new(),
            job_logs: BTreeMap::new(),
            log_open: BTreeSet::new(),
            log_fetching: BTreeSet::new(),
            registry: RegistryData::default(),
            registry_wanted: false,
            rules_wanted: false,
            registry_removing: BTreeSet::new(),
            registry_refused: BTreeMap::new(),
            signins: BTreeMap::new(),
            signin_terminals: BTreeSet::new(),
            follow_live: false,
            paging: false,
            runs_page: RUNS_PAGE,
            fetching_runs: BTreeSet::new(),
            fetching_defs: BTreeSet::new(),
            validating: BTreeSet::new(),
            rules: None,
            revoking: BTreeSet::new(),
            system_clock: false,
            status_seen_at_ms: 0,
            open: None,
            changes: BTreeMap::new(),
            reading_changes: BTreeSet::new(),
            reread_changes: BTreeSet::new(),
            patches: BTreeMap::new(),
            stale_patches: BTreeSet::new(),
            #[cfg(test)]
            patch_reads: 0,
            routings: BTreeMap::new(),
            sending: 0,
            roots: crate::local::Roots::default(),
            seen: crate::vm::projects::Seen::default(),
            seen_file: None,
            composer: crate::vm::composer_prefs::ComposerPrefs::default(),
            composer_file: None,
            configured: BTreeMap::new(),
            looking: None,
            thread_hints: BTreeMap::new(),
            closes: BTreeMap::new(),
            closing: BTreeSet::new(),
            last_failure: None,
            bridge: None,
        }
    }

    /// Asks the shell to show `run`.
    pub(crate) fn show_run(&mut self, run: RunId, cx: &mut Context<Self>) {
        cx.emit(StoreEvent::ShowRun(run));
    }

    /// The canonical repository root `dir` is in, if any.
    pub(crate) fn root_of(&self, dir: &std::path::Path) -> Option<PathBuf> {
        self.roots.root_of(dir)
    }

    /// When you last opened each run.
    pub(crate) fn seen(&self) -> &crate::vm::projects::Seen {
        &self.seen
    }

    /// What the composer's picker remembers.
    pub(crate) fn composer_prefs(&self) -> &crate::vm::composer_prefs::ComposerPrefs {
        &self.composer
    }

    /// Changes what the composer remembers and saves it, when there is a file for it.
    pub(crate) fn update_composer_prefs(
        &mut self,
        change: impl FnOnce(&mut crate::vm::composer_prefs::ComposerPrefs),
        cx: &mut Context<Self>,
    ) {
        change(&mut self.composer);
        if let Some(path) = &self.composer_file {
            crate::local::save_composer(path, &self.composer);
        }
        cx.notify();
    }

    /// The run on screen, if one is.
    pub(crate) fn looking_at(&self) -> Option<&RunId> {
        self.looking.as_ref()
    }

    /// The Run screen shows `run` (`None`: no run is on screen). The run left and the run shown
    /// are both seen now.
    pub(crate) fn look_at(&mut self, run: Option<RunId>, cx: &mut Context<Self>) {
        let now = self.now_ms();
        for run in self.looking.iter().chain(run.iter()) {
            self.seen.mark(run, now);
        }
        self.looking = run;
        if let Some(path) = &self.seen_file {
            crate::local::save_seen(path, &self.seen);
        }
        cx.notify();
    }

    pub(crate) fn state(&self) -> &State {
        &self.state
    }

    pub(crate) fn connection(&self) -> ConnectionStatus {
        self.state.connection()
    }

    /// Why there is no daemon, in the words Home shows.
    pub(crate) fn offline_reason(&self) -> Option<String> {
        self.state.offline().map(offline_reason)
    }

    /// The socket the app was pointed at (the real daemon), when there is one.
    pub(crate) fn socket(&self) -> Option<&std::path::Path> {
        self.socket.as_deref()
    }

    /// `RENO_REGISTRY_DEMO=offline`: the fake daemon serves its cached registry copy. Does
    /// nothing against a real daemon.
    pub(crate) fn demo_registry_offline(&self) {
        if let Some(fake) = &self.fake {
            fake.set_registry_offline(true);
        }
    }

    /// `RENO_REGISTRY_DEMO=browser:...`: what the fake daemon says of the next sign-in of `id`.
    /// Does nothing against a real daemon.
    pub(crate) fn demo_script_signin(&self, id: &ExecutorId, events: Vec<reno_proto::SigninEvent>) {
        if let Some(fake) = &self.fake {
            fake.script_signin(id, events);
        }
    }

    #[cfg(test)]
    pub(crate) fn fake(&self) -> Option<&Arc<FakeDaemon>> {
        self.fake.as_ref()
    }

    /// Which composer controls are available on this connection.
    pub(crate) fn capabilities(&self) -> Capabilities {
        self.capabilities
    }

    #[cfg(test)]
    pub(crate) fn set_capabilities(&mut self, capabilities: Capabilities, cx: &mut Context<Self>) {
        self.capabilities = capabilities;
        cx.notify();
    }

    /// The ONE seam to the answer contract. Today it is the fake daemon's mocked ROAD-1 contract,
    /// and only where [`Capabilities::answer_questions`] is on; ROAD-1 replaces what this returns
    /// with the real daemon's `question.answer`, and nothing else in the app changes.
    fn question_desk(&self) -> Option<&dyn QuestionDesk> {
        if !self.capabilities.answer_questions {
            return None;
        }
        self.fake.as_deref().map(|fake| fake as &dyn QuestionDesk)
    }

    /// What the question `request` of `run` takes (typed text, its length).
    pub(crate) fn question_rules(&self, run: &RunId, request: &str) -> QuestionRules {
        self.question_desk()
            .map_or_else(QuestionRules::default, |desk| desk.rules(run, request))
    }

    /// Sends an answer. `Err(QuestionNotPending)` when this connection cannot answer at all.
    pub(crate) fn answer_question(
        &mut self,
        run: &RunId,
        request: AnswerRequest,
        cx: &mut Context<Self>,
    ) -> Result<(), AnswerError> {
        let result = self
            .question_desk()
            .ok_or(AnswerError::QuestionNotPending)
            .and_then(|desk| desk.answer(run, request));
        cx.notify();
        result
    }

    pub(crate) fn is_deciding(&self, id: &ApprovalId) -> bool {
        self.deciding.contains(id)
    }

    pub(crate) fn is_firing(&self, name: &str) -> bool {
        self.firing.contains(name)
    }

    /// Whether a probe of this executor (`None`: of all of them) was asked for and has not
    /// answered.
    pub(crate) fn is_checking(&self, id: Option<&ExecutorId>) -> bool {
        self.checking.contains(&id.cloned())
    }

    pub(crate) fn is_installing(&self, id: &ExecutorId) -> bool {
        self.installing.contains(id)
    }

    /// Whether ages and job clocks follow the system clock (a real daemon) rather than the
    /// state's newest timestamp (the fake).
    pub(crate) fn clock_is_live(&self) -> bool {
        self.system_clock
    }

    /// Whether a job request for this executor was sent and has not answered.
    pub(crate) fn is_starting(&self, id: &ExecutorId) -> bool {
        self.starting.contains(id)
    }

    /// The output of this executor's current or last job, once fetched.
    pub(crate) fn job_log(&self, id: &ExecutorId) -> Option<&JobLog> {
        self.job_logs.get(id)
    }

    pub(crate) fn is_log_open(&self, id: &ExecutorId) -> bool {
        self.log_open.contains(id)
    }

    /// A page of finished runs is being asked for.
    pub(crate) fn is_paging(&self) -> bool {
        self.paging
    }

    #[cfg(test)]
    pub(crate) fn set_runs_page(&mut self, limit: u32) {
        self.runs_page = limit;
    }

    pub(crate) fn is_validating(&self, kind: DefKind, name: &str) -> bool {
        self.validating.contains(&(kind, name.to_owned()))
    }

    /// "Now" for ages on Home.
    pub(crate) fn now_ms(&self) -> u64 {
        summary::reference_now(&self.state, self.system_clock.then(system_now_ms))
    }

    /// When the last `daemon.status` arrived, on the same clock as [`Store::now_ms`].
    pub(crate) fn status_seen_at_ms(&self) -> u64 {
        self.status_seen_at_ms
    }

    /// Applies one update and notifies only for what changed on screen.
    pub(crate) fn apply(&mut self, update: Update, cx: &mut Context<Self>) -> Changed {
        let before =
            matches!(update, Update::Executors(_)).then(|| self.state.executors().to_vec());
        let changed = self.state.apply(update);
        if let Some(before) = before {
            let events = job_events(
                &before,
                self.state.executors(),
                self.registry.listing.as_ref(),
            );
            let registry_moved = !events.is_empty()
                || registry_agents(&before) != registry_agents(self.state.executors());
            for event in events {
                cx.emit(event);
            }
            // What is added, gone or finished changes the registry's list.
            if registry_moved && self.registry.listing.is_some() {
                self.load_registry(false, cx);
            }
            if changed.executors {
                for id in self.log_open.clone() {
                    self.fetch_job_log(id, cx);
                }
            }
        }
        if changed.daemon {
            self.status_seen_at_ms = self.now_ms();
            // The daemon's features are known now: a registry list that was waiting can go.
            if self.registry_wanted && self.registry.listing.is_none() {
                self.load_registry(false, cx);
            }
            // So can standing rules a page asked for before then.
            if self.rules_wanted && self.rules.is_none() {
                self.load_rules(cx);
            }
        }
        let open_run_changed = self.open.as_ref().is_some_and(|run| {
            changed.run(run) || self.thread_run_ids(run).iter().any(|r| changed.run(r))
        });
        if changed.connection
            || changed.daemon
            || changed.runs
            || changed.approvals
            || changed.triggers
            || changed.executors
            || changed.defs
            || changed.history
            || changed.threads
            || changed.feeds
            || open_run_changed
            // All runs is showing: a followed run's new event moves its lane's tail.
            || (self.follow_live && changed.touched_runs().next().is_some())
        {
            cx.notify();
        }
        if changed.connection && self.state.connection() == ConnectionStatus::Connected {
            self.refetch_after_reconnect(cx);
        }
        // What has no watch is read again when a watch says it may have changed (ARCH-10).
        if changed.defs {
            self.ensure_def_records(cx);
        }
        if changed.triggers {
            for run in vm::triggers::runs_to_fetch(&self.state) {
                self.ensure_run(&run, cx);
            }
        }
        if changed.runs {
            self.follow_waiting_questions(cx);
            if self.follow_live {
                self.follow_live_runs(cx);
            }
        }
        if changed.threads
            && let Some(run) = self.open.clone()
        {
            self.follow_thread_siblings(&run, cx);
        }
        if changed.approvals && self.state.feed(Feed::ApprovalHistory) == FeedState::Live {
            self.load_history(cx);
        }
        if open_run_changed
            && let Some(run) = self.open.clone()
            && let Some((bot, version)) = vm::runs::bot_to_fetch(&self.state, &run)
        {
            self.ensure_bot(&bot, version, cx);
        }
        changed
    }

    /// Shows the daemon as unreachable with `reason`, without a mirror. For tests of the offline
    /// screens.
    pub(crate) fn go_offline(&mut self, reason: impl Into<String>, cx: &mut Context<Self>) {
        self.mirror = None;
        self.apply(
            Update::Offline(Offline {
                kind: OfflineKind::Other,
                detail: reason.into(),
                attempt: 0,
            }),
            cx,
        );
    }

    /// Starts `FakeDaemon` in-process, over the in-memory transport.
    pub(crate) fn start_fake(&mut self, cx: &mut Context<Self>) {
        self.start_fake_with(
            FakeConfig {
                registry_agents: true,
                question_run: true,
                ..FakeConfig::default()
            },
            cx,
        );
    }

    /// Like [`Store::start_fake`], with a config (an empty world, missing features).
    pub(crate) fn start_fake_with(&mut self, config: FakeConfig, cx: &mut Context<Self>) {
        self.roots = crate::local::Roots::literal();
        match FakeDaemon::start(config) {
            Ok(daemon) => {
                let daemon = Arc::new(daemon);
                self.fake = Some(Arc::clone(&daemon));
                self.capabilities = Capabilities::FAKE;
                let connector: Connector = Arc::new(move || {
                    let daemon = Arc::clone(&daemon);
                    Box::pin(async move { daemon.connect().await })
                });
                self.start_connector(connector, cx);
            }
            Err(e) => self.go_offline(format!("the fake daemon did not start: {e}"), cx),
        }
    }

    /// Connects to the daemon's socket (`reno_client::default_socket_path`). While nothing
    /// answers, Home says the daemon is not running and how to start it, and the mirror keeps
    /// trying.
    pub(crate) fn start_socket(&mut self, cx: &mut Context<Self>) {
        self.system_clock = true;
        let Some(path) = reno_client::default_socket_path() else {
            self.go_offline("Cannot locate the daemon socket: HOME is not set.", cx);
            return;
        };
        if let Some(file) = crate::local::seen_path(&path) {
            self.seen = crate::local::load_seen(&file, system_now_ms());
            self.seen_file = Some(file);
        }
        if let Some(file) = crate::local::composer_path(&path) {
            self.composer = crate::local::load_composer(&file);
            self.composer_file = Some(file);
        }
        match daemon_keeper(&path, |key| std::env::var(key).ok()) {
            Some(keeper) => {
                self.socket = Some(path);
                self.start_connector(keeper.connector(), cx);
            }
            None => self.connect_socket(path, cx),
        }
    }

    /// Connects to `path` through `keeper`, which starts the daemon when nothing answers.
    #[cfg(test)]
    pub(crate) fn connect_keeper(
        &mut self,
        path: PathBuf,
        keeper: &Arc<DaemonKeeper>,
        cx: &mut Context<Self>,
    ) {
        self.socket = Some(path);
        self.start_connector(keeper.connector(), cx);
    }

    /// Connects to the daemon socket at `path`, and reconnects when it is lost.
    pub(crate) fn connect_socket(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.socket = Some(path.clone());
        let connector: Connector = Arc::new(move || {
            let path = path.clone();
            Box::pin(async move { Client::connect_unix(&path).await })
        });
        self.start_connector(connector, cx);
    }

    /// Starts a supervised mirror over `connector`.
    pub(crate) fn start_connector(&mut self, connector: Connector, cx: &mut Context<Self>) {
        let boot = Tokio::spawn(cx, async move {
            Mirror::supervise(connector, MirrorConfig::default())
        });
        self.bridge = Some(cx.spawn(async move |this, cx| {
            let booted = boot.await;
            this.update(cx, |store, cx| match booted {
                Ok((mirror, updates)) => store.attach(mirror, updates, cx),
                Err(e) => store.go_offline(format!("the connection task failed: {e}"), cx),
            })
            .ok();
        }));
    }

    /// Takes the mirror and its update channel, and consumes the channel on GPUI's executor.
    pub(crate) fn attach(&mut self, mirror: Mirror, mut updates: Updates, cx: &mut Context<Self>) {
        self.mirror = Some(mirror);
        self.bridge = Some(cx.spawn(async move |this, cx| {
            while let Some(update) = updates.next().await {
                let applied = this.update(cx, |store, cx| {
                    store.apply(update, cx);
                });
                if applied.is_err() {
                    return;
                }
            }
        }));
    }

    /// The connected client. A tab whose answers stay in the tab, not the mirror, calls this.
    pub(crate) fn client(&self) -> Option<Client> {
        self.mirror.as_ref().and_then(Mirror::client)
    }

    /// Whether a task or a steer is on its way to the daemon.
    pub(crate) fn is_sending(&self) -> bool {
        self.sending > 0
    }

    /// The routing decision of `run`, as far as it is known.
    pub(crate) fn routing(&self, run: &RunId) -> Option<&RunRouting> {
        self.routings.get(run).map(|e| &e.routing)
    }

    /// `route.preview` for `params`: the decision the daemon would make. The caller owns the
    /// task; dropping it cancels the request.
    pub(crate) fn preview_route(
        &self,
        params: TaskRunParams,
        cx: &gpui::App,
    ) -> Task<Result<RoutingDecision, String>> {
        let Some(client) = self.client() else {
            return Task::ready(Err("the daemon is not connected".to_owned()));
        };
        if !self.state.supports(reno_proto::FEATURE_ROUTING) {
            return Task::ready(Err("this daemon cannot preview routes".to_owned()));
        }
        let call = Tokio::spawn(cx, async move { client.route_preview(params).await });
        cx.background_spawn(async move { flatten(call.await) })
    }

    /// Reads `run.routing` for `run` unless the answer held is for its current executor and
    /// status. Quiet: a missing decision is shown as missing, not as an error toast.
    pub(crate) fn ensure_routing(&mut self, run: &RunId, cx: &mut Context<Self>) {
        if !self.state.supports(reno_proto::FEATURE_ROUTING) {
            return;
        }
        let Some(summary) = self.state.run(run) else {
            return;
        };
        let key = Some((summary.executor.clone(), summary.status));
        if self
            .routings
            .get(run)
            .is_some_and(|e| e.key == key || e.routing == RunRouting::Loading)
        {
            return;
        }
        let kept = self.routings.remove(run).map(|e| e.routing);
        self.routings.insert(
            run.clone(),
            RoutingEntry {
                key: key.clone(),
                // Keep showing the old answer while the new one comes.
                routing: kept
                    .filter(|r| matches!(r, RunRouting::Ready(_)))
                    .unwrap_or(RunRouting::Loading),
            },
        );
        let done_run = run.clone();
        let failed_run = run.clone();
        self.call(
            "Could not read the routing decision",
            {
                let run = run.clone();
                move |client| async move { client.run_routing(&run).await }
            },
            cx,
            D {
                ok: Box::new(move |store, decision, cx| {
                    store.routings.insert(
                        done_run,
                        RoutingEntry {
                            key,
                            routing: RunRouting::Ready(decision),
                        },
                    );
                    cx.notify();
                }),
                err: Box::new(move |store, cx| {
                    if let Some(entry) = store.routings.get_mut(&failed_run)
                        && entry.routing == RunRouting::Loading
                    {
                        entry.routing = RunRouting::Failed("the daemon would not say".to_owned());
                        // Fetched again when the run changes, not in a loop.
                    }
                    cx.notify();
                }),
                quiet: true,
            },
        );
        cx.notify();
    }

    /// What `run` changed, as last read; `None` before it was asked for.
    pub(crate) fn changes(&self, run: &RunId) -> Option<&RunChanges> {
        self.changes_in(run, ReviewScope::Thread)
    }

    /// What `run` changed within `scope`, as last read.
    pub(crate) fn changes_in(&self, run: &RunId, scope: ReviewScope) -> Option<&RunChanges> {
        self.changes.get(&(run.clone(), scope))
    }

    /// The patch of `path` in `run` within `scope`, as last read.
    pub(crate) fn patch_in(
        &self,
        run: &RunId,
        scope: ReviewScope,
        path: &str,
    ) -> Option<&Loaded<Patch>> {
        self.patches.get(&(run.clone(), scope, path.to_owned()))
    }

    /// Whether the daemon can say what a run changed.
    pub(crate) fn supports_changes(&self) -> bool {
        self.state.supports(reno_proto::FEATURE_RUN_CHANGES)
    }

    /// Whether the daemon serves the Files tab (feature `files`).
    pub(crate) fn supports_files(&self) -> bool {
        self.state.supports(reno_proto::FEATURE_FILES)
    }

    /// Whether the daemon serves the Terminal tab (feature `terminals`).
    pub(crate) fn supports_terminals(&self) -> bool {
        self.state.supports(reno_proto::FEATURE_TERMINALS)
    }

    /// Whether the daemon has threads (feature `threads`).
    pub(crate) fn supports_threads(&self) -> bool {
        self.state.supports(reno_proto::FEATURE_THREADS)
    }

    /// Whether the daemon has History and the Review upgrade (feature `history`): the commit
    /// log, the uncommitted scope, file sources for syntax colour, and the diff checksum.
    pub(crate) fn supports_history(&self) -> bool {
        self.state.supports(reno_proto::FEATURE_HISTORY)
    }

    /// Whether the daemon can answer a diff over `scope`.
    pub(crate) fn supports_scope(&self, scope: ReviewScope) -> bool {
        self.supports_changes() && scope.needs().is_none_or(|f| self.state.supports(f))
    }

    /// How many patch reads were sent so far.
    #[cfg(test)]
    pub(crate) fn patch_reads(&self) -> usize {
        self.patch_reads
    }

    /// Whether the patches of `scope` wait to be read again.
    #[cfg(test)]
    pub(crate) fn has_stale_patches(&self, run: &RunId, scope: ReviewScope) -> bool {
        self.stale_patches
            .iter()
            .any(|(r, s, _)| r == run && *s == scope)
    }

    /// Reads where `run` works and which files its thread changed.
    pub(crate) fn load_changes(&mut self, run: &RunId, cx: &mut Context<Self>) {
        self.load_changes_in(run, ReviewScope::Thread, cx);
    }

    /// Reads where `run` works and which files it changed (`workspace.get` and `workspace.diff`),
    /// over `scope`. What was read before stays on screen while the new answer comes. Asked
    /// again while a read is in flight, it reads once more when that ends.
    pub(crate) fn load_changes_in(
        &mut self,
        run: &RunId,
        scope: ReviewScope,
        cx: &mut Context<Self>,
    ) {
        if !self.supports_scope(scope) {
            return;
        }
        let key = (run.clone(), scope);
        if !self.reading_changes.insert(key.clone()) {
            self.reread_changes.insert(key);
            return;
        }
        self.changes.entry(key.clone()).or_insert(RunChanges {
            workspace: Loaded::Loading,
            diff: Loaded::Loading,
        });
        let (ok_key, err_key, id) = (key.clone(), key, run.clone());
        self.call(
            "Could not read the run's changes",
            move |client| async move {
                let diff_call = async {
                    match scope.diff_scope() {
                        None => client.workspace_diff(id.clone(), None).await,
                        Some(diff_scope) => {
                            client
                                .workspace_diff_scoped(id.clone(), None, diff_scope)
                                .await
                        }
                    }
                };
                let (workspace, diff) =
                    futures::future::join(client.get_workspace(id.clone()), diff_call).await;
                let workspace = match workspace {
                    Ok(info) => Loaded::Ready(Some(info)),
                    Err(e) if e.code() == Some(reno_proto::ErrorCode::NotFound) => {
                        Loaded::Ready(None)
                    }
                    Err(e) => Loaded::Failed(reason(&e)),
                };
                let diff = match diff {
                    Ok(diff) => Loaded::Ready(diff),
                    Err(e) => Loaded::Failed(reason(&e)),
                };
                Ok(RunChanges { workspace, diff })
            },
            cx,
            D {
                ok: Box::new(move |store, read: RunChanges, cx| {
                    store.accept_changes(ok_key.clone(), read, cx);
                }),
                err: Box::new(move |store, cx| {
                    store.reading_changes.remove(&err_key);
                    store.reread_changes.remove(&err_key);
                    if let Some(c) = store.changes.get_mut(&err_key)
                        && c.diff == Loaded::Loading
                    {
                        c.diff = Loaded::Failed("the daemon is not connected".to_owned());
                    }
                    cx.notify();
                }),
                quiet: true,
            },
        );
        cx.notify();
    }

    /// Takes what a read of the changes found. When the diff moved on (its checksum, or the list
    /// where the daemon sends none), the patches read for the old one stay on screen and are
    /// marked to be read again, so Review does not blank and the scroll position holds.
    pub(crate) fn accept_changes(
        &mut self,
        key: (RunId, ReviewScope),
        read: RunChanges,
        cx: &mut Context<Self>,
    ) {
        self.reading_changes.remove(&key);
        if let Some(old) = self.changes.get(&key)
            && old.diff != read.diff
            && !matches!(old.diff, Loaded::Loading)
        {
            let stale: Vec<_> = self
                .patches
                .keys()
                .filter(|(run, s, _)| run == &key.0 && *s == key.1)
                .cloned()
                .collect();
            self.stale_patches.extend(stale);
        }
        self.changes.insert(key.clone(), read);
        if self.reread_changes.remove(&key) {
            self.load_changes_in(&key.0, key.1, cx);
        }
        cx.notify();
    }

    /// Forgets a failed read of a patch, so the next [`Self::load_patch_in`] asks again.
    pub(crate) fn forget_failed_patch(&mut self, run: &RunId, scope: ReviewScope, path: &str) {
        let key = (run.clone(), scope, path.to_owned());
        if matches!(self.patches.get(&key), Some(Loaded::Failed(_))) {
            self.patches.remove(&key);
        }
    }

    /// Reads the patch of `path` in `run` over `scope` unless it was read, failed or is on its way.
    pub(crate) fn load_patch_in(
        &mut self,
        run: &RunId,
        scope: ReviewScope,
        path: &str,
        cx: &mut Context<Self>,
    ) {
        if !self.supports_scope(scope) {
            return;
        }
        let key = (run.clone(), scope, path.to_owned());
        // A patch that went stale is read again with the old one still showing.
        let refetch = self.stale_patches.remove(&key);
        if self.patches.contains_key(&key) && !refetch {
            return;
        }
        if !refetch {
            self.patches.insert(key.clone(), Loaded::Loading);
        }
        #[cfg(test)]
        {
            self.patch_reads += 1;
        }
        let (id, file) = (run.clone(), path.to_owned());
        let (ok_key, err_key) = (key.clone(), key);
        self.call(
            "Could not read the patch",
            move |client| async move {
                let read = match scope.diff_scope() {
                    None => client.workspace_diff(id, Some(file)).await,
                    Some(diff_scope) => {
                        client
                            .workspace_diff_scoped(id, Some(file), diff_scope)
                            .await
                    }
                };
                Ok(match read {
                    Ok(d) => Loaded::Ready(Patch {
                        text: d.patch,
                        truncated: d.patch_truncated,
                        checksum: d.checksum,
                    }),
                    Err(e) => Loaded::Failed(reason(&e)),
                })
            },
            cx,
            D {
                ok: Box::new(move |store, patch, cx| {
                    store.patches.insert(ok_key, patch);
                    cx.notify();
                }),
                err: Box::new(move |store, cx| {
                    store.patches.insert(
                        err_key,
                        Loaded::Failed("the daemon is not connected".to_owned()),
                    );
                    cx.notify();
                }),
                quiet: true,
            },
        );
        cx.notify();
    }

    /// Ends the wait before the next connection attempt.
    pub(crate) fn retry_now(&mut self) {
        if let Some(mirror) = &self.mirror {
            mirror.retry_now();
        }
    }

    /// Asks for a fresh `daemon.status` (uptime and run counts have no watch of their own).
    pub(crate) fn refresh_status(&mut self) {
        if let Some(mirror) = &self.mirror {
            mirror.refresh_status();
        }
    }

    /// What has no watch is fetched again when the connection is back.
    fn refetch_after_reconnect(&mut self, cx: &mut Context<Self>) {
        self.fetching_runs.clear();
        self.fetching_defs.clear();
        self.paging = false;
        if matches!(self.state.feed(Feed::ApprovalHistory), FeedState::Failed(_)) {
            self.load_history(cx);
        }
        self.ensure_def_records(cx);
        if self.registry_wanted {
            self.registry.fetching = false;
            self.load_registry(false, cx);
        }
        if self.rules_wanted || self.rules.is_some() {
            self.load_rules(cx);
        }
        if let Some(run) = self.open.clone() {
            // The follow resumes by itself; the summary of a run outside the window needs `run.get`.
            self.ensure_run(&run, cx);
        }
    }

    /// Shows `run`: starts following its transcript from where the state has it. The mirror's
    /// tasks live on tokio, so the call enters the runtime first. A run already followed is left
    /// alone.
    pub(crate) fn open_run(&mut self, run: RunId, cx: &mut Context<Self>) {
        self.open = Some(run.clone());
        let from = self.state.transcript(&run).map(Transcript::resume_from);
        if let Some(mirror) = &self.mirror {
            let _entered = Tokio::handle(cx).enter();
            mirror.follow_run(run.clone(), from);
        }
        self.ensure_run(&run, cx);
        self.follow_thread_siblings(&run, cx);
        if let Some((bot, version)) = vm::runs::bot_to_fetch(&self.state, &run) {
            self.ensure_bot(&bot, version, cx);
        }
        cx.notify();
    }

    /// The thread of `run`: what `thread.list` says, else what `task.run` answered.
    pub(crate) fn thread_of(&self, run: &RunId) -> Option<ThreadId> {
        self.state
            .thread_of(run)
            .or_else(|| self.thread_hints.get(run))
            .cloned()
    }

    /// The runs of `run`'s thread, oldest first, `run` among them (just `run` for a run with no
    /// known thread).
    pub(crate) fn thread_run_ids(&self, run: &RunId) -> Vec<RunId> {
        self.state
            .thread_of(run)
            .and_then(|thread| self.state.thread(thread))
            .map(|thread| thread.runs.iter().map(|r| r.run_id.clone()).collect())
            .unwrap_or_else(|| vec![run.clone()])
    }

    /// Follows the transcripts of the other runs of the open run's thread, each from where the
    /// state has it, through the same machinery as the open run. A run followed is left alone.
    fn follow_thread_siblings(&mut self, run: &RunId, cx: &mut Context<Self>) {
        for sibling in self.thread_run_ids(run) {
            if &sibling == run {
                continue;
            }
            let from = self.state.transcript(&sibling).map(Transcript::resume_from);
            if let Some(mirror) = &self.mirror {
                let _entered = Tokio::handle(cx).enter();
                mirror.follow_run(sibling.clone(), from);
            }
            self.ensure_run(&sibling, cx);
        }
    }

    /// `thread.close` (never forced): the worktree is kept when it holds unmerged work, and the
    /// answer says so. The sidebar follows through a fresh `thread.list`.
    pub(crate) fn close_thread(&mut self, thread: ThreadId, cx: &mut Context<Self>) {
        if !self.closing.insert(thread.clone()) {
            return;
        }
        let (id, ok_id, err_id) = (thread.clone(), thread.clone(), thread);
        self.call(
            "Could not close the thread",
            move |client| async move { client.close_thread(id, false).await },
            cx,
            D {
                ok: Box::new(move |store, result: reno_proto::ThreadCloseResult, cx| {
                    store.closing.remove(&ok_id);
                    store.closes.insert(ok_id.clone(), result);
                    if let Some(mirror) = &store.mirror {
                        mirror.refresh_threads();
                    }
                    cx.emit(StoreEvent::ThreadClosed);
                    cx.notify();
                }),
                err: Box::new(move |store, cx| {
                    store.closing.remove(&err_id);
                    cx.notify();
                }),
                quiet: false,
            },
        );
        cx.notify();
    }

    /// What `thread.close` answered for `thread` in this session.
    pub(crate) fn close_result(&self, thread: &ThreadId) -> Option<&reno_proto::ThreadCloseResult> {
        self.closes.get(thread)
    }

    pub(crate) fn is_closing(&self, thread: &ThreadId) -> bool {
        self.closing.contains(thread)
    }

    /// Starts following the transcript of every run waiting on a question, so Home and the run can show
    /// the question itself. Only waiting runs are loaded (not every run), each followed once
    /// from where its transcript stands; the follow ends when the run does.
    fn follow_waiting_questions(&mut self, cx: &mut Context<Self>) {
        let Some(mirror) = &self.mirror else {
            return;
        };
        let _entered = Tokio::handle(cx).enter();
        for run in summary::waiting_questions(&self.state) {
            let from = self
                .state
                .transcript(&run.run_id)
                .map(Transcript::resume_from);
            mirror.follow_run(run.run_id.clone(), from);
        }
    }

    /// All runs is showing (or has stopped): while it shows, every live run's transcript is
    /// followed, so each lane can say what its run is doing now. Followed once on entry and as
    /// runs start; a follow ends with its run, and nothing is followed while the page is away.
    pub(crate) fn set_follow_live(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.follow_live == on {
            return;
        }
        self.follow_live = on;
        if on {
            self.follow_live_runs(cx);
        }
    }

    /// Follows the transcript of every live run. A run followed is left alone.
    fn follow_live_runs(&mut self, cx: &mut Context<Self>) {
        let Some(mirror) = &self.mirror else {
            return;
        };
        let _entered = Tokio::handle(cx).enter();
        for run in vm::shell::live_runs(&self.state) {
            let from = self
                .state
                .transcript(&run.run_id)
                .map(Transcript::resume_from);
            mirror.follow_run(run.run_id.clone(), from);
        }
    }

    /// Runs one request on tokio. `done` runs on the foreground with the answer; a failure is
    /// reported as `StoreEvent::Failed("{what}: {reason}")`.
    fn call<T, F, Fut>(
        &mut self,
        what: &'static str,
        request: F,
        cx: &mut Context<Self>,
        done: D<T>,
    ) where
        T: Send + 'static,
        F: FnOnce(Client) -> Fut,
        Fut: Future<Output = Result<T, ClientError>> + Send + 'static,
    {
        let Some(client) = self.client() else {
            Self::fail(&done, what, "the daemon is not connected", cx);
            (done.err)(self, cx);
            return;
        };
        let call = Tokio::spawn(cx, request(client));
        cx.spawn(async move |this, cx| {
            let result = flatten(call.await);
            this.update(cx, |store, cx| match result {
                Ok(value) => (done.ok)(store, value, cx),
                Err(reason) => {
                    store.last_failure = Some(reason.clone());
                    (done.err)(store, cx);
                    if !done.quiet {
                        cx.emit(StoreEvent::Failed(format!("{what}: {reason}")));
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    fn fail<T>(done: &D<T>, what: &str, reason: &str, cx: &mut Context<Self>) {
        if !done.quiet {
            cx.emit(StoreEvent::Failed(format!("{what}: {reason}")));
        }
    }

    /// `run.steer`. A failure is reported as a store event, with the ticket the sender can use
    /// to take its draft back.
    pub(crate) fn steer(
        &mut self,
        run: RunId,
        prompt: String,
        ticket: u64,
        cx: &mut Context<Self>,
    ) {
        self.sending += 1;
        self.call(
            "Could not steer the run",
            move |client| async move { client.steer(&run, prompt).await },
            cx,
            D {
                ok: Box::new(|store, (), cx| {
                    store.sending = store.sending.saturating_sub(1);
                    cx.notify();
                }),
                err: Box::new(move |store, cx| {
                    store.sending = store.sending.saturating_sub(1);
                    cx.emit(StoreEvent::SendFailed(ticket));
                }),
                quiet: false,
            },
        );
        cx.notify();
    }

    /// `run.cancel`: Stop. The run's new state arrives through `runs.watch`.
    pub(crate) fn cancel_run(&mut self, run: RunId, cx: &mut Context<Self>) {
        self.call(
            "Could not stop the run",
            move |client| async move {
                client
                    .cancel_run(&run, Some("stopped from the app".to_owned()))
                    .await
                    .map(|_| ())
            },
            cx,
            D::fire_and_forget(),
        );
    }

    /// Answers a held action. The card leaves when the daemon reports the new list.
    pub(crate) fn decide(
        &mut self,
        id: ApprovalId,
        decision: ApprovalDecision,
        cx: &mut Context<Self>,
    ) {
        self.decide_with_message(id, decision, None, cx);
    }

    /// [`Store::decide`] with the reason for a denial, which reaches the harness.
    pub(crate) fn decide_with_message(
        &mut self,
        id: ApprovalId,
        decision: ApprovalDecision,
        message: Option<String>,
        cx: &mut Context<Self>,
    ) {
        self.decide_remembering(id, decision, message, None, cx);
    }

    /// [`Store::decide_with_message`] that also keeps the answer as a standing rule when
    /// `remember` is given. The rule list is read again once the daemon has taken the answer.
    pub(crate) fn decide_remembering(
        &mut self,
        id: ApprovalId,
        decision: ApprovalDecision,
        message: Option<String>,
        remember: Option<reno_proto::Remember>,
        cx: &mut Context<Self>,
    ) {
        if self.client().is_none() || !self.deciding.insert(id.clone()) {
            return;
        }
        cx.notify();
        let ok_id = id.clone();
        let err_id = id.clone();
        let remembered = remember.is_some();
        self.call(
            "Could not send the decision",
            move |client| async move {
                client
                    .decide_remembering(&id, decision, message, remember)
                    .await
            },
            cx,
            D {
                ok: Box::new(move |store, (), cx| {
                    store.deciding.remove(&ok_id);
                    if remembered {
                        store.load_rules(cx);
                    }
                    cx.notify();
                }),
                err: Box::new(move |store, cx| {
                    store.deciding.remove(&err_id);
                    cx.notify();
                }),
                quiet: false,
            },
        );
    }

    /// `executor.refresh`: probes one executor (`None`: all) again. The answer is applied at
    /// once; `executors.watch` says the same.
    pub(crate) fn refresh_executors(&mut self, id: Option<ExecutorId>, cx: &mut Context<Self>) {
        if self.client().is_none() || !self.checking.insert(id.clone()) {
            return;
        }
        cx.notify();
        let (ok_id, err_id) = (id.clone(), id.clone());
        self.call(
            "Could not check the executor",
            move |client| async move { client.refresh_executors(id).await },
            cx,
            D {
                ok: Box::new(move |store, list, cx| {
                    store.checking.remove(&ok_id);
                    store.apply(Update::Executors(list), cx);
                    cx.notify();
                }),
                err: Box::new(move |store, cx| {
                    store.checking.remove(&err_id);
                    cx.notify();
                }),
                quiet: false,
            },
        );
    }

    /// `executor.refresh` for usage only: reads the account's plan meters again. The daemon
    /// reads at most once per 30 s, so a person looking at the picker may call this freely;
    /// nothing calls it on a timer (ARCH-10). A failure is silent: the meters stay as they were.
    pub(crate) fn refresh_usage(&mut self, id: ExecutorId, cx: &mut Context<Self>) {
        if self.client().is_none() || !self.state.supports(reno_proto::FEATURE_EXECUTOR_USAGE) {
            return;
        }
        self.call(
            "Could not read the plan usage",
            move |client| async move { client.refresh_executor_usage(Some(id)).await },
            cx,
            D {
                ok: Box::new(|store, list, cx| {
                    store.apply(Update::Executors(list), cx);
                    cx.notify();
                }),
                err: Box::new(|_, cx| cx.notify()),
                quiet: true,
            },
        );
    }

    /// Whether the daemon has the ACP registry (feature `executor.registry`).
    pub(crate) fn supports_registry(&self) -> bool {
        self.state.supports(reno_proto::FEATURE_EXECUTOR_REGISTRY)
    }

    /// Whether the daemon can sign an agent in itself (`executor.signin.start`): the agent's own
    /// ACP `authenticate`, in the browser. Until the feature is negotiated nothing offers it.
    pub(crate) fn supports_browser_sign_in(&self) -> bool {
        self.state.supports(reno_proto::FEATURE_EXECUTOR_SIGNIN)
    }

    pub(crate) fn registry(&self) -> &RegistryData {
        &self.registry
    }

    /// Why this agent's last removal was refused, if it was.
    pub(crate) fn registry_refusal(&self, id: &str) -> Option<&str> {
        self.registry_refused.get(id).map(String::as_str)
    }

    /// Forgets a refusal once the person has seen it and cancelled.
    pub(crate) fn dismiss_registry_refusal(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.registry_refused.remove(id).is_some() {
            cx.notify();
        }
    }

    pub(crate) fn is_removing(&self, id: &str) -> bool {
        self.registry_removing.contains(id)
    }

    /// `executor.registry.list`: reads the index (`refresh` skips the daemon's cache). One at a
    /// time; the last good answer stays on screen while the next comes and when it fails. Quiet:
    /// the group says offline or failed itself, so no toast. Does nothing for a daemon without
    /// `executor.registry`.
    pub(crate) fn load_registry(&mut self, refresh: bool, cx: &mut Context<Self>) {
        if !self.supports_registry() || self.client().is_none() || self.registry.fetching {
            return;
        }
        self.registry.fetching = true;
        cx.notify();
        self.call(
            "Could not read the registry",
            move |client| async move { client.registry_list(refresh).await },
            cx,
            D {
                ok: Box::new(|store, list, cx| {
                    store.registry = RegistryData {
                        listing: Some(list),
                        failure: None,
                        fetching: false,
                    };
                    cx.notify();
                }),
                err: Box::new(|store, cx| {
                    store.registry.fetching = false;
                    store.registry.failure = Some(
                        store
                            .last_failure
                            .clone()
                            .unwrap_or_else(|| "could not read the registry".to_owned()),
                    );
                    cx.notify();
                }),
                quiet: true,
            },
        );
    }

    /// The Executors screen opened: reads the list once if it has none.
    pub(crate) fn ensure_registry(&mut self, cx: &mut Context<Self>) {
        self.registry_wanted = true;
        if self.registry.listing.is_none() {
            self.load_registry(false, cx);
        }
    }

    /// `executor.registry.add`: adds agent `id`, or updates it, as an install job. The progress
    /// is the executor's job in `executors.watch`.
    pub(crate) fn registry_add(&mut self, id: &str, cx: &mut Context<Self>) {
        let Ok(executor) = format!("acp.{id}").parse::<ExecutorId>() else {
            return;
        };
        let id = id.to_owned();
        self.job_call(
            executor,
            "Could not add the agent",
            move |client| async move { client.registry_add(&id).await },
            cx,
        );
    }

    /// `executor.registry.remove`: deletes an added agent's install and keys. A refusal (a run
    /// is queued or live) is put on the agent's line, not in a toast.
    pub(crate) fn registry_remove(&mut self, id: &str, cx: &mut Context<Self>) {
        if self.client().is_none() || !self.registry_removing.insert(id.to_owned()) {
            return;
        }
        self.registry_refused.remove(id);
        cx.notify();
        let (ok_id, err_id, request) = (id.to_owned(), id.to_owned(), id.to_owned());
        self.call(
            "Could not remove the agent",
            move |client| async move { client.registry_remove(&request).await },
            cx,
            D {
                ok: Box::new(move |store, _, cx| {
                    store.registry_removing.remove(&ok_id);
                    store.load_registry(false, cx);
                    cx.notify();
                }),
                err: Box::new(move |store, cx| {
                    store.registry_removing.remove(&err_id);
                    let reason = store.last_failure.clone().unwrap_or_default();
                    store.registry_refused.insert(err_id, reason);
                    cx.notify();
                }),
                quiet: true,
            },
        );
    }

    /// A registry request whose answer the caller owns (the env editor, the own-agent form):
    /// the caller shows the failure where the person is looking, never in a toast, so a value
    /// typed in cannot reach one. Dropping the task cancels the wait.
    pub(crate) fn registry_task<T, Fut>(
        &self,
        request: impl FnOnce(Client) -> Fut,
        cx: &gpui::App,
    ) -> Task<Result<T, String>>
    where
        T: Send + 'static,
        Fut: Future<Output = Result<T, ClientError>> + Send + 'static,
    {
        let Some(client) = self.client() else {
            return Task::ready(Err("the daemon is not connected".to_owned()));
        };
        if !self.supports_registry() {
            return Task::ready(Err("this daemon has no ACP registry".to_owned()));
        }
        let call = Tokio::spawn(cx, request(client));
        cx.background_spawn(async move { flatten(call.await) })
    }

    /// `executor.install`: fetches the executor's adapter. It waits for the daemon, which can
    /// take minutes; the new health arrives through `executors.watch`.
    pub(crate) fn install_executor(&mut self, id: ExecutorId, cx: &mut Context<Self>) {
        if self.client().is_none() || !self.installing.insert(id.clone()) {
            return;
        }
        cx.notify();
        let (ok_id, err_id, request_id) = (id.clone(), id.clone(), id);
        self.call(
            "Could not install the executor",
            move |client| async move { client.install_executor(&request_id).await },
            cx,
            D {
                ok: Box::new(move |store, _, cx| {
                    store.installing.remove(&ok_id);
                    cx.emit(StoreEvent::ExecutorInstalled(ok_id.to_string()));
                    cx.notify();
                }),
                err: Box::new(move |store, cx| {
                    store.installing.remove(&err_id);
                    cx.notify();
                }),
                quiet: false,
            },
        );
    }

    /// Runs one executor job request. The answer is not applied: `executors.watch` carries every
    /// change in order, and an older answer must never overwrite a newer snapshot.
    fn job_call<Fut>(
        &mut self,
        id: ExecutorId,
        what: &'static str,
        request: impl FnOnce(Client) -> Fut,
        cx: &mut Context<Self>,
    ) where
        Fut: Future<Output = Result<ExecutorInfo, ClientError>> + Send + 'static,
    {
        if self.client().is_none() || !self.starting.insert(id.clone()) {
            return;
        }
        cx.notify();
        let (ok_id, err_id) = (id.clone(), id);
        self.call(
            what,
            request,
            cx,
            D {
                ok: Box::new(move |store, _, cx| {
                    store.starting.remove(&ok_id);
                    cx.notify();
                }),
                err: Box::new(move |store, cx| {
                    store.starting.remove(&err_id);
                    cx.notify();
                }),
                quiet: false,
            },
        );
    }

    /// `executor.job.start`: installs or updates `id` as a job. An update of an executor with
    /// runs in flight is queued unless `force`.
    pub(crate) fn start_executor_job(
        &mut self,
        id: ExecutorId,
        kind: JobKind,
        force: bool,
        cx: &mut Context<Self>,
    ) {
        let request = id.clone();
        self.job_call(
            id,
            match kind {
                JobKind::Install => "Could not start the install",
                JobKind::Update => "Could not start the update",
            },
            move |client| async move { client.start_executor_job(&request, kind, force).await },
            cx,
        );
    }

    /// "Update now anyway": the queued update is stopped and the update starts at once, as one
    /// request, because the daemon allows one job per executor.
    pub(crate) fn start_update_now(&mut self, id: ExecutorId, cx: &mut Context<Self>) {
        let request = id.clone();
        self.job_call(
            id,
            "Could not start the update",
            move |client| async move {
                client.cancel_executor_job(&request).await?;
                client
                    .start_executor_job(&request, JobKind::Update, true)
                    .await
            },
            cx,
        );
    }

    /// `executor.job.cancel`.
    pub(crate) fn cancel_executor_job(&mut self, id: ExecutorId, cx: &mut Context<Self>) {
        let request = id.clone();
        self.job_call(
            id,
            "Could not stop the job",
            move |client| async move { client.cancel_executor_job(&request).await },
            cx,
        );
    }

    /// `executor.job.dismiss`: clears a failed job.
    pub(crate) fn dismiss_executor_job(&mut self, id: ExecutorId, cx: &mut Context<Self>) {
        self.job_logs.remove(&id);
        let request = id.clone();
        self.job_call(
            id,
            "Could not clear the failed job",
            move |client| async move { client.dismiss_executor_job(&request).await },
            cx,
        );
    }

    /// `executor.auto_update.set`.
    pub(crate) fn set_executor_auto_update(
        &mut self,
        id: ExecutorId,
        on: bool,
        verified_only: bool,
        cx: &mut Context<Self>,
    ) {
        let request = id.clone();
        self.job_call(
            id,
            "Could not change auto-update",
            move |client| async move {
                client
                    .set_executor_auto_update(&request, on, verified_only)
                    .await
            },
            cx,
        );
    }

    /// Shows or hides the output of this executor's job. Shown, it is fetched now and again
    /// whenever the job changes; hidden, nothing is kept up to date.
    pub(crate) fn show_job_log(&mut self, id: ExecutorId, show: bool, cx: &mut Context<Self>) {
        if show {
            self.log_open.insert(id.clone());
            self.fetch_job_log(id, cx);
        } else {
            self.log_open.remove(&id);
            self.job_logs.remove(&id);
            cx.notify();
        }
    }

    /// `executor.job.log`, at most one at a time per executor.
    fn fetch_job_log(&mut self, id: ExecutorId, cx: &mut Context<Self>) {
        if self.client().is_none() || !self.log_fetching.insert(id.clone()) {
            return;
        }
        let (ok_id, err_id, request) = (id.clone(), id.clone(), id);
        self.call(
            "Could not read the log",
            move |client| async move { client.executor_job_log(&request).await },
            cx,
            D {
                ok: Box::new(move |store, log: reno_proto::ExecutorJobLogResult, cx| {
                    store.log_fetching.remove(&ok_id);
                    if store.log_open.contains(&ok_id) {
                        store.job_logs.insert(
                            ok_id,
                            JobLog {
                                lines: log.lines,
                                truncated: log.truncated,
                            },
                        );
                    }
                    cx.notify();
                }),
                err: Box::new(move |store, cx| {
                    store.log_fetching.remove(&err_id);
                    cx.notify();
                }),
                quiet: true,
            },
        );
    }

    /// Probes `id` again after the person came back from a sign-in in Terminal, and says so when
    /// it is still signed out.
    pub(crate) fn recheck_after_sign_in(&mut self, id: ExecutorId, cx: &mut Context<Self>) {
        if self.client().is_none() || !self.checking.insert(Some(id.clone())) {
            return;
        }
        cx.notify();
        let (ok_id, err_id, request) = (id.clone(), id.clone(), Some(id));
        self.call(
            "Could not check the executor",
            move |client| async move { client.refresh_executors(request).await },
            cx,
            D {
                ok: Box::new(move |store, list: Vec<ExecutorInfo>, cx| {
                    store.checking.remove(&Some(ok_id.clone()));
                    let still_out = list.iter().any(|e| {
                        e.descriptor.id == ok_id
                            && e.health
                                .as_ref()
                                .is_some_and(|h| matches!(h.signed_in, SignedIn::No { .. }))
                    });
                    // A first install that failed, run in Terminal, and now found: the failed
                    // job is stale. Clear it.
                    let healed = list.iter().any(|e| {
                        e.descriptor.id == ok_id
                            && e.job.as_ref().is_some_and(|j| {
                                j.kind == JobKind::Install
                                    && matches!(j.phase, JobPhase::Failed { .. })
                            })
                            && e.health.as_ref().is_some_and(|h| {
                                matches!(h.installed, reno_proto::InstallStatus::Installed { .. })
                            })
                    });
                    store.apply(Update::Executors(list), cx);
                    if healed {
                        store.dismiss_executor_job(ok_id.clone(), cx);
                    }
                    if still_out {
                        cx.emit(StoreEvent::StillSignedOut(summary::executor_name(
                            ok_id.as_str(),
                        )));
                    }
                    cx.notify();
                }),
                err: Box::new(move |store, cx| {
                    store.checking.remove(&Some(err_id));
                    cx.notify();
                }),
                quiet: false,
            },
        );
    }

    /// `task.run` on the given executor id.
    #[cfg(test)]
    pub(crate) fn run_task(&mut self, prompt: String, executor_id: &str, cx: &mut Context<Self>) {
        let executor = match executor_id.parse() {
            Ok(executor) => executor,
            Err(e) => {
                cx.emit(StoreEvent::Failed(format!(
                    "Bad executor {executor_id}: {e}"
                )));
                return;
            }
        };
        let params = TaskRunParams {
            prompt,
            executor: Some(executor),
            ..TaskRunParams::default()
        };
        self.submit_task(params, 0, cx);
    }

    /// `task.run`. The run opens on [`StoreEvent::RunStarted`]; a failure is a
    /// [`StoreEvent::Failed`] and then [`StoreEvent::SendFailed`] with `ticket`.
    pub(crate) fn submit_task(
        &mut self,
        params: TaskRunParams,
        ticket: u64,
        cx: &mut Context<Self>,
    ) {
        self.sending += 1;
        self.call(
            "Could not start the task",
            move |client| async move { client.submit_task(params).await },
            cx,
            D {
                ok: Box::new(|store, started: reno_proto::TaskRunResult, cx| {
                    store.sending = store.sending.saturating_sub(1);
                    if let Some(decision) = started.routing {
                        store.routings.insert(
                            started.run_id.clone(),
                            RoutingEntry {
                                key: None,
                                routing: RunRouting::Ready(Some(decision)),
                            },
                        );
                    }
                    if let Some(thread) = started.thread_id {
                        store.thread_hints.insert(started.run_id.clone(), thread);
                    }
                    cx.emit(StoreEvent::RunStarted(started.run_id));
                }),
                err: Box::new(move |store, cx| {
                    store.sending = store.sending.saturating_sub(1);
                    let why = store.last_failure.take().unwrap_or_default();
                    cx.emit(StoreEvent::SendFailed(ticket));
                    cx.emit(StoreEvent::SendRefused { ticket, why });
                }),
                quiet: false,
            },
        );
        cx.notify();
    }

    /// `trigger.fire`: fires a trigger now. The new state arrives through `triggers.watch`.
    pub(crate) fn fire_trigger(&mut self, name: String, cx: &mut Context<Self>) {
        if self.client().is_none() || !self.firing.insert(name.clone()) {
            return;
        }
        cx.notify();
        let (ok_name, err_name, event_name) = (name.clone(), name.clone(), name.clone());
        self.call(
            "Could not fire the trigger",
            move |client| async move { client.fire_trigger(&name).await },
            cx,
            D {
                ok: Box::new(move |store, fired: reno_proto::TriggerFireResult, cx| {
                    store.firing.remove(&ok_name);
                    cx.emit(StoreEvent::TriggerFired {
                        name: event_name,
                        outcome: fired.outcome,
                        run: fired.run_id,
                    });
                    cx.notify();
                }),
                err: Box::new(move |store, cx| {
                    store.firing.remove(&err_name);
                    cx.notify();
                }),
                quiet: false,
            },
        );
    }

    /// `run.list`: the first page of finished runs, or the next one. One request at a time.
    pub(crate) fn load_finished_page(&mut self, cx: &mut Context<Self>) {
        let Some(params) = vm::finished::next_page(&self.state, self.runs_page) else {
            return;
        };
        let filter = vm::finished::STATUSES.to_vec();
        if self.client().is_none() || self.paging {
            return;
        }
        self.paging = true;
        cx.notify();
        self.call(
            "Could not load runs",
            move |client| async move { client.list_runs_page(params).await },
            cx,
            D {
                ok: Box::new(move |store, page: reno_proto::RunListResult, cx| {
                    store.paging = false;
                    store.apply(
                        Update::RunsPage {
                            filter,
                            runs: page.runs,
                            next: page.next,
                        },
                        cx,
                    );
                    cx.notify();
                }),
                err: Box::new(|store, cx| {
                    store.paging = false;
                    cx.notify();
                }),
                quiet: false,
            },
        );
    }

    /// `approval.history`, when a screen opens or a decision lands.
    pub(crate) fn load_history(&mut self, cx: &mut Context<Self>) {
        if !matches!(
            self.state.feed(Feed::ApprovalHistory),
            FeedState::Loading | FeedState::Live | FeedState::Failed(_)
        ) {
            return;
        }
        self.call(
            "Could not load the approval history",
            move |client| async move { client.approval_history(None, Some(50)).await },
            cx,
            D {
                ok: Box::new(|store, records: Vec<reno_proto::ApprovalRecord>, cx| {
                    // A decision's run may be older than the runs the window holds: fetch it, so
                    // the Project page can place the decision in its project.
                    let runs: Vec<RunId> = records.iter().map(|r| r.run_id.clone()).collect();
                    store.apply(Update::ApprovalHistory(records), cx);
                    for run in &runs {
                        store.ensure_run(run, cx);
                    }
                }),
                err: Box::new(|store, cx| {
                    store.apply(
                        Update::Feed(
                            Feed::ApprovalHistory,
                            FeedState::Failed("the request failed".to_owned()),
                        ),
                        cx,
                    );
                }),
                quiet: true,
            },
        );
    }

    /// The standing rules as `rule.list` last said (`None` before it has).
    pub(crate) fn rules(&self) -> Option<&Result<Vec<reno_proto::StandingRule>, String>> {
        self.rules.as_ref()
    }

    /// Whether a revoke of this rule is in flight.
    pub(crate) fn is_revoking(&self, id: &reno_proto::RuleId) -> bool {
        self.revoking.contains(id)
    }

    /// `rule.list`: the rules in force. A daemon without them leaves the list unread.
    pub(crate) fn load_rules(&mut self, cx: &mut Context<Self>) {
        self.rules_wanted = true;
        if !self.state.supports(reno_proto::FEATURE_RULES) {
            return;
        }
        self.call(
            "Could not load the standing rules",
            move |client| async move { client.list_rules(false).await },
            cx,
            D {
                ok: Box::new(|store, rules, cx| {
                    store.rules = Some(Ok(rules));
                    cx.notify();
                }),
                err: Box::new(|store, cx| {
                    if store.rules.is_none() {
                        store.rules = Some(Err("the request failed".to_owned()));
                    }
                    cx.notify();
                }),
                quiet: true,
            },
        );
    }

    /// `rule.add`: `command` runs without asking in the project at `root`; then the list is read
    /// again.
    pub(crate) fn add_project_rule(
        &mut self,
        root: PathBuf,
        command: String,
        cx: &mut Context<Self>,
    ) {
        if !self.state.supports(reno_proto::FEATURE_PROJECT_RULES) {
            return;
        }
        self.call(
            "Could not add the command",
            move |client| async move {
                client
                    .add_rule(reno_proto::RuleAddParams {
                        project: root,
                        effect: reno_proto::StandingEffect::Allow,
                        command,
                        tool: None,
                        expires_in_secs: None,
                    })
                    .await
            },
            cx,
            D {
                ok: Box::new(|store, _rule, cx| {
                    store.load_rules(cx);
                    cx.notify();
                }),
                err: Box::new(|_, cx| cx.notify()),
                quiet: false,
            },
        );
    }

    /// When the last effort and option changes of `run` take hold, from the daemon's answers.
    pub(crate) fn config_applied(&self, run: &RunId) -> ConfigApplied {
        self.configured.get(run).copied().unwrap_or_default()
    }

    /// `run.configure`: changes a run's effort and options. The daemon says when each takes
    /// hold; the run's summary brings the new values. A refusal is a toast.
    pub(crate) fn configure_run(
        &mut self,
        run: RunId,
        effort: Option<reno_proto::EffortChange>,
        options: BTreeMap<String, String>,
        cx: &mut Context<Self>,
    ) {
        if !self.state.supports(reno_proto::FEATURE_RUN_CONFIGURE) {
            return;
        }
        let key = run.clone();
        self.call(
            "Could not change the run",
            move |client| async move { client.configure_run(&run, effort, options).await },
            cx,
            D {
                ok: Box::new(move |store, result: reno_proto::RunConfigureResult, cx| {
                    let applied = store.configured.entry(key.clone()).or_default();
                    applied.effort = result.effort_applies.or(applied.effort);
                    applied.options = result.options_apply.or(applied.options);
                    cx.notify();
                }),
                err: Box::new(|_, cx| cx.notify()),
                quiet: false,
            },
        );
    }

    /// `run.set_model`: the live run uses `model` from its next model call. The run's summary
    /// and a journalled `ModelChanged` bring the change; a refusal is a toast.
    pub(crate) fn set_run_model(&mut self, run: RunId, model: String, cx: &mut Context<Self>) {
        if !self.state.supports(reno_proto::FEATURE_RUN_MODEL) {
            return;
        }
        self.call(
            "Could not change the model",
            move |client| async move { client.set_run_model(&run, model).await },
            cx,
            D {
                ok: Box::new(|_, _result: reno_proto::RunSetModelResult, cx| cx.notify()),
                err: Box::new(|_, cx| cx.notify()),
                quiet: false,
            },
        );
    }

    /// `rule.revoke`, then the list is read again.
    pub(crate) fn revoke_rule(&mut self, id: reno_proto::RuleId, cx: &mut Context<Self>) {
        if self.client().is_none() || !self.revoking.insert(id.clone()) {
            return;
        }
        cx.notify();
        let (ok_id, err_id) = (id.clone(), id.clone());
        self.call(
            "Could not revoke the rule",
            move |client| async move { client.revoke_rule(&id).await },
            cx,
            D {
                ok: Box::new(move |store, (), cx| {
                    store.revoking.remove(&ok_id);
                    store.load_rules(cx);
                    cx.notify();
                }),
                err: Box::new(move |store, cx| {
                    store.revoking.remove(&err_id);
                    cx.notify();
                }),
                quiet: false,
            },
        );
    }

    /// `run.get` for a run the window does not hold (opened from a trigger or Home).
    pub(crate) fn ensure_run(&mut self, run: &RunId, cx: &mut Context<Self>) {
        if self.state.run(run).is_some() || !self.fetching_runs.insert(run.clone()) {
            return;
        }
        let id = run.clone();
        let (ok_id, err_id) = (run.clone(), run.clone());
        self.call(
            "Could not load the run",
            move |client| async move { client.get_run(&id).await },
            cx,
            D {
                ok: Box::new(move |store, summary, cx| {
                    store.fetching_runs.remove(&ok_id);
                    store.apply(Update::RunUpserted(summary), cx);
                }),
                err: Box::new(move |store, _| {
                    store.fetching_runs.remove(&err_id);
                }),
                quiet: true,
            },
        );
    }

    /// `def.get` for every definition in force whose body is not known yet.
    pub(crate) fn ensure_def_records(&mut self, cx: &mut Context<Self>) {
        let missing: Vec<(DefKind, String, u32)> = self
            .state
            .defs()
            .defs
            .iter()
            .filter(|d| self.state.def_record(d.kind, &d.name, d.version).is_none())
            .map(|d| (d.kind, d.name.clone(), d.version))
            .filter(|key| !self.fetching_defs.contains(key))
            .collect();
        for key in missing {
            self.fetch_def(key, cx);
        }
    }

    /// `def.get` for one bot's current version, for the access mode on a run.
    pub(crate) fn ensure_bot(&mut self, name: &str, version: u32, cx: &mut Context<Self>) {
        if self.state.def_record(DefKind::Bot, name, version).is_some() {
            return;
        }
        self.fetch_def((DefKind::Bot, name.to_owned(), version), cx);
    }

    fn fetch_def(&mut self, key: (DefKind, String, u32), cx: &mut Context<Self>) {
        if !self.fetching_defs.insert(key.clone()) {
            return;
        }
        let (kind, name, version) = key.clone();
        let (ok_key, err_key) = (key.clone(), key);
        self.call(
            "Could not read the definition",
            move |client| async move { client.get_def(kind, &name, Some(version)).await },
            cx,
            D {
                ok: Box::new(move |store, record, cx| {
                    store.fetching_defs.remove(&ok_key);
                    store.apply(Update::DefRecord(record), cx);
                }),
                // Left in the set so a failing read is not retried on every notification; a
                // reconnect clears the set.
                err: Box::new(move |_, _| drop(err_key)),
                quiet: true,
            },
        );
    }

    /// `def.validate` of the stored body of a definition: checks it against every rule again.
    pub(crate) fn validate_def(&mut self, kind: DefKind, name: String, cx: &mut Context<Self>) {
        let version = self
            .state
            .defs()
            .defs
            .iter()
            .find(|d| d.kind == kind && d.name == name)
            .map(|d| d.version);
        let Some(version) = version else {
            return;
        };
        if !self.validating.insert((kind, name.clone())) {
            return;
        }
        cx.notify();
        let (ok_name, err_name, call_name) = (name.clone(), name.clone(), name);
        self.call(
            "Could not validate the definition",
            move |client| async move {
                let record = client.get_def(kind, &call_name, Some(version)).await?;
                client.validate_def(kind, &call_name, record.body).await
            },
            cx,
            D {
                ok: Box::new(move |store, result, cx| {
                    store.validating.remove(&(kind, ok_name.clone()));
                    store.apply(
                        Update::Validation {
                            kind,
                            name: ok_name,
                            result,
                        },
                        cx,
                    );
                    cx.notify();
                }),
                err: Box::new(move |store, cx| {
                    store.validating.remove(&(kind, err_name));
                    cx.notify();
                }),
                quiet: false,
            },
        );
    }
}

type OnOk<T> = Box<dyn FnOnce(&mut Store, T, &mut Context<Store>)>;
type OnErr = Box<dyn FnOnce(&mut Store, &mut Context<Store>)>;

/// What to do with the answer of one request.
struct D<T> {
    ok: OnOk<T>,
    err: OnErr,
    /// Do not report a failure as a [`StoreEvent::Failed`].
    quiet: bool,
}

impl<T: 'static> D<T> {
    fn then(ok: impl FnOnce(&mut Store, T, &mut Context<Store>) + 'static) -> Self {
        Self {
            ok: Box::new(ok),
            err: Box::new(|_, _| {}),
            quiet: false,
        }
    }
}

impl D<()> {
    fn fire_and_forget() -> Self {
        Self::then(|_, (), _| {})
    }
}

/// A failed request in the daemon's own words (its JSON-RPC code means nothing to a person).
fn reason(e: &ClientError) -> String {
    match e {
        ClientError::Remote(error) => error.message.clone(),
        e => e.to_string(),
    }
}

fn flatten<T>(result: Result<Result<T, ClientError>, gpui_tokio::JoinError>) -> Result<T, String> {
    match result {
        Ok(Ok(value)) => Ok(value),
        // The daemon's own words; its JSON-RPC code means nothing to a person.
        Ok(Err(ClientError::Remote(error))) => Err(error.message.clone()),
        Ok(Err(e)) => Err(e.to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// The registry agents among `executors`, by id: when this set moves, the registry list is stale.
fn registry_agents(executors: &[ExecutorInfo]) -> BTreeSet<&str> {
    executors
        .iter()
        .filter(|e| e.source == reno_proto::ExecutorSource::Registry)
        .map(|e| e.descriptor.id.as_str())
        .collect()
}

fn system_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The keeper that connects to the daemon at `socket` and starts it when nothing answers: the
/// app starts a daemon when it finds none (see `reno_client::launcher::keeper`), unless
/// `RENO_NO_AUTOSTART` is set. `None` when there is nowhere to keep a daemon's files.
fn daemon_keeper(
    socket: &std::path::Path,
    var: impl Fn(&str) -> Option<String>,
) -> Option<Arc<DaemonKeeper>> {
    let data_dir = reno_client::default_data_dir()?;
    let launcher = Launcher::from_env(data_dir, socket.to_path_buf()).ok()?;
    let disabled = var("RENO_NO_AUTOSTART").is_some_and(|v| !v.is_empty() && v != "0");
    let config = KeeperConfig {
        auto_start: !disabled,
        ..KeeperConfig::default()
    };
    Some(DaemonKeeper::new(socket.to_path_buf(), launcher, config))
}

/// The sentence the offline screens show.
pub(crate) fn offline_reason(offline: &Offline) -> String {
    match offline.kind {
        OfflineKind::NotRunning => "Daemon not running — run `reno daemon start`".to_owned(),
        OfflineKind::Starting => "Starting the daemon…".to_owned(),
        OfflineKind::StartFailed => format!("The daemon did not start: {}", offline.detail),
        OfflineKind::Incompatible => format!(
            "The daemon cannot talk to this app ({}). Update one of them, then restart the daemon",
            offline.detail
        ),
        OfflineKind::Lost => "The daemon connection closed.".to_owned(),
        OfflineKind::Other => offline.detail.clone(),
    }
}

/// The line under the reason: whether the app is trying again.
pub(crate) fn retry_line(state: &State) -> Option<String> {
    let offline = state.offline()?;
    let line = match state.connection() {
        ConnectionStatus::Connecting => "Reconnecting…".to_owned(),
        _ if offline.kind == OfflineKind::Starting => "Waiting for it to answer.".to_owned(),
        _ if offline.attempt <= 1 => "Retrying shortly.".to_owned(),
        _ => format!(
            "Tried {} times. Retrying with a growing pause, at most every 30 s.",
            offline.attempt
        ),
    };
    Some(line)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_daemon_refusal_reads_as_its_message_without_the_code() {
        let refused: Result<Result<(), ClientError>, gpui_tokio::JoinError> = Ok(Err(
            ClientError::Remote(Box::new(reno_proto::ErrorObject::new(
                reno_proto::ErrorCode::Conflict,
                "run run_a has ended",
            ))),
        ));
        assert_eq!(flatten(refused), Err("run run_a has ended".to_owned()));
    }

    use super::*;

    fn offline(kind: OfflineKind, detail: &str, attempt: u32) -> Offline {
        Offline {
            kind,
            detail: detail.into(),
            attempt,
        }
    }

    #[test]
    fn a_missing_daemon_says_how_to_start_it() {
        let missing = offline(OfflineKind::NotRunning, "no daemon is listening", 1);
        assert_eq!(
            offline_reason(&missing),
            "Daemon not running — run `reno daemon start`"
        );
    }

    #[test]
    fn a_daemon_being_started_or_failing_to_start_says_so_with_the_reason() {
        let starting = offline(OfflineKind::Starting, "starting the daemon", 1);
        assert_eq!(offline_reason(&starting), "Starting the daemon…");
        let failed = offline(
            OfflineKind::StartFailed,
            "renod exited (exit status: 3). Last log lines:\nrenod: locked",
            2,
        );
        assert_eq!(
            offline_reason(&failed),
            "The daemon did not start: renod exited (exit status: 3). Last log lines:\nrenod: locked"
        );
        let mut state = State::new();
        state.apply(Update::Offline(starting));
        assert_eq!(
            retry_line(&state).as_deref(),
            Some("Waiting for it to answer.")
        );
    }

    #[test]
    fn auto_start_is_on_unless_the_environment_turns_it_off() {
        // The keepers are only built here, never used.
        let socket = std::path::Path::new("/nonexistent/reno.sock");
        let on = daemon_keeper(socket, |_| None);
        let off = daemon_keeper(socket, |key| {
            (key == "RENO_NO_AUTOSTART").then(|| "1".to_owned())
        });
        let zero = daemon_keeper(socket, |key| {
            (key == "RENO_NO_AUTOSTART").then(|| "0".to_owned())
        });
        // Without HOME there is no data directory, and no keeper at all.
        if let (Some(on), Some(off), Some(zero)) = (on, off, zero) {
            assert!(on.auto_start() && zero.auto_start() && !off.auto_start());
        }
    }

    #[test]
    fn other_failures_say_what_went_wrong() {
        let other = offline(OfflineKind::Other, "transport error: broken pipe", 1);
        assert_eq!(offline_reason(&other), "transport error: broken pipe");
        let incompatible = offline(OfflineKind::Incompatible, "protocol 1 vs 2", 1);
        assert!(
            offline_reason(&incompatible).starts_with("The daemon cannot talk to this app"),
            "{}",
            offline_reason(&incompatible)
        );
        let lost = offline(OfflineKind::Lost, "gone", 0);
        assert_eq!(offline_reason(&lost), "The daemon connection closed.");
    }

    #[test]
    fn the_retry_line_follows_the_attempts() {
        let mut state = State::new();
        assert_eq!(retry_line(&state), None);
        state.apply(Update::Offline(offline(OfflineKind::Lost, "x", 1)));
        assert_eq!(retry_line(&state).as_deref(), Some("Retrying shortly."));
        state.apply(Update::Offline(offline(OfflineKind::NotRunning, "x", 4)));
        assert!(retry_line(&state).unwrap().starts_with("Tried 4 times"));
        state.apply(Update::Connection(ConnectionStatus::Connecting));
        assert_eq!(retry_line(&state).as_deref(), Some("Reconnecting…"));
    }
}
