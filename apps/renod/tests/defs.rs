//! Definitions through the daemon: first start, edits noticed on disk and published to a
//! subscriber, the last good set on a bad edit, runs pinned to a bot version, and restarts.
//!
//! The daemon is served in-process on a temp socket, with its definitions in a temp directory
//! (`Paths::new` never points at the real `~/.reno`). Waits follow the daemon's own
//! `defs.watch` stream; the failsafe only turns a hang into a failure.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::fs;
use std::path::Path;

use common::{Fixture, id, within};
use futures::StreamExt;
use futures::stream::BoxStream;
use reno_client::{Client, ClientError};
use reno_proto::defs::{BotPin, DefKind, DefsSnapshot};
use reno_proto::{ErrorCode, TaskRunParams};
use renod::{Daemon, DaemonError, GitOutcome};
use serde_json::json;

type Watch = BoxStream<'static, Result<DefsSnapshot, ClientError>>;

macro_rules! daemon_test {
    ($(#[$meta:meta])* async fn $name:ident() $body:block) => {
        $(#[$meta])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() $body
    };
}

fn code(error: &ClientError) -> ErrorCode {
    error
        .code()
        .unwrap_or_else(|| panic!("not a daemon error: {error:?}"))
}

fn version_of(snapshot: &DefsSnapshot, kind: DefKind, name: &str) -> Option<u32> {
    snapshot
        .defs
        .iter()
        .find(|d| d.kind == kind && d.name == name)
        .map(|d| d.version)
}

/// Reads snapshots until `ready` accepts one.
async fn until(watch: &mut Watch, ready: impl Fn(&DefsSnapshot) -> bool) -> DefsSnapshot {
    within(async {
        loop {
            let snapshot = watch
                .next()
                .await
                .expect("the watch ended")
                .expect("a snapshot");
            if ready(&snapshot) {
                return snapshot;
            }
        }
    })
    .await
}

fn bot_text(name: &str, instructions: &str, executors: &str) -> String {
    format!("name = \"{name}\"\ninstructions = \"{instructions}\"\nexecutors = [{executors}]\n")
}

fn write(defs: &Path, rel: &str, text: &str) {
    let path = defs.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, text).unwrap();
}

fn list(snapshot: &DefsSnapshot) -> Vec<(DefKind, &str, u32)> {
    snapshot
        .defs
        .iter()
        .map(|d| (d.kind, d.name.as_str(), d.version))
        .collect()
}

daemon_test! {
    async fn first_start_creates_the_example_files_and_loads_them_clean() {
        let fx = Fixture::start().await;
        let defs = &fx.paths.defs_dir;
        assert!(defs.join("bots/example.toml").is_file());
        assert!(defs.join("policy.toml").is_file());
        assert!(defs.join("triggers").is_dir());
        assert!(fx.client.supports("defs"));

        let snapshot = within(fx.client.list_defs(None)).await.unwrap();
        assert_eq!(snapshot.errors, [], "the shipped files must validate");
        assert_eq!(list(&snapshot), [(DefKind::Bot, "example", 1), (DefKind::Policy, "policy", 1)]);

        let bots = within(fx.client.list_defs(Some(DefKind::Bot))).await.unwrap();
        assert_eq!(list(&bots), [(DefKind::Bot, "example", 1)]);

        let record = within(fx.client.get_def(DefKind::Bot, "example", None)).await.unwrap();
        assert_eq!((record.version, record.latest), (1, true));
        assert_eq!(record.hash, snapshot.defs[0].hash);
        assert_eq!(record.body["name"], "example");
        assert_eq!(record.body["access"], "supervised");
        let policy = within(fx.client.get_def(DefKind::Policy, "policy", Some(1))).await.unwrap();
        assert_eq!(policy.body["default"], "ask");
        assert_eq!(policy.body["rules"][0]["effect"], "deny");
    }
}

daemon_test! {
    async fn an_edit_on_disk_reaches_a_subscriber_as_a_new_version() {
        let fx = Fixture::start().await;
        let mut watch = within(fx.client.watch_defs()).await.unwrap();
        let first = until(&mut watch, |_| true).await;
        assert_eq!(version_of(&first, DefKind::Bot, "example"), Some(1));

        write(&fx.paths.defs_dir, "bots/example.toml", &bot_text("example", "changed", "\"codex\""));
        let second = until(&mut watch, |s| version_of(s, DefKind::Bot, "example") == Some(2)).await;
        assert!(second.errors.is_empty());
        assert_ne!(second.defs[0].hash, first.defs[0].hash);

        let v1 = within(fx.client.get_def(DefKind::Bot, "example", Some(1))).await.unwrap();
        let v2 = within(fx.client.get_def(DefKind::Bot, "example", Some(2))).await.unwrap();
        assert_eq!((v1.latest, v2.latest), (false, true));
        assert_ne!(v1.body["instructions"], "changed");
        assert_eq!(v2.body["instructions"], "changed");
        // def.list agrees with what was published.
        assert_eq!(within(fx.client.list_defs(None)).await.unwrap(), second);
    }
}

daemon_test! {
    async fn a_bad_edit_keeps_the_last_good_set_and_a_fix_clears_the_errors() {
        let fx = Fixture::start().await;
        let defs = &fx.paths.defs_dir;
        let original = fs::read_to_string(defs.join("bots/example.toml")).unwrap();
        let mut watch = within(fx.client.watch_defs()).await.unwrap();
        until(&mut watch, |_| true).await;

        write(defs, "bots/example.toml", &bot_text("example", "x", "\"claude-code\", \"nope\""));
        let broken = until(&mut watch, |s| !s.errors.is_empty()).await;
        assert_eq!(
            broken.errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["bots/example.toml: executors[1]: unknown executor \"nope\""]
        );
        // The last good set is still what is in force.
        assert_eq!(version_of(&broken, DefKind::Bot, "example"), Some(1));
        assert_eq!(within(fx.client.list_defs(None)).await.unwrap(), broken);
        let record = within(fx.client.get_def(DefKind::Bot, "example", None)).await.unwrap();
        assert_eq!(record.version, 1, "nothing was stored for the bad edit");

        // Putting the original back is not a new version: the content is version 1's.
        write(defs, "bots/example.toml", &original);
        let fixed = until(&mut watch, |s| s.errors.is_empty()).await;
        assert_eq!(version_of(&fixed, DefKind::Bot, "example"), Some(1));
    }
}

daemon_test! {
    async fn a_syntax_error_and_an_unknown_field_are_reported_with_their_place() {
        let fx = Fixture::start().await;
        let defs = &fx.paths.defs_dir;
        let mut watch = within(fx.client.watch_defs()).await.unwrap();
        until(&mut watch, |_| true).await;

        write(defs, "policy.toml", "default = \"ask\"\nmode = 1\n");
        let broken = until(&mut watch, |s| !s.errors.is_empty()).await;
        assert_eq!(broken.errors.len(), 1);
        assert_eq!(broken.errors[0].file, "policy.toml");
        assert!(broken.errors[0].message.starts_with("line 2, column 1: unknown field `mode`"));
    }
}

daemon_test! {
    async fn new_and_removed_files_change_the_set_and_history_is_kept() {
        let fx = Fixture::start().await;
        let defs = &fx.paths.defs_dir;
        let mut watch = within(fx.client.watch_defs()).await.unwrap();
        until(&mut watch, |_| true).await;

        write(defs, "bots/scout.toml", &bot_text("scout", "look around", "\"mock\""));
        write(
            defs,
            "triggers/manual-scout.toml",
            "name = \"manual-scout\"\n[when]\nkind = \"manual\"\n[target]\nbot = \"scout\"\nprompt = \"go\"\n",
        );
        let with = until(&mut watch, |s| {
            version_of(s, DefKind::Bot, "scout").is_some()
                && version_of(s, DefKind::Trigger, "manual-scout").is_some()
        })
        .await;
        assert!(with.errors.is_empty(), "{:?}", with.errors);

        fs::remove_file(defs.join("triggers/manual-scout.toml")).unwrap();
        fs::remove_file(defs.join("bots/scout.toml")).unwrap();
        let without = until(&mut watch, |s| version_of(s, DefKind::Bot, "scout").is_none()).await;
        assert!(without.errors.is_empty(), "{:?}", without.errors);
        assert_eq!(version_of(&without, DefKind::Bot, "example"), Some(1));

        // What was removed can still be read.
        let record = within(fx.client.get_def(DefKind::Bot, "scout", None)).await.unwrap();
        assert_eq!(record.version, 1);
    }
}

daemon_test! {
    async fn a_whitespace_only_edit_is_not_a_new_version() {
        let fx = Fixture::start().await;
        let defs = &fx.paths.defs_dir;
        let mut watch = within(fx.client.watch_defs()).await.unwrap();
        until(&mut watch, |_| true).await;

        let original = fs::read_to_string(defs.join("bots/example.toml")).unwrap();
        write(defs, "bots/example.toml", &format!("# reformatted\n\n\n{}\n\n# end\n", original.replace("\n\n", "\n\n\n")));
        // A later, real change: once it is published the whitespace edit has certainly been read.
        write(defs, "bots/marker.toml", &bot_text("marker", "m", "\"mock\""));
        let seen = until(&mut watch, |s| version_of(s, DefKind::Bot, "marker").is_some()).await;
        assert_eq!(version_of(&seen, DefKind::Bot, "example"), Some(1));
    }
}

daemon_test! {
    async fn a_body_can_be_validated_over_the_wire_without_saving() {
        let fx = Fixture::start().await;
        let good = json!({ "name": "scout", "instructions": "x", "executors": ["mock"] });
        let result = within(fx.client.validate_def(DefKind::Bot, "scout", good)).await.unwrap();
        assert!(result.valid);
        assert_eq!(result.hash.as_deref().map(str::len), Some(64));

        let bad = json!({ "name": "scout", "instructions": "x", "executors": ["nope"], "triggers": ["ghost"] });
        let result = within(fx.client.validate_def(DefKind::Bot, "scout", bad)).await.unwrap();
        assert!(!result.valid);
        assert_eq!(
            result.errors.iter().map(ToString::to_string).collect::<Vec<_>>(),
            [
                "bots/scout.toml: executors[0]: unknown executor \"nope\"",
                "bots/scout.toml: triggers[0]: unknown trigger \"ghost\"",
            ]
        );
        assert_eq!(result.hash, None);

        // Nothing was saved.
        let e = within(fx.client.get_def(DefKind::Bot, "scout", None)).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::NotFound);
    }
}

