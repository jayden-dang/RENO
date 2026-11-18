//! Behaviour tests for the stored auto-update choices: defaults for a missing row, survival of a
//! reopen, and the daily-check time kept apart from the choices.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use reno_proto::ExecutorId;

use crate::Store;
use crate::executor_settings::ExecutorSetting;

fn id(name: &str) -> ExecutorId {
    name.parse().unwrap()
}

#[tokio::test]
async fn an_executor_with_no_row_has_the_defaults_and_a_choice_survives_a_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reno.db");
    {
        let store = Store::open(&path).unwrap();
        assert!(store.executor_settings().await.unwrap().is_empty());
        assert_eq!(
            ExecutorSetting::defaults(id("codex")),
            ExecutorSetting {
                executor: id("codex"),
                auto_update: true,
                verified_only: false,
                prev_self_update: None,
                last_checked_ms: None,
            }
        );
        let mut off = ExecutorSetting::defaults(id("claude-code"));
        off.auto_update = false;
        off.verified_only = true;
        off.prev_self_update = Some(r#"{"was":"0","parent":false}"#.to_owned());
        store.put_executor_setting(&off, 5).await.unwrap();
    }
    let store = Store::open(&path).unwrap();
    let rows = store.executor_settings().await.unwrap();
    assert_eq!(rows.len(), 1);
    assert!(!rows[0].auto_update && rows[0].verified_only);
    assert_eq!(
        rows[0].prev_self_update.as_deref(),
        Some(r#"{"was":"0","parent":false}"#)
    );
}

#[tokio::test]
async fn the_check_time_is_kept_when_the_choices_change_and_the_reverse() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("reno.db")).unwrap();
    // A check on an executor with no row creates one with the defaults.
    store
        .put_executor_checked(&id("grok"), 1_000)
        .await
        .unwrap();
    let row = &store.executor_settings().await.unwrap()[0];
    assert!(row.auto_update);
    assert_eq!(row.last_checked_ms, Some(1_000));
    let mut changed = row.clone();
    changed.auto_update = false;
    store.put_executor_setting(&changed, 2).await.unwrap();
    let row = &store.executor_settings().await.unwrap()[0];
    assert!(!row.auto_update);
    assert_eq!(row.last_checked_ms, Some(1_000));
    store
        .put_executor_checked(&id("grok"), 9_000)
        .await
        .unwrap();
    let row = &store.executor_settings().await.unwrap()[0];
    assert!(!row.auto_update, "a check does not turn the switch back on");
    assert_eq!(row.last_checked_ms, Some(9_000));
}

#[tokio::test]
async fn a_record_that_is_not_json_is_refused_by_the_table() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("reno.db")).unwrap();
    let mut bad = ExecutorSetting::defaults(id("grok"));
    bad.prev_self_update = Some("not json".to_owned());
    assert!(store.put_executor_setting(&bad, 1).await.is_err());
}
