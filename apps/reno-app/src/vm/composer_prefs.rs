//! What the composer's picker remembers between launches: starred models, the effort last used
//! with each model, and the model options (context window) last chosen for each.
//!
//! A subset of zeron's `ComposerDefaults` (`crates/ui/src/settings/composer.rs`): `favorites`,
//! `reasoning_by_model` and `model_options_by_model`, keyed by harness id and model id. The
//! harness and model last picked, the label cache, device, project and access mode are not
//! carried over: Reno's Home picks its route itself, and its labels come from the CLI.
//!
//! The file is `app-composer.json` beside the daemon socket (`local::composer_path`).

use std::collections::BTreeMap;

use reno_proto::{Effort, ExecutorId};
use serde::{Deserialize, Serialize};

/// One starred model.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Star {
    pub(crate) harness: String,
    pub(crate) model: String,
}

/// A model's chosen options: option id to choice id (`contextWindow` to `1m`).
pub(crate) type Options = BTreeMap<String, String>;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(crate) struct ComposerPrefs {
    /// Starred models, in starring order.
    pub(crate) favourites: Vec<Star>,
    /// The effort last chosen, by harness then model id. A model left at its default has none.
    pub(crate) effort: BTreeMap<String, BTreeMap<String, Effort>>,
    /// The options last chosen, by harness then model id.
    pub(crate) model_options: BTreeMap<String, BTreeMap<String, Options>>,
}

impl ComposerPrefs {
    pub(crate) fn is_favourite(&self, harness: &ExecutorId, model: &str) -> bool {
        self.favourites
            .iter()
            .any(|s| s.harness == harness.as_str() && s.model == model)
    }

    /// Stars or unstars a model; returns whether it is starred after.
    pub(crate) fn toggle_favourite(&mut self, harness: &ExecutorId, model: &str) -> bool {
        let at = self
            .favourites
            .iter()
            .position(|s| s.harness == harness.as_str() && s.model == model);
        match at {
            Some(at) => {
                self.favourites.remove(at);
                false
            }
            None => {
                self.favourites.push(Star {
                    harness: harness.to_string(),
                    model: model.to_owned(),
                });
                true
            }
        }
    }

    /// The effort last chosen for this model.
    pub(crate) fn effort_for(&self, harness: &ExecutorId, model: &str) -> Option<Effort> {
        self.effort.get(harness.as_str())?.get(model).copied()
    }

    /// Remembers the effort chosen for a model; `None` (back to its default) forgets it.
    pub(crate) fn remember_effort(
        &mut self,
        harness: &ExecutorId,
        model: &str,
        effort: Option<Effort>,
    ) {
        let by_model = self.effort.entry(harness.to_string()).or_default();
        match effort {
            Some(effort) => {
                by_model.insert(model.to_owned(), effort);
            }
            None => {
                by_model.remove(model);
            }
        }
        if by_model.is_empty() {
            self.effort.remove(harness.as_str());
        }
    }

    /// The options last chosen for this model.
    pub(crate) fn options_for(&self, harness: &ExecutorId, model: &str) -> Option<&Options> {
        self.model_options.get(harness.as_str())?.get(model)
    }

    /// Remembers one option's choice for a model.
    pub(crate) fn remember_option(
        &mut self,
        harness: &ExecutorId,
        model: &str,
        option: &str,
        choice: &str,
    ) {
        self.model_options
            .entry(harness.to_string())
            .or_default()
            .entry(model.to_owned())
            .or_default()
            .insert(option.to_owned(), choice.to_owned());
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn id(word: &str) -> ExecutorId {
        word.parse().unwrap()
    }

    #[test]
    fn stars_toggle_per_harness_and_keep_their_order() {
        let mut prefs = ComposerPrefs::default();
        assert!(prefs.toggle_favourite(&id("claude-code"), "opus"));
        assert!(prefs.toggle_favourite(&id("codex"), "gpt"));
        assert!(prefs.is_favourite(&id("claude-code"), "opus"));
        assert!(
            !prefs.is_favourite(&id("codex"), "opus"),
            "the same model id under another harness is another star"
        );
        assert!(!prefs.toggle_favourite(&id("claude-code"), "opus"));
        assert!(!prefs.is_favourite(&id("claude-code"), "opus"));
        assert_eq!(prefs.favourites.len(), 1);
    }

    #[test]
    fn effort_is_remembered_per_model_and_forgotten_at_default() {
        let mut prefs = ComposerPrefs::default();
        let (claude, codex) = (id("claude-code"), id("codex"));
        prefs.remember_effort(&claude, "opus", Some(Effort::Max));
        prefs.remember_effort(&claude, "haiku", Some(Effort::Low));
        prefs.remember_effort(&codex, "opus", Some(Effort::High));
        assert_eq!(prefs.effort_for(&claude, "opus"), Some(Effort::Max));
        assert_eq!(prefs.effort_for(&codex, "opus"), Some(Effort::High));
        prefs.remember_effort(&claude, "opus", None);
        assert_eq!(prefs.effort_for(&claude, "opus"), None);
        assert_eq!(prefs.effort_for(&claude, "haiku"), Some(Effort::Low));
        prefs.remember_effort(&claude, "haiku", None);
        assert!(
            !prefs.effort.contains_key("claude-code"),
            "no empty maps are kept"
        );
    }

    #[test]
    fn options_are_remembered_per_model() {
        let mut prefs = ComposerPrefs::default();
        let claude = id("claude-code");
        prefs.remember_option(&claude, "opus", "contextWindow", "1m");
        prefs.remember_option(&claude, "opus", "contextWindow", "200k");
        prefs.remember_option(&claude, "sonnet", "contextWindow", "1m");
        assert_eq!(
            prefs
                .options_for(&claude, "opus")
                .and_then(|o| o.get("contextWindow"))
                .map(String::as_str),
            Some("200k")
        );
        assert_eq!(
            prefs
                .options_for(&claude, "sonnet")
                .and_then(|o| o.get("contextWindow"))
                .map(String::as_str),
            Some("1m")
        );
        assert_eq!(
            prefs
                .options_for(&claude, "haiku")
                .and_then(|o| o.get("contextWindow"))
                .map(String::as_str),
            None
        );
    }

    #[test]
    fn it_reads_back_what_it_wrote_and_tolerates_older_files() {
        let mut prefs = ComposerPrefs::default();
        prefs.toggle_favourite(&id("codex"), "gpt");
        prefs.remember_effort(&id("codex"), "gpt", Some(Effort::XHigh));
        prefs.remember_option(&id("claude-code"), "opus", "contextWindow", "1m");
        let json = serde_json::to_string(&prefs).unwrap();
        assert!(json.contains("\"xhigh\"") && json.contains("modelOptions"));
        assert_eq!(serde_json::from_str::<ComposerPrefs>(&json).unwrap(), prefs);
        assert_eq!(
            serde_json::from_str::<ComposerPrefs>("{}").unwrap(),
            ComposerPrefs::default()
        );
    }
}
