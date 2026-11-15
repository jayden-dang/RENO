//! The definitions state: loading through the port, versions, the last good set, and the bot
//! pin on a run. The port is a fake the test drives; nothing reads a directory or sleeps.

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use reno_proto::defs::{DefError, DefFile, DefKind, ValidationContext};
use reno_proto::{RunStatus, Seq};
use serde_json::json;

use super::{Fx, id, mock, registry, until};
use crate::{DefSnapshot, Definitions, DefsSetup, KernelConfig, KernelError};
use std::sync::Arc;
use tokio::sync::mpsc;

/// A port whose files and failures the test sets, and whose change signal it fires.
#[derive(Debug, Default)]
pub(super) struct FakePort {
    state: Mutex<Option<DefSnapshot>>,
    tx: Mutex<Option<mpsc::Sender<()>>>,
    snapshots: AtomicUsize,
    watch_fails: bool,
}

impl FakePort {
    pub(super) fn with(files: Vec<DefFile>) -> Arc<Self> {
        let port = Self::default();
        port.set(files);
        Arc::new(port)
    }

    pub(super) fn set(&self, files: Vec<DefFile>) {
        *self.state.lock().unwrap() = Some(DefSnapshot {
            files,
            problems: Vec::new(),
        });
    }

    fn set_problem(&self, files: Vec<DefFile>, problem: DefError) {
        *self.state.lock().unwrap() = Some(DefSnapshot {
            files,
            problems: vec![problem],
        });
    }

    fn fail(&self) {
        *self.state.lock().unwrap() = None;
    }

    /// Fires the change signal, as a debounced watcher would.
    fn touch(&self) {
        let tx = self
            .tx
            .lock()
            .unwrap()
            .clone()
            .expect("the kernel is watching");
        let _ = tx.try_send(());
    }

    fn reads(&self) -> usize {
        self.snapshots.load(Ordering::SeqCst)
    }

    /// Fires the signal and waits until the kernel has read the files for it.
    pub(super) async fn touch_and_wait(&self) {
        let before = self.reads();
        self.touch();
        until(async || (self.reads() > before).then_some(())).await;
        // The read is followed by validating and, if it is good, storing and publishing.
        tokio::task::yield_now().await;
    }

    /// Closes the signal channel, as a watcher that has died would.
    fn stop_watching(&self) {
        self.tx.lock().unwrap().take();
    }
}

#[async_trait]
impl Definitions for FakePort {
    async fn snapshot(&self) -> crate::Result<DefSnapshot> {
        self.snapshots.fetch_add(1, Ordering::SeqCst);
        self.state
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| KernelError::Definitions("the disk is on fire".into()))
    }

    fn watch(&self) -> crate::Result<mpsc::Receiver<()>> {
        if self.watch_fails {
            return Err(KernelError::Definitions("no watcher".into()));
        }
        let (tx, rx) = mpsc::channel(1);
        *self.tx.lock().unwrap() = Some(tx);
        Ok(rx)
    }
}

pub(super) fn file(kind: DefKind, name: &str, text: &str) -> DefFile {
    DefFile {
        kind,
        name: name.to_owned(),
        text: text.to_owned(),
    }
}

fn bot(name: &str, instructions: &str) -> DefFile {
    file(
        DefKind::Bot,
        name,
        &format!(
            "name = \"{name}\"\ninstructions = \"{instructions}\"\nexecutors = [\"codex\", \"mock\"]\n"
        ),
    )
}

pub(super) fn setup(port: &Arc<FakePort>) -> DefsSetup {
    DefsSetup::new(port.clone(), ValidationContext::with_builtin([]))
}

async fn fx_with(port: &Arc<FakePort>) -> Fx {
    Fx::with_definitions(KernelConfig::default(), registry(&[mock([])]), setup(port)).await
}

fn bot_version(fx: &Fx, name: &str) -> Option<u32> {
    fx.kernel
        .current_defs()
        .version_of(DefKind::Bot, name)
        .map(|v| v.version)
}

