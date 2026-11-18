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

/// The model of `candidate` that `model` names: a listed id (a `[1m]` suffix aside) or an alias
/// (the first listed model whose id contains it). `None` when it names none.
pub(crate) fn listed_model<'a>(candidate: &'a Candidate, model: &str) -> Option<&'a ModelInfo> {
    let caps = &candidate.descriptor.caps;
    let base = caps.base_model(model);
    let models = listed(candidate);
    models
        .iter()
        .find(|m| m.id == model || m.id == base)
        .or_else(|| {
            caps.is_model_alias(model)
                .then(|| models.iter().find(|m| m.id.contains(base)))
                .flatten()
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
    let base = caps.base_model(model);
    let named = models.iter().find(|m| m.id == model || m.id == base);
    let resolved = named.or_else(|| {
        caps.is_model_alias(model)
            .then(|| models.iter().find(|m| m.id.contains(base)))
            .flatten()
    });
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
    let base = caps.base_model(model);
    let named = models.iter().find(|m| m.id == model || m.id == base);
    let resolved = named.or_else(|| {
        caps.is_model_alias(model)
            .then(|| models.iter().find(|m| m.id.contains(base)))
            .flatten()
    });
    match resolved {
        Some(info) => refusal(info).map_or(Ok(()), Err),
        None if caps.is_model_alias(model) => some_model(),
        None => Err(format!(
            "model {model:?} is not in {executor}'s model list, so its options are unknown"
        )),
    }
}
