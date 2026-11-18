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

/// The journal's tool outputs of `run`.
fn journaled_outputs(journal: &[(reno_proto::Seq, ExecEvent)]) -> Vec<reno_proto::ToolOutput> {
    journal
        .iter()
        .filter_map(|(_, event)| match event {
            ExecEvent::ToolResult { output, .. } => output.clone(),
            _ => None,
        })
        .collect()
}

/// No window of `needle` of `min` bytes survives in `shown`.
fn no_fragment(shown: &str, needle: &str, min: usize) {
    for window in needle.as_bytes().windows(min) {
        let window = std::str::from_utf8(window).unwrap();
        assert!(!shown.contains(window), "fragment {window:?} leaked");
    }
}

/// `before` bytes of filler, the token, then `after` bytes of filler.
fn around(before: usize, after: usize) -> String {
    format!("{}{TOKEN}{}", "a".repeat(before), "b".repeat(after))
}

#[tokio::test(start_paused = true)]
async fn a_secret_across_the_journal_cut_leaves_no_fragment() {
    let half = reno_proto::ToolOutput::MAX_BYTES / 2;
    let raw_half = reno_proto::ToolOutput::RAW_MAX_BYTES / 2;
    let texts = [
        // Across the end of the journal's head.
        around(half - 10, 20_000),
        // Across the start of the journal's tail.
        around(20_000, half - 15),
        // Across the end of the raw head and the start of the raw tail, in a huge output.
        around(raw_half - 10, 1_500_000),
        around(1_500_000, raw_half - 15),
    ];
    let mut steps = Vec::new();
    for (n, text) in texts.iter().enumerate() {
        steps.push(MockStep::Emit(ExecEvent::ToolResult {
            id: format!("t{n}"),
            ok: true,
            summary: None,
            // What the drivers hand over: the raw cut, no journal cut.
            output: Some(reno_proto::ToolOutput::from_raw(text)),
        }));
    }
    steps.push(MockStep::Emit(ExecEvent::Done {
        stop: StopReason::Completed,
        result: None,
        diagnostic: None,
        reason: None,
    }));
    let m = mock(steps);
    let secrets = Arc::new(FakeSecrets::new().with("GITHUB_TOKEN", TOKEN));
    let fx = fx_with(
        bot_with_secrets("secrets = [\"GITHUB_TOKEN\"]"),
        Some(secrets),
        &m,
    )
    .await;
    let run = go(&fx).await;
    fx.wait_status(&run, RunStatus::Succeeded).await;

    let journal = fx.journal_with_done(&run).await;
    let outputs = journaled_outputs(&journal);
    assert_eq!(outputs.len(), texts.len());
    for (output, text) in outputs.iter().zip(&texts) {
        assert_eq!(output.bytes, text.len() as u64, "bytes is the real size");
        assert!(output.truncated);
        assert!(output.text.len() <= reno_proto::ToolOutput::MAX_BYTES + 40);
        no_fragment(&output.text, TOKEN, 6);
    }
    let shown = serde_json::to_string(&journal).unwrap();
    no_fragment(&shown, TOKEN, 6);

    // The same texts, cut at the journal limit before redaction (the old order), would have
    // left a fragment: this is what the test is for.
    let old = reno_proto::ToolOutput::from_text(&texts[0]);
    assert!(
        old.text.contains(&TOKEN[..10]),
        "the fixture straddles the cut"
    );
}

#[tokio::test(start_paused = true)]
async fn a_tool_output_is_cut_for_the_journal_even_when_the_run_has_no_secrets() {
    let big = "q".repeat(300_000);
    let m = mock([
        MockStep::Emit(ExecEvent::ToolResult {
            id: "t1".into(),
            ok: true,
            summary: None,
            output: Some(reno_proto::ToolOutput::from_raw(&big)),
        }),
        MockStep::Emit(ExecEvent::Done {
            stop: StopReason::Completed,
            result: None,
            diagnostic: None,
            reason: None,
        }),
    ]);
    let fx = fx_with(bot_with_secrets(""), None, &m).await;
    let run = go(&fx).await;
    fx.wait_status(&run, RunStatus::Succeeded).await;
    let outputs = journaled_outputs(&fx.journal_with_done(&run).await);
    assert_eq!(outputs.len(), 1);
    assert_eq!(outputs[0].bytes, 300_000);
    assert!(outputs[0].text.len() <= reno_proto::ToolOutput::MAX_BYTES + 40);
}