async fn kinds(fx: &Fx) -> Vec<String> {
    fx.store
        .events_since(Seq(0), 100_000)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.kind)
        .filter(|k| k.starts_with("definition."))
        .collect()
}

#[tokio::test(start_paused = true)]
async fn the_first_load_stores_and_publishes_before_start_returns() {
    let port = FakePort::with(vec![
        bot("keeper", "a"),
        file(DefKind::Policy, "policy", "default = \"deny\"\n"),
    ]);
    let fx = fx_with(&port).await;

    let current = fx.kernel.current_defs();
    assert!(current.errors().is_empty());
    assert_eq!(current.set().len(), 2);
    assert_eq!(bot_version(&fx, "keeper"), Some(1));
    assert_eq!(
        kinds(&fx).await,
        ["definition.created", "definition.created"]
    );

    let snapshot = current.snapshot(None);
    let listed: Vec<_> = snapshot
        .defs
        .iter()
        .map(|d| (d.kind, d.name.as_str(), d.version))
        .collect();
    assert_eq!(
        listed,
        [(DefKind::Bot, "keeper", 1), (DefKind::Policy, "policy", 1)]
    );
    assert_eq!(current.snapshot(Some(DefKind::Policy)).defs.len(), 1);
    // The published version is the one in the store.
    let stored = fx
        .store
        .current_definitions(Some(DefKind::Bot))
        .await
        .unwrap();
    assert_eq!(stored[0].hash, snapshot.defs[0].hash);
}

#[tokio::test(start_paused = true)]
async fn an_edit_becomes_a_new_version_and_is_published() {
    let port = FakePort::with(vec![bot("keeper", "a")]);
    let fx = fx_with(&port).await;
    let mut watch = fx.kernel.watch_defs();
    watch.borrow_and_update();

    port.set(vec![bot("keeper", "b")]);
    port.touch();
    until(async || (bot_version(&fx, "keeper") == Some(2)).then_some(())).await;
    assert!(watch.has_changed().unwrap());
    let published = watch.borrow_and_update().clone();
    assert_eq!(published.set().bot("keeper").unwrap().instructions, "b");
    assert_eq!(
        kinds(&fx).await,
        ["definition.created", "definition.created"]
    );
}

#[tokio::test(start_paused = true)]
async fn a_reload_that_changes_nothing_costs_no_store_write() {
    let port = FakePort::with(vec![bot("keeper", "a")]);
    let fx = fx_with(&port).await;
    let writes = fx.idle().await;
    let events = kinds(&fx).await;
    let mut watch = fx.kernel.watch_defs();
    watch.borrow_and_update();

    port.touch_and_wait().await;
    port.touch_and_wait().await;

    assert_eq!(
        fx.idle().await,
        writes,
        "a spurious reload wrote to the store"
    );
    assert_eq!(kinds(&fx).await, events);
    assert!(
        !watch.has_changed().unwrap(),
        "nothing changed, nothing was published"
    );
    assert_eq!(bot_version(&fx, "keeper"), Some(1));
}

#[tokio::test(start_paused = true)]
async fn a_whitespace_edit_is_not_a_new_version() {
    let port = FakePort::with(vec![bot("keeper", "a")]);
    let fx = fx_with(&port).await;
    let writes = fx.idle().await;

    port.set(vec![file(
        DefKind::Bot,
        "keeper",
        "# reformatted\n\nexecutors   = [ \"codex\",\n  \"mock\" ]\ninstructions=\"a\"\nname = \"keeper\"\n",
    )]);
    port.touch_and_wait().await;

    assert_eq!(bot_version(&fx, "keeper"), Some(1));
    assert_eq!(fx.idle().await, writes);
}

