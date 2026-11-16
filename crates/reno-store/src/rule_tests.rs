//! Behaviour tests for standing rules: created with the decision that made them, scoped, expiring
//! with the clock the caller passes, revoked once, and never edited. `now` is a number the test
//! owns.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use reno_proto::{
    ActionClass, ApprovalId, ApprovalOutcome, ApprovalStatus, ApprovalSubject, CommandPattern,
    ExecutorId, ResolvedBy, RuleId, RuleMatcher, RunId, Seq, StandingEffect, StandingScope, TaskId,
};
use serde_json::json;
use tempfile::TempDir;

use crate::approvals::NewApproval;
use crate::rules::NewRule;
use crate::{NewTask, Store, StoreError};

fn run_id(n: usize) -> RunId {
    RunId::from_suffix(&format!("r{n}")).unwrap()
}

fn rule_id(n: usize) -> RuleId {
    RuleId::from_suffix(&format!("u{n}")).unwrap()
}

fn matcher(command: &str) -> RuleMatcher {
    RuleMatcher {
        action: ActionClass::Command,
        tool: "bash".into(),
        command: Some(CommandPattern::Exact {
            command: command.into(),
        }),
        paths: vec![],
    }
}

fn rule(
    n: usize,
    scope: StandingScope,
    effect: StandingEffect,
    expires_at: Option<i64>,
) -> NewRule {
    NewRule {
        id: rule_id(n),
        scope,
        effect,
        matcher: matcher("git push"),
        expires_at,
        source_approval: None,
    }
}

async fn store_with_runs(runs: usize) -> (TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("reno.db")).unwrap();
    let task = TaskId::from_suffix("t0").unwrap();
    store
        .create_task(
            NewTask {
                id: task.clone(),
                goal: "g".into(),
                class: None,
                spec: json!({}),
            },
            0,
        )
        .await
        .unwrap();
    for n in 0..runs {
        let executor: ExecutorId = "mock".parse().unwrap();
        store
            .enqueue_run(&task, &run_id(n), &executor, 0)
            .await
            .unwrap();
        store.claim_next("test", 1, 60_000).await.unwrap().unwrap();
    }
    (dir, store)
}

fn pending(n: usize, run: usize) -> NewApproval {
    NewApproval {
        id: ApprovalId::from_suffix(&format!("a{n}")).unwrap(),
        run_id: run_id(run),
        request_id: format!("req{n}"),
        subject: ApprovalSubject {
            action: ActionClass::Command,
            tool: "bash".into(),
            command: Some("git push".into()),
            paths: vec![],
            raw: json!({}),
        },
        expires_at: 1_000_000,
    }
}

#[tokio::test]
async fn a_rule_is_stored_whole_with_its_event() {
    let (_d, store) = store_with_runs(1).await;
    let new = rule(
        1,
        StandingScope::Bot {
            bot: "scout".into(),
        },
        StandingEffect::Allow,
        Some(5_000),
    );
    store.create_rule(new.clone(), 100).await.unwrap();
    let row = store.get_rule(&rule_id(1)).await.unwrap().unwrap();
    assert_eq!(row.scope, new.scope);
    assert_eq!(row.effect, StandingEffect::Allow);
    assert_eq!(row.matcher, new.matcher);
    assert_eq!(
        (
            row.created_by.as_str(),
            row.created_at_ms,
            row.expires_at_ms,
            row.revoked_at_ms
        ),
        ("human", 100, Some(5_000), None)
    );
    let events = store.events_since(Seq(0), 1000).await.unwrap();
    let created: Vec<_> = events.iter().filter(|e| e.kind == "rule.created").collect();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].principal.as_deref(), Some("human"));
    let text = created[0].data.to_string();
    assert!(
        !text.contains("git push"),
        "the matcher is digested, not copied: {text}"
    );
    assert!(matches!(
        store.create_rule(new, 101).await,
        Err(StoreError::AlreadyExists { .. })
    ));
}

#[tokio::test]
async fn a_decision_and_its_rule_commit_together_or_not_at_all() {
    let (_d, store) = store_with_runs(1).await;
    store.create_approval(pending(1, 0), 1, 10).await.unwrap();
    let id = pending(1, 0).id;
    store
        .resolve_approval_remembering(
            &id,
            ApprovalOutcome::Denied,
            ResolvedBy::Human,
            None,
            rule(
                1,
                StandingScope::Run { run: run_id(0) },
                StandingEffect::Deny,
                None,
            ),
            1,
            20,
        )
        .await
        .unwrap();
    let row = store.get_rule(&rule_id(1)).await.unwrap().unwrap();
    assert_eq!(row.source_approval, Some(id.clone()));
    assert_eq!(
        store.get_approval(&id).await.unwrap().unwrap().status,
        ApprovalStatus::Denied
    );

    // Deciding it again fails, and so leaves no second rule.
    let again = store
        .resolve_approval_remembering(
            &id,
            ApprovalOutcome::Approved,
            ResolvedBy::Human,
            None,
            rule(2, StandingScope::Always, StandingEffect::Allow, None),
            1,
            30,
        )
        .await;
    assert!(matches!(again, Err(StoreError::NotFound { .. })));
    assert!(store.get_rule(&rule_id(2)).await.unwrap().is_none());

    // A stale fence is refused whole as well.
    store.create_approval(pending(3, 0), 1, 40).await.unwrap();
    let stale = store
        .resolve_approval_remembering(
            &pending(3, 0).id,
            ApprovalOutcome::Approved,
            ResolvedBy::Human,
            None,
            rule(3, StandingScope::Always, StandingEffect::Allow, None),
            7,
            50,
        )
        .await;
    assert!(matches!(stale, Err(StoreError::StaleFence { .. })));
    assert!(store.get_rule(&rule_id(3)).await.unwrap().is_none());
    assert_eq!(
        store
            .get_approval(&pending(3, 0).id)
            .await
            .unwrap()
            .unwrap()
            .status,
        ApprovalStatus::Pending
    );
}

