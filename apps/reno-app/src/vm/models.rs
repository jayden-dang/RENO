//! The harness · model picker: which harness a task goes to and which of its models, as the
//! harness's own CLI reported them when it was last checked (`executor.list`, `health.models`).
//! Nothing here is a list Reno keeps: a model the CLI did not report is not offered.
//!
//! `None` as a pick is "Auto": the router chooses the harness and the harness its default model.

use reno_client::State;
use reno_proto::{
    Effort, ExecEvent, ExecutorId, ExecutorInfo, InstallStatus, ModelSource, RunId, SignedIn,
};

use crate::summary;

/// What the person chose: a harness, one of its models (`None`: the harness's default), and one
/// of that model's effort levels (`None`: the model's default).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pick {
    pub(crate) executor: ExecutorId,
    pub(crate) model: Option<String>,
    pub(crate) effort: Option<Effort>,
}

/// One model on offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelChoice {
    pub(crate) id: String,
    /// "Sonnet 5.5", or the id when the CLI gave no name.
    pub(crate) name: String,
    /// The effort levels the CLI says it takes, least to most; empty when it did not say.
    pub(crate) efforts: Vec<Effort>,
    /// Its own level when none is asked for.
    pub(crate) default_effort: Option<Effort>,
}

/// One harness on offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HarnessChoice {
    pub(crate) id: ExecutorId,
    pub(crate) name: String,
    pub(crate) models: Vec<ModelChoice>,
    /// The models came from the CLI itself; false is the adapter's fallback list.
    pub(crate) live: bool,
    /// Installed and not signed out: a task sent to it would start.
    pub(crate) usable: bool,
    /// A running run's model can be changed (`ExecutorCaps::model_switch`).
    pub(crate) switches: bool,
}

fn choice(info: &ExecutorInfo) -> Option<HarnessChoice> {
    let health = info.health.as_ref();
    let installed = health.is_some_and(|h| matches!(h.installed, InstallStatus::Installed { .. }));
    if !installed || info.descriptor.id.as_str() == "mock" {
        return None;
    }
    let signed_out = health.is_some_and(|h| matches!(h.signed_in, SignedIn::No { .. }));
    let (models, live) = health.map_or_else(
        || (info.descriptor.caps.models.clone(), false),
        |h| {
            (
                h.models.models.clone(),
                h.models.source == ModelSource::Live,
            )
        },
    );
    Some(HarnessChoice {
        id: info.descriptor.id.clone(),
        name: summary::executor_name(info.descriptor.id.as_str()),
        models: models
            .into_iter()
            .map(|m| ModelChoice {
                name: m.display_name.clone().unwrap_or_else(|| m.id.clone()),
                efforts: m.efforts,
                default_effort: m.default_effort,
                id: m.id,
            })
            .collect(),
        live,
        usable: !signed_out,
        switches: info.descriptor.caps.model_switch,
    })
}

/// Every installed harness, usable ones first, in the daemon's order.
pub(crate) fn harnesses(state: &State) -> Vec<HarnessChoice> {
    let mut all: Vec<HarnessChoice> = state.executors().iter().filter_map(choice).collect();
    all.sort_by_key(|h| !h.usable);
    all
}

/// One harness, when it is installed.
pub(crate) fn harness(state: &State, id: &ExecutorId) -> Option<HarnessChoice> {
    state
        .executors()
        .iter()
        .find(|e| &e.descriptor.id == id)
        .and_then(choice)
}

/// A model's name as its harness gave it ("Sonnet 5.5"); the id itself when the harness does
/// not list it (an alias, or a model it no longer reports).
pub(crate) fn model_name(state: &State, executor: &ExecutorId, model: &str) -> String {
    harness(state, executor)
        .and_then(|h| h.models.into_iter().find(|m| m.id == model))
        .map_or_else(|| model.to_owned(), |m| m.name)
}

/// The model a run is using, as its harness said: the last `ModelChanged` in its journal, else
/// the model `Started` reported, else (journal not loaded yet) the one it asked for.
pub(crate) fn in_use(state: &State, run: &RunId) -> Option<String> {
    let reported = state.transcript(run).and_then(|t| {
        t.events.iter().rev().find_map(|(_, event)| match event {
            ExecEvent::ModelChanged { model } | ExecEvent::Started { model, .. } => {
                Some(model.clone())
            }
            _ => None,
        })
    });
    reported.or_else(|| state.run(run).and_then(|r| r.model.clone()))
}

/// One model of a harness, when the harness lists it.
pub(crate) fn model(state: &State, executor: &ExecutorId, model: &str) -> Option<ModelChoice> {
    harness(state, executor).and_then(|h| h.models.into_iter().find(|m| m.id == model))
}