#[tokio::test(start_paused = true)]
async fn an_invalid_edit_keeps_the_last_good_set_and_publishes_the_errors() {
    let port = FakePort::with(vec![bot("keeper", "a")]);
    let fx = fx_with(&port).await;
    let writes = fx.idle().await;
    let mut watch = fx.kernel.watch_defs();
    watch.borrow_and_update();

    // `nope` is not a known executor.
    port.set(vec![file(
        DefKind::Bot,
        "keeper",
        "name = \"keeper\"\ninstructions = \"b\"\nexecutors = [\"mock\", \"nope\"]\n",
    )]);
    port.touch();
    until(async || (!fx.kernel.current_defs().errors().is_empty()).then_some(())).await;

    let current = fx.kernel.current_defs();
    assert_eq!(
        current
            .errors()
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>(),
        ["bots/keeper.toml: executors[1]: unknown executor \"nope\""]
    );
    // The set in force is the old one, at the old version.
    assert_eq!(current.set().bot("keeper").unwrap().instructions, "a");
    assert_eq!(bot_version(&fx, "keeper"), Some(1));
    // Nothing was stored for the bad edit.
    assert_eq!(fx.idle().await, writes);
    assert_eq!(kinds(&fx).await, ["definition.created"]);
    // The subscriber sees the errors beside the old set.
    let seen = watch.borrow_and_update().snapshot(None);
    assert_eq!(seen.defs.len(), 1);
    assert_eq!(seen.errors.len(), 1);

    // A run submitted now still runs under the last good version.
    let run = fx
        .kernel
        .submit_for_bot("keeper", "go", None, "/work".into(), None)
        .await
        .unwrap();
    assert_eq!(
        fx.kernel.get_run(&run).await.unwrap().bot.unwrap().version,
        1
    );

    // Fixing the file clears the errors and moves on.
    port.set(vec![bot("keeper", "c")]);
    port.touch();
    until(async || (bot_version(&fx, "keeper") == Some(2)).then_some(())).await;
    assert!(fx.kernel.current_defs().errors().is_empty());
}

#[tokio::test(start_paused = true)]
async fn one_bad_file_keeps_every_definition_at_its_old_version() {
    let port = FakePort::with(vec![bot("keeper", "a"), bot("scout", "a")]);
    let fx = fx_with(&port).await;

    // keeper is fine, scout is broken: neither change is taken.
    port.set(vec![
        bot("keeper", "b"),
        file(DefKind::Bot, "scout", "name = "),
    ]);
    port.touch();
    until(async || (!fx.kernel.current_defs().errors().is_empty()).then_some(())).await;
    assert_eq!(bot_version(&fx, "keeper"), Some(1));
    assert_eq!(bot_version(&fx, "scout"), Some(1));
    assert_eq!(
        fx.kernel
            .current_defs()
            .set()
            .bot("keeper")
            .unwrap()
            .instructions,
        "a"
    );
}

#[tokio::test(start_paused = true)]
async fn an_unreadable_file_refuses_the_whole_snapshot() {
    let port = FakePort::with(vec![bot("keeper", "a")]);
    let fx = fx_with(&port).await;
    let problem = DefError::new("bots/broken.toml", "", "the file is not valid UTF-8");

    port.set_problem(vec![bot("keeper", "b")], problem.clone());
    port.touch();
    until(async || (!fx.kernel.current_defs().errors().is_empty()).then_some(())).await;
    assert_eq!(fx.kernel.current_defs().errors(), [problem]);
    assert_eq!(bot_version(&fx, "keeper"), Some(1));
}

#[tokio::test(start_paused = true)]
async fn a_failing_port_is_reported_and_the_set_stays() {
    let port = FakePort::with(vec![bot("keeper", "a")]);
    let fx = fx_with(&port).await;

    port.fail();
    port.touch();
    until(async || (!fx.kernel.current_defs().errors().is_empty()).then_some(())).await;
    let current = fx.kernel.current_defs();
    assert_eq!(current.errors().len(), 1);
    assert!(
        current.errors()[0].message.contains("the disk is on fire"),
        "{:?}",
        current.errors()
    );
    assert_eq!(bot_version(&fx, "keeper"), Some(1));

    port.set(vec![bot("keeper", "a")]);
    port.touch();
    until(async || fx.kernel.current_defs().errors().is_empty().then_some(())).await;
    assert_eq!(bot_version(&fx, "keeper"), Some(1));
}

