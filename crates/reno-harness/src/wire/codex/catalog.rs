//! Model catalog and the wire spellings of effort and sandbox for Codex.
//!
//! The live catalog comes from the app server's paginated `model/list` (experimentalApi); see
//! [`parse_model_page`]. The snapshot below is the offline fallback, kept in newest-first order
//! so the descriptor stays useful when discovery has not run or cannot.
//!
//! Sans-IO (ARCH-6).

use reno_proto::{Effort, ModelInfo, SandboxLevel};
use serde_json::Value;

use crate::upstream_proto::{Model, ModelOption, ModelOptionChoice, ReasoningLevel};

/// `turn/start`'s `effort`.
pub(crate) fn effort(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
    }
}

/// `thread/start`'s `sandbox` param (kebab-case wire words).
pub(crate) fn sandbox_mode(sandbox: SandboxLevel) -> &'static str {
    match sandbox {
        SandboxLevel::ReadOnly => "read-only",
        SandboxLevel::WorkspaceWrite => "workspace-write",
        SandboxLevel::Unrestricted => "danger-full-access",
    }
}

/// `turn/start`'s `sandboxPolicy.type` (camelCase variant of the same policy).
pub(crate) fn sandbox_policy_type(sandbox: SandboxLevel) -> &'static str {
    match sandbox {
        SandboxLevel::ReadOnly => "readOnly",
        SandboxLevel::WorkspaceWrite => "workspaceWrite",
        SandboxLevel::Unrestricted => "dangerFullAccess",
    }
}

/// `turn/start`'s full `sandboxPolicy` object. Workspace-write (auto-accept edits) keeps network
/// access: agents fetch dependencies and hit APIs, and a network-less sandbox would ask before
/// every one of those commands.
pub(crate) fn sandbox_policy_value(sandbox: SandboxLevel) -> Value {
    let mut policy = serde_json::Map::new();
    policy.insert("type".into(), sandbox_policy_type(sandbox).into());
    if matches!(sandbox, SandboxLevel::WorkspaceWrite) {
        policy.insert("networkAccess".into(), true.into());
    }
    Value::Object(policy)
}

/// One `model/list` page: the visible models (each with whether the account's default) and the
/// cursor of the next page. A hidden model, or one with no id, is skipped on its own: the rest
/// of the page is kept.
pub(crate) fn parse_model_page(result: &Value) -> (Vec<(ModelInfo, bool)>, Option<String>) {
    let mut models = Vec::new();
    for item in result
        .get("data")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        if item.get("hidden").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        let Some(id) = item
            .get("model")
            .and_then(Value::as_str)
            .or_else(|| item.get("id").and_then(Value::as_str))
            .map(str::trim)
            .filter(|id| !id.is_empty())
        else {
            continue;
        };
        let display_name = item
            .get("displayName")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|label| !label.is_empty())
            .unwrap_or(id)
            .to_owned();
        models.push((
            ModelInfo {
                id: id.to_owned(),
                display_name: Some(display_name),
            },
            item.get("isDefault").and_then(Value::as_bool) == Some(true),
        ));
    }
    let next_cursor = result
        .get("nextCursor")
        .and_then(Value::as_str)
        .map(str::to_owned);
    (models, next_cursor)
}

/// The curated fallback as the descriptor carries it.
pub(crate) fn static_model_infos() -> Vec<ModelInfo> {
    static_models()
        .into_iter()
        .map(|model| ModelInfo {
            id: model.id,
            display_name: Some(model.label),
        })
        .collect()
}

const ULTRA_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
    ReasoningLevel::Ultra,
];

const MAX_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
    ReasoningLevel::Max,
];

const XHIGH_LADDER: &[ReasoningLevel] = &[
    ReasoningLevel::Low,
    ReasoningLevel::Medium,
    ReasoningLevel::High,
    ReasoningLevel::XHigh,
];

/// The service-tier select the app server reports per model (`serviceTiers` /
/// `additionalSpeedTiers` in `model/list`); "default" means Standard and is
/// omitted from the wire params entirely.
fn service_tier() -> ModelOption {
    ModelOption {
        id: "serviceTier".into(),
        label: "Service Tier".into(),
        choices: vec![
            ModelOptionChoice {
                id: "default".into(),
                label: "Standard".into(),
            },
            ModelOptionChoice {
                id: "fast".into(),
                label: "Fast".into(),
            },
        ],
        default_choice: "default".into(),
    }
}

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