daemon_test! {
    async fn def_get_of_something_that_is_not_there_is_not_found() {
        let fx = Fixture::start().await;
        for (kind, name, version) in [
            (DefKind::Bot, "ghost", None),
            (DefKind::Bot, "example", Some(9)),
            (DefKind::Trigger, "ghost", None),
        ] {
            let e = within(fx.client.get_def(kind, name, version)).await.unwrap_err();
            assert_eq!(code(&e), ErrorCode::NotFound, "{kind} {name} {version:?}");
        }
    }
}

async fn run_for_bot(
    client: &Client,
    bot: &str,
    executor: Option<&str>,
) -> Result<reno_proto::RunId, ClientError> {
    within(client.run_task(TaskRunParams {
        prompt: "go".into(),
        executor: executor.map(id),
        bot: Some(bot.into()),
        cwd: None,
        model: None,
        in_place: false,
    }))
    .await
}

daemon_test! {
    async fn a_run_for_a_bot_pins_the_version_it_was_submitted_under() {
        let fx = Fixture::start().await;
        let defs = &fx.paths.defs_dir;
        let mut watch = within(fx.client.watch_defs()).await.unwrap();
        until(&mut watch, |_| true).await;
        write(defs, "bots/tester.toml", &bot_text("tester", "v1", "\"mock\""));
        until(&mut watch, |s| version_of(s, DefKind::Bot, "tester") == Some(1)).await;

        // No executor named: the bot's first one that the daemon has.
        let first = run_for_bot(&fx.client, "tester", None).await.unwrap();
        write(defs, "bots/tester.toml", &bot_text("tester", "v2", "\"mock\""));
        until(&mut watch, |s| version_of(s, DefKind::Bot, "tester") == Some(2)).await;
        let second = run_for_bot(&fx.client, "tester", Some("mock")).await.unwrap();

        let pin = |version| Some(BotPin { name: "tester".into(), version });
        let one = within(fx.client.get_run(&first)).await.unwrap();
        let two = within(fx.client.get_run(&second)).await.unwrap();
        assert_eq!((one.bot, one.executor), (pin(1), id("mock")));
        assert_eq!(two.bot, pin(2));

        // The pin outlives the run and is what `run.list` shows.
        fx.events(&first, None).await;
        assert_eq!(fx.status(&first).await, reno_proto::RunStatus::Succeeded);
        let listed = within(fx.client.list_runs(reno_proto::RunListParams::default())).await.unwrap();
        let mut pins: Vec<_> = listed.iter().filter_map(|r| r.bot.clone()).collect();
        pins.sort_by_key(|p| p.version);
        assert_eq!(pins, [pin(1).unwrap(), pin(2).unwrap()]);

        // A run submitted without a bot has none.
        let plain = fx.run("mock", "no bot").await;
        assert_eq!(within(fx.client.get_run(&plain)).await.unwrap().bot, None);
    }
}

