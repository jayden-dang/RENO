//! The app-server's notification methods the driver knows by name, handled or ignored on
//! purpose. A notification outside this table is one the driver has never seen: it is dropped
//! and counted (`Executor::unknown_frames`), the early signal that the CLI moved.
//!
//! The names are the `ServerNotification` methods of the 0.169.0 wire schema
//! (`codex app-server generate-json-schema`, snapshot under `tests/fixtures/codex/schema`), plus
//! the two settle methods older app-servers send. A test keeps the table and the snapshot
//! together.

/// Notification methods that are not unknown.
pub(crate) const KNOWN_NOTIFICATIONS: &[&str] = &[
    "error",
    "thread/started",
    "thread/status/changed",
    "thread/readState/changed",
    "thread/archived",
    "thread/deleted",
    "thread/unarchived",
    "thread/closed",
    "thread/reverted",
    "skills/changed",
    "thread/name/updated",
    "thread/section/changed",
    "thread/attachment/updated",
    "thread/goal/updated",
    "thread/prediction/updated",
    "thread/goal/cleared",
    "thread/queue/changed",
    "project/changed",
    "thread/project/updated",
    "thread/environment/connected",
    "thread/environment/disconnected",
    "thread/settings/updated",
    "thread/tokenUsage/updated",
    "turn/started",
    "hook/started",
    "turn/completed",
    "hook/completed",
    "turn/diff/updated",
    "turn/plan/updated",
    "item/started",
    "item/autoApprovalReview/started",
    "item/autoApprovalReview/completed",
    "autoApprovalReview/strictReviewRequired",
    "item/completed",
    "item/agentMessage/delta",
    "item/plan/delta",
    "command/exec/outputDelta",
    "process/outputDelta",
    "process/exited",
    "item/commandExecution/outputDelta",
    "item/commandExecution/terminalInteraction",
    "item/fileChange/outputDelta",
    "item/fileChange/patchUpdated",
    "serverRequest/resolved",
    "item/mcpToolCall/progress",
    "mcpServer/oauthLogin/completed",
    "mcpServer/startupStatus/updated",
    "mcpServer/event/stream/notification",
    "account/updated",
    "account/gatewayOAuth/changed",
    "account/rateLimits/updated",
    "app/list/updated",
    "remoteControl/status/changed",
    "externalAgentConfig/import/progress",
    "externalAgentConfig/import/completed",
    "fs/changed",
    "item/reasoning/summaryTextDelta",
    "item/reasoning/summaryPartAdded",
    "item/reasoning/textDelta",
    "thread/compacted",
    "model/rerouted",
    "model/verification",
    "modelProvider/authRecoveryStarted",
    "modelProvider/authRecoveryCompleted",
    "turn/moderationMetadata",
    "model/safetyBuffering/updated",
    "warning",
    "guardianWarning",
    "deprecationNotice",
    "configWarning",
    "fuzzyFileSearch/sessionUpdated",
    "fuzzyFileSearch/sessionCompleted",
    "thread/realtime/started",
    "thread/realtime/itemAdded",
    "thread/realtime/item/started",
    "thread/realtime/item/transcript/delta",
    "thread/realtime/item/completed",
    "thread/realtime/transcript/delta",
    "thread/realtime/transcript/done",
    "thread/realtime/outputAudio/delta",
    "thread/realtime/sdp",
    "thread/realtime/error",
    "thread/realtime/closed",
    "windows/worldWritableWarning",
    "windowsSandbox/setupCompleted",
    "account/login/completed",
    // Older app-servers settle a turn with these; the driver reads them.
    "turn/failed",
    "turn/aborted",
];

/// Whether the driver knows this notification method by name.
pub(crate) fn is_known_notification(method: &str) -> bool {
    KNOWN_NOTIFICATIONS.contains(&method)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table is the snapshot's `ServerNotification` methods plus the two legacy settle
    /// methods, so refreshing the snapshot without the table (or the reverse) fails here.
    #[test]
    #[allow(clippy::unwrap_used, clippy::expect_used)]
    fn the_table_is_the_snapshot_s_notification_methods() {
        let schema: serde_json::Value = serde_json::from_str(include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/fixtures/codex/schema/ServerNotification.json"
        )))
        .unwrap();
        let mut from_schema: Vec<&str> = schema["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| {
                v.pointer("/properties/method/enum/0")
                    .unwrap()
                    .as_str()
                    .unwrap()
            })
            .collect();
        from_schema.extend(["turn/failed", "turn/aborted"]);
        from_schema.sort_unstable();
        let mut table: Vec<&str> = KNOWN_NOTIFICATIONS.to_vec();
        table.sort_unstable();
        assert_eq!(table, from_schema);
    }

    #[test]
    fn a_method_nobody_named_is_unknown() {
        assert!(is_known_notification("item/agentMessage/delta"));
        assert!(is_known_notification("thread/status/changed"));
        assert!(!is_known_notification("zz/futureNotification"));
    }
}
