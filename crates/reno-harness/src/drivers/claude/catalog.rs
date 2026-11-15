//! The curated Claude model list, and which Claude Code versions know each model.
//!
//! The executor's descriptor lists these (`ExecutorCaps::models`): `descriptor()` is
//! synchronous and runs in no test, so the list is data, not a discovery call. A live
//! `initialize` handshake could append the models an account or gateway adds; no caller asks
//! for that yet.

use crate::upstream_proto::{Model, ModelOption, ModelOptionChoice, ReasoningLevel};

/// A boolean toggle rendered as an off/on select (the Rust `ModelOption` wire
/// type has no dedicated boolean kind).
fn toggle(id: &str, label: &str) -> ModelOption {
    ModelOption {
        id: id.into(),
        label: label.into(),
        choices: vec![
            ModelOptionChoice {
                id: "off".into(),
                label: "Off".into(),
            },
            ModelOptionChoice {
                id: "on".into(),
                label: "On".into(),
            },
        ],
        default_choice: "off".into(),
    }
}

/// The 200K/1M context-window select carried by the long-context models. The
/// 1M window is selected via a model-id suffix (`<model>[1m]`), exactly how the
/// CLI itself does it.
pub(crate) fn context_window() -> ModelOption {
    ModelOption {
        id: "contextWindow".into(),
        label: "Context Window".into(),
        choices: vec![
            ModelOptionChoice {
                id: "200k".into(),
                label: "200K".into(),
            },
            ModelOptionChoice {
                id: "1m".into(),
                label: "1M".into(),
            },
        ],
        default_choice: "200k".into(),
    }
}

/// The Context Window select for models whose plain id already runs with the
/// 1M window (the Fable models, Opus 4.7+, Sonnet 5+). Left unset, they run
/// at 1M, so the select says so; 200K is an explicit choice that the launch
/// enforces with `CLAUDE_CODE_DISABLE_1M_CONTEXT`.
fn native_1m_context_window() -> ModelOption {
    ModelOption {
        default_choice: "1m".into(),
        ..context_window()
    }
}

const FULL_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
    ReasoningLevel::Ultracode,
    ReasoningLevel::Ultrathink,
];

/// opus-4-7 / sonnet-5+ tier (claude.ts `claudeEffortsFor`): xhigh native,
/// no ultracode.
const XHIGH_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
    ReasoningLevel::Ultrathink,
];

fn model(
    id: &str,
    label: &str,
    description: &str,
    ladder: &[ReasoningLevel],
    options: Vec<ModelOption>,
) -> Model {
    Model {
        default: false,
        id: id.into(),
        label: label.into(),
        provider: None,
        description: (!description.is_empty()).then(|| description.into()),
        reasoning_levels: ladder.to_vec(),
        options,
    }
}

/// The oldest Claude Code that lists each curated model (T3 Code's model
/// manifest, Nov 2026). An older CLI doesn't report the model at all.
const MIN_CLI: &[(&str, &str)] = &[
    ("claude-fable-5-2", "2.1.290"),
    ("claude-fable-5-1", "2.1.257"),
    ("claude-opus-5-5", "2.1.280"),
    ("claude-opus-5", "2.1.219"),
    ("claude-opus-4-8", "2.1.154"),
    ("claude-opus-4-7", "2.1.111"),
    ("claude-sonnet-5-5", "2.1.284"),
    ("claude-haiku-5-5", "2.1.293"),
];

/// Whether `cli` (a Claude Code version) is known and older than the one
/// `id` needs. An unknown or unparsable version is never too old.
fn too_old_for(id: &str, cli: Option<&str>) -> bool {
    let Some(cli) = cli.and_then(|v| semver::Version::parse(v.trim()).ok()) else {
        return false;
    };
    MIN_CLI
        .iter()
        .find(|(model, _)| *model == id)
        .and_then(|(_, min)| semver::Version::parse(min).ok())
        .is_some_and(|min| cli < min)
}

/// The curated models a CLI of version `cli` lists: the rows it is too old for are dropped. An
/// unknown version keeps every row.
pub(super) fn supported_models(cli: Option<&str>) -> Vec<Model> {
    let mut models = static_models();
    models.retain(|model| !too_old_for(&model.id, cli));
    models
}

