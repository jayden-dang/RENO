//! The store: one GPUI `Entity` that owns the `reno_client::State` mirror (ARCHITECTURE 17.6).
//!
//! # How the mirror is bridged
//!
//! `reno-client` runs on tokio and hands over a bounded `futures` channel of `Update`s
//! (ARCH-11, ARCH-12). The store never blocks on tokio:
//!
//! 1. The connection is made on tokio (`gpui_tokio::Tokio::spawn`), because `Mirror::start`
//!    spawns its tasks there.
//! 2. [`Store::attach`] then reads the channel on GPUI's own executor with a GPUI task the store
//!    owns. A `futures` receiver needs no tokio context, so each `next().await` wakes the
//!    foreground task directly.
//! 3. Each update goes through the pure `State::apply`, and `notify` is called only for what the
//!    returned `Changed` says a view shows (connection, daemon, runs, approvals). A transcript-only
//!    change redraws nothing on Home.
//!
//! Actions (`decide`, `run_task`) run on tokio and report back. A decided approval is not removed
//! locally: the card goes away when the daemon's `approvals.watch` update arrives.
//!
//! Dropping the store drops its task, the `Mirror` (which cancels its tracked tasks) and the
//! client.

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use futures::StreamExt;
use gpui::{Context, EventEmitter, Task};
use gpui_tokio::Tokio;
use reno_client::fake::FakeDaemon;
use reno_client::{
    Changed, Client, ClientError, ConnectionStatus, Mirror, MirrorConfig, State, Update, Updates,
};
use reno_proto::{ApprovalDecision, ApprovalId, RunId, TaskRunParams};

use crate::summary;

/// Things the shell reacts to that are not state: a started run, a failed action.
#[derive(Clone, Debug)]
pub(crate) enum StoreEvent {
    RunStarted(RunId),
    Failed(String),
}

pub(crate) struct Store {
    state: State,
    client: Option<Client>,
    /// Keeps the mirror's tasks alive; dropping it cancels them.
    mirror: Option<Mirror>,
    /// Why there is no daemon, shown on Home when disconnected.
    offline_reason: Option<String>,
    /// Approvals with a decision in flight (their buttons are disabled).
    deciding: BTreeSet<ApprovalId>,
    /// Ages use the system clock (a real daemon) or the state's newest timestamp (the fake).
    system_clock: bool,
    /// The boot task and then the update consumer. Owned here so it dies with the store.
    bridge: Option<Task<()>>,
}

impl EventEmitter<StoreEvent> for Store {}

impl Store {
    /// A store that has not connected yet.
    pub(crate) fn connecting() -> Self {
        Self {
            state: State::new(),
            client: None,
            mirror: None,
            offline_reason: None,
            deciding: BTreeSet::new(),
            system_clock: false,
            bridge: None,
        }
    }

    pub(crate) fn state(&self) -> &State {
        &self.state
    }

    pub(crate) fn connection(&self) -> ConnectionStatus {
        self.state.connection()
    }

    pub(crate) fn offline_reason(&self) -> Option<&str> {
        self.offline_reason.as_deref()
    }

    pub(crate) fn is_deciding(&self, id: &ApprovalId) -> bool {
        self.deciding.contains(id)
    }

    /// "Now" for ages on Home.
    pub(crate) fn now_ms(&self) -> u64 {
        summary::reference_now(&self.state, self.system_clock.then(system_now_ms))
    }

    /// Applies one update and notifies only for what changed on screen.
    pub(crate) fn apply(&mut self, update: Update, cx: &mut Context<Self>) -> Changed {
        let changed = self.state.apply(update);
        if changed.connection || changed.daemon || changed.runs || changed.approvals {
            cx.notify();
        }
        changed
    }

    /// Marks the daemon as unreachable, with the reason Home shows.
    pub(crate) fn go_offline(&mut self, reason: impl Into<String>, cx: &mut Context<Self>) {
        self.offline_reason = Some(reason.into());
        self.client = None;
        self.mirror = None;
        self.apply(Update::Connection(ConnectionStatus::Disconnected), cx);
        // The reason changed even when the status did not.
        cx.notify();
    }

    /// Starts `FakeDaemon` in-process, over the in-memory transport.
    pub(crate) fn start_fake(&mut self, cx: &mut Context<Self>) {
        let boot = Tokio::spawn(cx, async {
            let client = FakeDaemon::spawn().await?;
            let (mirror, updates) = Mirror::start(client.clone(), MirrorConfig::default());
            Ok::<_, ClientError>((client, mirror, updates))
        });
        self.bridge = Some(cx.spawn(async move |this, cx| {
            let booted = match boot.await {
                Ok(Ok(parts)) => Ok(parts),
                Ok(Err(e)) => Err(format!("the fake daemon did not start: {e}")),
                Err(e) => Err(format!("the connection task failed: {e}")),
            };
            this.update(cx, |store, cx| match booted {
                Ok((client, mirror, updates)) => store.attach(client, mirror, updates, cx),
                Err(reason) => store.go_offline(reason, cx),
            })
            .ok();
        }));
    }

