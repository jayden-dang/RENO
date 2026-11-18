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

/// The run's `Done` carries the typed refusal, so no client reads the diagnostic's text.
async fn assert_secret_missing(fx: &Fx, run: &RunId) {
    match fx.journal_with_done(run).await.last().unwrap().1.clone() {
        ExecEvent::Done {
            reason: Some(reno_proto::StopDetail::StartRefused { failure, .. }),
            ..
        } => assert_eq!(failure, reno_proto::StartRefusal::SecretMissing),
        other => panic!("not a typed secret refusal: {other:?}"),
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
    assert_secret_missing(&fx, &run).await;
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
    assert_secret_missing(&fx, &run).await;
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
            reason: None,
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

#[tokio::test(start_paused = true)]
async fn a_resumed_run_gets_its_secrets_again_and_stays_redacted() {
    // A resume is a new start: the bot's secrets are resolved again, and the second attempt's
    // events are redacted like the first's.
    let secrets = Arc::new(FakeSecrets::new().with("GITHUB_TOKEN", TOKEN));
    let m = mock([
        MockStep::started("sess-1", "mock-1"),
        MockStep::text(format!("token {TOKEN}")),
        // The executor stops by itself: interrupted, and a person can resume it.
        MockStep::Emit(ExecEvent::Done {
            stop: StopReason::Interrupted,
            result: None,
            diagnostic: None,
            reason: None,
        }),
    ]);
    let fx = fx_with(
        bot_with_secrets("secrets = [\"GITHUB_TOKEN\"]"),
        Some(secrets.clone()),
        &m,
    )
    .await;
    let run = go(&fx).await;
    until(async || {
        fx.store
            .get_run(&run)
            .await
            .unwrap()
            .and_then(|row| row.session_ref)
    })
    .await;
    fx.wait_status(&run, RunStatus::Interrupted).await;
    fx.journal_with_done(&run).await;

    assert_eq!(fx.kernel.resume_run(&run, None).await.unwrap(), 2);
    until(async || (m.log().specs.len() == 2).then_some(())).await;
    let specs = m.log().specs;
    assert!(specs[1].resume.is_some());
    assert_eq!(specs[1].env["GITHUB_TOKEN"], TOKEN);
    assert_eq!(specs[1].env["RENO_RUN_ID"], run.as_str());
    assert_eq!(secrets.asked().len(), 2, "resolved once per attempt");

    let shown = serde_json::to_string(&fx.journal_with_done(&run).await).unwrap();
    assert!(!shown.contains(TOKEN), "{shown}");
}

#[tokio::test(start_paused = true)]
async fn a_secret_in_a_tools_output_is_redacted_before_it_is_journaled() {
    let secrets = Arc::new(FakeSecrets::new().with("GITHUB_TOKEN", TOKEN));
    let m = mock([
        MockStep::Emit(ExecEvent::ToolResult {
            id: "t1".into(),
            ok: true,
            summary: Some(format!("last line {TOKEN}")),
            output: Some(reno_proto::ToolOutput::from_text(&format!(
                "GITHUB_TOKEN={TOKEN}\nmore"
            ))),
        }),
        MockStep::Emit(ExecEvent::Done {
            stop: StopReason::Completed,
            result: None,
            diagnostic: None,
            reason: None,
        }),
    ]);
    let fx = fx_with(
        bot_with_secrets("secrets = [\"GITHUB_TOKEN\"]"),
        Some(secrets),
        &m,
    )
    .await;

    let run = go(&fx).await;
    fx.wait_status(&run, RunStatus::Succeeded).await;

    let journal = fx.journal_with_done(&run).await;
    let shown = serde_json::to_string(&journal).unwrap();
    assert!(!shown.contains(TOKEN), "{shown}");
    assert!(
        shown.contains("GITHUB_TOKEN=[secret:GITHUB_TOKEN]\\nmore"),
        "{shown}"
    );
}

// ---- a value split across streamed events ----

const PROBE: &str = "sk-probe-9f3a7c21d4e85b60";

fn text_of(journal: &[(reno_proto::Seq, ExecEvent)]) -> String {
    journal
        .iter()
        .filter_map(|(_, e)| match e {
            ExecEvent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Runs `steps` with `PROBE_TOKEN` as the bot's secret, to the end. Returns the journal and what
/// a subscriber saw.
async fn run_streamed(steps: Vec<MockStep>) -> (Vec<(reno_proto::Seq, ExecEvent)>, Vec<ExecEvent>) {
    let secrets = Arc::new(FakeSecrets::new().with("PROBE_TOKEN", PROBE));
    let m = mock(steps);
    let fx = fx_with(
        bot_with_secrets("secrets = [\"PROBE_TOKEN\"]"),
        Some(secrets),
        &m,
    )
    .await;
    let run = go(&fx).await;
    let live: Vec<ExecEvent> = fx
        .events(&run, None)
        .await
        .into_iter()
        .map(|item| item.unwrap().event)
        .collect();
    let journal = fx.journal_with_done(&run).await;
    (journal, live)
}

fn assert_no_secret(journal: &[(reno_proto::Seq, ExecEvent)], live: &[ExecEvent]) {
    let shown = serde_json::to_string(journal).unwrap();
    assert!(!shown.contains(PROBE), "{shown}");
    let seen = serde_json::to_string(live).unwrap();
    assert!(!seen.contains(PROBE), "{seen}");
    let joined: String = live
        .iter()
        .filter_map(|e| match e {
            ExecEvent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(!joined.contains(PROBE), "{joined}");
    assert!(!text_of(journal).contains(PROBE));
}

#[tokio::test(start_paused = true)]
async fn the_live_probe_split_across_text_events_is_redacted() {
    let (journal, live) = run_streamed(vec![
        MockStep::text("token="),
        MockStep::text("sk-probe-9f"),
        MockStep::text("3a7c21d4e85b60\n"),
    ])
    .await;
    assert_no_secret(&journal, &live);
    assert_eq!(text_of(&journal), "token=[secret:PROBE_TOKEN]\n");
}

#[tokio::test(start_paused = true)]
async fn a_secret_split_at_every_offset_or_one_char_at_a_time_never_reaches_the_journal() {
    let whole = format!("say {PROBE} ok");
    for cut in 0..=whole.len() {
        let (a, b) = whole.split_at(cut);
        let (journal, live) = run_streamed(vec![MockStep::text(a), MockStep::text(b)]).await;
        assert_no_secret(&journal, &live);
        assert_eq!(
            text_of(&journal),
            "say [secret:PROBE_TOKEN] ok",
            "cut {cut}"
        );
    }
    let chars: Vec<MockStep> = whole
        .chars()
        .map(|c| MockStep::text(c.to_string()))
        .collect();
    let (journal, live) = run_streamed(chars).await;
    assert_no_secret(&journal, &live);
    assert_eq!(text_of(&journal), "say [secret:PROBE_TOKEN] ok");
}

#[tokio::test(start_paused = true)]
async fn a_held_prefix_is_journaled_before_the_tool_that_follows_it() {
    let tool = ExecEvent::Tool {
        id: "t1".into(),
        call: ToolCall::new(
            "Bash",
            serde_json::json!({"command": "ls"}),
            ActionClass::Command,
        ),
    };
    let (journal, _) = run_streamed(vec![
        MockStep::text("see sk-probe-9f"),
        MockStep::Emit(tool.clone()),
        MockStep::text("after"),
    ])
    .await;
    let kinds: Vec<&ExecEvent> = journal.iter().map(|(_, e)| e).collect();
    let tool_at = kinds.iter().position(|e| **e == tool).unwrap();
    assert_eq!(text_of(&journal[..tool_at]), "see sk-probe-9f");
    assert_eq!(text_of(&journal[tool_at..]), "after");
}

#[tokio::test(start_paused = true)]
async fn a_held_prefix_is_journaled_when_the_run_ends() {
    let (journal, _) = run_streamed(vec![MockStep::text("tail sk-probe-9f3a")]).await;
    assert_eq!(text_of(&journal), "tail sk-probe-9f3a");
    assert!(matches!(journal.last().unwrap().1, ExecEvent::Done { .. }));
}

#[tokio::test(start_paused = true)]
async fn a_held_prefix_is_journaled_after_the_quiet_timer() {
    let secrets = Arc::new(FakeSecrets::new().with("PROBE_TOKEN", PROBE));
    let m = mock([
        MockStep::text("wait sk-probe"),
        MockStep::Sleep(std::time::Duration::from_secs(3600)),
    ]);
    let fx = fx_with(
        bot_with_secrets("secrets = [\"PROBE_TOKEN\"]"),
        Some(secrets),
        &m,
    )
    .await;
    let run = go(&fx).await;
    // Only the safe part first; the tail follows once the run has been quiet.
    let journal = until(async || {
        let journal = fx.journal(&run);
        (text_of(&journal) == "wait sk-probe").then_some(journal)
    })
    .await;
    assert!(!matches!(journal.last().unwrap().1, ExecEvent::Done { .. }));
    assert_eq!(
        journal
            .iter()
            .filter(|(_, e)| matches!(e, ExecEvent::Text { .. }))
            .count(),
        2
    );
}

#[tokio::test(start_paused = true)]
async fn a_lookalike_that_never_completes_is_journaled_intact() {
    let (journal, live) = run_streamed(vec![
        MockStep::text("a sk-probe-9f"),
        MockStep::text("zzz b sk-pr"),
    ])
    .await;
    assert_no_secret(&journal, &live);
    assert_eq!(text_of(&journal), "a sk-probe-9fzzz b sk-pr");
}

#[tokio::test(start_paused = true)]
async fn text_that_cannot_start_a_secret_is_journaled_at_once() {
    let (journal, _) = run_streamed(vec![MockStep::text("one"), MockStep::text("two")]).await;
    let texts: Vec<&str> = journal
        .iter()
        .filter_map(|(_, e)| match e {
            ExecEvent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["one", "two"]);
}

#[tokio::test(start_paused = true)]
async fn a_json_escaped_secret_split_across_events_is_redacted() {
    let key = "line-one-secret\nline-two-secret";
    let secrets = Arc::new(FakeSecrets::new().with("PROBE_TOKEN", key));
    let m = mock([
        MockStep::text(r"k: line-one-sec"),
        MockStep::text(r"ret\nline-two"),
        MockStep::text(r"-secret!"),
    ]);
    let fx = fx_with(
        bot_with_secrets("secrets = [\"PROBE_TOKEN\"]"),
        Some(secrets),
        &m,
    )
    .await;
    let run = go(&fx).await;
    let journal = fx.journal_with_done(&run).await;
    assert_eq!(text_of(&journal), "k: [secret:PROBE_TOKEN]!");
}

#[tokio::test(start_paused = true)]
async fn random_chunkings_of_a_secret_never_leak() {
    let whole = format!("a {PROBE} b {PROBE}{PROBE} sk-probe c");
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;
    for _ in 0..20 {
        let mut steps = Vec::new();
        let mut at = 0;
        while at < whole.len() {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let end = (at + 1 + (seed % 11) as usize).min(whole.len());
            steps.push(MockStep::text(&whole[at..end]));
            at = end;
        }
        let (journal, live) = run_streamed(steps).await;
        assert_no_secret(&journal, &live);
        assert_eq!(
            text_of(&journal),
            "a [secret:PROBE_TOKEN] b [secret:PROBE_TOKEN][secret:PROBE_TOKEN] sk-probe c"
        );
    }
}
