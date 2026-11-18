//! Secrets (T1.13) through the daemon: `secret.list`, `secret.set` and `secret.delete` over the
//! socket, a bot's secrets reaching its run, a missing one refusing it, and the value never
//! showing in what a client can read. The Keychain is `MemoryKeychain`: no real Keychain is
//! touched (ARCH-16).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{Fixture, id, within};
use futures::StreamExt;
use reno_harness::exec::{MockExecutor, MockStep};
use reno_kernel::ExecutorRegistry;
use reno_proto::defs::DefKind;
use reno_proto::{ErrorCode, ExecEvent, RunId, RunStatus, StopReason, TaskRunParams};
use renod::secrets::{Keychain as _, KeychainSecrets, MemoryKeychain};
use renod::{ExecutorSet, ExecutorSource};

const TOKEN: &str = "ghp_exampleExampleExample1234";

macro_rules! daemon_test {
    ($(#[$meta:meta])* async fn $name:ident() $body:block) => {
        $(#[$meta])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() $body
    };
}

fn keychain() -> (Arc<MemoryKeychain>, Arc<KeychainSecrets>) {
    let memory = Arc::new(MemoryKeychain::new());
    (memory.clone(), Arc::new(KeychainSecrets::new(memory)))
}

fn bot_task(bot: &str) -> TaskRunParams {
    TaskRunParams {
        model_options: Default::default(),
        effort: None,
        prompt: "go".into(),
        executor: None,
        bot: Some(bot.to_owned()),
        cwd: None,
        model: None,
        in_place: false,
        attachments: Vec::new(),
    }
}

async fn write_bot(fx: &Fixture, name: &str, extra: &str) {
    let mut watch = within(fx.client.watch_defs()).await.unwrap();
    let path = fx.paths.defs_dir.join(format!("bots/{name}.toml"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        format!("name = \"{name}\"\ninstructions = \"x\"\nexecutors = [\"mock\"]\n{extra}\n"),
    )
    .unwrap();
    within(async {
        while let Some(snapshot) = watch.next().await {
            let snapshot = snapshot.unwrap();
            if snapshot
                .defs
                .iter()
                .any(|d| d.kind == DefKind::Bot && d.name == name)
            {
                return;
            }
        }
        panic!("the watch ended");
    })
    .await;
}

async fn events(fx: &Fixture, run: &RunId) -> Vec<ExecEvent> {
    fx.events(run, None)
        .await
        .into_iter()
        .map(|item| item.event)
        .collect()
}

daemon_test! {
    async fn set_list_and_delete_work_over_the_socket_and_never_return_a_value() {
        let (memory, secrets) = keychain();
        let fx = Fixture::with(|c| c.secrets = Some(secrets)).await;
        write_bot(&fx, "keeper", "secrets = [\"GITHUB_TOKEN\"]").await;

        // Asked for by a bot, set by nobody.
        let listed = within(fx.client.list_secrets()).await.unwrap().secrets;
        assert_eq!(listed.len(), 1);
        assert_eq!((listed[0].name.as_str(), listed[0].set), ("GITHUB_TOKEN", false));
        assert_eq!(listed[0].used_by, ["keeper"]);

        assert!(!within(fx.client.set_secret("GITHUB_TOKEN", TOKEN)).await.unwrap());
        assert!(within(fx.client.set_secret("GITHUB_TOKEN", TOKEN)).await.unwrap());
        within(fx.client.set_secret("EXTRA_KEY", "another-secret-value")).await.unwrap();
        // The value went to the keychain, byte for byte.
        assert_eq!(memory.peek("GITHUB_TOKEN").unwrap(), TOKEN.as_bytes());

        let listed = within(fx.client.list_secrets()).await.unwrap();
        let shown = format!("{listed:?}");
        assert!(!shown.contains(TOKEN) && !shown.contains("another-secret"), "{shown}");
        let names: Vec<(&str, bool)> =
            listed.secrets.iter().map(|s| (s.name.as_str(), s.set)).collect();
        assert_eq!(names, [("EXTRA_KEY", true), ("GITHUB_TOKEN", true)]);

        assert!(within(fx.client.delete_secret("EXTRA_KEY")).await.unwrap());
        assert!(!within(fx.client.delete_secret("EXTRA_KEY")).await.unwrap());
        assert!(memory.peek("EXTRA_KEY").is_none());
    }
}

daemon_test! {
    async fn a_bad_name_is_invalid_params_and_a_locked_keychain_is_unavailable() {
        let (memory, secrets) = keychain();
        let fx = Fixture::with(|c| c.secrets = Some(secrets)).await;

        for bad in ["lower", "RENO_X", "", "A B"] {
            let error = within(fx.client.set_secret(bad, "some-value")).await.unwrap_err();
            assert_eq!(error.code(), Some(ErrorCode::InvalidParams), "{bad}");
        }
        let error = within(fx.client.set_secret("OK_NAME", "")).await.unwrap_err();
        assert_eq!(error.code(), Some(ErrorCode::InvalidParams));
        assert!(memory.accounts().unwrap().is_empty());

        memory.set_locked(true);
        let error = within(fx.client.set_secret("OK_NAME", "some-value")).await.unwrap_err();
        assert_eq!(error.code(), Some(ErrorCode::Unavailable));
        assert!(!format!("{error:?}").contains("some-value"));
        let error = within(fx.client.list_secrets()).await.unwrap_err();
        assert_eq!(error.code(), Some(ErrorCode::Unavailable));
    }
}

daemon_test! {
    async fn a_daemon_without_a_secret_store_says_so() {
        let fx = Fixture::start().await;
        let error = within(fx.client.list_secrets()).await.unwrap_err();
        assert_eq!(error.code(), Some(ErrorCode::Unavailable));
        let error = within(fx.client.set_secret("A_TOKEN", "some-value")).await.unwrap_err();
        assert_eq!(error.code(), Some(ErrorCode::Unavailable));
    }
}

daemon_test! {
    async fn a_run_needs_its_secret_set_and_then_gets_it_and_the_journal_never_shows_it() {
        let (_, secrets) = keychain();
        // The agent prints the token it was given, in text, in a tool call and in its result.
        let echo = MockExecutor::new(id("mock")).with_steps([
            MockStep::started("s", "mock-1"),
            MockStep::text(format!("my token is {TOKEN}")),
            MockStep::Emit(ExecEvent::Done {
                stop: StopReason::Completed,
                result: Some(format!("used {TOKEN}")),
                diagnostic: None,
                reason: None,
            }),
        ]);
        let log = echo.clone();
        let fx = Fixture::with(move |c| {
            c.secrets = Some(secrets);
            c.executors = ExecutorSource::Given(ExecutorSet::from_registry(
                ExecutorRegistry::new().with(Arc::new(echo)),
            ));
        })
        .await;
        write_bot(&fx, "keeper", "secrets = [\"GITHUB_TOKEN\"]").await;

        // Not set: the run fails before the executor starts, naming the secret.
        let refused = within(fx.client.run_task(bot_task("keeper"))).await.unwrap();
        let journal = events(&fx, &refused).await;
        let Some(ExecEvent::Done { stop, diagnostic, .. }) = journal.last() else {
            panic!("no Done: {journal:?}");
        };
        assert_eq!(*stop, StopReason::Error);
        let why = diagnostic.clone().unwrap();
        assert!(why.contains("GITHUB_TOKEN") && why.contains("not set"), "{why}");
        assert!(matches!(
            journal.last(),
            Some(ExecEvent::Done {
                reason: Some(reno_proto::StopDetail::StartRefused {
                    failure: reno_proto::StartRefusal::SecretMissing,
                    ..
                }),
                ..
            })
        ), "{journal:?}");
        assert_eq!(fx.status(&refused).await, RunStatus::Failed);
        assert!(log.log().specs.is_empty());

        // Set: the run starts with it in its environment, and nothing a client reads has it.
        within(fx.client.set_secret("GITHUB_TOKEN", TOKEN)).await.unwrap();
        let run = within(fx.client.run_task(bot_task("keeper"))).await.unwrap();
        let journal = events(&fx, &run).await;
        assert_eq!(fx.status(&run).await, RunStatus::Succeeded);
        assert_eq!(log.log().specs[0].env["GITHUB_TOKEN"], TOKEN);
        let shown = serde_json::to_string(&journal).unwrap();
        assert!(!shown.contains(TOKEN), "{shown}");
        assert!(shown.contains("[secret:GITHUB_TOKEN]"), "{shown}");
        let summary = format!("{:?}", within(fx.client.get_run(&run)).await.unwrap());
        assert!(!summary.contains(TOKEN));
    }
}
