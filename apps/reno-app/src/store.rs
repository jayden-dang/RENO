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

use std::collections::BTreeSet;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use gpui::{Context, EventEmitter, Task};
use gpui_tokio::Tokio;
use reno_client::fake::{FakeConfig, FakeDaemon};
use reno_client::launcher::{DaemonKeeper, KeeperConfig, Launcher};
use reno_client::{
    Changed, Client, ClientError, ConnectionStatus, Connector, Feed, FeedState, Mirror,
    MirrorConfig, Offline, OfflineKind, RunPage, State, Transcript, Update, Updates, filter_key,
};
use reno_proto::defs::DefKind;
use reno_proto::{
    ApprovalDecision, ApprovalId, FiringOutcome, RunId, RunListParams, RunStatus, TaskRunParams,
};

use crate::summary;
use crate::vm;

/// How many runs one page of the runs list asks for.
pub(crate) const RUNS_PAGE: u32 = 50;

/// Things the shell reacts to that are not state: a started run, a failed action.
#[derive(Clone, Debug)]
pub(crate) enum StoreEvent {
    RunStarted(RunId),
    /// A trigger was fired by hand: what became of it.
    TriggerFired {
        name: String,
        outcome: FiringOutcome,
        run: Option<RunId>,
    },
    Failed(String),
}

pub(crate) struct Store {
    state: State,
    /// Keeps the mirror's tasks alive; dropping it cancels them.
    mirror: Option<Mirror>,
    /// The fake daemon of `--fake`, kept alive for as long as the store.
    fake: Option<Arc<FakeDaemon>>,
    /// Where the real daemon's socket is, shown in Settings.
    socket: Option<PathBuf>,
    /// Approvals with a decision in flight (their buttons are disabled).
    deciding: BTreeSet<ApprovalId>,
    /// Triggers being fired by hand (their buttons are disabled).
    firing: BTreeSet<String>,
    /// Filters whose page request is in flight.
    paging: BTreeSet<Vec<RunStatus>>,
    /// Runs being fetched with `run.get`.
    fetching_runs: BTreeSet<RunId>,
    /// Definition versions being fetched with `def.get`.
    fetching_defs: BTreeSet<(DefKind, String, u32)>,
    /// Definitions being validated.
    validating: BTreeSet<(DefKind, String)>,
    /// Ages use the system clock (a real daemon) or the state's newest timestamp (the fake).
    system_clock: bool,
    /// The clock reading when the last `daemon.status` arrived, to carry its uptime forward.
    status_seen_at_ms: u64,
    /// The run the Run screen shows: its transcript changes notify.
    open: Option<RunId>,
    /// The boot task and then the update consumer. Owned here so it dies with the store.
    bridge: Option<Task<()>>,
}

impl EventEmitter<StoreEvent> for Store {}

