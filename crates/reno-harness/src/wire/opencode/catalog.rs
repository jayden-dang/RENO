//! The model catalog: `GET /provider` (1.x) and `GET /api/model` (2.x) as the models a run may
//! name, their effort levels, their context windows and which providers the server holds
//! credentials for.
//!
//! `/provider` carries the whole models.dev catalog (233 providers on 1.19.5): only what the
//! picker and the run consume is decoded, with serde skipping the rest. Models are listed for the
//! CONNECTED providers only: `connected` names the usable set (credentialed, declared in config,
//! and the anonymous `opencode` tier with its free models, which a clean install lists too), and
//! every other model fails a run with "Model not found".

use std::collections::{BTreeMap, HashMap};

use reno_proto::{Effort, ModelInfo, ModelOption, ModelOptionChoice};
use serde::Deserialize;
use serde_json::Value;

/// What `/provider` (1.x) is decoded to, and `/api/model` (2.x) folded into.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct ProviderCatalog {
    all: Option<Vec<Provider>>,
    connected: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct Provider {
    id: Option<String>,
    name: Option<String>,
    models: Option<BTreeMap<String, ProviderModel>>,
}

#[derive(Debug, Deserialize)]
struct ProviderModel {
    #[serde(default)]
    limit: ProviderLimit,
    name: Option<String>,
    variants: Option<BTreeMap<String, serde::de::IgnoredAny>>,
}

#[derive(Debug, Default, Deserialize)]
struct ProviderLimit {
    context: Option<u64>,
}

/// `GET /api/model` (2.x): `{"location": .., "data": [model]}`.
#[derive(Debug, Deserialize)]
struct V2ModelList {
    #[serde(default)]
    data: Vec<V2Model>,
}

#[derive(Debug, Deserialize)]
struct V2Model {
    #[serde(rename = "providerID")]
    provider_id: String,
    id: String,
    name: Option<String>,
    #[serde(default)]
    limit: ProviderLimit,
    #[serde(default)]
    variants: Vec<V2Variant>,
    enabled: Option<bool>,
}

/// 2.x carries a variant as `{id, settings}`.
#[derive(Debug, Deserialize)]
struct V2Variant {
    id: String,
}

impl ProviderCatalog {
    /// A 1.x `/provider` body.
    pub(crate) fn from_v1(body: &[u8]) -> Result<Self, String> {
        serde_json::from_slice(body).map_err(|e| format!("opencode /provider: {e}"))
    }

    /// A 2.x `/api/model` body, folded into the 1.x shape so everything after is one code path.
    /// `/api/model` names no providers (the id stands in for the name) and no `connected` set:
    /// it lists enabled models only.
    pub(crate) fn from_v2(body: &[u8]) -> Result<Self, String> {
        let list: V2ModelList =
            serde_json::from_slice(body).map_err(|e| format!("opencode /api/model: {e}"))?;
        let mut order: Vec<String> = Vec::new();
        let mut grouped: HashMap<String, BTreeMap<String, ProviderModel>> = HashMap::new();
        for model in list.data.into_iter().filter(|m| m.enabled != Some(false)) {
            let models = grouped.entry(model.provider_id.clone()).or_insert_with(|| {
                order.push(model.provider_id.clone());
                BTreeMap::new()
            });
            models.insert(
                model.id,
                ProviderModel {
                    limit: model.limit,
                    name: model.name,
                    variants: Some(
                        model
                            .variants
                            .into_iter()
                            .map(|v| (v.id, serde::de::IgnoredAny))
                            .collect(),
                    ),
                },
            );
        }
        Ok(Self {
            all: Some(
                order
                    .into_iter()
                    .map(|id| Provider {
                        models: grouped.remove(&id),
                        name: Some(id.clone()),
                        id: Some(id),
                    })
                    .collect(),
            ),
            connected: None,
        })
    }

    fn visible(&self) -> impl Iterator<Item = (&str, &Provider)> {
        let connected: Option<Vec<&str>> = self
            .connected
            .as_ref()
            .filter(|c| !c.is_empty())
            .map(|c| c.iter().map(String::as_str).collect());
        self.all
            .iter()
            .flatten()
            .filter_map(|p| Some((p.id.as_deref()?, p)))
            .filter(move |(id, _)| connected.as_ref().is_none_or(|c| c.contains(id)))
    }

