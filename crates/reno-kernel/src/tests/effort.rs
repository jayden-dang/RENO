#![cfg(test)]

//! A run's reasoning effort: the task's, else the bot's, checked against what the executor's
//! model lists before anything starts, kept by a resumed attempt, and used by the router.

use std::path::PathBuf;

use reno_harness::exec::{MockExecutor, MockStep};
use reno_proto::defs::DefKind;
use reno_proto::{Effort, ModelInfo, RejectReason, RunStatus};

use super::defs::{FakePort, file, setup};
use super::*;
use crate::{KernelError, TaskRequest};

fn model(id: &str, efforts: &[Effort]) -> ModelInfo {
    ModelInfo {
        options: Vec::new(),
        id: id.into(),
        display_name: None,
        description: None,
        efforts: efforts.to_vec(),
        default_effort: None,
    }
}

/// A mock named `name` that lists `models` and answers to `aliases`.
fn lister(name: &str, models: Vec<ModelInfo>, aliases: &[&str]) -> MockExecutor {
    let m = MockExecutor::new(id(name)).with_steps([MockStep::text("hi")]);
    let mut descriptor = reno_harness::exec::Executor::descriptor(&m);
    descriptor.caps.models = models;
    descriptor.caps.model_aliases = aliases.iter().map(|a| (*a).to_owned()).collect();
    m.with_descriptor(descriptor)
}

fn request(executor: &str, model: Option<&str>, effort: Option<Effort>) -> TaskRequest {
    TaskRequest::new("go", PathBuf::from("/work"))
        .executor(id(executor))
        .model(model.map(str::to_owned))
        .effort(effort)
}

fn invalid(error: KernelError) -> String {
    match error {
        KernelError::Invalid(why) => why,
        other => panic!("not Invalid: {other:?}"),
    }
}

const LMH: [Effort; 3] = [Effort::Low, Effort::Medium, Effort::High];

#[tokio::test(start_paused = true)]
async fn the_tasks_effort_reaches_the_executor_and_the_summary() {
    let m = lister("mock", vec![model("mock-1", &LMH)], &[]);
    let fx = Fx::new(config(1), registry(std::slice::from_ref(&m))).await;
    let (run, _) = fx
        .kernel
        .submit_task(request("mock", Some("mock-1"), Some(Effort::High)))
        .await
        .unwrap();
    fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(m.log().specs[0].effort, Some(Effort::High));
    assert_eq!(
        fx.kernel.get_run(&run).await.unwrap().effort,
        Some(Effort::High)
    );
    let (listed, _) = fx.kernel.list_runs(None, None, None).await.unwrap();
    assert_eq!(listed[0].effort, Some(Effort::High));

    // No effort asked: none is passed, whatever the model lists.
    let (run, _) = fx
        .kernel
        .submit_task(request("mock", Some("mock-1"), None))
        .await
        .unwrap();
    fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(m.log().specs[1].effort, None);
    assert_eq!(fx.kernel.get_run(&run).await.unwrap().effort, None);
}

#[tokio::test(start_paused = true)]
async fn an_effort_the_model_does_not_list_is_refused_naming_the_model_and_its_levels() {
    let m = lister("mock", vec![model("mock-1", &LMH)], &[]);
    let fx = Fx::new(config(1), registry(std::slice::from_ref(&m))).await;
    let why = invalid(
        fx.kernel
            .submit_task(request("mock", Some("mock-1"), Some(Effort::XHigh)))
            .await
            .unwrap_err(),
    );
    assert!(why.contains("mock-1") && why.contains("xhigh"), "{why}");
    assert!(why.contains("low, medium, high"), "{why}");
    assert!(m.log().specs.is_empty());
    // Nothing was queued.
    assert!(
        fx.kernel
            .list_runs(None, None, None)
            .await
            .unwrap()
            .0
            .is_empty()
    );
}

#[tokio::test(start_paused = true)]
async fn a_model_that_lists_no_levels_takes_no_effort() {
    let m = lister("mock", vec![model("mock-1", &[])], &[]);
    let fx = Fx::new(config(1), registry(std::slice::from_ref(&m))).await;
    let why = invalid(
        fx.kernel
            .submit_task(request("mock", Some("mock-1"), Some(Effort::Low)))
            .await
            .unwrap_err(),
    );
    assert!(why.contains("mock-1") && why.contains("no effort"), "{why}");
    // A model the executor does not list at all: its levels are not known either.
    let why = invalid(
        fx.kernel
            .submit_task(request("mock", Some("mystery"), Some(Effort::Low)))
            .await
            .unwrap_err(),
    );
    assert!(why.contains("mystery"), "{why}");
}

