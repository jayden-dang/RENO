//! `model/list` as Reno's `ModelInfo`s, and the effort words both ways.
//!
//! A row is Meta's (`providerId` `meta`, the one provider a run pins), newest first as the host
//! sends them, the catalog's default row moved to the front. Muse's tiers are per model
//! (`variants`); the tiers Reno's `Effort` has no word for (`none`, `ultra`) are left out of what
//! a person can choose, and an `"unknown"` variants value (an older host) falls back to the
//! common ladder. A logged-out host answers with an empty catalog (`source: bundledCatalog`), so
//! an empty list is not a failure but a missing sign-in, which `account/read` reports.

use std::collections::HashSet;

use reno_proto::{Effort, ModelInfo};
use serde_json::Value;

use super::frames::string;

/// The tiers an older host implies when a row reports none (`"unknown"`).
const FALLBACK_TIERS: &[&str] = &["low", "medium", "high", "xhigh", "max"];

/// The model id a run passes for "the harness's own choice".
pub(crate) const DEFAULT_MODEL: &str = "default";

/// The curated list a host that has not answered offers: whatever model Muse itself defaults to.
pub(crate) fn default_models() -> Vec<ModelInfo> {
    vec![ModelInfo {
        id: DEFAULT_MODEL.into(),
        display_name: Some("Muse default".into()),
        description: Some("The model Muse Code picks for the account".into()),
        efforts: Vec::new(),
        default_effort: None,
        options: Vec::new(),
    }]
}

/// Whether `model` names no model at all (the run uses Muse's own).
pub(crate) fn is_default_model(model: Option<&str>) -> bool {
    model
        .map(str::trim)
        .is_none_or(|m| m.is_empty() || m == DEFAULT_MODEL)
}

/// The tier word Muse takes for `effort`.
pub(crate) fn tier(effort: Effort) -> &'static str {
    effort.as_str()
}

/// Meta's rows from a `model/list` result, the catalog default first.
pub(crate) fn model_infos(result: &Value) -> Vec<ModelInfo> {
    let mut seen = HashSet::new();
    let mut rows: Vec<(bool, ModelInfo)> = result["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|row| matches!(row["providerId"].as_str(), None | Some("meta")))
        .filter_map(|row| {
            let id = string(row, "modelId").trim();
            if id.is_empty() || !seen.insert(id.to_owned()) {
                return None;
            }
            let label = string(row, "displayLabel").trim();
            let tiers: Vec<&str> = match row["variants"].as_array() {
                Some(tiers) => tiers.iter().filter_map(Value::as_str).collect(),
                None => FALLBACK_TIERS.to_vec(),
            };
            Some((
                row["isDefault"] == true,
                ModelInfo {
                    id: id.to_owned(),
                    display_name: Some(
                        if label.is_empty() || (label == id && id.starts_with("muse-")) {
                            pretty(id)
                        } else {
                            label.to_owned()
                        },
                    ),
                    description: row["description"]
                        .as_str()
                        .map(str::trim)
                        .filter(|d| !d.is_empty())
                        .map(str::to_owned),
                    efforts: tiers.into_iter().filter_map(Effort::from_word).collect(),
                    default_effort: row["defaultReasoningEffort"]
                        .as_str()
                        .and_then(Effort::from_word),
                    options: Vec::new(),
                },
            ))
        })
        .collect();
    // Stable: the default first, the host's order (newest first) after it.
    rows.sort_by_key(|(default, _)| !*default);
    rows.into_iter().map(|(_, model)| model).collect()
}

/// `muse-spark-1.2` as `Muse Spark 1.2`.
fn pretty(id: &str) -> String {
    id.split('-')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) if first.is_ascii_lowercase() => {
                    first.to_ascii_uppercase().to_string() + chars.as_str()
                }
                _ => word.to_owned(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn meta_rows_map_with_their_tiers_default_first() {
        let models = model_infos(&json!({"providerId": "meta", "models": [
            {"modelId": "muse-spark-1.3", "displayLabel": "muse-spark-1.3", "providerId": "meta",
             "isDefault": false, "variants": ["none", "low", "medium", "high", "ultra"],
             "description": null},
            {"modelId": "muse-spark-1.2", "displayLabel": "Muse Spark", "providerId": "meta",
             "isDefault": true, "variants": "unknown", "defaultReasoningEffort": "high",
             "description": "Fast"},
            {"modelId": "other", "displayLabel": "Other", "providerId": "elsewhere"},
            {"modelId": "muse-spark-1.3", "displayLabel": "dup", "providerId": "meta"}
        ]}));
        assert_eq!(
            models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            ["muse-spark-1.2", "muse-spark-1.3"]
        );
        assert_eq!(models[0].description.as_deref(), Some("Fast"));
        assert_eq!(models[0].display_name.as_deref(), Some("Muse Spark"));
        assert_eq!(models[0].default_effort, Some(Effort::High));
        // `"unknown"` variants fall back to the common ladder.
        assert_eq!(models[0].efforts.len(), 5);
        // The id reads as a name, and a tier Reno has no word for is not offered.
        assert_eq!(models[1].display_name.as_deref(), Some("Muse Spark 1.3"));
        assert_eq!(
            models[1].efforts,
            [Effort::Low, Effort::Medium, Effort::High]
        );
        assert_eq!(models[1].default_effort, None);
    }

    #[test]
    fn a_logged_out_host_has_no_models_and_the_default_model_is_no_model() {
        assert!(
            model_infos(&json!({"providerId": "meta", "source": "bundledCatalog", "models": []}))
                .is_empty()
        );
        assert!(model_infos(&json!({})).is_empty());
        assert!(is_default_model(None));
        assert!(is_default_model(Some(" ")));
        assert!(is_default_model(Some("default")));
        assert!(!is_default_model(Some("muse-spark-1.3")));
        assert_eq!(default_models()[0].id, DEFAULT_MODEL);
    }

    #[test]
    fn every_effort_has_the_word_muse_takes() {
        for effort in Effort::ALL {
            assert_eq!(Effort::from_word(tier(effort)), Some(effort));
        }
    }
}
