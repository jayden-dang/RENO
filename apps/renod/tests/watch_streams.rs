//! File watches that cannot start, and watchers that must not leak.
//!
//! The machine's pool of FSEvents streams is finite and shared by every process on it, so a start can be
//! refused for a while. These tests use a watcher factory under the test's control, so they do
//! not use a platform stream and do not depend on how many others are in use: it refuses until
//! it is opened, keeps the handlers it is given so the test can raise events, and is wrapped in
//! `Counting` so a stream that is not stopped shows up as a count above zero.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use common::{Fixture, within};
use futures::StreamExt;
use notify::Event;
use notify::EventKind;
use notify::event::CreateKind;
use reno_kernel::{Watch, WatchSpec};
use reno_proto::{TriggerInfo, TriggerStatus};
use renod::watcher::{Counting, Handler, WatcherGuard};
use renod::{FsWatch, WatcherFactory};

/// Refuses every start until it is opened, then starts watchers that report what the test raises.
#[derive(Default)]
struct Switch {
    open: AtomicBool,
    started: Mutex<Vec<Handler>>,
    refused: Mutex<usize>,
}

impl std::fmt::Debug for Switch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Switch")
    }
}

impl Switch {
    fn open(&self) {
        self.open.store(true, Ordering::SeqCst);
    }

    fn refused(&self) -> usize {
        *self.refused.lock().unwrap()
    }

    /// A file appears at `path`, for every watcher that was started.
    fn create(&self, path: &Path) {
        let event = Event::new(EventKind::Create(CreateKind::File)).add_path(path.to_owned());
        for handler in self.started.lock().unwrap().iter_mut() {
            handler(Ok(event.clone()));
        }
    }
}

impl WatcherFactory for Switch {
    fn start(&self, roots: &[PathBuf], handler: Handler) -> Result<WatcherGuard, String> {
        if !self.open.load(Ordering::SeqCst) {
            *self.refused.lock().unwrap() += 1;
            return Err(format!(
                "cannot watch {}: unable to start FSEvent stream",
                roots[0].display()
            ));
        }
        self.started.lock().unwrap().push(handler);
        Ok(WatcherGuard::new(()))
    }
}