#[tokio::test(start_paused = true)]
async fn a_bad_first_load_runs_on_the_last_good_set_from_the_store() {
    let port = FakePort::with(vec![bot("keeper", "a"), bot("scout", "a")]);
    let fx = fx_with(&port).await;
    fx.kernel.shutdown().await;

    // The daemon was down while the files were broken.
    port.set(vec![
        bot("keeper", "b"),
        file(DefKind::Bot, "scout", "name = "),
    ]);
    let (dir, store) = fx.into_store();
    let fx = Fx::over_definitions(
        dir,
        store,
        KernelConfig::default(),
        registry(&[mock([])]),
        setup(&port),
    )
    .await;

    let current = fx.kernel.current_defs();
    assert_eq!(current.errors().len(), 1, "{:?}", current.errors());
    assert_eq!(current.set().bot("keeper").unwrap().instructions, "a");
    assert_eq!(bot_version(&fx, "keeper"), Some(1));
    assert_eq!(bot_version(&fx, "scout"), Some(1));
    // And it can start runs on it.
    fx.kernel
        .submit_for_bot("scout", "go", None, "/work".into(), None)
        .await
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn a_restart_over_unchanged_files_writes_nothing() {
    let port = FakePort::with(vec![
        bot("keeper", "a"),
        file(DefKind::Policy, "policy", ""),
    ]);
    let fx = fx_with(&port).await;
    let events = kinds(&fx).await;
    fx.kernel.shutdown().await;

    // What a kernel with no definitions writes when it starts over this store: its own claim.
    let (dir, store) = fx.into_store();
    let plain = Fx::over(dir, store, KernelConfig::default(), registry(&[mock([])])).await;
    let baseline = plain.idle().await;
    plain.kernel.shutdown().await;

    let (dir, store) = plain.into_store();
    let fx = Fx::over_definitions(
        dir,
        store,
        KernelConfig::default(),
        registry(&[mock([])]),
        setup(&port),
    )
    .await;
    assert_eq!(
        fx.idle().await,
        baseline,
        "loading unchanged definitions wrote"
    );
    assert_eq!(kinds(&fx).await, events);
    assert_eq!(bot_version(&fx, "keeper"), Some(1));
}

#[tokio::test(start_paused = true)]
async fn a_removed_file_leaves_the_set_and_its_history_stays() {
    let port = FakePort::with(vec![bot("keeper", "a"), bot("scout", "a")]);
    let fx = fx_with(&port).await;

    port.set(vec![bot("keeper", "a")]);
    port.touch();
    until(async || (bot_version(&fx, "scout").is_none()).then_some(())).await;
    assert_eq!(fx.kernel.current_defs().set().bots().len(), 1);
    assert_eq!(
        kinds(&fx)
            .await
            .iter()
            .filter(|k| *k == "definition.retired")
            .count(),
        1
    );

    let err = fx
        .kernel
        .submit_for_bot("scout", "go", None, "/work".into(), None)
        .await
        .unwrap_err();
    assert!(
        matches!(err, KernelError::DefinitionNotFound { .. }),
        "{err:?}"
    );

    // Its versions can still be read.
    let record = fx
        .kernel
        .get_definition(DefKind::Bot, "scout", None)
        .await
        .unwrap();
    assert_eq!((record.version, record.latest), (1, true));

    // Put back unchanged, it is the same version again.
    port.set(vec![bot("keeper", "a"), bot("scout", "a")]);
    port.touch();
    until(async || (bot_version(&fx, "scout") == Some(1)).then_some(())).await;
}

#[tokio::test(start_paused = true)]
async fn a_submitted_run_pins_the_version_in_force_and_keeps_it() {
    let port = FakePort::with(vec![bot("keeper", "a")]);
    let fx = fx_with(&port).await;

    let first = fx
        .kernel
        .submit_for_bot("keeper", "one", None, "/work".into(), None)
        .await
        .unwrap();
    port.set(vec![bot("keeper", "b")]);
    port.touch();
    until(async || (bot_version(&fx, "keeper") == Some(2)).then_some(())).await;
    let second = fx
        .kernel
        .submit_for_bot("keeper", "two", None, "/work".into(), None)
        .await
        .unwrap();
    let plain = fx.submit("three").await;

    let pin = |run| {
        let kernel = fx.kernel.clone();
        async move { kernel.get_run(&run).await.unwrap().bot }
    };
    let pinned = |version| {
        Some(reno_proto::defs::BotPin {
            name: "keeper".into(),
            version,
        })
    };
    assert_eq!(pin(first.clone()).await, pinned(1));
    assert_eq!(pin(second.clone()).await, pinned(2));
    assert_eq!(pin(plain).await, None);

    // The pin survives the run, and shows in the listing.
    fx.wait_status(&first, RunStatus::Succeeded).await;
    assert_eq!(pin(first.clone()).await, pinned(1));
    let (listed, _) = fx.kernel.list_runs(None, None, None).await.unwrap();
    assert_eq!(listed.iter().filter(|r| r.bot == pinned(1)).count(), 1);

    // The version the run holds is one the store can produce.
    let held = fx
        .kernel
        .get_definition(DefKind::Bot, "keeper", Some(1))
        .await
        .unwrap();
    assert_eq!(held.body["instructions"], "a");
    assert!(!held.latest);
}

#[tokio::test(start_paused = true)]
async fn a_bot_run_takes_the_first_executor_of_the_bot_that_the_kernel_has() {
    // The bot lists codex first; only `mock` is registered.
    let port = FakePort::with(vec![bot("keeper", "a")]);
    let fx = fx_with(&port).await;
    let run = fx
        .kernel
        .submit_for_bot("keeper", "go", None, "/work".into(), None)
        .await
        .unwrap();
    assert_eq!(fx.kernel.get_run(&run).await.unwrap().executor, id("mock"));

    // An explicit executor wins.
    let run = fx
        .kernel
        .submit_for_bot("keeper", "go", Some(id("codex")), "/work".into(), None)
        .await
        .unwrap();
    assert_eq!(fx.kernel.get_run(&run).await.unwrap().executor, id("codex"));

    // A bot none of whose executors is here has nowhere to run.
    port.set(vec![file(
        DefKind::Bot,
        "keeper",
        "name = \"keeper\"\ninstructions = \"a\"\nexecutors = [\"codex\"]\n",
    )]);
    port.touch();
    until(async || (bot_version(&fx, "keeper") == Some(2)).then_some(())).await;
    let err = fx
        .kernel
        .submit_for_bot("keeper", "go", None, "/work".into(), None)
        .await
        .unwrap_err();
    assert!(matches!(err, KernelError::Invalid(_)), "{err:?}");
}

#[tokio::test(start_paused = true)]
async fn an_unknown_bot_cannot_be_run() {
    let port = FakePort::with(vec![]);
    let fx = fx_with(&port).await;
    let err = fx
        .kernel
        .submit_for_bot("ghost", "go", None, "/work".into(), None)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, KernelError::DefinitionNotFound { kind: DefKind::Bot, name, version: None } if name == "ghost"),
        "{err:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn definitions_can_be_read_by_version() {
    let port = FakePort::with(vec![bot("keeper", "a")]);
    let fx = fx_with(&port).await;
    port.set(vec![bot("keeper", "b")]);
    port.touch();
    until(async || (bot_version(&fx, "keeper") == Some(2)).then_some(())).await;

    let newest = fx
        .kernel
        .get_definition(DefKind::Bot, "keeper", None)
        .await
        .unwrap();
    assert_eq!((newest.version, newest.latest), (2, true));
    assert_eq!(newest.body["instructions"], "b");
    let old = fx
        .kernel
        .get_definition(DefKind::Bot, "keeper", Some(1))
        .await
        .unwrap();
    assert_eq!((old.version, old.latest), (1, false));
    assert_eq!(old.body["instructions"], "a");
    assert_eq!(old.hash.len(), 64);

    for (name, version) in [("keeper", Some(3)), ("ghost", None)] {
        let err = fx
            .kernel
            .get_definition(DefKind::Bot, name, version)
            .await
            .unwrap_err();
        assert!(
            matches!(err, KernelError::DefinitionNotFound { .. }),
            "{err:?}"
        );
    }
}