#[tokio::test(start_paused = true)]
async fn an_alias_is_checked_against_the_model_it_names() {
    let m = lister(
        "mock",
        vec![
            model("claude-opus-9", &[Effort::Low, Effort::Max]),
            model("claude-haiku-9", &[]),
        ],
        &["opus", "haiku"],
    );
    let fx = Fx::new(config(1), registry(std::slice::from_ref(&m))).await;
    let (run, _) = fx
        .kernel
        .submit_task(request("mock", Some("opus[1m]"), Some(Effort::Max)))
        .await
        .unwrap();
    fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(m.log().specs[0].effort, Some(Effort::Max));
    let why = invalid(
        fx.kernel
            .submit_task(request("mock", Some("haiku"), Some(Effort::Low)))
            .await
            .unwrap_err(),
    );
    assert!(why.contains("claude-haiku-9"), "{why}");
}

#[tokio::test(start_paused = true)]
async fn with_no_model_named_some_listed_model_must_take_the_effort() {
    let m = lister(
        "mock",
        vec![
            model("small", &[Effort::Low]),
            model("big", &[Effort::High, Effort::XHigh]),
        ],
        &[],
    );
    let fx = Fx::new(config(1), registry(std::slice::from_ref(&m))).await;
    let (run, _) = fx
        .kernel
        .submit_task(request("mock", None, Some(Effort::XHigh)))
        .await
        .unwrap();
    fx.wait_status(&run, RunStatus::Succeeded).await;
    let why = invalid(
        fx.kernel
            .submit_task(request("mock", None, Some(Effort::Max)))
            .await
            .unwrap_err(),
    );
    assert!(
        why.contains("max") && why.contains("big: levels high, xhigh"),
        "{why}"
    );
}

fn bot_with(extra: &str) -> String {
    format!("name = \"keeper\"\ninstructions = \"x\"\nexecutors = [\"mock\"]\n{extra}")
}

async fn fx_bot(m: &MockExecutor, extra: &str) -> Fx {
    let port = FakePort::with(vec![file(DefKind::Bot, "keeper", &bot_with(extra))]);
    Fx::with_definitions(config(2), registry(std::slice::from_ref(m)), setup(&port)).await
}

#[tokio::test(start_paused = true)]
async fn the_tasks_effort_beats_the_bots_and_the_bots_beats_none() {
    let m = lister("mock", vec![model("mock-1", &LMH)], &[]);
    let fx = fx_bot(&m, "model = \"mock-1\"\neffort = \"medium\"").await;
    let bot_task = |effort| {
        TaskRequest::new("go", PathBuf::from("/work"))
            .bot("keeper")
            .effort(effort)
    };
    let (run, _) = fx.kernel.submit_task(bot_task(None)).await.unwrap();
    fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(
        fx.kernel.get_run(&run).await.unwrap().effort,
        Some(Effort::Medium)
    );
    let (run, _) = fx
        .kernel
        .submit_task(bot_task(Some(Effort::Low)))
        .await
        .unwrap();
    fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(
        fx.kernel.get_run(&run).await.unwrap().effort,
        Some(Effort::Low)
    );
    let efforts: Vec<_> = m.log().specs.iter().map(|s| s.effort).collect();
    assert_eq!(efforts, [Some(Effort::Medium), Some(Effort::Low)]);
    // The bot's own effort is checked too.
    let bad = fx_bot(&m, "model = \"mock-1\"\neffort = \"max\"").await;
    let why = invalid(bad.kernel.submit_task(bot_task(None)).await.unwrap_err());
    assert!(why.contains("mock-1") && why.contains("max"), "{why}");
    // A task's effort is what is checked when it overrides an unusable bot effort.
    let (run, _) = bad
        .kernel
        .submit_task(bot_task(Some(Effort::High)))
        .await
        .unwrap();
    bad.wait_status(&run, RunStatus::Succeeded).await;
}