/// Written best-first (`sort_best_first` keeps it so). Options follow what
/// the CLI's initialize reports per model (`supportsFastMode`,
/// `supportsThinkingOff`); it reports no 1M flag, so only the rows that
/// already carried the context-window select keep it.
pub(crate) fn static_models() -> Vec<Model> {
    let mut models = vec![
        model(
            "claude-fable-5-2",
            "Fable 5.2",
            "For your toughest challenges",
            FULL_LADDER,
            vec![toggle("fastMode", "Fast Mode")],
        ),
        model(
            "claude-fable-5-1",
            "Fable 5.1",
            "Previous generation Fable",
            FULL_LADDER,
            vec![native_1m_context_window()],
        ),
        model(
            "claude-fable-5",
            "Fable 5",
            "Older generation Fable",
            FULL_LADDER,
            vec![native_1m_context_window()],
        ),
        model(
            "claude-opus-5-5",
            "Opus 5.5",
            "Best for everyday, complex tasks",
            FULL_LADDER,
            vec![native_1m_context_window(), toggle("fastMode", "Fast Mode")],
        ),
        model(
            "claude-opus-4-8",
            "Opus 4.8",
            "Previous generation Opus",
            FULL_LADDER,
            vec![toggle("fastMode", "Fast Mode")],
        ),
        model(
            "claude-opus-4-7",
            "Opus 4.7",
            "Older generation Opus",
            XHIGH_LADDER,
            vec![toggle("fastMode", "Fast Mode")],
        ),
        model(
            "claude-sonnet-5-5",
            "Sonnet 5.5",
            "Most efficient for simpler tasks",
            XHIGH_LADDER,
            vec![],
        ),
        model(
            "claude-sonnet-5",
            "Sonnet 5",
            "Previous generation Sonnet",
            XHIGH_LADDER,
            vec![native_1m_context_window()],
        ),
        model(
            "claude-haiku-5-5",
            "Haiku 5.5",
            "Fastest for quick answers",
            XHIGH_LADDER,
            vec![],
        ),
        model(
            "claude-haiku-4-5",
            "Haiku 4.5",
            "Previous generation Haiku",
            &[],
            vec![toggle("thinking", "Thinking")],
        ),
    ];
    // The CLI's default as of 2.1.322, for when discovery is unavailable;
    // a live catalog marks the CLI's own default instead.
    for model in &mut models {
        model.default = model.id == "claude-opus-5-5";
    }
    models
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(models: &[Model]) -> Vec<&str> {
        models.iter().map(|m| &*m.id).collect()
    }

    #[test]
    fn the_curated_list_has_one_default_and_unique_ids() {
        let models = static_models();
        assert_eq!(models.iter().filter(|m| m.default).count(), 1);
        let mut seen = std::collections::HashSet::new();
        assert!(models.iter().all(|m| seen.insert(m.id.clone())));
    }

    #[test]
    fn rows_an_old_cli_is_too_old_for_are_dropped() {
        // 2.1.200: older than Opus 5 (2.1.219) and everything after it.
        assert_eq!(
            ids(&supported_models(Some("2.1.200"))),
            [
                "claude-fable-5",
                "claude-opus-4-8",
                "claude-opus-4-7",
                "claude-sonnet-5",
                "claude-haiku-4-5"
            ]
        );
        // Each model at exactly its minimum stays, one patch below goes.
        for (id, min) in MIN_CLI {
            let min = semver::Version::parse(min).unwrap();
            let below = semver::Version::new(min.major, min.minor, min.patch - 1).to_string();
            assert!(!too_old_for(id, Some(&min.to_string())), "{id}");
            assert!(too_old_for(id, Some(&below)), "{id}");
        }
    }

    #[test]
    fn an_unknown_cli_version_keeps_every_curated_row() {
        for cli in [None, Some(""), Some("not a version")] {
            assert_eq!(
                ids(&supported_models(cli)),
                ids(&static_models()),
                "{cli:?}"
            );
        }
    }
}