#[tokio::test(start_paused = true)]
async fn a_body_can_be_validated_without_saving() {
    let port = FakePort::with(vec![bot("keeper", "a")]);
    let fx = fx_with(&port).await;
    let writes = fx.idle().await;

    let good = json!({ "name": "scout", "instructions": "x", "executors": ["mock"] });
    let result = fx.kernel.validate_definition(DefKind::Bot, "scout", &good);
    assert!(result.valid, "{:?}", result.errors);
    assert_eq!(result.hash.as_deref().map(str::len), Some(64));

    let bad = json!({ "name": "scout", "instructions": "x", "executors": ["nope"] });
    let result = fx.kernel.validate_definition(DefKind::Bot, "scout", &bad);
    assert!(!result.valid);
    assert_eq!(result.hash, None);
    assert_eq!(
        result.errors[0].to_string(),
        "bots/scout.toml: executors[0]: unknown executor \"nope\""
    );

    assert_eq!(fx.idle().await, writes);
    assert_eq!(bot_version(&fx, "scout"), None);
}

#[tokio::test(start_paused = true)]
async fn a_dead_watch_ends_the_loader_and_shutdown_leaves_no_task() {
    let port = FakePort::with(vec![bot("keeper", "a")]);
    let fx = fx_with(&port).await;
    let running = fx.kernel.live_tasks();

    port.stop_watching();
    until(async || (fx.kernel.live_tasks() < running).then_some(())).await;
    // The set in force stays usable.
    assert_eq!(bot_version(&fx, "keeper"), Some(1));

    let report = fx.kernel.shutdown().await;
    assert!(!report.aborted);
    assert_eq!(fx.kernel.live_tasks(), 0);
}