#[tokio::test(start_paused = true)]
async fn a_trigger_fired_run_carries_its_bots_effort() {
    let m = lister("mock", vec![model("mock-1", &LMH)], &[]);
    let port = FakePort::with(vec![
        file(
            DefKind::Bot,
            "keeper",
            &bot_with("model = \"mock-1\"\neffort = \"high\""),
        ),
        file(
            DefKind::Trigger,
            "poke",
            "name = \"poke\"\n[when]\nkind = \"manual\"\n[target]\nbot = \"keeper\"\nprompt = \"go\"\n",
        ),
    ]);
    let fx =
        Fx::with_definitions(config(2), registry(std::slice::from_ref(&m)), setup(&port)).await;
    let run = fx
        .kernel
        .fire_trigger("poke")
        .await
        .unwrap()
        .run_id
        .unwrap();
    fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(m.log().specs[0].effort, Some(Effort::High));
    assert_eq!(
        fx.kernel.get_run(&run).await.unwrap().effort,
        Some(Effort::High)
    );
}

#[tokio::test(start_paused = true)]
async fn an_effort_with_no_executor_named_leaves_out_executors_that_never_list_it() {
    let low = lister("alpha", vec![model("a-1", &[Effort::Low])], &[]);
    let big = lister(
        "beta",
        vec![model("b-1", &[Effort::Low, Effort::XHigh])],
        &[],
    );
    let cfg = KernelConfig {
        default_executors: vec![id("alpha"), id("beta")],
        ..config(1)
    };
    let fx = Fx::new(cfg, registry(&[low.clone(), big.clone()])).await;
    let task = TaskRequest::new("go", PathBuf::from("/work")).effort(Some(Effort::XHigh));
    let decision = fx.kernel.preview_route(&task).unwrap();
    assert_eq!(decision.ranked, [id("beta")]);
    let rejected = decision
        .rejected
        .iter()
        .find(|r| r.executor == id("alpha"))
        .unwrap();
    match &rejected.reason {
        RejectReason::Excluded { why } => {
            assert!(why.contains("xhigh") && why.contains("a-1"), "{why}");
        }
        other => panic!("{other:?}"),
    }
    let (run, decision) = fx.kernel.submit_task(task).await.unwrap();
    assert_eq!(decision.chosen, Some(id("beta")));
    fx.wait_status(&run, RunStatus::Succeeded).await;
    assert_eq!(big.log().specs[0].effort, Some(Effort::XHigh));
    assert!(low.log().specs.is_empty());
    // Naming the executor that lacks it is a refusal, not a reroute.
    let why = invalid(
        fx.kernel
            .submit_task(request("alpha", None, Some(Effort::XHigh)))
            .await
            .unwrap_err(),
    );
    assert!(why.contains("alpha") || why.contains("a-1"), "{why}");
}

#[tokio::test(start_paused = true)]
async fn a_resumed_attempt_keeps_the_runs_effort() {
    let first = lister("mock", vec![model("mock-1", &LMH)], &[]).with_steps([
        MockStep::started("sess-1", "mock-1"),
        MockStep::Sleep(std::time::Duration::from_secs(3600)),
    ]);
    let after = lister("mock", vec![model("mock-1", &LMH)], &[]).with_steps([
        MockStep::started("sess-1", "mock-1"),
        MockStep::text("finished"),
    ]);
    let fx = Fx::new(config(1), registry(std::slice::from_ref(&first))).await;
    let (run, _) = fx
        .kernel
        .submit_task(request("mock", Some("mock-1"), Some(Effort::High)))
        .await
        .unwrap();
    until(async || {
        fx.store
            .get_run(&run)
            .await
            .unwrap()
            .and_then(|row| row.session_ref)
    })
    .await;
    let kernel = fx.kernel.clone();
    let stopping = tokio::spawn(async move { kernel.shutdown().await });
    until(async || stopping.is_finished().then_some(())).await;
    let (dir, store) = fx.into_store();
    let fx = Fx::over(
        dir,
        store,
        config(1),
        registry(std::slice::from_ref(&after)),
    )
    .await;
    fx.wait_status(&run, RunStatus::Succeeded).await;
    let specs = after.log().specs;
    assert_eq!(specs.len(), 1);
    assert!(specs[0].resume.is_some());
    assert_eq!(specs[0].effort, Some(Effort::High));
    assert_eq!(
        fx.kernel.get_run(&run).await.unwrap().effort,
        Some(Effort::High)
    );
}

