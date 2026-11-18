//! Selecting a run's model, effort and session mode on an open session.
//!
//! ACP has had two model-selection surfaces. Newer config-option agents use `category: model`
//! in the session's `configOptions`; Grok Build advertises only the first-class `models` state
//! and requires `session/set_model`. An explicit model request is strict on both, for every
//! agent: a model the session does not advertise, or one it refuses to switch to, fails the run
//! before it prompts, because running a different model than the one asked for is worse than
//! the error. Effort is best-effort: a rejected set is logged and the agent's default runs. The
//! session mode is strict the other way: a run that asks must never be left in a mode that
//! approves on its own.

use reno_proto::{AccessMode, Effort, ExecSpec, ModelInfo};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::session::control_request;
use crate::HarnessError;
use crate::agent_id::HarnessId;
use crate::jsonrpc::{Incoming, RpcClient};

/// How a session selects a run's model, effort and session mode.
#[derive(Clone, Copy)]
pub(super) struct ConfigRules {
    pub harness: HarnessId,
    pub effort_in_model_id: bool,
    pub effort_values: fn(Option<Effort>) -> Vec<&'static str>,
    /// Fixed for the run: the permissions the agent asks for follow the session mode chosen
    /// here, and the run's access mode is the one it was started with.
    pub access: AccessMode,
    /// The run asked for full access and `access` is `Supervised` only so that the agent's
    /// requests reach Reno ([`super::routed_access`]). A session that offers no asking mode is
    /// then run as it opens (nothing reaches Reno: the residual gap) instead of refused.
    pub routed_full_access: bool,
}

/// Normalize an option or model id for matching across naming styles (`fastMode` == `fast_mode`
/// == `fast-mode`).
fn norm_id(id: &str) -> String {
    id.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Whether a model id carries the 1M-context hint, in either spelling: the display form
/// `opus[1m]` or the SDK-id form `claude-opus-4-6-1m`.
fn context_hint_1m(id: &str) -> bool {
    id.contains("[1m]") || id.ends_with("-1m")
}

/// The id with a trailing long-context hint removed; `None` when it carries none.
fn strip_context_hint(id: &str) -> Option<&str> {
    id.strip_suffix("[1m]").or_else(|| id.strip_suffix("-1m"))
}

/// A display name with its trailing parenthetical removed ("Opus (1M context)" to "Opus"): a
/// folded base row must not keep the variant tag.
fn strip_trailing_parenthetical(name: &str) -> &str {
    match name.rfind(" (") {
        Some(at) if name.ends_with(')') => name[..at].trim_end(),
        _ => name,
    }
}

/// Pick the advertised model value for a requested model id. Agents differ in what they
/// advertise: full ids (`claude-opus-5`), SDK aliases (`opus`, `sonnet`, `haiku`) and
/// long-context variants in either hint spelling. The chosen context window's variant first
/// (`<model>[1m]`, `<model>-1m`; the default window has none and runs the model itself), then an
/// exact match, then a family-token fallback that prefers a variant matching the window.
fn pick_model_value(requested: &str, available: &[&str], window: Option<&str>) -> Option<String> {
    if let Some(window) = window
        .map(str::to_ascii_lowercase)
        .filter(|w| !w.is_empty())
    {
        for composed in [
            format!("{requested}[{window}]"),
            format!("{requested}-{window}"),
        ] {
            if available.contains(&composed.as_str()) {
                return Some(composed);
            }
        }
    }
    let context_1m = window.is_some_and(|w| w.eq_ignore_ascii_case("1m"));
    if available.contains(&requested) {
        return Some(requested.to_owned());
    }
    // Family fallback: "claude-opus-5" -> "opus" matches "opus[1m]".
    let family = ["fable", "opus", "sonnet", "haiku", "gpt"]
        .into_iter()
        .find(|f| norm_id(requested).contains(f))?;
    let candidates: Vec<&&str> = available
        .iter()
        .filter(|v| norm_id(v).contains(family))
        .collect();
    candidates
        .iter()
        .find(|v| context_hint_1m(v) == context_1m)
        .or_else(|| candidates.first())
        .map(|v| (**v).to_owned())
}

/// Fold an agent's long-context variants into one row each, with a context-window choice. A
/// `<id>[1m]` (or `<id>-1m`) whose bare base is listed too becomes the base's `1m` choice
/// (default: the plain model's window); one with no base listed (the CLI pins the 1M window)
/// is presented AS the base, defaulting to 1M. Sending recomposes the advertised id
/// ([`pick_model_value`]).
pub(super) fn fold_context_variants(models: Vec<ModelInfo>) -> Vec<ModelInfo> {
    let ids: Vec<String> = models.iter().map(|m| m.id.clone()).collect();
    models
        .into_iter()
        .filter_map(|mut model| {
            if let Some(base) = strip_context_hint(&model.id).map(str::to_owned) {
                if ids.contains(&base) {
                    return None;
                }
                let mut window = crate::drivers::claude::catalog::context_window();
                window.default_choice = "1m".into();
                model.options.push(window);
                model.display_name = model
                    .display_name
                    .as_deref()
                    .map(strip_trailing_parenthetical)
                    .filter(|n| !n.is_empty())
                    .map(str::to_owned);
                model.id = base;
                return Some(model);
            }
            if ids
                .iter()
                .any(|id| strip_context_hint(id) == Some(&model.id))
            {
                model
                    .options
                    .push(crate::drivers::claude::catalog::context_window());
            }
            Some(model)
        })
        .collect()
}

fn is_model_select(option: &Value) -> bool {
    option.get("type").and_then(Value::as_str) == Some("select")
        && option.get("category").and_then(Value::as_str) == Some("model")
}

/// Pick a first-class ACP model switch when the session uses the legacy `models` state instead
/// of a category=model config option. Config options remain preferred when present: org adapters
/// can expose a legacy `models` matrix alongside the canonical base-model config option.
fn first_class_model_change(
    session_response: &Value,
    requested: Option<&str>,
) -> Result<Option<String>, HarnessError> {
    let Some(requested) = requested else {
        return Ok(None);
    };
    let has_model_config = session_response
        .get("configOptions")
        .and_then(Value::as_array)
        .is_some_and(|options| options.iter().any(is_model_select));
    if has_model_config {
        return Ok(None);
    }
    let Some(models) = session_response.get("models") else {
        return Ok(None);
    };
    let available: Vec<&str> = models
        .get("availableModels")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|model| model.get("modelId").and_then(Value::as_str))
        .collect();
    if available.is_empty() {
        return Ok(None);
    }
    if !available.contains(&requested) {
        return Err(HarnessError::Protocol(format!(
            "agent does not advertise requested model {requested}; available models: {}",
            available.join(", ")
        )));
    }
    if models.get("currentModelId").and_then(Value::as_str) == Some(requested) {
        return Ok(None);
    }
    Ok(Some(requested.to_owned()))
}

