#![cfg(test)]

//! Secrets (T1.13): a bot's named secrets reach its run's environment, a missing one stops the
//! run before anything is spawned, and the values never reach the journal.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use futures::stream::BoxStream;
use reno_harness::exec::{ExecControls, ExecError, Executor, MockExecutor, MockStep};
use reno_proto::defs::DefKind;
use reno_proto::{
    ActionClass, ExecEvent, ExecSpec, HealthProbe, RunId, RunStatus, StopReason, ToolCall,
};

use super::defs::{FakePort, file, setup};
use super::*;
use crate::{FakeSecrets, Secrets};

const TOKEN: &str = "ghp_exampleExampleExample1234";
const OTHER: &str = "other-secret-value-5678";

fn bot_with_secrets(secrets: &str) -> reno_proto::defs::DefFile {
    file(
        DefKind::Bot,
        "keeper",
        &format!("name = \"keeper\"\ninstructions = \"x\"\nexecutors = [\"mock\"]\n{secrets}\n"),
    )
}

async fn fx_with(
    bot: reno_proto::defs::DefFile,
    secrets: Option<Arc<FakeSecrets>>,
    mock: &MockExecutor,
) -> Fx {
    let port = FakePort::with(vec![bot]);
    let mut definitions = setup(&port);
    if let Some(secrets) = secrets {
        definitions = definitions.with_secrets(secrets);
    }
    Fx::with_definitions(config(1), registry(std::slice::from_ref(mock)), definitions).await
}

async fn go(fx: &Fx) -> RunId {
    fx.kernel
        .submit_for_bot("keeper", "go", None, PathBuf::from("/work"), None)
        .await
        .unwrap()
}

async fn done_diagnostic(fx: &Fx, run: &RunId) -> String {
    match fx.journal_with_done(run).await.last().unwrap().1.clone() {
        ExecEvent::Done { diagnostic, .. } => diagnostic.unwrap_or_default(),
        other => panic!("not a Done: {other:?}"),
    }
}

