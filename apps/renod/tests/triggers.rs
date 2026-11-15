//! Triggers through the daemon: `trigger.list`, `trigger.fire` and `triggers.watch` over the
//! socket, and an `fs` trigger fired by a real file change through `notify` (short real waits;
//! the failsafe only turns a hang into a failure).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::fs;
use std::path::Path;

use common::{Fixture, within};
use futures::StreamExt;
use reno_proto::{ErrorCode, FiringOutcome, RunStatus, TriggerInfo, TriggerStatus};

fn write(defs: &Path, rel: &str, text: &str) {
    let path = defs.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn trigger_text(name: &str, when: &str) -> String {
    format!(
        "name = \"{name}\"\n[when]\n{when}\n[target]\nbot = \"keeper\"\nprompt = \"for {{{{trigger}}}}\"\n"
    )
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
async fn list_fire_and_watch_over_the_socket() {
    let fx = Fixture::start().await;
    let defs = &fx.paths.defs_dir;
    write(
        defs,
        "bots/keeper.toml",
        "name = \"keeper\"\ninstructions = \"x\"\nexecutors = [\"mock\"]\n",
    );
    write(
        defs,
        "triggers/poke.toml",
        &trigger_text("poke", "kind = \"manual\""),
    );
    write(
        defs,
        "triggers/nightly.toml",
        &trigger_text(
            "nightly",
            "kind = \"cron\"\ncron = \"0 3 * * *\"\ntimezone = \"UTC\"",
        ),
    );

    let all = triggers_until(&fx, |all| all.len() == 2).await;
    assert_eq!(named(&all, "poke").unwrap().status, TriggerStatus::Manual);
    let nightly = named(&all, "nightly").unwrap();
    assert_eq!(nightly.status, TriggerStatus::Scheduled);
    assert!(nightly.next_due_at_ms.is_some());
    assert_eq!(fx.client.list_triggers().await.unwrap(), all);

    let fired = fx.client.fire_trigger("poke").await.unwrap();
    assert_eq!(fired.outcome, FiringOutcome::Fired);
    let run = fired.run_id.expect("a run was started");
    let summary = fx.client.get_run(&run).await.unwrap();
    assert_eq!(summary.prompt.as_deref(), Some("for poke"));
    assert_eq!(summary.bot.as_ref().unwrap().name, "keeper");
    let shown = triggers_until(&fx, |all| {
        named(all, "poke").is_some_and(|t| t.last_run_id.is_some())
    })
    .await;
    assert_eq!(
        named(&shown, "poke").unwrap().last_run_id,
        Some(run.clone())
    );

    let missing = fx.client.fire_trigger("ghost").await.unwrap_err();
    assert_eq!(missing.code(), Some(ErrorCode::NotFound));
    fx.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_file_change_fires_an_fs_trigger_through_notify() {
    let fx = Fixture::start().await;
    let watched = fx.dir.path().join("watched");
    fs::create_dir(&watched).unwrap();
    let defs = &fx.paths.defs_dir;
    write(
        defs,
        "bots/keeper.toml",
        "name = \"keeper\"\ninstructions = \"x\"\nexecutors = [\"mock\"]\n",
    );
    write(
        defs,
        "triggers/inbox.toml",
        &trigger_text(
            "inbox",
            &format!(
                "kind = \"fs\"\npaths = [\"{}/*.txt\"]\ndebounce = \"100ms\"",
                watched.display()
            ),
        ),
    );
    let all = triggers_until(&fx, |all| {
        named(all, "inbox").is_some_and(|t| t.status == TriggerStatus::Watching)
    })
    .await;
    assert!(named(&all, "inbox").unwrap().last_run_id.is_none());

    fs::write(watched.join("new.txt"), "hello").unwrap();
    let fired = triggers_until(&fx, |all| {
        named(all, "inbox").is_some_and(|t| t.last_run_id.is_some())
    })
    .await;
    let run = named(&fired, "inbox").unwrap().last_run_id.clone().unwrap();
    assert_eq!(
        fx.client.get_run(&run).await.unwrap().prompt.as_deref(),
        Some("for inbox")
    );
    within(async {
        while fx.status(&run).await != RunStatus::Succeeded {
            tokio::task::yield_now().await;
        }
    })
    .await;
    fx.stop().await;
}
