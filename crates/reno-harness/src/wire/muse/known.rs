//! The frames the driver knows by name, handled or ignored on purpose. A frame outside these
//! tables is one the driver has never seen: it is dropped and counted
//! (`Executor::unknown_frames`), the early signal that Muse moved.
//!
//! The names are the notifications, requests and item kinds of the 1.4.23 schema (`muse schema
//! generate-json-schema --experimental`): the notification table has all 70 of its entries, so a
//! name outside it is one a later Muse added. Most are chatter a coding run has no use for
//! (logs, peers, plugins, the journal); they are known, not counted.

/// Notifications the driver acts on.
pub(crate) const HANDLED_NOTIFICATIONS: &[&str] = &[
    "turn/started",
    "turn/completed",
    "item/started",
    "item/updated",
    "item/completed",
    "item/delta",
    "session/tokenUsage",
    "session/contextUsage",
    "approval/requested",
    "approval/updated",
    "approval/resolved",
    "userInput/requested",
    "userInput/settled",
    "view/gap",
    "session/closed",
];

/// Notifications that are known and mean nothing to a run.
pub(crate) const IGNORED_NOTIFICATIONS: &[&str] = &[
    "account/changed",
    "account/loginCompleted",
    "admission/requested",
    "admission/resolved",
    "approval/reviewing",
    "backgroundTerminal/inboxItemDrained",
    "computerUseSetup/changed",
    "cua/lockChanged",
    "initialized",
    "journal/confirmThrough",
    "log/gap",
    "log/physicalLine",
    "log/physicalReset",
    "log/record",
    "mcpServer/forfeited",
    "mcpServer/oauthLoginCompleted",
    "model/completed",
    "monitor/inboxItemAdmitted",
    "monitor/inboxItemDrained",
    "peer/inboxItemDrained",
    "peer/settlementsChanged",
    "plugin/changed",
    "plugin/marketplaceChanged",
    "pluginData/changed",
    "recap/completed",
    "session/activationChanged",
    "session/approvalModeChanged",
    "session/branchChanged",
    "session/driveChanged",
    "session/eventLogDegraded",
    "session/goalChanged",
    "session/leaveCompleted",
    "session/listChanged",
    "session/logFlushDegraded",
    "session/modelChanged",
    "session/modelRouteUnserved",
    "session/nameChanged",
    "session/permissionProfileChanged",
    "session/reasoningEffortChanged",
    "session/rulesChanged",
    "session/started",
    "session/statusChanged",
    "session/todoListChanged",
    "session/viewHealthChanged",
    "skill/changed",
    "skillProvider/updated",
    "turn/foregroundCompleted",
    "turn/modelAttemptStarted",
    "turn/modelRetryScheduled",
    "turn/retracted",
    "turn/retryScheduled",
    "turn/toolBatchSubmitted",
    "turn/unqueued",
    "usage/changed",
    "userInput/engaged",
];

/// Requests the host makes of the client that Reno answers (with an empty result, the real
/// question arriving as a notification).
pub(crate) const ANSWERED_REQUESTS: &[&str] = &["approval/request", "userInput/request"];

/// Item kinds the normalizer shows or tracks.
pub(crate) const SHOWN_ITEMS: &[&str] = &[
    "agentMessage",
    "reasoning",
    "toolCall",
    "userShell",
    "subagent",
    "workflow",
];

/// Item kinds that are known and not shown: the prompt itself, Muse's per-step housekeeping, and
/// the rest of the transcript's furniture.
pub(crate) const IGNORED_ITEMS: &[&str] = &[
    "userMessage",
    "reminderChild",
    "compaction",
    "hookRun",
    "sideChat",
    "webSearch",
    "monitorFeed",
    "damagedRange",
    "scheduledFire",
    "backgroundTerminalCompletion",
    "overdueNotice",
    "earlierHistory",
    "peerMessage",
    "subagentInput",
    "subagentResult",
    "systemNotice",
    "bodyCompletion",
    "skillRead",
];

/// Whether `method` is a notification the driver knows.
pub(crate) fn is_known_notification(method: &str) -> bool {
    HANDLED_NOTIFICATIONS.contains(&method) || IGNORED_NOTIFICATIONS.contains(&method)
}

/// Whether `kind` is an item kind the driver knows.
pub(crate) fn is_known_item(kind: &str) -> bool {
    SHOWN_ITEMS.contains(&kind) || IGNORED_ITEMS.contains(&kind)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tables_have_no_duplicates_and_do_not_overlap() {
        for table in [
            HANDLED_NOTIFICATIONS,
            IGNORED_NOTIFICATIONS,
            SHOWN_ITEMS,
            IGNORED_ITEMS,
        ] {
            let mut sorted = table.to_vec();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(sorted.len(), table.len());
        }
        for name in HANDLED_NOTIFICATIONS {
            assert!(!IGNORED_NOTIFICATIONS.contains(name), "{name}");
        }
        for kind in SHOWN_ITEMS {
            assert!(!IGNORED_ITEMS.contains(kind), "{kind}");
        }
    }

    #[test]
    fn a_future_frame_is_not_known() {
        assert!(is_known_notification("turn/completed"));
        assert!(is_known_notification("session/nameChanged"));
        assert!(!is_known_notification("turn/teleported"));
        assert!(is_known_item("toolCall"));
        assert!(is_known_item("reminderChild"));
        assert!(!is_known_item("hologram"));
    }
}