#[tokio::test(start_paused = true)]
async fn a_bots_secrets_reach_its_run_as_environment_variables_and_no_others_do() {
    let secrets = Arc::new(
        FakeSecrets::new()
            .with("GITHUB_TOKEN", TOKEN)
            .with("UNLISTED_TOKEN", OTHER),
    );
    let m = mock([]);
    let fx = fx_with(
        bot_with_secrets("secrets = [\"GITHUB_TOKEN\"]"),
        Some(secrets.clone()),
        &m,
    )
    .await;

    let run = go(&fx).await;
    fx.wait_status(&run, RunStatus::Succeeded).await;

    let env = m.log().specs[0].env.clone();
    let want: BTreeMap<String, String> = [
        ("GITHUB_TOKEN", TOKEN),
        ("RENO_BOT", "keeper"),
        ("RENO_RUN_ID", run.as_str()),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    assert_eq!(env, want);
    assert_eq!(secrets.asked(), [vec!["GITHUB_TOKEN".to_owned()]]);
}

#[tokio::test(start_paused = true)]
async fn a_bot_without_secrets_never_asks_the_store() {
    let secrets = Arc::new(FakeSecrets::new().with("GITHUB_TOKEN", TOKEN));
    let m = mock([]);
    let fx = fx_with(bot_with_secrets(""), Some(secrets.clone()), &m).await;

    let run = go(&fx).await;
    fx.wait_status(&run, RunStatus::Succeeded).await;

    assert!(secrets.asked().is_empty());
    let env = m.log().specs[0].env.clone();
    assert_eq!(
        env.keys().map(String::as_str).collect::<Vec<_>>(),
        ["RENO_BOT", "RENO_RUN_ID"]
    );
}

#[tokio::test(start_paused = true)]
async fn a_missing_secret_fails_the_run_before_anything_is_spawned_and_names_it() {
    // One of the two is set: the diagnostic names the other, and shows no value.
    let secrets = Arc::new(FakeSecrets::new().with("NPM_TOKEN", OTHER));
    let m = mock([]);
    let fx = fx_with(
        bot_with_secrets("secrets = [\"GITHUB_TOKEN\", \"NPM_TOKEN\"]"),
        Some(secrets),
        &m,
    )
    .await;

    let run = go(&fx).await;
    fx.wait_status(&run, RunStatus::Failed).await;

    let why = done_diagnostic(&fx, &run).await;
    assert!(why.starts_with("SecretMissing:"), "{why}");
    assert!(why.contains("GITHUB_TOKEN"), "{why}");
    assert!(
        !why.contains("NPM_TOKEN"),
        "a set secret is not reported: {why}"
    );
    assert!(!why.contains(OTHER), "{why}");
    assert!(m.log().specs.is_empty(), "the executor was never started");
    let stop = match fx.journal_with_done(&run).await.last().unwrap().1.clone() {
        ExecEvent::Done { stop, .. } => stop,
        other => panic!("not a Done: {other:?}"),
    };
    assert_eq!(stop, StopReason::Error);
}

#[tokio::test(start_paused = true)]
async fn a_daemon_with_no_secret_store_treats_every_listed_secret_as_missing() {
    let m = mock([]);
    let fx = fx_with(bot_with_secrets("secrets = [\"GITHUB_TOKEN\"]"), None, &m).await;

    let run = go(&fx).await;
    fx.wait_status(&run, RunStatus::Failed).await;

    let why = done_diagnostic(&fx, &run).await;
    assert!(why.starts_with("SecretMissing:"), "{why}");
    assert!(why.contains("GITHUB_TOKEN"), "{why}");
    assert!(m.log().specs.is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_store_that_cannot_be_read_fails_the_run_without_a_value_in_the_message() {
    let secrets = Arc::new(FakeSecrets::new().with("GITHUB_TOKEN", TOKEN));
    secrets.set_unavailable(Some("the keychain is locked"));
    let m = mock([]);
    let fx = fx_with(
        bot_with_secrets("secrets = [\"GITHUB_TOKEN\"]"),
        Some(secrets),
        &m,
    )
    .await;

    let run = go(&fx).await;
    fx.wait_status(&run, RunStatus::Failed).await;

    let why = done_diagnostic(&fx, &run).await;
    assert!(why.starts_with("SecretsUnavailable:"), "{why}");
    assert!(why.contains("keychain is locked"), "{why}");
    assert!(!why.contains(TOKEN), "{why}");
    assert!(m.log().specs.is_empty());
}

#[tokio::test(start_paused = true)]
async fn the_values_are_redacted_from_everything_the_run_journals() {
    let secrets = Arc::new(FakeSecrets::new().with("GITHUB_TOKEN", TOKEN));
    let m = mock([
        MockStep::text(format!("the token is {TOKEN}")),
        MockStep::Emit(ExecEvent::Tool {
            id: "t1".into(),
            call: ToolCall::new(
                "Bash",
                serde_json::json!({"command": format!("curl -H 'Authorization: token {TOKEN}'")}),
                ActionClass::Command,
            ),
        }),
        MockStep::Emit(ExecEvent::Done {
            stop: StopReason::Error,
            result: Some(format!("result {TOKEN}")),
            diagnostic: Some(format!("failed using {TOKEN}")),
        }),
    ]);
    let fx = fx_with(
        bot_with_secrets("secrets = [\"GITHUB_TOKEN\"]"),
        Some(secrets),
        &m,
    )
    .await;

    let run = go(&fx).await;
    fx.wait_status(&run, RunStatus::Failed).await;

    let journal = fx.journal_with_done(&run).await;
    let shown = serde_json::to_string(&journal).unwrap();
    assert!(!shown.contains(TOKEN), "{shown}");
    assert_eq!(shown.matches("[secret:GITHUB_TOKEN]").count(), 4, "{shown}");
}

#[tokio::test(start_paused = true)]
async fn a_run_without_secrets_is_not_touched_by_redaction() {
    let m = mock([MockStep::text("plain text")]);
    let fx = fx_with(bot_with_secrets(""), None, &m).await;
    let run = go(&fx).await;
    fx.wait_status(&run, RunStatus::Succeeded).await;
    let journal = fx.journal_with_done(&run).await;
    assert!(
        journal
            .iter()
            .any(|(_, e)| matches!(e, ExecEvent::Text { text } if text == "plain text"))
    );
}

#[tokio::test(start_paused = true)]
async fn the_fake_port_is_the_only_thing_that_holds_values() {
    // The kernel's own state (the store) holds names in a definition and never a value.
    let secrets = Arc::new(FakeSecrets::new().with("GITHUB_TOKEN", TOKEN));
    let m = mock([]);
    let fx = fx_with(
        bot_with_secrets("secrets = [\"GITHUB_TOKEN\"]"),
        Some(secrets.clone() as Arc<FakeSecrets>),
        &m,
    )
    .await;
    let run = go(&fx).await;
    fx.wait_status(&run, RunStatus::Succeeded).await;
    let events = fx
        .store
        .events_since(reno_proto::Seq(0), 100_000)
        .await
        .unwrap();
    assert!(!format!("{events:?}").contains(TOKEN));
    let rows = fx.store.list_runs(None).await.unwrap();
    assert!(!format!("{rows:?}").contains(TOKEN));
    assert!(
        secrets
            .names()
            .await
            .unwrap()
            .contains(&"GITHUB_TOKEN".to_owned())
    );
}

/// An executor that already holds the run's environment and refuses with a message that echoes
/// it, as a CLI that fails on a bad credential might.
#[derive(Debug)]
struct Echoing {
    inner: MockExecutor,
    sign_in: bool,
}

#[async_trait]
impl Executor for Echoing {
    fn descriptor(&self) -> reno_proto::ExecutorDescriptor {
        self.inner.descriptor()
    }
    async fn probe(&self) -> HealthProbe {
        self.inner.probe().await
    }
    async fn start(
        &self,
        spec: ExecSpec,
        _: ExecControls,
    ) -> Result<BoxStream<'static, ExecEvent>, ExecError> {
        let value = spec.env["GITHUB_TOKEN"].clone();
        Err(if self.sign_in {
            ExecError::NotSignedIn(format!("rejected credential {value}"))
        } else {
            ExecError::InvalidSpec(format!("GITHUB_TOKEN={value} is not accepted"))
        })
    }
}

#[tokio::test(start_paused = true)]
async fn what_an_executor_refuses_with_is_redacted_before_it_is_journaled_or_stored() {
    for sign_in in [false, true] {
        let secrets = Arc::new(FakeSecrets::new().with("GITHUB_TOKEN", TOKEN));
        let echoing = Echoing {
            inner: mock([]),
            sign_in,
        };
        let port = FakePort::with(vec![bot_with_secrets("secrets = [\"GITHUB_TOKEN\"]")]);
        let fx = Fx::with_definitions(
            config(1),
            crate::ExecutorRegistry::new().with(Arc::new(echoing)),
            setup(&port).with_secrets(secrets),
        )
        .await;

        let run = go(&fx).await;
        fx.wait_status(&run, RunStatus::Failed).await;

        let why = done_diagnostic(&fx, &run).await;
        assert!(!why.contains(TOKEN), "{why}");
        assert!(why.contains("[secret:GITHUB_TOKEN]"), "{why}");
        // The stored routing decision carries the refusal too.
        let routing = fx.kernel.run_routing(&run).await.unwrap();
        let shown = format!("{routing:?}");
        assert!(!shown.contains(TOKEN), "{shown}");
        let events = fx
            .store
            .events_since(reno_proto::Seq(0), 100_000)
            .await
            .unwrap();
        assert!(!format!("{events:?}").contains(TOKEN));
    }
}