fn write(defs: &Path, rel: &str, text: &str) {
    let path = defs.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

const BOT: &str = "instructions = \"x\"\nexecutors = [\"mock\"]\n";

fn bot(name: &str) -> String {
    format!("name = \"{name}\"\n{BOT}")
}

async fn defs_until(fx: &Fixture, ready: impl Fn(&[String]) -> bool) {
    let mut watch = fx.client.watch_defs().await.unwrap();
    within(async {
        loop {
            let snapshot = watch.next().await.expect("the watch ended").unwrap();
            let names: Vec<String> = snapshot.defs.iter().map(|d| d.name.clone()).collect();
            if ready(&names) {
                return;
            }
        }
    })
    .await;
}

async fn triggers_until(fx: &Fixture, ready: impl Fn(&[TriggerInfo]) -> bool) -> Vec<TriggerInfo> {
    let mut watch = fx.client.watch_triggers().await.unwrap();
    within(async {
        loop {
            let snapshot = watch.next().await.expect("the watch ended").unwrap();
            if ready(&snapshot.triggers) {
                return snapshot.triggers;
            }
        }
    })
    .await
}

fn named<'a>(all: &'a [TriggerInfo], name: &str) -> Option<&'a TriggerInfo> {
    all.iter().find(|t| t.name == name)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_definitions_watch_that_cannot_start_is_shown_then_recovers_and_edits_reload() {
    let switch = Arc::new(Switch::default());
    let counting = Arc::new(Counting::new(Arc::clone(&switch)));
    let fx = Fixture::with(|c| c.watchers = counting.clone()).await;

    // The daemon is up and serving the last good definitions, and says its watch is not running.
    let status = within(fx.client.status()).await.unwrap();
    let problem = status.definitions_watch.expect("the failure is shown");
    assert!(
        problem.contains("unable to start FSEvent stream"),
        "{problem}"
    );
    assert_eq!(counting.live(), 0);
    let loaded = within(fx.client.list_defs(None)).await.unwrap();
    assert_eq!(loaded.defs.len(), 2, "the example files are still in force");

    // An edit made while nothing watches is not seen yet. It is read when the watch starts: the
    // start is itself a reason to read again.
    write(&fx.paths.defs_dir, "bots/late.toml", &bot("late"));
    switch.open();
    defs_until(&fx, |names| names.iter().any(|n| n == "late")).await;
    let status = within(fx.client.status()).await.unwrap();
    assert_eq!(status.definitions_watch, None);
    assert!(switch.refused() >= 1);
    assert_eq!(counting.live(), 1);

    // And from then on edits are noticed as usual.
    write(&fx.paths.defs_dir, "bots/later.toml", &bot("later"));
    switch.create(
        &fx.paths
            .defs_dir
            .canonicalize()
            .unwrap()
            .join("bots/later.toml"),
    );
    defs_until(&fx, |names| names.iter().any(|n| n == "later")).await;

    fx.stop().await;
    assert_eq!(counting.live(), 0, "shutdown stopped every watcher");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_fs_trigger_whose_watch_cannot_start_shows_failed_then_watches_and_fires() {
    let switch = Arc::new(Switch::default());
    let counting = Arc::new(Counting::new(Arc::clone(&switch)));
    let dir = tempfile::tempdir().unwrap();
    let watched = fs::canonicalize(dir.path()).unwrap().join("watched");
    fs::create_dir(&watched).unwrap();
    let fx = Fixture::with(|c| {
        let defs = &c.paths.defs_dir;
        write(defs, "bots/keeper.toml", &bot("keeper"));
        write(
            defs,
            "triggers/inbox.toml",
            &format!(
                "name = \"inbox\"\n[when]\nkind = \"fs\"\npaths = [\"{}/*.txt\"]\ndebounce = \"100ms\"\n\
                 [target]\nbot = \"keeper\"\nprompt = \"for inbox\"\n",
                watched.display()
            ),
        );
        c.watchers = counting.clone();
    })
    .await;

    let failed = triggers_until(&fx, |all| {
        named(all, "inbox").is_some_and(|t| t.status == TriggerStatus::Failed)
    })
    .await;
    let error = named(&failed, "inbox").unwrap().error.clone().unwrap();
    assert!(error.contains("unable to start FSEvent stream"), "{error}");
    assert!(error.contains("retrying"), "{error}");

    switch.open();
    triggers_until(&fx, |all| {
        named(all, "inbox").is_some_and(|t| t.status == TriggerStatus::Watching)
    })
    .await;

    switch.create(&watched.join("new.txt"));
    let fired = triggers_until(&fx, |all| {
        named(all, "inbox").is_some_and(|t| t.last_run_id.is_some())
    })
    .await;
    assert!(named(&fired, "inbox").unwrap().error.is_none());

    fx.stop().await;
    assert_eq!(counting.live(), 0, "shutdown stopped every watcher");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_a_watch_stream_stops_its_watcher() {
    let switch = Arc::new(Switch::default());
    switch.open();
    let counting = Arc::new(Counting::new(Arc::clone(&switch)));
    let fs_watch = FsWatch::with_watchers(counting.clone());
    let dir = tempfile::tempdir().unwrap();
    let spec = |name: &str| WatchSpec {
        trigger: name.into(),
        paths: vec![dir.path().display().to_string()],
        debounce: Duration::from_millis(10),
    };

    let streams: Vec<_> = (0..8)
        .map(|n| fs_watch.watch(&spec(&format!("t{n}"))).unwrap())
        .collect();
    assert_eq!(counting.live(), 8);
    drop(streams);
    assert_eq!(
        counting.live(),
        0,
        "every dropped stream stopped its watcher"
    );
    within(fs_watch.shutdown()).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_daemon_dropped_without_shutdown_lets_go_of_every_stream() {
    let switch = Arc::new(Switch::default());
    switch.open();
    let counting = Arc::new(Counting::new(Arc::clone(&switch)));
    let dir = tempfile::tempdir().unwrap();
    let watched = fs::canonicalize(dir.path()).unwrap().join("watched");
    fs::create_dir(&watched).unwrap();
    let fx = Fixture::with(|c| {
        let defs = &c.paths.defs_dir;
        write(defs, "bots/keeper.toml", &bot("keeper"));
        write(
            defs,
            "triggers/inbox.toml",
            &format!(
                "name = \"inbox\"\n[when]\nkind = \"fs\"\npaths = [\"{}/*.txt\"]\ndebounce = \"100ms\"\n\
                 [target]\nbot = \"keeper\"\nprompt = \"for inbox\"\n",
                watched.display()
            ),
        );
        c.watchers = counting.clone();
    })
    .await;
    triggers_until(&fx, |all| {
        named(all, "inbox").is_some_and(|t| t.status == TriggerStatus::Watching)
    })
    .await;
    assert_eq!(
        counting.live(),
        2,
        "the definitions' stream and the trigger's"
    );

    // No `stop()`: the owner just goes away. The daemon's own tasks must not keep a stream.
    drop(fx);
    within(async {
        while counting.live() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
}
