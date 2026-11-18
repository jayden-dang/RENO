//! `Cursor.models.list()` as Reno's model list, and the defaults a run fills in.
//!
//! An item of the SDK's list (`ModelListItem`, 1.0.32 `options.d.ts`) is `{id, displayName,
//! description?, aliases?, parameters?: [{id, displayName?, values: [{value, displayName?}]}],
//! variants?: [{params: [{id, value}], isDefault?, isMaxMode?}]}`. The shim forwards the
//! parameters and the default variant only.

use std::collections::{BTreeMap, HashSet};

use reno_proto::{ModelInfo, ModelOption, ModelOptionChoice};
use serde_json::Value;

/// The model a run uses when none is named: Cursor's own router.
pub(crate) const AUTO: &str = "auto";

/// What a listing says: the models, and which ones default to a Max Mode variant.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Listed {
    /// The picker's models.
    pub(crate) models: Vec<ModelInfo>,
    /// Models whose default variant is a Max Mode one. Their defaults are never sent: the
    /// backend can reject the very variant its catalog marks default (zeron saw Grok 4.7's 500k
    /// default fail "Invalid parameters" while the bare id ran).
    pub(crate) max_mode: HashSet<String>,
}

/// The list a Cursor that has not answered offers.
pub(crate) fn curated() -> Vec<ModelInfo> {
    vec![ModelInfo {
        id: AUTO.into(),
        display_name: Some("Auto".into()),
        description: Some("Cursor picks the model".into()),
        efforts: Vec::new(),
        default_effort: None,
        options: Vec::new(),
    }]
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn array<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn default_variant(item: &Value) -> Option<&Value> {
    array(item, "variants")
        .iter()
        .find(|variant| variant.get("isDefault").and_then(Value::as_bool) == Some(true))
}

/// The picker's models and the Max Mode set, from the `items` of the shim's `models` frame.
pub(crate) fn listed(items: &Value) -> Listed {
    let mut listed = Listed::default();
    for item in items.as_array().map(Vec::as_slice).unwrap_or_default() {
        let Some(id) = text(item, "id") else {
            continue;
        };
        // `default` is a bare alias twin of the parameterized Auto entry: two "Auto" rows would
        // only confuse the picker.
        if id == "default" {
            continue;
        }
        let variant = default_variant(item);
        if variant.and_then(|v| v.get("isMaxMode").and_then(Value::as_bool)) == Some(true) {
            listed.max_mode.insert(id.clone());
        }
        // A variant marked default carries the catalog's preferred value for each parameter
        // (Auto's `optimize_for` is `balanced`).
        let preferred = variant.map(|v| array(v, "params")).unwrap_or_default();
        let options = array(item, "parameters")
            .iter()
            .filter_map(|parameter| {
                let option_id = text(parameter, "id")?;
                let choices: Vec<ModelOptionChoice> = array(parameter, "values")
                    .iter()
                    .filter_map(|choice| {
                        let value = text(choice, "value")?;
                        Some(ModelOptionChoice {
                            label: text(choice, "displayName").unwrap_or_else(|| value.clone()),
                            id: value,
                        })
                    })
                    .collect();
                let first = choices.first()?.id.clone();
                let default_choice = preferred
                    .iter()
                    .find(|p| text(p, "id").as_deref() == Some(option_id.as_str()))
                    .and_then(|p| text(p, "value"))
                    .unwrap_or(first);
                Some(ModelOption {
                    label: text(parameter, "displayName").unwrap_or_else(|| option_id.clone()),
                    id: option_id,
                    choices,
                    default_choice,
                    settable: true,
                })
            })
            .collect();
        listed.models.push(ModelInfo {
            display_name: Some(text(item, "displayName").unwrap_or_else(|| id.clone())),
            description: text(item, "description"),
            efforts: Vec::new(),
            default_effort: None,
            options,
            id,
        });
    }
    listed
}

/// Send every parameter the run left implicit as the account catalog's default, the value the
/// picker shows. Cursor resolves a bare model id on its side, which on some plans (Plus) is a
/// variant the account cannot run, while the catalog's default for the credential is one it can.
/// A model whose default is a Max Mode variant gets none (see [`Listed::max_mode`]).
pub(crate) fn fill_defaults(
    listed: &Listed,
    model: Option<&str>,
    options: &mut BTreeMap<String, String>,
) {
    let Some(model) = model.and_then(|id| listed.models.iter().find(|m| m.id == id)) else {
        return;
    };
    if listed.max_mode.contains(&model.id) {
        return;
    }
    for option in &model.options {
        options
            .entry(option.id.clone())
            .or_insert_with(|| option.default_choice.clone());
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// 1.0.28's real shapes: parameterized Auto, its bare `default` twin, a plain model, and one
    /// whose default variant is Max Mode.
    fn items() -> Value {
        json!([
            {"id":"auto-smart","displayName":"Auto","parameters":[{"id":"optimize_for","displayName":"Optimize For","values":[{"value":"intelligence","displayName":"Intelligence"},{"value":"balanced","displayName":"Balance"},{"value":"cost","displayName":"Cost"}]}],"variants":[{"params":[{"id":"optimize_for","value":"balanced"}],"isDefault":true}]},
            {"id":"default","displayName":"Auto","aliases":["auto"]},
            {"id":"claude-fable-5","displayName":"Claude Fable 5","description":"Anthropic frontier","parameters":[{"id":"thinking","values":[{"value":"enabled"},{"value":"disabled"}]}]},
            {"id":"grok-4.7","displayName":"Grok 4.7","parameters":[{"id":"context","values":[{"value":"256k"},{"value":"500k"}]}],"variants":[{"params":[{"id":"context","value":"500k"}],"isDefault":true,"isMaxMode":true}]}
        ])
    }

    #[test]
    fn the_catalog_becomes_models_options_and_catalog_defaults() {
        let listed = listed(&items());
        let ids: Vec<_> = listed.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["auto-smart", "claude-fable-5", "grok-4.7"]);
        let auto = &listed.models[0];
        assert_eq!(auto.display_name.as_deref(), Some("Auto"));
        assert_eq!(auto.options[0].id, "optimize_for");
        assert_eq!(auto.options[0].default_choice, "balanced");
        assert_eq!(auto.options[0].choices.len(), 3);
        // With no default variant the first value is the default; labels fall back to values.
        let fable = &listed.models[1];
        assert_eq!(fable.description.as_deref(), Some("Anthropic frontier"));
        assert_eq!(fable.options[0].default_choice, "enabled");
        assert_eq!(fable.options[0].choices[0].label, "enabled");
        assert!(listed.max_mode.contains("grok-4.7") && listed.max_mode.len() == 1);
    }

    #[test]
    fn a_parameter_with_no_values_and_an_item_with_no_id_are_left_out() {
        let listed = listed(&json!([
            {"displayName":"nameless"},
            {"id":"m","parameters":[{"id":"empty","values":[]},{"values":[{"value":"x"}]}]},
        ]));
        assert_eq!(listed.models.len(), 1);
        assert!(listed.models[0].options.is_empty());
        assert_eq!(super::listed(&json!("not a list")), Listed::default());
    }

    #[test]
    fn implicit_options_take_the_catalog_default_and_explicit_ones_stay() {
        let listed = listed(&items());
        let mut implicit = BTreeMap::new();
        fill_defaults(&listed, Some("auto-smart"), &mut implicit);
        assert_eq!(implicit["optimize_for"], "balanced");
        let mut explicit = BTreeMap::from([("optimize_for".to_owned(), "intelligence".to_owned())]);
        fill_defaults(&listed, Some("auto-smart"), &mut explicit);
        assert_eq!(explicit["optimize_for"], "intelligence");
    }

    #[test]
    fn a_max_mode_default_an_unknown_model_and_no_model_send_nothing() {
        let listed = listed(&items());
        for model in [Some("grok-4.7"), Some("not-listed"), None] {
            let mut options = BTreeMap::new();
            fill_defaults(&listed, model, &mut options);
            assert!(options.is_empty(), "{model:?}");
        }
    }

    #[test]
    fn the_curated_list_is_auto_alone() {
        let models = curated();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, AUTO);
    }
}