daemon_test! {
    async fn a_run_for_a_bot_is_refused_when_it_cannot_run() {
        let fx = Fixture::start().await;
        // Unknown bot.
        let e = run_for_bot(&fx.client, "ghost", None).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::NotFound);
        // The example bot lists claude-code and codex; only mock executors are here.
        let e = run_for_bot(&fx.client, "example", None).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::InvalidParams);
        // An executor the daemon does not have.
        let e = run_for_bot(&fx.client, "example", Some("no-such-executor")).await.unwrap_err();
        assert_eq!(code(&e), ErrorCode::NotFound);
        // Neither a bot nor an executor: the router chooses (T1.5), so this is no longer refused.
        let run = within(fx.client.run_task(TaskRunParams {
            prompt: "go".into(),
            executor: None,
            bot: None,
            cwd: None,
            model: None,
            in_place: false,
        }))
        .await
        .unwrap();
        let routed = within(fx.client.run_routing(&run)).await.unwrap().unwrap();
        assert_eq!(routed.rule_matched, reno_proto::RuleMatched::Default);
    }
}

daemon_test! {
    async fn definitions_survive_a_restart_and_an_offline_edit_is_one_new_version() {
        let fx = Fixture::start().await;
        let (dir, paths, stopped) = fx.stop().await;
        assert!(!stopped.kernel_aborted);

        // Edited while the daemon was down.
        write(&paths.defs_dir, "bots/example.toml", &bot_text("example", "offline", "\"codex\""));
        let executors = common::Executors::new();
        let config = common::config(&dir, &executors);
        let daemon = within(Daemon::start(config)).await.unwrap();
        let client = within(Client::connect_unix(&paths.socket)).await.unwrap();
        let snapshot = within(client.list_defs(None)).await.unwrap();
        assert_eq!(list(&snapshot), [(DefKind::Bot, "example", 2), (DefKind::Policy, "policy", 1)]);
        assert!(snapshot.errors.is_empty());
        drop(client);
        daemon.request_shutdown();
        within(daemon.wait()).await.unwrap();

        // Unchanged files on the next start add nothing.
        let config = common::config(&dir, &executors);
        let daemon = within(Daemon::start(config)).await.unwrap();
        let client = within(Client::connect_unix(&paths.socket)).await.unwrap();
        let again = within(client.list_defs(None)).await.unwrap();
        assert_eq!(again, snapshot);
        drop(client);
        daemon.request_shutdown();
        within(daemon.wait()).await.unwrap();
    }
}