/// Fail when the session's model config option does not offer `requested`.
fn validate_config_model_selection(
    session_response: &Value,
    requested: Option<&str>,
) -> Result<(), HarnessError> {
    let Some(requested) = requested else {
        return Ok(());
    };
    let Some(option) = session_response
        .get("configOptions")
        .and_then(Value::as_array)
        .and_then(|options| options.iter().find(|option| is_model_select(option)))
    else {
        return Ok(());
    };
    let available: Vec<&str> = option
        .get("options")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|choice| choice.get("value").and_then(Value::as_str))
        .collect();
    if pick_model_value(requested, &available, None).is_some() {
        return Ok(());
    }
    Err(HarnessError::Protocol(format!(
        "agent does not advertise requested model {requested}; available models: {}",
        available.join(", ")
    )))
}

fn config_option_category<'a>(session_response: &'a Value, config_id: &str) -> Option<&'a str> {
    session_response
        .get("configOptions")
        .and_then(Value::as_array)?
        .iter()
        .find(|option| option.get("id").and_then(Value::as_str) == Some(config_id))?
        .get("category")
        .and_then(Value::as_str)
}

fn is_model_config_option(session_response: &Value, config_id: &str) -> bool {
    config_option_category(session_response, config_id) == Some("model")
}

/// Session mode values that approve everything themselves, the agent's own full access.
const NO_PROMPT_MODES: [&str; 7] = [
    "bypassPermissions",
    "bypass_permissions",
    "bypass",
    "yolo",
    "agent-full-access",
    "danger-full-access",
    "full-access",
];

/// Session mode values that ask before acting, for a run that is not in full access:
/// Antigravity's `default` (its others are `auto_edit` and `yolo`).
const ASKING_MODES: [&str; 1] = ["default"];

