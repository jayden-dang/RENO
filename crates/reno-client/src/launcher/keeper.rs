//! [`DaemonKeeper`]: connect to the daemon, and start it when nothing answers.
//!
//! The app's [`Mirror`](crate::Mirror) already retries a failed connection on its own capped
//! backoff, so the keeper adds no timer and no task. It is the `Connector` the mirror calls:
//! each call tries the socket and, when nothing answers, decides whether to start the daemon.
//!
//! # Policy
//!
//! - **Auto-start.** The app starts a daemon when it finds none: Reno is local-first, the
//!   daemon is a per-user background process that the app is a window onto, and an app that
//!   only said "run `reno daemon start`" would be a worse experience for nothing gained. It can
//!   be turned off ([`KeeperConfig::auto_start`]), and the app does so with `RENO_NO_AUTOSTART=1`.
//! - **Launchd first.** With a LaunchAgent installed for this data directory, the keeper never
//!   spawns `renod`; it asks launchd (see [`Launcher::start`]).
//! - **Backoff.** Starts follow the [`RestartPolicy`]: 1 s doubling to 60 s between starts, reset
//!   once the daemon has stayed up for a minute. Between starts the keeper only reports.
//! - **No pile-up.** The child it spawned is kept and `try_wait`ed on the next attempt, which
//!   both reaps it when it dies and tells a crash from a slow start. A child that is still
//!   running is never started a second time, however long it takes to answer.
//! - **Reasons.** An attempt that fails is a [`ClientError::StartFailed`] whose text says how
//!   the daemon ended (exit status, or that it never answered) followed by the last lines of
//!   its logs, so the offline banner can say why.

use std::path::PathBuf;
use std::process::Child;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::logs::recent_trouble;
use super::policy::{RestartConfig, RestartPolicy, Verdict};
use super::{Launcher, Started};
use crate::client::Client;
use crate::error::ClientError;
use crate::mirror::Connector;

/// How many log lines go into a failure's text.
const TROUBLE_LINES: usize = 5;

/// Tuning of a [`DaemonKeeper`].
#[derive(Debug, Clone, Copy)]
pub struct KeeperConfig {
    /// Whether to start a daemon when none answers.
    pub auto_start: bool,
    /// The restart backoff.
    pub restart: RestartConfig,
    /// How long a started daemon gets to answer before the keeper calls the start failed.
    pub startup_window: Duration,
    /// The time, replaceable so that tests need not wait.
    pub now: fn() -> Instant,
}

impl Default for KeeperConfig {
    fn default() -> Self {
        Self {
            auto_start: true,
            restart: RestartConfig::default(),
            startup_window: Duration::from_secs(20),
            now: Instant::now,
        }
    }
}

#[derive(Debug)]
struct Inner {
    policy: RestartPolicy,
    /// The `renod` this keeper spawned, if it is still believed to be running.
    child: Option<Child>,
    /// When the last start was made, while it is still expected to come up.
    started_at: Option<Instant>,
    /// Why the last start did not work out.
    failure: Option<String>,
    /// Whether launchd was the last one asked to start it.
    launchd: bool,
}

/// Connects to the daemon, starting it when nothing answers. See the module docs.
#[derive(Debug)]
pub struct DaemonKeeper {
    socket: PathBuf,
    launcher: Launcher,
    config: KeeperConfig,
    inner: Mutex<Inner>,
}

/// What to do after a failed connection, decided under the lock and done outside it.
enum Plan {
    /// Report this and do nothing.
    Report(ClientError),
    /// Start the daemon.
    Start,
}

impl DaemonKeeper {
    /// A keeper for the daemon on `socket`, started through `launcher`.
    pub fn new(socket: PathBuf, launcher: Launcher, config: KeeperConfig) -> Arc<Self> {
        Arc::new(Self {
            socket,
            launcher,
            config,
            inner: Mutex::new(Inner {
                policy: RestartPolicy::new(config.restart),
                child: None,
                started_at: None,
                failure: None,
                launchd: false,
            }),
        })
    }

