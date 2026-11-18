//! The bus frames the reducer knows by name, handled or ignored on purpose. A frame outside this
//! table is one the driver has never seen: it is dropped and counted (`Executor::unknown_frames`),
//! the early signal that opencode moved.
//!
//! The names are the 1.x event vocabulary as zeron met it (1.14 to 1.19), after the 2.x frames
//! are rewritten into it; the 2.x frames [`super::v2`] drops on purpose are listed there. Whole
//! families the driver has no use for (`lsp.*`, `pty.*`, `tui.*`, ...) are matched by prefix.

/// Exact frame types.
const KNOWN: &[&str] = &[
    // The ones the reducer acts on.
    "session.status",
    "session.idle",
    "session.error",
    "session.warning",
    "session.created",
    "session.interrupted",
    "message.updated",
    "message.part.updated",
    "message.part.delta",
    "permission.asked",
    "question.asked",
    // Ignored on purpose: bookkeeping of the session and its transcript.
    "session.updated",
    "session.deleted",
    "session.diff",
    "session.compacted",
    "session.next.created",
    "message.removed",
    "message.part.removed",
    "permission.replied",
    "question.replied",
    "question.rejected",
    "todo.updated",
    "command.executed",
    // The wrapper 1.x puts around mirrored events.
    "sync",
];

/// Families ignored whole.
const KNOWN_PREFIXES: &[&str] = &[
    "server.",
    "file.",
    "vcs.",
    "lsp.",
    "mcp.",
    "pty.",
    "tui.",
    "installation.",
    "project.",
    "workspace.",
    "worktree.",
    "ide.",
    "global.",
    "storage.",
    "integration.",
    "plugin.",
];

/// Whether `kind` is a frame type the reducer knows.
pub(crate) fn is_known(kind: &str) -> bool {
    KNOWN.contains(&kind) || KNOWN_PREFIXES.iter().any(|prefix| kind.starts_with(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_acted_on_and_the_ignored_are_known_and_a_new_name_is_not() {
        for kind in [
            "session.idle",
            "message.part.delta",
            "server.connected",
            "server.heartbeat",
            "lsp.updated",
            "file.edited",
        ] {
            assert!(is_known(kind), "{kind}");
        }
        for kind in ["session.teleported", "message.part.explode", ""] {
            assert!(!is_known(kind), "{kind}");
        }
    }
}
