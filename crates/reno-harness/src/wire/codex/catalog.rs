//! Model catalog and the wire spellings of effort and sandbox for Codex.
//!
//! The live catalog comes from the app server's paginated `model/list` (experimentalApi); see
//! [`parse_model_page`]. The snapshot below is the offline fallback, kept in newest-first order
//! so the descriptor stays useful when discovery has not run or cannot.
//!
//! Sans-IO (ARCH-6).

use reno_proto::{Effort, ModelInfo, SandboxLevel, SignedIn};
use serde_json::Value;

/// `turn/start`'s `effort`.
pub(crate) fn effort(effort: Effort) -> &'static str {
    effort.as_str()
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
                default_effort: item
                    .get("defaultReasoningEffort")
                    .and_then(Value::as_str)
                    .and_then(Effort::from_word),
                efforts: supported_efforts(item),
                id: id.to_owned(),
                display_name: Some(display_name),
                description: None,
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

/// `supportedReasoningEfforts`: objects `{reasoningEffort, description}` (0.169 schema) or bare
/// words from an older build. Words Reno does not know (`ultra`) are skipped.
fn supported_efforts(item: &Value) -> Vec<Effort> {
    let mut levels: Vec<Effort> = Vec::new();
    for entry in item
        .get("supportedReasoningEfforts")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
    {
        let word = entry
            .as_str()
            .or_else(|| entry.get("reasoningEffort").and_then(Value::as_str));
        if let Some(level) = word.and_then(Effort::from_word)
            && !levels.contains(&level)
        {
            levels.push(level);
        }
    }
    levels
}

/// The curated fallback catalog: newest family first. The live `model/list` remains
/// authoritative whenever available.
const CURATED: &[(&str, &str)] = &[
    ("gpt-6-astra", "GPT-6-Astra"),
    ("gpt-5.6-sol", "GPT-5.6-Sol"),
    ("gpt-5.6-terra", "GPT-5.6-Terra"),
    ("gpt-5.6-luna", "GPT-5.6-Luna"),
    ("gpt-daybreak-blue-latest", "Daybreak Blue"),
    ("gpt-5.5", "GPT-5.5"),
    ("gpt-5.4", "GPT-5.4"),
    ("gpt-5.4-mini", "GPT-5.4-Mini"),
    ("gpt-5.3-codex-spark", "GPT-5.3-Codex-Spark"),
];

/// The curated fallback as the descriptor carries it.
pub(crate) fn static_model_infos() -> Vec<ModelInfo> {
    CURATED
        .iter()
        .map(|(id, label)| ModelInfo {
            default_effort: None,
            efforts: vec![],
            id: (*id).into(),
            display_name: Some((*label).into()),
            description: None,
        })
        .collect()
}

/// What to tell a person whose Codex is signed out.
const SIGN_IN_HINT: &str = "run `codex login`";

/// The sign-in state an `account/read` reply describes.
///
/// The reply is `{ account: { type: "chatgpt" | "apiKey", email?, .. } | null,
/// requiresOpenaiAuth: bool }`. An account is a login. No account while the configured provider
/// requires OpenAI auth is signed out. No account and no such requirement (a custom provider
/// that brings its own key) is not something this can judge, so it is `Unknown`, as is any other
/// shape.
pub(crate) fn account_state(reply: &Value) -> SignedIn {
    if let Some(account) = reply.get("account").filter(|account| account.is_object()) {
        return SignedIn::Yes {
            account: account
                .get("email")
                .and_then(Value::as_str)
                .filter(|email| !email.is_empty())
                .map(str::to_owned),
        };
    }
    match reply.get("requiresOpenaiAuth").and_then(Value::as_bool) {
        Some(true) => SignedIn::No {
            hint: SIGN_IN_HINT.into(),
        },
        Some(false) | None => SignedIn::Unknown,
    }
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
                    "supportedReasoningEfforts": [
                        { "reasoningEffort": "future", "description": "" },
                        { "reasoningEffort": "medium", "description": "" },
                        "xhigh"
                    ],
                    "defaultReasoningEffort": "medium",
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
                        default_effort: Some(Effort::Medium),
                        efforts: vec![Effort::Medium, Effort::XHigh],
                        id: "gpt-6-astra".into(),
                        display_name: Some("GPT-6-Astra".into()),
                        description: None,
                    },
                    true
                ),
                (
                    ModelInfo {
                        default_effort: None,
                        efforts: vec![],
                        id: "gpt-old".into(),
                        display_name: Some("gpt-old".into()),
                        description: None,
                    },
                    false
                ),
            ]
        );
        assert_eq!(parse_model_page(&json!({})).1, None);
    }

    #[test]
    fn account_replies_map_to_a_sign_in_state() {
        assert_eq!(
            account_state(
                &json!({"account": {"type": "chatgpt", "email": "a@b.c", "planType": "pro"}, "requiresOpenaiAuth": true})
            ),
            SignedIn::Yes {
                account: Some("a@b.c".into())
            }
        );
        assert_eq!(
            account_state(&json!({"account": {"type": "apiKey"}, "requiresOpenaiAuth": true})),
            SignedIn::Yes { account: None }
        );
        assert_eq!(
            account_state(&json!({"account": null, "requiresOpenaiAuth": true})),
            SignedIn::No {
                hint: SIGN_IN_HINT.into()
            }
        );
        // A provider that needs no OpenAI login: nothing to say.
        assert_eq!(
            account_state(&json!({"account": null, "requiresOpenaiAuth": false})),
            SignedIn::Unknown
        );
        assert_eq!(account_state(&json!({})), SignedIn::Unknown);
        assert_eq!(account_state(&json!([])), SignedIn::Unknown);
    }

    #[test]
    fn the_descriptor_fallback_is_the_curated_list_newest_first() {
        let infos = static_model_infos();
        assert_eq!(infos.len(), 9);
        assert_eq!(infos[0].id, "gpt-6-astra");
        assert_eq!(infos[0].display_name.as_deref(), Some("GPT-6-Astra"));
        let mut seen = std::collections::HashSet::new();
        assert!(infos.iter().all(|m| seen.insert(m.id.clone())));
    }
}