impl Store {
    /// A store that has not connected yet.
    pub(crate) fn connecting() -> Self {
        Self {
            state: State::new(),
            mirror: None,
            fake: None,
            socket: None,
            deciding: BTreeSet::new(),
            firing: BTreeSet::new(),
            paging: BTreeSet::new(),
            fetching_runs: BTreeSet::new(),
            fetching_defs: BTreeSet::new(),
            validating: BTreeSet::new(),
            system_clock: false,
            status_seen_at_ms: 0,
            open: None,
            bridge: None,
        }
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

    #[cfg(test)]
    pub(crate) fn fake(&self) -> Option<&Arc<FakeDaemon>> {
        self.fake.as_ref()
    }

    pub(crate) fn is_deciding(&self, id: &ApprovalId) -> bool {
        self.deciding.contains(id)
    }

    pub(crate) fn is_firing(&self, name: &str) -> bool {
        self.firing.contains(name)
    }

    pub(crate) fn is_paging(&self, statuses: &[RunStatus]) -> bool {
        self.paging.contains(&filter_key(statuses))
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
        let changed = self.state.apply(update);
        if changed.daemon {
            self.status_seen_at_ms = self.now_ms();
        }
        let open_run_changed = self.open.as_ref().is_some_and(|run| changed.run(run));
        if changed.connection
            || changed.daemon
            || changed.runs
            || changed.approvals
            || changed.triggers
            || changed.defs
            || changed.history
            || changed.feeds
            || open_run_changed
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
        self.start_fake_with(FakeConfig::default(), cx);
    }

    /// Like [`Store::start_fake`], with a config (an empty world, missing features).
    pub(crate) fn start_fake_with(&mut self, config: FakeConfig, cx: &mut Context<Self>) {
        match FakeDaemon::start(config) {
            Ok(daemon) => {
                let daemon = Arc::new(daemon);
                self.fake = Some(Arc::clone(&daemon));
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

    fn client(&self) -> Option<Client> {
        self.mirror.as_ref().and_then(Mirror::client)
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
        self.paging.clear();
        if matches!(self.state.feed(Feed::ApprovalHistory), FeedState::Failed(_)) {
            self.load_history(cx);
        }
        self.ensure_def_records(cx);
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
        if let Some((bot, version)) = vm::runs::bot_to_fetch(&self.state, &run) {
            self.ensure_bot(&bot, version, cx);
        }
        cx.notify();
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

    /// `run.steer`. A failure is reported as a store event.
    pub(crate) fn steer(&mut self, run: RunId, prompt: String, cx: &mut Context<Self>) {
        self.call(
            "Could not steer the run",
            move |client| async move { client.steer(&run, prompt).await },
            cx,
            D::fire_and_forget(),
        );
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
        if self.client().is_none() || !self.deciding.insert(id.clone()) {
            return;
        }
        cx.notify();
        let ok_id = id.clone();
        let err_id = id.clone();
        self.call(
            "Could not send the decision",
            move |client| async move { client.decide(&id, decision).await },
            cx,
            D {
                ok: Box::new(move |store, (), cx| {
                    store.deciding.remove(&ok_id);
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

    /// `task.run` on the given executor id.
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
            bot: None,
            cwd: None,
            model: None,
            in_place: false,
        };
        self.call(
            "Could not start the task",
            move |client| async move { client.run_task(params).await },
            cx,
            D::then(|_, run, cx| cx.emit(StoreEvent::RunStarted(run))),
        );
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

    /// `run.list`: the first page for a filter, or the next one when there is a cursor. A filter
    /// is paged one request at a time.
    pub(crate) fn load_runs_page(&mut self, statuses: &[RunStatus], cx: &mut Context<Self>) {
        let key = filter_key(statuses);
        let before = match self.state.run_page(&key) {
            RunPage::Done => return,
            RunPage::More(cursor) => Some(cursor),
            RunPage::NotLoaded => None,
        };
        if self.client().is_none() || !self.paging.insert(key.clone()) {
            return;
        }
        cx.notify();
        let params = RunListParams {
            status: (!key.is_empty()).then(|| key.clone()),
            limit: Some(RUNS_PAGE),
            before,
        };
        let (ok_key, err_key) = (key.clone(), key.clone());
        self.call(
            "Could not load runs",
            move |client| async move { client.list_runs_page(params).await },
            cx,
            D {
                ok: Box::new(move |store, page: reno_proto::RunListResult, cx| {
                    store.paging.remove(&ok_key);
                    store.apply(
                        Update::RunsPage {
                            filter: ok_key,
                            runs: page.runs,
                            next: page.next,
                        },
                        cx,
                    );
                    cx.notify();
                }),
                err: Box::new(move |store, cx| {
                    store.paging.remove(&err_key);
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
                ok: Box::new(|store, records, cx| {
                    store.apply(Update::ApprovalHistory(records), cx);
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

    /// `run.get` for a run the window does not hold (opened from a trigger or the inbox).
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

fn flatten<T>(result: Result<Result<T, ClientError>, gpui_tokio::JoinError>) -> Result<T, String> {
    match result {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(e.to_string()),
        Err(e) => Err(e.to_string()),
    }
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