    /// The providers the server can use. `None` when it did not say (2.x lists enabled models,
    /// not providers).
    pub(crate) fn connected(&self) -> Option<&[String]> {
        self.connected.as_deref()
    }

    /// The models of the connected providers, `provider/model` each, sorted by name within a
    /// provider; `agent`, when there is one to choose, is offered as an option of each.
    pub(crate) fn models(&self, agent: Option<&ModelOption>) -> Vec<ModelInfo> {
        let mut out = Vec::new();
        for (provider_id, provider) in self.visible() {
            let provider_name = provider.name.as_deref().unwrap_or(provider_id);
            let mut models: Vec<ModelInfo> = provider
                .models
                .iter()
                .flatten()
                .map(|(model_id, model)| {
                    let mut efforts: Vec<Effort> = model
                        .variants
                        .iter()
                        .flatten()
                        .filter_map(|(id, _)| effort_of_variant(id))
                        .collect();
                    efforts.sort();
                    efforts.dedup();
                    ModelInfo {
                        id: format!("{provider_id}/{model_id}"),
                        display_name: Some(model.name.clone().unwrap_or_else(|| model_id.clone())),
                        description: Some(provider_name.to_owned()),
                        efforts,
                        default_effort: None,
                        options: agent.cloned().into_iter().collect(),
                    }
                })
                .collect();
            models.sort_by(|a, b| a.display_name.cmp(&b.display_name));
            out.extend(models);
        }
        out
    }

    /// The context window of each `provider/model`.
    pub(crate) fn context_windows(&self) -> HashMap<String, u64> {
        self.all
            .iter()
            .flatten()
            .flat_map(|provider| {
                let provider_id = provider.id.as_deref();
                provider
                    .models
                    .iter()
                    .flatten()
                    .filter_map(move |(id, model)| {
                        Some((
                            format!("{}/{id}", provider_id?),
                            model.limit.context.filter(|n| *n > 0)?,
                        ))
                    })
            })
            .collect()
    }

    /// `wanted` as a (provider, model) pair the server lists: `provider/model`, or a bare model id
    /// only one provider has. An error says why not.
    pub(crate) fn resolve(&self, wanted: &str) -> Result<(String, String), String> {
        let listed = |provider: &str, model: &str| {
            self.all.iter().flatten().any(|p| {
                p.id.as_deref() == Some(provider)
                    && p.models.as_ref().is_some_and(|m| m.contains_key(model))
            })
        };
        if let Some((provider, model)) = wanted.split_once('/')
            && listed(provider, model)
        {
            return Ok((provider.to_owned(), model.to_owned()));
        }
        let owners: Vec<&str> = self
            .visible()
            .filter(|(_, p)| p.models.as_ref().is_some_and(|m| m.contains_key(wanted)))
            .map(|(id, _)| id)
            .collect();
        match owners.as_slice() {
            [one] => Ok(((*one).to_owned(), wanted.to_owned())),
            [] => Err(format!(
                "opencode lists no model {wanted:?} (models are named provider/model; the \
                 provider must be connected: `opencode auth login`)"
            )),
            many => Err(format!(
                "the model {wanted:?} is offered by {}: name it provider/model",
                many.join(", ")
            )),
        }
    }

    /// The reasoning variant of `provider/model` that stands for `effort`, when the model has one.
    pub(crate) fn variant(
        &self,
        provider: &str,
        model: &str,
        effort: Option<Effort>,
    ) -> Option<String> {
        let variants = self
            .all
            .iter()
            .flatten()
            .find(|p| p.id.as_deref() == Some(provider))?
            .models
            .as_ref()?
            .get(model)?
            .variants
            .as_ref()?;
        variant_candidates(effort?)
            .iter()
            .find(|candidate| variants.contains_key(**candidate))
            .map(|candidate| (*candidate).to_owned())
    }
}