#[tokio::test]
async fn active_rules_follow_scope_expiry_and_revocation() {
    let (_d, store) = store_with_runs(2).await;
    let make = |n, scope, expires| rule(n, scope, StandingEffect::Allow, expires);
    let bot = |name: &str| StandingScope::Bot { bot: name.into() };
    let run = |n| StandingScope::Run { run: run_id(n) };
    store
        .create_rule(make(1, StandingScope::Always, None), 10)
        .await
        .unwrap();
    store
        .create_rule(make(2, bot("scout"), None), 11)
        .await
        .unwrap();
    store
        .create_rule(make(3, bot("other"), None), 12)
        .await
        .unwrap();
    store.create_rule(make(4, run(0), None), 13).await.unwrap();
    store.create_rule(make(5, run(1), None), 14).await.unwrap();
    store
        .create_rule(make(6, StandingScope::Always, Some(1_000)), 15)
        .await
        .unwrap();

    let ids = |rules: Vec<reno_proto::StandingRule>| -> Vec<String> {
        rules.into_iter().map(|r| r.id.to_string()).collect()
    };
    assert_eq!(
        ids(store
            .active_rules(&run_id(0), Some("scout"), 500)
            .await
            .unwrap()),
        ["rule_u1", "rule_u2", "rule_u4", "rule_u6"]
    );
    assert_eq!(
        ids(store.active_rules(&run_id(1), None, 999).await.unwrap()),
        ["rule_u1", "rule_u5", "rule_u6"]
    );
    // Rule 6 expires at 1000, to the millisecond.
    assert_eq!(
        ids(store.active_rules(&run_id(1), None, 1_000).await.unwrap()),
        ["rule_u1", "rule_u5"]
    );

    store.revoke_rule(&rule_id(1), 2_000).await.unwrap();
    store.revoke_rule(&rule_id(1), 2_500).await.unwrap();
    assert_eq!(
        ids(store.active_rules(&run_id(1), None, 500).await.unwrap()),
        ["rule_u5", "rule_u6"]
    );
    let all = store.list_rules(true, 1_500).await.unwrap();
    assert_eq!(all.len(), 6);
    assert_eq!(all[0].id, rule_id(6), "newest first");
    let revoked = all.iter().find(|r| r.id == rule_id(1)).unwrap();
    assert_eq!(
        revoked.revoked_at_ms,
        Some(2_000),
        "the first revocation stands"
    );
    assert_eq!(store.list_rules(false, 1_500).await.unwrap().len(), 4);
    assert_eq!(
        store
            .events_since(Seq(0), 1000)
            .await
            .unwrap()
            .iter()
            .filter(|e| e.kind == "rule.revoked")
            .count(),
        1
    );
    assert!(matches!(
        store.revoke_rule(&rule_id(99), 3_000).await,
        Err(StoreError::NotFound { .. })
    ));
}

#[tokio::test]
async fn a_rule_is_never_edited_or_deleted() {
    let (_d, store) = store_with_runs(1).await;
    store
        .create_rule(
            rule(1, StandingScope::Always, StandingEffect::Deny, None),
            10,
        )
        .await
        .unwrap();
    let edit = |sql: &'static str| {
        let store = store.clone();
        async move {
            store
                .write(crate::writer::Durability::Normal, move |c| {
                    c.execute(sql, []).map(|_| ()).map_err(crate::error::db)
                })
                .await
        }
    };
    for sql in [
        "UPDATE standing_rules SET effect = 'allow'",
        "UPDATE standing_rules SET matcher = '{}'",
        "UPDATE standing_rules SET expires_at = 99999",
        "UPDATE standing_rules SET revoked_at = NULL",
        "DELETE FROM standing_rules",
    ] {
        assert!(edit(sql).await.is_err(), "{sql}");
    }
    edit("UPDATE standing_rules SET revoked_at = 50")
        .await
        .unwrap();
    assert!(
        edit("UPDATE standing_rules SET revoked_at = 60")
            .await
            .is_err()
    );
}