/// The curated fallback catalog: newest family first, with efforts as the
/// app server reports them. Daybreak Blue reports NO service tiers, so it
/// carries no trait — sending `serviceTier: priority` for it would be
/// rejected. The live `model/list` remains authoritative whenever available.
pub(crate) fn static_models() -> Vec<Model> {
    vec![
        model(
            "gpt-6-astra",
            "GPT-6-Astra",
            "Our most capable model for complex, demanding work.",
            ULTRA_LADDER,
            vec![service_tier()],
        ),
        model(
            "gpt-5.6-sol",
            "GPT-5.6-Sol",
            "Frontier reasoning flagship",
            ULTRA_LADDER,
            vec![service_tier()],
        ),
        model(
            "gpt-5.6-terra",
            "GPT-5.6-Terra",
            "Deep multi-step agentic work",
            ULTRA_LADDER,
            vec![service_tier()],
        ),
        model(
            "gpt-5.6-luna",
            "GPT-5.6-Luna",
            "Fast frontier model",
            MAX_LADDER,
            vec![service_tier()],
        ),
        model(
            "gpt-daybreak-blue-latest",
            "Daybreak Blue",
            "Frontier model for defensive cybersecurity work",
            ULTRA_LADDER,
            Vec::new(),
        ),
        model(
            "gpt-5.5",
            "GPT-5.5",
            "Previous generation flagship",
            XHIGH_LADDER,
            vec![service_tier()],
        ),
        model(
            "gpt-5.4",
            "GPT-5.4",
            "Reliable general coding",
            XHIGH_LADDER,
            vec![service_tier()],
        ),
        model(
            "gpt-5.4-mini",
            "GPT-5.4-Mini",
            "Small, fast and capable",
            XHIGH_LADDER,
            vec![service_tier()],
        ),
        model(
            "gpt-5.3-codex-spark",
            "GPT-5.3-Codex-Spark",
            "Ultra-fast lightweight coding",
            XHIGH_LADDER,
            vec![service_tier()],
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    use serde_json::json;

    #[test]
    fn effort_and_sandbox_have_their_wire_spellings() {
        assert_eq!(effort(Effort::Low), "low");
        assert_eq!(effort(Effort::Medium), "medium");
        assert_eq!(effort(Effort::High), "high");
        assert_eq!(sandbox_mode(SandboxLevel::ReadOnly), "read-only");
        assert_eq!(
            sandbox_mode(SandboxLevel::WorkspaceWrite),
            "workspace-write"
        );
        assert_eq!(
            sandbox_mode(SandboxLevel::Unrestricted),
            "danger-full-access"
        );
        assert_eq!(
            sandbox_policy_value(SandboxLevel::Unrestricted),
            json!({"type": "dangerFullAccess"})
        );
        assert_eq!(
            sandbox_policy_value(SandboxLevel::WorkspaceWrite),
            json!({"type": "workspaceWrite", "networkAccess": true})
        );
        assert_eq!(
            sandbox_policy_value(SandboxLevel::ReadOnly),
            json!({"type": "readOnly"})
        );
    }

    #[test]
    fn a_model_page_skips_hidden_models_and_keeps_the_rest() {
        let page = json!({
            "data": [
                { "id": "hidden", "model": "hidden", "displayName": "Hidden", "hidden": true },
                { "id": "", "model": "  " },
                {
                    "id": "gpt-6-astra",
                    "model": "gpt-6-astra",
                    "displayName": " GPT-6-Astra ",
                    "hidden": false,
                    "supportedReasoningEfforts": [{ "reasoningEffort": "future" }],
                    "isDefault": true
                },
                // An older build: no `model`, no name, no hidden flag.
                { "id": "gpt-old" }
            ],
            "nextCursor": "next"
        });
        let (models, cursor) = parse_model_page(&page);
        assert_eq!(cursor.as_deref(), Some("next"));
        assert_eq!(
            models,
            vec![
                (
                    ModelInfo {
                        id: "gpt-6-astra".into(),
                        display_name: Some("GPT-6-Astra".into())
                    },
                    true
                ),
                (
                    ModelInfo {
                        id: "gpt-old".into(),
                        display_name: Some("gpt-old".into())
                    },
                    false
                ),
            ]
        );
        assert_eq!(parse_model_page(&json!({})).1, None);
    }

    #[test]
    fn the_descriptor_fallback_is_the_curated_list() {
        let infos = static_model_infos();
        assert_eq!(infos.len(), static_models().len());
        assert_eq!(infos[0].id, "gpt-6-astra");
        assert_eq!(infos[0].display_name.as_deref(), Some("GPT-6-Astra"));
    }

    #[test]
    fn catalog_is_newest_first_with_service_tiers() {
        let models = static_models();
        assert_eq!(models.len(), 9);
        assert_eq!(models[0].id, "gpt-6-astra");
        assert!(models[0].reasoning_levels.contains(&ReasoningLevel::Ultra));
        assert!(!models[5].reasoning_levels.contains(&ReasoningLevel::Max));
        for m in &models {
            let tier = m.options.iter().find(|o| o.id == "serviceTier");
            // Daybreak Blue reports no service tiers on the wire.
            if m.id == "gpt-daybreak-blue-latest" {
                assert!(tier.is_none(), "{} must not carry serviceTier", m.id);
            } else {
                assert!(tier.is_some(), "{} missing serviceTier", m.id);
            }
        }
    }

    #[test]
    fn daybreak_blue_rides_the_full_ultra_ladder() {
        let models = static_models();
        let daybreak = models
            .iter()
            .find(|m| m.id == "gpt-daybreak-blue-latest")
            .expect("daybreak blue in catalog");
        assert_eq!(daybreak.label, "Daybreak Blue");
        assert!(daybreak.reasoning_levels.contains(&ReasoningLevel::Ultra));
        assert!(daybreak.options.is_empty());
    }
}
