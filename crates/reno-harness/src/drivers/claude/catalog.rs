//! The curated Claude model list, and which Claude Code versions know each model.
//!
//! The executor's descriptor lists these (`ExecutorCaps::models`): `descriptor()` is
//! synchronous and runs in no test, so the list is data, not a discovery call. It is the
//! fallback: the probe's live `initialize` handshake (see `discover`) replaces it, and its
//! aliases join [`aliases`], whenever the CLI answers.

use reno_proto::{
    CONTEXT_WINDOW_1M, CONTEXT_WINDOW_200K, MODEL_OPTION_CONTEXT_WINDOW, ModelInfo, ModelOption,
    ModelOptionChoice,
};

/// The 200K/1M context-window option. The 1M window is selected by a model-id suffix
/// (`<model>[1m]`), exactly how the CLI does it. Default 200K: the plain id runs 200K.
pub(super) fn context_window() -> ModelOption {
    ModelOption {
        settable: true,
        id: MODEL_OPTION_CONTEXT_WINDOW.into(),
        label: "Context window".into(),
        choices: vec![
            ModelOptionChoice {
                id: CONTEXT_WINDOW_200K.into(),
                label: "200K".into(),
            },
            ModelOptionChoice {
                id: CONTEXT_WINDOW_1M.into(),
                label: "1M".into(),
            },
        ],
        default_choice: CONTEXT_WINDOW_200K.into(),
    }
}

/// The context-window option of the models whose plain id already runs the 1M window: left
/// unset they run at 1M, so the option says so, and an explicit 200K is enforced at launch with
/// `CLAUDE_CODE_DISABLE_1M_CONTEXT` (upstream zeron#1083).
pub(super) fn native_1m_context_window() -> ModelOption {
    ModelOption {
        default_choice: CONTEXT_WINDOW_1M.into(),
        ..context_window()
    }
}

/// The curated table of models that offer the context-window choice (the CLI's `initialize`
/// reports no 1M flag): the long-context rows, by id. Absent means the model has no option.
const NATIVE_1M: &[&str] = &[
    "claude-fable-5-1",
    "claude-fable-5",
    "claude-opus-5-5",
    "claude-sonnet-5",
];

/// The per-model options Reno offers for the Claude model `id`: empty for a model not in the
/// curated table.
pub(super) fn options_for(id: &str) -> Vec<ModelOption> {
    if NATIVE_1M.contains(&id) {
        vec![native_1m_context_window()]
    } else {
        Vec::new()
    }
}

/// The curated models, best first: `(id, display name)`. Which CLI versions list each is in
/// [`MIN_CLI`].
const CURATED: &[(&str, &str)] = &[
    ("claude-fable-5-2", "Fable 5.2"),
    ("claude-fable-5-1", "Fable 5.1"),
    ("claude-fable-5", "Fable 5"),
    ("claude-opus-5-5", "Opus 5.5"),
    ("claude-opus-4-8", "Opus 4.8"),
    ("claude-opus-4-7", "Opus 4.7"),
    ("claude-sonnet-5-5", "Sonnet 5.5"),
    ("claude-sonnet-5", "Sonnet 5"),
    ("claude-haiku-5-5", "Haiku 5.5"),
    ("claude-haiku-4-5", "Haiku 4.5"),
];

/// The aliases Claude Code accepts for `--model` (code.claude.com model-config, Nov 2026). Each
/// resolves on the CLI's side to the newest model of its family for the account's provider; a
/// `[1m]` suffix asks for the 1M-token context. `default` is not one: it clears an override.
const ALIASES: &[&str] = &[
    "best",
    "fable",
    "opus",
    "sonnet",
    "haiku",
    "opusplan",
    "opus[1m]",
    "sonnet[1m]",
    "opusplan[1m]",
];

/// The aliases the CLI accepts: the documented [`ALIASES`], then any the CLI's own model list
/// added (`live`, from its `initialize` answer). The documented ones stay when a list is
/// known: `opusplan` and the `[1m]` forms are not rows of it but are accepted all the same.
pub(super) fn aliases(live: Option<&[String]>) -> Vec<String> {
    let mut all: Vec<String> = ALIASES.iter().map(|alias| (*alias).to_owned()).collect();
    for alias in live.unwrap_or_default() {
        if !all.contains(alias) {
            all.push(alias.clone());
        }
    }
    all
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
pub(super) fn supported_models(cli: Option<&str>) -> Vec<ModelInfo> {
    CURATED
        .iter()
        .filter(|(id, _)| !too_old_for(id, cli))
        .map(|(id, label)| ModelInfo {
            options: options_for(id),
            default_effort: None,
            efforts: vec![],
            id: (*id).into(),
            display_name: Some((*label).into()),
            description: None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(models: &[ModelInfo]) -> Vec<&str> {
        models.iter().map(|m| &*m.id).collect()
    }

    #[test]
    fn the_cli_aliases_are_the_documented_ones() {
        let aliases = aliases(None);
        for alias in [
            "sonnet",
            "opus",
            "haiku",
            "opusplan",
            "sonnet[1m]",
            "opus[1m]",
        ] {
            assert!(aliases.iter().any(|a| a == alias), "{alias}");
        }
        assert!(!aliases.iter().any(|a| a == "default"));
        // An alias is never also a curated id.
        assert!(
            CURATED
                .iter()
                .all(|(id, _)| !aliases.iter().any(|a| a == id))
        );
    }

    #[test]
    fn the_live_aliases_join_the_documented_ones_without_repeats() {
        let live = ["sonnet".to_owned(), "newalias".to_owned()];
        let all = aliases(Some(&live));
        assert!(all.iter().any(|a| a == "opusplan"));
        assert_eq!(all.iter().filter(|a| *a == "sonnet").count(), 1);
        assert!(all.iter().any(|a| a == "newalias"));
    }

    #[test]
    fn only_the_long_context_models_offer_the_window_choice() {
        let models = supported_models(None);
        let offering: Vec<&str> = models
            .iter()
            .filter(|m| !m.options.is_empty())
            .map(|m| &*m.id)
            .collect();
        assert_eq!(
            offering,
            [
                "claude-fable-5-1",
                "claude-fable-5",
                "claude-opus-5-5",
                "claude-sonnet-5"
            ]
        );
        let opus = models.iter().find(|m| m.id == "claude-opus-5-5").unwrap();
        let option = &opus.options[0];
        assert_eq!(option.id, "contextWindow");
        assert_eq!(option.default_choice, "1m");
        let choices: Vec<&str> = option.choices.iter().map(|c| &*c.id).collect();
        assert_eq!(choices, ["200k", "1m"]);
        assert!(options_for("claude-haiku-4-5").is_empty());
        assert!(options_for("claude-opus-5-5-unheard-of").is_empty());
        // The plain constructor defaults to 200K.
        assert_eq!(context_window().default_choice, "200k");
    }

    #[test]
    fn the_curated_list_has_unique_ids() {
        let mut seen = std::collections::HashSet::new();
        assert!(CURATED.iter().all(|(id, _)| seen.insert(*id)));
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
                ids(&supported_models(None)),
                "{cli:?}"
            );
        }
    }
}
