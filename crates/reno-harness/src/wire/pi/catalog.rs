//! Pi's model list as Reno's `ModelInfo`, model-name resolution and thinking levels.
//!
//! `get_available_models` returns a full model object for every provider Pi holds a credential
//! for (an API key in the environment or a stored login), so a non-empty list is also the one
//! sign-in fact the probe can read without a model call. A model is named `provider/id`: two
//! providers can serve the same model id.
//!
//! # Thinking levels
//!
//! Which levels a model takes is a pure function of the model object (`getSupportedThinkingLevels`
//! in `pi-ai`'s `models.js`): a non-reasoning model has `off` only; otherwise
//! `off, minimal, low, medium, high` unless `thinkingLevelMap` maps the level to `null`, plus
//! `xhigh` and `max` only when the map names them. It is ported here so listing models does not
//! need one `set_model` round trip per model, as the 0.85 driver did.

use reno_proto::{Effort, ModelInfo, ModelOption, ModelOptionChoice};
use serde_json::Value;

use super::frames::string;

/// Pi's levels, least to most, `off` first.
pub(crate) const LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// The id of the model option that turns thinking off for a model that allows it. `auto` uses
/// the run's effort (or Pi's own default); `off` sets the `off` level.
pub(crate) const THINKING_OPTION: &str = "pi_thinking";

/// The levels `model` accepts, `off` included when it does.
pub(crate) fn supported_levels(model: &Value) -> Vec<&'static str> {
    if model.get("reasoning").and_then(Value::as_bool) != Some(true) {
        return vec!["off"];
    }
    let map = model.get("thinkingLevelMap");
    LEVELS
        .into_iter()
        .filter(|level| {
            let mapped = map.and_then(|m| m.get(*level));
            match mapped {
                Some(Value::Null) => false,
                Some(_) => true,
                None => !matches!(*level, "xhigh" | "max"),
            }
        })
        .collect()
}

/// The effort a Pi level is.
pub(crate) fn effort_of(level: &str) -> Option<Effort> {
    Effort::from_word(level)
}

/// The Pi level a run asks for, if it asks for one: `off` when the model option says so, else
/// the effort. `None` leaves Pi at its own default.
pub(crate) fn level_for(
    effort: Option<Effort>,
    model_options: &std::collections::BTreeMap<String, String>,
) -> Option<&'static str> {
    if model_options
        .get(THINKING_OPTION)
        .is_some_and(|choice| choice == "off")
    {
        return Some("off");
    }
    effort.map(Effort::as_str)
}

/// `provider/id` of a model object.
pub(crate) fn model_name(model: &Value) -> String {
    let id = string(model, "id");
    match string(model, "provider") {
        "" => id.to_owned(),
        provider => format!("{provider}/{id}"),
    }
}

/// Reno's catalog entries for the `models` of a `get_available_models` reply. A model without an
/// id is dropped, and so is a repeat of one already listed.
pub(crate) fn model_infos(data: &Value) -> Vec<ModelInfo> {
    let mut infos: Vec<ModelInfo> = Vec::new();
    for model in data
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if string(model, "id").is_empty() {
            continue;
        }
        let id = model_name(model);
        if infos.iter().any(|known| known.id == id) {
            continue;
        }
        let levels = supported_levels(model);
        let efforts: Vec<Effort> = levels.iter().filter_map(|l| effort_of(l)).collect();
        let provider = string(model, "provider");
        infos.push(ModelInfo {
            id,
            display_name: Some(match string(model, "name") {
                "" => string(model, "id").to_owned(),
                name => name.to_owned(),
            })
            .filter(|name| !name.is_empty()),
            description: (!provider.is_empty()).then(|| provider_name(provider)),
            efforts,
            default_effort: None,
            options: if levels.contains(&"off") && levels.len() > 1 {
                vec![thinking_option()]
            } else {
                Vec::new()
            },
        });
    }
    infos
}

fn thinking_option() -> ModelOption {
    ModelOption {
        id: THINKING_OPTION.into(),
        label: "Thinking".into(),
        choices: vec![
            ModelOptionChoice {
                id: "auto".into(),
                label: "Use reasoning level".into(),
            },
            ModelOptionChoice {
                id: "off".into(),
                label: "Off".into(),
            },
        ],
        default_choice: "auto".into(),
        settable: true,
    }
}

/// Which `provider`/`id` the user's `wanted` model names among `data`'s `models`: the exact
/// `provider/id`, else a bare id that only one provider serves.
pub(crate) fn resolve_model(data: &Value, wanted: &str) -> Result<(String, String), String> {
    let models: Vec<&Value> = data
        .get("models")
        .and_then(Value::as_array)
        .map(|models| models.iter().collect())
        .unwrap_or_default();
    if let Some(found) = models.iter().find(|m| model_name(m) == wanted) {
        return Ok((
            string(found, "provider").to_owned(),
            string(found, "id").to_owned(),
        ));
    }
    let mut same_id = models.iter().filter(|m| string(m, "id") == wanted);
    match (same_id.next(), same_id.next()) {
        (Some(found), None) => Ok((
            string(found, "provider").to_owned(),
            string(found, "id").to_owned(),
        )),
        (Some(_), Some(_)) => Err(format!(
            "Pi model {wanted} is served by more than one provider; name it as provider/id"
        )),
        (None, _) => Err(format!("Pi model is not available: {wanted}")),
    }
}