/// Preference-ordered variant ids for an effort (models.dev metadata, or `variants` in an
/// opencode config).
fn variant_candidates(effort: Effort) -> &'static [&'static str] {
    match effort {
        Effort::Minimal => &["minimal", "low"],
        Effort::Low => &["low", "minimal"],
        Effort::Medium => &["medium"],
        Effort::High => &["high"],
        Effort::XHigh => &["xhigh", "x-high", "high"],
        Effort::Max => &["max", "xhigh", "high"],
    }
}

fn effort_of_variant(id: &str) -> Option<Effort> {
    match id {
        "minimal" => Some(Effort::Minimal),
        "low" => Some(Effort::Low),
        "medium" => Some(Effort::Medium),
        "high" => Some(Effort::High),
        "xhigh" | "x-high" => Some(Effort::XHigh),
        "max" => Some(Effort::Max),
        _ => None,
    }
}

/// The choice of the agent option that means "the server's own default agent".
pub(crate) const DEFAULT_AGENT_CHOICE: &str = "default";

/// The agents a person may choose (`plan` among them) as a model option. 1.x lists them bare
/// (`[{name, description, mode, hidden?}]`), 2.x as `{data: [{id, name, mode, hidden}]}`. Hidden
/// agents (compaction, summary, title) and subagents are not a choice for a run.
pub(crate) fn agent_option(agents: &Value) -> Option<ModelOption> {
    let list = super::requests::unwrap_data(agents).as_array()?;
    let mut choices = vec![ModelOptionChoice {
        id: DEFAULT_AGENT_CHOICE.into(),
        label: "Server default".into(),
    }];
    for agent in list {
        if agent.get("hidden").and_then(Value::as_bool) == Some(true)
            || agent.get("mode").and_then(Value::as_str) == Some("subagent")
        {
            continue;
        }
        let name = agent.get("name").and_then(Value::as_str);
        let id = agent.get("id").and_then(Value::as_str).or(name);
        if let (Some(id), Some(name)) = (id, name)
            && !id.is_empty()
            && id != DEFAULT_AGENT_CHOICE
        {
            choices.push(ModelOptionChoice {
                id: id.into(),
                label: name.into(),
            });
        }
    }
    (choices.len() > 1).then(|| ModelOption {
        id: "agent".into(),
        label: "Agent".into(),
        choices,
        default_choice: DEFAULT_AGENT_CHOICE.into(),
        settable: true,
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use serde_json::json;

    use super::*;

    fn v1() -> ProviderCatalog {
        ProviderCatalog::from_v1(
            json!({
                "all": [
                    {"id": "opencode", "name": "OpenCode Zen", "models": {
                        "big-pickle": {"name": "Big Pickle", "limit": {"context": 200_000}, "variants": {}},
                        "gpt-x": {"name": "GPT X", "limit": {"context": 0},
                                  "variants": {"low": {}, "high": {"effort": "high"}, "weird": {}, "x-high": {}}},
                    }},
                    {"id": "anthropic", "name": "Anthropic", "models": {"claude": {"name": "Claude"}}},
                    {"id": "dup", "name": "Dup", "models": {"gpt-x": {}}},
                ],
                "connected": ["opencode", "dup"],
                "default": {"opencode": "big-pickle"},
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap()
    }

    #[test]
    fn only_connected_providers_are_listed_with_their_efforts_and_windows() {
        let models = v1().models(None);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["opencode/big-pickle", "opencode/gpt-x", "dup/gpt-x"]);
        let gpt = &models[1];
        assert_eq!(gpt.display_name.as_deref(), Some("GPT X"));
        assert_eq!(gpt.description.as_deref(), Some("OpenCode Zen"));
        assert_eq!(gpt.efforts, [Effort::Low, Effort::High, Effort::XHigh]);
        assert!(models[0].efforts.is_empty());
        let windows = v1().context_windows();
        assert_eq!(windows.get("opencode/big-pickle"), Some(&200_000));
        assert!(
            !windows.contains_key("opencode/gpt-x"),
            "a window of 0 is not one"
        );
    }

    #[test]
    fn a_model_is_named_provider_slash_model_or_by_a_unique_bare_id() {
        let catalog = v1();
        assert_eq!(
            catalog.resolve("opencode/big-pickle").unwrap(),
            ("opencode".into(), "big-pickle".into())
        );
        assert_eq!(catalog.resolve("big-pickle").unwrap().0, "opencode");
        let ambiguous = catalog.resolve("gpt-x").unwrap_err();
        assert!(ambiguous.contains("opencode, dup") && ambiguous.contains("provider/model"));
        let missing = catalog.resolve("anthropic/claude");
        // Listed in `all`, so it resolves even though its provider is not connected: opencode
        // answers "Model not found" itself, and the person is told by the run's error.
        assert!(missing.is_ok());
        assert!(
            catalog
                .resolve("nope")
                .unwrap_err()
                .contains("lists no model")
        );
    }

    #[test]
    fn an_effort_picks_the_first_variant_the_model_has() {
        let catalog = v1();
        assert_eq!(
            catalog
                .variant("opencode", "gpt-x", Some(Effort::High))
                .as_deref(),
            Some("high")
        );
        assert_eq!(
            catalog
                .variant("opencode", "gpt-x", Some(Effort::XHigh))
                .as_deref(),
            Some("x-high")
        );
        // No `max` or `xhigh` variant: the nearest below it.
        assert_eq!(
            catalog
                .variant("opencode", "gpt-x", Some(Effort::Max))
                .as_deref(),
            Some("high")
        );
        assert_eq!(
            catalog.variant("opencode", "gpt-x", Some(Effort::Medium)),
            None
        );
        assert_eq!(catalog.variant("opencode", "gpt-x", None), None);
        assert_eq!(
            catalog.variant("opencode", "big-pickle", Some(Effort::High)),
            None
        );
    }

    #[test]
    fn the_v2_flat_list_folds_into_providers_and_skips_disabled_models() {
        let catalog = ProviderCatalog::from_v2(
            json!({"location": {}, "data": [
                {"id": "big-pickle", "providerID": "opencode", "name": "Big Pickle",
                 "limit": {"context": 200_000}, "variants": [{"id": "high", "settings": {}}], "enabled": true},
                {"id": "off", "providerID": "opencode", "enabled": false},
                {"id": "m", "providerID": "other"},
            ]})
            .to_string()
            .as_bytes(),
        )
        .unwrap();
        let ids: Vec<String> = catalog.models(None).into_iter().map(|m| m.id).collect();
        assert_eq!(ids, ["opencode/big-pickle", "other/m"]);
        assert_eq!(
            catalog
                .variant("opencode", "big-pickle", Some(Effort::High))
                .as_deref(),
            Some("high")
        );
        assert!(catalog.connected().is_none());
    }

    #[test]
    fn the_agent_option_offers_the_visible_primary_agents_in_both_shapes() {
        let bare = json!([
            {"name": "build", "mode": "primary"},
            {"name": "plan", "mode": "primary"},
            {"name": "general", "mode": "subagent"},
            {"name": "title", "mode": "primary", "hidden": true},
        ]);
        let wrapped = json!({"data": [{"id": "build", "name": "Build", "mode": "primary", "hidden": false}, {"id": "plan", "name": "Plan", "mode": "primary"}]});
        for (agents, labels) in [(bare, ["build", "plan"]), (wrapped, ["Build", "Plan"])] {
            let option = agent_option(&agents).unwrap();
            let got: Vec<&str> = option
                .choices
                .iter()
                .skip(1)
                .map(|c| c.label.as_str())
                .collect();
            assert_eq!(got, labels);
            assert_eq!(option.choices[1].id, "build");
            assert_eq!(option.default_choice, "default");
        }
        assert!(agent_option(&json!([])).is_none());
        let with =
            v1().models(agent_option(&json!([{"name": "plan", "mode": "primary"}])).as_ref());
        assert_eq!(with[0].options[0].id, "agent");
    }

    #[test]
    fn a_catalog_that_names_no_connected_provider_lists_every_one() {
        let catalog = ProviderCatalog::from_v1(
            json!({"all": [{"id": "a", "models": {"m": {}}}], "connected": []})
                .to_string()
                .as_bytes(),
        )
        .unwrap();
        assert_eq!(catalog.models(None).len(), 1);
    }
}