#[tokio::test(start_paused = true)]
async fn a_workspace_patch_and_paths_are_redacted_and_a_cut_value_is_dropped() {
    use crate::FakeWorkspace;
    let ws = Arc::new(FakeWorkspace::new().with_repo("/repo"));
    let m = mock([MockStep::Sleep(std::time::Duration::from_secs(5))]);
    let secrets = Arc::new(FakeSecrets::new().with("GITHUB_TOKEN", TOKEN));
    let port = FakePort::with(vec![bot_with_secrets("secrets = [\"GITHUB_TOKEN\"]")]);
    let definitions = setup(&port).with_secrets(secrets.clone());
    let fx = Fx::with_workspace(
        config(1),
        registry(std::slice::from_ref(&m)),
        Some(definitions),
        ws.clone(),
    )
    .await;
    let run = fx
        .kernel
        .submit_for_bot("keeper", "go", None, PathBuf::from("/repo"), None)
        .await
        .unwrap();
    fx.wait_status(&run, RunStatus::Running).await;
    until(async || ws.exists(&run).then_some(())).await;
    let file = |path: &str| reno_proto::ChangedFile {
        path: path.into(),
        from: Some(format!("old-{TOKEN}")),
        kind: reno_proto::ChangeKind::Modified,
        additions: 1,
        deletions: 0,
        binary: false,
    };
    ws.set_diff(
        &run,
        crate::RunChanges {
            commits: 1,
            files: vec![file(&format!("env-{TOKEN}.txt"))],
            patch: Some(format!("+GITHUB_TOKEN={TOKEN}\n+more {}", &TOKEN[..12])),
            patch_truncated: true,
        },
    );
    fx.wait_status(&run, RunStatus::Succeeded).await;

    let got = fx.kernel.workspace_diff(&run, Some("a")).await.unwrap();
    let shown = serde_json::to_string(&got).unwrap();
    assert!(!shown.contains(TOKEN), "{shown}");
    no_fragment(&shown, TOKEN, 6);
    assert_eq!(
        got.patch.as_deref(),
        Some("+GITHUB_TOKEN=[secret:GITHUB_TOKEN]\n+more ")
    );
    assert_eq!(got.files[0].path, "env-[secret:GITHUB_TOKEN].txt");
    assert_eq!(
        got.files[0].from.as_deref(),
        Some("old-[secret:GITHUB_TOKEN]")
    );

    // The values cannot be read now: the patch is withheld rather than shown raw.
    secrets.set_unavailable(Some("locked"));
    let got = fx.kernel.workspace_diff(&run, Some("a")).await.unwrap();
    assert_eq!(got.patch, None);
}

#[tokio::test(start_paused = true)]
async fn what_the_run_says_through_its_own_mcp_tools_is_redacted_too() {
    use reno_proto::{AgentApprovalParams, AgentReportParams, ReportStatus, token_digest};
    let m = mock([
        MockStep::started("s", "mock-1"),
        MockStep::AwaitSteer,
        MockStep::text("after"),
    ]);
    let link = Arc::new(crate::FakeRunLink::new());
    let secrets = Arc::new(FakeSecrets::new().with("GITHUB_TOKEN", TOKEN));
    let port = FakePort::with(vec![bot_with_secrets("secrets = [\"GITHUB_TOKEN\"]")]);
    let definitions = setup(&port).with_secrets(secrets);
    let fx = Fx::with_link(
        config(1),
        registry(std::slice::from_ref(&m)),
        Some(definitions),
        link.clone(),
    )
    .await;
    let run = go(&fx).await;
    until(async || (!m.log().specs.is_empty()).then_some(())).await;
    let session = fx
        .kernel
        .agent_authenticate(&run, &token_digest(&link.token_for(&run).unwrap()))
        .await
        .unwrap();

    fx.kernel
        .agent_report(
            &session,
            AgentReportParams {
                status: ReportStatus::Blocked,
                summary: format!("token {TOKEN} was rejected"),
                details: Some(format!("tried {TOKEN}")),
            },
        )
        .await
        .unwrap();
    let kernel = fx.kernel.clone();
    let asking = session.clone();
    let waiting = tokio::spawn(async move {
        kernel
            .agent_request_approval(
                &asking,
                AgentApprovalParams {
                    action: ActionClass::Command,
                    reason: format!("uses {TOKEN}"),
                    command: Some(format!("curl -H 'Authorization: {TOKEN}' x")),
                    paths: vec![format!("{TOKEN}.txt")],
                },
            )
            .await
    });
    let held = until(async || {
        fx.kernel
            .pending_approvals()
            .await
            .unwrap()
            .into_iter()
            .next()
    })
    .await;
    let shown = serde_json::to_string(&held).unwrap();
    assert!(!shown.contains(TOKEN), "{shown}");
    assert!(shown.contains("[secret:GITHUB_TOKEN]"), "{shown}");
    let summary = serde_json::to_string(&fx.kernel.get_run(&run).await.unwrap()).unwrap();
    assert!(!summary.contains(TOKEN), "{summary}");

    fx.kernel
        .decide(&held.id, reno_proto::ApprovalDecision::ApproveOnce)
        .await
        .unwrap();
    waiting.await.unwrap().unwrap();
    fx.kernel.steer(&run, "go on").await.unwrap();
    fx.wait_status(&run, RunStatus::Succeeded).await;
    let journal = serde_json::to_string(&fx.journal_with_done(&run).await).unwrap();
    assert!(!journal.contains(TOKEN), "{journal}");
    assert!(
        journal.contains("token [secret:GITHUB_TOKEN] was rejected"),
        "{journal}"
    );
}