daemon_test! {
    async fn a_daemon_starts_and_serves_when_git_is_missing() {
        let fx = Fixture::with(|config| {
            config.git_init = |_| Box::pin(async { GitOutcome::Unavailable });
        })
        .await;
        let snapshot = within(fx.client.list_defs(None)).await.unwrap();
        assert_eq!(snapshot.defs.len(), 2);
        assert!(fx.paths.defs_dir.join("bots/example.toml").is_file());
        assert!(!fx.paths.defs_dir.join(".git").exists());
    }
}

daemon_test! {
    async fn the_definitions_directory_is_initialised_as_a_repository_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        let fx = Fixture::with(|config| {
            config.git_init = |_| {
                CALLS.fetch_add(1, Ordering::SeqCst);
                Box::pin(async { GitOutcome::Initialized })
            };
        })
        .await;
        assert_eq!(CALLS.load(Ordering::SeqCst), 1);
        let (dir, _paths, _stopped) = fx.stop().await;

        let executors = common::Executors::new();
        let mut config = common::config(&dir, &executors);
        config.git_init = |_| {
            CALLS.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { GitOutcome::Initialized })
        };
        let daemon = within(Daemon::start(config)).await.unwrap();
        daemon.request_shutdown();
        within(daemon.wait()).await.unwrap();
        assert_eq!(CALLS.load(Ordering::SeqCst), 1, "the second start must not run git");
    }
}

daemon_test! {
    async fn a_definitions_directory_that_cannot_exist_stops_the_start() {
        let dir = tempfile::tempdir().unwrap();
        let in_the_way = dir.path().join("file");
        fs::write(&in_the_way, "x").unwrap();
        let executors = common::Executors::new();
        let mut config = common::config(&dir, &executors);
        config.paths = config.paths.clone().with_defs_dir(in_the_way.join("defs"));
        let error = within(Daemon::start(config)).await.unwrap_err();
        assert!(matches!(error, DaemonError::Definitions { .. }), "{error:?}");
        assert!(error.to_string().contains("cannot use the definitions directory"));
    }
}

daemon_test! {
    async fn shutdown_stops_the_definitions_watch() {
        let fx = Fixture::start().await;
        let mut watch = within(fx.client.watch_defs()).await.unwrap();
        until(&mut watch, |_| true).await;
        let (_dir, _paths, stopped) = fx.stop().await;
        assert!(!stopped.kernel_aborted && !stopped.server_aborted);
        // The subscription ends with the daemon.
        within(async { while watch.next().await.is_some() {} }).await;
    }
}