/// A Pi provider's name as people say it. Pi reports only its id (`openai-codex`); one not
/// listed reads as its id in words (`foo-bar` is `Foo Bar`).
pub(crate) fn provider_name(id: &str) -> String {
    let known = match id {
        "openai-codex" => "Codex",
        "opencode" => "OpenCode",
        "opencode-go" => "OpenCode Go",
        "anthropic" => "Anthropic",
        "openai" => "OpenAI",
        "azure-openai-responses" => "Azure OpenAI",
        "github-copilot" => "Copilot",
        "google" => "Google",
        "google-gemini-cli" => "Gemini CLI",
        "google-antigravity" => "Antigravity",
        "google-vertex" => "Vertex AI",
        "amazon-bedrock" => "Bedrock",
        "openrouter" => "OpenRouter",
        "vercel-ai-gateway" => "Vercel AI Gateway",
        "xai" => "xAI",
        "groq" => "Groq",
        "cerebras" => "Cerebras",
        "mistral" => "Mistral",
        "minimax" => "MiniMax",
        "minimax-cn" => "MiniMax China",
        "huggingface" => "Hugging Face",
        "zai" => "Z.ai",
        "kimi-coding" => "Kimi",
        _ => "",
    };
    if !known.is_empty() {
        return known.into();
    }
    id.split(['-', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect::<String>())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Three of the 68 models Pi 1.5.0 listed in the live handshake, trimmed to the fields the
    /// catalog reads: a model that cannot switch thinking off (`off: null`) and names `xhigh`
    /// and `max`, one that maps nothing, and a non-reasoning one.
    fn live() -> Value {
        json!({ "models": [
            { "id": "claude-fable-5", "name": "Claude Fable 5", "provider": "anthropic",
              "reasoning": true, "contextWindow": 1_000_000,
              "thinkingLevelMap": { "off": null, "xhigh": "xhigh", "max": "max" } },
            { "id": "gpt-5", "name": "GPT-5", "provider": "openai", "reasoning": true },
            { "id": "gpt-4", "name": "GPT-4", "provider": "openai", "reasoning": false }
        ]})
    }

    #[test]
    fn thinking_levels_follow_the_model_object() {
        let models = live();
        let levels = |i: usize| supported_levels(&models["models"][i]);
        assert_eq!(levels(0), ["minimal", "low", "medium", "high", "xhigh", "max"]);
        assert_eq!(levels(1), ["off", "minimal", "low", "medium", "high"]);
        assert_eq!(levels(2), ["off"]);
    }

    #[test]
    fn the_catalog_lists_efforts_and_offers_off_only_where_pi_allows_it() {
        let infos = model_infos(&live());
        assert_eq!(
            infos.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["anthropic/claude-fable-5", "openai/gpt-5", "openai/gpt-4"]
        );
        assert_eq!(infos[0].efforts, Effort::ALL[..].to_vec());
        assert!(infos[0].options.is_empty(), "this model cannot turn thinking off");
        assert_eq!(infos[1].efforts.len(), 4);
        assert_eq!(infos[1].options[0].id, THINKING_OPTION);
        assert!(infos[2].efforts.is_empty());
        assert!(infos[2].options.is_empty());
        assert_eq!(infos[0].description.as_deref(), Some("Anthropic"));
        assert_eq!(infos[0].display_name.as_deref(), Some("Claude Fable 5"));
    }

    #[test]
    fn repeats_and_nameless_models_are_dropped() {
        let data = json!({ "models": [
            { "id": "a", "provider": "p" }, { "id": "a", "provider": "p" }, { "provider": "p" },
            { "id": "a", "provider": "q" }
        ]});
        let ids: Vec<_> = model_infos(&data).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["p/a", "q/a"]);
    }

    #[test]
    fn a_model_is_found_by_provider_and_id_or_by_a_bare_id_only_one_provider_serves() {
        let data = json!({ "models": [
            { "id": "a", "provider": "p" }, { "id": "a", "provider": "q" }, { "id": "b", "provider": "p" }
        ]});
        assert_eq!(resolve_model(&data, "q/a").unwrap(), ("q".into(), "a".into()));
        assert_eq!(resolve_model(&data, "b").unwrap(), ("p".into(), "b".into()));
        assert!(resolve_model(&data, "a").unwrap_err().contains("more than one provider"));
        assert!(resolve_model(&data, "c").unwrap_err().contains("not available"));
    }

    #[test]
    fn the_level_a_run_asks_for() {
        let off: std::collections::BTreeMap<_, _> =
            [(THINKING_OPTION.to_owned(), "off".to_owned())].into();
        assert_eq!(level_for(Some(Effort::High), &off), Some("off"));
        assert_eq!(level_for(Some(Effort::XHigh), &Default::default()), Some("xhigh"));
        assert_eq!(level_for(None, &Default::default()), None);
    }

    #[test]
    fn provider_names_read_as_people_say_them() {
        assert_eq!(provider_name("openai-codex"), "Codex");
        assert_eq!(provider_name("github-copilot"), "Copilot");
        assert_eq!(provider_name("my-local_llm"), "My Local Llm");
        assert_eq!(provider_name("ollama"), "Ollama");
    }
}