    /// Connects to the daemon socket. `renod` does not exist yet, and the app has no edge to
    /// `reno-rpc` (ARCH-4), so today this only probes and reports why there is no daemon.
    pub(crate) fn start_socket(&mut self, cx: &mut Context<Self>) {
        self.system_clock = true;
        let path = socket_path();
        let probe = path.as_deref().map(std::os::unix::net::UnixStream::connect);
        let reason = offline_reason(
            path.as_deref(),
            probe.as_ref().and_then(|r| r.as_ref().err()),
        );
        self.go_offline(reason, cx);
    }

    /// Takes a connected client and the mirror's update channel, and consumes the channel on
    /// GPUI's executor.
    pub(crate) fn attach(
        &mut self,
        client: Client,
        mirror: Mirror,
        mut updates: Updates,
        cx: &mut Context<Self>,
    ) {
        self.client = Some(client);
        self.mirror = Some(mirror);
        self.offline_reason = None;
        self.bridge = Some(cx.spawn(async move |this, cx| {
            while let Some(update) = updates.next().await {
                let applied = this.update(cx, |store, cx| {
                    store.apply(update, cx);
                });
                if applied.is_err() {
                    return;
                }
            }
            // The mirror is gone: the connection ended.
            this.update(cx, |store, cx| {
                store.go_offline("the daemon connection closed", cx);
            })
            .ok();
        }));
    }

    /// Answers a held action. The card leaves when the daemon reports the new list.
    pub(crate) fn decide(
        &mut self,
        id: ApprovalId,
        decision: ApprovalDecision,
        cx: &mut Context<Self>,
    ) {
        let Some(client) = self.client.clone() else {
            return;
        };
        if !self.deciding.insert(id.clone()) {
            return;
        }
        cx.notify();
        let call = {
            let id = id.clone();
            Tokio::spawn(cx, async move { client.decide(&id, decision).await })
        };
        // One short-lived task per action; it holds the store weakly and ends with the call.
        cx.spawn(async move |this, cx| {
            let result = flatten(call.await);
            this.update(cx, |store, cx| {
                store.deciding.remove(&id);
                if let Err(reason) = result {
                    cx.emit(StoreEvent::Failed(format!(
                        "Could not send the decision: {reason}"
                    )));
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// `task.run` on the given executor id.
    pub(crate) fn run_task(&mut self, prompt: String, executor_id: &str, cx: &mut Context<Self>) {
        let Some(client) = self.client.clone() else {
            cx.emit(StoreEvent::Failed("The daemon is not running.".to_owned()));
            return;
        };
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
            executor,
            cwd: None,
            model: None,
        };
        let call = Tokio::spawn(cx, async move { client.run_task(params).await });
        cx.spawn(async move |this, cx| {
            let result = flatten(call.await);
            this.update(cx, |_, cx| match result {
                Ok(run) => cx.emit(StoreEvent::RunStarted(run)),
                Err(reason) => {
                    cx.emit(StoreEvent::Failed(format!(
                        "Could not start the task: {reason}"
                    )));
                }
            })
            .ok();
        })
        .detach();
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

/// `~/Library/Application Support/Reno/reno.sock` (ARCHITECTURE 14).
pub(crate) fn socket_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(
        PathBuf::from(home)
            .join("Library/Application Support/Reno")
            .join("reno.sock"),
    )
}

/// The sentence Home shows when there is no daemon.
pub(crate) fn offline_reason(path: Option<&Path>, probe_error: Option<&io::Error>) -> String {
    let Some(path) = path else {
        return "Cannot locate the daemon socket: HOME is not set.".to_owned();
    };
    match probe_error {
        Some(e)
            if matches!(
                e.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            format!("Nothing is listening on {}.", path.display())
        }
        Some(e) => format!("Could not reach {}: {e}.", path.display()),
        None => format!(
            "A daemon is listening on {}, but this build cannot attach to it yet.",
            path.display()
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offline_reasons() {
        let path = Path::new("/tmp/reno.sock");
        let missing = io::Error::from(io::ErrorKind::NotFound);
        assert_eq!(
            offline_reason(Some(path), Some(&missing)),
            "Nothing is listening on /tmp/reno.sock."
        );
        let refused = io::Error::from(io::ErrorKind::ConnectionRefused);
        assert!(offline_reason(Some(path), Some(&refused)).starts_with("Nothing is listening"));
        let denied = io::Error::from(io::ErrorKind::PermissionDenied);
        assert!(offline_reason(Some(path), Some(&denied)).starts_with("Could not reach"));
        assert!(offline_reason(Some(path), None).contains("cannot attach"));
        assert!(offline_reason(None, None).contains("HOME"));
    }

    #[test]
    fn socket_path_is_under_application_support() {
        if let Some(path) = socket_path() {
            assert!(path.ends_with("Library/Application Support/Reno/reno.sock"));
        }
    }
}
