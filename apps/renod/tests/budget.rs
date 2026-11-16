//! Budgets (T1.7) through the daemon, served in-process over mock executors: the kill switch and
//! its persistence across a restart, the typed refusal of a bot out of tokens, and the
//! `budget.status` view. No network, no real harness (ARCH-16).

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use common::{Fixture, config, id, within};
use futures::StreamExt;
use reno_client::Client;
use reno_harness::exec::{MockExecutor, MockStep};
use reno_kernel::ExecutorRegistry;
use reno_proto::defs::DefKind;
use reno_proto::{
    BudgetKind, BudgetStop, ErrorCode, ExecEvent, RunId, RunStatus, StopReason, TaskRunParams,
};
use renod::{Daemon, ExecutorSet, ExecutorSource};

macro_rules! daemon_test {
    ($(#[$meta:meta])* async fn $name:ident() $body:block) => {
        $(#[$meta])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() $body
    };
}

fn bot_task(bot: &str) -> TaskRunParams {
    TaskRunParams {
        prompt: "go".into(),
        executor: None,
        bot: Some(bot.to_owned()),
        cwd: None,
        model: None,
        in_place: false,
        attachments: Vec::new(),
    }
}

async fn write_bot(fx: &Fixture, name: &str, budget: &str) {
    let mut watch = within(fx.client.watch_defs()).await.unwrap();
    let path = fx.paths.defs_dir.join(format!("bots/{name}.toml"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        path,
        format!(
            "name = \"{name}\"\ninstructions = \"x\"\nexecutors = [\"mock\"]\n[budget]\n{budget}\n"
        ),
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

async fn finished(client: &Client, run: &RunId) -> Vec<ExecEvent> {
    within(
        client
            .run_events(run.clone(), None)
            .map(|item| item.expect("event").event)
            .collect::<Vec<_>>(),
    )
    .await
}

daemon_test! {
    async fn a_pause_holds_runs_over_the_socket_and_survives_a_restart() {
        let fx = Fixture::start().await;
        write_bot(&fx, "keeper", "max_concurrent_runs = 2").await;

        let paused = within(fx.client.pause(None, false)).await.unwrap();
        assert!(paused.changed);
        let status = within(fx.client.budget_status()).await.unwrap();
        assert!(status.paused);

        let held = within(fx.client.run_task(bot_task("keeper"))).await.unwrap();
        // A round trip through the daemon after the submit: the dispatcher has had its chance.
        assert_eq!(within(fx.client.budget_status()).await.unwrap().bots.iter().find(|b| b.bot == "keeper").unwrap().queued_runs, 1);
        assert_eq!(fx.status(&held).await, RunStatus::Queued);

        // The daemon stops and starts again over the same data directory.
        let (dir, paths, stopped) = fx.stop().await;
        assert!(!stopped.kernel_aborted);
        let executors = common::Executors::new();
        let daemon = within(Daemon::start(config(&dir, &executors))).await.unwrap();
        let client = within(Client::connect_unix(&paths.socket)).await.unwrap();

        let status = within(client.budget_status()).await.unwrap();
        assert!(status.paused, "the pause outlived the daemon");
        assert_eq!(within(client.get_run(&held)).await.unwrap().status, RunStatus::Queued);

        let resumed = within(client.resume(None)).await.unwrap();
        assert!(resumed.changed);
        let events = finished(&client, &held).await;
        assert!(events.iter().any(|e| matches!(e, ExecEvent::Done { stop: StopReason::Completed, .. })));
        assert!(!within(client.budget_status()).await.unwrap().paused);
        // Resuming again says nothing changed.
        assert!(!within(client.resume(None)).await.unwrap().changed);

        drop(client);
        daemon.request_shutdown();
        within(daemon.wait()).await.unwrap();
    }
}

daemon_test! {
    async fn pausing_one_bot_leaves_the_others_running_and_an_unknown_bot_is_not_found() {
        let fx = Fixture::start().await;
        write_bot(&fx, "keeper", "max_concurrent_runs = 2").await;
        write_bot(&fx, "scribe", "max_concurrent_runs = 2").await;

        assert!(within(fx.client.pause(Some("keeper"), false)).await.unwrap().changed);
        let held = within(fx.client.run_task(bot_task("keeper"))).await.unwrap();
        let free = within(fx.client.run_task(bot_task("scribe"))).await.unwrap();
        finished(&fx.client, &free).await;
        assert_eq!(fx.status(&free).await, RunStatus::Succeeded);
        assert_eq!(fx.status(&held).await, RunStatus::Queued);

        let status = within(fx.client.budget_status()).await.unwrap();
        let keeper = status.bots.iter().find(|b| b.bot == "keeper").unwrap();
        assert!(keeper.paused && keeper.queued_runs == 1);
        assert!(!status.bots.iter().find(|b| b.bot == "scribe").unwrap().paused);

        let unknown = within(fx.client.pause(Some("nobody"), false)).await.unwrap_err();
        assert_eq!(unknown.code(), Some(ErrorCode::NotFound));

        assert!(within(fx.client.resume(Some("keeper"))).await.unwrap().changed);
        finished(&fx.client, &held).await;
        assert_eq!(fx.status(&held).await, RunStatus::Succeeded);
        fx.stop().await;
    }
}

daemon_test! {
    async fn a_bot_that_spends_its_ceiling_is_stopped_and_then_refused_with_a_typed_error() {
        let spender = MockExecutor::new(id("mock")).with_steps([
            MockStep::Emit(ExecEvent::Usage { input_tokens: 80, output_tokens: 40, cache_read_tokens: 0, cache_write_tokens: 0, }),
            MockStep::Sleep(std::time::Duration::from_secs(3600)),
        ]);
        let registry = ExecutorRegistry::new().with(Arc::new(spender));
        let fx = Fixture::with(|c| {
            c.executors = ExecutorSource::Given(ExecutorSet::from_registry(registry));
        })
        .await;
        write_bot(&fx, "keeper", "daily_token_ceiling = 100").await;

        let run = within(fx.client.run_task(bot_task("keeper"))).await.unwrap();
        let events = finished(&fx.client, &run).await;
        let Some(ExecEvent::Done { stop, diagnostic, reason, .. }) = events.last() else {
            panic!("no Done: {events:?}");
        };
        assert_eq!(*stop, StopReason::Interrupted);
        // The typed reason is the contract; the diagnostic still says the same for display.
        let budget = reason.as_ref().and_then(reno_proto::StopDetail::budget).unwrap();
        assert_eq!(BudgetStop::from_diagnostic(diagnostic.as_deref().unwrap()), Some(budget));
        let summary = within(fx.client.get_run(&run)).await.unwrap();
        assert_eq!(summary.stop.as_ref().and_then(reno_proto::StopDetail::budget), Some(budget));
        assert_eq!(
            (budget.kind, budget.limit, budget.used),
            (BudgetKind::Tokens, 100, 120)
        );
        assert_eq!(fx.status(&run).await, RunStatus::Interrupted);

        let refused = within(fx.client.run_task(bot_task("keeper"))).await.unwrap_err();
        assert_eq!(refused.code(), Some(ErrorCode::BudgetExceeded));
        let reno_client::ClientError::Remote(object) = refused else {
            panic!("not a daemon error");
        };
        let payload: reno_proto::BudgetRefusal =
            serde_json::from_value(object.data.expect("typed data")).unwrap();
        assert_eq!(payload.bot, "keeper");
        assert_eq!(payload.stop.kind, BudgetKind::Tokens);
        assert_eq!((payload.stop.limit, payload.stop.used), (100, 120));

        let status = within(fx.client.budget_status()).await.unwrap();
        let keeper = status.bots.iter().find(|b| b.bot == "keeper").unwrap();
        assert_eq!((keeper.tokens_today, keeper.daily_token_ceiling), (120, Some(100)));
        fx.stop().await;
    }
}