    /// Whether this keeper starts a daemon when none answers.
    pub fn auto_start(&self) -> bool {
        self.config.auto_start
    }

    /// This keeper as the mirror's connector.
    pub fn connector(self: &Arc<Self>) -> Connector {
        let keeper = Arc::clone(self);
        Arc::new(move || {
            let keeper = Arc::clone(&keeper);
            Box::pin(async move { keeper.connect().await })
        })
    }

    /// Connects, or starts the daemon (subject to the policy) and says how that went.
    pub async fn connect(&self) -> Result<Client, ClientError> {
        match Client::connect_unix(&self.socket).await {
            Ok(client) => {
                let mut inner = self.lock();
                inner.policy.reachable((self.config.now)());
                inner.failure = None;
                inner.started_at = None;
                return Ok(client);
            }
            Err(error @ ClientError::NotRunning(_)) if !self.config.auto_start => {
                return Err(error);
            }
            Err(ClientError::NotRunning(_)) => {}
            Err(error) => return Err(error),
        }
        match self.plan() {
            Plan::Report(error) => Err(error),
            Plan::Start => Err(self.start().await),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn trouble(&self) -> String {
        recent_trouble(&self.launcher.logs(), TROUBLE_LINES)
    }

    fn failed(&self, reason: &str, then: &str) -> ClientError {
        let mut detail = reason.to_owned();
        let trouble = self.trouble();
        if !trouble.is_empty() {
            detail.push_str(". Last log lines:\n");
            detail.push_str(&trouble);
        }
        if !then.is_empty() {
            detail.push('\n');
            detail.push_str(then);
        }
        ClientError::StartFailed(detail)
    }

    fn plan(&self) -> Plan {
        let now = (self.config.now)();
        let mut inner = self.lock();
        if let Some(child) = inner.child.as_mut() {
            match child.try_wait() {
                Ok(None) => {
                    // Still running: never a second one, however slow it is.
                    let waited = inner.started_at.map(|at| now.saturating_duration_since(at));
                    return Plan::Report(match waited {
                        Some(waited) if waited >= self.config.startup_window => {
                            let reason = format!(
                                "renod is running but has not answered for {} seconds",
                                waited.as_secs()
                            );
                            self.failed(&reason, "")
                        }
                        _ => ClientError::Starting,
                    });
                }
                Ok(Some(status)) => {
                    inner.failure = Some(format!("renod exited ({status})"));
                    inner.child = None;
                    inner.started_at = None;
                }
                Err(error) => {
                    inner.failure = Some(format!("cannot watch renod: {error}"));
                    inner.child = None;
                    inner.started_at = None;
                }
            }
        }
        if inner.launchd
            && let Some(at) = inner.started_at
        {
            if now.saturating_duration_since(at) < self.config.startup_window {
                return Plan::Report(ClientError::Starting);
            }
            inner.failure = Some(format!(
                "launchd was asked to start renod but it has not answered for {} seconds",
                self.config.startup_window.as_secs()
            ));
            inner.started_at = None;
        }
        match inner.policy.unreachable(now) {
            Verdict::Start => Plan::Start,
            Verdict::Wait(wait) => {
                let reason = inner
                    .failure
                    .clone()
                    .unwrap_or_else(|| "the daemon did not answer".to_owned());
                let then = format!("Starting it again in {} s.", wait.as_secs().max(1));
                drop(inner);
                Plan::Report(self.failed(&reason, &then))
            }
        }
    }

    async fn start(&self) -> ClientError {
        let launcher = self.launcher.clone();
        let started = tokio::task::spawn_blocking(move || launcher.start()).await;
        let now = (self.config.now)();
        let outcome = match started {
            Ok(outcome) => outcome,
            Err(error) => {
                let reason = format!("the start task failed: {error}");
                self.lock().failure = Some(reason.clone());
                return self.failed(&reason, "");
            }
        };
        let mut inner = self.lock();
        match outcome {
            Ok(Started::Spawned(child)) => {
                inner.child = Some(child);
                inner.launchd = false;
            }
            Ok(Started::Launchd) => {
                inner.child = None;
                inner.launchd = true;
            }
            Err(error) => {
                let reason = error.to_string();
                inner.failure = Some(reason.clone());
                drop(inner);
                return self.failed(&reason, "");
            }
        }
        inner.started_at = Some(now);
        match inner.failure.clone() {
            // The first start of an episode is just "starting"; a restart still says why.
            None => ClientError::Starting,
            Some(reason) => {
                drop(inner);
                self.failed(&reason, "Restarted it; waiting for it to answer.")
            }
        }
    }

    /// Stops the child this keeper spawned. For tests: a real keeper leaves the daemon running.
    #[cfg(test)]
    fn kill_child(&self) {
        if let Some(mut child) = self.lock().child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::os::unix::fs::PermissionsExt;

    use super::*;
    use crate::launcher::{FakeLaunchctl, Launcher};

    thread_local! {
        static OFFSET: Cell<Duration> = const { Cell::new(Duration::ZERO) };
        static BASE: Instant = Instant::now();
    }

    fn fake_now() -> Instant {
        BASE.with(|base| *base + OFFSET.with(Cell::get))
    }

    fn advance(by: Duration) {
        OFFSET.with(|offset| offset.set(offset.get() + by));
    }

    struct Fx {
        _root: tempfile::TempDir,
        keeper: Arc<DaemonKeeper>,
        launchctl: Arc<FakeLaunchctl>,
        spawns: PathBuf,
    }

    /// A keeper whose `renod` is `body` (a shell script), over a socket nobody listens on.
    fn fx(body: &str, auto_start: bool, launchctl: FakeLaunchctl) -> Fx {
        OFFSET.with(|offset| offset.set(Duration::ZERO));
        let root = tempfile::tempdir().expect("tempdir");
        let data = root.path().join("data");
        let spawns = root.path().join("spawns");
        let renod = root.path().join("renod");
        std::fs::write(
            &renod,
            format!("#!/bin/sh\necho x >> '{}'\n{body}\n", spawns.display()),
        )
        .expect("renod");
        std::fs::set_permissions(&renod, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let launchctl = Arc::new(launchctl);
        let launcher = Launcher::new(
            data.clone(),
            data.join("reno.sock"),
            root.path().join("LaunchAgents"),
            launchctl.clone(),
        )
        .with_renod(renod);
        let keeper = DaemonKeeper::new(
            data.join("reno.sock"),
            launcher,
            KeeperConfig {
                auto_start,
                now: fake_now,
                ..KeeperConfig::default()
            },
        );
        Fx {
            _root: root,
            keeper,
            launchctl,
            spawns,
        }
    }

    impl Fx {
        fn spawn_count(&self) -> usize {
            std::fs::read_to_string(&self.spawns).map_or(0, |text| text.lines().count())
        }

        /// Waits until the script of the `n`th spawned `renod` has run its first line.
        async fn spawned(&self, n: usize) {
            for _ in 0..400 {
                if self.spawn_count() >= n {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            panic!("renod was not run {n} times");
        }

        /// Connects until the answer is not `Starting`: a child that exits takes a moment, and
        /// there is no event to wait on in a test either.
        async fn settle(&self) -> ClientError {
            for _ in 0..400 {
                match self.keeper.connect().await {
                    Err(ClientError::Starting) => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(other) => return other,
                    Ok(_) => panic!("nothing listens"),
                }
            }
            panic!("still starting");
        }
    }

    #[tokio::test]
    async fn with_auto_start_off_nothing_is_spawned_and_the_error_is_plain() {
        let fx = fx("exit 0", false, FakeLaunchctl::new());
        assert!(matches!(
            fx.keeper.connect().await,
            Err(ClientError::NotRunning(_))
        ));
        assert_eq!(fx.spawn_count(), 0);
    }

    #[tokio::test]
    async fn a_daemon_that_dies_is_reported_with_its_log_and_restarted_on_the_backoff() {
        let fx = fx(
            "echo 'renod: cannot open the store' >&2\nexit 3",
            true,
            FakeLaunchctl::new(),
        );
        assert!(matches!(
            fx.keeper.connect().await,
            Err(ClientError::Starting)
        ));
        let ClientError::StartFailed(detail) = fx.settle().await else {
            panic!("expected a failed start");
        };
        assert!(detail.contains("exit status: 3"), "{detail}");
        assert!(detail.contains("cannot open the store"), "{detail}");
        assert!(detail.contains("Starting it again in 1 s"), "{detail}");
        fx.spawned(1).await;
        assert_eq!(fx.spawn_count(), 1, "no second start before the backoff");

        // Asking again early changes nothing; after the pause it starts again, and then waits 2 s.
        let ClientError::StartFailed(_) = fx.keeper.connect().await.expect_err("early") else {
            panic!("expected a failed start");
        };
        assert_eq!(fx.spawn_count(), 1);
        advance(Duration::from_secs(1));
        let ClientError::StartFailed(detail) = fx.keeper.connect().await.expect_err("restart")
        else {
            panic!("a restart still says why");
        };
        assert!(detail.contains("Restarted it"), "{detail}");
        fx.spawned(2).await;
        let ClientError::StartFailed(detail) = fx.settle().await else {
            panic!("expected a failed start");
        };
        assert!(detail.contains("Starting it again in 2 s"), "{detail}");
    }

    #[tokio::test]
    async fn a_slow_daemon_is_never_started_twice() {
        let fx = fx("exec sleep 30", true, FakeLaunchctl::new());
        assert!(matches!(
            fx.keeper.connect().await,
            Err(ClientError::Starting)
        ));
        advance(Duration::from_secs(10));
        assert!(matches!(
            fx.keeper.connect().await,
            Err(ClientError::Starting)
        ));
        advance(Duration::from_secs(60));
        let ClientError::StartFailed(detail) = fx.keeper.connect().await.expect_err("slow") else {
            panic!("expected a failed start");
        };
        assert!(detail.contains("has not answered"), "{detail}");
        fx.spawned(1).await;
        assert_eq!(fx.spawn_count(), 1);
        fx.keeper.kill_child();
    }

    #[tokio::test]
    async fn with_an_agent_installed_the_keeper_asks_launchd_and_spawns_nothing() {
        let fx = fx("exit 0", true, FakeLaunchctl::loaded());
        let agent = fx.keeper.launcher.agent().expect("agent");
        fx.keeper.launcher.write_agent(&agent).expect("write");
        assert!(matches!(
            fx.keeper.connect().await,
            Err(ClientError::Starting)
        ));
        assert!(matches!(
            fx.keeper.connect().await,
            Err(ClientError::Starting)
        ));
        assert_eq!(
            fx.launchctl.calls().len(),
            1,
            "one kickstart while it starts"
        );
        assert_eq!(fx.spawn_count(), 0);

        advance(Duration::from_secs(30));
        assert!(matches!(
            fx.keeper.connect().await,
            Err(ClientError::StartFailed(_))
        ));
    }

    #[tokio::test]
    async fn a_start_that_cannot_happen_says_so() {
        let fx = fx("exit 0", true, FakeLaunchctl::loaded());
        let agent = fx.keeper.launcher.agent().expect("agent");
        fx.keeper.launcher.write_agent(&agent).expect("write");
        fx.launchctl.failing("Could not find service");
        let ClientError::StartFailed(detail) = fx.keeper.connect().await.expect_err("fails") else {
            panic!("expected a failed start");
        };
        assert!(detail.contains("Could not find service"), "{detail}");
    }
}
