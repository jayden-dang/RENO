//! Codex app-server: model catalog, elicitation mapping, the item normalizer and subagent ownership.

pub(crate) mod catalog;
pub(crate) mod elicitation;
pub(crate) mod normalize;
pub(crate) mod subagents;

/// A fresh id for an assistant or client message.
pub(crate) fn new_message_id() -> String {
    uuid::Uuid::new_v4().to_string()
}