fn windowed(id: &str) -> ModelInfo {
    ModelInfo {
        options: vec![reno_proto::ModelOption {
            id: "contextWindow".into(),
            label: "Context window".into(),
            choices: ["200k", "1m"]
                .map(|c| reno_proto::ModelOptionChoice {
                    id: c.into(),
                    label: c.into(),
                })
                .into(),
            default_choice: "1m".into(),
        }],
        ..model(id, &LMH)
    }
}

fn with_window(request: TaskRequest, choice: &str) -> TaskRequest {
    request.model_options([("contextWindow".to_owned(), choice.to_owned())].into())
}

#[tokio::test(start_paused = true)]
async fn a_model_option_the_model_lists_reaches_the_executor_and_the_summary() {
    let m = lister("mock", vec![windowed("mock-1")], &[]);
    let fx = Fx::new(config(1), registry(std::slice::from_ref(&m))).await;
    let (run, _) = fx
        .kernel
        .submit_task(with_window(request("mock", Some("mock-1"), None), "200k"))
        .await
        .unwrap();
    fx.wait_status(&run, RunStatus::Succeeded).await;
    let want: std::collections::BTreeMap<String, String> =
        [("contextWindow".to_owned(), "200k".to_owned())].into();
    assert_eq!(m.log().specs[0].model_options, want);
    assert_eq!(fx.kernel.get_run(&run).await.unwrap().model_options, want);
    let (listed, _) = fx.kernel.list_runs(None, None, None).await.unwrap();
    assert_eq!(listed[0].model_options, want);
}

#[tokio::test(start_paused = true)]
async fn an_unknown_option_or_choice_is_refused_naming_the_model_and_what_it_lists() {
    let m = lister("mock", vec![windowed("mock-1"), model("plain", &LMH)], &[]);
    let fx = Fx::new(config(1), registry(std::slice::from_ref(&m))).await;
    let why = invalid(
        fx.kernel
            .submit_task(with_window(request("mock", Some("mock-1"), None), "2m"))
            .await
            .unwrap_err(),
    );
    assert!(
        why.contains("mock-1") && why.contains("2m") && why.contains("200k, 1m"),
        "{why}"
    );
    let unknown = request("mock", Some("mock-1"), None)
        .model_options([("fastMode".to_owned(), "on".to_owned())].into());
    let why = invalid(fx.kernel.submit_task(unknown).await.unwrap_err());
    assert!(
        why.contains("fastMode") && why.contains("contextWindow"),
        "{why}"
    );
    // A model that lists no options takes none; an unlisted model's are unknown.
    for model in ["plain", "mystery"] {
        let why = invalid(
            fx.kernel
                .submit_task(with_window(request("mock", Some(model), None), "1m"))
                .await
                .unwrap_err(),
        );
        assert!(why.contains(model), "{why}");
    }
    assert!(m.log().specs.is_empty());
    assert!(
        fx.kernel
            .list_runs(None, None, None)
            .await
            .unwrap()
            .0
            .is_empty()
    );
}

#[tokio::test(start_paused = true)]
async fn an_attempt_that_resumes_keeps_the_runs_model_options() {
    let m = lister("mock", vec![windowed("mock-1")], &[]);
    let mut d = reno_harness::exec::Executor::descriptor(&m);
    d.caps.models = vec![windowed("mock-1")];
    let m = MockExecutor::new(id("mock"))
        .with_steps([
            MockStep::started("sess-1", "mock-1"),
            MockStep::Emit(reno_proto::ExecEvent::Done {
                stop: reno_proto::StopReason::Interrupted,
                result: None,
                diagnostic: None,
                reason: None,
            }),
        ])
        .with_descriptor(d);
    let fx = Fx::new(config(1), registry(std::slice::from_ref(&m))).await;
    let (run, _) = fx
        .kernel
        .submit_task(with_window(request("mock", Some("mock-1"), None), "200k"))
        .await
        .unwrap();
    fx.wait_status(&run, RunStatus::Interrupted).await;
    fx.kernel.resume_run(&run, None).await.unwrap();
    until(async || (m.log().specs.len() == 2).then_some(())).await;
    let specs = m.log().specs;
    assert_eq!(specs[0].model_options, specs[1].model_options);
    assert_eq!(
        specs[1]
            .model_options
            .get("contextWindow")
            .map(String::as_str),
        Some("200k")
    );
    assert_eq!(
        fx.kernel.get_run(&run).await.unwrap().model_options,
        specs[1].model_options
    );
}