/// The mode the session is in, when it is one that approves everything itself: the `mode`
/// config option's current value, or the older `modes` state's.
fn current_no_prompt_mode(session_response: &Value) -> Option<&str> {
    let option = session_response
        .get("configOptions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|option| option.get("category").and_then(Value::as_str) == Some("mode"))
        .and_then(|option| option.get("currentValue").and_then(Value::as_str));
    let legacy = session_response
        .get("modes")
        .and_then(|modes| modes.get("currentModeId"))
        .and_then(Value::as_str);
    [option, legacy]
        .into_iter()
        .flatten()
        .find(|mode| NO_PROMPT_MODES.contains(mode))
}

/// The `session/set_config_option` calls a session response's `configOptions` warrant for this
/// run:
/// - the requested model (category `model`),
/// - the effort (category `thought_level`, first advertised value from the spec's preference
///   list),
/// - the session mode (category `mode`) for `access`: the no-prompts mode in full access, an
///   asking mode otherwise.
///
/// Matched against advertised values and skipped when already current. Pure so it's testable;
/// the returned value is the request's flattened `value` payload (`{"value": id}`).
#[cfg(test)]
fn config_option_sets(
    session_response: &Value,
    model: Option<&str>,
    efforts: &[&'static str],
    access: AccessMode,
) -> Vec<(String, Value)> {
    config_option_sets_with_window(session_response, model, None, efforts, access)
}

/// [`config_option_sets`] for a run that chose a context window (`window`, a choice id).
fn config_option_sets_with_window(
    session_response: &Value,
    model: Option<&str>,
    window: Option<&str>,
    efforts: &[&'static str],
    access: AccessMode,
) -> Vec<(String, Value)> {
    let Some(options) = session_response
        .get("configOptions")
        .and_then(Value::as_array)
    else {
        return Vec::new();
    };
    let mut sets = Vec::new();
    for option in options {
        let Some(config_id) = option.get("id").and_then(Value::as_str) else {
            continue;
        };
        if option.get("type").and_then(Value::as_str) != Some("select") {
            continue;
        }
        let category = option.get("category").and_then(Value::as_str);
        let current = option.get("currentValue");
        let available: Vec<&str> = option
            .get("options")
            .and_then(Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|o| o.get("value").and_then(Value::as_str))
            .collect();

        let wanted: Option<&str> = match category {
            Some("model") => model
                .and_then(|m| pick_model_value(m, &available, window))
                .and_then(|picked| available.iter().copied().find(|a| *a == picked)),
            // Full access: the no-prompts mode when the agent offers one. claude-agent-acp
            // calls it `bypassPermissions`, codex-acp `agent-full-access`, Devin `bypass`.
            Some("mode") if access == AccessMode::FullAccess => {
                NO_PROMPT_MODES.into_iter().find(|v| available.contains(v))
            }
            // Any other mode keeps the agent asking: its permissions are decided per tool call.
            Some("mode") => ASKING_MODES.into_iter().find(|v| available.contains(v)),
            Some("thought_level") => efforts.iter().copied().find(|c| available.contains(c)),
            _ => None,
        };
        if let Some(value) = wanted
            && current.and_then(Value::as_str) != Some(value)
        {
            sets.push((config_id.to_owned(), json!({ "value": value })));
        }
    }
    sets
}

const EFFORT_SUFFIXES: [(&str, &str, Effort); 3] = [
    ("-low", " (Low)", Effort::Low),
    ("-medium", " (Medium)", Effort::Medium),
    ("-high", " (High)", Effort::High),
];

fn effort_rank(effort: Effort) -> u8 {
    match effort {
        Effort::Minimal => 0,
        Effort::Low => 1,
        Effort::Medium => 2,
        Effort::High => 3,
        Effort::XHigh => 4,
        Effort::Max => 5,
    }
}

/// The effort variants of one advertised model, with the id the agent advertises for each level.
struct EffortGroup<'a> {
    id: String,
    label: &'a str,
    variants: Vec<(Effort, &'a str)>,
}

/// Group advertised `(id, name)` choices by their display name. Antigravity names every effort
/// variant "<model> (Low|Medium|High)", but its ids don't always follow (a signed-in account
/// pairs `gemini-3.1-pro-low` with `gemini-pro-agent` for High), so the name is the only
/// reliable key. A group's id is a variant id minus its level suffix, falling back to the first
/// variant's id.
fn effort_groups<'a>(
    choices: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Vec<EffortGroup<'a>> {
    let mut groups: Vec<EffortGroup<'a>> = Vec::new();
    for (id, name) in choices {
        let variant = EFFORT_SUFFIXES.iter().find_map(|(_, label_suffix, level)| {
            name.strip_suffix(label_suffix).map(|base| (base, *level))
        });
        let Some((base, level)) = variant else {
            groups.push(EffortGroup {
                id: id.to_owned(),
                label: name,
                variants: Vec::new(),
            });
            continue;
        };
        match groups
            .iter_mut()
            .find(|group| !group.variants.is_empty() && group.label == base)
        {
            Some(group) => group.variants.push((level, id)),
            None => groups.push(EffortGroup {
                id: String::new(),
                label: base,
                variants: vec![(level, id)],
            }),
        }
    }
    for group in groups.iter_mut().filter(|group| !group.variants.is_empty()) {
        group.id = group
            .variants
            .iter()
            .find_map(|(_, id)| {
                EFFORT_SUFFIXES
                    .iter()
                    .find_map(|(id_suffix, _, _)| id.strip_suffix(id_suffix))
            })
            .unwrap_or(group.variants[0].1)
            .to_owned();
        group.variants.sort_by_key(|(level, _)| effort_rank(*level));
    }
    groups
}

/// The advertised variant id for a grouped model: the requested level when the model offers it,
/// else its strongest level. Ids the agent already advertises (a full variant id) pass through
/// untouched.
fn effort_variant_id(session_response: &Value, model: &str, effort: Option<Effort>) -> String {
    let choices: Vec<(&str, &str)> = session_response
        .get("configOptions")
        .and_then(Value::as_array)
        .and_then(|options| {
            options.iter().find(|o| {
                o.get("category").and_then(Value::as_str) == Some("model")
                    && o.get("id").and_then(Value::as_str) != Some("provider")
            })
        })
        .and_then(|o| o.get("options").and_then(Value::as_array))
        .map(|choices| {
            choices
                .iter()
                .filter_map(|c| {
                    let id = c.get("value").and_then(Value::as_str)?;
                    Some((id, c.get("name").and_then(Value::as_str).unwrap_or(id)))
                })
                .collect()
        })
        .unwrap_or_default();
    if choices.iter().any(|(id, _)| *id == model) {
        return model.to_owned();
    }
    let groups = effort_groups(choices);
    let Some(group) = groups.iter().find(|group| group.id == model) else {
        return model.to_owned();
    };
    let offered = |level: Effort| {
        group
            .variants
            .iter()
            .find(|(variant_level, _)| *variant_level == level)
            .map(|(_, id)| (*id).to_owned())
    };
    effort
        .and_then(offered)
        .or_else(|| {
            [Effort::High, Effort::Medium, Effort::Low]
                .into_iter()
                .find_map(offered)
        })
        .unwrap_or_else(|| model.to_owned())
}

/// Select `spec`'s model and effort, and the session mode for the run's access, on the open
/// session. Returns the session's options as they stand afterwards.
pub(super) async fn select_session_config(
    client: &RpcClient,
    incoming: &mut mpsc::Receiver<Incoming>,
    rules: ConfigRules,
    session_id: &str,
    mut session_response: Value,
    spec: &ExecSpec,
) -> Result<Value, HarnessError> {
    if rules.harness == HarnessId::Fx {
        // fx can report ask while retaining a saved automatic policy. Always write it before
        // prompting, including after session/load: Reno decides, whatever the run's access.
        control_request(
            client,
            incoming,
            rules.access,
            "session/set_config_option",
            json!({"sessionId": session_id, "configId": "mode", "value": "ask"}),
        )
        .await?;
        if let Some(model) = spec.model.as_deref() {
            control_request(
                client,
                incoming,
                rules.access,
                "session/set_config_option",
                json!({"sessionId": session_id, "configId": "model", "value": model}),
            )
            .await?;
        }
        // Both provider and model carry category=model. Only the model selector takes part in
        // model selection.
        if let Some(options) = session_response
            .get_mut("configOptions")
            .and_then(Value::as_array_mut)
        {
            options.retain(|option| option.get("id").and_then(Value::as_str) != Some("mode"));
            for option in options {
                if option.get("category").and_then(Value::as_str) == Some("model")
                    && option.get("id").and_then(Value::as_str) != Some("model")
                {
                    option["category"] = json!("other");
                }
            }
        }
    }
    let requested_model: Option<String> = match spec.model.as_deref() {
        Some(model) if rules.effort_in_model_id => {
            Some(effort_variant_id(&session_response, model, spec.effort))
        }
        model => model.map(str::to_owned),
    };
    validate_config_model_selection(&session_response, requested_model.as_deref())?;
    if let Some(model) = first_class_model_change(&session_response, requested_model.as_deref())? {
        control_request(
            client,
            incoming,
            rules.access,
            "session/set_model",
            json!({ "sessionId": session_id, "modelId": model }),
        )
        .await
        .map_err(|error| {
            HarnessError::Protocol(format!("agent rejected model switch to {model}: {error}"))
        })?;
        // The next comparison is against the model now current.
        if let Some(models) = session_response.get_mut("models") {
            models["currentModelId"] = Value::String(model);
        }
    }
    let efforts = (rules.effort_values)(spec.effort);
    let mut options_snapshot = session_response;
    let window = spec
        .model_options
        .get(reno_proto::MODEL_OPTION_CONTEXT_WINDOW)
        .map(String::as_str);
    let mut sets = config_option_sets_with_window(
        &options_snapshot,
        requested_model.as_deref(),
        window,
        &efforts,
        rules.access,
    );
    // Devin's effort choices depend on the selected model: switch the model first, then choose
    // from what it offers.
    let deferred = rules.harness == HarnessId::Devin
        && sets
            .iter()
            .any(|(id, _)| is_model_config_option(&options_snapshot, id));
    if deferred {
        sets.retain(|(id, _)| is_model_config_option(&options_snapshot, id));
    }
    let mut pass = 0;
    loop {
        for (config_id, payload) in std::mem::take(&mut sets) {
            let mut params = serde_json::Map::new();
            params.insert("sessionId".into(), session_id.into());
            params.insert("configId".into(), config_id.clone().into());
            if let Some(payload) = payload.as_object() {
                for (k, v) in payload {
                    params.insert(k.clone(), v.clone());
                }
            }
            match control_request(
                client,
                incoming,
                rules.access,
                "session/set_config_option",
                Value::Object(params),
            )
            .await
            {
                Ok(response) => {
                    if let Some(options) = response.get("configOptions") {
                        options_snapshot["configOptions"] = options.clone();
                    } else if let Some(value) = payload.get("value")
                        && let Some(option) = options_snapshot["configOptions"]
                            .as_array_mut()
                            .and_then(|options| {
                                options.iter_mut().find(|o| o["id"] == config_id.as_str())
                            })
                    {
                        // The next comparison is against the value now set.
                        option["currentValue"] = value.clone();
                    }
                }
                Err(e) => {
                    if requested_model.is_some()
                        && is_model_config_option(&options_snapshot, &config_id)
                    {
                        return Err(HarnessError::Protocol(format!(
                            "agent rejected requested model {}: {e}",
                            requested_model.as_deref().unwrap_or_default()
                        )));
                    }
                    // Left in a mode that approves on its own, a run that asks would never see
                    // a permission: it must not start.
                    if rules.access != AccessMode::FullAccess
                        && config_option_category(&options_snapshot, &config_id) == Some("mode")
                    {
                        return Err(HarnessError::Protocol(format!(
                            "agent rejected session mode {} for {:?} access: {e}",
                            payload["value"], rules.access
                        )));
                    }
                    tracing::debug!(
                        target: "reno_harness::acp",
                        "session/set_config_option {config_id}={payload} rejected (agent default runs): {e}"
                    );
                }
            }
        }
        pass += 1;
        if !deferred || pass > 1 {
            break;
        }
        sets = config_option_sets_with_window(
            &options_snapshot,
            requested_model.as_deref(),
            window,
            &efforts,
            rules.access,
        );
        sets.retain(|(id, _)| !is_model_config_option(&options_snapshot, id));
    }
    // Left in a mode that approves on its own (one the agent offers no asking mode to leave), a
    // run that asks would never see a permission: it must not start.
    if rules.access != AccessMode::FullAccess
        && !rules.routed_full_access
        && let Some(mode) = current_no_prompt_mode(&options_snapshot)
    {
        return Err(HarnessError::Protocol(format!(
            "the agent's session is in its {mode} mode, which approves on its own, and offers no \
             mode that asks: a run in {:?} access cannot start",
            rules.access
        )));
    }
    Ok(options_snapshot)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn session_with(options: Value) -> Value {
        json!({ "sessionId": "s-1", "configOptions": options })
    }

    #[test]
    fn config_option_sets_map_model_effort_and_mode() {
        let response = session_with(json!([
            {
                "id": "model", "name": "Model", "category": "model", "type": "select",
                "currentValue": "claude-sonnet-5",
                "options": [
                    { "value": "claude-sonnet-5", "name": "Sonnet 5" },
                    { "value": "claude-opus-5", "name": "Opus 5" },
                    { "value": "claude-opus-5[1m]", "name": "Opus 5 (1M)" },
                ],
            },
            {
                "id": "effort", "name": "Reasoning effort", "category": "thought_level",
                "type": "select", "currentValue": "high",
                "options": [
                    { "value": "low" }, { "value": "medium" }, { "value": "high" },
                    { "value": "max" },
                ],
            },
            {
                "id": "fast_mode", "category": "model_config", "type": "boolean",
                "currentValue": false,
            },
        ]));
        let full = AccessMode::FullAccess;
        // Model switch + effort preference list; an option no one asked about is untouched.
        assert_eq!(
            config_option_sets(&response, Some("claude-opus-5"), &["medium"], full),
            vec![
                ("model".to_owned(), json!({ "value": "claude-opus-5" })),
                ("effort".to_owned(), json!({ "value": "medium" })),
            ]
        );
        // Effort preference order: the first ADVERTISED candidate wins.
        assert_eq!(
            config_option_sets(&response, None, &["xhigh", "max"], full),
            vec![("effort".to_owned(), json!({ "value": "max" }))]
        );
        // Already-current values, unadvertised models and a session with no options set nothing.
        assert!(config_option_sets(&response, Some("claude-sonnet-5"), &["high"], full).is_empty());
        assert!(config_option_sets(&response, Some("gpt-5.6-sol"), &[], full).is_empty());
        assert!(
            config_option_sets(&json!({ "sessionId": "s" }), Some("x"), &["high"], full).is_empty()
        );
    }

    #[test]
    fn a_family_name_picks_the_advertised_variant_without_the_long_context_hint() {
        let available = ["opus[1m]", "opus", "sonnet"];
        assert_eq!(
            pick_model_value("claude-opus-5", &available, None),
            Some("opus".into())
        );
        assert_eq!(
            pick_model_value("claude-opus-5", &["opus[1m]"], None),
            Some("opus[1m]".into())
        );
        assert_eq!(
            pick_model_value("opus", &available, None),
            Some("opus".into())
        );
        assert_eq!(pick_model_value("grok-9", &available, None), None);
    }

    #[test]
    fn the_chosen_window_picks_the_advertised_variant_and_the_default_the_plain_model() {
        let available = ["claude-sonnet-5", "claude-sonnet-5[1m]", "opus[1m]", "opus"];
        assert_eq!(
            pick_model_value("claude-sonnet-5", &available, Some("1m")),
            Some("claude-sonnet-5[1m]".into())
        );
        assert_eq!(
            pick_model_value("claude-sonnet-5", &available, Some("200k")),
            Some("claude-sonnet-5".into())
        );
        // The family fallback follows the window too.
        assert_eq!(
            pick_model_value("claude-opus-5", &available, Some("1m")),
            Some("opus[1m]".into())
        );
        assert_eq!(
            pick_model_value("claude-opus-5", &available, None),
            Some("opus".into())
        );
    }

    fn row(id: &str, name: &str) -> ModelInfo {
        ModelInfo {
            id: id.into(),
            display_name: Some(name.into()),
            ..plain_model()
        }
    }

    fn plain_model() -> ModelInfo {
        ModelInfo {
            id: String::new(),
            display_name: None,
            description: None,
            efforts: vec![],
            default_effort: None,
            options: vec![],
        }
    }

    #[test]
    fn model_1m_variants_collapse_into_a_context_window_choice() {
        let models = fold_context_variants(vec![
            row("claude-sonnet-5", "Sonnet 5"),
            row("claude-sonnet-5[1m]", "Sonnet 5 (1M)"),
            // The SDK-id hint spelling collapses too.
            row("claude-opus-4-6", "Opus 4.6"),
            row("claude-opus-4-6-1m", "Opus 4.6 (1M)"),
            row("claude-haiku-4-5", "Haiku 4.5"),
        ]);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            ["claude-sonnet-5", "claude-opus-4-6", "claude-haiku-4-5"]
        );
        for folded in &models[..2] {
            let window = &folded.options[0];
            assert_eq!(window.id, "contextWindow");
            assert_eq!(window.default_choice, "200k");
            assert!(window.settable);
        }
        assert!(models[2].options.is_empty());
    }

    #[test]
    fn an_orphan_1m_variant_is_its_base_with_the_window_defaulting_to_1m() {
        let models = fold_context_variants(vec![
            row("opus[1m]", "Opus (1M context)"),
            row("claude-fable-5[1m]", "Fable 5"),
            row("sonnet", "Sonnet"),
        ]);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["opus", "claude-fable-5", "sonnet"]);
        // The variant tag leaves the name.
        assert_eq!(models[0].display_name.as_deref(), Some("Opus"));
        for pinned in &models[..2] {
            assert_eq!(pinned.options[0].default_choice, "1m");
        }
        assert!(models[2].options.is_empty());
    }

    #[test]
    fn grok_shaped_config_options_apply_the_chosen_effort_through_thought_level() {
        // `session/new` of grok 1.0.88: a `reasoning_effort` select of category `thought_level`.
        let response = session_with(json!([
            {
                "id": "model", "category": "model", "type": "select", "currentValue": "grok-4.7",
                "options": [{ "value": "grok-4.7" }, { "value": "grok-4.6" }],
            },
            {
                "id": "reasoning_effort", "category": "thought_level", "type": "select",
                "currentValue": "high",
                "options": [
                    { "value": "xhigh" }, { "value": "high" }, { "value": "medium" },
                    { "value": "low" },
                ],
            },
        ]));
        let full = AccessMode::FullAccess;
        assert_eq!(
            config_option_sets(&response, Some("grok-4.7"), &["low"], full),
            vec![("reasoning_effort".to_owned(), json!({ "value": "low" }))]
        );
        assert_eq!(
            config_option_sets(&response, None, &["xhigh", "high"], full),
            vec![("reasoning_effort".to_owned(), json!({ "value": "xhigh" }))]
        );
    }

    #[test]
    fn first_class_models_use_session_set_model_without_config_option() {
        let response = json!({
            "models": {
                "currentModelId": "grok-4.6",
                "availableModels": [
                    { "modelId": "grok-4.6", "name": "Grok 4.6" },
                    { "modelId": "grok-4.5", "name": "Grok 4.5" },
                ],
            },
        });
        assert_eq!(
            first_class_model_change(&response, Some("grok-4.5")).unwrap(),
            Some("grok-4.5".into())
        );
        assert_eq!(
            first_class_model_change(&response, Some("grok-4.6")).unwrap(),
            None
        );
        assert_eq!(first_class_model_change(&response, None).unwrap(), None);
        assert!(first_class_model_change(&response, Some("unknown")).is_err());
    }

    #[test]
    fn the_model_config_option_takes_precedence_over_legacy_models_state() {
        let response = json!({
            "models": {
                "currentModelId": "gpt-5.6-sol low",
                "availableModels": [{ "modelId": "gpt-5.6-sol low", "name": "low" }],
            },
            "configOptions": [{
                "id": "model", "category": "model", "type": "select",
                "currentValue": "gpt-5.6-sol",
                "options": [{ "value": "gpt-5.6-sol" }, { "value": "gpt-5.6-terra" }],
            }],
        });
        assert_eq!(
            first_class_model_change(&response, Some("gpt-5.6-terra")).unwrap(),
            None
        );
    }

    fn mode_option(current: &str, values: &[&str]) -> Value {
        session_with(json!([{
            "id": "mode", "category": "mode", "type": "select", "currentValue": current,
            "options": values.iter().map(|v| json!({ "value": v })).collect::<Vec<_>>(),
        }]))
    }

    #[test]
    fn full_access_picks_the_agents_no_prompt_mode_by_its_own_naming() {
        let full = AccessMode::FullAccess;
        for (values, chosen) in [
            (
                &["read-only", "agent", "agent-full-access"][..],
                "agent-full-access",
            ),
            (
                &["default", "plan", "bypassPermissions"][..],
                "bypassPermissions",
            ),
            (&["default", "auto_edit", "yolo"][..], "yolo"),
            (&["normal", "bypass"][..], "bypass"),
        ] {
            assert_eq!(
                config_option_sets(&mode_option("agent", values), None, &[], full),
                vec![("mode".to_owned(), json!({ "value": chosen }))],
                "{values:?}"
            );
        }
    }

    #[test]
    fn a_run_that_asks_is_kept_in_the_asking_mode() {
        // agy_acp_server's mode select.
        let antigravity = |current| mode_option(current, &["default", "auto_edit", "yolo"]);
        let mode = |value: &str| vec![("mode".to_owned(), json!({ "value": value }))];
        for access in [AccessMode::Supervised, AccessMode::AutoAcceptEdits] {
            assert!(
                config_option_sets(&antigravity("default"), None, &[], access).is_empty(),
                "{access:?}"
            );
            assert_eq!(
                config_option_sets(&antigravity("yolo"), None, &[], access),
                mode("default"),
                "{access:?}"
            );
            assert_eq!(
                config_option_sets(&antigravity("auto_edit"), None, &[], access),
                mode("default"),
                "{access:?}"
            );
        }
        // No asking mode on offer: nothing is set.
        assert!(
            config_option_sets(
                &mode_option("agent", &["agent", "agent-full-access"]),
                None,
                &[],
                AccessMode::Supervised
            )
            .is_empty()
        );
    }

    #[test]
    fn a_session_left_in_a_no_prompt_mode_is_seen_whichever_way_it_says_so() {
        let option = mode_option("yolo", &["auto_edit", "yolo"]);
        assert_eq!(current_no_prompt_mode(&option), Some("yolo"));
        let legacy = json!({ "modes": { "currentModeId": "bypassPermissions" } });
        assert_eq!(current_no_prompt_mode(&legacy), Some("bypassPermissions"));
        assert_eq!(
            current_no_prompt_mode(&mode_option("default", &["default", "yolo"])),
            None
        );
        assert_eq!(
            current_no_prompt_mode(&json!({ "modes": { "currentModeId": "plan" } })),
            None
        );
        assert_eq!(current_no_prompt_mode(&json!({})), None);
    }

    fn antigravity_signed_in_catalog() -> Value {
        session_with(json!([{
            "id": "model", "category": "model", "type": "select",
            "options": [
                {"value": "gemini-3.7-flash-high", "name": "Gemini 3.7 Flash (High)"},
                {"value": "gemini-3.7-flash-medium", "name": "Gemini 3.7 Flash (Medium)"},
                {"value": "gemini-3.7-flash-low", "name": "Gemini 3.7 Flash (Low)"},
                {"value": "gemini-pro-agent", "name": "Gemini 3.1 Pro (High)"},
                {"value": "gemini-3.1-pro-low", "name": "Gemini 3.1 Pro (Low)"},
                {"value": "gemini-legacy", "name": "Gemini Legacy"},
            ],
        }]))
    }

    #[test]
    fn effort_variants_group_by_name_even_when_ids_disagree() {
        let catalog = antigravity_signed_in_catalog();
        let options = catalog["configOptions"][0]["options"].as_array().unwrap();
        let groups = effort_groups(
            options
                .iter()
                .map(|o| (o["value"].as_str().unwrap(), o["name"].as_str().unwrap())),
        );
        let rows: Vec<(String, &str, Vec<Effort>)> = groups
            .iter()
            .map(|g| {
                (
                    g.id.clone(),
                    g.label,
                    g.variants.iter().map(|(level, _)| *level).collect(),
                )
            })
            .collect();
        assert_eq!(
            rows,
            vec![
                (
                    "gemini-3.7-flash".into(),
                    "Gemini 3.7 Flash",
                    vec![Effort::Low, Effort::Medium, Effort::High]
                ),
                (
                    "gemini-3.1-pro".into(),
                    "Gemini 3.1 Pro",
                    vec![Effort::Low, Effort::High]
                ),
                ("gemini-legacy".into(), "Gemini Legacy", vec![]),
            ]
        );
    }

    #[test]
    fn effort_variant_id_resolves_the_advertised_id_for_each_level() {
        let catalog = antigravity_signed_in_catalog();
        let id = |model, effort| effort_variant_id(&catalog, model, effort);
        assert_eq!(id("gemini-3.1-pro", Some(Effort::High)), "gemini-pro-agent");
        assert_eq!(
            id("gemini-3.1-pro", Some(Effort::Low)),
            "gemini-3.1-pro-low"
        );
        // A level the model does not offer clamps to its strongest.
        assert_eq!(
            id("gemini-3.1-pro", Some(Effort::Medium)),
            "gemini-pro-agent"
        );
        assert_eq!(id("gemini-3.1-pro", None), "gemini-pro-agent");
        assert_eq!(
            id("gemini-3.7-flash", Some(Effort::Medium)),
            "gemini-3.7-flash-medium"
        );
        // An id the agent already advertises passes through, and so does one it does not know.
        assert_eq!(
            id("gemini-pro-agent", Some(Effort::Low)),
            "gemini-pro-agent"
        );
        assert_eq!(id("gemini-legacy", Some(Effort::High)), "gemini-legacy");
        assert_eq!(id("unknown-model", None), "unknown-model");
    }

    #[test]
    fn an_unadvertised_model_is_refused_where_the_agent_is_strict() {
        let catalog = antigravity_signed_in_catalog();
        assert!(validate_config_model_selection(&catalog, Some("gemini-legacy")).is_ok());
        assert!(validate_config_model_selection(&catalog, None).is_ok());
        let error = validate_config_model_selection(&catalog, Some("nope")).unwrap_err();
        assert!(error.to_string().contains("available models"), "{error}");
        // No model option at all: nothing to check against.
        assert!(validate_config_model_selection(&json!({}), Some("nope")).is_ok());
    }
}