/// The chip's words: "Auto", "Codex", "Claude Code · Sonnet 5.5", or with an effort
/// "Claude Code · Sonnet 5.5 · high".
pub(crate) fn label(state: &State, pick: Option<&Pick>) -> String {
    let Some(pick) = pick else {
        return "Auto".to_owned();
    };
    let mut words = summary::executor_name(pick.executor.as_str());
    if let Some(model) = &pick.model {
        words.push_str(&format!(
            " \u{b7} {}",
            model_name(state, &pick.executor, model)
        ));
    }
    if let Some(effort) = pick.effort {
        words.push_str(&format!(" \u{b7} {}", effort.as_str()));
    }
    words
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use reno_client::fake::{FakeConfig, FakeDaemon};
    use reno_client::{Feed, FeedState};

    use super::*;

    /// The fake daemon's executors, as the app's state holds them.
    fn state() -> State {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let fake = FakeDaemon::start(FakeConfig::default()).unwrap();
            let client = fake.connect().await.unwrap();
            let executors = client.list_executors().await.unwrap();
            let mut state = State::new();
            state.apply(reno_client::Update::Executors(executors));
            state.apply(reno_client::Update::Feed(Feed::Executors, FeedState::Live));
            state
        })
    }

    #[test]
    fn harnesses_offer_the_models_their_cli_reported_and_say_who_switches() {
        let state = state();
        let all = harnesses(&state);
        let claude = all.iter().find(|h| h.id.as_str() == "claude-code").unwrap();
        assert!(claude.usable && claude.live && claude.switches);
        let names: Vec<_> = claude.models.iter().map(|m| m.name.as_str()).collect();
        assert_eq!(names, ["Opus 5.5", "Sonnet 5.5", "Haiku 5.5"]);
        let codex = all.iter().find(|h| h.id.as_str() == "codex").unwrap();
        assert!(!codex.usable, "signed out in the demo");
        assert!(!codex.switches);
        assert!(
            all.iter().all(|h| h.id.as_str() != "mock"),
            "the mock is not a choice"
        );
        let first_unusable = all.iter().position(|h| !h.usable).unwrap_or(all.len());
        assert!(all[..first_unusable].iter().all(|h| h.usable));
    }

    #[test]
    fn the_model_in_use_is_the_last_the_harness_reported() {
        use reno_proto::{RunStatus, RunSummary, SessionRef, TaskId};
        let run = RunId::from_suffix("a").unwrap();
        let mut state = State::new();
        state.apply(reno_client::Update::RunUpserted(RunSummary {
            effort: None,
            run_id: run.clone(),
            task_id: TaskId::from_suffix("a").unwrap(),
            executor: "claude-code".parse().unwrap(),
            status: RunStatus::Running,
            attempt: 1,
            title: "t".into(),
            prompt: None,
            model: Some("sonnet".into()),
            stop_reason: None,
            stop: None,
            not_resumed: None,
            session: None,
            bot: None,
            cwd: None,
            report: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }));
        assert_eq!(in_use(&state, &run).as_deref(), Some("sonnet"), "asked for");
        let event = |seq, event| reno_client::Update::RunEvent {
            run: run.clone(),
            seq,
            event,
        };
        state.apply(event(
            reno_proto::Seq(0),
            ExecEvent::Started {
                session: SessionRef("s".into()),
                model: "claude-sonnet-5-5".into(),
            },
        ));
        assert_eq!(in_use(&state, &run).as_deref(), Some("claude-sonnet-5-5"));
        state.apply(event(
            reno_proto::Seq(1),
            ExecEvent::ModelChanged {
                model: "claude-haiku-5-5".into(),
            },
        ));
        assert_eq!(in_use(&state, &run).as_deref(), Some("claude-haiku-5-5"));
    }

    #[test]
    fn the_chip_names_the_pick() {
        let state = state();
        let claude: ExecutorId = "claude-code".parse().unwrap();
        assert_eq!(label(&state, None), "Auto");
        assert_eq!(
            label(
                &state,
                Some(&Pick {
                    executor: claude.clone(),
                    model: None,
                    effort: None,
                })
            ),
            "Claude Code"
        );
        assert_eq!(
            label(
                &state,
                Some(&Pick {
                    executor: claude.clone(),
                    model: Some("claude-sonnet-5-5".into()),
                    effort: None,
                })
            ),
            "Claude Code \u{b7} Sonnet 5.5"
        );
        assert_eq!(
            label(
                &state,
                Some(&Pick {
                    executor: claude.clone(),
                    model: Some("claude-sonnet-5-5".into()),
                    effort: Some(Effort::XHigh),
                })
            ),
            "Claude Code \u{b7} Sonnet 5.5 \u{b7} xhigh"
        );
        let haiku = model(&state, &claude, "claude-haiku-5-5").unwrap();
        assert_eq!(haiku.efforts, [Effort::Low, Effort::Medium, Effort::High]);
        assert_eq!(haiku.default_effort, Some(Effort::Medium));
        assert_eq!(model_name(&state, &claude, "sonnet"), "sonnet");
    }
}
