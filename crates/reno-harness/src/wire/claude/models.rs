//! The model list in the CLI's answer to the `initialize` control request.
//!
//! `response.response.models` is a list of `{value, resolvedModel, displayName, description}`
//! (verified live against 2.1.326, fixture `live-2.1.326-initialize-models.json`): `value` is
//! what `--model` / `set_model` accept (an alias such as `sonnet`, `default`, or a full id), and
//! `resolvedModel` the id it means. Several values can mean one model.

use reno_proto::ModelInfo;
use serde::Deserialize;
use serde_json::Value;

/// What the CLI lists: one row per distinct resolved model, and the names that reach them.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct LiveModels {
    /// One row per resolved model, in the CLI's order.
    pub models: Vec<ModelInfo>,
    /// Every listed `value` that is not itself a resolved id and is not `default` (which clears
    /// an override rather than naming a model).
    pub aliases: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Row {
    #[serde(default)]
    value: String,
    #[serde(default)]
    resolved_model: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

/// The `response.response` payload of the `initialize` answer as a model list; `None` when it
/// lists no usable model (an older CLI, or an answer of another shape).
pub(crate) fn parse_initialize(payload: &Value) -> Option<LiveModels> {
    let rows: Vec<Row> = payload
        .get("models")?
        .as_array()?
        .iter()
        .filter_map(|row| serde_json::from_value(row.clone()).ok())
        .filter(|row: &Row| !row.value.is_empty() && !row.resolved_model.is_empty())
        .collect();
    let mut live = LiveModels::default();
    // The `default` row repeats whichever model the account defaults to, under a label that
    // only means "default"; it is a model row only when no other row names that model.
    let (defaults, named): (Vec<Row>, Vec<Row>) =
        rows.into_iter().partition(|row| row.value == "default");
    for row in named.into_iter().chain(defaults) {
        if row.value != row.resolved_model
            && row.value != "default"
            && !live.aliases.contains(&row.value)
        {
            live.aliases.push(row.value.clone());
        }
        if live
            .models
            .iter()
            .all(|known| known.id != row.resolved_model)
        {
            live.models.push(ModelInfo {
                id: row.resolved_model,
                display_name: row.display_name.filter(|name| !name.is_empty()),
                description: row.description.filter(|text| !text.is_empty()),
            });
        }
    }
    (!live.models.is_empty()).then_some(live)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIVE: &str =
        include_str!("../../../tests/fixtures/claude/live-2.1.326-initialize-models.json");

    fn payload() -> Value {
        let frame: Value = serde_json::from_str(LIVE).unwrap();
        frame["response"]["response"].clone()
    }

    #[test]
    fn the_live_answer_is_one_row_per_resolved_model_in_the_clis_order() {
        let live = parse_initialize(&payload()).unwrap();
        let ids: Vec<&str> = live.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "claude-opus-5-5",
                "claude-fable-5-2",
                "claude-sonnet-5-5",
                "claude-haiku-5-5",
                "claude-fable-5-1",
                "claude-haiku-4-5-20251001",
                "claude-sonnet-5",
                "claude-opus-5",
                "claude-fable-5",
                "claude-opus-4-8",
                "claude-opus-4-7",
                "claude-opus-4-6",
                "claude-sonnet-4-6",
            ]
        );
        // The model row is the named one, not the "Default (recommended)" repeat.
        assert_eq!(live.models[0].display_name.as_deref(), Some("Opus 5.5"));
        assert!(live.models[0].description.is_some());
    }

    #[test]
    fn values_that_differ_from_their_model_are_aliases_and_default_is_not() {
        let live = parse_initialize(&payload()).unwrap();
        assert_eq!(live.aliases, ["opus", "fable", "sonnet", "haiku"]);
    }

    #[test]
    fn an_answer_without_models_is_not_a_catalog() {
        for payload in [
            serde_json::json!({}),
            serde_json::json!({"models": []}),
            serde_json::json!({"models": "x"}),
            serde_json::json!({"models": [{"value": "x"}]}),
        ] {
            assert_eq!(parse_initialize(&payload), None, "{payload}");
        }
    }

    #[test]
    fn the_fixture_holds_no_account() {
        assert!(!LIVE.contains("account") && !LIVE.contains('@'));
    }
}