#[tokio::test(start_paused = true)]
async fn shutdown_stops_the_loader() {
    let port = FakePort::with(vec![bot("keeper", "a")]);
    let fx = fx_with(&port).await;
    assert!(
        fx.kernel.live_tasks() >= 3,
        "dispatcher, runs hub and loader"
    );
    let report = fx.kernel.shutdown().await;
    assert!(!report.aborted);
    assert_eq!(fx.kernel.live_tasks(), 0);
}

#[tokio::test(start_paused = true)]
async fn a_watch_that_cannot_start_leaves_the_first_load_in_force() {
    let port = Arc::new(FakePort {
        watch_fails: true,
        ..FakePort::default()
    });
    port.set(vec![bot("keeper", "a")]);
    let fx = fx_with(&port).await;
    assert_eq!(bot_version(&fx, "keeper"), Some(1));
    assert!(fx.kernel.current_defs().errors().is_empty());
    fx.kernel.shutdown().await;
    assert_eq!(fx.kernel.live_tasks(), 0);
}

#[tokio::test(start_paused = true)]
async fn without_a_port_the_set_is_empty_and_bodies_still_validate() {
    let fx = Fx::new(KernelConfig::default(), registry(&[mock([])])).await;
    assert!(fx.kernel.current_defs().set().is_empty());
    let err = fx
        .kernel
        .submit_for_bot("keeper", "go", None, "/work".into(), None)
        .await
        .unwrap_err();
    assert!(matches!(err, KernelError::DefinitionNotFound { .. }));
    let body = json!({ "name": "keeper", "instructions": "x", "executors": ["mock"] });
    assert!(
        fx.kernel
            .validate_definition(DefKind::Bot, "keeper", &body)
            .valid
    );
    let body = json!({ "name": "keeper", "instructions": "x", "executors": ["codex"] });
    assert!(
        fx.kernel
            .validate_definition(DefKind::Bot, "keeper", &body)
            .valid,
        "built-ins are known"
    );
    let body = json!({ "name": "keeper", "instructions": "x", "executors": ["grok"] });
    assert!(
        !fx.kernel
            .validate_definition(DefKind::Bot, "keeper", &body)
            .valid
    );
}
