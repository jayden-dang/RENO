//! Session ids and the layout of the shim's conversation store, as data.
//!
//! The SDK's agent id is the session reference (`SessionRef`). The shim keeps each conversation
//! in a store directory of its own under Reno's state root, and a marker file
//! `by-agent/<agent id>` that names the directory, so a later process finds it again. The id
//! becomes a file name there, which is why one that did not come from the SDK is refused before
//! anything is spawned.

/// The directory of the conversation stores, next to the managed adapters.
pub(crate) const STATE_DIR_NAME: &str = "cursor-state";

/// The shim's index from agent id to store directory (a file per agent).
pub(crate) const BY_AGENT: &str = "by-agent";

/// The directory of fresh stores, one per conversation.
pub(crate) const AGENTS: &str = "agents";

/// The lock file a run holds on its store while its shim is alive.
pub(crate) const OWNER_LOCK: &str = ".reno-owner.lock";

/// The longest agent id accepted.
const MAX_ID: usize = 128;

/// Whether `id` can be an SDK agent id and safely a file name: letters, digits, `-`, `_` and
/// `.`, not starting with a dot, at most [`MAX_ID`] bytes. The SDK's own ids are `agent-` and a
/// UUID.
pub(crate) fn valid_agent_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdk_ids_pass_and_anything_that_could_leave_the_directory_does_not() {
        for ok in [
            "agent-fixture",
            "agent-0a1b2c3d-4e5f-6789-abcd-ef0123456789",
            "a",
            "th_conf.1",
        ] {
            assert!(valid_agent_id(ok), "{ok}");
        }
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "../x",
            "a/b",
            "a\\b",
            "a b",
            "a\0b",
            "\u{e9}",
            &"x".repeat(129),
        ] {
            assert!(!valid_agent_id(bad), "{bad:?}");
        }
    }
}
