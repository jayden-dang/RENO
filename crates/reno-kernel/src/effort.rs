//! Whether an executor's model takes the effort a run asks for.
//!
//! The levels come from the models the executor lists: the live list its last probe found, else
//! the adapter's curated one (which names no levels, so nothing is accepted until a probe has
//! run). A model that lists no levels takes none.

use reno_proto::{Effort, ExecutorId, ModelInfo};

use crate::router::Candidate;

fn listed(candidate: &Candidate) -> &[ModelInfo] {
    candidate
        .health
        .as_ref()
        .map(|health| health.models.models.as_slice())
        .filter(|models| !models.is_empty())
        .unwrap_or(candidate.descriptor.caps.models.as_slice())
}

/// A model name as the app folds it for matching: lower case, `.`, `_` and spaces as `-`, no
/// `claude-` prefix (`Opus 5.5` and `claude-opus-5-5` are one name).
fn folded(name: &str) -> String {
    let name: String = name
        .trim()
        .to_lowercase()
        .chars()
        .map(|c| if matches!(c, '.' | '_' | ' ') { '-' } else { c })
        .collect();
    name.strip_prefix("claude-").unwrap_or(&name).to_owned()
}

/// The model of `candidate` that `model` names: a listed id; the name without its context hint
/// (`[1m]`, `-1m`); an alias of the executor (the first listed model whose id contains it); or a
/// model whose folded id or display name is the folded name (`opus-5.5` is `claude-opus-5-5`).
/// `None` when it names none.
pub(crate) fn listed_model<'a>(candidate: &'a Candidate, model: &str) -> Option<&'a ModelInfo> {
    let caps = &candidate.descriptor.caps;
    let models = listed(candidate);
    let hintless = model
        .strip_suffix("[1m]")
        .or_else(|| model.strip_suffix("-1m"));
    let base = caps.base_model(model);
    for name in std::iter::once(model).chain(hintless) {
        if let Some(found) = models.iter().find(|m| m.id == name) {
            return Some(found);
        }
    }
    if let Some(found) = models.iter().find(|m| m.id == base) {
        return Some(found);
    }
    if caps.is_model_alias(model) {
        return models.iter().find(|m| m.id.contains(base));
    }
    std::iter::once(model).chain(hintless).find_map(|name| {
        let name = folded(name);
        models.iter().find(|m| {
            folded(&m.id) == name || m.display_name.as_deref().map(folded) == Some(name.clone())
        })
    })
}

fn levels(efforts: &[Effort]) -> String {
    if efforts.is_empty() {
        "no effort levels".to_owned()
    } else {
        let words: Vec<&str> = efforts.iter().map(|e| e.as_str()).collect();
        format!("levels {}", words.join(", "))
    }
}

/// `Ok` when the model `model` (a listed id, an alias, or none: the executor's default) of
/// `candidate` takes `effort`; otherwise what to tell the person.
///
/// An alias means the first listed model whose id contains it (`opus` is the first `opus` row of
/// the CLI's list, which is how the CLI orders them). With no model named the executor's default
/// is not known, so the effort is accepted when some listed model takes it. A named model that
/// is not listed has unknown levels and is refused.
pub(crate) fn check(
    candidate: &Candidate,
    model: Option<&str>,
    effort: Effort,
) -> Result<(), String> {
    let executor: &ExecutorId = &candidate.descriptor.id;
    let models = listed(candidate);
    let caps = &candidate.descriptor.caps;
    let some_model = || {
        if models.iter().any(|m| m.efforts.contains(&effort)) {
            return Ok(());
        }
        let each: Vec<String> = models
            .iter()
            .map(|m| format!("{}: {}", m.id, levels(&m.efforts)))
            .collect();
        Err(format!(
            "no model of {executor} takes effort {} ({})",
            effort.as_str(),
            each.join("; ")
        ))
    };
    let Some(model) = model else {
        return some_model();
    };
    let resolved = listed_model(candidate, model);
    match resolved {
        Some(info) if info.efforts.contains(&effort) => Ok(()),
        Some(info) => Err(format!(
            "model {} of {executor} does not take effort {}; it lists {}",
            info.id,
            effort.as_str(),
            levels(&info.efforts)
        )),
        None if caps.is_model_alias(model) => some_model(),
        None => Err(format!(
            "model {model:?} is not in {executor}'s model list, so its effort levels are \
             unknown and effort {} cannot be asked of it",
            effort.as_str()
        )),
    }
}

/// `Ok` when the model `model` of `candidate` lists every option in `options` with the choice
/// asked for; otherwise what to tell the person. The model is resolved as [`check`] does; with
/// none named, the options are accepted when some listed model lists all of them.
pub(crate) fn check_options(
    candidate: &Candidate,
    model: Option<&str>,
    options: &std::collections::BTreeMap<String, String>,
) -> Result<(), String> {
    let executor: &ExecutorId = &candidate.descriptor.id;
    let models = listed(candidate);
    let caps = &candidate.descriptor.caps;
    let refusal = |info: &ModelInfo| -> Option<String> {
        for (option, choice) in options {
            let Some(listed) = info.options.iter().find(|o| &o.id == option) else {
                let have: Vec<&str> = info.options.iter().map(|o| o.id.as_str()).collect();
                return Some(format!(
                    "model {} of {executor} has no option {option:?}; it lists {}",
                    info.id,
                    if have.is_empty() {
                        "no options".to_owned()
                    } else {
                        have.join(", ")
                    }
                ));
            };
            if !listed.settable {
                return Some(format!(
                    "option {option:?} of model {} on {executor} is read-only: the agent \
                     reports it and has no way to set it",
                    info.id
                ));
            }
            if !listed.choices.iter().any(|c| &c.id == choice) {
                let have: Vec<&str> = listed.choices.iter().map(|c| c.id.as_str()).collect();
                return Some(format!(
                    "model {} of {executor} takes no {choice:?} for option {option:?}; it lists {}",
                    info.id,
                    have.join(", ")
                ));
            }
        }
        None
    };
    let some_model = || {
        let mut why = None;
        for info in models {
            match refusal(info) {
                None => return Ok(()),
                Some(w) => why = why.or(Some(w)),
            }
        }
        Err(why.unwrap_or_else(|| format!("{executor} lists no models, so no option can be set")))
    };
    let Some(model) = model else {
        return some_model();
    };
    let resolved = listed_model(candidate, model);
    match resolved {
        Some(info) => refusal(info).map_or(Ok(()), Err),
        None if caps.is_model_alias(model) => some_model(),
        None => Err(format!(
            "model {model:?} is not in {executor}'s model list, so its options are unknown"
        )),
    }
}
